//! OTLP export to an OpenTelemetry collector / Grafana LGTM stack. Feature-gated
//! (`otlp`); when the feature is off this module doesn't exist and the crate has
//! zero OTEL dependencies.
//!
//! All three signals (traces, logs, metrics) share one
//! [`opentelemetry_sdk::Resource`] (service name + version) and export over
//! OTLP/HTTP with batching. Batch export spawns background tasks, so [`build`]
//! must be called from within a Tokio runtime (every POSEIDEN binary is).

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::logs::LoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::runtime;
use opentelemetry_sdk::trace::TracerProvider;
use opentelemetry_sdk::Resource;
use tracing_subscriber::Layer;

use crate::config::{OtlpSettings, RuntimeInfo};
use crate::BoxLayer;

/// Live OTEL providers, kept so telemetry can be flushed + shut down on guard
/// drop. Without an explicit shutdown, batched spans/metrics buffered at exit
/// are lost.
pub struct OtelProviders {
    tracer: TracerProvider,
    logger: LoggerProvider,
    meter: SdkMeterProvider,
}

impl OtelProviders {
    /// Flush and stop every provider. Errors are swallowed - we're on the way
    /// out and a flush failure shouldn't mask the real exit path.
    pub fn shutdown(self) {
        let _ = self.tracer.shutdown();
        let _ = self.logger.shutdown();
        let _ = self.meter.shutdown();
    }
}

/// Build the OTLP sink: returns the tracing layers to install (trace bridge, log
/// bridge, metrics layer) plus the providers to hold for shutdown.
pub fn build(
    settings: &OtlpSettings,
    rt: &RuntimeInfo,
) -> Result<(Vec<BoxLayer>, OtelProviders), String> {
    let resource = Resource::new(vec![
        KeyValue::new("service.name", rt.service_name.clone()),
        KeyValue::new("service.version", rt.service_version.clone()),
    ]);
    let endpoint = settings.endpoint.trim_end_matches('/').to_string();

    let tracer_provider = build_tracer(&resource, &endpoint)?;
    let logger_provider = build_logger(&resource, &endpoint)?;
    let meter_provider = build_meter(&resource, &endpoint)?;

    // Register globals so context propagation + `global::meter()` work app-wide.
    opentelemetry::global::set_tracer_provider(tracer_provider.clone());
    opentelemetry::global::set_meter_provider(meter_provider.clone());

    let tracer = tracer_provider.tracer("poseiden-telemetry");
    let trace_layer = tracing_opentelemetry::layer().with_tracer(tracer).boxed();
    let log_layer = OpenTelemetryTracingBridge::new(&logger_provider).boxed();
    let metrics_layer = tracing_opentelemetry::MetricsLayer::new(meter_provider.clone()).boxed();

    Ok((
        vec![trace_layer, log_layer, metrics_layer],
        OtelProviders {
            tracer: tracer_provider,
            logger: logger_provider,
            meter: meter_provider,
        },
    ))
}

fn build_tracer(resource: &Resource, endpoint: &str) -> Result<TracerProvider, String> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| format!("trace exporter: {e}"))?;
    Ok(TracerProvider::builder()
        .with_resource(resource.clone())
        .with_batch_exporter(exporter, runtime::Tokio)
        .build())
}

fn build_logger(resource: &Resource, endpoint: &str) -> Result<LoggerProvider, String> {
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| format!("log exporter: {e}"))?;
    Ok(LoggerProvider::builder()
        .with_resource(resource.clone())
        .with_batch_exporter(exporter, runtime::Tokio)
        .build())
}

fn build_meter(resource: &Resource, endpoint: &str) -> Result<SdkMeterProvider, String> {
    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| format!("metric exporter: {e}"))?;
    let reader = PeriodicReader::builder(exporter, runtime::Tokio).build();
    Ok(SdkMeterProvider::builder()
        .with_resource(resource.clone())
        .with_reader(reader)
        .build())
}
