// `App`, the route table: its routes and hooks, its settings and the
// layers around the router, `serve`, and `request`, and the wrappers the
// compiler's adapters are given to it in (varyk-http spec 2.1, 2.2, 6.1,
// 6.3; milestone 5b4 spec 6.1).

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::error_handling::HandleErrorLayer;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, RawPathParams};
use axum::http::request::Parts;
use axum::http::{HeaderValue, Uri};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::routing::{MethodFilter, MethodRouter};
use futures_util::StreamExt;
use futures_util::future::{Either, select};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::sync::{Semaphore, oneshot};
use tokio::task::{AbortHandle, JoinError};
use tokio::time::Instant;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower::load_shed::LoadShedLayer;
use tower::{ServiceBuilder, ServiceExt};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::message::{
    Request, Response, Routed, State, error_body, internal, internal_because, internal_http,
};
use crate::metrics::{Metrics, Uncounted};

type Boxed<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type RouteRun = Arc<dyn Fn(Request) -> Boxed<Response> + Send + Sync>;
type BeforeRun = Arc<dyn Fn(Request) -> Boxed<Option<Response>> + Send + Sync>;
type AfterRun = Arc<dyn Fn(Request, Response) -> Boxed<Response> + Send + Sync>;

/// The default body limit, 2 MiB.
const BODY_LIMIT: u64 = 2 * 1024 * 1024;
/// The default request timeout, 30 seconds.
const TIMEOUT_MS: u64 = 30_000;
/// The default shutdown grace, 30 seconds.
const SHUTDOWN_GRACE_MS: u64 = 30_000;
/// The default address: this computer only.
const ADDRESS: &str = "127.0.0.1";
/// The default idle timeout, how long a client has to send a request's
/// headers, on a new connection and between the requests of a
/// kept-alive one (spec 3, 6.4): 75 seconds, nginx's own keep-alive
/// timeout, longer than the 60 seconds a load balancer commonly keeps an
/// idle connection to the service, so the load balancer closes it first.
const IDLE_TIMEOUT_MS: u64 = 75_000;
/// How long `serve` waits after a failure to accept a connection.
const ACCEPT_PAUSE: Duration = Duration::from_secs(1);
/// How often `serve` runs the metrics recorder's upkeep.
const UPKEEP: Duration = Duration::from_secs(5);

/// A route's adapter, wrapped by `route` for `App::get` and the others.
pub struct Route {
    run: RouteRun,
}

/// A `before` adapter, wrapped by `before_hook`: `None` lets the request
/// through, `Some` answers it.
pub struct BeforeHook {
    run: BeforeRun,
}

/// An `after` adapter, wrapped by `after_hook`: gives the response on.
pub struct AfterHook {
    run: AfterRun,
}

/// The route table: the app's state, its routes and hooks in the order
/// they were registered, and its settings as they were given. Nothing is
/// built, and no setting checked, until `serve` or `request`.
pub struct App {
    state: State,
    routes: Vec<Registered>,
    befores: Vec<Before>,
    afters: Vec<AfterRun>,
    address: String,
    body_limit: u64,
    timeout_ms: u64,
    shutdown_grace_ms: u64,
    idle_timeout_ms: u64,
    max_in_flight: Option<u64>,
    origins: Vec<String>,
    compress: bool,
    metrics: Option<MetricsSetting>,
}

/// `metrics(path)`: the path, and the app's recorder, made at the first
/// call and kept, so its counts outlive one router.
struct MetricsSetting {
    path: String,
    recorder: Result<Arc<Metrics>, String>,
}

/// A route as registered: its method, its path as the program wrote it,
/// and its adapter.
struct Registered {
    method: Method,
    pattern: String,
    run: RouteRun,
}

/// A `before` hook as registered: for every route, or for the routes
/// under `prefix`.
struct Before {
    prefix: Option<String>,
    run: BeforeRun,
}

#[derive(Clone, Copy, PartialEq)]
enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    fn name(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }

    fn filter(self) -> MethodFilter {
        match self {
            Method::Get => MethodFilter::GET,
            Method::Post => MethodFilter::POST,
            Method::Put => MethodFilter::PUT,
            Method::Patch => MethodFilter::PATCH,
            Method::Delete => MethodFilter::DELETE,
        }
    }
}

/// The route pattern, as the program wrote it, on every response a route
/// wrapper makes, for the metrics layer; a response without one is
/// `unmatched` (spec 6.1).
#[derive(Clone)]
pub(crate) struct RoutePattern(pub(crate) Arc<str>);

