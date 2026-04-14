use lazy_static::lazy_static;
use tokio_util::sync::CancellationToken;

pub mod datastore;
pub mod datatypes;
pub mod observers;
pub mod utils;

pub mod servers {
    pub mod authorized_request_context;
    pub mod normalized_path;
    pub mod sdk_key_normalizer;
}

lazy_static! {
    pub static ref GRACEFUL_SHUTDOWN_TOKEN: CancellationToken = CancellationToken::new();
}
