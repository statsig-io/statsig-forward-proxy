use std::sync::Arc;

use tokio::{sync::RwLock, task};

use crate::datastore::data_providers::DataProviderRequestResult;

use super::HttpDataProviderObserverTrait;

pub struct HttpDataProviderObserver {
    observers: Arc<RwLock<Vec<Arc<dyn HttpDataProviderObserverTrait + Send + Sync>>>>,
}

impl Default for HttpDataProviderObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpDataProviderObserver {
    pub fn new() -> Self {
        HttpDataProviderObserver {
            observers: Arc::new(RwLock::new(vec![])),
        }
    }

    pub async fn add_observer(
        &self,
        observer: Arc<dyn HttpDataProviderObserverTrait + Send + Sync>,
    ) {
        self.observers.write().await.push(observer);
    }

    pub async fn notify_all(
        &self,
        result: &DataProviderRequestResult,
        sdk_key: &str,
        lcut: u64,
        data: &Arc<String>,
        path: &str,
    ) {
        let result_copy = *result;
        let sdk_key_copy = sdk_key.to_string();
        let data_copy = Arc::clone(data);
        let shared_observer = self.observers.clone();
        let path_copy = path.to_string();
        task::spawn(async move {
            for observer in shared_observer.read().await.iter() {
                if !observer.force_notifier_to_wait_for_update() {
                    observer
                        .update(&result_copy, &sdk_key_copy, lcut, &data_copy, &path_copy)
                        .await;
                }
            }
        });

        for observer in self.observers.read().await.iter() {
            if observer.force_notifier_to_wait_for_update() {
                observer.update(result, sdk_key, lcut, data, path).await;
            }
        }
    }
}
