use super::data_providers::background_data_provider::{foreground_fetch, BackgroundDataProvider};
use super::data_providers::http_data_provider::ResponsePayload;
use super::data_providers::DataProviderRequestResult;
use super::sdk_key_store::SdkKeyStore;
use crate::observers::HttpDataProviderObserverTrait;
use crate::servers::authorized_request_context::AuthorizedRequestContext;
use dashmap::DashMap;

use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct IdlistForCompany {
    pub idlists: Arc<ResponsePayload>,
}

pub struct GetIdListStore {
    store: Arc<DashMap<Arc<AuthorizedRequestContext>, Arc<IdlistForCompany>>>,
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
        request_context: &Arc<AuthorizedRequestContext>,
        _lcut: u64,
        data: &Arc<ResponsePayload>,
    ) {
        if result == &DataProviderRequestResult::Error
            || result == &DataProviderRequestResult::DataAvailable
        {
            self.store.insert(
                Arc::clone(request_context),
                Arc::new(IdlistForCompany {
                    idlists: data.clone(),
                }),
            );
        } else if result == &DataProviderRequestResult::Unauthorized {
            self.store.remove(request_context);
        }
    }

    async fn get(
        &self,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<Arc<ResponsePayload>> {
        self.store
            .get(request_context)
            .map(|record| record.idlists.clone())
    }
}

impl GetIdListStore {
    pub fn new(
        sdk_key_store: Arc<SdkKeyStore>,
        background_data_provider: Arc<BackgroundDataProvider>,
    ) -> Self {
        GetIdListStore {
            store: Arc::new(DashMap::new()),
            sdk_key_store,
            background_data_provider,
        }
    }

    pub async fn get_id_lists(
        &self,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<Arc<IdlistForCompany>> {
        if !self.sdk_key_store.has_key(request_context) {
            // Since it's a cache-miss, just fill with a full payload
            // and check if we should return no update manually
            foreground_fetch(self.background_data_provider.clone(), request_context, 0).await;
        }

        self.store.get(request_context).map(|r| r.clone())
    }
}
