use clap::Parser;
use clap::ValueEnum;

use datastore::config_spec_store::ConfigSpecStore;
use datastore::data_providers::request_builder::CachedRequestBuilders;
use datastore::data_providers::request_builder::DcsRequestBuilder;
use datastore::data_providers::request_builder::IdlistRequestBuilder;
use datastore::get_id_list_store::GetIdListStore;
use datastore::log_event_store::LogEventStore;
use datastore::{
    caching::{in_memory_cache, redis_cache},
    data_providers::{background_data_provider, http_data_provider},
    sdk_key_store,
};
use futures::join;
use loggers::debug_logger;
use loggers::statsd_logger;
use observers::http_data_provider_observer::HttpDataProviderObserver;

use statsig::{Statsig, StatsigOptions, StatsigUser};

use observers::proxy_event_observer::ProxyEventObserver;
use observers::HttpDataProviderObserverTrait;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

pub mod datastore;
pub mod datatypes;
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
    // Deprecated in favour of generic terminology, but kept for backwards compatibility
    #[clap(long, action)]
    datadog_logging: bool,
    #[clap(long, action)]
    statsd_logging: bool,
    #[clap(long, action)]
    debug_logging: bool,
    #[clap(short, long, default_value = "1000")]
    maximum_concurrent_sdk_keys: u16,
    #[clap(short, long, default_value = "10")]
    polling_interval_in_s: u64,
    #[clap(short, long, default_value = "64")]
    update_batch_size: u64,
    #[clap(short, long, default_value = "70")]
    redis_leader_key_ttl: i64,
    #[clap(long, default_value = "86400")]
    redis_cache_ttl_in_s: i64,
    #[clap(long, action)]
    force_gcp_profiling_enabled: bool,
    #[clap(short, long, default_value = "500")]
    grpc_max_concurrent_streams: u32,
    // If you set this flag, you do not need an external process
    // to clean up the external datastore if your key becomes unauthorized.
    // The downside is that if there are issues with auth upstream, you might create
    // inavailability of the config.
    #[clap(long, action)]
    clear_external_datastore_on_unauthorized: bool,
}

