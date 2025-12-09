use crate::datastore::config_spec_store::ConfigSpecForCompany;
use crate::datastore::data_providers::http_data_provider::ResponsePayload;
use crate::datastore::data_providers::DataProviderRequestResult;
use crate::datastore::data_providers::FullRequestContext;
use crate::datastore::data_providers::ResponseContext;
use crate::observers::HttpDataProviderObserverTrait;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::broadcast::Sender;
use tokio::sync::RwLock;

use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::EventStat;
use crate::observers::OperationType;
use crate::observers::{ProxyEvent, ProxyEventType};
use crate::servers::authorized_request_context::AuthorizedRequestContext;

pub struct StreamingChannel {
    request_context: Arc<AuthorizedRequestContext>,
    last_updated: Arc<RwLock<u64>>,
    pub sender: Arc<RwLock<Sender<Option<(Arc<ResponsePayload>, u64)>>>>,
}

impl StreamingChannel {
    pub fn new(request_context: Arc<AuthorizedRequestContext>) -> Self {
        let (tx, _rx) = broadcast::channel(1);
        StreamingChannel {
            request_context,
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
        request_context: &Arc<FullRequestContext>,
        response_context: &Arc<ResponseContext>,
    ) {
        let mut wlock = self.last_updated.write().await;
        let is_newer_lcut = response_context.lcut > *wlock;
        if is_newer_lcut
            && self.request_context == request_context.authorized_request_context
            && response_context.result_type == DataProviderRequestResult::DataAvailable
        {
            ProxyEventObserver::publish_event(
                ProxyEvent::new_with_rc(
                    ProxyEventType::StreamingChannelGotNewData,
                    &request_context.authorized_request_context,
                )
                .with_stat(EventStat {
                    operation_type: OperationType::IncrByValue,
                    value: 1,
                }),
            );
            if self
                .sender
                .write()
                .await
                .send(Some((
                    Arc::clone(&response_context.body),
                    response_context.lcut,
                )))
                .is_err()
            {
                // TODO: Optimize code, no receivers are listening
                //       so we should consider removing ourselves
                //       as a dcs observer
            } else {
                *wlock = response_context.lcut;
            }
        }
    }

    async fn get(
        &self,
        _request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<Arc<ConfigSpecForCompany>> {
        unimplemented!("Not Used")
    }
}
