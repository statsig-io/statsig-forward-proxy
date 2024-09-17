use super::data_providers::background_data_provider::{foreground_fetch, BackgroundDataProvider};
use super::data_providers::DataProviderRequestResult;
use super::sdk_key_store::SdkKeyStore;

use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::{
    EventStat, HttpDataProviderObserverTrait, OperationType, ProxyEvent, ProxyEventType,
};
use crate::servers::http_server::AuthorizedRequestContext;
use dashmap::DashMap;

use chrono::Utc;
use parking_lot::RwLock;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct ConfigSpecForCompany {
    pub lcut: u64,
    pub config: Arc<String>,
}

pub struct ConfigSpecStore {
    store: Arc<DashMap<AuthorizedRequestContext, Arc<RwLock<ConfigSpecForCompany>>>>,
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
        request_context: &AuthorizedRequestContext,
        lcut: u64,
        data: &Arc<String>,
    ) {
        let should_insert = result == &DataProviderRequestResult::Error
            || (result == &DataProviderRequestResult::DataAvailable
                && !self.store.contains_key(request_context));

        if should_insert {
            let rc = request_context.clone();
            let new_data = Arc::new(RwLock::new(ConfigSpecForCompany {
                lcut,
                config: data.clone(),
            }));
            self.store.insert(rc, new_data);
            return;
        }

        if result == &DataProviderRequestResult::DataAvailable {
            let stored_lcut = self
                .store
                .get(request_context)
                .map(|record| record.read().lcut)
                .unwrap_or(0);

            if lcut > stored_lcut {
                let new_data = ConfigSpecForCompany {
                    lcut,
                    config: data.clone(),
                };
                if let Some(entry) = self.store.get_mut(request_context) {
                    *entry.write() = new_data;
                } else {
                    let rc = request_context.clone();
                    let wrapped_data = Arc::new(RwLock::new(new_data));
                    self.store.insert(rc, wrapped_data);
                }

                ProxyEventObserver::publish_event(
                    ProxyEvent::new_with_rc(
                        ProxyEventType::UpdateConfigSpecStorePropagationDelayMs,
                        request_context,
                    )
                    .with_lcut(lcut)
                    .with_stat(EventStat {
                        operation_type: OperationType::Distribution,
                        value: Utc::now().timestamp_millis() - (lcut as i64),
                    }),
                )
                .await;
            }
        } else if result == &DataProviderRequestResult::Unauthorized {
            self.store.remove(request_context);
        }
    }

    async fn get(&self, request_context: &AuthorizedRequestContext) -> Option<Arc<String>> {
        self.store
            .get(request_context)
            .map(|record| record.read().config.clone())
    }
}

impl ConfigSpecStore {
    pub fn new(
        sdk_key_store: Arc<SdkKeyStore>,
        background_data_provider: Arc<BackgroundDataProvider>,
    ) -> Self {
        ConfigSpecStore {
            store: Arc::new(DashMap::new()),
            sdk_key_store,
            background_data_provider,
            no_update_payload: Arc::new("{\"has_updates\":false}".to_string()),
        }
    }

    pub async fn get_config_spec(
        &self,
        request_context: &AuthorizedRequestContext,
        since_time: u64,
    ) -> Option<Arc<RwLock<ConfigSpecForCompany>>> {
        if !self.sdk_key_store.has_key(request_context).await {
            // Since it's a cache-miss, just fill with a full payload
            // and check if we should return no update manually
            foreground_fetch(
                self.background_data_provider.clone(),
                request_context,
                0,
                self.sdk_key_store.clone(),
            )
            .await;
        }

        // TODO: Since we use peek as an optimization
        //       make it such that we promote every X number of reads
        //       to ensure we don't evict the sdk key
        //
        // If the payload for sinceTime 0 is greater than since_time
        // then return the full payload.
        let record = self.store.get(request_context).map(|r| r.clone());

        match record {
            Some(record) => {
                // Move the read operation outside the lock
                let lcut = record.read().lcut;
                if lcut > since_time {
                    Some(record)
                } else {
                    Some(Arc::new(RwLock::new(ConfigSpecForCompany {
                        lcut: since_time,
                        config: Arc::clone(&self.no_update_payload),
                    })))
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
