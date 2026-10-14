//! Logs, traces and metrics. Stdout logging always; OTLP/HTTP export of all
//! three signals when an endpoint is configured (OpenObserve speaks OTLP).

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::config::TelemetryConfig;

#[derive(Default)]
pub struct TelemetryGuard {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    logger: Option<SdkLoggerProvider>,
}

impl TelemetryGuard {
    pub fn shutdown(self) {
        if let Some(t) = self.tracer {
            let _ = t.shutdown();
        }
        if let Some(m) = self.meter {
            let _ = m.shutdown();
        }
        if let Some(l) = self.logger {
            let _ = l.shutdown();
        }
    }
}

/// Stdout-only logging for CLI subcommands.
pub fn init_cli_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ =
        tracing_subscriber::registry().with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_filter(filter)).try_init();
}

/// Initialise logging and, if configured, OTLP export. `extra_headers`
/// (e.g. loaded from the vault) override configured headers.
pub fn init(cfg: &TelemetryConfig, extra_headers: BTreeMap<String, String>) -> Result<TelemetryGuard> {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.log_filter));
    let fmt_layer = match cfg.log_format.as_str() {
        "json" => tracing_subscriber::fmt::layer().json().with_current_span(true).boxed(),
        _ => tracing_subscriber::fmt::layer().boxed(),
    };

    let Some(endpoint) = cfg.otlp_endpoint.as_deref().filter(|e| !e.is_empty()) else {
        tracing_subscriber::registry().with(fmt_layer.with_filter(filter())).try_init()?;
        return Ok(TelemetryGuard::default());
    };
    let endpoint = endpoint.trim_end_matches('/');
    let mut headers: HashMap<String, String> = cfg.otlp_headers.clone().into_iter().collect();
    headers.extend(extra_headers);

    let resource = Resource::builder()
        .with_service_name(cfg.service_name.clone())
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .build();

    let span_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/traces"))
        .with_headers(headers.clone())
        .with_timeout(Duration::from_secs(10))
        .build()?;
    let tracer = SdkTracerProvider::builder().with_resource(resource.clone()).with_batch_exporter(span_exporter).build();

    let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/metrics"))
        .with_headers(headers.clone())
        .with_timeout(Duration::from_secs(10))
        .build()?;
    let reader = PeriodicReader::builder(metric_exporter).with_interval(Duration::from_secs(cfg.metrics_interval_secs.max(5))).build();
    let meter = SdkMeterProvider::builder().with_resource(resource.clone()).with_reader(reader).build();
    opentelemetry::global::set_meter_provider(meter.clone());

    let log_exporter = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/logs"))
        .with_headers(headers)
        .with_timeout(Duration::from_secs(10))
        .build()?;
    let logger = SdkLoggerProvider::builder().with_resource(resource).with_batch_exporter(log_exporter).build();

    // Keep the exporters' own HTTP client logs out of the export pipeline.
    let otel_filter = || filter().add_directive("hyper=off".parse().unwrap()).add_directive("reqwest=off".parse().unwrap());
    tracing_subscriber::registry()
        .with(fmt_layer.with_filter(filter()))
        .with(tracing_opentelemetry::layer().with_tracer(tracer.tracer("xolotl")).with_filter(otel_filter()))
        .with(OpenTelemetryTracingBridge::new(&logger).with_filter(otel_filter()))
        .try_init()?;

    opentelemetry::global::set_text_map_propagator(opentelemetry_sdk::propagation::TraceContextPropagator::new());
    tracing::info!(endpoint, "OTLP export enabled");
    Ok(TelemetryGuard { tracer: Some(tracer), meter: Some(meter), logger: Some(logger) })
}

/// Application metric instruments.
pub struct Metrics {
    pub requests: Counter<u64>,
    pub duration: Histogram<f64>,
    pub tokens: Counter<u64>,
    pub upstream_errors: Counter<u64>,
    pub account_usage: Gauge<f64>,
    pub account_available: Gauge<u64>,
    pub token_refreshes: Counter<u64>,
    pub unsaved_credentials: Gauge<u64>,
    pub mcp_tool_calls: Counter<u64>,
    pub mcp_tool_call_duration: Histogram<f64>,
    pub mcp_executions: Counter<u64>,
}

pub fn metrics() -> &'static Metrics {
    static M: OnceLock<Metrics> = OnceLock::new();
    M.get_or_init(|| {
        let meter = opentelemetry::global::meter("xolotl");
        Metrics {
            requests: meter.u64_counter("xolotl.requests").with_description("Proxied LLM requests").build(),
            duration: meter
                .f64_histogram("xolotl.request.duration")
                .with_unit("s")
                .with_description("End-to-end proxied request duration")
                .build(),
            tokens: meter.u64_counter("xolotl.tokens").with_description("Tokens by direction").build(),
            upstream_errors: meter.u64_counter("xolotl.upstream.errors").build(),
            account_usage: meter
                .f64_gauge("xolotl.account.usage_percent")
                .with_unit("%")
                .with_description("Subscription quota used per window")
                .build(),
            account_available: meter.u64_gauge("xolotl.account.available").with_description("1 when the account can take requests").build(),
            token_refreshes: meter.u64_counter("xolotl.oauth.refreshes").build(),
            unsaved_credentials: meter
                .u64_gauge("xolotl.vault.unsaved_credentials")
                .with_description("Refreshed OAuth credentials not yet written to the vault")
                .build(),
            mcp_tool_calls: meter.u64_counter("xolotl.mcp.tool_calls").with_description("Upstream MCP tool calls by status").build(),
            mcp_tool_call_duration: meter
                .f64_histogram("xolotl.mcp.tool_call.duration")
                .with_unit("s")
                .with_description("Upstream MCP tool call duration")
                .build(),
            mcp_executions: meter.u64_counter("xolotl.mcp.executions").with_description("Code-mode executions by outcome").build(),
        }
    })
}

/// The current OpenTelemetry trace id, if the current span is sampled.
pub fn current_trace_id() -> Option<String> {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    let ctx = tracing::Span::current().context();
    let span = ctx.span();
    let sc = span.span_context();
    sc.is_valid().then(|| sc.trace_id().to_string())
}
