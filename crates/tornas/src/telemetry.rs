//! OpenTelemetry export. Off unless an OTLP endpoint is configured; when it is,
//! traces and logs (and, once wired in [`metrics`](crate::metrics), metrics) are
//! pushed to the collector over OTLP/gRPC.
//!
//! This module owns the SDK providers so they outlive the layers that reference
//! them and can be flushed on shutdown. Building the `tracing` layers themselves is
//! [`logging`](crate::logging)'s job, so the whole subscriber is assembled in one
//! place.

use anyhow::Context;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::{Resource, logs::SdkLoggerProvider, trace::SdkTracerProvider};

/// The configured OTLP exporters, or nothing when export is off.
#[derive(Default)]
pub struct Telemetry {
    tracer_provider: Option<SdkTracerProvider>,
    logger_provider: Option<SdkLoggerProvider>,
}

/// Build the exporters. With no `endpoint`, returns an inert handle: no providers,
/// no network, and the `tracing` layers below are `None`.
pub fn init(endpoint: Option<&str>, service_name: &str) -> anyhow::Result<Telemetry> {
    let Some(endpoint) = endpoint.map(str::trim).filter(|e| !e.is_empty()) else {
        return Ok(Telemetry::default());
    };
    let resource = Resource::builder()
        .with_service_name(service_name.to_owned())
        .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            env!("CARGO_PKG_VERSION"),
        ))
        .build();

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

    Ok(Telemetry {
        tracer_provider: Some(tracer_provider),
        logger_provider: Some(logger_provider),
    })
}

impl Telemetry {
    /// A tracer for the `tracing-opentelemetry` span layer, when export is on.
    pub fn tracer(&self) -> Option<opentelemetry_sdk::trace::SdkTracer> {
        self.tracer_provider.as_ref().map(|p| p.tracer("tornas"))
    }

    /// The logger provider for the `opentelemetry-appender-tracing` bridge.
    pub fn logger_provider(&self) -> Option<&SdkLoggerProvider> {
        self.logger_provider.as_ref()
    }

    /// Flush and stop the exporters. Call once, at process exit.
    pub fn shutdown(&self) {
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
