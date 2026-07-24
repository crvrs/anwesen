//! Request-level telemetry for the HTTP surface, per
//! [ANW-37](https://crvrs.youtrack.cloud/issue/ANW-37).
//!
//! Two halves:
//!
//! - **Metrics** (every request): a request counter and a response-bytes
//!   counter keyed by route, status, and conditional-GET outcome, plus a
//!   duration histogram keyed by route and status. Exported over OTLP.
//! - **Traces** (slow or failing requests only): the incoming W3C
//!   `traceparent` is extracted, and a request at or over the configured
//!   threshold, or answering a 5xx, is recorded as a server span nested in
//!   the propagated context. Every other request stays metrics-only, so the
//!   ~3.6k requests/min steady state does not drown the trace backend.
//!
//! When no OTLP endpoint (or uptrace DSN) is configured, [`init`] returns
//! `None`, the request middleware is not installed, and the server behaves
//! exactly as it did before this module existed. External installs run
//! unchanged.
//!
//! Config mirrors the gestell uptrace surface (see the gestell PDR-GES-136
//! `[otel]` section): a `uptrace_dsn` shorthand, or a generic endpoint plus
//! headers.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, anyhow, bail};
use axum::http::HeaderMap;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, MeterProvider as _};
use opentelemetry::propagation::{Extractor, TextMapPropagator};
use opentelemetry::trace::{Span, SpanKind, Tracer, TracerProvider as _};
use opentelemetry_otlp::{
    MetricExporter, Protocol, SpanExporter, WithExportConfig, WithHttpConfig,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::Sampler;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use opentelemetry_semantic_conventions::resource::SERVICE_VERSION;

/// Raw telemetry options as parsed by clap on the `serve` command. Resolved
/// into an [`Option<TelemetryConfig>`] by [`TelemetryConfig::resolve`].
#[derive(Debug, Default)]
pub struct RawTelemetryArgs {
    /// `--uptrace-dsn` / `ANWESEN_UPTRACE_DSN`.
    pub uptrace_dsn: Option<String>,
    /// `--otlp-endpoint` / `ANWESEN_OTLP_ENDPOINT`.
    pub otlp_endpoint: Option<String>,
    /// `--otlp-header` / `ANWESEN_OTLP_HEADERS`, each `key=value`.
    pub otlp_headers: Vec<String>,
    /// `--otlp-slow-request-ms` / `ANWESEN_OTLP_SLOW_REQUEST_MS`.
    pub slow_request_ms: u64,
}

/// A resolved, telemetry-on configuration. Built only when an endpoint or a
/// DSN is present; absence is represented by `Ok(None)` from [`resolve`].
///
/// [`resolve`]: TelemetryConfig::resolve
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryConfig {
    /// OTLP/HTTP base URL. The exporter appends the per-signal path
    /// (`/v1/metrics`, `/v1/traces`).
    pub endpoint: String,
    /// Export headers (for uptrace, the `uptrace-dsn` entry) as ordered
    /// `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// A request at or over this duration, or answering a 5xx, is recorded
    /// as a server span.
    pub slow_request: Duration,
}

impl TelemetryConfig {
    /// Resolve raw clap options into an optional config.
    ///
    /// - Both `uptrace_dsn` and `otlp_endpoint` set is an error (they are
    ///   two ways to name the same endpoint).
    /// - Neither set means telemetry is off: `Ok(None)`.
    /// - A malformed `key=value` header or an unparseable DSN is an error,
    ///   surfaced at startup rather than silently dropping export.
    ///
    /// # Errors
    /// Returns an error when the two endpoint sources conflict, a header is
    /// not `key=value`, or the uptrace DSN cannot be parsed.
    pub fn resolve(raw: RawTelemetryArgs) -> anyhow::Result<Option<Self>> {
        let slow_request = Duration::from_millis(raw.slow_request_ms);
        let mut headers = parse_headers(&raw.otlp_headers)?;

        match (raw.uptrace_dsn, raw.otlp_endpoint) {
            (Some(_), Some(_)) => {
                bail!("--uptrace-dsn and --otlp-endpoint are mutually exclusive");
            }
            (Some(dsn), None) => {
                let (endpoint, dsn_header) = parse_uptrace_dsn(&dsn)?;
                // The DSN header leads; any explicit --otlp-header follows.
                headers.insert(0, dsn_header);
                Ok(Some(Self {
                    endpoint,
                    headers,
                    slow_request,
                }))
            }
            (None, Some(endpoint)) => Ok(Some(Self {
                endpoint,
                headers,
                slow_request,
            })),
            (None, None) => Ok(None),
        }
    }
}

/// Parse `key=value` header specs. Whitespace around key and value is
/// trimmed; an empty key or a spec with no `=` is an error.
fn parse_headers(specs: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        let (k, v) = spec
            .split_once('=')
            .ok_or_else(|| anyhow!("OTLP header {spec:?} is not key=value"))?;
        let k = k.trim();
        if k.is_empty() {
            bail!("OTLP header {spec:?} has an empty key");
        }
        out.push((k.to_string(), v.trim().to_string()));
    }
    Ok(out)
}

