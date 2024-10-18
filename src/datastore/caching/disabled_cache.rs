use crate::{
    datastore::data_providers::{http_data_provider::ResponsePayload, DataProviderRequestResult},
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
        _result: &DataProviderRequestResult,
        _request_context: &Arc<AuthorizedRequestContext>,
        _lcut: u64,
        _data: &Arc<ResponsePayload>,
    ) {
        /* noop */
    }

    async fn get(
        &self,
        _request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<Arc<ResponsePayload>> {
        return None;
    }
}
