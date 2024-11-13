use lazy_static::lazy_static;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tokio::{
    runtime::Handle,
    sync::broadcast::{self, Sender},
};

use super::{ProxyEvent, ProxyEventObserverTrait};

#[derive(Deserialize, Clone)]
pub struct EnvConfig {
    pub event_channel_size: Option<usize>,
}

lazy_static! {
    static ref CONFIG: EnvConfig = envy::from_env().expect("Malformed config");
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
        let (tx, _rx) = broadcast::channel(CONFIG.event_channel_size.unwrap_or(100000));
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
                        },
                        Err(RecvError::Closed) => {
                            eprintln!("[sfp] event writer dropped... removing reader...");
                            break;
                        },
                        Err(RecvError::Lagged(frames)) => {
                            eprintln!(
                                "[sfp] event writer lagging by {} messages. Consider increasing EVENT_CHANNEL_SIZE.",
                                frames
                            );
                        },
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