/// Parse an uptrace DSN (`https://<token>@host[:port]`) into the OTLP
/// endpoint base URL and the `uptrace-dsn` header uptrace expects. The full
/// DSN is echoed as the header value per uptrace's ingest contract.
fn parse_uptrace_dsn(dsn: &str) -> anyhow::Result<(String, (String, String))> {
    let dsn = dsn.trim();
    let (scheme, rest) = dsn
        .split_once("://")
        .context("uptrace DSN has no scheme (expected https://<token>@host)")?;
    // Host is whatever follows the credentials `@`; a DSN with no `@` is
    // treated as endpoint-only (lenient, though real uptrace DSNs carry a
    // token).
    let host = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    let host = host.trim_end_matches('/');
    if host.is_empty() {
        bail!("uptrace DSN has no host");
    }
    let endpoint = format!("{scheme}://{host}");
    Ok((endpoint, ("uptrace-dsn".to_string(), dsn.to_string())))
}

/// Semantic route bucket for the `http.route` label. Coarser than the axum
/// template on purpose: `/notes/{*path}` serves both a note fetch and a
/// folder listing, and "304 share of note fetches" needs the two apart. The
/// bucket is a function of the request path prefix and its trailing slash.
#[must_use]
pub fn classify_route(path: &str) -> &'static str {
    if path == "/health" {
        "health"
    } else if path == "/query" {
        "query"
    } else if path == "/notes/" {
        "folder"
    } else if path.starts_with("/notes/") {
        if path.ends_with('/') {
            "folder"
        } else {
            "note"
        }
    } else {
        "other"
    }
}

/// Whether a completed request additionally warrants a server span: it took
/// at least the slow threshold, or answered a server error. Everything else
/// stays metrics-only so the steady-state request rate does not flood the
/// trace backend.
#[must_use]
pub fn should_span(duration: Duration, slow_request: Duration, status: u16) -> bool {
    duration >= slow_request || status >= 500
}

/// Conditional-GET outcome for the `conditional_get` label, derived from the
/// response status and whether the request carried `If-None-Match`:
///
/// - `not_modified`: a 304 (the client's etag matched);
/// - `revalidated`: `If-None-Match` was present but the body was still sent
///   (etag mismatch);
/// - `unconditional`: no `If-None-Match` header.
///
/// Together these answer both the 304 share of note fetches and the
/// If-None-Match presence share.
#[must_use]
pub fn conditional_get(status: u16, if_none_match_present: bool) -> &'static str {
    if status == 304 {
        "not_modified"
    } else if if_none_match_present {
        "revalidated"
    } else {
        "unconditional"
    }
}

/// Histogram bucket boundaries for `http.server.request.duration`, in
/// seconds. Extended out to 600 s because the fleet p50 is ~305 s (ANW-38);
/// the default `OTel` buckets top out near 10 s, so every slow request would
/// land in one overflow bucket and the percentiles the issue needs would be
/// unreadable.
pub const DURATION_BUCKETS_SECONDS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
];

/// The W3C trace-context header values carried by a request, captured before
/// the handler consumes it. Extracting the parent context is deferred to
/// span-emission time, so the hot path (metrics-only requests) pays only two
/// header reads, not a full propagator extraction.
#[derive(Debug, Default, Clone)]
pub struct TraceHeaders {
    pub traceparent: Option<String>,
    pub tracestate: Option<String>,
}

