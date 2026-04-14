pub mod background_data_provider;
pub mod background_poll_dispatch;
mod background_request_interval_tracker;
pub mod dcs_blob_url_generator;
pub mod http_data_provider;
mod request_backoff;
pub mod request_builder;
pub mod startup_warmup_parser;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use http_data_provider::ResponsePayload;
use parking_lot::Mutex;

use crate::{
    servers::authorized_request_context::AuthorizedRequestContext,
    // utils::compress_encoder::CompressionEncoder,
};

use self::request_builder::RequestBuilderTrait;

pub const OUTBOUND_USER_AGENT: &str =
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HttpClientConfig {
    request_timeout: Duration,
    read_timeout: Duration,
    connect_timeout: Duration,
    pool_idle_timeout: Duration,
    pool_max_idle_per_host: usize,
}

impl HttpClientConfig {
    pub fn new(
        request_timeout_in_s: u64,
        read_timeout_in_s: u64,
        connect_timeout_in_s: u64,
        pool_idle_timeout_in_s: u64,
        pool_max_idle_per_host: usize,
    ) -> Self {
        Self {
            request_timeout: Duration::from_secs(request_timeout_in_s),
            read_timeout: Duration::from_secs(read_timeout_in_s),
            connect_timeout: Duration::from_secs(connect_timeout_in_s),
            pool_idle_timeout: Duration::from_secs(pool_idle_timeout_in_s),
            pool_max_idle_per_host,
        }
    }

    pub fn request_timeout(self) -> Duration {
        self.request_timeout
    }

    pub fn read_timeout(self) -> Duration {
        self.read_timeout
    }

    pub fn connect_timeout(self) -> Duration {
        self.connect_timeout
    }

    pub fn pool_max_idle_per_host(self) -> usize {
        self.pool_max_idle_per_host
    }

    pub fn pool_idle_timeout(self) -> Duration {
        self.pool_idle_timeout
    }

    pub fn build_client(self) -> Result<reqwest::Client, reqwest::Error> {
        reqwest::Client::builder()
            .user_agent(OUTBOUND_USER_AGENT)
            .timeout(self.request_timeout)
            .read_timeout(self.read_timeout)
            .connect_timeout(self.connect_timeout)
            .pool_idle_timeout(self.pool_idle_timeout)
            .pool_max_idle_per_host(self.pool_max_idle_per_host)
            .build()
    }

    pub fn background_request_guard_timeout(self) -> Duration {
        let buffered_timeout = self
            .request_timeout
            .checked_add(Duration::from_secs(5))
            .unwrap_or(self.request_timeout);

        buffered_timeout.max(Duration::from_secs(60))
    }
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self::new(30, 10, 10, 30, 10)
    }
}

#[derive(Clone)]
pub struct LazyHttpClient {
    config: HttpClientConfig,
    client: Arc<ArcSwapOption<reqwest::Client>>,
    build_lock: Arc<Mutex<()>>,
}

impl LazyHttpClient {
    pub fn new(config: HttpClientConfig) -> Self {
        Self {
            config,
            client: Arc::new(ArcSwapOption::empty()),
            build_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn config(&self) -> HttpClientConfig {
        self.config
    }

    pub fn get_or_build(&self) -> Result<Arc<reqwest::Client>, reqwest::Error> {
        if let Some(http_client) = self.client.load_full() {
            return Ok(http_client);
        }

        let _build_lock = self.build_lock.lock();
        if let Some(http_client) = self.client.load_full() {
            return Ok(http_client);
        }

        let built_http_client = Arc::new(self.config.build_client()?);
        self.client.store(Some(Arc::clone(&built_http_client)));
        Ok(built_http_client)
    }

    pub fn background_request_guard_timeout(&self) -> Duration {
        self.config.background_request_guard_timeout()
    }

    #[cfg(test)]
    pub fn is_built(&self) -> bool {
        self.client.load_full().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::{HttpClientConfig, LazyHttpClient};
    use std::time::Duration;

    #[test]
    fn background_request_guard_timeout_uses_existing_floor() {
        let config = HttpClientConfig::new(30, 10, 10, 30, 10);

        assert_eq!(
            config.background_request_guard_timeout(),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn background_request_guard_timeout_tracks_larger_request_timeout() {
        let config = HttpClientConfig::new(90, 10, 10, 30, 10);

        assert_eq!(
            config.background_request_guard_timeout(),
            Duration::from_secs(95)
        );
    }

    #[test]
    fn lazy_http_client_builds_on_demand() {
        let http_client = LazyHttpClient::new(HttpClientConfig::default());

        assert!(!http_client.is_built());

        http_client
            .get_or_build()
            .expect("default config should build a reqwest client");

        assert!(http_client.is_built());
    }
}

#[async_trait]
pub trait DataProviderTrait {
    async fn get(
        &self,
        http_client: &reqwest::Client,
        request_builder: &Arc<dyn RequestBuilderTrait>,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
    ) -> DataProviderResult;
}

pub struct FullRequestContext {
    pub authorized_request_context: Arc<AuthorizedRequestContext>,
}

pub struct ResponseContext {
    pub result_type: DataProviderRequestResult,
    pub lcut: u64,
    pub request_since_time: u64,
    pub body: Arc<ResponsePayload>,
}

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum DataProviderRequestResult {
    DataAvailable,
    NoDataAvailable,
    Unauthorized,
    ClientError,
    Error,
}

#[derive(Debug)]
pub struct DataProviderResult {
    result: DataProviderRequestResult,
    // encoding: CompressionEncoder,
    body: Option<Arc<ResponsePayload>>,
    lcut: u64,
}
