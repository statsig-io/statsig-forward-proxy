use crate::datastore::sdk_key_store::SdkKeyStore;
use crate::servers::http_server::AuthorizedRequestContext;

use super::request_builder::CachedRequestBuilders;
use super::{http_data_provider::HttpDataProvider, DataProviderRequestResult, DataProviderTrait};
use std::{collections::HashMap, sync::Arc};


use tokio::sync::RwLock;
use tokio::time::{sleep, Duration, Instant};

pub struct BackgroundDataProvider {
    http_data_prover: Arc<HttpDataProvider>,
    polling_interval_in_s: u64,
    update_batch_size: u64,
    sdk_key_store: Arc<SdkKeyStore>,
    foreground_fetch_lock: RwLock<HashMap<AuthorizedRequestContext, Arc<RwLock<Option<Instant>>>>>,
}

pub async fn foreground_fetch(
    bdp: Arc<BackgroundDataProvider>,
    request_context: &AuthorizedRequestContext,
    since_time: u64,
    sdk_key_store: Arc<SdkKeyStore>,
) {
    let mut master_lock = bdp.foreground_fetch_lock.write().await;
    let lock_ref: Arc<RwLock<Option<Instant>>> = match master_lock.contains_key(request_context) {
        true => Arc::clone(
            master_lock
                .get(request_context)
                .expect("validated existence"),
        ),
        false => {
            let new_lock_ref: Arc<RwLock<Option<Instant>>> = Arc::new(RwLock::new(None));
            master_lock.insert(request_context.clone(), Arc::clone(&new_lock_ref));
            new_lock_ref
        }
    };
    // Explicitly drop the master lock to allow other sdk keys to be processed
    drop(master_lock);

    // If already initialized, and we checked in the last minute
    // then return
    let mut per_key_lock = lock_ref.write().await;
    if let Some(init_time) = *per_key_lock {
        if !sdk_key_store.has_key(request_context).await
            && Instant::now().duration_since(init_time) < Duration::from_secs(60)
        {
            return;
        }
    }

    // If we won the race, then initialize and set
    // has initialized to true
    let mut data = HashMap::new();
    data.insert(request_context.clone(), since_time);
    BackgroundDataProvider::impl_foreground_fetch(data.into_iter(), &bdp.http_data_prover, 1).await;

    *per_key_lock = Some(Instant::now());
}

impl BackgroundDataProvider {
    pub fn new(
        data_provider: Arc<HttpDataProvider>,
        polling_interval_in_s: u64,
        update_batch_size: u64,
        sdk_key_store: Arc<SdkKeyStore>,
    ) -> Self {
        BackgroundDataProvider {
            http_data_prover: data_provider,
            polling_interval_in_s,
            update_batch_size,
            foreground_fetch_lock: RwLock::new(HashMap::new()),
            sdk_key_store,
        }
    }

    pub async fn start_background_thread(&self) {
        let shared_data_provider = self.http_data_prover.clone();
        let batch_size = self.update_batch_size;
        let polling_interval_in_s = self.polling_interval_in_s;
        let sdk_key_store = Arc::clone(&self.sdk_key_store);
        tokio::spawn(async move {
            loop {
                BackgroundDataProvider::impl_foreground_fetch(
                    sdk_key_store.get_registered_store().await,
                    &shared_data_provider,
                    batch_size,
                )
                .await;
                sleep(Duration::from_secs(polling_interval_in_s)).await;
            }
        });
    }

    async fn impl_foreground_fetch(
        store_iter: impl Iterator<Item = (AuthorizedRequestContext, u64)>,
        data_provider: &Arc<HttpDataProvider>,
        update_batch_size: u64,
    ) {
        let mut join_handles = Vec::new();

        for (request_context, lcut) in store_iter {
            let request_builder =
                CachedRequestBuilders::get_request_builder(&request_context.path).await;
            if !request_builder.should_make_request(&request_context).await {
                continue;
            }

            let data_provider = data_provider.clone();
            let join_handle = tokio::task::spawn(async move {
                let dp_result = data_provider
                    .get(&request_builder, &request_context, lcut)
                    .await;
                if dp_result.result == DataProviderRequestResult::DataAvailable {
                    let data = dp_result
                        .data
                        .expect("If data is available, data must exist");

                    // [For DCS] Occassionally, origin will return the same payload multiple times
                    // This could happen from cache inconsistency. If this happens,
                    // rather than notifying, just skip updating anyone.
                    //
                    // This allows all listeners to not worry about doing double work
                    if !request_context.use_lcut || lcut != data.1 {
                        request_builder
                            .get_observers()
                            .notify_all(&dp_result.result, &request_context, data.1, &data.0)
                            .await;
                    }
                } else if dp_result.result == DataProviderRequestResult::Error {
                    if let Some(backup_data) = request_builder
                        .get_backup_cache()
                        .get(&request_context)
                        .await
                    {
                        request_builder
                            .get_observers()
                            .notify_all(&dp_result.result, &request_context, lcut, &backup_data)
                            .await;
                    }
                } else if dp_result.result == DataProviderRequestResult::Unauthorized {
                    request_builder
                        .get_observers()
                        .notify_all(
                            &dp_result.result,
                            &request_context,
                            lcut,
                            &Arc::new("".to_string()),
                        )
                        .await;
                }
            });

            join_handles.push(join_handle);
            if join_handles.len() >= (update_batch_size as usize) {
                futures::future::join_all(join_handles).await;
                join_handles = Vec::new();
            }
        }
        futures::future::join_all(join_handles).await;
    }
}
