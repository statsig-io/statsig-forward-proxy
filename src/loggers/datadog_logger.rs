use crate::observers::{ProxyEvent, ProxyEventObserverTrait};
use async_trait::async_trait;

use dogstatsd::{BatchingOptions, Client, OptionsBuilder};
use lazy_static::lazy_static;
use serde::Deserialize;
use std::str::FromStr;

use std::time::Instant;
use std::{env, time::Duration};
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::observers::OperationType;

#[derive(Deserialize, Clone)]
pub struct EnvConfig {
    pub datadog_sampling_rate: Option<u64>,
    pub datadog_max_batch_time_ms: Option<u64>,
    pub datadog_max_batch_event_count: Option<usize>,
    pub datadog_max_backoff_ms: Option<u64>,
    pub dogstatsd_max_buffer_size: Option<usize>,
    pub dogstatsd_max_time_ms: Option<u64>,
    pub dogstatsd_max_retry_attempt: Option<usize>,
    pub dogstatsd_initial_retry_delay: Option<u64>,
    pub dogstatsd_client_initialization_retry_delay: Option<u64>,
    pub dogstatsd_client_initialization_max_retry_attempt: Option<usize>,
    pub statsd_socket: Option<String>,
    pub datadog_sender_buffer_size: Option<u64>,
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
    value: Option<String>,
    tags: Vec<String>,
}

pub struct DatadogLogger {
    sender: Sender<Operation>,
}

fn get_hostname() -> String {
    match env::var("HOSTNAME") {
        Ok(pod_id) => pod_id,
        Err(_err) => "localhost".to_string(),
    }
}

impl DatadogLogger {
    pub async fn new() -> Self {
        let config = envy::from_env::<EnvConfig>().expect("Malformed config");
        let (tx, rx): (Sender<Operation>, Receiver<Operation>) = mpsc::channel(
            usize::try_from(config.datadog_sender_buffer_size.unwrap_or(50000)).unwrap(),
        );
        tokio::spawn(async move {
            batching::process_operations(rx).await;
        });

        DatadogLogger { sender: tx }
    }
}

#[async_trait]
impl ProxyEventObserverTrait for DatadogLogger {
    async fn handle_event(&self, event: &ProxyEvent) {
        if let Some(stat) = &event.stat {
            let mut tags = vec![format!("sdk_key:{}", event.sdk_key)];
            if let Some(lcut) = event.lcut {
                tags.push(format!("lcut:{}", lcut));
            }
            if let Some(path) = &event.path {
                tags.push(format!("path:{}", path));
            }
            let suffix = match stat.operation_type {
                OperationType::Distribution => "latency",
                OperationType::Gauge => "gauge",
                OperationType::IncrByValue => "count",
            };
            let res = self.sender.try_send(Operation {
                operation_type: stat.operation_type,
                counter_name: format!("statsig.forward_proxy.{:?}.{}", event.event_type, suffix),
                value: Some(stat.value.to_string()),
                tags,
            });
            if let Some(e) = res.err() {
                println!(
                    "Failed to increment statsig.forward_proxy.{:?}.count: {}",
                    event.event_type, e
                );
            }
        }
    }
}

mod batching {
    use std::{collections::HashMap, sync::Arc};

    use dogstatsd::Options;

    use super::*;

    lazy_static! {
        static ref DATADOG_CLIENT: Arc<Client> = Arc::new(
            Client::new(initialize_datadog_client_options())
                .expect("Need datadog client if enabled")
        );
    }

