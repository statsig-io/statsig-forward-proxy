use super::config_spec_store::{ConfigSpecForCompany, ConfigSpecStore};
use super::data_providers::background_data_provider::{foreground_fetch, BackgroundDataProvider};
use super::data_providers::http_data_provider::ResponsePayload;
use super::data_providers::{DataProviderRequestResult, FullRequestContext, ResponseContext};

use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::{
    EventStat, HttpDataProviderObserverTrait, OperationType, ProxyEvent, ProxyEventType,
};
use crate::servers::authorized_request_context::{
    AuthorizedRequestContext, AuthorizedRequestContextCache,
};
use crate::servers::normalized_path::NormalizedPath;
use crate::utils::compress_encoder::CompressionEncoder;
use bytes::Bytes;

use chrono::Utc;
use lru::LruCache;
use parking_lot::RwLock;

use std::num::NonZeroUsize;
use std::sync::Arc;

use dashmap::DashMap;

type CompressionDictToResponseMap =
    Arc<RwLock<LruCache<Option<Arc<str>>, Arc<ConfigSpecForCompany>>>>;
pub struct SharedDictConfigSpecStore {
    store: Arc<DashMap<Arc<AuthorizedRequestContext>, CompressionDictToResponseMap>>,
    per_key_cache_size: NonZeroUsize,
    background_data_provider: Arc<BackgroundDataProvider>,
    pub no_update_config: Arc<ResponsePayload>,
}

use async_trait::async_trait;
#[async_trait]
impl HttpDataProviderObserverTrait for SharedDictConfigSpecStore {
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
            let per_dict_id_cache = self
                .store
                .entry(Arc::clone(&request_context.authorized_request_context))
                .or_insert_with(|| Arc::new(RwLock::new(LruCache::new(self.per_key_cache_size))))
                .downgrade()
                .clone();
            if !per_dict_id_cache
                .read()
                .contains(&request_context.zstd_dict_id)
            {
                per_dict_id_cache.write().push(
                    request_context.zstd_dict_id.clone(),
                    Arc::new(ConfigSpecForCompany {
                        lcut: response_context.lcut,
                        zstd_dict_id: response_context.zstd_dict_id.clone(),
                        config: response_context.body.clone(),
                    }),
                );
                return;
            }

            let stored_lcut = per_dict_id_cache
                .read()
                .peek(&request_context.zstd_dict_id)
                .map(|config_spec| config_spec.lcut)
                .unwrap_or(0);

            let mut per_dict_id_cache = per_dict_id_cache.write();
            if response_context.lcut > stored_lcut {
                per_dict_id_cache.push(
                    request_context.zstd_dict_id.clone(),
                    Arc::new(ConfigSpecForCompany {
                        lcut: response_context.lcut,
                        zstd_dict_id: response_context.zstd_dict_id.clone(),
                        config: response_context.body.clone(),
                    }),
                );

                // Release the lock before doing anything else, since we don't need it anymore
                drop(per_dict_id_cache);

                ProxyEventObserver::publish_event(
                    ProxyEvent::new_with_rc(
                        ProxyEventType::UpdateConfigSpecStorePropagationDelayMs,
                        &request_context.authorized_request_context,
                    )
                    .with_lcut(response_context.lcut)
                    .with_stat(EventStat {
                        operation_type: OperationType::Distribution,
                        value: Utc::now().timestamp_millis() - (response_context.lcut as i64),
                    }),
                );
            }
        } else if response_context.result_type == DataProviderRequestResult::Unauthorized {
            self.store
                .remove(&request_context.authorized_request_context);
        }
    }

    async fn get(
        &self,
        request_context: &Arc<AuthorizedRequestContext>,
        zstd_dict_id: &Option<Arc<str>>,
    ) -> Option<Arc<ConfigSpecForCompany>> {
        self.store
            .get(request_context)
            .and_then(|per_dict_id_cache| per_dict_id_cache.read().peek(zstd_dict_id).cloned())
    }
}

impl SharedDictConfigSpecStore {
    pub fn new(
        background_data_provider: Arc<BackgroundDataProvider>,
        per_key_cache_size: usize,
    ) -> Self {
        SharedDictConfigSpecStore {
            store: Arc::new(DashMap::new()),
            per_key_cache_size: NonZeroUsize::new(per_key_cache_size)
                .expect("per_key_cache_size must be non-zero"),
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
        zstd_dict_id: &Option<Arc<str>>,
    ) -> Option<Arc<ConfigSpecForCompany>> {
        let actual_dict_id = match zstd_dict_id {
            Some(id) if id.as_ref() == "null" => None,
            _ => zstd_dict_id.clone(),
        };

        let per_dict_id_cache = self
            .store
            .entry(Arc::clone(request_context))
            .or_insert_with(|| Arc::new(RwLock::new(LruCache::new(self.per_key_cache_size))))
            .downgrade()
            .clone();
        if !per_dict_id_cache.read().contains(&actual_dict_id) {
            // Since it's a cache-miss, just fill with a full payload
            // and check if we should return no update manually
            foreground_fetch(
                self.background_data_provider.clone(),
                request_context,
                0,
                &actual_dict_id,
                // Since it's a cache-miss, it doesn't matter what we do
                // if we receive a 4xx, so no point clearing any
                // caches
                false,
            )
            .await;
        }

        let record = per_dict_id_cache.read().peek(&actual_dict_id).cloned();
        match record {
            Some(record) => {
                if record.lcut > since_time {
                    Some(record)
                } else {
                    Some(Arc::new(ConfigSpecForCompany {
                        lcut: record.lcut,
                        zstd_dict_id: None,
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

pub async fn get_dictionary_compressed_config_spec_and_shadow(
    authorized_request_context_cache: Arc<AuthorizedRequestContextCache>,
    shared_dict_request_context: &Arc<AuthorizedRequestContext>,
    shared_dict_config_spec_store: &Arc<SharedDictConfigSpecStore>,
    config_spec_store: &Arc<ConfigSpecStore>,
    since_time: u64,
    zstd_dict_id: &Option<Arc<str>>,
) -> Option<Arc<ConfigSpecForCompany>> {
    let shared_dict_request_context_clone = Arc::clone(shared_dict_request_context);
    let config_spec_store = Arc::clone(config_spec_store);
    tokio::spawn(async move {
        // At the time of writing, Server SDK DataStores only support reading unencoded, uncompressed responses.
        // Therefore, we must rely on the existing DataStore logic invoked in the existing non-shared-dictionary
        // config spec store to keep DataStore up-to-date with uncompressed responses.
        // This effectively shadows all requests made to /v2/download_config_specs/d/... with a request to
        // /v2/download_config_specs
        let shadow_request_context = authorized_request_context_cache.get_or_insert(
            shared_dict_request_context_clone.sdk_key.clone(),
            NormalizedPath::V2DownloadConfigSpecs,
            CompressionEncoder::Gzip, // decompression is handled before committing to DataStore
        );
        let _ = config_spec_store
            .get_config_spec(&shadow_request_context, since_time)
            .await;
    });
    shared_dict_config_spec_store
        .get_config_spec(shared_dict_request_context, since_time, zstd_dict_id)
        .await
}
