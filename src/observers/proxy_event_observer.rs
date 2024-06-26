use lazy_static::lazy_static;
use std::sync::Arc;
use tokio::{sync::RwLock, task};

use super::{ProxyEvent, ProxyEventObserverTrait};

lazy_static! {
    static ref PROXY_EVENT_OBSERVER: ProxyEventObserver = ProxyEventObserver::new();
}

pub struct ProxyEventObserver {
    observers: Arc<RwLock<Vec<Arc<dyn ProxyEventObserverTrait + Send + Sync>>>>,
}

impl Default for ProxyEventObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl ProxyEventObserver {
    pub fn new() -> Self {
        ProxyEventObserver {
            observers: Arc::new(RwLock::new(vec![])),
        }
    }

    pub async fn add_observer(observer: Arc<dyn ProxyEventObserverTrait + Send + Sync>) {
        PROXY_EVENT_OBSERVER.observers.write().await.push(observer);
    }

    pub async fn publish_event(mut event: ProxyEvent) {
        event.sdk_key = format!(
            "{}{}",
            event.sdk_key.chars().take(20).collect::<String>(),
            "***"
        );
        task::spawn(async move {
            for observer in PROXY_EVENT_OBSERVER.observers.read().await.iter() {
                observer.handle_event(&event).await;
            }
        });
    }
}