#[derive(Deserialize, Debug)]
struct ConfigurationAndOverrides {
    statsig_endpoint: Option<String>,
    statsig_server_sdk_key: Option<String>,
    hostname: Option<String>,
    log_event_statsig_endpoint: Option<String>,
    log_event_dedupe_cache_limit: Option<usize>,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum TransportMode {
    GrpcAndHttp,
    Grpc,
    Http,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum CacheMode {
    Local,
    Redis,
}

async fn try_initialize_statsig_sdk_and_profiling(cli: &Cli, config: &ConfigurationAndOverrides) {
    if config.statsig_server_sdk_key.is_some() {
        let opts = StatsigOptions {
            // If we are using HTTP server, we can actually be self referential
            // to decide whether or not to enable GCP Profiling
            api_for_download_config_specs: match cli.mode {
                TransportMode::Grpc => "https://api.statsigcdn.com/v1".to_string(),
                TransportMode::Http => "http://0.0.0.0:8000/v1".to_string(),
                TransportMode::GrpcAndHttp => "http://0.0.0.0:8000/v1".to_string(),
            },
            ..StatsigOptions::default()
        };
        if let Some(err) = Statsig::initialize_with_options(
            &config
                .statsig_server_sdk_key
                .clone()
                .expect("validated existence"),
            opts,
        )
        .await
        {
            panic!("Failed to initialize statsig SDK: {}", err);
        }
    }

    let mut custom_ids = HashMap::new();
    custom_ids.insert(
        "podID".to_string(),
        config
            .hostname
            .clone()
            .unwrap_or("no_hostname_provided".to_string()),
    );
    let statsig_user = Arc::new(StatsigUser::with_custom_ids(custom_ids));
    let force_enable = cli.force_gcp_profiling_enabled;
    cloud_profiler_rust::maybe_start_profiling(
        "statsig-forward-proxy".to_string(),
        std::env::var("DD_VERSION").unwrap_or("missing_dd_version".to_string()),
        move || {
            force_enable
                || Statsig::check_gate(&statsig_user, "enable_gcp_profiler_for_sfp")
                    .unwrap_or(false)
        },
    )
    .await;
}

async fn create_config_spec_store(
    cli: &Cli,
    overrides: &ConfigurationAndOverrides,
    background_data_provider: Arc<background_data_provider::BackgroundDataProvider>,
    config_spec_observer: Arc<HttpDataProviderObserver>,
    cache_uuid: &str,
    sdk_key_store: &Arc<sdk_key_store::SdkKeyStore>,
) -> Arc<ConfigSpecStore> {
    let shared_cache: Arc<dyn HttpDataProviderObserverTrait + Send + Sync> = match cli.cache {
        CacheMode::Local => Arc::new(in_memory_cache::InMemoryCache::new(
            cli.maximum_concurrent_sdk_keys,
        )),
        CacheMode::Redis => Arc::new(
            redis_cache::RedisCache::new(
                "statsig".to_string(),
                cli.redis_leader_key_ttl,
                cache_uuid,
                true, /* check lcut */
                cli.clear_external_datastore_on_unauthorized,
                cli.redis_cache_ttl_in_s,
            )
            .await,
        ),
    };
    let dcs_request_builder = Arc::new(DcsRequestBuilder::new(
        overrides
            .statsig_endpoint
            .as_ref()
            .map_or("https://api.statsigcdn.com".to_string(), |s| s.to_string()),
        Arc::clone(&config_spec_observer),
        Arc::clone(&shared_cache),
    ));
    CachedRequestBuilders::add_request_builder(
        "/v1/download_config_specs/",
        dcs_request_builder.clone(),
    )
    .await;
    CachedRequestBuilders::add_request_builder("/v2/download_config_specs/", dcs_request_builder)
        .await;
    let config_spec_store = Arc::new(datastore::config_spec_store::ConfigSpecStore::new(
        sdk_key_store.clone(),
        background_data_provider.clone(),
    ));
    config_spec_observer
        .add_observer(sdk_key_store.clone())
        .await;
    config_spec_observer
        .add_observer(config_spec_store.clone())
        .await;
    config_spec_observer.add_observer(shared_cache).await;

    config_spec_store
}

async fn create_id_list_store(
    cli: &Cli,
    _overrides: &ConfigurationAndOverrides,
    background_data_provider: Arc<background_data_provider::BackgroundDataProvider>,
    idlist_observer: Arc<HttpDataProviderObserver>,
    cache_uuid: &str,
    sdk_key_store: &Arc<sdk_key_store::SdkKeyStore>,
) -> Arc<GetIdListStore> {
    let shared_cache: Arc<dyn HttpDataProviderObserverTrait + Send + Sync> = match cli.cache {
        CacheMode::Local => Arc::new(in_memory_cache::InMemoryCache::new(
            cli.maximum_concurrent_sdk_keys,
        )),
        CacheMode::Redis => {
            Arc::new(
                redis_cache::RedisCache::new(
                    "statsig_id_list".to_string(),
                    cli.redis_leader_key_ttl,
                    cache_uuid,
                    false, /* check lcut */
                    cli.clear_external_datastore_on_unauthorized,
                    cli.redis_cache_ttl_in_s,
                )
                .await,
            )
        }
    };
    let idlist_request_builder = Arc::new(IdlistRequestBuilder::new(
        Arc::clone(&idlist_observer),
        Arc::clone(&shared_cache),
    ));
    CachedRequestBuilders::add_request_builder("/v1/get_id_lists/", idlist_request_builder).await;
    let id_list_store = Arc::new(datastore::get_id_list_store::GetIdListStore::new(
        sdk_key_store.clone(),
        background_data_provider.clone(),
    ));
    idlist_observer.add_observer(sdk_key_store.clone()).await;
    idlist_observer.add_observer(id_list_store.clone()).await;
    idlist_observer.add_observer(shared_cache).await;

    id_list_store
}

async fn create_log_event_store(
    http_client: reqwest::Client,
    config: &ConfigurationAndOverrides,
) -> Arc<LogEventStore> {
    Arc::new(LogEventStore::new(
        config
            .log_event_statsig_endpoint
            .as_ref()
            .map_or("https://statsigapi.net", |s| s.as_str()),
        http_client,
        config.log_event_dedupe_cache_limit.unwrap_or(2_000_000),
    ))
}

#[rocket::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let overrides = envy::from_env::<ConfigurationAndOverrides>().expect("Envy Error");
    try_initialize_statsig_sdk_and_profiling(&cli, &overrides).await;

    if cli.datadog_logging || cli.statsd_logging {
        let datadog_logger = Arc::new(statsd_logger::StatsdLogger::new().await);
        ProxyEventObserver::add_observer(datadog_logger).await;
    }
    if cli.debug_logging {
        let debug_logger = Arc::new(debug_logger::DebugLogger::new());
        ProxyEventObserver::add_observer(debug_logger).await;
    }
    let http_client = reqwest::Client::builder()
        .gzip(true)
        .pool_idle_timeout(None)
        .build()
        .expect("We must have an http client");
    let shared_http_data_provider = Arc::new(http_data_provider::HttpDataProvider::new(
        http_client.clone(),
    ));
    let sdk_key_store = Arc::new(sdk_key_store::SdkKeyStore::new());
    let background_data_provider = Arc::new(background_data_provider::BackgroundDataProvider::new(
        shared_http_data_provider,
        cli.polling_interval_in_s,
        cli.update_batch_size,
        Arc::clone(&sdk_key_store),
    ));
    let cache_uuid = Uuid::new_v4().to_string();
    let config_spec_observer = Arc::new(HttpDataProviderObserver::new());
    let config_spec_store = create_config_spec_store(
        &cli,
        &overrides,
        Arc::clone(&background_data_provider),
        Arc::clone(&config_spec_observer),
        &cache_uuid,
        &sdk_key_store,
    )
    .await;
    let idlist_observer = Arc::new(HttpDataProviderObserver::new());
    let id_list_store = create_id_list_store(
        &cli,
        &overrides,
        Arc::clone(&background_data_provider),
        Arc::clone(&idlist_observer),
        &cache_uuid,
        &sdk_key_store,
    )
    .await;
    let log_event_store = create_log_event_store(http_client.clone(), &overrides).await;
    background_data_provider.start_background_thread().await;

    match cli.mode {
        TransportMode::Grpc => {
            servers::grpc_server::GrpcServer::start_server(
                cli.grpc_max_concurrent_streams,
                config_spec_store,
                config_spec_observer,
            )
            .await?
        }
        TransportMode::Http => {
            servers::http_server::HttpServer::start_server(
                config_spec_store,
                id_list_store,
                log_event_store,
            )
            .await?
        }
        TransportMode::GrpcAndHttp => {
            let grpc_server = servers::grpc_server::GrpcServer::start_server(
                cli.grpc_max_concurrent_streams,
                config_spec_store.clone(),
                config_spec_observer.clone(),
            );
            let http_server = servers::http_server::HttpServer::start_server(
                config_spec_store,
                id_list_store,
                log_event_store,
            );
            join!(async { grpc_server.await.ok() }, async {
                http_server.await.ok()
            },);
        }
    }
    Ok(())
}
