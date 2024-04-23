use super::request_builder::RequestBuilderTrait;
use super::{http_data_provider::HttpDataProvider, DataProviderRequestResult, DataProviderTrait};
use std::{collections::HashMap, sync::Arc};

use futures::future::try_join_all;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration, Instant};

pub struct BackgroundDataProvider {
    http_data_prover: Arc<HttpDataProvider>,
    polling_interval_in_s: u64,
    update_batch_size: u64,
    foreground_fetch_lock: RwLock<HashMap<String, Arc<RwLock<Option<Instant>>>>>,
    request_builder: RwLock<Arc<Vec<Arc<dyn RequestBuilderTrait>>>>,
}

pub async fn foreground_fetch(bdp: Arc<BackgroundDataProvider>, sdk_key: &str, since_time: u64) {
    let key = format!("{}|{}", sdk_key, since_time);
    let contains_key = bdp.foreground_fetch_lock.read().await.contains_key(&key);
    let lock_ref = match contains_key {
        true => Arc::clone(
            bdp.foreground_fetch_lock
                .read()
                .await
                .get(&key)
                .expect("validated existence"),
        ),
        false => {
            let new_lock_ref: Arc<RwLock<Option<Instant>>> = Arc::new(RwLock::new(None));
            bdp.foreground_fetch_lock
                .write()
                .await
                .insert(key.clone(), Arc::clone(&new_lock_ref));
            new_lock_ref
        }
    };

    // If already initialized, and we checked in the last minute
    // then return
    if let Some(init_time) = *lock_ref.read().await {
        if Instant::now().duration_since(init_time) < Duration::from_secs(60) {
            return;
        }
    }
    // Otherwise, grab write lock and set to true
    // after we finish initializing
    let mut lock = lock_ref.write().await;
    // Someone else could have won the race, so check one more time...
    if let Some(init_time) = *lock {
        if Instant::now().duration_since(init_time) < Duration::from_secs(60) {
            return;
        }
    }

    // If we won the race, then initialize and set
    // has initialized to true
    let tasks = bdp
        .request_builder
        .read()
        .await
        .iter()
        .map(|builder| {
            let shared_data_provider_clone = Arc::clone(&bdp.http_data_prover);
            let builder_clone = Arc::clone(builder);
            let sdk_key_copy = sdk_key.to_string();
            tokio::task::spawn(async move {
                let mut data = HashMap::new();
                data.insert(sdk_key_copy.clone(), since_time);
                BackgroundDataProvider::impl_foreground_fetch(
                    data.into_iter(),
                    &shared_data_provider_clone,
                    1,
                    &builder_clone,
                )
                .await;
            })
        })
        .collect::<Vec<_>>();
    if let Err(e) = try_join_all(tasks).await {
        eprintln!("Failed to join background data provider fetches: {:?}", e);
    }
    *lock = Some(Instant::now());
}

impl BackgroundDataProvider {
    pub fn new(
        data_provider: Arc<HttpDataProvider>,
        polling_interval_in_s: u64,
        update_batch_size: u64,
    ) -> Self {
        BackgroundDataProvider {
            http_data_prover: data_provider,
            polling_interval_in_s,
            update_batch_size,
            foreground_fetch_lock: RwLock::new(HashMap::new()),
            request_builder: RwLock::new(Arc::new(Vec::new())),
        }
    }

    pub async fn add_request_builder(&self, request_builder: Arc<dyn RequestBuilderTrait>) {
        let mut lock = self.request_builder.write().await;
        let data_vec = Arc::make_mut(&mut *lock);
        data_vec.push(request_builder);
    }

    pub async fn start_background_thread(&self) {
        let shared_data_provider = self.http_data_prover.clone();
        let batch_size = self.update_batch_size;
        let polling_interval_in_s = self.polling_interval_in_s;
        let request_builder = Arc::clone(&*self.request_builder.read().await);
        tokio::spawn(async move {
            loop {
                let tasks = request_builder
                    .iter()
                    .map(|builder| {
                        let shared_data_provider_clone = Arc::clone(&shared_data_provider);
                        let builder_clone = Arc::clone(builder);
                        tokio::task::spawn(async move {
                            if !builder_clone.should_make_request().await {
                                return;
                            }

                            let store_iter = builder_clone
                                .get_sdk_key_store()
                                .get_registered_store()
                                .await;
                            BackgroundDataProvider::impl_foreground_fetch(
                                store_iter,
                                &shared_data_provider_clone,
                                batch_size,
                                &builder_clone,
                            )
                            .await;
                        })
                    })
                    .collect::<Vec<_>>();
                if let Err(e) = try_join_all(tasks).await {
                    eprintln!("Failed to join background data provider fetches: {:?}", e);
                }
                sleep(Duration::from_secs(polling_interval_in_s)).await;
            }
        });
    }

    async fn impl_foreground_fetch(
        store_iter: impl Iterator<Item = (String, u64)>,
        data_provider: &Arc<HttpDataProvider>,
        update_batch_size: u64,
        request_builder: &Arc<dyn RequestBuilderTrait>,
    ) {
        let mut join_handles = Vec::new();

        for (sdk_key, lcut) in store_iter {
            let data_provider = data_provider.clone();
            let request_builder = request_builder.clone();
            let join_handle = tokio::task::spawn(async move {
                let dp_result = data_provider.get(&request_builder, &sdk_key, lcut).await;
                if dp_result.result == DataProviderRequestResult::NoDataAvailable
                    || dp_result.result == DataProviderRequestResult::DataAvailable
                {
                    let data = dp_result
                        .data
                        .expect("If data is available, data must exist");
                    request_builder
                        .get_observers()
                        .notify_all(
                            &dp_result.result,
                            &sdk_key,
                            data.1,
                            &data.0,
                            &request_builder.get_path(),
                        )
                        .await;
                } else if dp_result.result == DataProviderRequestResult::Error {
                    if let Some(backup_data) = request_builder
                        .get_backup_cache()
                        .get(&sdk_key, &request_builder.get_path())
                        .await
                    {
                        request_builder
                            .get_observers()
                            .notify_all(
                                &dp_result.result,
                                &sdk_key,
                                lcut,
                                &backup_data,
                                &request_builder.get_path(),
                            )
                            .await;
                    }
                } else if dp_result.result == DataProviderRequestResult::Unauthorized {
                    request_builder
                        .get_observers()
                        .notify_all(
                            &dp_result.result,
                            &sdk_key,
                            lcut,
                            &Arc::new("".to_string()),
                            &request_builder.get_path(),
                        )
                        .await;
                }
            });

            join_handles.push(join_handle);
            if join_handles.len() >= update_batch_size as usize {
                futures::future::join_all(join_handles).await;
                join_handles = Vec::new();
            }
        }
        futures::future::join_all(join_handles).await;
    }
}
