use std::sync::Arc;

use tokio::sync::RwLock;

use crate::datastore::data_providers::{FullRequestContext, ResponseContext};

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
        request_context: &Arc<FullRequestContext>,
        response_context: &Arc<ResponseContext>,
    ) {
        let (async_observers, sync_observers): (Vec<_>, Vec<_>) = {
            let observers = self.observers.read().await;
            observers
                .iter()
                .cloned()
                .partition(|o| !o.force_notifier_to_wait_for_update())
        };

        if !async_observers.is_empty() {
            let request_context_clone = Arc::clone(request_context);
            let response_context_clone = Arc::clone(response_context);
            rocket::tokio::spawn(async move {
                for observer in async_observers {
                    observer
                        .update(&request_context_clone, &response_context_clone)
                        .await;
                }
            });
        }

        for observer in sync_observers {
            observer.update(request_context, response_context).await;
        }
    }
}
