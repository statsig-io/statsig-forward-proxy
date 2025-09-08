use crate::datastore::sdk_key_store::{SdkKeyStore, SdkKeyStoreItem};
use crate::servers::authorized_request_context::AuthorizedRequestContext;
use crate::utils::compress_encoder::CompressionEncoder;
use crate::GRACEFUL_SHUTDOWN_TOKEN;

use super::http_data_provider::ResponsePayload;
use super::request_builder::{CachedRequestBuilders, RequestBuilderTrait};
use super::{http_data_provider::HttpDataProvider, DataProviderRequestResult, DataProviderTrait};
use super::{FullRequestContext, ResponseContext};
use std::sync::Arc;

use bytes::Bytes;
use tokio::runtime::Handle;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tokio::time::Duration;

use dashmap::DashMap;

type ForegroundFetchLockKey = (Arc<AuthorizedRequestContext>, Option<Arc<str>>);
pub struct BackgroundDataProvider {
    http_data_prover: Arc<HttpDataProvider>,
    polling_interval_in_s: u64,
    update_batch_size: u64,
    sdk_key_store: Arc<SdkKeyStore>,
    foreground_fetch_lock: DashMap<ForegroundFetchLockKey, Arc<RwLock<bool>>>,
    clear_datastore_on_unauthorized: bool,
    http_connection_pool_max_idle_per_host: usize,
    enable_http2: bool,
}

pub async fn foreground_fetch(
    bdp: Arc<BackgroundDataProvider>,
    request_context: &Arc<AuthorizedRequestContext>,
    since_time: u64,
    zstd_dict_id: &Option<Arc<str>>,
    clear_datastore_on_unauthorized: bool,
) {
    let key = (Arc::clone(request_context), zstd_dict_id.clone());
    let lock_ref: Arc<RwLock<bool>> = bdp
        .foreground_fetch_lock
        .entry(key.clone())
        .or_insert_with(|| Arc::new(RwLock::new(false)))
        .clone();

    // If the value is false, that means no one has fetched yet, so we should attempt to fetch.
    if !*lock_ref.read().await {
        let mut per_key_lock = lock_ref.write().await;

        // Double-check in case another thread updated while we were waiting for the write lock
        if !*per_key_lock {
            *per_key_lock = true;

            BackgroundDataProvider::impl_foreground_fetch(
                vec![SdkKeyStoreItem {
                    request_context: Arc::clone(request_context),
                    lcut: since_time,
                    zstd_dict_id: zstd_dict_id.clone(),
                }],
                &bdp.http_data_prover,
                1,
                clear_datastore_on_unauthorized,
                bdp.http_connection_pool_max_idle_per_host,
                bdp.enable_http2,
            )
            .await;

            // Key eviction: Remove this lock from the cache after 30 seconds
            // This effectively rate limits requests to once per 30 seconds
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                bdp.foreground_fetch_lock.remove(&key);
            });
        }
    }
}

impl BackgroundDataProvider {
    pub fn new(
        data_provider: Arc<HttpDataProvider>,
        polling_interval_in_s: u64,
        update_batch_size: u64,
        sdk_key_store: Arc<SdkKeyStore>,
        clear_datastore_on_unauthorized: bool,
        http_connection_pool_max_idle_per_host: usize,
        enable_http2: bool,
    ) -> Self {
        BackgroundDataProvider {
            http_data_prover: data_provider,
            polling_interval_in_s,
            update_batch_size,
            foreground_fetch_lock: DashMap::new(),
            sdk_key_store,
            clear_datastore_on_unauthorized,
            http_connection_pool_max_idle_per_host,
            enable_http2,
        }
    }

