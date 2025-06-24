pub mod http_data_provider_observer;
pub mod proxy_event_observer;

use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use once_cell::sync::Lazy;

use crate::{
    datastore::{
        config_spec_store::ConfigSpecForCompany,
        data_providers::{FullRequestContext, ResponseContext},
    },
    servers::authorized_request_context::AuthorizedRequestContext,
};

#[async_trait]
pub trait HttpDataProviderObserverTrait {
    fn force_notifier_to_wait_for_update(&self) -> bool;

    async fn update(
        &self,
        request_context: &Arc<FullRequestContext>,
        response_context: &Arc<ResponseContext>,
    );
    async fn get(
        &self,
        request_context: &Arc<AuthorizedRequestContext>,
        zstd_dict_id: &Option<Arc<str>>,
    ) -> Option<Arc<ConfigSpecForCompany>>;
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
    ConfigSpecStoreGotData,
    GrpcStreamingStreamedInitialized,
    GrpcStreamingStreamedResponse,
    GrpcStreamingStreamUnauthorized,
    GrpcStreamingHealthcheckSent,
    GrpcStreamingStreamDisconnected,
    GrpcEstimatedActiveStreams,
    StreamingChannelGotNewData,
    UpdateConfigSpecStorePropagationDelayMs,
    LogEventStoreDeduped,
    LogEventStoreDedupeCacheCleared,
    NginxCacheBytesUsed,
    NginxCacheBytesLimit,
}

impl std::fmt::Display for ProxyEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ProxyEventType::HttpServerRequestSuccess => write!(f, "HttpServerRequestSuccess"),
            ProxyEventType::HttpServerRequestFailed => write!(f, "HttpServerRequestFailed"),
            ProxyEventType::HttpDataProviderGotData => write!(f, "HttpDataProviderGotData"),
            ProxyEventType::HttpDataProviderNoData => write!(f, "HttpDataProviderNoData"),
            ProxyEventType::HttpDataProviderNoDataDueToBadLcut => {
                write!(f, "HttpDataProviderNoDataDueToBadLcut")
            }
            ProxyEventType::HttpDataProviderError => write!(f, "HttpDataProviderError"),
            ProxyEventType::RedisCacheWriteSucceed => write!(f, "RedisCacheWriteSucceed"),
            ProxyEventType::RedisCacheWriteFailed => write!(f, "RedisCacheWriteFailed"),
            ProxyEventType::RedisCacheReadSucceed => write!(f, "RedisCacheReadSucceed"),
            ProxyEventType::RedisCacheReadMiss => write!(f, "RedisCacheReadMiss"),
            ProxyEventType::RedisCacheWriteSkipped => write!(f, "RedisCacheWriteSkipped"),
            ProxyEventType::RedisCacheDeleteSucceed => write!(f, "RedisCacheDeleteSucceed"),
            ProxyEventType::RedisCacheDeleteFailed => write!(f, "RedisCacheDeleteFailed"),
            ProxyEventType::RedisCacheReadFailed => write!(f, "RedisCacheReadFailed"),
            ProxyEventType::InMemoryCacheWriteSucceed => write!(f, "InMemoryCacheWriteSucceed"),
            ProxyEventType::InMemoryCacheWriteSkipped => write!(f, "InMemoryCacheWriteSkipped"),
            ProxyEventType::InMemoryCacheReadSucceed => write!(f, "InMemoryCacheReadSucceed"),
            ProxyEventType::ConfigSpecStoreGotData => write!(f, "ConfigSpecStoreGotData"),
            ProxyEventType::GrpcStreamingStreamedInitialized => {
                write!(f, "GrpcStreamingStreamedInitialized")
            }
            ProxyEventType::GrpcStreamingStreamUnauthorized => {
                write!(f, "GrpcStreamingStreamUnauthorized")
            }
            ProxyEventType::GrpcStreamingStreamedResponse => {
                write!(f, "GrpcStreamingStreamedResponse")
            }
            ProxyEventType::GrpcStreamingHealthcheckSent => {
                write!(f, "GrpcStreamingHealthcheckSent")
            }
            ProxyEventType::GrpcEstimatedActiveStreams => {
                write!(f, "GrpcEstimatedActiveStreams")
            }
            ProxyEventType::GrpcStreamingStreamDisconnected => {
                write!(f, "GrpcStreamingStreamDisconnected")
            }
            ProxyEventType::StreamingChannelGotNewData => write!(f, "StreamingChannelGotNewData"),
            ProxyEventType::UpdateConfigSpecStorePropagationDelayMs => {
                write!(f, "UpdateConfigSpecStorePropagationDelayMs")
            }
            ProxyEventType::LogEventStoreDeduped => write!(f, "LogEventStoreDeduped"),
            ProxyEventType::LogEventStoreDedupeCacheCleared => {
                write!(f, "LogEventStoreDedupeCacheCleared")
            }
            ProxyEventType::NginxCacheBytesUsed => write!(f, "NginxCacheBytesUsed"),
            ProxyEventType::NginxCacheBytesLimit => write!(f, "NginxCacheBytesLimit"),
        }
    }
}

