// `Request` and `Response`, and the `respond_` functions the compiler's
// adapters call to turn what a handler or a hook gave into a response
// (varyk-http spec 2.3, 6.2; milestone 5b4 spec 6.1).

use std::any::Any;
use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use futures_util::Stream;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};

/// The app's state, as `App::new` was given it.
pub(crate) type State = Arc<dyn Any + Send + Sync>;

/// One request, as a handler, a hook, and `App::request` see it. A copy
/// is cheap: the body is shared.
#[derive(Clone)]
pub struct Request {
    method: String,
    /// The path as received, still percent-encoded, without the query.
    path: String,
    /// The query string as received, without the `?`.
    query: String,
    /// Header names and values as given or received; a received value
    /// that is not UTF-8 is left out.
    headers: Vec<(String, String)>,
    body: Bytes,
    /// What the router adds to a request it routed: none for a request a
    /// program made with `Request::new`.
    routed: Option<Arc<Routed>>,
}

/// What the router adds to a request: the app's state, the path
/// parameters by the route's own names, the early-response channel, and
/// a body or an upgrade kept for `bind`.
pub(crate) struct Routed {
    state: State,
    params: Vec<(String, String)>,
    /// Armed by the route wrapper before the adapter runs; a live
    /// handler's `bind` sends its response through it (spec 6.3).
    early: Mutex<Option<oneshot::Sender<Response>>>,
    /// A `multipart/form-data` body, kept unread for `bind::<Multipart>`
    /// to take, once (spec 6.2).
    upload: Mutex<Option<Upload>>,
    /// A WebSocket upgrade, kept for `bind::<WebSocket>` to take, once
    /// (spec 6.2).
    upgrade: Mutex<Option<Upgrade>>,
    /// Set when the multipart body crosses the body limit while it is
    /// read; the route wrapper then answers the 413 (spec 2.5).
    over_limit: Arc<AtomicBool>,
    /// The message of the `Err` a live connection gave because its client
    /// has gone, so that `Err`, passed on by the handler, is logged at
    /// debug level, not as an error (spec 3.1).
    client_gone: ClientGone,
}

/// The message of the `Err` a live connection gave for a client that has
/// gone, once it gave one.
pub(crate) type ClientGone = Arc<Mutex<Option<String>>>;

/// The `Err` a live connection gives because its client has gone, with
/// its message recorded in `record`, so the route wrapper can tell it
/// from any other `Err` the handler gives (spec 3.1).
pub(crate) fn client_gone(record: &ClientGone, message: &str) -> varyk_std::Error {
    if let Ok(mut slot) = record.lock() {
        *slot = Some(message.to_string());
    }
    varyk_std::Error::new(message.to_string())
}

/// A multipart body as the route wrapper kept it: the stream, the body
/// limit, and the flag to set when the stream crosses it.
pub(crate) struct Upload {
    pub(crate) body: Body,
    pub(crate) limit: u64,
    pub(crate) over_limit: Arc<AtomicBool>,
}

/// A WebSocket upgrade as the route wrapper kept it, with the body limit,
/// which is the largest message (spec 2.5).
pub(crate) struct Upgrade {
    pub(crate) socket: axum::extract::ws::WebSocketUpgrade,
    pub(crate) limit: u64,
}

impl Routed {
    pub(crate) fn new(state: State, params: Vec<(String, String)>) -> Arc<Routed> {
        Arc::new(Routed {
            state,
            params,
            early: Mutex::new(None),
            upload: Mutex::new(None),
            upgrade: Mutex::new(None),
            over_limit: Arc::new(AtomicBool::new(false)),
            client_gone: Arc::new(Mutex::new(None)),
        })
    }

    /// Keeps a multipart `body` unread for `bind::<Multipart>`, to be read
    /// up to `limit` bytes.
    pub(crate) fn keep_upload(&self, body: Body, limit: u64) {
        if let Ok(mut slot) = self.upload.lock() {
            *slot = Some(Upload {
                body,
                limit,
                over_limit: Arc::clone(&self.over_limit),
            });
        }
    }

