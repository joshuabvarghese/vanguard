//! Wires Vanguard's existing `tracing` instrumentation to Azure Monitor
//! (Application Insights) via OpenTelemetry, and implements
//! [`TelemetrySink`] on top of the same pipeline.
//!
//! Deliberate design choice: this does *not* introduce a second,
//! parallel logging system. `tracing::info!`/`error!` calls already
//! scattered through `reconcile.rs`, `operator.rs`, `api.rs` etc. keep
//! working unchanged — `init()` just adds an OpenTelemetry layer to the
//! existing `tracing_subscriber::Registry` (in `main.rs`) that exports
//! spans as Application Insights dependencies/requests and events as
//! traces. `TelemetrySink::emit` is a thin convenience for the
//! business-level events (`api.rs` tenant lifecycle, `chaos.rs` chaos
//! runs) that want a named custom event rather than a log line, using the
//! same exporter so there's exactly one place ingestion behavior is
//! configured.

use crate::cloud::TelemetrySink;
use async_trait::async_trait;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::TracerProvider;
use std::collections::HashMap;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry};

/// Installs the global `tracing` subscriber with an Azure Monitor export
/// layer. Call once at startup, in place of the plain
/// `tracing_subscriber::fmt().init()` call `main.rs` uses when Azure isn't
/// configured. Returns the `TracerProvider` so `main.rs` can flush it on
/// graceful shutdown (Application Insights batches exports; a bare
/// `process::exit` would drop the last batch).
pub fn init(connection_string: &str) -> anyhow::Result<TracerProvider> {
    let exporter = opentelemetry_application_insights::Exporter::new_from_connection_string(
        connection_string,
        reqwest::Client::new(),
    )?;
    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .build();
    let tracer = provider.tracer("vanguard-control-plane");

    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let fmt_layer = tracing_subscriber::fmt::layer();
    let filter = EnvFilter::from_default_env().add_directive("info".parse()?);

    Registry::default()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer)
        .try_init()?;

    Ok(provider)
}

/// [`TelemetrySink`] backed by the OpenTelemetry pipeline installed by
/// [`init`]. Emits a `tracing` event on a dedicated target
/// (`vanguard::telemetry_event`) carrying the event name and properties as
/// structured fields; the App Insights exporter surfaces these as custom
/// events, distinct from the ordinary log-level spans everything else
/// produces.
#[derive(Default)]
pub struct AzureMonitorTelemetrySink;

#[async_trait]
impl TelemetrySink for AzureMonitorTelemetrySink {
    async fn emit(&self, event_name: &str, properties: HashMap<String, String>) {
        let props_json = serde_json::to_string(&properties).unwrap_or_default();
        tracing::info!(
            target: "vanguard::telemetry_event",
            event_name,
            properties = %props_json,
        );
    }
}
