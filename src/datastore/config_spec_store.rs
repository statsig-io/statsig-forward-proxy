use super::data_providers::background_data_provider::{foreground_fetch, BackgroundDataProvider};
use super::data_providers::http_data_provider::ResponsePayload;
use super::data_providers::{DataProviderRequestResult, FullRequestContext, ResponseContext};
use super::sdk_key_store::SdkKeyStore;

use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::{
    EventStat, HttpDataProviderObserverTrait, OperationType, ProxyEvent, ProxyEventType,
};
use crate::servers::authorized_request_context::{
    AuthorizedRequestContext, AuthorizedRequestContextCache,
};
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
        request_context: &Arc<FullRequestContext>,
        response_context: &Arc<ResponseContext>,
    ) {
        if response_context.result_type == DataProviderRequestResult::Error
            || response_context.result_type == DataProviderRequestResult::DataAvailable
        {
            if !self
                .store
                .contains_key(&request_context.authorized_request_context)
            {
                let rc = Arc::clone(&request_context.authorized_request_context);
                let new_data = Arc::new(ConfigSpecForCompany {
                    lcut: response_context.lcut,
                    config: response_context.body.clone(),
                });
                self.store.insert(rc, new_data);
                return;
            }

            let stored_lcut = self
                .store
                .get(&request_context.authorized_request_context)
                .map(|record| record.lcut)
                .unwrap_or(0);

            if response_context.lcut > stored_lcut {
                let new_data = Arc::new(ConfigSpecForCompany {
                    lcut: response_context.lcut,
                    config: response_context.body.clone(),
                });
                self.store
                    .entry(request_context.authorized_request_context.clone())
                    .and_modify(|entry| {
                        *entry = new_data.clone();
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new_with_rc(
                                ProxyEventType::UpdateConfigSpecStorePropagationDelayMs,
                                &request_context.authorized_request_context,
                            )
                            .with_lcut(response_context.lcut)
                            .with_stat(EventStat {
                                operation_type: OperationType::Distribution,
                                value: Utc::now().timestamp_millis()
                                    - (response_context.lcut as i64),
                            }),
                        );
                    })
                    .or_insert(new_data);
            }
        } else if response_context.result_type == DataProviderRequestResult::Unauthorized {
            self.store
                .remove(&request_context.authorized_request_context);
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
                use_proto: false,
                encoding: Arc::new(CompressionEncoder::PlainText),
                data: Arc::new(Bytes::from_static(b"{\"has_updates\":false}")),
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

pub fn shadow_fetch_json_config_spec(
    authorized_request_context_cache: Arc<AuthorizedRequestContextCache>,
    rc: &Arc<AuthorizedRequestContext>,
    config_spec_store: &Arc<ConfigSpecStore>,
    since_time: u64,
) {
    let spec_store_clone: Arc<ConfigSpecStore> = Arc::clone(config_spec_store);
    let rc_clone = Arc::clone(rc);
    tokio::spawn(async move {
        let shadow_request_context = authorized_request_context_cache.get_or_insert(
            rc_clone.sdk_key.clone(),
            rc_clone.path.clone(),
            vec![CompressionEncoder::Gzip], // decompression is handled before committing to DataStore
            false,                          // Fetch json format only
        );

        let _ = spec_store_clone
            .get_config_spec(&shadow_request_context, since_time)
            .await;
    });
}
