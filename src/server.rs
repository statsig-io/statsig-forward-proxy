use clap::ArgAction;
use clap::Parser;
use clap::ValueEnum;

use cloud_profiler_rust::CloudProfilerConfiguration;
use datastore::caching::disabled_cache;
use datastore::config_spec_store::ConfigSpecStore;
use datastore::data_providers::request_builder::CachedRequestBuilders;
use datastore::data_providers::request_builder::DcsRequestBuilder;
use datastore::data_providers::request_builder::IdlistRequestBuilder;
use datastore::log_event_store::LogEventStore;
use datastore::{
    caching::redis_cache,
    data_providers::{background_data_provider, http_data_provider},
    sdk_key_store,
};
use futures::join;
use loggers::debug_logger;
use loggers::stats_logger;
use observers::http_data_provider_observer::HttpDataProviderObserver;
use tokio_util::sync::CancellationToken;

use datastore::id_list_store::GetIdListStore;
use servers::authorized_request_context::AuthorizedRequestContextCache;
use statsig::{Statsig, StatsigOptions};

use observers::proxy_event_observer::ProxyEventObserver;
use observers::HttpDataProviderObserverTrait;
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use lazy_static::lazy_static;

pub mod datastore;
pub mod datatypes;
pub mod loggers;
pub mod observers;
pub mod servers;
pub mod utils;
use serde::Deserialize;

lazy_static! {
    pub static ref GRACEFUL_SHUTDOWN_TOKEN: CancellationToken = CancellationToken::new();
}

#[derive(Parser, Clone)]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[arg(value_enum)]
    mode: TransportMode,
    #[arg(value_enum)]
    cache: CacheMode,
    #[clap(long, action)]
    double_write_cache_for_legacy_key: bool,
    // Deprecated: Same as statsd logging
    #[clap(long, action)]
    datadog_logging: bool,
    #[clap(long, action)]
    statsd_logging: bool,
    #[clap(long, action)]
    statsig_logging: bool,
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
    #[clap(long, default_value = "20000")]
    log_event_process_queue_size: usize,
    #[clap(long, action)]
    force_gcp_profiling_enabled: bool,
    #[clap(short, long, default_value = "500")]
    grpc_max_concurrent_streams: u32,
    // By default, we do not enable this configuration. This allows you to ensure
    // if there are any issues with authorization on Statsig's end you still
    // have a payload to server.
    //
    // This means if you delete a key, until the service is restarted, the entry
    // from internal or external caches
    #[clap(long, action)]
    clear_datastore_on_unauthorized: bool,

    // Authorization and TLS Configuration:
    #[clap(long, default_value = None)]
    x509_server_cert_path: Option<String>,
    #[clap(long, default_value = None)]
    x509_server_key_path: Option<String>,
    #[clap(long, default_value = None)]
    x509_client_cert_path: Option<String>,
    #[clap(long, action = ArgAction::Set, default_value = "true")]
    enforce_tls: bool,
    #[clap(long, default_value = "false")]
    enforce_mtls: bool,
}

#[derive(Deserialize, Debug)]
struct ConfigurationAndOverrides {
    statsig_endpoint: Option<String>,
    statsig_server_sdk_key: Option<String>,
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
    Disabled,
    Redis,
}

