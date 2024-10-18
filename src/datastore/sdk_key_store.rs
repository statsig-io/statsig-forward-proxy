use super::data_providers::http_data_provider::ResponsePayload;
use super::data_providers::DataProviderRequestResult;
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
        result: &DataProviderRequestResult,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
        _data: &Arc<ResponsePayload>,
    ) {
        match result {
            DataProviderRequestResult::DataAvailable => {
                if !request_context.use_lcut
                    || self
                        .keystore
                        .read()
                        .get(request_context)
                        .map_or(true, |&v| v < lcut)
                {
                    self.keystore
                        .write()
                        .insert(Arc::clone(request_context), lcut);
                }
            }
            DataProviderRequestResult::Unauthorized => {
                self.keystore.write().remove(request_context);
            }
            _ => {}
        }
    }

    async fn get(
        &self,
        _request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<Arc<ResponsePayload>> {
        None
    }
}

pub struct SdkKeyStore {
    keystore: Arc<RwLock<HashMap<Arc<AuthorizedRequestContext>, u64>>>,
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

    pub async fn get_registered_store(&self) -> Vec<(Arc<AuthorizedRequestContext>, u64)> {
        self.keystore
            .read()
            .iter()
            .map(|(key, &value)| (key.clone(), value))
            .collect()
    }
}