    /// Keeps a WebSocket upgrade for `bind::<WebSocket>`, its messages at
    /// most `limit` bytes.
    pub(crate) fn keep_upgrade(&self, socket: axum::extract::ws::WebSocketUpgrade, limit: u64) {
        if let Ok(mut slot) = self.upgrade.lock() {
            *slot = Some(Upgrade { socket, limit });
        }
    }

    /// Whether the multipart body crossed the body limit while it was
    /// read.
    pub(crate) fn over_limit(&self) -> bool {
        self.over_limit.load(Ordering::SeqCst)
    }

    /// Where a live connection records its `Err` for a client that has
    /// gone.
    pub(crate) fn client_gone(&self) -> ClientGone {
        Arc::clone(&self.client_gone)
    }

    /// Gives the request the channel an early response is sent through.
    pub(crate) fn arm(&self, sender: oneshot::Sender<Response>) {
        if let Ok(mut slot) = self.early.lock() {
            *slot = Some(sender);
        }
    }
}

/// One response: what a handler builds, what `App::request` gives, and
/// what the client receives.
pub struct Response {
    status: u16,
    /// Header names and values as set, checked when the response is sent.
    headers: Vec<(String, String)>,
    body: Content,
    /// Why the response cannot be sent as built (a cookie a cookie cannot
    /// hold), logged when it is sent as a 500 instead.
    problem: Option<String>,
    /// A 500's message, logged with the request's method and path when
    /// the response is sent, never sent itself (spec 6.1).
    hidden: Option<String>,
}

/// What a response sends: text, a file opened by `Response::file` and
/// read when the response is sent, or the events an `Sse` sends.
enum Content {
    Text(String),
    File(std::fs::File),
    Events(mpsc::Receiver<String>),
}

/// The size of one chunk of a file being sent.
const CHUNK: usize = 64 * 1024;

/// How long an event stream may be quiet before a comment line is sent,
/// so proxies keep the connection open (spec 2.5).
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// A path value: an integer, `bool`, or text.
pub trait Plain: std::str::FromStr {}

impl Plain for i8 {}
impl Plain for i16 {}
impl Plain for i32 {}
impl Plain for i64 {}
impl Plain for u8 {}
impl Plain for u16 {}
impl Plain for u32 {}
impl Plain for u64 {}
impl Plain for usize {}
impl Plain for bool {}
impl Plain for String {}

/// A query value: a plain value, required, or an `Option` of one.
pub trait QueryValue: Sized {
    /// The value of the query parameter `name`, `found` in the query
    /// string or not.
    fn from_query(name: &str, found: Option<String>) -> Result<Self, Response>;
}

impl<T: Plain> QueryValue for T {
    fn from_query(name: &str, found: Option<String>) -> Result<T, Response> {
        match found {
            Some(text) => read("query parameter", name, &text),
            None => Err(error_body(
                400,
                &format!("the query parameter `{name}` is missing"),
            )),
        }
    }
}

impl<T: Plain> QueryValue for Option<T> {
    fn from_query(name: &str, found: Option<String>) -> Result<Option<T>, Response> {
        match found {
            Some(text) => read("query parameter", name, &text).map(Some),
            None => Ok(None),
        }
    }
}

/// A value the package makes for one request, bound by its type: every
/// struct named at the package's root but `App`, `Response`, and
/// `Client`.
pub trait Bound: Sized {
    fn bound(request: &Request) -> impl Future<Output = Result<Self, Response>> + Send;
}

impl Bound for Request {
    async fn bound(request: &Request) -> Result<Request, Response> {
        Ok(request.clone())
    }
}

impl Request {
    /// A request for `App::request`; `path` may carry a query string.
    /// What is not valid HTTP is kept as given and makes `App::request` a
    /// 500.
    pub fn new(method: &str, path: &str) -> Request {
        let (path, query) = match path.split_once('?') {
            Some((path, query)) => (path, query),
            None => (path, ""),
        };
        Request {
            method: method.to_string(),
            path: path.to_string(),
            query: query.to_string(),
            headers: Vec::new(),
            body: Bytes::new(),
            routed: None,
        }
    }

    pub fn method(&self) -> String {
        self.method.clone()
    }

    /// The path, without its query string.
    pub fn path(&self) -> String {
        self.path.clone()
    }

    /// The header `name`, matched without regard to case.
    pub fn header(&self, name: &str) -> Option<String> {
        find_header(&self.headers, name)
    }

