use super::data_providers::background_data_provider::{foreground_fetch, BackgroundDataProvider};
use super::data_providers::DataProviderRequestResult;
use super::sdk_key_store::SdkKeyStore;
use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::EventStat;
use crate::observers::OperationType;
use crate::observers::{HttpDataProviderObserverTrait, ProxyEvent, ProxyEventType};
use std::collections::HashMap;

use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone, Debug)]
pub struct ConfigSpecForCompany {
    pub lcut: u64,
    pub config: Arc<String>,
}

pub struct ConfigSpecStore {
    store: Arc<RwLock<HashMap<String, Arc<RwLock<ConfigSpecForCompany>>>>>,
    sdk_key_store: Arc<SdkKeyStore>,
    background_data_provider: Arc<BackgroundDataProvider>,
    no_update_payload: Arc<String>,
}

use async_trait::async_trait;
#[async_trait]
impl HttpDataProviderObserverTrait for ConfigSpecStore {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        true
    }

    async fn update(
        &self,
        result: &DataProviderRequestResult,
        sdk_key: &str,
        lcut: u64,
        data: &Arc<String>,
    ) {
        let record = self.store.read().await.get(sdk_key).cloned();
        if (result == &DataProviderRequestResult::Error
            || result == &DataProviderRequestResult::DataAvailable)
            && record.is_none()
        {
            self.store.write().await.insert(
                sdk_key.to_owned(),
                Arc::new(RwLock::new(ConfigSpecForCompany {
                    lcut,
                    config: data.clone(),
                })),
            );
        } else if result == &DataProviderRequestResult::DataAvailable {
            ProxyEventObserver::publish_event(
                ProxyEvent::new(ProxyEventType::ConfigSpecStoreGotData, sdk_key.to_string())
                    .with_stat(EventStat {
                        operation_type: OperationType::IncrByValue,
                        value: 1,
                    }),
            )
            .await;
            let stored_lcut = match record {
                Some(record) => record.read().await.lcut,
                None => 0,
            };

            // If LCUT is not newer, then there is nothing to save
            if stored_lcut >= lcut {
                return;
            // Lcut is newer than stored lcut, so update everything
            } else {
                let hm_r_lock = self.store.read().await;
                let mut w_lock = hm_r_lock
                    .get(sdk_key)
                    .expect("Record must exist")
                    .write()
                    .await;
                w_lock.lcut = lcut;
                w_lock.config = data.clone();
            }
        } else if result == &DataProviderRequestResult::Unauthorized && record.is_some() {
            self.store.write().await.remove(sdk_key);
        }
    }

    async fn get(&self, key: &str) -> Option<Arc<String>> {
        match self.store.read().await.get(key) {
            Some(record) => Some(record.read().await.config.clone()),
            None => None,
        }
    }
}

impl ConfigSpecStore {
    pub fn new(
        sdk_key_store: Arc<SdkKeyStore>,
        background_data_provider: Arc<BackgroundDataProvider>,
    ) -> Self {
        ConfigSpecStore {
            store: Arc::new(RwLock::new(HashMap::new())),
            sdk_key_store,
            background_data_provider,
            no_update_payload: Arc::new("{\"has_updates\":false}".to_string()),
        }
    }

    pub async fn get_config_spec(
        &self,
        sdk_key: &str,
        since_time: u64,
    ) -> Option<Arc<RwLock<ConfigSpecForCompany>>> {
        if !self.sdk_key_store.has_key(sdk_key, since_time).await {
            // Since it's a cache-miss, just fill with a full payload
            // and check if we should return no update manually
            foreground_fetch(self.background_data_provider.clone(), sdk_key, 0).await;
        }

        // TODO: Since we use peek as an optimization
        //       make it such that we promote every X number of reads
        //       to ensure we don't evict the sdk key
        //
        // If the payload for sinceTime 0 is greater than since_time
        // then return the full payload.
        let read_lock = self.store.read().await;
        let record = read_lock.get(sdk_key);
        match record {
            Some(record) => {
                if record.read().await.lcut > since_time {
                    Some(Arc::clone(record))
                } else {
                    Some(Arc::new(
                        ConfigSpecForCompany {
                            lcut: since_time,
                            config: self.no_update_payload.clone(),
                        }
                        .into(),
                    ))
                }
            }
            None => {
                // If the store still doesn't have a value, then it either
                // means its a 401 or a 5xx. For now, assume its a 401
                // since we have a backup cache
                // TODO: Harden this code
                None
            }
        }
    }
}