async fn try_initialize_statsig_sdk_and_profiling(cli: &Cli, config: &ConfigurationAndOverrides) {
    let enable_statsig = match &config.statsig_server_sdk_key {
        Some(server_sdk_key) => {
            let opts = StatsigOptions {
                // If we are using HTTP server, we can actually be self referential
                // to decide whether or not to enable GCP Profiling
                api_for_download_config_specs: match cli.mode {
                    TransportMode::Grpc => "https://api.statsigcdn.com/v1".to_string(),
                    TransportMode::Http => "http://127.0.0.1:8001/v1".to_string(),
                    TransportMode::GrpcAndHttp => "http://127.0.0.1:8001/v1".to_string(),
                },
                disable_user_agent_support: true,
                ..StatsigOptions::default()
            };
            if let Some(err) = Statsig::initialize_with_options(server_sdk_key, opts).await {
                panic!("Failed to initialize statsig SDK: {}", err);
            }

            true
        }
        None => {
            if cli.statsig_logging {
                panic!("Must define statsig server sdk key if using statsig logging");
            }

            false
        }
    };

    let force_enable = cli.force_gcp_profiling_enabled;
    cloud_profiler_rust::maybe_start_profiling(
        "statsig-services".to_string(),
        "statsig-forward-proxy".to_string(),
        std::env::var("DD_VERSION").unwrap_or("missing_dd_version".to_string()),
        move || {
            if enable_statsig {
                force_enable
                    || Statsig::check_gate(
                        &utils::statsig_sdk_wrapper::STATSIG_USER,
                        "enable_gcp_profiler_for_sfp",
                    )
                    .unwrap_or(false)
            } else {
                force_enable
            }
        },
        move || {
            let default = CloudProfilerConfiguration {
                sampling_rate: 1000,
            };
            match Statsig::get_config::<CloudProfilerConfiguration>(
                &utils::statsig_sdk_wrapper::STATSIG_USER,
                "rust_cloud_profiler_configuration",
            ) {
                Ok(config) => config.value.unwrap_or(default),
                Err(_e) => default,
            }
        },
    )
    .await;
}

async fn create_config_spec_store(
    _cli: &Cli,
    overrides: &ConfigurationAndOverrides,
    background_data_provider: Arc<background_data_provider::BackgroundDataProvider>,
    config_spec_observer: Arc<HttpDataProviderObserver>,
    shared_cache: &Arc<dyn HttpDataProviderObserverTrait + Send + Sync>,
    sdk_key_store: &Arc<sdk_key_store::SdkKeyStore>,
) -> Arc<ConfigSpecStore> {
    let dcs_request_builder = Arc::new(DcsRequestBuilder::new(
        overrides
            .statsig_endpoint
            .as_ref()
            .map_or("https://api.statsigcdn.com".to_string(), |s| s.to_string()),
        Arc::clone(&config_spec_observer),
        Arc::clone(shared_cache),
    ));
    CachedRequestBuilders::add_request_builder(
        "/v1/download_config_specs/",
        dcs_request_builder.clone(),
    );
    CachedRequestBuilders::add_request_builder("/v2/download_config_specs/", dcs_request_builder);
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
    config_spec_observer
        .add_observer(Arc::clone(shared_cache))
        .await;

    config_spec_store
}

async fn create_log_event_store(
    http_client: reqwest::Client,
    config: &ConfigurationAndOverrides,
) -> Arc<LogEventStore> {
    let store = Arc::new(LogEventStore::new(
        config
            .log_event_statsig_endpoint
            .as_ref()
            .map_or("https://statsigapi.net", |s| s.as_str()),
        http_client,
        config.log_event_dedupe_cache_limit.unwrap_or(2_000_000),
    ));

    store.clone()
}

async fn create_id_list_store(
    _cli: &Cli,
    background_data_provider: Arc<background_data_provider::BackgroundDataProvider>,
    idlist_observer: Arc<HttpDataProviderObserver>,
    shared_cache: &Arc<dyn HttpDataProviderObserverTrait + Send + Sync>,
    sdk_key_store: &Arc<sdk_key_store::SdkKeyStore>,
) -> Arc<GetIdListStore> {
    let idlist_request_builder = Arc::new(IdlistRequestBuilder::new(
        Arc::clone(&idlist_observer),
        Arc::clone(shared_cache),
    ));
    CachedRequestBuilders::add_request_builder("/v1/get_id_lists/", idlist_request_builder);
    let id_list_store = Arc::new(datastore::id_list_store::GetIdListStore::new(
        sdk_key_store.clone(),
        background_data_provider.clone(),
    ));
    idlist_observer.add_observer(sdk_key_store.clone()).await;
    idlist_observer.add_observer(id_list_store.clone()).await;
    idlist_observer.add_observer(Arc::clone(shared_cache)).await;

    id_list_store
}

