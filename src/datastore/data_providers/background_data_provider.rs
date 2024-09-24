use crate::datastore::sdk_key_store::SdkKeyStore;
use crate::servers::authorized_request_context::AuthorizedRequestContext;

use super::request_builder::{CachedRequestBuilders, RequestBuilderTrait};
use super::{http_data_provider::HttpDataProvider, DataProviderRequestResult, DataProviderTrait};
use std::collections::hash_map::Entry;
use std::{collections::HashMap, sync::Arc};

use tokio::runtime::Handle;
use tokio::sync::RwLock;
use tokio::time::{Duration, Instant};

pub struct BackgroundDataProvider {
    http_data_prover: Arc<HttpDataProvider>,
    polling_interval_in_s: u64,
    update_batch_size: u64,
    sdk_key_store: Arc<SdkKeyStore>,
    foreground_fetch_lock:
        RwLock<HashMap<Arc<AuthorizedRequestContext>, Arc<RwLock<Option<Instant>>>>>,
}

pub async fn foreground_fetch(
    bdp: Arc<BackgroundDataProvider>,
    request_context: &Arc<AuthorizedRequestContext>,
    since_time: u64,
) {
    let lock_ref: Arc<RwLock<Option<Instant>>> = {
        let mut master_lock = bdp.foreground_fetch_lock.write().await;
        match master_lock.entry(Arc::clone(request_context)) {
            Entry::Occupied(entry) => Arc::clone(entry.get()),
            Entry::Vacant(entry) => {
                let new_lock_ref = Arc::new(RwLock::new(None));
                entry.insert(Arc::clone(&new_lock_ref));
                new_lock_ref
            }
        }
    };

    let should_fetch = {
        let per_key_lock = lock_ref.read().await;
        should_perform_fetch(&per_key_lock)
    };

    if should_fetch {
        let mut per_key_lock = lock_ref.write().await;

        // Double-check in case another thread updated while we were waiting for the write lock
        if should_perform_fetch(&per_key_lock) {
            // Release the lock before the potentially long-running operation
            *per_key_lock = Some(Instant::now());

            BackgroundDataProvider::impl_foreground_fetch(
                vec![(Arc::clone(request_context), since_time)],
                &bdp.http_data_prover,
                1,
            )
            .await;
        }
    }
}

fn should_perform_fetch(per_key_lock: &Option<Instant>) -> bool {
    match *per_key_lock {
        Some(init_time) => {
            let duration = Instant::now().duration_since(init_time);
            println!("Duration since last fetch: {:?}", duration);
            duration >= Duration::from_secs(30)
        }
        None => true,
    }
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
        rocket::tokio::task::spawn_blocking(move || {
            Handle::current().block_on(async move {
                loop {
                    BackgroundDataProvider::impl_foreground_fetch(
                        sdk_key_store.get_registered_store().await,
                        &shared_data_provider,
                        batch_size,
                    )
                    .await;
                    rocket::tokio::time::sleep(Duration::from_secs(polling_interval_in_s)).await;
                }
            });
        });
    }

    async fn impl_foreground_fetch(
        store_iter: Vec<(Arc<AuthorizedRequestContext>, u64)>,
        data_provider: &Arc<HttpDataProvider>,
        update_batch_size: u64,
    ) {
        let mut join_handles = Vec::with_capacity(update_batch_size as usize);

        for (request_context, lcut) in store_iter {
            let request_builder = CachedRequestBuilders::get_request_builder(&request_context.path);
            if !request_builder.should_make_request(&request_context).await {
                continue;
            }

            let data_provider = data_provider.clone();
            let join_handle = tokio::task::spawn(async move {
                Self::process_request(data_provider, request_builder, &request_context, lcut).await;
            });

            join_handles.push(join_handle);
            if join_handles.len() >= (update_batch_size as usize) {
                futures::future::join_all(join_handles.drain(..)).await;
            }
        }
        futures::future::join_all(join_handles).await;
    }

    async fn process_request(
        data_provider: Arc<HttpDataProvider>,
        request_builder: Arc<dyn RequestBuilderTrait>,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
    ) {
        let dp_result = data_provider
            .get(&request_builder, request_context, lcut)
            .await;

        match dp_result.result {
            DataProviderRequestResult::DataAvailable => {
                if let Some(data) = dp_result.data {
                    if !request_context.use_lcut || lcut != data.1 {
                        Self::notify_observers(
                            &request_builder,
                            &dp_result.result,
                            request_context,
                            data.1,
                            &data.0,
                        )
                        .await;
                    }
                }
            }
            DataProviderRequestResult::Error => {
                if let Some(backup_data) = request_builder
                    .get_backup_cache()
                    .get(request_context)
                    .await
                {
                    Self::notify_observers(
                        &request_builder,
                        &dp_result.result,
                        request_context,
                        lcut,
                        &backup_data,
                    )
                    .await;
                }
            }
            DataProviderRequestResult::Unauthorized => {
                Self::notify_observers(
                    &request_builder,
                    &dp_result.result,
                    request_context,
                    lcut,
                    &Arc::from(String::new()),
                )
                .await;
            }
            _ => {}
        }
    }

    async fn notify_observers(
        request_builder: &Arc<dyn RequestBuilderTrait>,
        result: &DataProviderRequestResult,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
        data: &Arc<str>,
    ) {
        request_builder
            .get_observers()
            .notify_all(result, request_context, lcut, data)
            .await;
    }
}
