use crate::observers::{ProxyEvent, ProxyEventObserverTrait};
use crate::GRACEFUL_SHUTDOWN_TOKEN;
use async_trait::async_trait;

use dogstatsd::{BatchingOptions, Client, OptionsBuilder};
use lazy_static::lazy_static;
use memchr::memchr;
use parking_lot::{Mutex, RwLock};
use serde::Deserialize;
use tokio::runtime::Handle;

use statsig::Statsig;
use std::sync::Arc;
use std::{env, time::Duration};
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::Instant;

use crate::observers::OperationType;
use fxhash::FxHashMap;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};
use opentelemetry::{global, KeyValue};
use opentelemetry_otlp::{Protocol, WithExportConfig};

use smallvec::SmallVec;

#[derive(Deserialize, Clone)]
pub struct EnvConfig {
    pub datadog_max_batch_time_ms: Option<u64>,
    pub datadog_max_batch_event_count: Option<usize>,
    pub dogstatsd_max_buffer_size: Option<usize>,
    pub dogstatsd_max_time_ms: Option<u64>,
    pub dogstatsd_max_retry_attempt: Option<usize>,
    pub dogstatsd_initial_retry_delay: Option<u64>,
    pub statsd_host_override: Option<String>,
    pub statsd_port_override: Option<String>,
    pub statsd_socket: Option<String>,
    pub datadog_sender_buffer_size: Option<u64>,

    pub otel_exporter_endpoint: Option<String>,
}

lazy_static! {
    static ref CONFIG: EnvConfig = envy::from_env().expect("Malformed config");
    static ref TAG_CACHE: Mutex<FxHashMap<ProxyEvent, (Arc<SmallVec<[String; 4]>>, Instant)>> =
        Mutex::new(FxHashMap::default());
}

/**
 * Default env configuration for tags found here:
 * https://docs.datadoghq.com/getting_started/tagging/unified_service_tagging
 */
#[derive(Deserialize, Clone)]
pub struct DefaultTagsConfig {
    pub dd_env: Option<String>,
    pub dd_version: Option<String>,
    pub dd_service: Option<String>,
}

#[derive(Clone, PartialEq, Debug)]
struct Operation {
    operation_type: OperationType,
    counter_name: String,
    value: i64,
    tags: Arc<SmallVec<[String; 4]>>,
}

pub struct StatsLogger {
    sender: Sender<Operation>,
    enable_datadog_logging: bool,
}

fn get_hostname() -> String {
    match env::var("HOSTNAME") {
        Ok(pod_id) => pod_id,
        Err(_err) => "localhost".to_string(),
    }
}

impl StatsLogger {
    pub async fn new(
        enable_statsd_logging: bool,
        enable_datadog_logging: bool,
        enable_statsig_logging: bool,
        enable_otlp_logging: bool,
    ) -> Self {
        let (tx, rx): (Sender<Operation>, Receiver<Operation>) = mpsc::channel(
            usize::try_from(CONFIG.datadog_sender_buffer_size.unwrap_or(50000)).unwrap(),
        );
        rocket::tokio::task::spawn_blocking(move || {
            Handle::current().block_on(async move {
                batching::process_operations(
                    rx,
                    enable_statsd_logging || enable_datadog_logging,
                    enable_statsig_logging,
                    enable_otlp_logging,
                )
                .await;
            });
        });

        let graceful_shutdown_token = GRACEFUL_SHUTDOWN_TOKEN.clone();
        rocket::tokio::task::spawn_blocking(move || {
            Handle::current().block_on(async move {
                loop {
                    if tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(60)) => { false },
                        _ = graceful_shutdown_token.cancelled() => {
                            true
                        },
                    } {
                        break;
                    }

                    TAG_CACHE
                        .lock()
                        .retain(|_, (_, last_used)| last_used.elapsed() > Duration::from_secs(60));
                }
            });
        });

        StatsLogger {
            sender: tx,
            enable_datadog_logging,
        }
    }

    fn get_or_create_tags(event: &ProxyEvent) -> Arc<SmallVec<[String; 4]>> {
        TAG_CACHE
            .lock()
            .entry(event.clone())
            .or_insert_with(|| {
                let mut tags = SmallVec::new();
                if let Some(sdk_key) = &event.get_sdk_key() {
                    tags.push(sdk_key_tag(sdk_key));
                }
                if let Some(path) = &event.get_path() {
                    tags.push(path_tag(path));
                }
                if let Some(lcut) = event.lcut {
                    tags.push(lcut_tag(lcut));
                }
                if let Some(code) = event.status_code {
                    tags.push(status_code_tag(code));
                }
                if let Some(encodings) = event.get_accept_encodings() {
                    tags.push(format_tag("accept-encoding", &encodings));
                }
                if let Some(enc) = &event.response_encoding {
                    tags.push(format_tag("content-encoding", enc.as_str()));
                }
                (Arc::new(tags), Instant::now())
            })
            .0
            .clone()
    }
}

