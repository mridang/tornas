//! OpenTelemetry. A Prometheus meter provider is always installed so `/metrics`
//! works; when an OTLP endpoint is configured, traces, logs and metrics are also
//! pushed to the collector over OTLP/gRPC.
//!
//! This module owns the SDK providers so they outlive the layers and instruments
//! that reference them and can be flushed on shutdown. Building the `tracing`
//! layers is [`logging`](crate::logging)'s job and the metric instruments are
//! [`metrics`](crate::metrics)', so the wiring stays where it belongs.

use anyhow::Context;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::{
    Resource,
    logs::SdkLoggerProvider,
    metrics::{PeriodicReader, SdkMeterProvider},
    trace::SdkTracerProvider,
};

/// The telemetry providers. The meter provider and its Prometheus registry always
/// exist; the OTLP tracer and logger only when export is on.
pub struct Telemetry {
    registry: prometheus::Registry,
    meter_provider: SdkMeterProvider,
    tracer_provider: Option<SdkTracerProvider>,
    logger_provider: Option<SdkLoggerProvider>,
}

/// Install the meter provider (always) and, when `endpoint` is set, the OTLP
/// exporters. Sets the global meter provider so [`opentelemetry::global::meter`]
/// works from anywhere.
pub fn init(endpoint: Option<&str>, service_name: &str) -> anyhow::Result<Telemetry> {
    let endpoint = endpoint.map(str::trim).filter(|e| !e.is_empty());
    let resource = Resource::builder()
        .with_service_name(service_name.to_owned())
        .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            env!("CARGO_PKG_VERSION"),
        ))
        .build();

    // The scrape endpoint: a Prometheus reader gathering into our own registry.
    let registry = prometheus::Registry::new();
    let prometheus = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        // Keep the series names and TYPE lines this project has always exposed:
        // no unit or _total suffixing, and no target_info / otel_scope_info metrics.
        .without_units()
        .without_counter_suffixes()
        .without_target_info()
        .without_scope_info()
        .build()
        .context("building the Prometheus metric reader")?;
    let mut meter = SdkMeterProvider::builder()
        .with_reader(prometheus)
        .with_resource(resource.clone());

    let (tracer_provider, logger_provider) = if let Some(endpoint) = endpoint {
        let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .context("building OTLP metric exporter")?;
        meter = meter.with_reader(PeriodicReader::builder(metric_exporter).build());

        let span_exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .context("building OTLP span exporter")?;
        let tracer_provider = SdkTracerProvider::builder()
            .with_batch_exporter(span_exporter)
            .with_resource(resource.clone())
            .build();

        let log_exporter = opentelemetry_otlp::LogExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .context("building OTLP log exporter")?;
        let logger_provider = SdkLoggerProvider::builder()
            .with_batch_exporter(log_exporter)
            .with_resource(resource)
            .build();

        (Some(tracer_provider), Some(logger_provider))
    } else {
        (None, None)
    };

    let meter_provider = meter.build();
    opentelemetry::global::set_meter_provider(meter_provider.clone());

    Ok(Telemetry {
        registry,
        meter_provider,
        tracer_provider,
        logger_provider,
    })
}

impl Telemetry {
    /// The Prometheus registry the `/metrics` handler encodes.
    pub fn registry(&self) -> prometheus::Registry {
        self.registry.clone()
    }

    /// A tracer for the `tracing-opentelemetry` span layer, when export is on.
    pub fn tracer(&self) -> Option<opentelemetry_sdk::trace::SdkTracer> {
        self.tracer_provider.as_ref().map(|p| p.tracer("tornas"))
    }

    /// The logger provider for the `opentelemetry-appender-tracing` bridge.
    pub fn logger_provider(&self) -> Option<&SdkLoggerProvider> {
        self.logger_provider.as_ref()
    }

    /// Flush and stop the providers. Call once, at process exit.
    pub fn shutdown(&self) {
        if let Err(e) = self.meter_provider.shutdown() {
            tracing::warn!("shutting down meter provider: {e}");
        }
        if let Some(p) = &self.tracer_provider
            && let Err(e) = p.shutdown()
        {
            tracing::warn!("shutting down tracer provider: {e}");
        }
        if let Some(p) = &self.logger_provider
            && let Err(e) = p.shutdown()
        {
            tracing::warn!("shutting down logger provider: {e}");
        }
    }
}
