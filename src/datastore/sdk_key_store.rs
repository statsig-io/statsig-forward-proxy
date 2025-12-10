use super::config_spec_store::ConfigSpecForCompany;
use super::data_providers::{DataProviderRequestResult, FullRequestContext, ResponseContext};
use crate::observers::HttpDataProviderObserverTrait;
use crate::servers::authorized_request_context::AuthorizedRequestContext;
use async_trait::async_trait;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

#[async_trait]
impl HttpDataProviderObserverTrait for SdkKeyStore {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        true
    }

    async fn update(
        &self,
        request_context: &Arc<FullRequestContext>,
        response_context: &Arc<ResponseContext>,
    ) {
        match response_context.result_type {
            DataProviderRequestResult::DataAvailable => {
                if !request_context.authorized_request_context.use_lcut
                    || self
                        .keystore
                        .read()
                        .get(&request_context.authorized_request_context)
                        .map_or(true, |v| *v < response_context.lcut)
                {
                    self.keystore.write().insert(
                        Arc::clone(&request_context.authorized_request_context),
                        response_context.lcut,
                    );
                }
            }
            DataProviderRequestResult::Unauthorized => {
                self.keystore
                    .write()
                    .remove(&request_context.authorized_request_context);
            }
            _ => {}
        }
    }

    async fn get(
        &self,
        _request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<Arc<ConfigSpecForCompany>> {
        None
    }
}

#[derive(Clone, Debug)]
pub struct SdkKeyStoreItem {
    pub request_context: Arc<AuthorizedRequestContext>,
    pub lcut: u64,
}

type SdkKeyStoreValue = u64; // LCUT
pub struct SdkKeyStore {
    keystore: Arc<RwLock<HashMap<Arc<AuthorizedRequestContext>, SdkKeyStoreValue>>>,
}

impl Default for SdkKeyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SdkKeyStore {
    pub fn new() -> SdkKeyStore {
        SdkKeyStore {
            keystore: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn has_key(&self, request_context: &Arc<AuthorizedRequestContext>) -> bool {
        self.keystore.read().contains_key(request_context)
    }

    pub fn get_registered_store(&self) -> Vec<SdkKeyStoreItem> {
        self.keystore
            .read()
            .iter()
            .map(|(key, value)| SdkKeyStoreItem {
                request_context: key.clone(),
                lcut: *value,
            })
            .collect()
    }
}
