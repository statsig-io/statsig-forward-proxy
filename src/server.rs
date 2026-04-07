use clap::ArgAction;
use clap::Parser;
use clap::ValueEnum;

use datastore::caching::disabled_cache;
use datastore::config_spec_store::ConfigSpecStore;
use datastore::data_providers::dcs_blob_url_generator::DcsBlobUrlGenerator;
use datastore::data_providers::request_builder::CachedRequestBuilders;
use datastore::data_providers::request_builder::DcsBlobConfig;
use datastore::data_providers::request_builder::DcsRequestBuilder;
use datastore::data_providers::request_builder::IdListFileRequestBuilder;
use datastore::data_providers::request_builder::IdlistRequestBuilder;
use datastore::data_providers::HttpClientConfig;
use datastore::deltas_store::DeltasStore;
use datastore::id_list_file_refresh_observer::IdListFileRefreshObserver;
use datastore::id_list_file_store::IdListFileStore;
use datastore::log_event_store::LogEventStore;
use datastore::{
    caching::redis_cache,
    data_providers::{
        background_data_provider, http_data_provider, startup_warmup_parser, OUTBOUND_USER_AGENT,
    },
    sdk_key_store,
};
use futures::join;
use loggers::debug_logger;
use loggers::nginx_cache_monitor;
use loggers::stats_logger;
use observers::http_data_provider_observer::HttpDataProviderObserver;
use servers::normalized_path::NormalizedPath;
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
use std::env;

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
    otlp_logging: bool,
    #[clap(long, action)]
    statsig_logging: bool,
    #[clap(long, default_value = None, value_parser = utils::deserialization_helpers::parse_kv_pair::<String, String>, value_delimiter = ',')]
    statsig_logging_tags: Option<Vec<(String, String)>>,
    #[clap(long, action)]
    debug_logging: bool,
    #[clap(short, long, default_value = "1000")]
    maximum_concurrent_sdk_keys: u16,
    #[clap(short, long, default_value = "10")]
    polling_interval_in_s: u64,
    #[clap(short = 'u', long, alias = "update-batch-size", default_value = "64")]
    max_in_flight: u64,
    /// Adds a minimum delay between background request launches to reduce
    /// synchronized bursts across pods. `0` disables this pacing.
    #[clap(long, default_value = "0")]
    background_poll_item_spacing_ms: u64,
    #[clap(long, default_value = "5")]
    redis_connection_timeout_in_s: u64,
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
    // Max total percentage of total streams that can be connected
    // for the GRPC ready endpoint to return that it is ready to
    // serve requests.
    #[clap(long, default_value = "90")]
    grpc_ready_endpoint_percentage_of_total_streams_threshold: usize,
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
    #[clap(long, action = ArgAction::SetTrue)]
    enforce_tls: bool,
    #[clap(long, default_value = "false")]
    enforce_mtls: bool,
    #[clap(long, default_value = "10")]
    http_connection_pool_max_idle_per_host: usize,
    #[clap(long, default_value = "30")]
    http_connection_pool_idle_timeout_in_s: u64,
    #[clap(long, default_value = "30")]
    http_request_timeout_in_s: u64,
    #[clap(long, default_value = "10")]
    http_read_timeout_in_s: u64,
    #[clap(long, default_value = "10")]
    http_connect_timeout_in_s: u64,
    #[clap(long, action)]
    deltas_background_loop_enabled: bool,
    #[clap(long, action)]
    id_list_file_refresh_observer_enabled: bool,
    #[clap(long, default_value = None)]
    deltas_background_loop_sleep_time_in_s: Option<u64>,
    #[clap(long, default_value = None)]
    deltas_num_responses_to_persist: Option<usize>,
}

impl Cli {
    fn http_client_config(&self) -> HttpClientConfig {
        HttpClientConfig::new(
            self.http_request_timeout_in_s,
            self.http_read_timeout_in_s,
            self.http_connect_timeout_in_s,
            self.http_connection_pool_idle_timeout_in_s,
            self.http_connection_pool_max_idle_per_host,
        )
    }
}

