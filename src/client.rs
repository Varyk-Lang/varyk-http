// `Client`, the HTTP client (varyk-http spec 2.6): reqwest with rustls
// and the bundled web roots, over HTTP/1.1.

use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::{Action, Attempt, Policy};

/// The default timeout for a whole exchange, 30 seconds.
const TIMEOUT_MS: u64 = 30_000;
/// The default limit on a response body, 10 MiB.
const BODY_LIMIT: u64 = 10 * 1024 * 1024;
/// How many redirects are followed before the next is given back.
const REDIRECTS: usize = 10;

/// An HTTP client; `client.clone()` shares its connections.
#[derive(Clone)]
pub struct Client {
    headers: Vec<(String, String)>,
    timeout_ms: u64,
    body_limit: u64,
    /// The reqwest client built from the settings, rebuilt by each setter;
    /// a setting that cannot work is the `Err` of every later call.
    built: Result<reqwest::Client, varyk_std::Error>,
}

impl Client {
    /// A client with no default headers.
    #[allow(clippy::new_without_default)] // Varyk calls `new`, not `default`.
    pub fn new() -> Client {
        let mut client = Client {
            headers: Vec::new(),
            timeout_ms: TIMEOUT_MS,
            body_limit: BODY_LIMIT,
            built: Err(varyk_std::Error::new(String::new())),
        };
        client.rebuild();
        client
    }

    /// A header sent with every request, replacing one of the same name.
    pub fn set_header(&mut self, name: &str, value: &str) {
        self.headers
            .retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        self.headers.push((name.to_string(), value.to_string()));
        self.rebuild();
    }

    /// How long a whole exchange may take, in milliseconds.
    pub fn set_timeout(&mut self, ms: u64) {
        self.timeout_ms = ms;
        self.rebuild();
    }

    /// The largest response body read, in bytes.
    pub fn set_body_limit(&mut self, bytes: u64) {
        self.body_limit = bytes;
        self.rebuild();
    }

    pub async fn get(&self, url: &str) -> Result<crate::message::Response, varyk_std::Error> {
        self.send(reqwest::Method::GET, url, None).await
    }

    pub async fn delete(&self, url: &str) -> Result<crate::message::Response, varyk_std::Error> {
        self.send(reqwest::Method::DELETE, url, None).await
    }

    /// Sends `body` as JSON.
    pub async fn post<T: varyk_std::serde::Serialize + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<crate::message::Response, varyk_std::Error> {
        let body = varyk_std::json::stringify(body);
        self.send(reqwest::Method::POST, url, Some(body)).await
    }

    /// Sends `body` as JSON.
    pub async fn put<T: varyk_std::serde::Serialize + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<crate::message::Response, varyk_std::Error> {
        let body = varyk_std::json::stringify(body);
        self.send(reqwest::Method::PUT, url, Some(body)).await
    }

    /// Sends `body` as JSON.
    pub async fn patch<T: varyk_std::serde::Serialize + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<crate::message::Response, varyk_std::Error> {
        let body = varyk_std::json::stringify(body);
        self.send(reqwest::Method::PATCH, url, Some(body)).await
    }

    fn rebuild(&mut self) {
        self.built = self.build().map_err(|reason| {
            varyk_std::Error::new(format!("the client cannot be built: {reason}"))
        });
    }

    /// The reqwest client of the settings; the reason when one cannot
    /// work.
    fn build(&self) -> Result<reqwest::Client, String> {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.headers {
            // The value is never named: it may be a token.
            let key = HeaderName::from_bytes(name.as_bytes());
            let value = HeaderValue::from_bytes(value.as_bytes());
            let (Ok(key), Ok(value)) = (key, value) else {
                return Err(format!(
                    "set_header({name:?}, ..) cannot work: the name or the value is not valid HTTP"
                ));
            };
            headers.insert(key, value);
        }
        if self.timeout_ms == 0 {
            return Err(
                "set_timeout(0) cannot work: the timeout is at least 1 millisecond".to_string(),
            );
        }
        if self.body_limit == 0 {
            return Err("set_body_limit(0) cannot work: the limit is at least 1 byte".to_string());
        }
        reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_millis(self.timeout_ms))
            .redirect(Policy::custom(same_origin))
            .http1_only()
            // No proxy from the environment: client proxies are not in this
            // version (spec 9), and a default header must not go to one.
            .no_proxy()
            .build()
            .map_err(|err| innermost(&err.without_url()))
    }

    /// Sends one request and reads its response whole, up to the body
    /// limit. An `Err`'s message names the URL's scheme and host only,
    /// never its path or query, which may carry a token.
    async fn send(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<String>,
    ) -> Result<crate::message::Response, varyk_std::Error> {
        let client = self.built.as_ref().map_err(Clone::clone)?;
        let url = reqwest::Url::parse(url)
            .map_err(|err| varyk_std::Error::new(format!("the URL cannot be read: {err}")))?;
        let origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(varyk_std::Error::new(format!(
                "the URL {origin} is not http or https"
            )));
        }
        let failed = |err: reqwest::Error| {
            let err = err.without_url();
            varyk_std::Error::new(if err.is_timeout() {
                format!(
                    "the request to {origin} timed out after {} ms",
                    self.timeout_ms
                )
            } else {
                format!("the request to {origin} failed: {}", innermost(&err))
            })
        };
        let mut request = client.request(method, url);
        if let Some(body) = body {
            request = request
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        let mut response = request.send().await.map_err(failed)?;
        let too_large = || {
            varyk_std::Error::new(format!(
                "the response from {origin} is larger than the body limit of {} bytes",
                self.body_limit
            ))
        };
        if response
            .content_length()
            .is_some_and(|declared| declared > self.body_limit)
        {
            return Err(too_large());
        }
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let mut read: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(failed)? {
            if (read.len() as u64).saturating_add(chunk.len() as u64) > self.body_limit {
                return Err(too_large());
            }
            read.extend_from_slice(&chunk);
        }
        Ok(crate::message::Response::read_back(status, &headers, read))
    }
}

/// The redirect policy: up to ten, each to the scheme, host, and port of
/// the first request; any other is given back as its 3xx response, so a
/// default header never goes to a host the program did not name.
fn same_origin(attempt: Attempt) -> Action {
    let follow = match attempt.previous().first() {
        Some(first) => {
            let next = attempt.url();
            attempt.previous().len() <= REDIRECTS
                && next.scheme() == first.scheme()
                && next.host_str() == first.host_str()
                && next.port_or_known_default() == first.port_or_known_default()
        }
        None => false,
    };
    if follow {
        attempt.follow()
    } else {
        attempt.stop()
    }
}

/// What the innermost cause of `err` says: a connection refused, a name
/// not found, a TLS failure. reqwest's own text may name the URL, so only
/// the causes beneath it are read.
fn innermost(err: &reqwest::Error) -> String {
    let mut cause: &dyn std::error::Error = err;
    while let Some(source) = cause.source() {
        cause = source;
    }
    cause.to_string()
}
