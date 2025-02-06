use crate::{
    datastore::{
        config_spec_store::ConfigSpecForCompany,
        data_providers::{FullRequestContext, ResponseContext},
    },
    observers::HttpDataProviderObserverTrait,
    servers::authorized_request_context::AuthorizedRequestContext,
};

use std::sync::Arc;

#[derive(Default)]
pub struct DisabledCache {}

use async_trait::async_trait;
#[async_trait]
impl HttpDataProviderObserverTrait for DisabledCache {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        false
    }

    async fn update(
        &self,
        _request_context: &Arc<FullRequestContext>,
        _response_context: &Arc<ResponseContext>,
    ) {
        /* noop */
    }

    async fn get(
        &self,
        _request_context: &Arc<AuthorizedRequestContext>,
        _zstd_dict_id: &Option<Arc<str>>,
    ) -> Option<Arc<ConfigSpecForCompany>> {
        return None;
    }
}