#[async_trait]
impl ProxyEventObserverTrait for StatsLogger {
    async fn handle_event(&self, event: &ProxyEvent) {
        if let Some(stat) = &event.stat {
            let tags = StatsLogger::get_or_create_tags(event);

            let suffix = operation_suffix(&stat.operation_type);
            let operation_type =
                determine_operation_type(&stat.operation_type, self.enable_datadog_logging);

            if let Err(e) = self
                .sender
                .send(Operation {
                    operation_type,
                    counter_name: create_counter_name(&event.event_type.to_string(), suffix),
                    value: stat.value,
                    tags,
                })
                .await
            {
                eprintln!(
                    "Failed to send statsig.forward_proxy.{:?}.{}: {}",
                    event.event_type, suffix, e
                );
            }
        }
    }
}

// Helper functions to replace format! calls
fn sdk_key_tag(sdk_key: &str) -> String {
    let mut s = String::with_capacity(8 + sdk_key.len());
    s.push_str("sdk_key:");
    s.push_str(sdk_key);
    s
}

fn path_tag(path: &str) -> String {
    let mut s = String::with_capacity(5 + path.len());
    s.push_str("path:");
    s.push_str(path);
    s
}

fn lcut_tag(lcut: u64) -> String {
    let mut s = String::with_capacity(16);
    s.push_str("lcut:");
    s.push_str(&lcut.to_string());
    s
}

fn status_code_tag(code: u16) -> String {
    let mut s = String::with_capacity(16);
    s.push_str("status_code:");
    s.push_str(&code.to_string());
    s
}

fn format_tag(tag: &str, value: &str) -> String {
    let mut s = String::with_capacity(16);
    s.push_str(tag);
    s.push(':');
    s.push_str(value);
    s
}

fn operation_suffix(operation_type: &OperationType) -> &'static str {
    match operation_type {
        OperationType::Distribution | OperationType::Timing => "latency",
        OperationType::Gauge => "gauge",
        OperationType::IncrByValue => "count",
    }
}

fn determine_operation_type(
    stat_operation_type: &OperationType,
    enable_datadog_logging: bool,
) -> OperationType {
    match stat_operation_type {
        OperationType::Distribution | OperationType::Timing if enable_datadog_logging => {
            OperationType::Distribution
        }
        OperationType::Distribution | OperationType::Timing => OperationType::Timing,
        _ => *stat_operation_type,
    }
}

fn create_counter_name(event_type: &str, suffix: &str) -> String {
    let mut s = String::with_capacity(26 + event_type.len() + suffix.len());
    s.push_str("statsig.forward_proxy.");
    s.push_str(event_type);
    s.push('.');
    s.push_str(suffix);
    s
}

mod batching {

    use std::sync::Arc;

    use cached::proc_macro::once;
    use dogstatsd::Options;
    use statsig::StatsigEvent;