#[derive(Deserialize, Debug)]
struct ConfigurationAndOverrides {
    statsig_endpoint: Option<String>,
    statsig_endpoint_deltas: Option<String>,
    statsig_endpoint_download_id_list_file: Option<String>,
    enable_blob_storage_for_download_config_specs: Option<bool>,
    dcs_blob_storage_connection_string: Option<String>,
    dcs_blob_storage_company_id: Option<String>,
    statsig_server_sdk_key: Option<String>,
    log_event_statsig_endpoint: Option<String>,
    log_event_dedupe_cache_limit: Option<usize>,
    tokio_worker_threads_background: Option<usize>,
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

struct BackgroundRuntimeGuard(Option<tokio::runtime::Runtime>);

impl BackgroundRuntimeGuard {
    fn new(runtime: tokio::runtime::Runtime) -> Self {
        Self(Some(runtime))
    }

    fn handle(&self) -> tokio::runtime::Handle {
        self.0
            .as_ref()
            .expect("background runtime must exist while guard is alive")
            .handle()
            .clone()
    }
}

impl Drop for BackgroundRuntimeGuard {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

async fn try_initialize_statsig_sdk_and_profiling(cli: &Cli, config: &ConfigurationAndOverrides) {
    match &config.statsig_server_sdk_key {
        Some(server_sdk_key) => {
            let opts = StatsigOptions {
                disable_user_agent_support: true,
                ..StatsigOptions::default()
            };
            if let Some(err) = Statsig::initialize_with_options(server_sdk_key, opts).await {
                panic!("Failed to initialize statsig SDK: {err}");
            }

            if let Some(tags) = &cli.statsig_logging_tags {
                utils::statsig_sdk_wrapper::STATSIG_USER_FACTORY.set(tags.clone());
            }
        }
        None => {
            if cli.statsig_logging {
                panic!("Must define statsig server sdk key if using statsig logging");
            }
        }
    };
}

async fn create_config_spec_store(
    _cli: &Cli,
    overrides: &ConfigurationAndOverrides,
    background_data_provider: Arc<background_data_provider::BackgroundDataProvider>,
    config_spec_observer: Arc<HttpDataProviderObserver>,
    shared_cache: &Arc<dyn HttpDataProviderObserverTrait + Send + Sync>,
    sdk_key_store: &Arc<sdk_key_store::SdkKeyStore>,
) -> Arc<ConfigSpecStore> {
    let dcs_blob_config = create_dcs_blob_config(overrides);
    let dcs_request_builder = Arc::new(DcsRequestBuilder::new(
        overrides
            .statsig_endpoint
            .as_ref()
            .map_or("https://api.statsigcdn.com".to_string(), |s| s.to_string()),
        Arc::clone(&config_spec_observer),
        Arc::clone(shared_cache),
        dcs_blob_config,
    ));
    CachedRequestBuilders::add_request_builder(
        NormalizedPath::V1DownloadConfigSpecs,
        dcs_request_builder.clone(),
    );
    CachedRequestBuilders::add_request_builder(
        NormalizedPath::V2DownloadConfigSpecs,
        dcs_request_builder.clone(),
    );
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
    overrides: &ConfigurationAndOverrides,
    background_data_provider: Arc<background_data_provider::BackgroundDataProvider>,
    idlist_observer: Arc<HttpDataProviderObserver>,
    shared_cache: &Arc<dyn HttpDataProviderObserverTrait + Send + Sync>,
    sdk_key_store: &Arc<sdk_key_store::SdkKeyStore>,
) -> Arc<GetIdListStore> {
    let idlist_request_builder = Arc::new(IdlistRequestBuilder::new(
        overrides
            .statsig_endpoint
            .as_ref()
            .map_or("https://api.statsigcdn.com".to_string(), |s| s.to_string()),
        Arc::clone(&idlist_observer),
        Arc::clone(shared_cache),
    ));
    CachedRequestBuilders::add_request_builder(
        NormalizedPath::V1GetIdLists,
        idlist_request_builder,
    );
    let id_list_store = Arc::new(datastore::id_list_store::GetIdListStore::new(
        sdk_key_store.clone(),
        background_data_provider.clone(),
    ));
    idlist_observer.add_observer(sdk_key_store.clone()).await;
    idlist_observer.add_observer(id_list_store.clone()).await;
    idlist_observer.add_observer(Arc::clone(shared_cache)).await;

    id_list_store
}

async fn create_id_list_file_store(
    overrides: &ConfigurationAndOverrides,
    background_data_provider: Arc<background_data_provider::BackgroundDataProvider>,
) -> Arc<IdListFileStore> {
    let id_list_file_observer = Arc::new(HttpDataProviderObserver::new());
    let id_list_file_request_builder = Arc::new(IdListFileRequestBuilder::new(
        resolve_download_id_list_file_base_url(overrides),
        Arc::clone(&id_list_file_observer),
        Arc::new(disabled_cache::DisabledCache::default()),
    ));
    CachedRequestBuilders::add_request_builder(
        NormalizedPath::V1DownloadIdListFile,
        id_list_file_request_builder,
    );

    let id_list_file_store = Arc::new(IdListFileStore::new(background_data_provider));
    id_list_file_observer
        .add_observer(id_list_file_store.clone())
        .await;

    id_list_file_store
}

fn resolve_download_id_list_file_base_url(overrides: &ConfigurationAndOverrides) -> String {
    overrides
        .statsig_endpoint_download_id_list_file
        .as_ref()
        .or(overrides.statsig_endpoint.as_ref())
        .map_or("https://api.statsigcdn.com".to_string(), |s| s.to_string())
}

fn create_dcs_blob_config(overrides: &ConfigurationAndOverrides) -> Option<DcsBlobConfig> {
    if !overrides
        .enable_blob_storage_for_download_config_specs
        .unwrap_or(false)
    {
        return None;
    }

    let connection_string = overrides.dcs_blob_storage_connection_string.as_ref()?;
    let company_id = overrides.dcs_blob_storage_company_id.as_ref()?;
    if company_id.trim().is_empty() {
        eprintln!("Failed to initialize DCS blob URL generator: company id is empty");
        return None;
    }
    match DcsBlobUrlGenerator::from_connection_string(connection_string) {
        Ok(generator) => Some(DcsBlobConfig {
            url_generator: Arc::new(generator),
            company_id: company_id.clone(),
        }),
        Err(error) => {
            eprintln!("Failed to initialize DCS blob URL generator: {error}");
            None
        }
    }
}

async fn maybe_create_deltas_store(
    cli: &Cli,
    overrides: &ConfigurationAndOverrides,
    config_spec_observer: Arc<HttpDataProviderObserver>,
    http_client_config: HttpClientConfig,
) -> Option<(
    Arc<DeltasStore>,
    Arc<background_data_provider::BackgroundDataProvider>,
)> {
    if !cli.deltas_background_loop_enabled {
        return None;
    }

    let deltas_polling_interval_in_s = cli
        .deltas_background_loop_sleep_time_in_s
        .unwrap_or(cli.polling_interval_in_s);
    let deltas_num_responses_to_persist = cli.deltas_num_responses_to_persist.unwrap_or(20);
    let deltas_sdk_key_store = Arc::new(sdk_key_store::SdkKeyStore::new());
    let deltas_http_provider = Arc::new(http_data_provider::HttpDataProvider {});
    let deltas_background_data_provider =
        Arc::new(background_data_provider::BackgroundDataProvider::new(
            deltas_http_provider,
            deltas_polling_interval_in_s,
            Arc::clone(&deltas_sdk_key_store),
            cli.clear_datastore_on_unauthorized,
            http_client_config,
            background_data_provider::BackgroundPollDispatchConfig::new(
                cli.max_in_flight,
                cli.background_poll_item_spacing_ms,
            ),
        ));

    let deltas_observer = Arc::new(HttpDataProviderObserver::new());
    let disabled_cache: Arc<dyn HttpDataProviderObserverTrait + Send + Sync> =
        Arc::new(disabled_cache::DisabledCache::default());
    let deltas_request_builder = Arc::new(DcsRequestBuilder::new(
        overrides
            .statsig_endpoint_deltas
            .as_ref()
            .map_or("https://api.statsigcdn.com".to_string(), |s| s.to_string()),
        Arc::clone(&deltas_observer),
        disabled_cache,
        None,
    ));
    CachedRequestBuilders::add_request_builder(
        NormalizedPath::V2DownloadConfigSpecsDeltas,
        deltas_request_builder,
    );

    let deltas_store = Arc::new(DeltasStore::new(
        Arc::clone(&deltas_sdk_key_store),
        Arc::clone(&deltas_background_data_provider),
        deltas_num_responses_to_persist,
    ));
    deltas_observer
        .add_observer(
            Arc::clone(&deltas_store) as Arc<dyn HttpDataProviderObserverTrait + Send + Sync>
        )
        .await;
    config_spec_observer
        .add_observer(
            Arc::clone(&deltas_store) as Arc<dyn HttpDataProviderObserverTrait + Send + Sync>
        )
        .await;

    Some((deltas_store, deltas_background_data_provider))
}

#[rocket::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env::set_var("RUST_BACKTRACE", "1");

