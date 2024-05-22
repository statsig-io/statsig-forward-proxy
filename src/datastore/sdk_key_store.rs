use super::data_providers::DataProviderRequestResult;
use crate::observers::{
    proxy_event_observer::ProxyEventObserver, HttpDataProviderObserverTrait, ProxyEvent,
    ProxyEventType,
};
use crate::observers::{EventStat, OperationType};
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
        sdk_key: &str,
        lcut: u64,
        _data: &Arc<String>,
        path: &str,
    ) {
        if result == &DataProviderRequestResult::DataAvailable {
            let mut write_lock = self.keystore.write().await;
            if path.eq("/v1/get_id_lists")
                || (path.eq("/v1/download_config_specs")
                    && *write_lock.get(sdk_key).unwrap_or(&0) < lcut)
            {
                write_lock.insert(sdk_key.to_string(), lcut);
            }
        } else if result == &DataProviderRequestResult::Unauthorized {
            let contains_key = self.keystore.read().await.contains_key(sdk_key);
            if contains_key {
                self.keystore.write().await.remove(sdk_key);
            }
        }
    }

    async fn get(&self, _key: &str, _path: &str) -> Option<Arc<String>> {
        // Not used
        None
    }
}

pub struct SdkKeyStore {
    path: String,
    keystore: Arc<RwLock<HashMap<String, u64>>>,
}

impl SdkKeyStore {
    pub fn new(path: String) -> SdkKeyStore {
        SdkKeyStore {
            path,
            keystore: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn has_key(&self, key: &str, _since_time: u64) -> bool {
        match self.keystore.read().await.contains_key(key) {
            true => {
                ProxyEventObserver::publish_event(
                    ProxyEvent::new(ProxyEventType::SdkKeyStoreCacheHit, key.to_string())
                        .with_path(self.path.clone()),
                )
                .await;
                true
            }
            false => {
                ProxyEventObserver::publish_event(
                    ProxyEvent::new(ProxyEventType::SdkKeyStoreCacheMiss, key.to_string())
                        .with_path(self.path.clone())
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

    pub async fn get_registered_store(&self) -> IntoIter<std::string::String, u64> {
        self.keystore.read().await.clone().into_iter()
    }
}