    use crate::utils;

    use super::*;

    fn send(
        event: Operation,
        write_to_statsd: bool,
        write_to_statsig: bool,
        write_to_otlp: bool,
    ) -> bool {
        if write_to_statsig {
            send_to_statsig(&event);
        }

        if write_to_statsd {
            send_to_statsd(&event);
        }
        if write_to_otlp {
            send_to_otlp(&event);
        }
        true
    }

    fn send_to_statsig(event: &Operation) {
        let metadata = event
            .tags
            .iter()
            .filter_map(|s| parse_key_value(s))
            .collect();

        Statsig::log_event(
            &utils::statsig_sdk_wrapper::STATSIG_USER_FACTORY.get(),
            StatsigEvent {
                event_name: event.counter_name.clone(),
                value: Some(event.value.into()),
                metadata: Some(metadata),
            },
        );
    }

    fn send_to_otlp(event: &Operation) -> bool {
        match get_otlp() {
            Some(otlp) => {
                match event.operation_type {
                    OperationType::Distribution => otlp.send_histogram_event(event),
                    OperationType::Timing => otlp.send_histogram_event(event),
                    OperationType::Gauge => otlp.send_gauge_event(event),
                    OperationType::IncrByValue => otlp.send_count_event(event),
                };
                true // TODO(xin): Not sure what's the implication here
            }
            None => false,
        }
    }

    pub fn parse_key_value(s: &str) -> Option<(String, serde_json::Value)> {
        if let Some(idx) = memchr(b':', s.as_bytes()) {
            let (key, value) = s.split_at(idx);
            Some((
                key.to_string(),
                serde_json::Value::String(value[1..].to_string()),
            ))
        } else {
            None
        }
    }

    async fn flush(
        batch: Vec<Operation>,
        incr_op_map: FxHashMap<(String, Arc<SmallVec<[String; 4]>>), i64>,
        write_to_statsd: bool,
        write_to_statsig: bool,
        write_to_otlp: bool,
    ) {
        for ((counter_name, tags), incr_amount) in incr_op_map {
            send(
                Operation {
                    operation_type: OperationType::IncrByValue,
                    counter_name,
                    value: incr_amount,
                    tags,
                },
                write_to_statsd,
                write_to_statsig,
                write_to_otlp,
            );
        }

        for event in batch {
            send(event, write_to_statsd, write_to_statsig, write_to_otlp);
        }
    }

    pub(crate) async fn process_operations(
        mut rx: Receiver<Operation>,
        write_to_statsd: bool,
        write_to_statsig: bool,
        write_to_otlp: bool,
    ) {
        let mut batch = Vec::with_capacity(CONFIG.datadog_max_batch_event_count.unwrap_or(3000));
        let mut incr_op_map: FxHashMap<(String, Arc<SmallVec<[String; 4]>>), i64> =
            FxHashMap::default();
        let mut last_updated = Instant::now();
        let max_batch_time =
            Duration::from_millis(CONFIG.datadog_max_batch_time_ms.unwrap_or(10000));
        let max_batch_size = CONFIG.datadog_max_batch_event_count.unwrap_or(3000);
        let graceful_shutdown_token = GRACEFUL_SHUTDOWN_TOKEN.clone();

        loop {
            tokio::select! {
                Some(operation) = rx.recv() => {
                    match operation.operation_type {
                        OperationType::IncrByValue => {
                            *incr_op_map
                                .entry((operation.counter_name.clone(), operation.tags.clone()))
                                .or_insert(0) += operation.value;
                        }
                        _ => batch.push(operation),
                    }

                    if batch.len() + incr_op_map.len() >= max_batch_size
                        || last_updated.elapsed() >= max_batch_time
                    {
                        let final_batch = std::mem::take(&mut batch);
                        let final_incr_op_map = std::mem::take(&mut incr_op_map);
                        tokio::spawn(flush(
                            final_batch,
                            final_incr_op_map,
                            write_to_statsd,
                            write_to_statsig,
                            write_to_otlp
                        ));
                        last_updated = Instant::now();
                    }
                },
                _ = graceful_shutdown_token.cancelled() => break,
            }
        }
    }

