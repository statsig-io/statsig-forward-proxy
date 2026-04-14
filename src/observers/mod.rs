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
    servers::{
        authorized_request_context::AuthorizedRequestContext, normalized_path::NormalizedPath,
    },
    utils::compress_encoder::encoding_priority,
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
    ) -> Option<Arc<ConfigSpecForCompany>>;
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ProxyEventType {
    HttpServerRequestSuccess,
    HttpServerRequestFailed,
    HttpDataProviderGotData,
    HttpDataProviderNoData,
    HttpDataProviderNoDataDueToBadLcut,
    HttpDataProviderError,
    DownloadIdListFileSizeBytes,
    DcsFetchSource,
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
            ProxyEventType::DownloadIdListFileSizeBytes => {
                write!(f, "DownloadIdListFileSizeBytes")
            }
            ProxyEventType::DcsFetchSource => write!(f, "DcsFetchSource"),
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
    pub stat: Option<EventStat>,
    pub status_code: Option<u16>,
    pub response_encoding: Option<String>,
    pub sdk_type: Option<String>,
    // `statsig-sdk-version` is semver-like (string), not numeric.
    pub sdk_version: Option<String>,
    pub accept_encoding: Option<String>,
    pub service: Option<String>,
    pub response_payload_type: Option<String>,
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
            stat: None,
            status_code: None,
            response_encoding: None,
            sdk_type: None,
            sdk_version: None,
            accept_encoding: None,
            service: None,
            response_payload_type: None,
        }
    }

    pub fn new(event_type: ProxyEventType) -> ProxyEvent {
        ProxyEvent {
            event_type,
            request_context: None,
            lcut: None,
            stat: None,
            status_code: None,
            response_encoding: None,
            sdk_type: None,
            sdk_version: None,
            accept_encoding: None,
            service: None,
            response_payload_type: None,
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

    pub fn get_id_list_file_id(&self) -> Option<&str> {
        self.request_context.as_ref().and_then(|rc| {
            if rc.path == NormalizedPath::V1DownloadIdListFile {
                rc.file_id.as_deref()
            } else {
                None
            }
        })
    }

    pub fn should_tag_id_list_file_id(&self) -> bool {
        matches!(self.event_type, ProxyEventType::DownloadIdListFileSizeBytes)
    }

    pub fn get_accept_encodings(&self) -> Option<String> {
        if let Some(accept_encoding) = self.accept_encoding.clone() {
            return Some(accept_encoding);
        }

        let rc = self.request_context.as_ref()?;
        if rc.encodings.is_empty() {
            return Some("none".to_string());
        }

        // Avoid `Debug` formatting (spaces/brackets) since those values become metric tags.
        // Keep ordering stable and close to the logger normalization ordering.
        let mut encodings = rc.encodings.clone();
        encodings.sort_by_key(|encoding| encoding_priority(*encoding));
        encodings.dedup();

        Some(
            encodings
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("+"),
        )
    }

    pub fn with_response_encoding(mut self, encoding: String) -> Self {
        let normalized = encoding
            .split(',')
            .map(|part| part.trim())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("+");
        self.response_encoding = Some(normalized);
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

    pub fn with_status_code(mut self, code: u16) -> Self {
        self.status_code = Some(code);
        self
    }

    pub fn with_sdk_type(mut self, sdk_type: String) -> Self {
        self.sdk_type = Some(sdk_type);
        self
    }

    pub fn with_sdk_version(mut self, sdk_version: String) -> Self {
        self.sdk_version = Some(sdk_version);
        self
    }

    pub fn with_accept_encoding(mut self, accept_encoding: String) -> Self {
        self.accept_encoding = Some(accept_encoding);
        self
    }

    pub fn with_service(mut self, service: String) -> Self {
        self.service = Some(service);
        self
    }

    pub fn with_response_payload_type(mut self, payload_type: String) -> Self {
        self.response_payload_type = Some(payload_type);
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
        self.event_type.hash(state);
        if let Some(rc) = &self.request_context {
            rc.sdk_key.hash(state);
            rc.path.hash(state);
        }
        self.lcut.hash(state);
        self.response_encoding.hash(state);
        self.accept_encoding.hash(state);
        self.sdk_type.hash(state);
        self.sdk_version.hash(state);
        self.service.hash(state);
        self.response_payload_type.hash(state);
        self.status_code.hash(state);
    }
}

impl PartialEq for ProxyEvent {
    fn eq(&self, other: &Self) -> bool {
        self.event_type == other.event_type
            && self.request_context == other.request_context
            && self.lcut == other.lcut
            && self.response_encoding == other.response_encoding
            && self.accept_encoding == other.accept_encoding
            && self.sdk_type == other.sdk_type
            && self.sdk_version == other.sdk_version
            && self.service == other.service
            && self.response_payload_type == other.response_payload_type
            && self.status_code == other.status_code
    }
}

impl Eq for ProxyEvent {}