#[derive(Clone, PartialEq, Debug, Copy, Hash)]
pub enum OperationType {
    Distribution,
    Timing,
    Gauge,
    IncrByValue,
}

#[derive(Debug, Clone)]
pub struct EventStat {
    pub operation_type: OperationType,
    pub value: i64,
}

#[derive(Clone, Debug)]
pub struct ProxyEvent {
    pub event_type: ProxyEventType,
    request_context: Option<Arc<AuthorizedRequestContext>>,
    pub lcut: Option<u64>,
    pub zstd_dict_id: Option<Arc<str>>,
    pub stat: Option<EventStat>,
    pub status_code: Option<u16>,
}

impl ProxyEvent {
    pub fn new_with_rc(
        event_type: ProxyEventType,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> ProxyEvent {
        ProxyEvent {
            event_type,
            request_context: Some(Arc::clone(request_context)),
            lcut: None,
            zstd_dict_id: None,
            stat: None,
            status_code: None,
        }
    }

    pub fn new(event_type: ProxyEventType) -> ProxyEvent {
        ProxyEvent {
            event_type,
            request_context: None,
            lcut: None,
            zstd_dict_id: None,
            stat: None,
            status_code: None,
        }
    }

    pub fn get_sdk_key(&self) -> Option<Arc<str>> {
        self.request_context.as_ref().map(|rc| {
            let cache = SDK_KEY_CACHE.read();
            if let Some(cached_key) = cache.get(&rc.sdk_key) {
                cached_key.clone()
            } else {
                drop(cache); // Release the read lock
                let mut cache = SDK_KEY_CACHE.write();
                // Check again in case another thread inserted the key
                if let Some(cached_key) = cache.get(&rc.sdk_key) {
                    cached_key.clone()
                } else {
                    let new_key: Arc<str> = if rc.sdk_key.len() > 20 {
                        let mut truncated = rc.sdk_key[..20].to_string();
                        truncated.push_str("***");
                        Arc::from(truncated)
                    } else {
                        Arc::from(rc.sdk_key.as_str())
                    };
                    cache.insert(rc.sdk_key.to_string(), new_key.clone());
                    new_key
                }
            }
        })
    }

    pub fn get_path(&self) -> Option<&str> {
        self.request_context.as_ref().map(|rc| rc.path.as_str())
    }

    pub fn with_lcut(mut self, lcut: u64) -> Self {
        self.lcut = Some(lcut);
        self
    }

    pub fn with_zstd_dict_id(mut self, zstd_dict_id: Option<Arc<str>>) -> Self {
        self.zstd_dict_id = zstd_dict_id;
        self
    }

    pub fn with_stat(mut self, stat: EventStat) -> Self {
        self.stat = Some(stat);
        self
    }

    pub fn with_status_code(mut self, code: u16) -> Self {
        self.status_code = Some(code);
        self
    }
}

static SDK_KEY_CACHE: Lazy<RwLock<HashMap<String, Arc<str>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

#[async_trait]
pub trait ProxyEventObserverTrait {
    async fn handle_event(&self, event: &ProxyEvent);
}

use std::hash::{Hash, Hasher};

impl Hash for ProxyEvent {
    fn hash<H: Hasher>(&self, state: &mut H) {
        if let Some(rc) = &self.request_context {
            rc.sdk_key.hash(state);
            rc.path.hash(state);
        }
        self.lcut.hash(state);
        self.zstd_dict_id.hash(state);
        self.status_code.hash(state);
    }
}

impl PartialEq for ProxyEvent {
    fn eq(&self, other: &Self) -> bool {
        self.request_context == other.request_context
            && self.lcut == other.lcut
            && self.status_code == other.status_code
            && self.zstd_dict_id == other.zstd_dict_id
    }
}

impl Eq for ProxyEvent {}
