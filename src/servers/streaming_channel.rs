use crate::datastore::data_providers::DataProviderRequestResult;
use crate::observers::HttpDataProviderObserverTrait;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::broadcast::Sender;
use tokio::sync::RwLock;

use super::grpc_server::statsig_forward_proxy::ConfigSpecResponse;
use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::EventStat;
use crate::observers::OperationType;
use crate::observers::{ProxyEvent, ProxyEventType};

pub struct StreamingChannel {
    key: String,
    last_updated: Arc<RwLock<u64>>,
    pub sender: Arc<RwLock<Sender<Option<ConfigSpecResponse>>>>,
}

impl StreamingChannel {
    pub fn new(key: &str) -> Self {
        let (tx, _rx) = broadcast::channel(1);
        StreamingChannel {
            key: key.to_string(),
            last_updated: Arc::new(RwLock::new(0)),
            sender: Arc::new(RwLock::new(tx)),
        }
    }
}

#[async_trait]
impl HttpDataProviderObserverTrait for StreamingChannel {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        false
    }

    async fn update(
        &self,
        result: &DataProviderRequestResult,
        key: &str,
        lcut: u64,
        data: &Arc<String>,
        path: &str,
    ) {
        let mut wlock = self.last_updated.write().await;
        let is_newer_lcut = lcut > *wlock;
        if is_newer_lcut && self.key == key && result == &DataProviderRequestResult::DataAvailable {
            ProxyEventObserver::publish_event(
                ProxyEvent::new(ProxyEventType::StreamingChannelGotNewData, key.to_string())
                    .with_path(path.to_string())
                    .with_stat(EventStat {
                        operation_type: OperationType::IncrByValue,
                        value: 1,
                    }),
            )
            .await;
            if self
                .sender
                .write()
                .await
                .send(Some(ConfigSpecResponse {
                    spec: data.to_string(),
                    last_updated: lcut,
                }))
                .is_err()
            {
                // TODO: Optimize code, no receivers are listening
                //       so we should consider removing ourselves
                //       as a dcs observer
            } else {
                *wlock = lcut;
            }
        }
    }

    async fn get(&self, _key: &str, _path: &str) -> Option<Arc<String>> {
        unimplemented!("Not Used")
    }
}