    /// The cookie `name` from the `cookie` header.
    pub fn cookie(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("cookie"))
            .flat_map(|(_, value)| value.split(';'))
            .find_map(|pair| {
                let (key, value) = pair.trim().split_once('=')?;
                (key == name).then(|| value.to_string())
            })
    }

    /// The body as text; empty when it is not UTF-8.
    pub fn body(&self) -> String {
        match std::str::from_utf8(&self.body) {
            Ok(text) => text.to_string(),
            Err(_) => String::new(),
        }
    }

    /// Sets the header `name`, replacing one of the same name.
    pub fn set_header(&mut self, name: &str, value: &str) {
        put_header(&mut self.headers, name, value);
    }

    pub fn set_body(&mut self, text: &str) {
        self.body = Bytes::from(text.to_string());
    }

    /// The path parameter `name` as a `P`; a 400 naming it when it does
    /// not read as one.
    pub fn param<P: Plain>(&self, name: &str) -> Result<P, Response> {
        let found = self
            .routed
            .as_ref()
            .and_then(|routed| routed.params.iter().find(|(key, _)| key == name));
        match found {
            Some((_, text)) => read("path parameter", name, text),
            None => Err(internal_because(format!(
                "the route has no path parameter `{name}`; this is a bug in the compiler, please report it"
            ))),
        }
    }

    /// The query parameter `name` as a `Q`: a 400 naming it when it does
    /// not read as one, or when a required one is missing.
    pub fn query<Q: QueryValue>(&self, name: &str) -> Result<Q, Response> {
        let found = form_urlencoded::parse(self.query.as_bytes())
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned());
        Q::from_query(name, found)
    }

    /// The body read from JSON as a `B`; a 400 with the parser's message,
    /// naming `name`, the handler's parameter, when it is not one.
    pub async fn json<B: varyk_std::serde::de::DeserializeOwned>(
        &self,
        name: &str,
    ) -> Result<B, Response> {
        let text = match std::str::from_utf8(&self.body) {
            Ok(text) => text,
            Err(_) => {
                return Err(error_body(
                    400,
                    &format!("the body `{name}` is not valid: it is not UTF-8 text"),
                ));
            }
        };
        varyk_std::json::parse(text).map_err(|err| {
            error_body(
                400,
                &format!("the body `{name}` is not valid: {}", err.message()),
            )
        })
    }

    /// The app's state, as the `S` the routes were checked against.
    pub fn state<S: Send + Sync + 'static>(&self) -> Result<Arc<S>, Response> {
        let Some(routed) = &self.routed else {
            return Err(internal_because(
                "the request was not routed by an app, so it has no state".to_string(),
            ));
        };
        Arc::clone(&routed.state).downcast::<S>().map_err(|_| {
            internal_because(
                "the app's state is not the type its routes were checked against; this is a bug in the compiler, please report it"
                    .to_string(),
            )
        })
    }

    /// A value the package makes for this request, bound by its type.
    pub async fn bind<K: Bound>(&self) -> Result<K, Response> {
        K::bound(self).await
    }

    /// The request the router received, its body already read.
    pub(crate) fn received(
        parts: &axum::http::request::Parts,
        body: Bytes,
        routed: Arc<Routed>,
    ) -> Request {
        Request {
            method: parts.method.as_str().to_string(),
            path: parts.uri.path().to_string(),
            query: parts.uri.query().unwrap_or_default().to_string(),
            headers: header_list(&parts.headers),
            body,
            routed: Some(routed),
        }
    }

    /// Sends `response` through the early-response channel, once; `false`
    /// when there is no channel or it was used already (spec 6.3).
    pub(crate) fn send_early(&self, response: Response) -> bool {
        let Some(routed) = &self.routed else {
            return false;
        };
        let sender = match routed.early.lock() {
            Ok(mut slot) => slot.take(),
            Err(_) => None,
        };
        match sender {
            Some(sender) => sender.send(response).is_ok(),
            None => false,
        }
    }

    /// Where a live connection records its `Err` for a client that has
    /// gone; a record of its own for a request no app routed.
    pub(crate) fn client_gone(&self) -> ClientGone {
        match &self.routed {
            Some(routed) => routed.client_gone(),
            None => Arc::new(Mutex::new(None)),
        }
    }

    /// The multipart body the route wrapper kept, once; none for any
    /// other request, and once it was taken.
    pub(crate) fn take_upload(&self) -> Option<Upload> {
        let routed = self.routed.as_ref()?;
        match routed.upload.lock() {
            Ok(mut slot) => slot.take(),
            Err(_) => None,
        }
    }

    /// The WebSocket upgrade the route wrapper kept, once; none for a
    /// request that is not one, every request `App::request` sends
    /// included, and once it was taken.
    pub(crate) fn take_upgrade(&self) -> Option<Upgrade> {
        let routed = self.routed.as_ref()?;
        match routed.upgrade.lock() {
            Ok(mut slot) => slot.take(),
            Err(_) => None,
        }
    }

    /// The request as `http` has it, for `App::request`; the reason when
    /// what was given is not valid HTTP.
    pub(crate) fn into_http(self) -> Result<axum::http::Request<Body>, String> {
        let method = Method::from_bytes(self.method.as_bytes())
            .map_err(|_| format!("the method {:?} is not valid HTTP", self.method))?;
        if !self.path.starts_with('/') {
            return Err(format!("the path {:?} does not start with `/`", self.path));
        }
        let target = if self.query.is_empty() {
            self.path.clone()
        } else {
            format!("{}?{}", self.path, self.query)
        };
        let uri = Uri::try_from(target.as_str())
            .map_err(|_| format!("the path {target:?} is not valid HTTP"))?;
        let headers = header_map(&self.headers)?;
        let mut request = axum::http::Request::new(Body::from(self.body));
        *request.method_mut() = method;
        *request.uri_mut() = uri;
        *request.headers_mut() = headers;
        Ok(request)
    }
}

