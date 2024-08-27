use super::data_providers::DataProviderRequestResult;
use crate::observers::{
    proxy_event_observer::ProxyEventObserver, HttpDataProviderObserverTrait, ProxyEvent,
    ProxyEventType,
};
use crate::observers::{EventStat, OperationType};
use crate::servers::http_server::AuthorizedRequestContext;
use async_trait::async_trait;

use std::collections::hash_map::IntoIter;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
#[async_trait]
impl HttpDataProviderObserverTrait for SdkKeyStore {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        true
    }

    async fn update(
        &self,
        result: &DataProviderRequestResult,
        request_context: &AuthorizedRequestContext,
        lcut: u64,
        _data: &Arc<String>,
    ) {
        if result == &DataProviderRequestResult::DataAvailable {
            let mut write_lock = self.keystore.write().await;
            if !request_context.use_lcut || *write_lock.get(request_context).unwrap_or(&0) < lcut {
                write_lock.insert(request_context.clone(), lcut);
            }
        } else if result == &DataProviderRequestResult::Unauthorized {
            let contains_key = self.keystore.read().await.contains_key(request_context);
            if contains_key {
                self.keystore.write().await.remove(request_context);
            }
        }
    }

    async fn get(&self, _request_context: &AuthorizedRequestContext) -> Option<Arc<String>> {
        // Not used
        None
    }
}

pub struct SdkKeyStore {
    keystore: Arc<RwLock<HashMap<AuthorizedRequestContext, u64>>>,
}

impl Default for SdkKeyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SdkKeyStore {
    pub fn new() -> SdkKeyStore {
        SdkKeyStore {
            keystore: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn has_key(&self, request_context: &AuthorizedRequestContext) -> bool {
        match self.keystore.read().await.contains_key(request_context) {
            true => {
                ProxyEventObserver::publish_event(ProxyEvent::new(
                    ProxyEventType::SdkKeyStoreCacheHit,
                    request_context,
                ))
                .await;
                true
            }
            false => {
                ProxyEventObserver::publish_event(
                    ProxyEvent::new(ProxyEventType::SdkKeyStoreCacheMiss, request_context)
                        .with_stat(EventStat {
                            operation_type: OperationType::IncrByValue,
                            value: 1,
                        }),
                )
                .await;
                false
            }
        }
    }

    pub async fn get_registered_store(&self) -> IntoIter<AuthorizedRequestContext, u64> {
        self.keystore.read().await.clone().into_iter()
    }
}
