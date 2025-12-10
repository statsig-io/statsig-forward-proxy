pub mod background_data_provider;
pub mod http_data_provider;
pub mod request_builder;
use std::sync::Arc;

use async_trait::async_trait;
use http_data_provider::ResponsePayload;

use crate::{
    servers::authorized_request_context::AuthorizedRequestContext,
    // utils::compress_encoder::CompressionEncoder,
};

use self::request_builder::RequestBuilderTrait;
#[async_trait]
pub trait DataProviderTrait {
    async fn get(
        &self,
        http_client: &reqwest::Client,
        request_builder: &Arc<dyn RequestBuilderTrait>,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
    ) -> DataProviderResult;
}

pub struct FullRequestContext {
    pub authorized_request_context: Arc<AuthorizedRequestContext>,
}

pub struct ResponseContext {
    pub result_type: DataProviderRequestResult,
    pub lcut: u64,
    pub body: Arc<ResponsePayload>,
}

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum DataProviderRequestResult {
    DataAvailable,
    NoDataAvailable,
    Unauthorized,
    Error,
}

#[derive(Debug)]
pub struct DataProviderResult {
    result: DataProviderRequestResult,
    // encoding: CompressionEncoder,
    body: Option<Arc<ResponsePayload>>,
    lcut: u64,
}
