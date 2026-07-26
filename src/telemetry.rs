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
//! Transport configuration comes from the standard `OTEL_EXPORTER_OTLP_*`
//! environment variables only, read by the `OpenTelemetry` SDK itself
//! ([ANW-42](https://crvrs.youtrack.cloud/issue/ANW-42)). anwesen parses no
//! addresses and appends no per-signal paths. It checks two things at
//! startup, both cases the SDK would otherwise export nowhere in silence:
//!
//! - `OTEL_EXPORTER_OTLP_ENDPOINT` carries no query or fragment, because the
//!   SDK's own concatenation mangles those (measured against a sink: a
//!   `?tail` base sends `POST /?tail/v1/metrics`, a `#frag` base sends
//!   `POST /`).
//! - No protocol variable asks for anything but `http/protobuf`, the only
//!   transport this binary is built with.
//!
//! When no endpoint variable is set, [`TelemetryConfig::resolve`] returns
//! `None`, the request middleware is not installed, and the server behaves
//! exactly as it did before this module existed. External installs run
//! unchanged.

use std::time::{Duration, SystemTime};

use anyhow::{Context as _, bail};
use axum::http::HeaderMap;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, MeterProvider as _};
use opentelemetry::propagation::{Extractor, TextMapPropagator};
use opentelemetry::trace::{Span, SpanKind, Tracer, TracerProvider as _};
use opentelemetry_otlp::{MetricExporter, SpanExporter};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::Sampler;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use opentelemetry_semantic_conventions::resource::{SERVICE_NAME, SERVICE_VERSION};

/// The removed telemetry options, still parsed so their presence is an error
/// rather than a silent config drop on upgrade
/// ([ANW-42](https://crvrs.youtrack.cloud/issue/ANW-42)), plus the one
/// surviving option. Resolved into an [`Option<TelemetryConfig>`] by
/// [`TelemetryConfig::resolve`].
#[derive(Debug, Default)]
pub struct RawTelemetryArgs {
    /// Removed `--uptrace-dsn` / `ANWESEN_UPTRACE_DSN`.
    pub uptrace_dsn: Option<String>,
    /// Removed `--otlp-endpoint` / `ANWESEN_OTLP_ENDPOINT`.
    pub otlp_endpoint: Option<String>,
    /// Removed `--otlp-header` / `ANWESEN_OTLP_HEADERS`.
    pub otlp_headers: Vec<String>,
    /// `--otlp-slow-request-ms` / `ANWESEN_OTLP_SLOW_REQUEST_MS`. Kept: it
    /// decides when anwesen emits a span, not where the export goes, and no
    /// standard variable covers it.
    pub slow_request_ms: u64,
}

/// The OTLP transport variables the SDK reads, captured once at startup so
/// the telemetry-on decision and the startup checks stay pure functions of
/// them. Values are the raw strings; anwesen does not parse them beyond the
/// checks in [`check_generic_endpoint`] and [`check_protocols`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OtelEnv {
    /// `OTEL_EXPORTER_OTLP_ENDPOINT`: base URL, per-signal path appended by
    /// the SDK.
    pub endpoint: Option<String>,
    /// `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`: full URL, used verbatim.
    pub metrics_endpoint: Option<String>,
    /// `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`: full URL, used verbatim.
    pub traces_endpoint: Option<String>,
    /// `OTEL_EXPORTER_OTLP_PROTOCOL`.
    pub protocol: Option<String>,
    /// `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL`.
    pub metrics_protocol: Option<String>,
    /// `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL`.
    pub traces_protocol: Option<String>,
}

impl OtelEnv {
    /// Read the transport variables from the process environment. An empty or
    /// whitespace-only value counts as unset: an empty endpoint would
    /// otherwise turn telemetry on and export to the SDK's `localhost:4318`
    /// default.
    #[must_use]
    pub fn from_env() -> Self {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Self {
            endpoint: var("OTEL_EXPORTER_OTLP_ENDPOINT"),
            metrics_endpoint: var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT"),
            traces_endpoint: var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"),
            protocol: var("OTEL_EXPORTER_OTLP_PROTOCOL"),
            metrics_protocol: var("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL"),
            traces_protocol: var("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL"),
        }
    }

