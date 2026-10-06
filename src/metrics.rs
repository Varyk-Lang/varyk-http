// The metrics layer and its Prometheus exporter (varyk-http spec 2.7).
// The crate is written `::metrics::`, since this module has its name.

use std::sync::Arc;
use std::time::Instant;

use ::metrics::{Key, Label, Level, Metadata, Recorder};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle, PrometheusRecorder};

use crate::server::RoutePattern;

/// The buckets of the Prometheus clients' defaults, in seconds.
const BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

static METADATA: Metadata<'static> =
    Metadata::new(module_path!(), Level::INFO, Some(module_path!()));

/// One app's recorder, written to directly: the `metrics` crate's global
/// recorder is never installed, so two apps keep their counts apart.
pub(crate) struct Metrics {
    recorder: PrometheusRecorder,
    handle: PrometheusHandle,
}

/// On a response that is not counted: the metrics path's own.
#[derive(Clone)]
pub(crate) struct Uncounted;

impl Metrics {
    /// A recorder with the default buckets; the reason when it cannot be
    /// built.
    pub(crate) fn new() -> Result<Metrics, String> {
        let builder = PrometheusBuilder::new()
            .set_buckets(&BUCKETS)
            .map_err(|err| format!("the metrics recorder cannot be built: {err}"))?;
        let recorder = builder.build_recorder();
        let handle = recorder.handle();
        Ok(Metrics { recorder, handle })
    }

    fn record(&self, method: &'static str, route: String, status: u16, seconds: f64) {
        let counted = Key::from_parts(
            "http_requests_total",
            vec![
                Label::new("method", method),
                Label::new("route", route.clone()),
                Label::new("status", status.to_string()),
            ],
        );
        self.recorder
            .register_counter(&counted, &METADATA)
            .increment(1);
        let timed = Key::from_parts(
            "http_request_duration_seconds",
            vec![Label::new("method", method), Label::new("route", route)],
        );
        self.recorder
            .register_histogram(&timed, &METADATA)
            .record(seconds);
    }

    /// Drains histogram data, so memory does not grow when nothing
    /// scrapes; `serve` runs it every 5 seconds (spec 2.7).
    pub(crate) fn upkeep(&self) {
        self.handle.run_upkeep();
    }

    /// The Prometheus text, as the metrics path answers it; not counted.
    pub(crate) fn response(&self) -> axum::response::Response {
        let mut response = axum::response::Response::new(Body::from(self.handle.render()));
        *response.status_mut() = StatusCode::OK;
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        );
        response.extensions_mut().insert(Uncounted);
        response
    }
}

/// The metrics layer: counts and times every response but the metrics
/// path's, labelled by the method, the route's pattern (`unmatched` for a
/// response no route wrapper made), and the status.
pub(crate) async fn count(
    State(metrics): State<Arc<Metrics>>,
    req: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    let method = method_label(req.method().as_str());
    let started = Instant::now();
    let response = next.run(req).await;
    if response.extensions().get::<Uncounted>().is_none() {
        let route = match response.extensions().get::<RoutePattern>() {
            Some(pattern) => pattern.0.to_string(),
            None => "unmatched".to_string(),
        };
        let seconds = started.elapsed().as_secs_f64();
        metrics.record(method, route, response.status().as_u16(), seconds);
    }
    response
}

/// A method as a label: one of the common ones, or `other`, so a client
/// cannot add labels.
fn method_label(method: &str) -> &'static str {
    match method {
        "GET" => "GET",
        "HEAD" => "HEAD",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "OPTIONS" => "OPTIONS",
        _ => "other",
    }
}
