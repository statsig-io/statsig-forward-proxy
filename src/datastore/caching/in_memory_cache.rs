use crate::{
    datastore::{
        config_spec_store::ConfigSpecForCompany, data_providers::DataProviderRequestResult,
    },
    observers::{
        proxy_event_observer::ProxyEventObserver, EventStat, HttpDataProviderObserverTrait,
        OperationType, ProxyEvent, ProxyEventType,
    },
};

use lru::LruCache;
use std::{num::NonZeroUsize, sync::Arc};
use tokio::sync::RwLock;

pub struct InMemoryCache {
    data: Arc<RwLock<LruCache<String, ConfigSpecForCompany>>>,
}

impl InMemoryCache {
    pub fn new(maximum_concurrent_sdk_keys: u16) -> Self {
        InMemoryCache {
            data: Arc::new(RwLock::new(LruCache::new(
                NonZeroUsize::new(maximum_concurrent_sdk_keys.into()).unwrap(),
            ))),
        }
    }

    fn get_storage_key(key: &str, path: &str) -> String {
        format!("{}|{}", path, key)
    }
}

use async_trait::async_trait;
#[async_trait]
impl HttpDataProviderObserverTrait for InMemoryCache {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        false
    }

    async fn update(
        &self,
        result: &DataProviderRequestResult,
        key: &str,
        lcut: u64,
        data: &Arc<String>,
        path: &str,
    ) {
        let storage_key = InMemoryCache::get_storage_key(key, path);
        if result == &DataProviderRequestResult::DataAvailable {
            if let Some(record) = self.data.read().await.peek(&storage_key) {
                self.data.write().await.put(
                    storage_key,
                    ConfigSpecForCompany {
                        lcut,
                        config: data.clone(),
                    },
                );

                if lcut > record.lcut {
                    ProxyEventObserver::publish_event(
                        ProxyEvent::new(ProxyEventType::InMemoryCacheWriteSucceed, key.to_string())
                            .with_stat(EventStat {
                                operation_type: OperationType::IncrByValue,
                                value: 1,
                            }),
                    )
                    .await;
                } else {
                    ProxyEventObserver::publish_event(
                        ProxyEvent::new(ProxyEventType::InMemoryCacheWriteSkipped, key.to_string())
                            .with_stat(EventStat {
                                operation_type: OperationType::IncrByValue,
                                value: 1,
                            }),
                    )
                    .await;
                }
            } else {
                ProxyEventObserver::publish_event(
                    ProxyEvent::new(ProxyEventType::InMemoryCacheWriteSucceed, key.to_string())
                        .with_stat(EventStat {
                            operation_type: OperationType::IncrByValue,
                            value: 1,
                        }),
                )
                .await;
            }
        } else if result == &DataProviderRequestResult::Unauthorized {
            let contains_key = self.data.read().await.peek(&storage_key).is_some();
            if contains_key {
                self.data.write().await.pop(&storage_key);
            }
        }
    }

    async fn get(&self, key: &str, path: &str) -> Option<Arc<String>> {
        ProxyEventObserver::publish_event(
            ProxyEvent::new(ProxyEventType::InMemoryCacheReadSucceed, key.to_string())
                .with_path(path.to_string())
                .with_stat(EventStat {
                    operation_type: OperationType::IncrByValue,
                    value: 1,
                }),
        )
        .await;

        self.data
            .read()
            .await
            .peek(&InMemoryCache::get_storage_key(key, path))
            .map(|record| record.config.clone())
    }
}
