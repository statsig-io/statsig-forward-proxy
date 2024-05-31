use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{sync::RwLock, time::Instant};

use crate::{
    datastore::sdk_key_store::SdkKeyStore,
    observers::{
        http_data_provider_observer::HttpDataProviderObserver, HttpDataProviderObserverTrait,
    },
};

#[async_trait]
pub trait RequestBuilderTrait: Send + Sync + 'static {
    async fn make_request(
        &self,
        http_client: &reqwest::Client,
        key: &str,
        lcut: u64,
    ) -> Result<reqwest::Response, reqwest::Error>;
    fn get_path(&self) -> String;
    async fn is_an_update(&self, body: &str, sdk_key: &str) -> bool;
    fn get_observers(&self) -> Arc<HttpDataProviderObserver>;
    fn get_backup_cache(&self) -> Arc<dyn HttpDataProviderObserverTrait + Sync + Send>;
    fn get_sdk_key_store(&self) -> Arc<SdkKeyStore>;
    async fn should_make_request(&self) -> bool;
    fn should_check_lcut_before_notifying_observers(&self) -> bool;
}

pub struct DcsRequestBuilder {
    pub base_url: String,
    pub http_observers: Arc<HttpDataProviderObserver>,
    pub backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
    pub sdk_key_store: Arc<SdkKeyStore>,
}

impl DcsRequestBuilder {
    pub fn new(
        base_url: String,
        http_observers: Arc<HttpDataProviderObserver>,
        backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
        sdk_key_store: Arc<SdkKeyStore>,
    ) -> DcsRequestBuilder {
        DcsRequestBuilder {
            base_url,
            http_observers,
            backup_cache,
            sdk_key_store,
        }
    }
}

#[async_trait]
impl RequestBuilderTrait for DcsRequestBuilder {
    async fn make_request(
        &self,
        http_client: &reqwest::Client,
        key: &str,
        lcut: u64,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let url = match lcut == 0 {
            true => format!("{}/v1/download_config_specs/{}.json", self.base_url, key),
            false => format!(
                "{}/v1/download_config_specs/{}.json?sinceTime={}",
                self.base_url, key, lcut
            ),
        };

        http_client.get(url).send().await
    }

    fn get_path(&self) -> String {
        "/v1/download_config_specs".to_string()
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

    fn get_sdk_key_store(&self) -> Arc<SdkKeyStore> {
        Arc::clone(&self.sdk_key_store)
    }

    async fn should_make_request(&self) -> bool {
        true
    }

    fn should_check_lcut_before_notifying_observers(&self) -> bool {
        true
    }
}

pub struct IdlistRequestBuilder {
    pub http_observers: Arc<HttpDataProviderObserver>,
    pub backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
    pub sdk_key_store: Arc<SdkKeyStore>,
    last_request: RwLock<Instant>,
    last_response_hash: RwLock<HashMap<String, String>>,
}

impl IdlistRequestBuilder {
    pub fn new(
        http_observers: Arc<HttpDataProviderObserver>,
        backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
        sdk_key_store: Arc<SdkKeyStore>,
    ) -> IdlistRequestBuilder {
        IdlistRequestBuilder {
            http_observers,
            backup_cache,
            sdk_key_store,
            last_request: RwLock::new(Instant::now()),
            last_response_hash: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl RequestBuilderTrait for IdlistRequestBuilder {
    async fn make_request(
        &self,
        http_client: &reqwest::Client,
        key: &str,
        _lcut: u64,
    ) -> Result<reqwest::Response, reqwest::Error> {
        http_client
            .post("https://api.statsig.com/v1/get_id_lists".to_string())
            .header("statsig-api-key", key)
            .body("{}".to_string())
            .send()
            .await
    }

    fn get_path(&self) -> String {
        "/v1/get_id_lists".to_string()
    }

    async fn is_an_update(&self, body: &str, sdk_key: &str) -> bool {
        let hash = format!("{:x}", Sha256::digest(body));
        let mut wlock = self.last_response_hash.write().await;
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

    fn get_sdk_key_store(&self) -> Arc<SdkKeyStore> {
        Arc::clone(&self.sdk_key_store)
    }

    async fn should_make_request(&self) -> bool {
        let should_make_request =
            self.last_request.read().await.elapsed() > Duration::from_secs(60);
        // TODO: Make configurable, but for now, match sdk interval
        if should_make_request {
            *self.last_request.write().await = Instant::now();
            return true;
        }
        false
    }

    fn should_check_lcut_before_notifying_observers(&self) -> bool {
        false
    }
}
