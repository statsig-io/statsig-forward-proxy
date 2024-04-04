use crate::{
    datastore::{
        config_spec_store::ConfigSpecForCompany, data_providers::DataProviderRequestResult,
    },
    observers::{
        proxy_event_observer::ProxyEventObserver, EventStat, NewDcsObserverTrait, OperationType,
        ProxyEvent, ProxyEventType,
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
}

use async_trait::async_trait;
#[async_trait]
impl NewDcsObserverTrait for InMemoryCache {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        false
    }

    async fn update(
        &self,
        result: &DataProviderRequestResult,
        key: &str,
        lcut: u64,
        data: &Arc<String>,
    ) {
        if result == &DataProviderRequestResult::DataAvailable {
            if let Some(record) = self.data.read().await.peek(key) {
                self.data.write().await.put(
                    key.to_string(),
                    ConfigSpecForCompany {
                        lcut,
                        config: data.clone(),
                    },
                );

                if lcut > record.lcut {
                    ProxyEventObserver::publish_event(
                        ProxyEvent::new(ProxyEventType::InMemoryCacheWriteSucceed, key.to_string())
                            .with_lcut(lcut)
                            .with_stat(EventStat {
                                operation_type: OperationType::IncrByValue,
                                value: 1,
                            }),
                    )
                    .await;
                } else {
                    ProxyEventObserver::publish_event(
                        ProxyEvent::new(ProxyEventType::InMemoryCacheWriteSkipped, key.to_string())
                            .with_lcut(lcut)
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
                        .with_lcut(lcut)
                        .with_stat(EventStat {
                            operation_type: OperationType::IncrByValue,
                            value: 1,
                        }),
                )
                .await;
            }
        }
    }

    async fn get(&self, key: &str) -> Option<Arc<String>> {
        ProxyEventObserver::publish_event(
            ProxyEvent::new(ProxyEventType::InMemoryCacheReadSucceed, key.to_string()).with_stat(
                EventStat {
                    operation_type: OperationType::IncrByValue,
                    value: 1,
                },
            ),
        )
        .await;

        self.data
            .read()
            .await
            .peek(key)
            .map(|record| record.config.clone())
    }
}
