use async_trait::async_trait;

use cached::proc_macro::once;
use parking_lot::RwLock;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};
use tokio::time::{Duration, Instant};

use crate::{
    observers::{
        http_data_provider_observer::HttpDataProviderObserver, HttpDataProviderObserverTrait,
    },
    servers::{
        authorized_request_context::AuthorizedRequestContext, normalized_path::NormalizedPath,
    },
    utils::compress_encoder::CompressionEncoder,
};

use once_cell::sync::Lazy;

type RequestBuilderCache = Lazy<Arc<RwLock<HashMap<NormalizedPath, Arc<dyn RequestBuilderTrait>>>>>;

static REQUEST_BUILDERS: RequestBuilderCache = Lazy::new(|| Arc::new(RwLock::new(HashMap::new())));

pub struct CachedRequestBuilders {}

impl CachedRequestBuilders {
    pub fn add_request_builder(
        path: NormalizedPath,
        request_builder: Arc<dyn RequestBuilderTrait>,
    ) {
        let mut lock = REQUEST_BUILDERS.write();
        lock.insert(path, request_builder);
    }

    pub fn get_request_builder(path: &NormalizedPath) -> Arc<dyn RequestBuilderTrait> {
        let lock = REQUEST_BUILDERS.read();
        lock.get(path).cloned().unwrap_or_else(|| {
            eprintln!("No request builder found for path: {}", path.as_str());
            Arc::new(NoopRequestBuilder {})
        })
    }
}

#[async_trait]
pub trait RequestBuilderTrait: Send + Sync + 'static {
    async fn make_request(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
        zstd_dict_id: &Option<Arc<str>>,
    ) -> Result<reqwest::Response, reqwest::Error>;
    async fn is_an_update(&self, body: &str, sdk_key: &str) -> bool;
    fn get_observers(&self) -> Arc<HttpDataProviderObserver>;
    fn get_backup_cache(&self) -> Arc<dyn HttpDataProviderObserverTrait + Sync + Send>;
    async fn should_make_request(&self, rc: &Arc<AuthorizedRequestContext>) -> bool;
}

pub struct NoopRequestBuilder {}

#[async_trait]
impl RequestBuilderTrait for NoopRequestBuilder {
    async fn make_request(
        &self,
        _http_client: &reqwest::Client,
        _request_context: &Arc<AuthorizedRequestContext>,
        _lcut: u64,
        _zstd_dict_id: &Option<Arc<str>>,
    ) -> Result<reqwest::Response, reqwest::Error> {
        unimplemented!()
    }

    async fn is_an_update(&self, _body: &str, _sdk_key: &str) -> bool {
        unimplemented!()
    }

    fn get_observers(&self) -> Arc<HttpDataProviderObserver> {
        unimplemented!()
    }

    fn get_backup_cache(&self) -> Arc<dyn HttpDataProviderObserverTrait + Sync + Send> {
        unimplemented!()
    }

    async fn should_make_request(&self, _rc: &Arc<AuthorizedRequestContext>) -> bool {
        false
    }
}

pub struct DcsRequestBuilder {
    pub base_url: String,
    pub http_observers: Arc<HttpDataProviderObserver>,
    pub backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
}

impl DcsRequestBuilder {
    pub fn new(
        base_url: String,
        http_observers: Arc<HttpDataProviderObserver>,
        backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
    ) -> DcsRequestBuilder {
        DcsRequestBuilder {
            base_url,
            http_observers,
            backup_cache,
        }
    }
}

#[once]
fn get_package_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[async_trait]
impl RequestBuilderTrait for DcsRequestBuilder {
    async fn make_request(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
        _zstd_dict_id: &Option<Arc<str>>,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let url = match lcut == 0 {
            true => format!(
                "{}{}{}.json",
                self.base_url, request_context.path, request_context.sdk_key
            ),
            false => format!(
                "{}{}{}.json?sinceTime={}",
                self.base_url, request_context.path, request_context.sdk_key, lcut
            ),
        };

        let mut request = http_client
            .get(url)
            .header("x-sfp-version", get_package_version())
            .timeout(Duration::from_secs(30));

        if request_context.encoding != CompressionEncoder::PlainText {
            request = request.header(
                reqwest::header::ACCEPT_ENCODING,
                request_context.encoding.to_string(),
            );
        }

        request.send().await
    }

    async fn is_an_update(&self, body: &str, _sdk_key: &str) -> bool {
        // TODO: This should be more robust
        !body.eq("{\"has_updates\":false}")
    }

    fn get_observers(&self) -> Arc<HttpDataProviderObserver> {
        Arc::clone(&self.http_observers)
    }

    fn get_backup_cache(&self) -> Arc<dyn HttpDataProviderObserverTrait + Sync + Send> {
        Arc::clone(&self.backup_cache)
    }

    async fn should_make_request(&self, _rc: &Arc<AuthorizedRequestContext>) -> bool {
        true
    }
}