impl TraceHeaders {
    #[must_use]
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let get = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        Self {
            traceparent: get("traceparent"),
            tracestate: get("tracestate"),
        }
    }
}

/// Lets the W3C propagator read the captured header values directly, without
/// rebuilding a `HeaderMap`. Only `traceparent` and `tracestate` matter for
/// trace-context extraction.
impl Extractor for TraceHeaders {
    fn get(&self, key: &str) -> Option<&str> {
        match key {
            "traceparent" => self.traceparent.as_deref(),
            "tracestate" => self.tracestate.as_deref(),
            _ => None,
        }
    }

    fn keys(&self) -> Vec<&str> {
        let mut keys = Vec::with_capacity(2);
        if self.traceparent.is_some() {
            keys.push("traceparent");
        }
        if self.tracestate.is_some() {
            keys.push("tracestate");
        }
        keys
    }
}

/// Live telemetry handle: owns the OTLP meter and tracer providers and the
/// instruments, and records one observation per request. Created by [`init`]
/// only when telemetry is configured; when it is not, the request middleware
/// is never installed and this type is never constructed.
pub struct Telemetry {
    inner: TelemetryInner,
}

impl Telemetry {
    /// Record one completed request: bump the metric instruments, and -- when
    /// the request was at or over the slow threshold or answered a 5xx --
    /// emit a server span nested under the propagated trace context.
    ///
    /// `start` is the wall-clock instant the request arrived, used as the
    /// span start time so the span's own duration matches `duration`.
    #[allow(clippy::too_many_arguments)]
    pub fn finish(
        &self,
        route: &'static str,
        method: &str,
        status: u16,
        if_none_match_present: bool,
        body_bytes: u64,
        duration: Duration,
        start: SystemTime,
        trace: &TraceHeaders,
    ) {
        self.inner
            .record_metrics(route, status, if_none_match_present, body_bytes, duration);
        if should_span(duration, self.inner.slow_request, status) {
            self.inner
                .record_span(route, method, status, duration, start, trace);
        }
    }

    /// Flush and shut down the providers. Called once at server exit.
    pub fn shutdown(&self) {
        self.inner.shutdown();
    }
}

/// Build the telemetry handle from a resolved config, standing up the OTLP
/// meter and tracer providers.
///
/// # Errors
/// Returns an error when an OTLP exporter cannot be constructed.
pub fn init(config: TelemetryConfig) -> anyhow::Result<Telemetry> {
    Ok(Telemetry {
        inner: TelemetryInner::new(config)?,
    })
}

// -- OTLP wiring seam -------------------------------------------------------
//
// `TelemetryInner` owns the OTel providers and instruments. The public
// surface above (`finish`, `shutdown`, `TraceHeaders`, `classify_route`,
// `conditional_get`, the bucket boundaries) is stable; only the bodies below
// change as the exporters are wired.

struct TelemetryInner {
    slow_request: Duration,
    meter_provider: SdkMeterProvider,
    tracer_provider: SdkTracerProvider,
    tracer: SdkTracer,
    requests: Counter<u64>,
    response_bytes: Counter<u64>,
    duration: Histogram<f64>,
    propagator: TraceContextPropagator,
}