    /// Whether any endpoint variable is set. No endpoint means telemetry off.
    fn any_endpoint(&self) -> bool {
        self.endpoint.is_some() || self.metrics_endpoint.is_some() || self.traces_endpoint.is_some()
    }

    /// The endpoint to name in the startup log: the generic base when set,
    /// otherwise whichever per-signal URL is.
    fn describe(&self) -> &str {
        self.endpoint
            .as_deref()
            .or(self.metrics_endpoint.as_deref())
            .or(self.traces_endpoint.as_deref())
            .unwrap_or("")
    }
}

/// A resolved, telemetry-on configuration. Built only when an endpoint
/// variable is set; absence is represented by `Ok(None)` from [`resolve`].
/// It carries no transport settings: the exporters read those from the
/// environment themselves.
///
/// [`resolve`]: TelemetryConfig::resolve
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryConfig {
    /// A request at or over this duration, or answering a 5xx, is recorded
    /// as a server span.
    pub slow_request: Duration,
    /// The transport variables, kept for the startup log line only.
    env: OtelEnv,
}

impl TelemetryConfig {
    /// Resolve the surviving option plus the OTLP transport variables into an
    /// optional config.
    ///
    /// - A removed flag or `ANWESEN_` variable is an error naming its `OTEL_`
    ///   replacement: a deployment exporting today must fail one restart
    ///   rather than go quiet.
    /// - No endpoint variable set means telemetry is off: `Ok(None)`.
    /// - A query or fragment on `OTEL_EXPORTER_OTLP_ENDPOINT` is an error.
    /// - A protocol this binary cannot speak is an error.
    ///
    /// # Errors
    /// Returns an error when a removed option is present, when the generic
    /// endpoint carries a query or a fragment, or when a protocol variable
    /// asks for anything but `http/protobuf`.
    pub fn resolve(raw: &RawTelemetryArgs, env: OtelEnv) -> anyhow::Result<Option<Self>> {
        check_removed(raw)?;
        if !env.any_endpoint() {
            return Ok(None);
        }
        if let Some(endpoint) = &env.endpoint {
            check_generic_endpoint(endpoint)?;
        }
        check_protocols(&env)?;
        Ok(Some(Self {
            slow_request: Duration::from_millis(raw.slow_request_ms),
            env,
        }))
    }
}

/// Fail on any removed telemetry option, naming the `OTEL_` variable that
/// replaces it. Silence is the expensive failure here: an upgrade that drops
/// the export config would stop telemetry with nothing in the log.
fn check_removed(raw: &RawTelemetryArgs) -> anyhow::Result<()> {
    let removed: [(&str, &str, bool); 3] = [
        (
            "--uptrace-dsn / ANWESEN_UPTRACE_DSN",
            "OTEL_EXPORTER_OTLP_ENDPOINT=https://api.uptrace.dev plus \
             OTEL_EXPORTER_OTLP_HEADERS=uptrace-dsn=<the DSN, verbatim>",
            raw.uptrace_dsn.is_some(),
        ),
        (
            "--otlp-endpoint / ANWESEN_OTLP_ENDPOINT",
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            raw.otlp_endpoint.is_some(),
        ),
        (
            "--otlp-header / ANWESEN_OTLP_HEADERS",
            "OTEL_EXPORTER_OTLP_HEADERS",
            !raw.otlp_headers.is_empty(),
        ),
    ];
    for (option, replacement, present) in removed {
        if present {
            bail!("{option} was removed in anwesen 0.3.0; use {replacement} instead");
        }
    }
    Ok(())
}

