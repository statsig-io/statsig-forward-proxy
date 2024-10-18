use std::sync::Arc;

use tokio::sync::RwLock;

use crate::{
    datastore::data_providers::{http_data_provider::ResponsePayload, DataProviderRequestResult},
    servers::authorized_request_context::AuthorizedRequestContext,
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
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
        data: &Arc<ResponsePayload>,
    ) {
        let (async_observers, sync_observers): (Vec<_>, Vec<_>) = {
            let observers = self.observers.read().await;
            observers
                .iter()
                .cloned()
                .partition(|o| !o.force_notifier_to_wait_for_update())
        };

        if !async_observers.is_empty() {
            let result_copy = *result;
            let rc_clone = Arc::clone(request_context);
            let data_copy = Arc::clone(data);
            rocket::tokio::spawn(async move {
                for observer in async_observers {
                    observer
                        .update(&result_copy, &rc_clone, lcut, &data_copy)
                        .await;
                }
            });
        }

        for observer in sync_observers {
            observer.update(result, request_context, lcut, data).await;
        }
    }
}
