use std::sync::Arc;

use tokio::{sync::RwLock, task};

use crate::{
    datastore::data_providers::DataProviderRequestResult,
    servers::http_server::AuthorizedRequestContext,
};

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
        request_context: &AuthorizedRequestContext,
        lcut: u64,
        data: &Arc<String>,
    ) {
        let result_copy = *result;
        let rc_clone = request_context.clone();
        let data_copy = Arc::clone(data);
        let shared_observer = self.observers.clone();
        task::spawn(async move {
            for observer in shared_observer.read().await.iter() {
                if !observer.force_notifier_to_wait_for_update() {
                    observer
                        .update(&result_copy, &rc_clone, lcut, &data_copy)
                        .await;
                }
            }
        });

        for observer in self.observers.read().await.iter() {
            if observer.force_notifier_to_wait_for_update() {
                observer.update(result, request_context, lcut, data).await;
            }
        }
    }
}