/// Reject a query or fragment on `OTEL_EXPORTER_OTLP_ENDPOINT`. The SDK
/// appends the per-signal path to this value textually, so a `?grpc=4317`
/// tail sends `POST /?grpc=4317/v1/metrics` and a `#frag` tail sends
/// `POST /` -- both dead exports, neither logged. Measured against a sink on
/// opentelemetry-otlp 0.32 ([ANW-42]). A path prefix composes correctly and
/// is left alone.
///
/// [ANW-42]: https://crvrs.youtrack.cloud/issue/ANW-42
fn check_generic_endpoint(endpoint: &str) -> anyhow::Result<()> {
    if let Some(bad) = endpoint.find(['?', '#']) {
        let tail = &endpoint[bad..];
        bail!(
            "OTEL_EXPORTER_OTLP_ENDPOINT {endpoint:?} has a trailing {tail:?}; \
             the exporter would append the signal path after it and export nowhere. \
             Pass the base URL alone, and put an uptrace DSN in \
             OTEL_EXPORTER_OTLP_HEADERS=uptrace-dsn=<the DSN, verbatim>"
        );
    }
    Ok(())
}

/// The one OTLP protocol this binary speaks. `opentelemetry-otlp` is built
/// with the `http-proto` feature alone (Cargo.toml), so neither `grpc` nor
/// `http/json` has a transport behind it.
const SUPPORTED_PROTOCOL: &str = "http/protobuf";

/// Reject a protocol variable this binary cannot honor. The exporter builder
/// picks its transport at compile time, so `OTEL_EXPORTER_OTLP_PROTOCOL=grpc`
/// does not switch anything: the export keeps going out as HTTP protobuf,
/// with nothing in the log (measured, [ANW-42]). An operator who follows
/// uptrace's console to the gRPC port would get exactly the dead-silent
/// export this issue exists to remove, so it fails at startup instead.
///
/// [ANW-42]: https://crvrs.youtrack.cloud/issue/ANW-42
fn check_protocols(env: &OtelEnv) -> anyhow::Result<()> {
    let vars = [
        ("OTEL_EXPORTER_OTLP_PROTOCOL", env.protocol.as_deref()),
        (
            "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
            env.metrics_protocol.as_deref(),
        ),
        (
            "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
            env.traces_protocol.as_deref(),
        ),
    ];
    for (name, value) in vars {
        let Some(value) = value else { continue };
        if value != SUPPORTED_PROTOCOL {
            bail!(
                "{name}={value:?} is not supported; this build speaks \
                 {SUPPORTED_PROTOCOL} only. Unset the variable, and point the \
                 endpoint at the collector's HTTP port rather than its gRPC one"
            );
        }
    }
    Ok(())
}

