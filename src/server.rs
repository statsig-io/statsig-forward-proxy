use clap::Parser;
use clap::ValueEnum;
use datastore::{
    caching::{in_memory_cache, redis_cache},
    data_providers::{background_data_provider, http_data_provider},
    sdk_key_store,
};
use loggers::datadog_logger;
use loggers::debug_logger;
use observers::new_dcs_observer::NewDcsObserver;

use observers::proxy_event_observer::ProxyEventObserver;
use observers::NewDcsObserverTrait;
use std::sync::Arc;

pub mod datastore;
pub mod loggers;
pub mod observers;
pub mod servers;
use serde::Deserialize;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[arg(value_enum)]
    mode: TransportMode,
    #[arg(value_enum)]
    cache: CacheMode,
    #[clap(long, action)]
    datadog_logging: bool,
    #[clap(long, action)]
    debug_logging: bool,
    #[clap(short, long, default_value = "1000")]
    maximum_concurrent_sdk_keys: u16,
    #[clap(short, long, default_value = "10")]
    polling_interval_in_s: u64,
    #[clap(short, long, default_value = "64")]
    update_batch_size: u64,
}

#[derive(Deserialize, Debug)]
struct Overrides {
    statsig_endpoint: Option<String>,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum TransportMode {
    Grpc,
    Http,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum CacheMode {
    Local,
    Redis,
}

#[rocket::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let overrides = envy::from_env::<Overrides>().expect("Envy Error");

    let shared_cache: Arc<dyn NewDcsObserverTrait + Send + Sync> = match cli.cache {
        CacheMode::Local => Arc::new(in_memory_cache::InMemoryCache::new(
            cli.maximum_concurrent_sdk_keys,
        )),
        CacheMode::Redis => Arc::new(redis_cache::RedisCache::new().await),
    };

    if cli.datadog_logging {
        let datadog_logger = Arc::new(datadog_logger::DatadogLogger::new().await);
        ProxyEventObserver::add_observer(datadog_logger).await;
    }
    if cli.debug_logging {
        let debug_logger = Arc::new(debug_logger::DebugLogger::new());
        ProxyEventObserver::add_observer(debug_logger).await;
    }

    let shared_http_data_provider = Arc::new(http_data_provider::HttpDataProvider::new(
        &overrides
            .statsig_endpoint
            .unwrap_or_else(|| "https://api.statsigcdn.com".to_string()),
    ));
    let shared_dcs_observer = Arc::new(NewDcsObserver::new());
    let background_data_provider = background_data_provider::BackgroundDataProvider::new(
        shared_cache.clone(),
        shared_http_data_provider,
        shared_dcs_observer.clone(),
        cli.polling_interval_in_s,
        cli.update_batch_size,
    );
    let shared_background_data_provider = Arc::new(background_data_provider);
    let sdk_key_store = Arc::new(sdk_key_store::SdkKeyStore::new());
    let shared_sdk_key_store = sdk_key_store.clone();
    let config_spec_store = datastore::config_spec_store::ConfigSpecStore::new(
        sdk_key_store.clone(),
        shared_background_data_provider.clone(),
    );
    let shared_config_spec_store = Arc::new(config_spec_store);
    shared_dcs_observer
        .add_observer(shared_sdk_key_store.clone())
        .await;
    shared_dcs_observer
        .add_observer(shared_config_spec_store.clone())
        .await;
    shared_dcs_observer.add_observer(shared_cache).await;
    shared_background_data_provider.start_background_thread(shared_sdk_key_store);

    match cli.mode {
        TransportMode::Grpc => {
            servers::grpc_server::GrpcServer::start_server(shared_config_spec_store).await?
        }
        TransportMode::Http => {
            servers::http_server::HttpServer::start_server(shared_config_spec_store).await?
        }
    }
    Ok(())
}