    let cli = Cli::parse();
    let overrides = envy::from_env::<ConfigurationAndOverrides>().expect("Envy Error");

    println!("[SFP] Checking to initialize Statsig SDK and Profiling...");
    try_initialize_statsig_sdk_and_profiling(&cli, &overrides).await;

    println!("[SFP] Initializing event observers...");
    if cli.datadog_logging || cli.statsd_logging || cli.statsig_logging || cli.otlp_logging {
        let stats_logger = Arc::new(
            stats_logger::StatsLogger::new(
                cli.statsd_logging,
                cli.datadog_logging,
                cli.statsig_logging,
                cli.otlp_logging,
            )
            .await,
        );
        ProxyEventObserver::add_observer(stats_logger).await;
    }
    if cli.debug_logging {
        let debug_logger = Arc::new(debug_logger::DebugLogger::new());
        ProxyEventObserver::add_observer(debug_logger).await;
    }

    println!("[SFP] Parsing startup warm-up keys...");
    let warmup_store_items = match startup_warmup_parser::load_startup_warmup_keys_from_env() {
        Ok(Some(startup_warmup_parse_result)) => {
            let valid_count = startup_warmup_parse_result.store_items.len();
            println!(
                "[SFP] Startup warm-up key summary: configured={}, valid={}, invalid={}",
                startup_warmup_parse_result.configured_count,
                valid_count,
                startup_warmup_parse_result.invalid_count,
            );
            Some(startup_warmup_parse_result.store_items)
        }
        Ok(None) => {
            println!("[SFP] No startup warm-up keys configured.");
            None
        }
        Err(err) => {
            eprintln!(
                "[SFP] Failed to parse startup warm-up keys from {}: {err}",
                startup_warmup_parser::STARTUP_WARMUP_KEYS_ENV
            );
            None
        }
    };

