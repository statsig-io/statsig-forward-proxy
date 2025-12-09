use async_trait::async_trait;

use cached::proc_macro::once;
use parking_lot::RwLock;
use reqwest::Version;
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
    utils::compress_encoder::{format_compression_encodings, CompressionEncoder},
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
    ) -> Result<reqwest::Response, reqwest::Error>;
    async fn is_an_update(
        &self,
        body: &[u8],
        headers: &reqwest::header::HeaderMap,
        rc: &Arc<AuthorizedRequestContext>,
    ) -> bool;
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
    ) -> Result<reqwest::Response, reqwest::Error> {
        unimplemented!()
    }

    async fn is_an_update(
        &self,
        _body: &[u8],
        _headers: &reqwest::header::HeaderMap,
        _rc: &Arc<AuthorizedRequestContext>,
    ) -> bool {
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
            .version(Version::HTTP_2)
            .header("x-sfp-version", get_package_version())
            .timeout(Duration::from_secs(30));

        if request_context.encodings != vec![CompressionEncoder::PlainText] {
            request = request.header(
                reqwest::header::ACCEPT_ENCODING,
                format_compression_encodings(&request_context.encodings),
            );
        }

        request.send().await
    }

    async fn is_an_update(
        &self,
        _body: &[u8],
        headers: &reqwest::header::HeaderMap,
        _rc: &Arc<AuthorizedRequestContext>,
    ) -> bool {
        // If this header is not present, we default to "Yes, this is an update"
        // Otherwise, this header will be "true" if this is NOT an update.
        headers
            .get("x-cache-hit")
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| value == "false")
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
    last_request_by_key: RwLock<HashMap<Arc<AuthorizedRequestContext>, Instant>>,
    last_response_hash: RwLock<HashMap<Arc<AuthorizedRequestContext>, String>>,
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
    ) -> Result<reqwest::Response, reqwest::Error> {
        match http_client
            .post("https://api.statsig.com/v1/get_id_lists".to_string())
            .header("x-sfp-version", get_package_version())
            .header("statsig-api-key", request_context.sdk_key.clone())
            .timeout(Duration::from_secs(30))
            .body("{}".to_string())
            .version(Version::HTTP_2)
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
                        .remove(Arc::as_ref(request_context));
                }
                Ok(response)
            }
            Err(e) => Err(e),
        }
    }

    async fn is_an_update(
        &self,
        body: &[u8],
        _headers: &reqwest::header::HeaderMap,
        rc: &Arc<AuthorizedRequestContext>,
    ) -> bool {
        let hash = format!("{:x}", Sha256::digest(body));
        let mut wlock = self.last_response_hash.write();
        let mut is_an_update = true;
        if let Some(old_hash) = wlock.get(rc) {
            is_an_update = hash != *old_hash;
        }

        if is_an_update {
            wlock.insert(Arc::clone(rc), hash);
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
        match wlock.get_mut(rc) {
            Some(last_request) => {
                if last_request.elapsed() > Duration::from_secs(60) {
                    wlock.insert(Arc::clone(rc), Instant::now());
                    return true;
                }

                return false;
            }
            None => {
                wlock.insert(Arc::clone(rc), Instant::now());
                return true;
            }
        }
    }
}