    pub async fn start_background_thread(&self) {
        let shared_data_provider = self.http_data_prover.clone();
        let batch_size = self.update_batch_size;
        let polling_interval_in_s = self.polling_interval_in_s;
        let sdk_key_store = Arc::clone(&self.sdk_key_store);
        let graceful_shutdown_token = GRACEFUL_SHUTDOWN_TOKEN.clone();
        let clear_datastore_on_unauthorized = self.clear_datastore_on_unauthorized;
        let http_connection_pool_max_idle_per_host = self.http_connection_pool_max_idle_per_host;
        let enable_http2 = self.enable_http2;
        rocket::tokio::task::spawn_blocking(move || {
            Handle::current().block_on(async move {
                loop {
                    BackgroundDataProvider::impl_foreground_fetch(
                        sdk_key_store.get_registered_store(),
                        &shared_data_provider,
                        batch_size,
                        clear_datastore_on_unauthorized,
                        http_connection_pool_max_idle_per_host,
                        enable_http2,
                    )
                    .await;

                    if tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(polling_interval_in_s)) => { false },
                        _ = graceful_shutdown_token.cancelled() => {
                            true
                        },
                    } {
                        break;
                    }
                }
            });
        });
    }

    async fn impl_foreground_fetch(
        store_iter: Vec<SdkKeyStoreItem>,
        data_provider: &Arc<HttpDataProvider>,
        update_batch_size: u64,
        clear_datastore_on_unauthorized: bool,
        http_connection_pool_max_idle_per_host: usize,
        enable_http2: bool,
    ) {
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(http_connection_pool_max_idle_per_host);
        if enable_http2 {
            builder = builder.http2_prior_knowledge();
        }
        match builder.build() {
            Ok(http_client) => {
                let mut join_handles = Vec::with_capacity(update_batch_size as usize);

                for item in store_iter {
                    let request_builder =
                        CachedRequestBuilders::get_request_builder(&item.request_context.path);
                    if !request_builder
                        .should_make_request(&item.request_context)
                        .await
                    {
                        continue;
                    }

                    let data_provider = data_provider.clone();
                    let client_clone = http_client.clone();
                    let join_handle = tokio::task::spawn(async move {
                        match timeout(
                            Duration::from_secs(60),
                            Self::process_request(
                                data_provider,
                                request_builder,
                                &Arc::new(FullRequestContext {
                                    authorized_request_context: Arc::clone(&item.request_context),
                                    zstd_dict_id: item.zstd_dict_id.clone(),
                                }),
                                item.lcut,
                                &item.zstd_dict_id,
                                &client_clone,
                                clear_datastore_on_unauthorized,
                            ),
                        )
                        .await
                        {
                            Ok(_) => {}
                            Err(_) => {
                                let mut key = item.request_context.sdk_key.clone();
                                key.truncate(20);
                                eprintln!(
                                "Error: process_request timed out after 60 seconds for request_context.. skipping update..({}): {}",
                                key,
                                item.request_context.path)
                            }
                        }
                    });

                    join_handles.push(join_handle);
                    if join_handles.len() >= (update_batch_size as usize) {
                        futures::future::join_all(join_handles.drain(..)).await;
                    }
                }
                futures::future::join_all(join_handles).await;
            }
            Err(e) => {
                eprintln!("Failed to build http client.. skipping background update...: {e}");
            }
        }
    }

    async fn process_request(
        data_provider: Arc<HttpDataProvider>,
        request_builder: Arc<dyn RequestBuilderTrait>,
        request_context: &Arc<FullRequestContext>,
        lcut: u64,
        zstd_dict_id: &Option<Arc<str>>,
        http_client: &reqwest::Client,
        clear_datastore_on_unauthorized: bool,
    ) {
        let dp_result = data_provider
            .get(
                http_client,
                &request_builder,
                &request_context.authorized_request_context,
                lcut,
                zstd_dict_id,
            )
            .await;

        match dp_result.result {
            DataProviderRequestResult::DataAvailable => {
                if let Some(data) = dp_result.body {
                    if !request_context.authorized_request_context.use_lcut
                        || lcut != dp_result.lcut
                    {
                        Self::notify_observers(
                            request_context,
                            &Arc::new(ResponseContext {
                                result_type: dp_result.result,
                                lcut: dp_result.lcut,
                                zstd_dict_id: dp_result.zstd_dict_id,
                                body: data,
                            }),
                            &request_builder,
                        )
                        .await;
                    }
                }
            }
            DataProviderRequestResult::Error => {
                if let Some(backup_data) = request_builder
                    .get_backup_cache()
                    .get(&request_context.authorized_request_context, zstd_dict_id)
                    .await
                {
                    Self::notify_observers(
                        request_context,
                        &Arc::new(ResponseContext {
                            result_type: dp_result.result,
                            lcut: backup_data.lcut,
                            zstd_dict_id: backup_data.zstd_dict_id.clone(),
                            body: Arc::clone(&backup_data.config),
                        }),
                        &request_builder,
                    )
                    .await;
                }
            }
            DataProviderRequestResult::Unauthorized => {
                if clear_datastore_on_unauthorized {
                    Self::notify_observers(
                        request_context,
                        &Arc::new(ResponseContext {
                            result_type: dp_result.result,
                            lcut,
                            zstd_dict_id: dp_result.zstd_dict_id,
                            body: Arc::new(ResponsePayload {
                                encoding: Arc::new(CompressionEncoder::PlainText),
                                data: Arc::new(Bytes::new()),
                            }),
                        }),
                        &request_builder,
                    )
                    .await;
                }
            }
            _ => {}
        }
    }

    async fn notify_observers(
        request_context: &Arc<FullRequestContext>,
        response_context: &Arc<ResponseContext>,
        request_builder: &Arc<dyn RequestBuilderTrait>,
    ) {
        request_builder
            .get_observers()
            .notify_all(request_context, response_context)
            .await;
    }
}