    println!("[SFP] Initializing data providers...");
    let shared_http_data_provider = Arc::new(http_data_provider::HttpDataProvider {});
    let sdk_key_store = Arc::new(sdk_key_store::SdkKeyStore::new());
    let background_poll_dispatch_config =
        background_data_provider::BackgroundPollDispatchConfig::new(
            cli.max_in_flight,
            cli.background_poll_item_spacing_ms,
        );
    let http_client_config = cli.http_client_config();
    let background_data_provider = Arc::new(background_data_provider::BackgroundDataProvider::new(
        shared_http_data_provider,
        cli.polling_interval_in_s,
        Arc::clone(&sdk_key_store),
        cli.clear_datastore_on_unauthorized,
        http_client_config,
        background_poll_dispatch_config,
    ));
    let cache_uuid = Uuid::new_v4().to_string();

    println!("[SFP] Initializing caches...");
    let redis_cache: Arc<dyn HttpDataProviderObserverTrait + Send + Sync> = match cli.cache {
        CacheMode::Redis => Arc::new(
            redis_cache::RedisCache::new(
                cli.redis_leader_key_ttl,
                &cache_uuid,
                true, /* check lcut */
                cli.redis_cache_ttl_in_s,
                cli.double_write_cache_for_legacy_key,
                cli.redis_connection_timeout_in_s,
            )
            .await,
        ),
        CacheMode::Disabled => Arc::new(disabled_cache::DisabledCache::default()),
    };
    let idlist_redis_cache: Arc<dyn HttpDataProviderObserverTrait + Send + Sync> = match cli.cache {
        CacheMode::Redis => Arc::new(
            redis_cache::RedisCache::new(
                cli.redis_leader_key_ttl,
                &cache_uuid,
                true, /* check lcut */
                cli.redis_cache_ttl_in_s,
                cli.double_write_cache_for_legacy_key,
                cli.redis_connection_timeout_in_s,
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
        &overrides,
        Arc::clone(&background_data_provider),
        Arc::clone(&idlist_observer),
        &idlist_redis_cache,
        &sdk_key_store,
    )
    .await;
    let id_list_file_store =
        create_id_list_file_store(&overrides, Arc::clone(&background_data_provider)).await;
    if cli.id_list_file_refresh_observer_enabled {
        println!("[SFP] Enabling id-list file refresh observer...");
        idlist_observer
            .add_observer(Arc::new(IdListFileRefreshObserver::new(Arc::clone(
                &id_list_file_store,
            ))))
            .await;
    }
    let deltas_runtime = maybe_create_deltas_store(
        &cli,
        &overrides,
        Arc::clone(&config_spec_observer),
        http_client_config,
    )
    .await;
    let deltas_store = deltas_runtime.as_ref().map(|(store, _)| Arc::clone(store));
    let default_background_worker_threads = std::thread::available_parallelism()
        .map(|threads| threads.get())
        .unwrap_or(1);
    let background_worker_threads = overrides
        .tokio_worker_threads_background
        .unwrap_or(default_background_worker_threads);
    if background_worker_threads == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "TOKIO_WORKER_THREADS_BACKGROUND must be greater than 0",
        )
        .into());
    }
    println!(
        "[SFP] Starting dedicated background runtime with {background_worker_threads} worker thread(s) (TOKIO_WORKER_THREADS_BACKGROUND)..."
    );
    let background_runtime = BackgroundRuntimeGuard::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(background_worker_threads)
            .enable_all()
            .thread_name("sfp-bg")
            .build()?,
    );
    let background_runtime_handle = background_runtime.handle();

    println!("[SFP] Starting background thread...");
    background_data_provider
        .start_background_thread(warmup_store_items, &background_runtime_handle)
        .await;
    if let Some((_, deltas_background_data_provider)) = &deltas_runtime {
        deltas_background_data_provider
            .start_background_thread(None, &background_runtime_handle)
            .await;
    }
    let rc_cache = Arc::new(AuthorizedRequestContextCache::new());
    // Default buffer size is 20000 messages
    let http_client = reqwest::Client::builder()
        .user_agent(OUTBOUND_USER_AGENT)
        .timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(cli.http_connection_pool_max_idle_per_host)
        .build()
        .expect("We must have an http client");
    let log_event_store = create_log_event_store(http_client.clone(), &overrides).await;

    nginx_cache_monitor::NginxCacheMonitor::start_monitoring().await;

    println!("[SFP] Initializing Servers...");
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
                servers::http_server::HttpServerDependencies {
                    config_spec_store,
                    deltas_store: deltas_store.clone(),
                    log_event_store,
                    id_list_store,
                    id_list_file_store,
                    rc_cache,
                    sdk_key_store: sdk_key_store.clone(),
                },
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
                servers::http_server::HttpServerDependencies {
                    config_spec_store,
                    deltas_store: deltas_store.clone(),
                    log_event_store,
                    id_list_store,
                    id_list_file_store,
                    rc_cache,
                    sdk_key_store: sdk_key_store.clone(),
                },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_overrides(
        statsig_endpoint: Option<&str>,
        statsig_endpoint_download_id_list_file: Option<&str>,
    ) -> ConfigurationAndOverrides {
        ConfigurationAndOverrides {
            statsig_endpoint: statsig_endpoint.map(str::to_string),
            statsig_endpoint_deltas: None,
            statsig_endpoint_download_id_list_file: statsig_endpoint_download_id_list_file
                .map(str::to_string),
            enable_blob_storage_for_download_config_specs: None,
            dcs_blob_storage_connection_string: None,
            dcs_blob_storage_company_id: None,
            statsig_server_sdk_key: None,
            log_event_statsig_endpoint: None,
            log_event_dedupe_cache_limit: None,
            tokio_worker_threads_background: None,
        }
    }

    #[test]
    fn cli_http_client_config_defaults_match_existing_values() {
        let cli = Cli::parse_from(["server", "http", "disabled"]);

        assert_eq!(cli.http_client_config(), HttpClientConfig::default());
    }

    #[test]
    fn cli_http_client_config_uses_explicit_override_values() {
        let cli = Cli::parse_from([
            "server",
            "http",
            "disabled",
            "--http-connection-pool-max-idle-per-host",
            "25",
            "--http-request-timeout-in-s",
            "45",
            "--http-read-timeout-in-s",
            "20",
            "--http-connect-timeout-in-s",
            "5",
            "--http-connection-pool-idle-timeout-in-s",
            "15",
        ]);

        assert_eq!(
            cli.http_client_config(),
            HttpClientConfig::new(45, 20, 5, 15, 25)
        );
    }

    #[test]
    fn cli_accepts_update_batch_size_as_alias_for_max_in_flight() {
        let cli = Cli::parse_from(["server", "http", "disabled", "--update-batch-size", "32"]);

        assert_eq!(cli.max_in_flight, 32);
    }

    #[test]
    fn cli_disables_id_list_file_refresh_observer_by_default() {
        let cli = Cli::parse_from(["server", "http", "disabled"]);

        assert!(!cli.id_list_file_refresh_observer_enabled);
    }

    #[test]
    fn cli_enables_id_list_file_refresh_observer_with_flag() {
        let cli = Cli::parse_from([
            "server",
            "http",
            "disabled",
            "--id-list-file-refresh-observer-enabled",
        ]);

        assert!(cli.id_list_file_refresh_observer_enabled);
    }

    #[test]
    fn download_id_list_file_base_url_uses_specific_override_first() {
        let overrides = make_overrides(
            Some("https://api.statsigcdn.com"),
            Some("https://idliststorage.blob.core.windows.net/idlists"),
        );
        assert_eq!(
            resolve_download_id_list_file_base_url(&overrides),
            "https://idliststorage.blob.core.windows.net/idlists"
        );
    }

    #[test]
    fn download_id_list_file_base_url_falls_back_to_statsig_endpoint() {
        let overrides = make_overrides(Some("https://proxy.example.com"), None);
        assert_eq!(
            resolve_download_id_list_file_base_url(&overrides),
            "https://proxy.example.com"
        );
    }

    #[test]
    fn download_id_list_file_base_url_falls_back_to_legacy_default() {
        let overrides = make_overrides(None, None);
        assert_eq!(
            resolve_download_id_list_file_base_url(&overrides),
            "https://api.statsigcdn.com"
        );
    }

    #[test]
    fn create_dcs_blob_config_returns_none_for_empty_company_id() {
        let mut overrides = make_overrides(None, None);
        overrides.enable_blob_storage_for_download_config_specs = Some(true);
        overrides.dcs_blob_storage_connection_string = Some("DefaultEndpointsProtocol=https;AccountName=idliststorage;AccountKey=ZmFrZS1rZXk=;EndpointSuffix=core.windows.net".to_string());
        overrides.dcs_blob_storage_company_id = Some("   ".to_string());

        assert!(create_dcs_blob_config(&overrides).is_none());
    }
}
