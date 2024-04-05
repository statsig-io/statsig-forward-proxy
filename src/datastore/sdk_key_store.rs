use super::data_providers::DataProviderRequestResult;
use crate::observers::EventStat;
use crate::observers::OperationType;
use crate::observers::{
    proxy_event_observer::ProxyEventObserver, NewDcsObserverTrait, ProxyEvent, ProxyEventType,
};
use async_trait::async_trait;
use std::collections::hash_map::IntoIter;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
#[async_trait]
impl NewDcsObserverTrait for SdkKeyStore {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        true
    }

    async fn update(
        &self,
        result: &DataProviderRequestResult,
        sdk_key: &str,
        lcut: u64,
        _data: &Arc<String>,
    ) {
        if result == &DataProviderRequestResult::DataAvailable
            || result == &DataProviderRequestResult::NoDataAvailable
        {
            let mut write_lock = self.keystore.write().await;
            if *write_lock.get(sdk_key).unwrap_or(&0) < lcut {
                write_lock.insert(sdk_key.to_string(), lcut);
            }
        }
    }

    async fn get(&self, _key: &str) -> Option<Arc<String>> {
        // Not used
        None
    }
}

pub struct SdkKeyStore {
    keystore: Arc<RwLock<HashMap<String, u64>>>,
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

    pub async fn has_key(&self, key: &str, since_time: u64) -> bool {
        match self.keystore.read().await.contains_key(key) {
            true => {
                ProxyEventObserver::publish_event(
                    ProxyEvent::new(ProxyEventType::SdkKeyStoreCacheHit, key.to_string())
                        .with_lcut(since_time),
                )
                .await;
                true
            }
            false => {
                ProxyEventObserver::publish_event(
                    ProxyEvent::new(ProxyEventType::SdkKeyStoreCacheMiss, key.to_string())
                        .with_lcut(since_time),
                )
                .await;
                false
            }
        }
    }

    pub async fn get_registered_store(&self) -> IntoIter<std::string::String, u64> {
        self.keystore.read().await.clone().into_iter()
    }
}
