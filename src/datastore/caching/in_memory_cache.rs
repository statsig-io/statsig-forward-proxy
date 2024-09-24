use crate::{
    datastore::{
        config_spec_store::ConfigSpecForCompany, data_providers::DataProviderRequestResult,
    },
    observers::{
        proxy_event_observer::ProxyEventObserver, EventStat, HttpDataProviderObserverTrait,
        OperationType, ProxyEvent, ProxyEventType,
    },
    servers::authorized_request_context::AuthorizedRequestContext,
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
impl HttpDataProviderObserverTrait for InMemoryCache {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        false
    }

    async fn update(
        &self,
        result: &DataProviderRequestResult,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
        data: &Arc<str>,
    ) {
        let storage_key = request_context.to_string();
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
                        ProxyEvent::new_with_rc(
                            ProxyEventType::InMemoryCacheWriteSucceed,
                            request_context,
                        )
                        .with_stat(EventStat {
                            operation_type: OperationType::IncrByValue,
                            value: 1,
                        }),
                    );
                } else {
                    ProxyEventObserver::publish_event(
                        ProxyEvent::new_with_rc(
                            ProxyEventType::InMemoryCacheWriteSkipped,
                            request_context,
                        )
                        .with_stat(EventStat {
                            operation_type: OperationType::IncrByValue,
                            value: 1,
                        }),
                    );
                }
            } else {
                ProxyEventObserver::publish_event(
                    ProxyEvent::new_with_rc(
                        ProxyEventType::InMemoryCacheWriteSucceed,
                        request_context,
                    )
                    .with_stat(EventStat {
                        operation_type: OperationType::IncrByValue,
                        value: 1,
                    }),
                );
            }
        } else if result == &DataProviderRequestResult::Unauthorized {
            let contains_key = self.data.read().await.peek(&storage_key).is_some();
            if contains_key {
                self.data.write().await.pop(&storage_key);
            }
        }
    }

    async fn get(&self, request_context: &Arc<AuthorizedRequestContext>) -> Option<Arc<str>> {
        ProxyEventObserver::publish_event(
            ProxyEvent::new_with_rc(ProxyEventType::InMemoryCacheReadSucceed, request_context)
                .with_stat(EventStat {
                    operation_type: OperationType::IncrByValue,
                    value: 1,
                }),
        );

        self.data
            .read()
            .await
            .peek(&request_context.to_string())
            .map(|record| record.config.clone())
    }
}
