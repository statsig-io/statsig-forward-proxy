pub mod http_data_provider_observer;
pub mod proxy_event_observer;

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    datastore::data_providers::DataProviderRequestResult,
    servers::http_server::AuthorizedRequestContext,
};

#[async_trait]
pub trait HttpDataProviderObserverTrait {
    fn force_notifier_to_wait_for_update(&self) -> bool;

    async fn update(
        &self,
        result: &DataProviderRequestResult,
        request_context: &AuthorizedRequestContext,
        lcut: u64,
        data: &Arc<String>,
    );
    async fn get(&self, request_context: &AuthorizedRequestContext) -> Option<Arc<String>>;
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum ProxyEventType {
    HttpServerRequestSuccess,
    HttpServerRequestFailed,
    HttpDataProviderGotData,
    HttpDataProviderNoData,
    HttpDataProviderNoDataDueToBadLcut,
    HttpDataProviderError,
    RedisCacheWriteSucceed,
    RedisCacheWriteFailed,
    RedisCacheReadSucceed,
    RedisCacheReadMiss,
    RedisCacheWriteSkipped,
    RedisCacheDeleteSucceed,
    RedisCacheDeleteFailed,
    RedisCacheReadFailed,
    InMemoryCacheWriteSucceed,
    InMemoryCacheWriteSkipped,
    InMemoryCacheReadSucceed,
    SdkKeyStoreCacheMiss,
    SdkKeyStoreCacheHit,
    ConfigSpecStoreGotData,
    ConfigSpecStoreGotNoData,
    GrpcStreamingStreamedInitialized,
    GrpcStreamingStreamedResponse,
    GrpcStreamingStreamDisconnected,
    StreamingChannelGotNewData,
    UpdateConfigSpecStorePropagationDelayMs,
}

#[derive(Clone, PartialEq, Debug, Copy)]
pub enum OperationType {
    #[allow(dead_code)]
    Distribution,
    #[allow(dead_code)]
    Gauge,
    IncrByValue,
}

#[derive(Debug)]
pub struct EventStat {
    pub operation_type: OperationType,
    pub value: i64,
}

pub struct ProxyEvent {
    pub event_type: ProxyEventType,
    pub sdk_key: String,
    pub path: String,
    pub lcut: Option<u64>,
    pub stat: Option<EventStat>,
    pub status_code: Option<u16>,
}

impl ProxyEvent {
    pub fn new(
        event_type: ProxyEventType,
        request_context: &AuthorizedRequestContext,
    ) -> ProxyEvent {
        ProxyEvent {
            event_type,
            sdk_key: request_context.sdk_key.clone(),
            path: request_context.path.clone(),
            lcut: None,
            stat: None,
            status_code: None
        }
    }

    pub fn with_lcut(mut self, lcut: u64) -> Self {
        self.lcut = Some(lcut);
        self
    }

    pub fn with_stat(mut self, stat: EventStat) -> Self {
        self.stat = Some(stat);
        self
    }

    pub fn with_status_code(mut self,code: u16) -> Self {
        self.status_code = Some(code);
        self
    }
}

#[async_trait]
pub trait ProxyEventObserverTrait {
    async fn handle_event(&self, event: &ProxyEvent);
}