    fn initialize_datadog_client_options() -> Options {
        let mut builder = OptionsBuilder::new();
        let config = envy::from_env::<EnvConfig>().expect("Malformed config");

        // Use UDS
        builder.socket_path(config.statsd_socket);

        // Use Batching
        let max_buffer_size = config.dogstatsd_max_buffer_size.unwrap_or(8000); // Ideal value is OS based
        let max_time = Duration::from_millis(config.dogstatsd_max_time_ms.unwrap_or(30000));
        let max_retry_attempts = config.dogstatsd_max_retry_attempt.unwrap_or(3);
        let initial_retry_delay = config.dogstatsd_initial_retry_delay.unwrap_or(10);
        builder.batching_options(BatchingOptions {
            max_buffer_size,
            max_time,
            max_retry_attempts,
            initial_retry_delay,
        });

        // Default Tags
        let tags = envy::from_env::<DefaultTagsConfig>().expect("Malformed config");
        [
            tags.dd_version.map(|v| format!("version:{}", v)),
            tags.dd_env.map(|v| format!("env:{}", v)),
            tags.dd_service.map(|v| format!("service:{}", v)),
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

    fn compress_batch(batch: Vec<Operation>) -> Vec<Operation> {
        let mut incr_op_map: HashMap<(String, Vec<String>), i64> = HashMap::new();

        let mut compressed_batch: Vec<Operation> = Vec::new();

        for event in batch {
            match event.operation_type {
                OperationType::Distribution => {
                    compressed_batch.push(event);
                }
                OperationType::Gauge => {
                    compressed_batch.push(event);
                }
                OperationType::IncrByValue => {
                    let incr_amount = i64::from_str(event.value.unwrap().as_str()).unwrap();

                    *incr_op_map
                        .entry((event.counter_name, event.tags))
                        .or_insert(0) += incr_amount;
                }
            }
        }

        for ((counter_name, tags), incr_amount) in incr_op_map.iter() {
            compressed_batch.push(Operation {
                operation_type: OperationType::IncrByValue,
                counter_name: counter_name.clone(),
                value: Some(incr_amount.to_string()),
                tags: tags.clone(),
            });
        }

        compressed_batch
    }

    fn send(event: Operation) -> bool {
        match event.operation_type {
            OperationType::Distribution => {
                match DATADOG_CLIENT.distribution(
                    event.counter_name,
                    event.value.unwrap(),
                    event.tags,
                ) {
                    Ok(()) => true,
                    Err(err) => {
                        eprintln!("Failed to update latency for counter: {})", err);
                        false
                    }
                }
            }
            OperationType::Gauge => {
                match DATADOG_CLIENT.gauge(event.counter_name, event.value.unwrap(), event.tags) {
                    Ok(()) => true,
                    Err(err) => {
                        eprintln!("Failed to gauge for counter: {}", err);
                        false
                    }
                }
            }
            OperationType::IncrByValue => {
                let incr_amount = i64::from_str(event.value.unwrap().as_str()).unwrap();

                match DATADOG_CLIENT.incr_by_value(event.counter_name, incr_amount, event.tags) {
                    Ok(()) => true,
                    Err(err) => {
                        eprintln!("Failed to increment counter: {}", err);
                        false
                    }
                }
            }
        }
    }

    fn flush(batch: Vec<Operation>) {
        tokio::spawn(async move {
            for event in batch {
                send(event);
            }
        });
    }

    pub(crate) async fn process_operations(mut rx: Receiver<Operation>) {
        let config = envy::from_env::<EnvConfig>().expect("Malformed config");
        let mut batch: Vec<Operation> = vec![];
        let mut last_updated = Instant::now();
        let should_flush = |batch_len, last_updated| {
            last_updated + Duration::from_millis(config.datadog_max_batch_time_ms.unwrap_or(10000))
                < Instant::now()
                || config.datadog_max_batch_event_count.unwrap_or(3000) < batch_len
        };

        loop {
            if let Some(operation) = rx.recv().await {
                batch.push(operation);

                if should_flush(batch.len(), last_updated) {
                    batch = compress_batch(batch);
                    flush(batch);
                    last_updated = Instant::now();
                    batch = vec![];
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_compress_batch() {
            // Test case 1: Test with one distribution operation
            let operation1 = Operation {
                operation_type: OperationType::Distribution,
                counter_name: "counter1".to_string(),
                value: Some("100".to_string()),
                tags: vec!["tag1".to_string()],
            };

            let compressed_batch1 = compress_batch(vec![operation1.clone()]);
            assert_eq!(compressed_batch1, vec![operation1]);

            // Test case 2: Test with multiple increment operations with the same counter and tags
            let operation2_1 = Operation {
                operation_type: OperationType::IncrByValue,
                counter_name: "counter2".to_string(),
                value: Some("10".to_string()),
                tags: vec!["tag2".to_string()],
            };
            let operation2_2 = Operation {
                operation_type: OperationType::IncrByValue,
                counter_name: "counter2".to_string(),
                value: Some("20".to_string()),
                tags: vec!["tag2".to_string()],
            };

            let compressed_batch2 =
                compress_batch(vec![operation2_1.clone(), operation2_2.clone()]);
            assert_eq!(
                compressed_batch2,
                vec![Operation {
                    operation_type: OperationType::IncrByValue,
                    counter_name: "counter2".to_string(),
                    value: Some("30".to_string()),
                    tags: vec!["tag2".to_string()],
                }]
            );
        }
    }
}
