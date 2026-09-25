use async_trait::async_trait;
use opentelemetry::metrics::MeterProvider;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{
    Protocol, OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_EXPORTER_OTLP_METRICS_ENDPOINT,
    OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE,
};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    metrics_service_server::{MetricsService, MetricsServiceServer},
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::any_value;
use opentelemetry_proto::tonic::metrics::v1::{
    metric, AggregationTemporality, Histogram, HistogramDataPoint,
};
use opentelemetry_sdk::metrics::{exporter::PushMetricExporter, SdkMeterProvider, Temporality};
use statsig_forward_proxy::otlp::build_metrics_exporter;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::TcpListenerStream;
use tonic_otel::transport::Server;

#[derive(Debug)]
struct InProcessOtlpReceiver {
    requests: mpsc::UnboundedSender<ExportMetricsServiceRequest>,
}

#[async_trait]
impl MetricsService for InProcessOtlpReceiver {
    async fn export(
        &self,
        request: tonic_otel::Request<ExportMetricsServiceRequest>,
    ) -> Result<tonic_otel::Response<ExportMetricsServiceResponse>, tonic_otel::Status> {
        self.requests
            .send(request.into_inner())
            .expect("test receiver should still be listening");
        Ok(tonic_otel::Response::new(ExportMetricsServiceResponse {
            partial_success: None,
        }))
    }
}

fn find_histogram<'a>(
    request: &'a ExportMetricsServiceRequest,
    metric_name: &str,
) -> &'a Histogram {
    request
        .resource_metrics
        .iter()
        .flat_map(|resource| &resource.scope_metrics)
        .flat_map(|scope| &scope.metrics)
        .find_map(|candidate| match candidate.data.as_ref() {
            Some(metric::Data::Histogram(histogram)) if candidate.name == metric_name => {
                Some(histogram)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing histogram {metric_name}"))
}

fn string_attribute<'a>(point: &'a HistogramDataPoint, key: &str) -> Option<&'a str> {
    point
        .attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| match value.value.as_ref() {
            Some(any_value::Value::StringValue(value)) => Some(value.as_str()),
            _ => None,
        })
}

async fn receive_export(
    requests: &mut mpsc::UnboundedReceiver<ExportMetricsServiceRequest>,
) -> ExportMetricsServiceRequest {
    tokio::time::timeout(Duration::from_secs(5), requests.recv())
        .await
        .expect("OTLP exporter should send within five seconds")
        .expect("OTLP receiver should remain connected")
}

async fn assert_export_starts_tls_handshake(protocol: Protocol) {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "https://127.0.0.1:{}",
        listener.local_addr().unwrap().port()
    );
    let handshake = tokio::spawn(async move {
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("OTLP exporter should connect within five seconds")
            .expect("OTLP TLS listener should accept the connection");
        let mut tls_record_header = [0; 2];
        tokio::time::timeout(
            Duration::from_secs(5),
            stream.read_exact(&mut tls_record_header),
        )
        .await
        .expect("OTLP exporter should write within five seconds")
        .expect("OTLP exporter should write a TLS handshake");
        tls_record_header
    });

    let exporter = temp_env::with_vars(
        [
            (OTEL_EXPORTER_OTLP_ENDPOINT, None),
            (OTEL_EXPORTER_OTLP_METRICS_ENDPOINT, None),
            ("OTEL_EXPORTER_ENDPOINT", None),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", Some("1")),
            ("NO_PROXY", Some("*")),
            ("no_proxy", Some("*")),
        ],
        || build_metrics_exporter(protocol, Some(&endpoint)).unwrap(),
    );
    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter)
        .build();
    meter_provider
        .meter("statsig.forward_proxy")
        .u64_counter("statsig.forward_proxy.TlsRegressionTest.count")
        .build()
        .add(1, &[]);

    let flush_result = meter_provider.force_flush();
    let tls_record_header = handshake.await.unwrap_or_else(|error| {
        panic!("TLS handshake failed after export {flush_result:?}: {error}")
    });
    assert_eq!(tls_record_header, [0x16, 0x03]);
    let _ = meter_provider.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exporters_use_tls_for_https_endpoints() {
    assert_export_starts_tls_handshake(Protocol::HttpBinary).await;
    assert_export_starts_tls_handshake(Protocol::Grpc).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exporter_sends_delta_histograms_to_receiver() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (request_sender, mut requests) = mpsc::unbounded_channel();
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    let server = tokio::spawn(
        Server::builder()
            .add_service(MetricsServiceServer::new(InProcessOtlpReceiver {
                requests: request_sender,
            }))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = shutdown_receiver.await;
            }),
    );
    tokio::task::yield_now().await;

    let exporter = temp_env::with_vars(
        [
            (OTEL_EXPORTER_OTLP_ENDPOINT, Some(endpoint.as_str())),
            (OTEL_EXPORTER_OTLP_METRICS_ENDPOINT, None),
            ("OTEL_EXPORTER_ENDPOINT", None),
            (
                OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE,
                Some("delta"),
            ),
        ],
        || build_metrics_exporter(Protocol::Grpc, None).unwrap(),
    );
    assert_eq!(exporter.temporality(), Temporality::Delta);

    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter)
        .build();
    let histogram = meter_provider
        .meter("statsig.forward_proxy")
        .f64_histogram("statsig.forward_proxy.UpdateConfigSpecStorePropagationDelayMs.latency")
        .build();

    histogram.record(7.0, &[KeyValue::new("lcut", "first")]);
    meter_provider.force_flush().unwrap();
    let first_export = receive_export(&mut requests).await;
    let first_histogram = find_histogram(
        &first_export,
        "statsig.forward_proxy.UpdateConfigSpecStorePropagationDelayMs.latency",
    );
    assert_eq!(
        first_histogram.aggregation_temporality,
        AggregationTemporality::Delta as i32
    );
    assert_eq!(first_histogram.data_points.len(), 1);
    assert_eq!(first_histogram.data_points[0].count, 1);
    assert_eq!(first_histogram.data_points[0].sum, Some(7.0));
    assert_eq!(
        string_attribute(&first_histogram.data_points[0], "lcut"),
        Some("first")
    );

    histogram.record(11.0, &[KeyValue::new("lcut", "second")]);
    meter_provider.force_flush().unwrap();
    let second_export = receive_export(&mut requests).await;
    let second_histogram = find_histogram(
        &second_export,
        "statsig.forward_proxy.UpdateConfigSpecStorePropagationDelayMs.latency",
    );
    assert_eq!(
        second_histogram.aggregation_temporality,
        AggregationTemporality::Delta as i32
    );
    assert_eq!(second_histogram.data_points.len(), 1);
    assert_eq!(second_histogram.data_points[0].count, 1);
    assert_eq!(second_histogram.data_points[0].sum, Some(11.0));
    assert_eq!(
        string_attribute(&second_histogram.data_points[0], "lcut"),
        Some("second")
    );
    assert!(second_histogram
        .data_points
        .iter()
        .all(|point| string_attribute(point, "lcut") != Some("first")));

    meter_provider.shutdown().unwrap();
    shutdown_sender.send(()).unwrap();
    server.await.unwrap().unwrap();
}
