pub mod background_data_provider;
pub mod http_data_provider;
pub mod request_builder;
use std::sync::Arc;

use async_trait::async_trait;

use self::request_builder::RequestBuilderTrait;
#[async_trait]
pub trait DataProviderTrait {
    async fn get(
        &self,
        request_builder: &Arc<dyn RequestBuilderTrait>,
        key: &str,
        lcut: u64,
    ) -> DataProviderResult;
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
    data: Option<(Arc<String>, u64)>,
}