impl Response {
    /// A 200 with `value` as JSON.
    pub fn json<T: varyk_std::serde::Serialize + ?Sized>(value: &T) -> Response {
        Response {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Content::Text(varyk_std::json::stringify(value)),
            problem: None,
            hidden: None,
        }
    }

    /// A 200 with `s` as plain text.
    pub fn text(s: &str) -> Response {
        Response {
            status: 200,
            headers: vec![(
                "content-type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            )],
            body: Content::Text(s.to_string()),
            problem: None,
            hidden: None,
        }
    }

    /// A 204 with no body.
    pub fn empty() -> Response {
        Response {
            status: 204,
            headers: Vec::new(),
            body: Content::Text(String::new()),
            problem: None,
            hidden: None,
        }
    }

    /// The file `name` in the folder `dir`, a 200 with the content type of
    /// its extension, read when the response is sent. A name that is
    /// absolute, has an empty part or one starting with `.`, or leads out
    /// of the folder, links followed, is a 404 as a missing file is, the
    /// reason logged at debug level (spec 2.3).
    pub fn file(dir: &str, name: &str) -> Response {
        match open_in(dir, name) {
            Ok(file) => Response {
                status: 200,
                headers: vec![
                    ("content-type".to_string(), content_type(name).to_string()),
                    ("x-content-type-options".to_string(), "nosniff".to_string()),
                ],
                body: Content::File(file),
                problem: None,
                hidden: None,
            },
            Err(reason) => {
                // Escaped: the name may be the client's (spec 3.1).
                varyk_std::tracing::debug!("the file {name:?} in {dir:?} is not served: {reason}");
                error_body(404, "not found")
            }
        }
    }

    /// Sets the status; one outside 100 to 599 makes the response a 500
    /// when it is sent.
    pub fn set_status(&mut self, code: u16) {
        self.status = code;
    }

    /// Sets the header `name`, replacing one of the same name; a name or
    /// value that is not valid HTTP makes the response a 500 when it is
    /// sent.
    pub fn set_header(&mut self, name: &str, value: &str) {
        put_header(&mut self.headers, name, value);
    }

    /// Adds a cookie with `Path=/`, `HttpOnly`, `Secure`, and
    /// `SameSite=Lax`, kept for `max_age` seconds; `0` deletes it. A name
    /// that is not a token, or a value with a character a cookie cannot
    /// hold (`;`, `,`, a space, a quote), makes the response a 500 when it
    /// is sent, so no value can add attributes.
    pub fn set_cookie(&mut self, name: &str, value: &str, max_age: i64) {
        if !is_token(name) || !value.bytes().all(is_cookie_octet) {
            if self.problem.is_none() {
                self.problem = Some(format!(
                    "the cookie {name:?} has a name or a value a cookie cannot hold"
                ));
            }
            return;
        }
        self.headers.push((
            "set-cookie".to_string(),
            format!("{name}={value}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Lax"),
        ));
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    /// The header `name`, matched without regard to case.
    pub fn header(&self, name: &str) -> Option<String> {
        find_header(&self.headers, name)
    }

    /// The body as text: a file's content read whole; empty when it
    /// cannot be read or is not UTF-8, and for an event stream a handler
    /// holds.
    pub fn body(&self) -> String {
        match &self.body {
            Content::Text(text) => text.clone(),
            Content::Events(_) => String::new(),
            Content::File(file) => {
                let mut file = file;
                let mut read = Vec::new();
                let whole = file
                    .seek(SeekFrom::Start(0))
                    .and_then(|_| file.read_to_end(&mut read));
                match whole {
                    Ok(_) => String::from_utf8(read).unwrap_or_default(),
                    Err(_) => String::new(),
                }
            }
        }
    }

    /// The body read from JSON as a `T`.
    pub fn read_json<T: varyk_std::serde::de::DeserializeOwned>(
        &self,
    ) -> Result<T, varyk_std::Error> {
        varyk_std::json::parse(&self.body())
    }

    /// A 500's message, never sent.
    pub(crate) fn hidden_message(&self) -> Option<&str> {
        self.hidden.as_deref()
    }

    /// The response as axum sends it, for the request `method` and `path`:
    /// a 500's message is logged, and a response that cannot be sent as
    /// built is the fixed 500 with the reason logged.
    pub(crate) fn into_sent(self, method: &str, path: &str) -> axum::response::Response {
        let path = logged_path(path);
        // Escaped, as `{:?}` writes a string, so a line break in it
        // cannot start a new log line (spec 3.1).
        if let Some(message) = &self.hidden {
            varyk_std::tracing::error!("{method} {path}: {message:?}");
        }
        match self.into_http() {
            Ok(response) => response,
            Err(reason) => {
                varyk_std::tracing::error!(
                    "{method} {path}: the response cannot be sent: {reason}"
                );
                internal_http()
            }
        }
    }

    fn into_http(self) -> Result<axum::response::Response, String> {
        if let Some(problem) = self.problem {
            return Err(problem);
        }
        if !(100..=599).contains(&self.status) {
            return Err(format!(
                "the status {} is not between 100 and 599",
                self.status
            ));
        }
        let status = StatusCode::from_u16(self.status)
            .map_err(|_| format!("the status {} is not valid HTTP", self.status))?;
        let headers = header_map(&self.headers)?;
        let body = match self.body {
            Content::Text(text) => Body::from(text),
            Content::File(mut file) => {
                file.seek(SeekFrom::Start(0))
                    .map_err(|err| format!("the file cannot be read: {err}"))?;
                Body::from_stream(chunks(tokio::fs::File::from_std(file)))
            }
            Content::Events(events) => Body::from_stream(stream_of(events)),
        };
        let mut response = axum::response::Response::new(body);
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        Ok(response)
    }

    /// The response axum gave, its body read whole, for `App::request`:
    /// an event stream's events once its handler returns.
    pub(crate) async fn received(response: axum::response::Response) -> Response {
        let (parts, body) = response.into_parts();
        let body = match axum::body::to_bytes(body, usize::MAX).await {
            Ok(bytes) => bytes.to_vec(),
            Err(_) => Vec::new(),
        };
        Response::read_back(parts.status.as_u16(), &parts.headers, body)
    }

    /// A response received whole, by `App::request` or the client; its
    /// body empty when it is not UTF-8.
    pub(crate) fn read_back(status: u16, headers: &HeaderMap, body: Vec<u8>) -> Response {
        Response {
            status,
            headers: header_list(headers),
            body: Content::Text(String::from_utf8(body).unwrap_or_default()),
            problem: None,
            hidden: None,
        }
    }

    /// The `text/event-stream` response an `Sse` sends its events into.
    pub(crate) fn events(events: mpsc::Receiver<String>) -> Response {
        Response {
            status: 200,
            headers: vec![
                ("content-type".to_string(), "text/event-stream".to_string()),
                ("cache-control".to_string(), "no-cache".to_string()),
            ],
            body: Content::Events(events),
            problem: None,
            hidden: None,
        }
    }
}

/// A handler that returns nothing: a 204.
pub fn respond_empty() -> Response {
    Response::empty()
}

/// A handler's value: a 200 with its JSON.
pub fn respond_json<T: varyk_std::serde::Serialize + ?Sized>(value: &T) -> Response {
    Response::json(value)
}

/// A handler's `Option`: a 200 with its JSON, or a 404 for `None`.
pub fn respond_option<T: varyk_std::serde::Serialize>(value: Option<T>) -> Response {
    match value {
        Some(value) => Response::json(&value),
        None => error_body(404, "not found"),
    }
}

/// `path` as a log line carries it (spec 3.1): every byte that is not
/// printable ASCII percent-encoded, so text a client sent, such as a
/// Unicode line separator, cannot reach the log raw.
pub(crate) fn logged_path(path: &str) -> String {
    let mut logged = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_graphic() {
            logged.push(char::from(byte));
        } else {
            logged.push_str(&format!("%{byte:02X}"));
        }
    }
    logged
}

/// A handler's or a `before` hook's `Err`: with a status from 400 to 599,
/// that status and `{"error": message}`; otherwise a 500 with a fixed
/// body, the message kept to be logged with the method and path, never
/// sent.
pub fn respond_error(error: varyk_std::Error) -> Response {
    match error.status() {
        Some(status) if (400..=599).contains(&status) => error_body(status, error.message()),
        Some(status) => internal_because(format!(
            "{} (its status {status} is not between 400 and 599, so it is sent as a 500)",
            error.message()
        )),
        None => internal_because(error.message().to_string()),
    }
}

/// A `before` hook's result: `None` lets the request through, `Some`
/// answers it.
pub fn respond_before(result: Result<bool, varyk_std::Error>) -> Option<Response> {
    match result {
        Ok(true) => None,
        Ok(false) => Some(error_body(403, "forbidden")),
        Err(error) => Some(respond_error(error)),
    }
}

/// `{"error": message}` with `status`.
pub(crate) fn error_body(status: u16, message: &str) -> Response {
    let mut body = std::collections::BTreeMap::new();
    body.insert("error", message);
    let mut response = Response::json(&body);
    response.status = status;
    response
}

/// The 500 with the fixed body, for a failure whose message is never
/// sent.
pub(crate) fn internal() -> Response {
    error_body(500, "internal error")
}

/// The fixed 500, with `message` to be logged when it is sent.
pub(crate) fn internal_because(message: String) -> Response {
    let mut response = internal();
    response.hidden = Some(message);
    response
}

/// The fixed 500 as axum sends it, for a response that cannot be, and
/// for a panic outside a handler.
pub(crate) fn internal_http() -> axum::response::Response {
    let mut response = axum::response::Response::new(Body::from(r#"{"error":"internal error"}"#));
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// Whether a `content-type` is `multipart/form-data`, whose body the
/// route wrapper keeps as a stream (spec 6.2).
pub(crate) fn is_multipart(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("multipart/form-data"))
}

/// The reason `name` is not a plain name of a file in a folder: empty,
/// absolute, or with a part that is empty or starts with `.` (so `.` and
/// `..` too). The rule of `Response::file` and `save_to`.
pub(crate) fn plain_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("the name is empty".to_string());
    }
    if name.starts_with(['/', '\\']) || Path::new(name).is_absolute() {
        return Err("the name is absolute".to_string());
    }
    for part in name.split(['/', '\\']) {
        if part.is_empty() {
            return Err("the name has an empty part".to_string());
        }
        if part.starts_with('.') {
            return Err("a part of the name starts with `.`".to_string());
        }
    }
    if !Path::new(name)
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err("the name is not a plain path".to_string());
    }
    Ok(())
}

/// The folder `dir`, links followed.
pub(crate) fn folder(dir: &str) -> Result<PathBuf, String> {
    std::fs::canonicalize(dir).map_err(|err| format!("the folder cannot be found: {err}"))
}

/// The file `name` in the folder `dir`, opened; the reason when the name
/// is not plain, leads out of the folder (links followed), or is not a
/// file that can be opened.
fn open_in(dir: &str, name: &str) -> Result<std::fs::File, String> {
    plain_name(name)?;
    let folder = folder(dir)?;
    let path = std::fs::canonicalize(folder.join(name))
        .map_err(|err| format!("the file cannot be found: {err}"))?;
    if !path.starts_with(&folder) {
        return Err("the name leads out of the folder".to_string());
    }
    let file =
        std::fs::File::open(&path).map_err(|err| format!("the file cannot be opened: {err}"))?;
    let is_file = file
        .metadata()
        .map_err(|err| format!("the file cannot be read: {err}"))?
        .is_file();
    if !is_file {
        return Err("the name is not a file".to_string());
    }
    Ok(file)
}

/// The content type of the file `name`, from its extension: a fixed table
/// of common ones, `application/octet-stream` for any other (spec 2.3).
fn content_type(name: &str) -> &'static str {
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// `file` in chunks, read as the response is sent; the stream ends after
/// a read error, which is logged.
fn chunks(
    file: tokio::fs::File,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    futures_util::stream::unfold(Some(file), |file| async move {
        let mut file = file?;
        let mut buffer = vec![0; CHUNK];
        match file.read(&mut buffer).await {
            Ok(0) => None,
            Ok(n) => {
                buffer.truncate(n);
                Some((Ok(Bytes::from(buffer)), Some(file)))
            }
            Err(err) => {
                varyk_std::tracing::error!("a file being sent cannot be read: {err}");
                Some((Err(err), None))
            }
        }
    })
}

/// The events of an `Sse` as they are sent, with a comment line after
/// every quiet `KEEP_ALIVE`; the stream ends when the `Sse` is dropped,
/// once its handler returns.
fn stream_of(
    events: mpsc::Receiver<String>,
) -> impl Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static {
    futures_util::stream::unfold(events, |mut events| async move {
        match tokio::time::timeout(KEEP_ALIVE, events.recv()).await {
            Ok(Some(event)) => Some((Ok(Bytes::from(event)), events)),
            Ok(None) => None,
            Err(_) => Some((Ok(Bytes::from_static(b": keep-alive\n\n")), events)),
        }
    })
}

/// `text` read as the `T` of the parameter `name`; a 400 naming it.
fn read<T: Plain>(what: &str, name: &str, text: &str) -> Result<T, Response> {
    text.parse::<T>().map_err(|_| {
        error_body(
            400,
            &format!("the {what} `{name}` cannot be read from `{text}`"),
        )
    })
}

fn find_header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

fn put_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    headers.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
    headers.push((name.to_string(), value.to_string()));
}

