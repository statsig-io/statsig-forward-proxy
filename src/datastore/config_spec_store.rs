use super::data_providers::background_data_provider::{foreground_fetch, BackgroundDataProvider};
use super::data_providers::http_data_provider::ResponsePayload;
use super::data_providers::DataProviderRequestResult;
use super::sdk_key_store::SdkKeyStore;

use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::{
    EventStat, HttpDataProviderObserverTrait, OperationType, ProxyEvent, ProxyEventType,
};
use crate::servers::authorized_request_context::AuthorizedRequestContext;
use crate::utils::compress_encoder::CompressionEncoder;
use bytes::Bytes;

use chrono::Utc;

use std::sync::Arc;

use dashmap::DashMap;

#[derive(Clone, Debug)]
pub struct ConfigSpecForCompany {
    pub lcut: u64,
    pub config: Arc<ResponsePayload>,
}

pub struct ConfigSpecStore {
    store: Arc<DashMap<Arc<AuthorizedRequestContext>, Arc<ConfigSpecForCompany>>>,
    sdk_key_store: Arc<SdkKeyStore>,
    background_data_provider: Arc<BackgroundDataProvider>,
    pub no_update_config: Arc<ResponsePayload>,
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
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
        data: &Arc<ResponsePayload>,
    ) {
        if result == &DataProviderRequestResult::Error
            || result == &DataProviderRequestResult::DataAvailable
        {
            if !self.store.contains_key(request_context) {
                let rc = request_context.clone();
                let new_data = Arc::new(ConfigSpecForCompany {
                    lcut,
                    config: data.clone(),
                });
                self.store.insert(rc, new_data);
                return;
            }

            let stored_lcut = self
                .store
                .get(request_context)
                .map(|record| record.lcut)
                .unwrap_or(0);

            if lcut > stored_lcut {
                let new_data = Arc::new(ConfigSpecForCompany {
                    lcut,
                    config: data.clone(),
                });
                self.store
                    .entry(request_context.clone())
                    .and_modify(|entry| {
                        *entry = new_data.clone();
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
                        );
                    })
                    .or_insert(new_data);
            }
        } else if result == &DataProviderRequestResult::Unauthorized {
            self.store.remove(request_context);
        }
    }

    async fn get(
        &self,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<Arc<ConfigSpecForCompany>> {
        self.store.get(request_context).map(|record| record.clone())
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
            no_update_config: Arc::new(ResponsePayload {
                encoding: Arc::new(CompressionEncoder::PlainText),
                data: Arc::new(Bytes::from("{\"has_updates\":false}".to_string())),
            }),
        }
    }

    pub async fn get_config_spec(
        &self,
        request_context: &Arc<AuthorizedRequestContext>,
        since_time: u64,
    ) -> Option<Arc<ConfigSpecForCompany>> {
        if !self.sdk_key_store.has_key(request_context) {
            // Since it's a cache-miss, just fill with a full payload
            // and check if we should return no update manually
            foreground_fetch(
                self.background_data_provider.clone(),
                request_context,
                0,
                // Since it's a cache-miss, it doesn't matter what we do
                // if we receive a 4xx, so no point clearing any
                // caches
                false,
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
                let lcut = record.lcut;
                if lcut > since_time {
                    Some(record)
                } else {
                    Some(Arc::new(ConfigSpecForCompany {
                        lcut,
                        config: Arc::clone(&self.no_update_config),
                    }))
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