pub struct SharedDictDcsRequestBuilder {
    pub base_url: String,
    pub http_observers: Arc<HttpDataProviderObserver>,
    pub backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
}

impl SharedDictDcsRequestBuilder {
    pub fn new(
        base_url: String,
        http_observers: Arc<HttpDataProviderObserver>,
        backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
    ) -> SharedDictDcsRequestBuilder {
        SharedDictDcsRequestBuilder {
            base_url,
            http_observers,
            backup_cache,
        }
    }
}

#[async_trait]
impl RequestBuilderTrait for SharedDictDcsRequestBuilder {
    async fn make_request(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
        zstd_dict_id: &Option<Arc<str>>,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut url = match zstd_dict_id {
            Some(dict_id) => format!(
                "{}{}{}/{}.json",
                self.base_url, request_context.path, dict_id, request_context.sdk_key
            ),
            None => format!(
                "{}{}{}/{}.json",
                self.base_url, request_context.path, "null", request_context.sdk_key
            ),
        };

        if lcut != 0 {
            url.push_str(&format!("?sinceTime={}", lcut));
        }

        let mut request = http_client
            .get(url)
            .header("x-sfp-version", get_package_version())
            .timeout(Duration::from_secs(30));

        if request_context.encoding != CompressionEncoder::PlainText {
            request = request.header(
                reqwest::header::ACCEPT_ENCODING,
                request_context.encoding.to_string(),
            );
        }

        request.send().await
    }

    async fn is_an_update(&self, _body: &str, _sdk_key: &str) -> bool {
        // TODO: classify responses based on response header
        true
    }

    fn get_observers(&self) -> Arc<HttpDataProviderObserver> {
        Arc::clone(&self.http_observers)
    }

    fn get_backup_cache(&self) -> Arc<dyn HttpDataProviderObserverTrait + Sync + Send> {
        Arc::clone(&self.backup_cache)
    }

    async fn should_make_request(&self, _rc: &Arc<AuthorizedRequestContext>) -> bool {
        true
    }
}

pub struct IdlistRequestBuilder {
    pub http_observers: Arc<HttpDataProviderObserver>,
    pub backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
    last_request_by_key: RwLock<HashMap<String, Instant>>,
    last_response_hash: RwLock<HashMap<String, String>>,
}

impl IdlistRequestBuilder {
    pub fn new(
        http_observers: Arc<HttpDataProviderObserver>,
        backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
    ) -> IdlistRequestBuilder {
        IdlistRequestBuilder {
            http_observers,
            backup_cache,
            last_request_by_key: RwLock::new(HashMap::new()),
            last_response_hash: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl RequestBuilderTrait for IdlistRequestBuilder {
    async fn make_request(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
        _lcut: u64,
        _zstd_dict_id: &Option<Arc<str>>,
    ) -> Result<reqwest::Response, reqwest::Error> {
        match http_client
            .post("https://api.statsig.com/v1/get_id_lists".to_string())
            .header("x-sfp-version", get_package_version())
            .header("statsig-api-key", request_context.sdk_key.clone())
            .timeout(Duration::from_secs(30))
            .body("{}".to_string())
            .send()
            .await
        {
            Ok(response) => {
                let status_code = response.status().as_u16();
                // If unauthorized, remove key from last response hash such that
                // we will reload data into memory if for some reason the key is
                // re-authorized
                if status_code == 401 || status_code == 403 {
                    self.last_response_hash
                        .write()
                        .remove(&request_context.sdk_key);
                }
                Ok(response)
            }
            Err(e) => Err(e),
        }
    }

    async fn is_an_update(&self, body: &str, sdk_key: &str) -> bool {
        let hash = format!("{:x}", Sha256::digest(body));
        let mut wlock = self.last_response_hash.write();
        let mut is_an_update = true;
        if let Some(old_hash) = wlock.get(sdk_key) {
            is_an_update = hash != *old_hash;
        }

        if is_an_update {
            wlock.insert(sdk_key.to_string(), hash);
        }

        is_an_update
    }

    fn get_observers(&self) -> Arc<HttpDataProviderObserver> {
        Arc::clone(&self.http_observers)
    }

    fn get_backup_cache(&self) -> Arc<dyn HttpDataProviderObserverTrait + Sync + Send> {
        Arc::clone(&self.backup_cache)
    }

    async fn should_make_request(&self, rc: &Arc<AuthorizedRequestContext>) -> bool {
        let mut wlock = self.last_request_by_key.write();
        let key = rc.to_string();
        match wlock.get_mut(&key) {
            Some(last_request) => {
                if last_request.elapsed() > Duration::from_secs(60) {
                    wlock.insert(key, Instant::now());
                    return true;
                }

                return false;
            }
            None => {
                wlock.insert(key, Instant::now());
                return true;
            }
        }
    }
}