/// The headers of `map` whose values are UTF-8 text.
fn header_list(map: &HeaderMap) -> Vec<(String, String)> {
    map.iter()
        .filter_map(|(name, value)| {
            let value = std::str::from_utf8(value.as_bytes()).ok()?;
            Some((name.as_str().to_string(), value.to_string()))
        })
        .collect()
}

/// `headers` as `http` has them; the reason when one is not valid HTTP.
fn header_map(headers: &[(String, String)]) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let key = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("the header name {name:?} is not valid HTTP"))?;
        let value = HeaderValue::from_bytes(value.as_bytes())
            .map_err(|_| format!("the value of the header {name:?} is not valid HTTP"))?;
        map.append(key, value);
    }
    Ok(map)
}

/// A cookie name: an HTTP token.
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// A byte a cookie value may hold (RFC 6265 cookie-octet).
fn is_cookie_octet(b: u8) -> bool {
    matches!(b, 0x21 | 0x23..=0x2B | 0x2D..=0x3A | 0x3C..=0x5B | 0x5D..=0x7E)
}

#[cfg(test)]
mod tests {
    use super::logged_path;

    #[test]
    fn a_logged_path_keeps_printable_ascii_and_encodes_the_rest() {
        assert_eq!(logged_path("/users/1%0A"), "/users/1%0A");
        assert_eq!(logged_path("/a\u{2028}b"), "/a%E2%80%A8b");
        assert_eq!(logged_path("/caf\u{e9} x\t"), "/caf%C3%A9%20x%09");
    }
}