impl App {
    /// An app whose handlers read `state`.
    pub fn new<S: Send + Sync + 'static>(state: Arc<S>) -> App {
        App {
            state,
            routes: Vec::new(),
            befores: Vec::new(),
            afters: Vec::new(),
            address: ADDRESS.to_string(),
            body_limit: BODY_LIMIT,
            timeout_ms: TIMEOUT_MS,
            shutdown_grace_ms: SHUTDOWN_GRACE_MS,
            idle_timeout_ms: IDLE_TIMEOUT_MS,
            max_in_flight: None,
            origins: Vec::new(),
            compress: false,
            metrics: None,
        }
    }

    pub fn get(&mut self, path: &str, route: Route) {
        self.add(Method::Get, path, route);
    }

    pub fn post(&mut self, path: &str, route: Route) {
        self.add(Method::Post, path, route);
    }

    pub fn put(&mut self, path: &str, route: Route) {
        self.add(Method::Put, path, route);
    }

    pub fn patch(&mut self, path: &str, route: Route) {
        self.add(Method::Patch, path, route);
    }

    pub fn delete(&mut self, path: &str, route: Route) {
        self.add(Method::Delete, path, route);
    }

    pub fn before(&mut self, hook: BeforeHook) {
        self.befores.push(Before {
            prefix: None,
            run: hook.run,
        });
    }

    pub fn before_on(&mut self, prefix: &str, hook: BeforeHook) {
        self.befores.push(Before {
            prefix: Some(prefix.to_string()),
            run: hook.run,
        });
    }

    pub fn after(&mut self, hook: AfterHook) {
        self.afters.push(hook.run);
    }

    // The settings (spec 2.1). Each is recorded as given and checked
    // when the app is built, so a value that cannot work makes `serve` an
    // `Err` and `request` a 500, never a panic; the last call wins, and
    // `allow_origin` adds.

    /// The address to listen on, `127.0.0.1` until set; `0.0.0.0` in a
    /// container.
    pub fn set_address(&mut self, addr: &str) {
        self.address = addr.to_string();
    }

    /// The largest request body, in bytes; a larger one is a 413.
    pub fn set_body_limit(&mut self, bytes: u64) {
        self.body_limit = bytes;
    }

    /// How long a request may take, in milliseconds, until its response;
    /// a longer one is a 503.
    pub fn set_timeout(&mut self, ms: u64) {
        self.timeout_ms = ms;
    }

    /// How long requests in flight have to finish once a shutdown is
    /// asked for, in milliseconds.
    pub fn set_shutdown_grace(&mut self, ms: u64) {
        self.shutdown_grace_ms = ms;
    }

    /// How long a connection may wait for a request's headers, in
    /// milliseconds, idle between kept-alive requests or still sending
    /// them; it is then closed.
    pub fn set_idle_timeout(&mut self, ms: u64) {
        self.idle_timeout_ms = ms;
    }

    /// At most `n` requests at once; one beyond them is a 503.
    pub fn set_max_in_flight(&mut self, n: u64) {
        self.max_in_flight = Some(n);
    }

    /// Allows the CORS origin `origin`, such as `https://app.example`;
    /// `"*"` allows any.
    pub fn allow_origin(&mut self, origin: &str) {
        self.origins.push(origin.to_string());
    }

    /// Compresses responses with gzip for a client that accepts it.
    pub fn compress(&mut self) {
        self.compress = true;
    }

    /// Serves Prometheus text at `path`.
    pub fn metrics(&mut self, path: &str) {
        let recorder = match self.metrics.take() {
            Some(earlier) => earlier.recorder,
            None => Metrics::new().map(Arc::new),
        };
        self.metrics = Some(MetricsSetting {
            path: path.to_string(),
            recorder,
        });
    }

    /// Serves on `port` until SIGTERM or ctrl-c, then gives requests in
    /// flight the shutdown grace (spec 2.1, 6.4).
    pub async fn serve(&self, port: u16) -> Result<bool, varyk_std::Error> {
        let ip = self.address.parse::<IpAddr>().ok();
        // An IPv6 address is shown bracketed, as in a URL: `[::1]:80`.
        let place = match ip {
            Some(ip) => SocketAddr::new(ip, port).to_string(),
            None => format!("{}:{port}", self.address),
        };
        let cannot =
            |reason: String| varyk_std::Error::new(format!("cannot serve on {place}: {reason}"));
        let router = self.service().map_err(cannot)?;
        // `service` refused an address that is not an IP address.
        let Some(ip) = ip else {
            return Err(cannot("the address is not an IP address".to_string()));
        };
        let listener = tokio::net::TcpListener::bind(SocketAddr::new(ip, port))
            .await
            .map_err(|err| cannot(err.to_string()))?;
        let hint = if ip == IpAddr::from(Ipv4Addr::LOCALHOST) {
            " (set_address(\"0.0.0.0\") to accept outside connections)"
        } else {
            ""
        };
        // The address bound, so `serve(0)` shows the port it was given.
        let bound = match listener.local_addr() {
            Ok(bound) => bound.to_string(),
            Err(_) => place.clone(),
        };
        varyk_std::tracing::info!("listening on http://{bound}{hint}");
        let _upkeep = self.metrics.as_ref().and_then(|setting| {
            let recorder = Arc::clone(setting.recorder.as_ref().ok()?);
            let task = tokio::spawn(async move {
                let mut every = tokio::time::interval(UPKEEP);
                loop {
                    every.tick().await;
                    recorder.upkeep();
                }
            });
            Some(AbortOnDrop(Some(task.abort_handle())))
        });
        // Each connection is served by hyper with the idle timeout as its
        // header-read timeout, so a client that sends its headers slowly,
        // or nothing, or sits idle between requests, cannot hold a
        // connection open (spec 3).
        // The signal stops accepting and starts the grace: hyper finishes
        // the requests in flight and closes idle connections. A connection
        // that became a WebSocket is the handler's, not tracked here; it
        // runs on until the runtime ends, at once in a program whose
        // `main` returns after `serve`.
        // Each connection holds a receiver of `stopping`: the signal is
        // sent through it, and `closed` tells when every connection ended.
        let (stopping, stop_watch) = tokio::sync::watch::channel(false);
        let mut stop = pin!(stop_signal());
        loop {
            // The signal is polled first, and the pause after a failure is
            // raced against it, so `serve` stops on a signal even while
            // every accept fails (out of file descriptors, say).
            let accepted = async {
                match listener.accept().await {
                    Ok((stream, _)) => Some(stream),
                    Err(err) if one_connection(&err) => {
                        varyk_std::tracing::debug!("a connection failed as it was accepted: {err}");
                        None
                    }
                    Err(err) => {
                        // Wait a moment, as axum's own server does, rather
                        // than spin.
                        varyk_std::tracing::error!("a connection cannot be accepted: {err}");
                        tokio::time::sleep(ACCEPT_PAUSE).await;
                        None
                    }
                }
            };
            match select(stop.as_mut(), pin!(accepted)).await {
                Either::Left(((), _)) => break,
                Either::Right((None, _)) => {}
                Either::Right((Some(stream), _)) => {
                    let io = TokioIo::new(stream);
                    let service = TowerToHyperService::new(router.clone());
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder
                        .timer(TokioTimer::new())
                        .header_read_timeout(Duration::from_millis(self.idle_timeout_ms));
                    let connection = builder.serve_connection(io, service).with_upgrades();
                    tokio::spawn(connect(connection, stop_watch.clone()));
                }
            }
        }
        drop(listener);
        drop(stop_watch);
        varyk_std::tracing::info!(
            "shutting down: requests in flight have {} ms to finish",
            self.shutdown_grace_ms
        );
        stopping.send_replace(true);
        // A second signal ends the grace at once.
        let grace = pin!(tokio::time::sleep(Duration::from_millis(
            self.shutdown_grace_ms
        )));
        let again = pin!(stop_signal());
        match select(pin!(stopping.closed()), select(grace, again)).await {
            Either::Left(((), _)) => {
                varyk_std::tracing::info!("stopped: every request was answered");
            }
            Either::Right((Either::Left(_), _)) => {
                varyk_std::tracing::info!(
                    "stopped: the shutdown grace ended with requests still in flight"
                );
            }
            Either::Right((Either::Right(_), _)) => {
                varyk_std::tracing::info!(
                    "stopped: a second signal ended the shutdown grace with requests still in flight"
                );
            }
        }
        Ok(true)
    }

    /// Runs `req` through the app with no port and gives the response the
    /// server would have sent.
    pub async fn request(&self, req: crate::message::Request) -> crate::message::Response {
        let router = match self.service() {
            Ok(router) => router,
            Err(reason) => {
                varyk_std::tracing::error!("the app cannot answer requests: {reason}");
                return internal();
            }
        };
        let sent = match req.into_http() {
            Ok(sent) => sent,
            Err(reason) => {
                varyk_std::tracing::error!("the request cannot be sent: {reason}");
                return internal();
            }
        };
        match router.oneshot(sent).await {
            Ok(response) => Response::received(response).await,
            Err(never) => match never {},
        }
    }

    fn add(&mut self, method: Method, path: &str, route: Route) {
        self.routes.push(Registered {
            method,
            pattern: path.to_string(),
            run: route.run,
        });
    }

    /// The router with its layers, as `serve` and `request` run it: the
    /// request log outermost, then metrics, catch-panic, the in-flight
    /// limit, CORS, and compression (spec 6.1). The reason when a setting
    /// cannot work or the routes conflict.
    fn service(&self) -> Result<axum::Router, String> {
        self.check_settings()?;
        let cors = cors(&self.origins)?;
        let metrics = match &self.metrics {
            Some(setting) => {
                let recorder = setting
                    .recorder
                    .as_ref()
                    .map_err(|err| format!("metrics({:?}) cannot work: {err}", setting.path))?;
                Some((setting.path.as_str(), Arc::clone(recorder)))
            }
            None => None,
        };
        let mut router = self.router(metrics.as_ref().map(|(path, m)| (*path, m)))?;
        // `Router::layer` wraps each route and the fallbacks: the last
        // layer given is the outermost.
        if self.compress {
            router = router.layer(CompressionLayer::new());
        }
        if let Some(cors) = cors {
            router = router.layer(cors);
        }
        if let Some(n) = self.max_in_flight {
            // `check_settings` keeps `n` within what a semaphore holds.
            let permits = usize::try_from(n).unwrap_or(Semaphore::MAX_PERMITS);
            router = router.layer(
                ServiceBuilder::new()
                    .layer(HandleErrorLayer::new(busy))
                    .layer(LoadShedLayer::new())
                    .layer(GlobalConcurrencyLimitLayer::new(permits)),
            );
        }
        router = router.layer(CatchPanicLayer::custom(panicked));
        if let Some((_, recorder)) = metrics {
            router = router.layer(from_fn_with_state(recorder, crate::metrics::count));
        }
        Ok(router.layer(from_fn(request_log)))
    }

    /// The reason the first setting that cannot work cannot, naming it.
    fn check_settings(&self) -> Result<(), String> {
        let cannot = |setting: String, why: &str| Err(format!("{setting} cannot work: {why}"));
        if self.address.parse::<std::net::IpAddr>().is_err() {
            return cannot(
                format!("set_address({:?})", self.address),
                "the address is not an IP address such as 127.0.0.1 or 0.0.0.0",
            );
        }
        if self.body_limit == 0 {
            return cannot(
                "set_body_limit(0)".to_string(),
                "the limit is at least 1 byte",
            );
        }
        if self.timeout_ms == 0 {
            return cannot(
                "set_timeout(0)".to_string(),
                "the timeout is at least 1 millisecond",
            );
        }
        if self.shutdown_grace_ms == 0 {
            return cannot(
                "set_shutdown_grace(0)".to_string(),
                "the grace is at least 1 millisecond",
            );
        }
        if self.idle_timeout_ms == 0 {
            return cannot(
                "set_idle_timeout(0)".to_string(),
                "the timeout is at least 1 millisecond",
            );
        }
        if let Some(n) = self.max_in_flight {
            let fits = usize::try_from(n).is_ok_and(|n| n <= Semaphore::MAX_PERMITS);
            if n == 0 || !fits {
                return cannot(
                    format!("set_max_in_flight({n})"),
                    &format!("the limit is from 1 to {}", Semaphore::MAX_PERMITS),
                );
            }
        }
        if let Some(setting) = &self.metrics {
            let path = &setting.path;
            match positional(path) {
                Ok((_, names)) if names.is_empty() => {}
                _ => {
                    return cannot(
                        format!("metrics({path:?})"),
                        "the path is a route path without `{name}` parts, such as \"/metrics\"",
                    );
                }
            }
        }
        Ok(())
    }

    /// The router of the routes and hooks as they are now, and of the
    /// metrics path when it is set: each path given to axum with its
    /// parameters named by position, after the conflict check, so axum
    /// never panics on them (spec 6.1). The reason when the routes
    /// conflict.
    fn router(&self, metrics: Option<(&str, &Arc<Metrics>)>) -> Result<axum::Router, String> {
        let afters = Arc::new(self.afters.clone());
        let deadline = Duration::from_millis(self.timeout_ms);
        let mut matcher = matchit::Router::new();
        // Each distinct positional path, with its routes' indexes.
        let mut paths: Vec<(String, Vec<usize>)> = Vec::new();
        let mut names = Vec::new();
        for (index, route) in self.routes.iter().enumerate() {
            let (positional, params) = positional(&route.pattern)?;
            names.push(params);
            let earlier = paths.iter_mut().find(|(path, _)| *path == positional);
            match earlier {
                Some((_, indexes)) => {
                    if let Some(other) = indexes
                        .iter()
                        .filter_map(|i| self.routes.get(*i))
                        .find(|other| other.method == route.method)
                    {
                        return Err(format!(
                            "the routes {} {} and {} {} match the same requests",
                            other.method.name(),
                            other.pattern,
                            route.method.name(),
                            route.pattern
                        ));
                    }
                    indexes.push(index);
                }
                None => {
                    matcher.insert(positional.clone(), ()).map_err(|err| {
                        format!(
                            "the route {} {} conflicts with another: {err}",
                            route.method.name(),
                            route.pattern
                        )
                    })?;
                    paths.push((positional, vec![index]));
                }
            }
        }
        if let Some((path, _)) = metrics {
            if matcher.insert(path, ()).is_err() {
                return Err(format!(
                    "metrics({path:?}) cannot work: a route has that path"
                ));
            }
        }
        let fallback = Arc::new(Fallback {
            state: Arc::clone(&self.state),
            afters: Arc::clone(&afters),
        });
        let mut router = axum::Router::new();
        if let Some((path, recorder)) = metrics {
            router = router.route(path, metrics_route(recorder));
        }
        for (positional, indexes) in paths {
            let mut methods = MethodRouter::new();
            let mut allow = Vec::new();
            for index in indexes {
                let (Some(route), Some(params)) = (self.routes.get(index), names.get(index)) else {
                    continue;
                };
                allow.push(route.method.name());
                if route.method == Method::Get {
                    allow.push("HEAD");
                }
                let wrapper = Arc::new(Wrapper {
                    pattern: Arc::from(route.pattern.as_str()),
                    names: params.clone(),
                    befores: self
                        .befores
                        .iter()
                        .filter(|before| covers(before.prefix.as_deref(), &route.pattern))
                        .map(|before| Arc::clone(&before.run))
                        .collect(),
                    afters: Arc::clone(&afters),
                    state: Arc::clone(&self.state),
                    run: Arc::clone(&route.run),
                    body_limit: self.body_limit,
                    timeout: deadline,
                });
                methods = methods.on(route.method.filter(), move |req: axum::extract::Request| {
                    Arc::clone(&wrapper).handle(req)
                });
            }
            let fallback = Arc::clone(&fallback);
            let allow = allow.join(",");
            methods = methods.fallback(move |req: axum::extract::Request| {
                let mut response = error_body(405, "method not allowed");
                response.set_header("allow", &allow);
                Arc::clone(&fallback).answer(req, response)
            });
            router = router.route(&positional, methods);
        }
        Ok(router.fallback(move |req: axum::extract::Request| {
            Arc::clone(&fallback).answer(req, error_body(404, "not found"))
        }))
    }
}