/// The resource attributes anwesen supplies as defaults, minus every key the
/// environment already sets.
///
/// `service.name=anwesen` and the crate version are defaults, not policy:
/// contract point 6 of [ANW-42] keeps `OTEL_SERVICE_NAME` and
/// `OTEL_RESOURCE_ATTRIBUTES` working. Attaching them unconditionally blocks
/// the override, because an attribute set on the builder wins over the one
/// the SDK's own detectors read from those variables (measured by sie).
///
/// Only key presence matters here; the values stay the SDK's to parse.
///
/// [ANW-42]: https://crvrs.youtrack.cloud/issue/ANW-42
fn default_resource_attrs(
    service_name: Option<&str>,
    resource_attributes: Option<&str>,
) -> Vec<KeyValue> {
    let from_attrs: Vec<&str> = resource_attributes
        .unwrap_or_default()
        .split(',')
        .filter_map(|entry| entry.split_once('='))
        .map(|(key, _)| key.trim())
        .collect();
    let set_by_env = |key: &str| {
        (key == SERVICE_NAME && service_name.is_some_and(|v| !v.trim().is_empty()))
            || from_attrs.contains(&key)
    };
    [
        (SERVICE_NAME, "anwesen"),
        (SERVICE_VERSION, env!("CARGO_PKG_VERSION")),
    ]
    .into_iter()
    .filter(|(key, _)| !set_by_env(key))
    .map(|(key, value)| KeyValue::new(key, value))
    .collect()
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
        let TelemetryConfig { slow_request, env } = config;

        // Service identity is a default only: a key OTEL_SERVICE_NAME or
        // OTEL_RESOURCE_ATTRIBUTES already carries is left to the SDK's own
        // detectors, because a builder attribute would win over them.
        let mut resource = Resource::builder();
        for attr in default_resource_attrs(
            std::env::var("OTEL_SERVICE_NAME").ok().as_deref(),
            std::env::var("OTEL_RESOURCE_ATTRIBUTES").ok().as_deref(),
        ) {
            resource = resource.with_attribute(attr);
        }
        let resource = resource.build();

        // -- metrics: thread-based periodic reader over an OTLP/HTTP exporter.
        // No endpoint, headers, or protocol here: the SDK reads
        // OTEL_EXPORTER_OTLP_* itself and composes the per-signal URL
        // (ANW-42). `.with_http()` picks the only transport this binary is
        // built with, http/protobuf -- the default, and what anwesen sent
        // before. Any other OTEL_EXPORTER_OTLP_PROTOCOL value would be
        // ignored here, so `check_protocols` rejects it at startup.
        let metric_exporter = MetricExporter::builder()
            .with_http()
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
            endpoint = %env.describe(),
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

    fn generic(endpoint: &str) -> OtelEnv {
        OtelEnv {
            endpoint: Some(endpoint.into()),
            ..OtelEnv::default()
        }
    }

    #[test]
    fn resolve_off_when_no_endpoint_variable_is_set() {
        let cfg = TelemetryConfig::resolve(&raw(), OtelEnv::default()).unwrap();
        assert!(cfg.is_none(), "no OTEL_ endpoint means telemetry off");
    }

    #[test]
    fn resolve_on_for_each_endpoint_variable() {
        let per_signal = |field: fn(&mut OtelEnv)| {
            let mut env = OtelEnv::default();
            field(&mut env);
            env
        };
        for env in [
            generic("https://collector.example.com"),
            per_signal(|e| {
                e.metrics_endpoint = Some("https://collector.example.com/v1/metrics".into());
            }),
            per_signal(|e| {
                e.traces_endpoint = Some("https://collector.example.com/v1/traces".into());
            }),
        ] {
            let cfg = TelemetryConfig::resolve(
                &RawTelemetryArgs {
                    slow_request_ms: 250,
                    ..raw()
                },
                env.clone(),
            )
            .unwrap_or_else(|e| panic!("{env:?}: {e}"))
            .expect("telemetry on");
            assert_eq!(cfg.slow_request, Duration::from_millis(250));
        }
    }

    /// The tails the SDK mangles: a query lands in the query string with the
    /// signal path behind it, a fragment drops the signal path entirely.
    #[test]
    fn resolve_rejects_a_query_or_fragment_on_the_generic_endpoint() {
        for endpoint in [
            "https://api.uptrace.dev?grpc=4317",
            "https://api.uptrace.dev/?grpc=4317",
            "https://api.uptrace.dev:4318?grpc=4317",
            "https://api.uptrace.dev#frag",
        ] {
            let err = TelemetryConfig::resolve(&raw(), generic(endpoint)).unwrap_err();
            assert!(err.to_string().contains("trailing"), "{endpoint}: {err}");
        }
    }

    /// A path prefix composes correctly (`/otlp` -> `POST /otlp/v1/metrics`),
    /// and the per-signal variables are used verbatim, tail and all.
    #[test]
    fn resolve_accepts_a_path_prefix_and_per_signal_tails() {
        for env in [
            generic("https://collector.example.com/otlp"),
            generic("https://collector.example.com/"),
            OtelEnv {
                metrics_endpoint: Some("https://api.uptrace.dev/v1/metrics?grpc=4317".into()),
                ..OtelEnv::default()
            },
        ] {
            TelemetryConfig::resolve(&raw(), env.clone())
                .unwrap_or_else(|e| panic!("{env:?}: {e}"))
                .expect("telemetry on");
        }
    }

    /// `.with_http()` fixes the transport at compile time, so a protocol this
    /// build cannot speak is a dead export with nothing in the log.
    #[test]
    fn resolve_rejects_a_protocol_this_build_cannot_speak() {
        let with_protocol = |field: fn(&mut OtelEnv)| {
            let mut env = generic("https://collector.example.com");
            field(&mut env);
            env
        };
        for env in [
            with_protocol(|e| e.protocol = Some("grpc".into())),
            with_protocol(|e| e.protocol = Some("http/json".into())),
            with_protocol(|e| e.metrics_protocol = Some("grpc".into())),
            with_protocol(|e| e.traces_protocol = Some("grpc".into())),
        ] {
            let err = TelemetryConfig::resolve(&raw(), env.clone())
                .unwrap_err()
                .to_string();
            assert!(err.contains("http/protobuf"), "{env:?}: {err}");
        }
    }

    /// The default protocol is the one this build speaks, so naming it
    /// explicitly is not an error.
    #[test]
    fn resolve_accepts_the_supported_protocol_spelled_out() {
        let env = OtelEnv {
            protocol: Some("http/protobuf".into()),
            metrics_protocol: Some("http/protobuf".into()),
            ..generic("https://collector.example.com")
        };
        TelemetryConfig::resolve(&raw(), env)
            .unwrap()
            .expect("telemetry on");
    }

    /// Contract point 6: `service.name` stays anwesen's default only while
    /// the environment supplies none. A builder attribute wins over the SDK's
    /// detectors, so the default has to step aside for the override to work.
    #[test]
    fn service_identity_defaults_step_aside_for_the_environment() {
        let keys = |attrs: &[KeyValue]| {
            attrs
                .iter()
                .map(|kv| kv.key.as_str().to_string())
                .collect::<Vec<_>>()
        };

        let plain = default_resource_attrs(None, None);
        assert_eq!(keys(&plain), [SERVICE_NAME, SERVICE_VERSION]);

        let named = default_resource_attrs(Some("svcname-override"), None);
        assert_eq!(keys(&named), [SERVICE_VERSION]);

        let attrs = default_resource_attrs(
            None,
            Some("deployment.environment=prod,service.name=attrs-override"),
        );
        assert_eq!(keys(&attrs), [SERVICE_VERSION]);

        let versioned = default_resource_attrs(None, Some("service.version=9.9.9"));
        assert_eq!(keys(&versioned), [SERVICE_NAME]);

        // An empty OTEL_SERVICE_NAME supplies nothing; the SDK ignores it too.
        let empty = default_resource_attrs(Some("  "), None);
        assert_eq!(keys(&empty), [SERVICE_NAME, SERVICE_VERSION]);
    }

    #[test]
    fn resolve_rejects_the_removed_uptrace_dsn() {
        let err = TelemetryConfig::resolve(
            &RawTelemetryArgs {
                uptrace_dsn: Some("https://tok@api.uptrace.dev".into()),
                ..raw()
            },
            OtelEnv::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--uptrace-dsn"), "{err}");
        assert!(err.contains("OTEL_EXPORTER_OTLP_HEADERS"), "{err}");
    }

    #[test]
    fn resolve_rejects_the_removed_otlp_endpoint() {
        let err = TelemetryConfig::resolve(
            &RawTelemetryArgs {
                otlp_endpoint: Some("https://collector.example.com".into()),
                ..raw()
            },
            OtelEnv::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--otlp-endpoint"), "{err}");
        assert!(err.contains("OTEL_EXPORTER_OTLP_ENDPOINT"), "{err}");
    }

    #[test]
    fn resolve_rejects_the_removed_otlp_header() {
        let err = TelemetryConfig::resolve(
            &RawTelemetryArgs {
                otlp_headers: vec!["authorization=Bearer xyz".into()],
                ..raw()
            },
            OtelEnv::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--otlp-header"), "{err}");
        assert!(err.contains("OTEL_EXPORTER_OTLP_HEADERS"), "{err}");
    }

    /// A removed option fails even with a correct `OTEL_` endpoint alongside
    /// it: the operator's intent is in the flag, and half-applied config is
    /// the silent failure this issue removes.
    #[test]
    fn resolve_rejects_a_removed_option_alongside_a_good_endpoint() {
        let err = TelemetryConfig::resolve(
            &RawTelemetryArgs {
                uptrace_dsn: Some("https://tok@api.uptrace.dev".into()),
                ..raw()
            },
            generic("https://api.uptrace.dev"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("removed"), "{err}");
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