    // ------------------------------
    // StatsD integrations
    // ------------------------------
    #[once(option = true)]
    fn get_or_initialize_statsd_client() -> Option<Arc<Client>> {
        match Client::new(initialize_datadog_client_options()) {
            Ok(client) => Some(Arc::new(client)),
            Err(e) => {
                eprintln!("Failed to initialize statsd client: {e}");
                None
            }
        }
    }

    fn initialize_datadog_client_options() -> Options {
        let mut builder = OptionsBuilder::new();

        // By default, the destination UDP address is 127.0.0.1:8125
        // However, allow users to override if they want
        if CONFIG.statsd_host_override.is_some() && CONFIG.statsd_port_override.is_some() {
            builder.to_addr(format!(
                "{}:{}",
                CONFIG
                    .statsd_host_override
                    .as_ref()
                    .expect("validated existence"),
                CONFIG
                    .statsd_port_override
                    .as_ref()
                    .expect("validated existence")
            ));
        }
        // Use UDS if socket is specified
        builder.socket_path(CONFIG.statsd_socket.clone());

        // Use Batching
        let max_buffer_size = CONFIG.dogstatsd_max_buffer_size.unwrap_or(8000); // Ideal value is OS based
        let max_time = Duration::from_millis(CONFIG.dogstatsd_max_time_ms.unwrap_or(30000));
        let max_retry_attempts = CONFIG.dogstatsd_max_retry_attempt.unwrap_or(3);
        let initial_retry_delay = CONFIG.dogstatsd_initial_retry_delay.unwrap_or(10);
        builder.batching_options(BatchingOptions {
            max_buffer_size,
            max_time,
            max_retry_attempts,
            initial_retry_delay,
        });

        // Default Tags
        let tags = envy::from_env::<DefaultTagsConfig>().expect("Malformed config");
        [
            tags.dd_version.map(|v| format!("version:{v}")),
            tags.dd_env.map(|v| format!("env:{v}")),
            tags.dd_service.map(|v| format!("service:{v}")),
            Some(format!("pod_name:{}", get_hostname())),
        ]
        .iter()
        .for_each(|tag| {
            if tag.is_some() {
                builder.default_tag(tag.to_owned().unwrap());
            }
        });

        builder.build()
    }

    fn send_to_statsd(event: &Operation) -> bool {
        match event.operation_type {
            OperationType::Timing => send_timing(event),
            OperationType::Distribution => send_distribution(event),
            OperationType::Gauge => send_gauge(event),
            OperationType::IncrByValue => send_incr_by_value(event),
        }
    }

    fn send_timing(event: &Operation) -> bool {
        if let Some(statsd_client) = get_or_initialize_statsd_client() {
            match statsd_client.timing(&event.counter_name, event.value, event.tags.as_ref()) {
                Ok(()) => return true,
                Err(err) => {
                    eprintln!("Failed to update timing for counter: {err}");
                }
            }
        }
        false
    }

    fn send_distribution(event: &Operation) -> bool {
        if let Some(statsd_client) = get_or_initialize_statsd_client() {
            match statsd_client.distribution(
                &event.counter_name,
                event.value.to_string(),
                event.tags.as_ref(),
            ) {
                Ok(()) => return true,
                Err(err) => {
                    eprintln!("Failed to update latency for counter: {err}");
                }
            }
        }
        false
    }

    fn send_gauge(event: &Operation) -> bool {
        if let Some(statsd_client) = get_or_initialize_statsd_client() {
            match statsd_client.gauge(
                &event.counter_name,
                event.value.to_string(),
                event.tags.as_ref(),
            ) {
                Ok(()) => return true,
                Err(err) => {
                    eprintln!("Failed to update gauge for counter: {err}");
                }
            }
        }
        false
    }