/// The metrics path: the recorder's text on `GET` (and `HEAD`), a 405 on
/// any other method, no hook, and not counted (spec 2.7).
fn metrics_route(recorder: &Arc<Metrics>) -> MethodRouter {
    let recorder = Arc::clone(recorder);
    MethodRouter::new()
        .on(MethodFilter::GET, move || {
            let recorder = Arc::clone(&recorder);
            async move { recorder.response() }
        })
        .fallback(|req: axum::extract::Request| async move {
            let mut response = error_body(405, "method not allowed");
            response.set_header("allow", "GET,HEAD");
            let mut sent = response.into_sent(req.method().as_str(), req.uri().path());
            sent.extensions_mut().insert(Uncounted);
            sent
        })
}

/// The CORS layer for the allowed `origins`, none when there are none
/// (spec 2.1): preflights for the five route methods and the headers
/// `content-type` and `authorization`, never credentials. The reason
/// when an origin cannot work.
fn cors(origins: &[String]) -> Result<Option<CorsLayer>, String> {
    if origins.is_empty() {
        return Ok(None);
    }
    let any = origins.iter().any(|origin| origin == "*");
    if let Some(other) = origins.iter().find(|origin| any && *origin != "*") {
        return Err(format!(
            "allow_origin(\"*\") cannot work beside allow_origin({other:?}): \"*\" already allows every origin"
        ));
    }
    let allowed = if any {
        AllowOrigin::any()
    } else {
        let mut values = Vec::new();
        for origin in origins {
            let value = url_origin(origin).ok_or_else(|| {
                format!(
                    "allow_origin({origin:?}) cannot work: an origin is a scheme, http or https, and a host in lower case, with an optional port, such as \"https://app.example\", or \"*\""
                )
            })?;
            values.push(value);
        }
        AllowOrigin::list(values)
    };
    Ok(Some(
        CorsLayer::new()
            .allow_origin(allowed)
            .allow_methods([
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::PUT,
                axum::http::Method::PATCH,
                axum::http::Method::DELETE,
            ])
            .allow_headers([
                axum::http::header::CONTENT_TYPE,
                axum::http::header::AUTHORIZATION,
            ]),
    ))
}