impl TelemetryInner {
    fn new(config: TelemetryConfig) -> anyhow::Result<Self> {
        let TelemetryConfig {
            endpoint,
            headers,
            slow_request,
        } = config;
        // `.with_endpoint()` is used verbatim by the exporter (the `/v1/*`
        // suffix is only auto-appended for the generic OTEL env var), so we
        // append the per-signal path ourselves.
        let endpoint = endpoint.trim_end_matches('/').to_string();
        let header_map: HashMap<String, String> = headers.into_iter().collect();

        let resource = Resource::builder()
            .with_service_name("anwesen")
            .with_attribute(KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION")))
            .build();

        // -- metrics: thread-based periodic reader over an OTLP/HTTP exporter.
        let metric_exporter = MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(format!("{endpoint}/v1/metrics"))
            .with_headers(header_map.clone())
            .build()
            .context("build OTLP metric exporter")?;
        let reader = PeriodicReader::builder(metric_exporter)
            .with_interval(Duration::from_secs(15))
            .build();
        let meter_provider = SdkMeterProvider::builder()
            .with_resource(resource.clone())
            .with_reader(reader)
            .build();
        let meter = meter_provider.meter("anwesen");
        let requests = meter
            .u64_counter("http.server.requests")
            .with_unit("{request}")
            .with_description("HTTP requests by route, status, and conditional-GET outcome")
            .build();
        let response_bytes = meter
            .u64_counter("http.server.response.body.size")
            .with_unit("By")
            .with_description(
                "HTTP response body bytes by route, status, and conditional-GET outcome",
            )
            .build();
        let duration = meter
            .f64_histogram("http.server.request.duration")
            .with_unit("s")
            .with_description("HTTP request duration by route and status")
            .with_boundaries(DURATION_BUCKETS_SECONDS.to_vec())
            .build();

        // -- traces: thread-based batch processor over an OTLP/HTTP exporter.
        let span_exporter = SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(format!("{endpoint}/v1/traces"))
            .with_headers(header_map)
            .build()
            .context("build OTLP span exporter")?;
        let tracer_provider = SdkTracerProvider::builder()
            .with_resource(resource)
            // Span creation is already gated to slow/5xx requests in
            // `record_span`, so sample everything we choose to build.
            .with_sampler(Sampler::AlwaysOn)
            .with_batch_exporter(span_exporter)
            .build();
        let tracer = tracer_provider.tracer("anwesen");

        tracing::info!(
            endpoint = %endpoint,
            slow_request_ms = slow_request.as_millis(),
            "telemetry: OTLP export enabled"
        );

        Ok(Self {
            slow_request,
            meter_provider,
            tracer_provider,
            tracer,
            requests,
            response_bytes,
            duration,
            propagator: TraceContextPropagator::new(),
        })
    }

    fn record_metrics(
        &self,
        route: &'static str,
        status: u16,
        if_none_match_present: bool,
        body_bytes: u64,
        duration: Duration,
    ) {
        let attrs = [
            KeyValue::new("http.route", route),
            KeyValue::new("http.response.status_code", i64::from(status)),
            KeyValue::new(
                "conditional_get",
                conditional_get(status, if_none_match_present),
            ),
        ];
        self.requests.add(1, &attrs);
        self.response_bytes.add(body_bytes, &attrs);
        // The histogram stays at route x status per the contract; the
        // conditional-GET dimension lives on the counters only.
        self.duration.record(
            duration.as_secs_f64(),
            &[
                KeyValue::new("http.route", route),
                KeyValue::new("http.response.status_code", i64::from(status)),
            ],
        );
    }

    fn record_span(
        &self,
        route: &'static str,
        method: &str,
        status: u16,
        duration: Duration,
        start: SystemTime,
        trace: &TraceHeaders,
    ) {
        let parent_cx = self.propagator.extract(trace);
        let mut span = self
            .tracer
            .span_builder(format!("{method} {route}"))
            .with_kind(SpanKind::Server)
            .with_start_time(start)
            .with_attributes(vec![
                KeyValue::new("http.request.method", method.to_string()),
                KeyValue::new("http.route", route),
                KeyValue::new("http.response.status_code", i64::from(status)),
            ])
            .start_with_context(&self.tracer, &parent_cx);
        span.end_with_timestamp(start + duration);
    }

    fn shutdown(&self) {
        if let Err(e) = self.meter_provider.shutdown() {
            tracing::warn!(error = %e, "telemetry: meter provider shutdown");
        }
        if let Err(e) = self.tracer_provider.shutdown() {
            tracing::warn!(error = %e, "telemetry: tracer provider shutdown");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw() -> RawTelemetryArgs {
        RawTelemetryArgs::default()
    }

    #[test]
    fn resolve_off_when_nothing_set() {
        let cfg = TelemetryConfig::resolve(raw()).unwrap();
        assert!(cfg.is_none(), "no endpoint/DSN means telemetry off");
    }

    #[test]
    fn resolve_generic_endpoint() {
        let cfg = TelemetryConfig::resolve(RawTelemetryArgs {
            otlp_endpoint: Some("https://collector.example.com".into()),
            otlp_headers: vec!["authorization=Bearer xyz".into()],
            slow_request_ms: 500,
            ..raw()
        })
        .unwrap()
        .expect("telemetry on");
        assert_eq!(cfg.endpoint, "https://collector.example.com");
        assert_eq!(
            cfg.headers,
            vec![("authorization".to_string(), "Bearer xyz".to_string())]
        );
        assert_eq!(cfg.slow_request, Duration::from_millis(500));
    }

    #[test]
    fn resolve_uptrace_dsn_splits_endpoint_and_header() {
        let cfg = TelemetryConfig::resolve(RawTelemetryArgs {
            uptrace_dsn: Some("https://SECRET_TOKEN@api.uptrace.dev".into()),
            slow_request_ms: 250,
            ..raw()
        })
        .unwrap()
        .expect("telemetry on");
        assert_eq!(cfg.endpoint, "https://api.uptrace.dev");
        assert_eq!(
            cfg.headers,
            vec![(
                "uptrace-dsn".to_string(),
                "https://SECRET_TOKEN@api.uptrace.dev".to_string()
            )]
        );
        assert_eq!(cfg.slow_request, Duration::from_millis(250));
    }

    #[test]
    fn resolve_dsn_header_leads_explicit_headers() {
        let cfg = TelemetryConfig::resolve(RawTelemetryArgs {
            uptrace_dsn: Some("https://tok@api.uptrace.dev".into()),
            otlp_headers: vec!["x-extra=1".into()],
            ..raw()
        })
        .unwrap()
        .expect("telemetry on");
        assert_eq!(cfg.headers[0].0, "uptrace-dsn");
        assert_eq!(cfg.headers[1], ("x-extra".to_string(), "1".to_string()));
    }

    #[test]
    fn resolve_rejects_both_endpoint_sources() {
        let err = TelemetryConfig::resolve(RawTelemetryArgs {
            uptrace_dsn: Some("https://tok@api.uptrace.dev".into()),
            otlp_endpoint: Some("https://collector.example.com".into()),
            ..raw()
        })
        .unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn resolve_rejects_malformed_header() {
        let err = TelemetryConfig::resolve(RawTelemetryArgs {
            otlp_endpoint: Some("https://collector.example.com".into()),
            otlp_headers: vec!["no-equals-sign".into()],
            ..raw()
        })
        .unwrap_err();
        assert!(err.to_string().contains("key=value"));
    }

    #[test]
    fn resolve_rejects_dsn_without_scheme() {
        let err = TelemetryConfig::resolve(RawTelemetryArgs {
            uptrace_dsn: Some("tok@api.uptrace.dev".into()),
            ..raw()
        })
        .unwrap_err();
        assert!(err.to_string().contains("scheme"));
    }

    #[test]
    fn classify_route_buckets() {
        assert_eq!(classify_route("/health"), "health");
        assert_eq!(classify_route("/query"), "query");
        assert_eq!(classify_route("/notes/"), "folder");
        assert_eq!(classify_route("/notes/a.md"), "note");
        assert_eq!(classify_route("/notes/Projects/a.md"), "note");
        assert_eq!(classify_route("/notes/Projects/"), "folder");
        assert_eq!(classify_route("/notes/Projects/anwesen/"), "folder");
        assert_eq!(classify_route("/"), "other");
        assert_eq!(classify_route("/favicon.ico"), "other");
    }

    #[test]
    fn should_span_on_slow_or_5xx() {
        let threshold = Duration::from_millis(500);
        // Fast + success: metrics-only.
        assert!(!should_span(Duration::from_millis(10), threshold, 200));
        assert!(!should_span(Duration::from_millis(499), threshold, 304));
        // At or over the threshold: span.
        assert!(should_span(Duration::from_millis(500), threshold, 200));
        assert!(should_span(Duration::from_secs(305), threshold, 200));
        // 5xx spans even when fast.
        assert!(should_span(Duration::from_millis(1), threshold, 500));
        assert!(should_span(Duration::from_millis(1), threshold, 503));
        // 4xx is a client error, not a server span trigger on its own.
        assert!(!should_span(Duration::from_millis(1), threshold, 404));
    }

    #[test]
    fn conditional_get_outcomes() {
        assert_eq!(conditional_get(304, true), "not_modified");
        // A 304 with no header cannot happen in practice, but the status
        // wins: it is still "not modified".
        assert_eq!(conditional_get(304, false), "not_modified");
        assert_eq!(conditional_get(200, true), "revalidated");
        assert_eq!(conditional_get(200, false), "unconditional");
        assert_eq!(conditional_get(404, false), "unconditional");
    }
}
