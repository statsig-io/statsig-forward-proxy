use crate::observers::NewDcsObserverTrait;
use crate::{datastore::sdk_key_store, observers::new_dcs_observer::NewDcsObserver};

use super::{
    http_data_provider::{self, HttpDataProvider},
    DataProviderRequestResult, DataProviderTrait,
};
use std::{collections::HashMap, sync::Arc};

use tokio::time::{sleep, Duration};

use cached::proc_macro::cached;

pub struct BackgroundDataProvider {
    http_data_prover: Arc<http_data_provider::HttpDataProvider>,
    backup_cache: Arc<dyn NewDcsObserverTrait + Sync + Send>,
    dcs_observer: Arc<NewDcsObserver>,
}

// Note: sync_write synchronizes on the entire store and not just
//       the cache key. This means if there are a constant stream
//       of cold cache requests, you will have a bottleneck. However,
//       since cold cache requests should stop pretty quickly, we
//       should be fine.
#[cached(
    key = "String",
    convert = r#"{ format!("{}|{}", sdk_key, since_time) }"#,
    size = 100,
    sync_writes = true
)]
pub async fn foreground_fetch(bdp: Arc<BackgroundDataProvider>, sdk_key: &str, since_time: u64) {
    let mut data = HashMap::new();
    data.insert(sdk_key.to_string(), since_time);
    BackgroundDataProvider::impl_foreground_fetch(
        data.into_iter(),
        bdp.http_data_prover.clone(),
        bdp.backup_cache.clone(),
        bdp.dcs_observer.clone(),
    )
    .await;
}

impl BackgroundDataProvider {
    pub fn new(
        backup_cache: Arc<dyn NewDcsObserverTrait + Sync + Send>,
        data_provider: Arc<HttpDataProvider>,
        dcs_observer: Arc<NewDcsObserver>,
    ) -> Self {
        BackgroundDataProvider {
            http_data_prover: data_provider,
            backup_cache,
            dcs_observer,
        }
    }

    pub fn start_background_thread(&self, sdk_key_store: Arc<sdk_key_store::SdkKeyStore>) {
        let shared_data_provider = self.http_data_prover.clone();
        let shared_observer = self.dcs_observer.clone();
        let shared_backup_cache = self.backup_cache.clone();
        tokio::spawn(async move {
            loop {
                let store_iter = sdk_key_store.get_registered_store().await;
                BackgroundDataProvider::impl_foreground_fetch(
                    store_iter,
                    shared_data_provider.clone(),
                    shared_backup_cache.clone(),
                    shared_observer.clone(),
                )
                .await;
                sleep(Duration::from_secs(10)).await;
            }
        });
    }

    async fn impl_foreground_fetch(
        store_iter: impl Iterator<Item = (String, u64)>,
        data_provider: Arc<HttpDataProvider>,
        backup_cache: Arc<dyn NewDcsObserverTrait + Sync + Send>,
        dcs_observer: Arc<NewDcsObserver>,
    ) {
        let mut join_handles = Vec::new();

        for (sdk_key, lcut) in store_iter {
            let data_provider = data_provider.clone();
            let backup_cache = backup_cache.clone();
            let dcs_observer = dcs_observer.clone();

            let join_handle = tokio::task::spawn(async move {
                let dp_result = data_provider.get(&sdk_key, lcut).await;
                if dp_result.result == DataProviderRequestResult::NoDataAvailable
                    || dp_result.result == DataProviderRequestResult::DataAvailable
                {
                    let data = dp_result
                        .data
                        .expect("If data is available, data must exist");
                    dcs_observer
                        .notify_all(&dp_result.result, &sdk_key, data.1, &data.0)
                        .await;
                } else if dp_result.result == DataProviderRequestResult::Error {
                    if let Some(backup_data) = backup_cache.get(&sdk_key).await {
                        dcs_observer
                            .notify_all(&dp_result.result, &sdk_key, lcut, &backup_data)
                            .await;
                    }
                }
            });

            join_handles.push(join_handle);
        }

        futures::future::join_all(join_handles).await;
    }
}
