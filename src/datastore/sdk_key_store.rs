use super::data_providers::DataProviderRequestResult;
use crate::observers::HttpDataProviderObserverTrait;
use crate::servers::authorized_request_context::AuthorizedRequestContext;
use async_trait::async_trait;
use dashmap::DashMap;

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
        _data: &Arc<str>,
    ) {
        let write_lock = self.keystore.clone();
        match result {
            DataProviderRequestResult::DataAvailable => {
                if !request_context.use_lcut
                    || write_lock.get(request_context).map_or(true, |v| *v < lcut)
                {
                    write_lock.insert(Arc::clone(request_context), lcut);
                }
            }
            DataProviderRequestResult::Unauthorized => {
                if write_lock.contains_key(request_context) {
                    write_lock.remove(request_context);
                }
            }
            _ => {}
        }
    }

    async fn get(&self, _request_context: &Arc<AuthorizedRequestContext>) -> Option<Arc<str>> {
        None
    }
}

pub struct SdkKeyStore {
    keystore: Arc<DashMap<Arc<AuthorizedRequestContext>, u64>>,
}

impl Default for SdkKeyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SdkKeyStore {
    pub fn new() -> SdkKeyStore {
        SdkKeyStore {
            keystore: Arc::new(DashMap::new()),
        }
    }

    pub fn has_key(&self, request_context: &Arc<AuthorizedRequestContext>) -> bool {
        self.keystore.contains_key(request_context)
    }

    pub async fn get_registered_store(&self) -> Vec<(Arc<AuthorizedRequestContext>, u64)> {
        self.keystore
            .iter()
            .map(|entry| (entry.key().clone(), *entry.value()))
            .collect()
    }
}
