use async_trait::async_trait;

use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};
use tokio::time::Duration;

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

        if request_context.use_gzip {
            http_client
                .get(url)
                .header(reqwest::header::ACCEPT_ENCODING, "gzip")
                .timeout(Duration::from_secs(30))
                .send()
                .await
        } else {
            http_client
                .get(url)
                .timeout(Duration::from_secs(30))
                .send()
                .await
        }
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
