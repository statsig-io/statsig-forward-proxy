use lazy_static::lazy_static;
use std::sync::Arc;
use tokio::{
    runtime::Handle,
    sync::broadcast::{self, Sender},
};

use super::{ProxyEvent, ProxyEventObserverTrait};

lazy_static! {
    static ref PROXY_EVENT_OBSERVER: ProxyEventObserver = ProxyEventObserver::new();
}

pub struct ProxyEventObserver {
    pub sender: Arc<Sender<Arc<ProxyEvent>>>,
}

impl Default for ProxyEventObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl ProxyEventObserver {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(1000);
        ProxyEventObserver {
            sender: Arc::new(tx),
        }
    }

    pub async fn add_observer(observer: Arc<dyn ProxyEventObserverTrait + Send + Sync>) {
        let mut reader = PROXY_EVENT_OBSERVER.sender.subscribe();
        rocket::tokio::task::spawn_blocking(move || {
            Handle::current().block_on(async move {
                loop {
                    match reader.recv().await {
                        Ok(event) => {
                            observer.handle_event(&event).await;
                        }
                        Err(e) => {
                            eprintln!("[sfp] event writer dropped... removing reader...: {}", e);
                            break;
                        }
                    }
                }
            });
        });
    }

    pub fn publish_event(event: ProxyEvent) {
        if let Err(e) = PROXY_EVENT_OBSERVER.sender.send(Arc::new(event)) {
            eprintln!("[sfp] Dropping event... Buffer limit hit... {}", e);
        }
    }
}