/// `origin` as a header value when it is a URL origin as a browser sends
/// it: `http` or `https`, `://`, and a host in lower case with an
/// optional port, nothing more.
fn url_origin(origin: &str) -> Option<HeaderValue> {
    let (scheme, rest) = origin.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let plain = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-.:[]".contains(&b);
    if rest.is_empty() || !rest.bytes().all(plain) {
        return None;
    }
    let uri = Uri::try_from(origin).ok()?;
    let authority = uri.authority()?;
    if authority.as_str() != rest || authority.host().is_empty() {
        return None;
    }
    if authority.port().is_some() && authority.port_u16().is_none() {
        return None;
    }
    HeaderValue::from_str(origin).ok()
}

/// One connection as `serve` serves it: HTTP/1, with upgrades.
type Connection = hyper::server::conn::http1::UpgradeableConnection<
    TokioIo<tokio::net::TcpStream>,
    TowerToHyperService<axum::Router>,
>;

/// Serves one connection until it ends, or, once `stop_watch` says the
/// server is stopping, until its request in flight is answered; holding
/// `stop_watch` until then is what `serve` waits for (spec 6.4).
async fn connect(connection: Connection, mut stop_watch: tokio::sync::watch::Receiver<bool>) {
    let mut connection = pin!(connection);
    // The watch's guard is dropped at once, so the task stays `Send`.
    let stopped = async {
        let _ = stop_watch.wait_for(|stop| *stop).await.map(|_| ());
    };
    let ended = match select(connection.as_mut(), pin!(stopped)).await {
        Either::Left((ended, _)) => ended,
        Either::Right((_, mut rest)) => {
            rest.as_mut().graceful_shutdown();
            rest.await
        }
    };
    if let Err(err) = ended {
        varyk_std::tracing::debug!("a connection ended: {err}");
    }
}

