use async_trait::async_trait;

use parking_lot::RwLock;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::time::Instant;

use crate::{
    observers::{
        http_data_provider_observer::HttpDataProviderObserver, HttpDataProviderObserverTrait,
    },
    servers::authorized_request_context::AuthorizedRequestContext,
};

use once_cell::sync::Lazy;

type RequestBuilderCache = Lazy<Arc<RwLock<HashMap<String, Arc<dyn RequestBuilderTrait>>>>>;

static REQUEST_BUILDERS: RequestBuilderCache = Lazy::new(|| Arc::new(RwLock::new(HashMap::new())));

pub struct CachedRequestBuilders {}

impl CachedRequestBuilders {
    pub fn add_request_builder(path: &str, request_builder: Arc<dyn RequestBuilderTrait>) {
        let mut lock = REQUEST_BUILDERS.write();
        lock.insert(path.to_string(), request_builder);
    }

    pub fn get_request_builder(path: &str) -> Arc<dyn RequestBuilderTrait> {
        let lock = REQUEST_BUILDERS.read();
        lock.get(path).cloned().unwrap_or_else(|| {
            eprintln!("No request builder found for path: {}", path);
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

        http_client.get(url).send().await
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
    ) -> Result<reqwest::Response, reqwest::Error> {
        match http_client
            .post("https://api.statsig.com/v1/get_id_lists".to_string())
            .header("statsig-api-key", request_context.sdk_key.clone())
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
