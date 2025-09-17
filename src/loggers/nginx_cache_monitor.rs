use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::{EventStat, OperationType, ProxyEvent, ProxyEventType};
use crate::GRACEFUL_SHUTDOWN_TOKEN;
use envy;
use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Deserialize)]
pub struct NginxCacheConfig {
    pub proxy_cache_path_configuration: String,
    pub proxy_cache_max_size_in_mb: u64,
}

pub struct NginxCacheMonitor;

impl NginxCacheMonitor {
    pub async fn start_monitoring() {
        // Load configuration from environment variables
        let config = envy::from_env::<NginxCacheConfig>().expect("Failed to load configuration");

        // Spawn a background task to monitor directory utilization
        tokio::spawn(async move {
            let cache_path = Path::new(&config.proxy_cache_path_configuration);
            let max_cache_size = config.proxy_cache_max_size_in_mb * 1024 * 1024; // Convert MB to bytes

            loop {
                match calculate_dir_size(cache_path) {
                    Ok(size) => {
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new(ProxyEventType::NginxCacheBytesUsed).with_stat(
                                EventStat {
                                    operation_type: OperationType::Gauge,
                                    value: size as i64,
                                },
                            ),
                        );
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new(ProxyEventType::NginxCacheBytesLimit).with_stat(
                                EventStat {
                                    operation_type: OperationType::Gauge,
                                    value: max_cache_size as i64,
                                },
                            ),
                        );
                    }
                    Err(e) => {
                        eprintln!("Failed to calculate directory size: {e}");
                    }
                }

                // Sleep for a while before checking again
                if tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => { false },
                    _ = GRACEFUL_SHUTDOWN_TOKEN.cancelled() => {
                        true
                    },
                } {
                    break;
                }
            }
        });
    }
}

/// Calculate the total size of a directory recursively
fn calculate_dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total_size = 0;

    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                total_size += calculate_dir_size(&entry.path())?;
            } else {
                total_size += metadata.len();
            }
        }
    }

    Ok(total_size)
}