#[rocket::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let overrides = envy::from_env::<ConfigurationAndOverrides>().expect("Envy Error");
    try_initialize_statsig_sdk_and_profiling(&cli, &overrides).await;

    if cli.datadog_logging || cli.statsd_logging || cli.statsig_logging {
        let stats_logger = Arc::new(
            stats_logger::StatsLogger::new(
                cli.statsd_logging,
                cli.datadog_logging,
                cli.statsig_logging,
            )
            .await,
        );
        ProxyEventObserver::add_observer(stats_logger).await;
    }
    if cli.debug_logging {
        let debug_logger = Arc::new(debug_logger::DebugLogger::new());
        ProxyEventObserver::add_observer(debug_logger).await;
    }
    let shared_http_data_provider = Arc::new(http_data_provider::HttpDataProvider {});
    let sdk_key_store = Arc::new(sdk_key_store::SdkKeyStore::new());
    let background_data_provider = Arc::new(background_data_provider::BackgroundDataProvider::new(
        shared_http_data_provider,
        cli.polling_interval_in_s,
        cli.update_batch_size,
        Arc::clone(&sdk_key_store),
        cli.clear_datastore_on_unauthorized,
    ));
    let cache_uuid = Uuid::new_v4().to_string();
    let redis_cache: Arc<dyn HttpDataProviderObserverTrait + Send + Sync> = match cli.cache {
        CacheMode::Redis => Arc::new(
            redis_cache::RedisCache::new(
                "statsig".to_string(),
                cli.redis_leader_key_ttl,
                &cache_uuid,
                true, /* check lcut */
                cli.redis_cache_ttl_in_s,
                cli.double_write_cache_for_legacy_key,
            )
            .await,
        ),
        CacheMode::Disabled => Arc::new(disabled_cache::DisabledCache::default()),
    };
    let config_spec_observer = Arc::new(HttpDataProviderObserver::new());
    let config_spec_store = create_config_spec_store(
        &cli,
        &overrides,
        Arc::clone(&background_data_provider),
        Arc::clone(&config_spec_observer),
        &redis_cache,
        &sdk_key_store,
    )
    .await;
    let idlist_observer = Arc::new(HttpDataProviderObserver::new());
    let id_list_store = create_id_list_store(
        &cli,
        Arc::clone(&background_data_provider),
        Arc::clone(&idlist_observer),
        &redis_cache,
        &sdk_key_store,
    )
    .await;
    background_data_provider.start_background_thread().await;
    let rc_cache = Arc::new(AuthorizedRequestContextCache::new());
    // Default buffer size is 20000 messages
    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("We must have an http client");
    let log_event_store = create_log_event_store(http_client.clone(), &overrides).await;
    match cli.mode {
        TransportMode::Grpc => {
            servers::grpc_server::GrpcServer::start_server(
                &cli,
                config_spec_store,
                config_spec_observer,
                rc_cache,
            )
            .await?
        }
        TransportMode::Http => {
            servers::http_server::HttpServer::start_server(
                &cli,
                config_spec_store,
                log_event_store,
                id_list_store,
                rc_cache,
            )
            .await?
        }
        TransportMode::GrpcAndHttp => {
            let grpc_server = servers::grpc_server::GrpcServer::start_server(
                &cli,
                config_spec_store.clone(),
                config_spec_observer.clone(),
                rc_cache.clone(),
            );
            let http_server = servers::http_server::HttpServer::start_server(
                &cli,
                config_spec_store,
                log_event_store,
                id_list_store,
                rc_cache,
            );
            join!(async { grpc_server.await.ok() }, async {
                http_server.await.ok()
            },);
        }
    }

    // Terminate all actively running loops in other threads
    GRACEFUL_SHUTDOWN_TOKEN.cancel();

    Ok(())
}