/// Whether a failure to accept is one connection's, which the client
/// ended before it was accepted, not the listener's.
fn one_connection(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionRefused
    )
}

/// Waits for ctrl-c or, on Unix, SIGTERM; a signal that cannot be
/// listened for is logged and waited for no further.
async fn stop_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            varyk_std::tracing::error!("ctrl-c cannot be listened for: {err}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(err) => {
                varyk_std::tracing::error!("SIGTERM cannot be listened for: {err}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    select(pin!(ctrl_c), pin!(terminate)).await;
}

/// The in-flight limit's refusal: a 503, past the `after` hooks.
async fn busy(_: tower::BoxError) -> axum::response::Response {
    error_body(503, "server busy").into_sent("", "")
}

/// A panic outside a handler, in a hook: the fixed 500, the panic logged
/// (spec 2.1, 6.1). A handler's panic is the route wrapper's.
fn panicked(payload: Box<dyn std::any::Any + Send + 'static>) -> axum::response::Response {
    let message = panic_message(payload.as_ref());
    varyk_std::tracing::error!("a hook panicked: {message:?}");
    internal_http()
}

/// The request log (spec 3.1): one line per request at info level after
/// its response, with the method, the path as received without its
/// query, the status, and the time.
async fn request_log(req: axum::extract::Request, next: Next) -> axum::response::Response {
    let method = req.method().clone();
    let path = crate::message::logged_path(req.uri().path());
    let started = std::time::Instant::now();
    let response = next.run(req).await;
    varyk_std::tracing::info!(
        "{method} {path} {} {}ms",
        response.status().as_u16(),
        started.elapsed().as_millis()
    );
    response
}

/// `pattern` with its `{name}` segments written `{p0}`, `{p1}`, ..., and
/// the names in order; the reason when it is not a route path.
fn positional(pattern: &str) -> Result<(String, Vec<String>), String> {
    let invalid = || format!("the route path {pattern:?} is not valid");
    if pattern == "/" {
        return Ok(("/".to_string(), Vec::new()));
    }
    let rest = pattern.strip_prefix('/').ok_or_else(invalid)?;
    let mut path = String::new();
    let mut names: Vec<String> = Vec::new();
    for segment in rest.split('/') {
        path.push('/');
        if let Some(name) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            let identifier = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !identifier || names.iter().any(|known| known == name) {
                return Err(invalid());
            }
            path.push_str(&format!("{{p{}}}", names.len()));
            names.push(name.to_string());
        } else if !segment.is_empty()
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b))
        {
            path.push_str(segment);
        } else {
            return Err(invalid());
        }
    }
    Ok((path, names))
}

