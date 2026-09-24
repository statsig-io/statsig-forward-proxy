use opentelemetry_otlp::{ExporterBuildError, MetricExporter, Protocol, WithExportConfig};

pub fn build_metrics_exporter(
    protocol: Protocol,
    endpoint: Option<&str>,
) -> Result<MetricExporter, ExporterBuildError> {
    match protocol {
        Protocol::Grpc => {
            let mut builder = MetricExporter::builder()
                .with_tonic()
                .with_protocol(Protocol::Grpc);
            if let Some(endpoint) = endpoint {
                builder = builder.with_endpoint(endpoint);
            }
            builder.build()
        }
        Protocol::HttpBinary | Protocol::HttpJson => {
            let mut builder = MetricExporter::builder()
                .with_http()
                .with_protocol(protocol);
            if let Some(endpoint) = endpoint {
                builder = builder.with_endpoint(endpoint);
            }
            builder.build()
        }
    }
}
