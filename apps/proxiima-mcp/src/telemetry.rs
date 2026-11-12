//! Metric export over OTLP: the serving command's meter provider.
//!
//! The runtime records through the global meter, so the provider is installed
//! before the runtime starts. The OpenTelemetry SDK reads the standard
//! `OTEL_EXPORTER_OTLP_*` variables from the process environment itself; the
//! host decides only whether to install one.

use opentelemetry::global;
use opentelemetry_otlp::MetricExporter;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::resource::{EnvResourceDetector, TelemetryResourceDetector};
use proxiima::ProxiimaError;

use crate::CliError;
use crate::settings::{OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_SERVICE_NAME};

const DEFAULT_SERVICE_NAME: &str = "proxiima-mcp";

/// The installed meter provider; [`Guard::shutdown`] flushes its last interval.
#[derive(Debug)]
#[must_use = "dropping the guard skips the final flush; call `shutdown`"]
pub(crate) struct Guard(SdkMeterProvider);

impl Guard {
    /// Export what the last interval collected and stop the reader. A failed
    /// flush is logged, not returned: the process is ending either way.
    pub(crate) fn shutdown(self) {
        if let Err(err) = self.0.shutdown() {
            tracing::warn!(error = %err, "the final OpenTelemetry metrics export failed");
        }
    }
}

/// Install the global meter provider when `OTEL_EXPORTER_OTLP_ENDPOINT` is
/// set, and nothing otherwise.
///
/// # Errors
///
/// A configuration error when the exporter refuses its environment, for
/// example an endpoint that is not a URI.
pub(crate) fn install(lookup: &impl Fn(&str) -> Option<String>) -> Result<Option<Guard>, CliError> {
    if OTEL_EXPORTER_OTLP_ENDPOINT.read(lookup).is_none() {
        return Ok(None);
    }
    let provider = build_provider(lookup)?;
    global::set_meter_provider(provider.clone());
    Ok(Some(Guard(provider)))
}

/// A meter provider that pushes every default interval over HTTP/protobuf.
fn build_provider(lookup: &impl Fn(&str) -> Option<String>) -> Result<SdkMeterProvider, CliError> {
    let exporter = MetricExporter::builder()
        .with_http()
        .build()
        .map_err(|err| {
            CliError::Runtime(ProxiimaError::Config(format!(
                "{OTEL_EXPORTER_OTLP_ENDPOINT}: {err}"
            )))
        })?;
    Ok(SdkMeterProvider::builder()
        .with_periodic_exporter(exporter)
        .with_resource(resource(lookup))
        .build())
}

/// The service name is `OTEL_SERVICE_NAME`, else `service.name` of
/// `OTEL_RESOURCE_ATTRIBUTES`, else `proxiima-mcp`.
fn resource(lookup: &impl Fn(&str) -> Option<String>) -> Resource {
    let named = Resource::builder_empty()
        .with_service_name(DEFAULT_SERVICE_NAME)
        .with_detector(Box::new(TelemetryResourceDetector))
        .with_detector(Box::new(EnvResourceDetector::new()));
    match OTEL_SERVICE_NAME.read(lookup) {
        Some(name) => named.with_service_name(name),
        None => named,
    }
    .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::Key;

    #[test]
    fn nothing_is_installed_without_an_endpoint() {
        assert!(install(&|_| None).expect("install").is_none());
        assert!(
            install(&|_| Some(String::new()))
                .expect("install")
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_provider_is_built_without_a_network() {
        let provider = build_provider(&|_| None).expect("provider");
        provider.shutdown().expect("shutdown");
    }

    #[test]
    fn the_service_name_defaults_and_the_environment_names_win() {
        let name = |lookup: &dyn Fn(&str) -> Option<String>| {
            resource(&lookup)
                .get(&Key::from_static_str("service.name"))
                .map(|value| value.to_string())
        };
        assert_eq!(name(&|_| None).as_deref(), Some("proxiima-mcp"));
        assert_eq!(
            name(&|key| (key == "OTEL_SERVICE_NAME").then(|| "edge".to_owned())).as_deref(),
            Some("edge")
        );
    }
}
