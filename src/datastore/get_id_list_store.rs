use super::data_providers::background_data_provider::{foreground_fetch, BackgroundDataProvider};
use super::data_providers::DataProviderRequestResult;
use super::sdk_key_store::SdkKeyStore;
use crate::observers::HttpDataProviderObserverTrait;
use crate::servers::http_server::AuthorizedRequestContext;
use std::collections::HashMap;

use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone, Debug)]
pub struct IdlistForCompany {
    pub idlists: Arc<String>,
}

pub struct GetIdListStore {
    store: Arc<RwLock<HashMap<AuthorizedRequestContext, Arc<RwLock<IdlistForCompany>>>>>,
    sdk_key_store: Arc<SdkKeyStore>,
    background_data_provider: Arc<BackgroundDataProvider>,
}

use async_trait::async_trait;
#[async_trait]
impl HttpDataProviderObserverTrait for GetIdListStore {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        true
    }

    async fn update(
        &self,
        result: &DataProviderRequestResult,
        request_context: &AuthorizedRequestContext,
        _lcut: u64,
        data: &Arc<String>,
    ) {
        if result == &DataProviderRequestResult::Error
            || result == &DataProviderRequestResult::DataAvailable
        {
            self.store.write().await.insert(
                request_context.clone(),
                Arc::new(RwLock::new(IdlistForCompany {
                    idlists: data.clone(),
                })),
            );
        } else if result == &DataProviderRequestResult::Unauthorized {
            let contains_key = self.store.read().await.contains_key(request_context);
            if contains_key {
                self.store.write().await.remove(request_context);
            }
        }
    }

    async fn get(&self, request_context: &AuthorizedRequestContext) -> Option<Arc<String>> {
        match self.store.read().await.get(request_context) {
            Some(record) => Some(record.read().await.idlists.clone()),
            None => None,
        }
    }
}

impl GetIdListStore {
    pub fn new(
        sdk_key_store: Arc<SdkKeyStore>,
        background_data_provider: Arc<BackgroundDataProvider>,
    ) -> Self {
        GetIdListStore {
            store: Arc::new(RwLock::new(HashMap::new())),
            sdk_key_store,
            background_data_provider,
        }
    }

    pub async fn get_id_lists(
        &self,
        request_context: &AuthorizedRequestContext,
    ) -> Option<Arc<RwLock<IdlistForCompany>>> {
        if !self.sdk_key_store.has_key(request_context).await {
            // Since it's a cache-miss, just fill with a full payload
            // and check if we should return no update manually
            foreground_fetch(
                self.background_data_provider.clone(),
                request_context,
                0,
                self.sdk_key_store.clone(),
            )
            .await;
        }

        self.store.read().await.get(request_context).cloned()
    }
}