/// Whether a `before` hook for `prefix` (every route for none) covers
/// the route `pattern`, on whole segments.
fn covers(prefix: Option<&str>, pattern: &str) -> bool {
    match prefix {
        None | Some("/") => true,
        Some(prefix) => {
            pattern == prefix
                || pattern
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
        }
    }
}

/// One route's wrapper around its adapter (spec 6.3).
struct Wrapper {
    pattern: Arc<str>,
    /// The route's own parameter names, by position.
    names: Vec<String>,
    /// The `before` hooks that cover the route, in registration order.
    befores: Vec<BeforeRun>,
    afters: Arc<Vec<AfterRun>>,
    state: State,
    run: RouteRun,
    body_limit: u64,
    timeout: Duration,
}

impl Wrapper {
    async fn handle(self: Arc<Self>, req: axum::extract::Request) -> axum::response::Response {
        let deadline = Instant::now().checked_add(self.timeout);
        let (mut parts, body) = req.into_parts();
        let method = parts.method.as_str().to_string();
        let path = parts.uri.path().to_string();
        let (request, response) = match self.params(&mut parts).await {
            Err(response) => {
                let routed = Routed::new(Arc::clone(&self.state), Vec::new());
                (Request::received(&parts, Bytes::new(), routed), response)
            }
            Ok(params) if multipart(&parts) => {
                let routed = Routed::new(Arc::clone(&self.state), params);
                if declared_over(&parts, self.body_limit) {
                    (Request::received(&parts, Bytes::new(), routed), too_large())
                } else {
                    // Kept as a stream for `bind::<Multipart>` (spec 6.2).
                    routed.keep_upload(body, self.body_limit);
                    let request = Request::received(&parts, Bytes::new(), Arc::clone(&routed));
                    let response = self
                        .answer(&request, &routed, deadline, &method, &path)
                        .await;
                    // Over the limit, whatever the handler gave is the
                    // 413, with nothing logged: the fault is the client's.
                    if routed.over_limit() {
                        (request, too_large())
                    } else {
                        (request, response)
                    }
                }
            }
            Ok(params) => {
                let routed = Routed::new(Arc::clone(&self.state), params);
                // A WebSocket upgrade is kept for `bind::<WebSocket>`; it is
                // a `GET` with no body, so reading the body costs nothing
                // (spec 6.2).
                if let Ok(socket) = WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
                    routed.keep_upgrade(socket, self.body_limit);
                }
                match within(deadline, read_body(&parts, body, self.body_limit)).await {
                    None => (Request::received(&parts, Bytes::new(), routed), timed_out()),
                    Some(Err(response)) => {
                        (Request::received(&parts, Bytes::new(), routed), response)
                    }
                    Some(Ok(bytes)) => {
                        let request = Request::received(&parts, bytes, Arc::clone(&routed));
                        let response = self
                            .answer(&request, &routed, deadline, &method, &path)
                            .await;
                        (request, response)
                    }
                }
            }
        };
        let mut response = response;
        for after in self.afters.iter() {
            response = after(request.clone(), response).await;
        }
        let mut sent = response.into_sent(&method, &path);
        sent.extensions_mut()
            .insert(RoutePattern(Arc::clone(&self.pattern)));
        sent
    }

    /// The path parameters by the route's own names; a 400 when one is
    /// not UTF-8 text once percent-decoded.
    async fn params(&self, parts: &mut Parts) -> Result<Vec<(String, String)>, Response> {
        let raw = RawPathParams::from_request_parts(parts, &())
            .await
            .map_err(|_| error_body(400, "a path parameter is not valid UTF-8 text"))?;
        Ok(raw
            .iter()
            .filter_map(|(key, value)| {
                let index = key.strip_prefix('p')?.parse::<usize>().ok()?;
                Some((self.names.get(index)?.clone(), value.to_string()))
            })
            .collect())
    }

    /// Runs the `before` hooks, stopping at the first refusal, then the
    /// adapter as a task, until the deadline.
    async fn answer(
        &self,
        request: &Request,
        routed: &Routed,
        deadline: Option<Instant>,
        method: &str,
        path: &str,
    ) -> Response {
        let befores = async {
            for before in &self.befores {
                if let Some(response) = before(request.clone()).await {
                    return Some(response);
                }
            }
            None
        };
        match within(deadline, befores).await {
            None => return timed_out(),
            Some(Some(response)) => return response,
            Some(None) => {}
        }
        // A response sent through the channel always wins over the
        // adapter's; the task is aborted when the client goes away or the
        // deadline passes before any response.
        let (sender, mut early) = oneshot::channel();
        routed.arm(sender);
        let mut task = tokio::spawn((self.run)(request.clone()));
        let mut guard = AbortOnDrop(Some(task.abort_handle()));
        match within(deadline, select(&mut early, &mut task)).await {
            None => match early.try_recv() {
                Ok(response) => {
                    guard.disarm();
                    watch(task, method, path, routed.client_gone());
                    response
                }
                Err(_) => timed_out(),
            },
            Some(Either::Left((Ok(response), _))) => {
                guard.disarm();
                watch(task, method, path, routed.client_gone());
                response
            }
            Some(Either::Left((Err(_), _))) => match within(deadline, &mut task).await {
                None => timed_out(),
                Some(joined) => finished(joined),
            },
            Some(Either::Right((joined, _))) => match early.try_recv() {
                Ok(response) => {
                    ignored(joined, method, path, &routed.client_gone());
                    response
                }
                Err(_) => finished(joined),
            },
        }
    }
}

