use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::{EventStat, OperationType, ProxyEvent, ProxyEventType};
use crate::GRACEFUL_SHUTDOWN_TOKEN;
use envy;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
pub struct NginxCacheConfig {
    pub proxy_cache_path_configuration: Option<String>,
    pub proxy_cache_download_path_configuration: Option<String>,
    pub proxy_cache_download_id_list_path_configuration: Option<String>,
    pub proxy_cache_max_size_in_mb: u64,
}

pub struct NginxCacheMonitor;

impl NginxCacheMonitor {
    pub async fn start_monitoring() {
        // Load configuration from environment variables
        let config = envy::from_env::<NginxCacheConfig>().expect("Failed to load configuration");

        // Spawn a background task to monitor directory utilization
        tokio::spawn(async move {
            let cache_paths = cache_paths(&config);
            if cache_paths.is_empty() {
                eprintln!("Failed to monitor nginx cache: no cache paths configured");
                return;
            }
            let max_cache_size = max_cache_size_bytes(&config);

            loop {
                match calculate_cache_size(&cache_paths) {
                    Ok(size) => {
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new(ProxyEventType::NginxCacheBytesUsed).with_stat(
                                EventStat {
                                    operation_type: OperationType::Gauge,
                                    value: event_stat_value(size),
                                },
                            ),
                        );
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new(ProxyEventType::NginxCacheBytesLimit).with_stat(
                                EventStat {
                                    operation_type: OperationType::Gauge,
                                    value: event_stat_value(max_cache_size),
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

fn cache_paths(config: &NginxCacheConfig) -> Vec<PathBuf> {
    match (
        &config.proxy_cache_download_path_configuration,
        &config.proxy_cache_download_id_list_path_configuration,
    ) {
        (Some(download_path), Some(id_list_path)) => {
            vec![PathBuf::from(download_path), PathBuf::from(id_list_path)]
        }
        _ => config
            .proxy_cache_path_configuration
            .iter()
            .map(PathBuf::from)
            .collect(),
    }
}

fn max_cache_size_bytes(config: &NginxCacheConfig) -> u64 {
    config
        .proxy_cache_max_size_in_mb
        .saturating_mul(1024)
        .saturating_mul(1024)
        .saturating_mul(cache_paths(config).len() as u64)
}

fn event_stat_value(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn calculate_cache_size(paths: &[PathBuf]) -> std::io::Result<u64> {
    let mut total_size: u64 = 0;

    for path in paths {
        total_size = total_size.saturating_add(calculate_dir_size(path)?);
    }

    Ok(total_size)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;

    fn config(download_path: &Path, id_list_path: &Path, max_size_mb: u64) -> NginxCacheConfig {
        NginxCacheConfig {
            proxy_cache_path_configuration: None,
            proxy_cache_download_path_configuration: Some(download_path.display().to_string()),
            proxy_cache_download_id_list_path_configuration: Some(
                id_list_path.display().to_string(),
            ),
            proxy_cache_max_size_in_mb: max_size_mb,
        }
    }

    #[test]
    fn cache_paths_uses_split_nginx_cache_directories() {
        let config = config(
            Path::new("/cache/download_cache"),
            Path::new("/cache/download_id_list"),
            1024,
        );

        assert_eq!(
            cache_paths(&config),
            vec![
                PathBuf::from("/cache/download_cache"),
                PathBuf::from("/cache/download_id_list")
            ]
        );
    }

    #[test]
    fn cache_paths_falls_back_to_legacy_root_cache_directory() {
        let config = NginxCacheConfig {
            proxy_cache_path_configuration: Some("/cache".to_string()),
            proxy_cache_download_path_configuration: None,
            proxy_cache_download_id_list_path_configuration: None,
            proxy_cache_max_size_in_mb: 1024,
        };

        assert_eq!(cache_paths(&config), vec![PathBuf::from("/cache")]);
    }

    #[test]
    fn max_cache_size_bytes_accounts_for_both_nginx_caches() {
        let config = config(
            Path::new("/cache/download_cache"),
            Path::new("/cache/download_id_list"),
            1024,
        );

        assert_eq!(max_cache_size_bytes(&config), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn calculate_cache_size_sums_both_split_cache_directories() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let download_path = temp_dir.path().join("download_cache");
        let id_list_path = temp_dir.path().join("download_id_list");
        let nested_path = id_list_path.join("nested");
        fs::create_dir_all(&download_path).expect("download cache dir");
        fs::create_dir_all(&nested_path).expect("nested id list cache dir");

        write_file(&download_path.join("a"), 7);
        write_file(&nested_path.join("b"), 11);

        assert_eq!(
            calculate_cache_size(&[download_path, id_list_path]).expect("cache size"),
            18
        );
    }

    #[test]
    fn event_stat_value_saturates_when_u64_exceeds_i64() {
        assert_eq!(event_stat_value(u64::MAX), i64::MAX);
    }

    fn write_file(path: &Path, bytes: usize) {
        let mut file = File::create(path).expect("create file");
        file.write_all(&vec![0; bytes]).expect("write file");
    }
}
