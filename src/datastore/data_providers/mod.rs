pub mod background_data_provider;
pub mod http_data_provider;
use std::sync::Arc;

use async_trait::async_trait;
#[async_trait]
pub trait DataProviderTrait {
    async fn get(&self, key: &str, lcut: u64) -> DataProviderResult;
}

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum DataProviderRequestResult {
    DataAvailable,
    NoDataAvailable,
    Error,
}

#[derive(Debug)]
pub struct DataProviderResult {
    result: DataProviderRequestResult,
    data: Option<(Arc<String>, u64)>,
}
