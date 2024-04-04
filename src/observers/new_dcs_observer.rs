use std::sync::Arc;

use tokio::{sync::RwLock, task};

use crate::datastore::data_providers::DataProviderRequestResult;

use super::NewDcsObserverTrait;

pub struct NewDcsObserver {
    observers: Arc<RwLock<Vec<Arc<dyn NewDcsObserverTrait + Send + Sync>>>>,
}

impl Default for NewDcsObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl NewDcsObserver {
    pub fn new() -> Self {
        NewDcsObserver {
            observers: Arc::new(RwLock::new(vec![])),
        }
    }

    pub async fn add_observer(&self, observer: Arc<dyn NewDcsObserverTrait + Send + Sync>) {
        self.observers.write().await.push(observer);
    }

    pub async fn notify_all(
        &self,
        result: &DataProviderRequestResult,
        sdk_key: &str,
        lcut: u64,
        data: &Arc<String>,
    ) {
        let result_copy = *result;
        let sdk_key_copy = sdk_key.to_string();
        let data_copy = Arc::clone(data);
        let shared_observer = self.observers.clone();
        task::spawn(async move {
            for observer in shared_observer.read().await.iter() {
                if !observer.force_notifier_to_wait_for_update() {
                    observer
                        .update(&result_copy, &sdk_key_copy, lcut, &data_copy)
                        .await;
                }
            }
        });

        for observer in self.observers.read().await.iter() {
            if observer.force_notifier_to_wait_for_update() {
                observer.update(result, sdk_key, lcut, data).await;
            }
        }
    }
}