/// The 404 and 405 fallbacks: the `after` hooks run on their response,
/// and no `before` hook runs (spec 2.2).
struct Fallback {
    state: State,
    afters: Arc<Vec<AfterRun>>,
}

impl Fallback {
    async fn answer(
        self: Arc<Self>,
        req: axum::extract::Request,
        response: Response,
    ) -> axum::response::Response {
        let (parts, _) = req.into_parts();
        let routed = Routed::new(Arc::clone(&self.state), Vec::new());
        let request = Request::received(&parts, Bytes::new(), routed);
        let mut response = response;
        for after in self.afters.iter() {
            response = after(request.clone(), response).await;
        }
        response.into_sent(parts.method.as_str(), parts.uri.path())
    }
}

/// `future`'s output, or `None` when `deadline` passes first.
async fn within<F: Future>(deadline: Option<Instant>, future: F) -> Option<F::Output> {
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, future).await.ok(),
        None => Some(future.await),
    }
}

/// The body read whole, up to `limit` bytes: a 413 for a declared length
/// over it, and for a body that crosses it while it is read.
async fn read_body(parts: &Parts, body: Body, limit: u64) -> Result<Bytes, Response> {
    if declared_over(parts, limit) {
        return Err(too_large());
    }
    let mut stream = body.into_data_stream();
    let mut read: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| error_body(400, "the body cannot be read"))?;
        if (read.len() as u64).saturating_add(chunk.len() as u64) > limit {
            return Err(too_large());
        }
        read.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(read))
}

/// Whether the request declares a body longer than `limit` bytes.
fn declared_over(parts: &Parts, limit: u64) -> bool {
    parts
        .headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|declared| declared > limit)
}

/// Whether the request's body is `multipart/form-data`.
fn multipart(parts: &Parts) -> bool {
    parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(crate::message::is_multipart)
}

fn too_large() -> Response {
    error_body(413, "body too large")
}

fn timed_out() -> Response {
    error_body(503, "request timed out")
}

/// The adapter task's outcome: its response, or for a panic the fixed
/// 500 with the panic's message to be logged.
fn finished(joined: Result<Response, JoinError>) -> Response {
    match joined {
        Ok(response) => response,
        Err(err) => internal_because(failure(err)),
    }
}