    fn send_incr_by_value(event: &Operation) -> bool {
        if let Some(statsd_client) = get_or_initialize_statsd_client() {
            match statsd_client.incr_by_value(&event.counter_name, event.value, event.tags.as_ref())
            {
                Ok(()) => return true,
                Err(err) => {
                    eprintln!("Failed to update increment for counter: {err}");
                }
            }
        }
        false
    }

    /*
    OTLP Integration
     */
    #[once(option = true)]
    fn get_otlp() -> Option<Arc<OTLPLogger>> {
        OTLPLogger::new().map(Arc::new)
    }
}

struct OTLPLogger {
    pub meter: Arc<Meter>,
    pub counter_map: RwLock<FxHashMap<String, Counter<f64>>>,
    pub gauge_map: RwLock<FxHashMap<String, Gauge<f64>>>,
    pub histogram_map: RwLock<FxHashMap<String, Histogram<f64>>>,
}

impl OTLPLogger {
    pub fn new() -> Option<Self> {
        let mut exporter_builder = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpJson);
        if let Some(e) = &CONFIG.otel_exporter_endpoint {
            exporter_builder = exporter_builder.with_endpoint(e);
        }
        exporter_builder
            .build()
            .ok()
            .map(|e| {
                let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
                    .with_periodic_exporter(e)
                    .build();
                global::set_meter_provider(meter_provider.clone());
                Arc::new(global::meter("statsig.forward_proxy"))
            })
            .map(|meter| Self {
                meter,
                counter_map: RwLock::new(FxHashMap::default()),
                gauge_map: RwLock::new(FxHashMap::default()),
                histogram_map: RwLock::new(FxHashMap::default()),
            })
    }

    fn send_histogram_event(&self, event: &Operation) {
        let mut histogram_map = self.histogram_map.write();
        let tags = Self::convert_tags(event.tags.clone());
        match histogram_map.get(&event.counter_name) {
            Some(h) => {
                h.record(event.value as f64, &tags);
            }
            None => {
                let h = self
                    .meter
                    .f64_histogram(event.counter_name.to_string())
                    .build();
                h.record(event.value as f64, &tags); // TODO conversion
                histogram_map.insert(event.counter_name.to_string(), h);
            }
        };
    }

    fn send_count_event(&self, event: &Operation) {
        let mut counter_map = self.counter_map.write();
        let tags = Self::convert_tags(event.tags.clone());
        match counter_map.get(&event.counter_name) {
            Some(c) => {
                c.add(event.value as f64, &tags); // TODO conversion
            }
            None => {
                let c = self
                    .meter
                    .f64_counter(event.counter_name.to_string())
                    .build();
                c.add(event.value as f64, &tags); // TODO conversion
                counter_map.insert(event.counter_name.to_string(), c);
            }
        };
    }

    fn send_gauge_event(&self, event: &Operation) {
        let mut gauge_map = self.gauge_map.write();
        let tags = Self::convert_tags(event.tags.clone());
        match gauge_map.get(&event.counter_name) {
            Some(g) => {
                g.record(event.value as f64, &tags); // TODO conversion
            }
            None => {
                let g = self.meter.f64_gauge(event.counter_name.to_string()).build();
                g.record(event.value as f64, &tags); // TODO conversion
                gauge_map.insert(event.counter_name.to_string(), g);
            }
        };
    }

    fn convert_tags(tags: Arc<SmallVec<[String; 4]>>) -> Vec<KeyValue> {
        tags.iter()
            .filter_map(|t| {
                if let Some(idx) = memchr(b':', t.as_bytes()) {
                    let (key, value) = t.split_at(idx);
                    Some(KeyValue::new(key.to_string(), value.to_string()))
                } else {
                    None
                }
            })
            .collect()
    }
}
