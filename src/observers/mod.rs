pub mod http_data_provider_observer;
pub mod proxy_event_observer;

use std::sync::Arc;

use async_trait::async_trait;

use crate::datastore::data_providers::DataProviderRequestResult;

#[async_trait]
pub trait HttpDataProviderObserverTrait {
    fn force_notifier_to_wait_for_update(&self) -> bool;

    async fn update(
        &self,
        result: &DataProviderRequestResult,
        sdk_key: &str,
        lcut: u64,
        data: &Arc<String>,
        path: &str,
    );
    async fn get(&self, key: &str, path: &str) -> Option<Arc<String>>;
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
    pub path: Option<String>,
    pub lcut: Option<u64>,
    pub stat: Option<EventStat>,
}

impl ProxyEvent {
    pub fn new(event_type: ProxyEventType, sdk_key: String) -> ProxyEvent {
        ProxyEvent {
            event_type,
            sdk_key,
            path: None,
            lcut: None,
            stat: None,
        }
    }

    pub fn with_path(mut self, path: String) -> Self {
        self.path = Some(path);
        self
    }

    pub fn with_lcut(mut self, lcut: u64) -> Self {
        self.lcut = Some(lcut);
        self
    }

    pub fn with_stat(mut self, stat: EventStat) -> Self {
        self.stat = Some(stat);
        self
    }
}

#[async_trait]
pub trait ProxyEventObserverTrait {
    async fn handle_event(&self, event: &ProxyEvent);
}