/// After an early response, the adapter's own response is ignored: an
/// error's message, whatever its status, and a panic are only logged,
/// with the request's `method` and `path`, never sent (spec 2.5): an
/// error at debug level when it is the one a live connection gave
/// because its client has gone (recorded in `gone`), which is no fault of
/// the service, and every other error at error level, as a panic always
/// is (spec 3.1).
fn ignored(
    joined: Result<Response, JoinError>,
    method: &str,
    path: &str,
    gone: &crate::message::ClientGone,
) {
    let path = crate::message::logged_path(path);
    // A panic is the service's fault whether or not the client has gone.
    let response = match joined {
        Ok(response) => response,
        Err(err) => {
            let message = failure(err);
            varyk_std::tracing::error!("{method} {path}: {message:?}");
            return;
        }
    };
    let message = match response.hidden_message() {
        Some(message) => message.to_string(),
        None if response.status() >= 400 => {
            // An `Err` with a status: its message is the body's `error`.
            let body = response.body();
            let parsed: Result<std::collections::HashMap<String, String>, varyk_std::Error> =
                varyk_std::json::parse(&body);
            match parsed.ok().and_then(|mut fields| fields.remove("error")) {
                Some(message) => message,
                None => body,
            }
        }
        None => return,
    };
    let recorded = match gone.lock() {
        Ok(slot) => slot.clone(),
        Err(_) => None,
    };
    if recorded.as_deref() == Some(message.as_str()) {
        varyk_std::tracing::debug!("{method} {path}: the client has gone: {message:?}");
    } else {
        varyk_std::tracing::error!("{method} {path}: {message:?}");
    }
}

/// Lets a live handler run on after its early response, logging how it
/// ends.
fn watch(
    task: tokio::task::JoinHandle<Response>,
    method: &str,
    path: &str,
    gone: crate::message::ClientGone,
) {
    let method = method.to_string();
    let path = path.to_string();
    tokio::spawn(async move { ignored(task.await, &method, &path, &gone) });
}

/// What a join error says: the panic's message, or the cancellation.
fn failure(err: JoinError) -> String {
    match err.try_into_panic() {
        Ok(payload) => format!("the handler panicked: {}", panic_message(payload.as_ref())),
        Err(_) => "the handler was cancelled".to_string(),
    }
}

/// What a panic said, when it said it in text.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    match payload.downcast_ref::<&str>() {
        Some(text) => (*text).to_string(),
        None => match payload.downcast_ref::<String>() {
            Some(text) => text.clone(),
            None => "a value that is not text".to_string(),
        },
    }
}

/// Aborts the adapter task when dropped, unless disarmed: a request whose
/// client goes away, or whose deadline passes, before any response
/// cancels its handler (spec 6.3).
struct AbortOnDrop(Option<AbortHandle>);

impl AbortOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// Wraps a route's adapter for `App::get` and the others.
pub fn route<F, Fut>(adapter: F) -> Route
where
    F: Fn(crate::message::Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = crate::message::Response> + Send + 'static,
{
    Route {
        run: Arc::new(move |req| Box::pin(adapter(req))),
    }
}

/// Wraps a `before` adapter for `App::before` and `App::before_on`.
pub fn before_hook<F, Fut>(adapter: F) -> BeforeHook
where
    F: Fn(crate::message::Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Option<crate::message::Response>> + Send + 'static,
{
    BeforeHook {
        run: Arc::new(move |req| Box::pin(adapter(req))),
    }
}

/// Wraps an `after` adapter for `App::after`.
pub fn after_hook<F, Fut>(adapter: F) -> AfterHook
where
    F: Fn(crate::message::Request, crate::message::Response) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = crate::message::Response> + Send + 'static,
{
    AfterHook {
        run: Arc::new(move |req, res| Box::pin(adapter(req, res))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::App;

    /// Serves an app with no routes on `port`, with an idle timeout of
    /// one second.
    async fn serve(port: u16) -> TcpStream {
        let mut app = App::new(Arc::new(()));
        app.set_idle_timeout(1000);
        tokio::spawn(async move {
            if let Err(err) = app.serve(port).await {
                panic!("the app cannot serve: {}", err.message());
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(stream) => stream,
            Err(err) => panic!("cannot connect: {err}"),
        }
    }

    async fn write(stream: &mut TcpStream, text: &str) {
        if let Err(err) = stream.write_all(text.as_bytes()).await {
            panic!("cannot write: {err}");
        }
    }

    /// How long the server takes to close `stream`, reading past any
    /// response it sends first; `None` when it is still open after three
    /// seconds.
    async fn closed_after(stream: &mut TcpStream) -> Option<Duration> {
        let mut buffer = [0u8; 1024];
        let started = tokio::time::Instant::now();
        let deadline = started + Duration::from_secs(3);
        loop {
            match tokio::time::timeout_at(deadline, stream.read(&mut buffer)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return Some(started.elapsed()),
                Ok(Ok(_)) => {}
                Err(_) => return None,
            }
        }
    }

    /// Closed by the idle timeout of one second, and not before it.
    fn by_the_timeout(after: Option<Duration>) -> bool {
        matches!(after, Some(after) if after >= Duration::from_millis(900))
    }

    #[test]
    fn a_connection_that_sends_part_of_its_headers_is_closed() {
        varyk_std::run(async {
            let mut stream = serve(41818).await;
            write(&mut stream, "GET /hello HTTP/1.1\r\nHost: a\r\n").await;
            assert!(by_the_timeout(closed_after(&mut stream).await));
        });
    }

    #[test]
    fn an_idle_kept_alive_connection_is_closed() {
        varyk_std::run(async {
            let mut stream = serve(41819).await;
            write(&mut stream, "GET /hello HTTP/1.1\r\nHost: a\r\n\r\n").await;
            assert!(by_the_timeout(closed_after(&mut stream).await));
        });
    }
}
