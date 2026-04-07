use crate::datastore::sdk_key_store::{SdkKeyStore, SdkKeyStoreItem};
use crate::servers::authorized_request_context::AuthorizedRequestContext;
use crate::utils::compress_encoder::CompressionEncoder;
use crate::GRACEFUL_SHUTDOWN_TOKEN;

use super::background_poll_dispatch::dispatch_rank;
use super::http_data_provider::ResponsePayload;
use super::request_backoff::{RequestBackoffController, RequestBackoffKey, RequestBackoffPolicy};
use super::request_builder::{CachedRequestBuilders, RequestBuilderTrait};
use super::{http_data_provider::HttpDataProvider, DataProviderRequestResult, DataProviderTrait};
use super::{FullRequestContext, HttpClientConfig, LazyHttpClient, ResponseContext};
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::RwLock;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio::time::{Duration, Instant};

use dashmap::DashMap;

pub use super::background_poll_dispatch::BackgroundPollDispatchConfig;

type ForegroundFetchLockKey = RequestBackoffKey;

#[derive(Clone, Copy, Debug)]
enum FetchExecutionMode {
    Foreground,
    BackgroundPoll { poll_cycle: u64 },
}

#[derive(Clone, Copy, Debug, Default)]
struct FetchExecutionSummary {
    attempted_count: usize,
}

struct PreparedFetchItem {
    item: SdkKeyStoreItem,
    request_builder: Arc<dyn RequestBuilderTrait>,
    request_key: ForegroundFetchLockKey,
}

#[derive(Clone, Copy, Debug)]
struct PreparedFetchExecutionConfig {
    max_in_flight: usize,
    launch_spacing: Duration,
    request_guard_timeout: Duration,
    clear_datastore_on_unauthorized: bool,
}

#[derive(Clone, Copy, Debug)]
struct ForegroundFetchConfig {
    request_guard_timeout: Duration,
    clear_datastore_on_unauthorized: bool,
    execution_mode: FetchExecutionMode,
    background_poll_dispatch_config: BackgroundPollDispatchConfig,
}

pub struct BackgroundDataProvider {
    http_data_prover: Arc<HttpDataProvider>,
    http_client: LazyHttpClient,
    polling_interval_in_s: u64,
    sdk_key_store: Arc<SdkKeyStore>,
    foreground_fetch_lock: DashMap<ForegroundFetchLockKey, Arc<RwLock<bool>>>,
    clear_datastore_on_unauthorized: bool,
    request_backoff_controller: Arc<RequestBackoffController>,
    background_poll_dispatch_config: BackgroundPollDispatchConfig,
}

pub async fn foreground_fetch(
    bdp: Arc<BackgroundDataProvider>,
    request_context: &Arc<AuthorizedRequestContext>,
    since_time: u64,
    clear_datastore_on_unauthorized: bool,
) {
    let key = (Arc::clone(request_context), None);
    let lock_ref: Arc<RwLock<bool>> = bdp
        .foreground_fetch_lock
        .entry(key.clone())
        .or_insert_with(|| Arc::new(RwLock::new(false)))
        .clone();

    // If the value is false, that means no one has fetched yet, so we should attempt to fetch.
    if !*lock_ref.read().await {
        let mut per_key_lock = lock_ref.write().await;

        // Double-check in case another thread updated while we were waiting for the write lock
        if !*per_key_lock {
            *per_key_lock = true;

            let summary = BackgroundDataProvider::impl_foreground_fetch_with_cached_client(
                vec![SdkKeyStoreItem {
                    request_context: Arc::clone(request_context),
                    lcut: since_time,
                }],
                &bdp.http_data_prover,
                clear_datastore_on_unauthorized,
                &bdp.http_client,
                Arc::clone(&bdp.request_backoff_controller),
                FetchExecutionMode::Foreground,
                bdp.background_poll_dispatch_config,
            )
            .await;

            if summary.attempted_count == 0 {
                *per_key_lock = false;
                drop(per_key_lock);
                bdp.foreground_fetch_lock.remove(&key);
                return;
            }

            // Key eviction: Remove this lock from the cache after 30 seconds
            // This effectively rate limits requests to once per 30 seconds
            let graceful_shutdown_token = GRACEFUL_SHUTDOWN_TOKEN.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = async {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        bdp.foreground_fetch_lock.remove(&key);
                    } => {},
                    _ = graceful_shutdown_token.cancelled() => {},
                }
            });
        }
    }
}

impl BackgroundDataProvider {
    pub fn new(
        data_provider: Arc<HttpDataProvider>,
        polling_interval_in_s: u64,
        sdk_key_store: Arc<SdkKeyStore>,
        clear_datastore_on_unauthorized: bool,
        http_client_config: HttpClientConfig,
        background_poll_dispatch_config: BackgroundPollDispatchConfig,
    ) -> Self {
        BackgroundDataProvider {
            http_data_prover: data_provider,
            http_client: LazyHttpClient::new(http_client_config),
            polling_interval_in_s,
            foreground_fetch_lock: DashMap::new(),
            sdk_key_store,
            clear_datastore_on_unauthorized,
            request_backoff_controller: Arc::new(RequestBackoffController::new(
                RequestBackoffPolicy::from_polling_interval(polling_interval_in_s),
            )),
            background_poll_dispatch_config,
        }
    }

    pub fn http_client_config(&self) -> HttpClientConfig {
        self.http_client.config()
    }

    async fn impl_foreground_fetch_with_cached_client(
        store_iter: Vec<SdkKeyStoreItem>,
        data_provider: &Arc<HttpDataProvider>,
        clear_datastore_on_unauthorized: bool,
        http_client: &LazyHttpClient,
        request_backoff_controller: Arc<RequestBackoffController>,
        execution_mode: FetchExecutionMode,
        background_poll_dispatch_config: BackgroundPollDispatchConfig,
    ) -> FetchExecutionSummary {
        let fetch_config = ForegroundFetchConfig {
            request_guard_timeout: http_client.background_request_guard_timeout(),
            clear_datastore_on_unauthorized,
            execution_mode,
            background_poll_dispatch_config,
        };
        match http_client.get_or_build() {
            Ok(built_http_client) => {
                Self::impl_foreground_fetch(
                    store_iter,
                    data_provider,
                    built_http_client.as_ref(),
                    request_backoff_controller,
                    fetch_config,
                )
                .await
            }
            Err(e) => {
                eprintln!(
                    "Failed to build http client for background data provider; skipping fetch and retrying later: {e}"
                );
                FetchExecutionSummary::default()
            }
        }
    }

    pub async fn start_background_thread(
        &self,
        startup_warmup_items: Option<Vec<SdkKeyStoreItem>>,
        runtime_handle: &tokio::runtime::Handle,
    ) {
        if let Some(store_items) = startup_warmup_items {
            if !store_items.is_empty() {
                BackgroundDataProvider::impl_foreground_fetch_with_cached_client(
                    store_items,
                    &self.http_data_prover,
                    self.clear_datastore_on_unauthorized,
                    &self.http_client,
                    Arc::clone(&self.request_backoff_controller),
                    FetchExecutionMode::Foreground,
                    self.background_poll_dispatch_config,
                )
                .await;
            }
        }

        let shared_data_provider = self.http_data_prover.clone();
        let polling_interval_in_s = self.polling_interval_in_s;
        let sdk_key_store = Arc::clone(&self.sdk_key_store);
        let graceful_shutdown_token = GRACEFUL_SHUTDOWN_TOKEN.clone();
        let clear_datastore_on_unauthorized = self.clear_datastore_on_unauthorized;
        let http_client = self.http_client.clone();
        let request_backoff_controller = Arc::clone(&self.request_backoff_controller);
        let background_poll_dispatch_config = self.background_poll_dispatch_config;
        runtime_handle.spawn(async move {
            tokio::select! {
                _ = BackgroundDataProvider::start_background_fetch_interval(
                    &sdk_key_store,
                    &shared_data_provider,
                    polling_interval_in_s,
                    clear_datastore_on_unauthorized,
                    http_client,
                    request_backoff_controller,
                    background_poll_dispatch_config,
                ) => {},
                _ = graceful_shutdown_token.cancelled() => {},
            }
        });
    }

    async fn start_background_fetch_interval(
        sdk_key_store: &Arc<SdkKeyStore>,
        shared_data_provider: &Arc<HttpDataProvider>,
        polling_interval_in_s: u64,
        clear_datastore_on_unauthorized: bool,
        http_client: LazyHttpClient,
        request_backoff_controller: Arc<RequestBackoffController>,
        background_poll_dispatch_config: BackgroundPollDispatchConfig,
    ) {
        let base_poll_interval = Duration::from_secs(polling_interval_in_s.max(1));
        let mut next_poll_at = tokio::time::Instant::now();
        let mut poll_cycle = 0_u64;
        loop {
            tokio::time::sleep_until(next_poll_at).await;
            BackgroundDataProvider::impl_foreground_fetch_with_cached_client(
                sdk_key_store.get_registered_store(),
                shared_data_provider,
                clear_datastore_on_unauthorized,
                &http_client,
                Arc::clone(&request_backoff_controller),
                FetchExecutionMode::BackgroundPoll { poll_cycle },
                background_poll_dispatch_config,
            )
            .await;
            poll_cycle = poll_cycle.wrapping_add(1);
            next_poll_at += background_poll_dispatch_config
                .poll_interval_for_cycle(base_poll_interval, poll_cycle);
            request_backoff_controller.prune_stale();
        }
    }

    async fn impl_foreground_fetch(
        store_iter: Vec<SdkKeyStoreItem>,
        data_provider: &Arc<HttpDataProvider>,
        http_client: &reqwest::Client,
        request_backoff_controller: Arc<RequestBackoffController>,
        fetch_config: ForegroundFetchConfig,
    ) -> FetchExecutionSummary {
        let mut summary = FetchExecutionSummary::default();
        let mut prepared_requests =
            Vec::with_capacity(fetch_config.background_poll_dispatch_config.max_in_flight);

        for item in store_iter {
            let request_builder =
                CachedRequestBuilders::get_request_builder(&item.request_context.path);
            if !request_builder
                .should_make_request(&item.request_context)
                .await
            {
                continue;
            }

            let request_key = (Arc::clone(&item.request_context), None);
            if !request_backoff_controller.can_attempt(&request_key) {
                continue;
            }

            prepared_requests.push(PreparedFetchItem {
                item,
                request_builder,
                request_key,
            });
        }

        let prepared_request_count = prepared_requests.len();
        if let FetchExecutionMode::BackgroundPoll { poll_cycle } = fetch_config.execution_mode {
            if fetch_config
                .background_poll_dispatch_config
                .background_dispatch_shaping_enabled(prepared_request_count)
            {
                prepared_requests.sort_by_key(|prepared| {
                    dispatch_rank(
                        &prepared.request_key,
                        poll_cycle,
                        fetch_config.background_poll_dispatch_config.jitter_seed(),
                    )
                });
            }
        }

        let execution_config = PreparedFetchExecutionConfig {
            max_in_flight: fetch_config.background_poll_dispatch_config.max_in_flight,
            launch_spacing: match fetch_config.execution_mode {
                FetchExecutionMode::BackgroundPoll { .. } => fetch_config
                    .background_poll_dispatch_config
                    .launch_spacing(),
                FetchExecutionMode::Foreground => Duration::ZERO,
            },
            request_guard_timeout: fetch_config.request_guard_timeout,
            clear_datastore_on_unauthorized: fetch_config.clear_datastore_on_unauthorized,
        };

        summary.attempted_count = Self::execute_prepared_requests_streaming(
            prepared_requests,
            Arc::clone(data_provider),
            http_client.clone(),
            Arc::clone(&request_backoff_controller),
            execution_config,
        )
        .await;
        summary
    }

    async fn execute_prepared_requests_streaming(
        prepared_requests: Vec<PreparedFetchItem>,
        data_provider: Arc<HttpDataProvider>,
        http_client: reqwest::Client,
        request_backoff_controller: Arc<RequestBackoffController>,
        execution_config: PreparedFetchExecutionConfig,
    ) -> usize {
        let mut in_flight = JoinSet::new();
        let mut pending_items = prepared_requests.into_iter();
        let mut attempted_count = 0usize;
        let mut next_launch_at = Instant::now();

        loop {
            while in_flight.len() < execution_config.max_in_flight {
                let Some(prepared_request) = pending_items.next() else {
                    break;
                };

                Self::wait_for_launch_slot(&mut next_launch_at, execution_config.launch_spacing)
                    .await;
                Self::spawn_prepared_request(
                    &mut in_flight,
                    prepared_request,
                    Arc::clone(&data_provider),
                    http_client.clone(),
                    Arc::clone(&request_backoff_controller),
                    execution_config,
                );
                attempted_count += 1;
            }

            if in_flight.is_empty() {
                break;
            }

            if let Some(Err(err)) = in_flight.join_next().await {
                eprintln!("Background fetch task failed: {err}");
            }
        }

        attempted_count
    }

    async fn wait_for_launch_slot(next_launch_at: &mut Instant, launch_spacing: Duration) {
        if launch_spacing.is_zero() {
            return;
        }

        let launch_at = (*next_launch_at).max(Instant::now());
        tokio::time::sleep_until(launch_at).await;
        *next_launch_at = launch_at + launch_spacing;
    }

    fn spawn_prepared_request(
        in_flight: &mut JoinSet<()>,
        prepared_request: PreparedFetchItem,
        data_provider: Arc<HttpDataProvider>,
        http_client: reqwest::Client,
        request_backoff_controller: Arc<RequestBackoffController>,
        execution_config: PreparedFetchExecutionConfig,
    ) {
        let PreparedFetchItem {
            item,
            request_builder,
            request_key,
        } = prepared_request;

        in_flight.spawn(async move {
            match timeout(
                execution_config.request_guard_timeout,
                Self::process_request(
                    data_provider,
                    request_builder,
                    &Arc::new(FullRequestContext {
                        authorized_request_context: Arc::clone(&item.request_context),
                    }),
                    item.lcut,
                    &http_client,
                    execution_config.clear_datastore_on_unauthorized,
                ),
            )
            .await
            {
                Ok(result) => {
                    request_backoff_controller.record_result(&request_key, result);
                }
                Err(_) => {
                    request_backoff_controller.record_failure(&request_key);
                    let mut key = item.request_context.sdk_key.clone();
                    key.truncate(20);
                    eprintln!(
                        "Error: process_request timed out after {:?} for request_context.. skipping update..({}): {}",
                        execution_config.request_guard_timeout,
                        key,
                        item.request_context.path
                    )
                }
            }
        });
    }

    async fn process_request(
        data_provider: Arc<HttpDataProvider>,
        request_builder: Arc<dyn RequestBuilderTrait>,
        request_context: &Arc<FullRequestContext>,
        lcut: u64,
        http_client: &reqwest::Client,
        clear_datastore_on_unauthorized: bool,
    ) -> DataProviderRequestResult {
        let dp_result = data_provider
            .get(
                http_client,
                &request_builder,
                &request_context.authorized_request_context,
                lcut,
            )
            .await;

        match dp_result.result {
            DataProviderRequestResult::DataAvailable => {
                if let Some(data) = dp_result.body {
                    if !request_context.authorized_request_context.use_lcut
                        || lcut != dp_result.lcut
                    {
                        Self::notify_observers(
                            request_context,
                            &Arc::new(ResponseContext {
                                result_type: dp_result.result,
                                lcut: dp_result.lcut,
                                request_since_time: lcut,
                                body: data,
                            }),
                            &request_builder,
                        )
                        .await;
                    }
                }
            }
            DataProviderRequestResult::Error => {
                if let Some(backup_data) = request_builder
                    .get_backup_cache()
                    .get(&request_context.authorized_request_context)
                    .await
                {
                    Self::notify_observers(
                        request_context,
                        &Arc::new(ResponseContext {
                            result_type: dp_result.result,
                            lcut: backup_data.lcut,
                            request_since_time: lcut,
                            body: Arc::clone(&backup_data.config),
                        }),
                        &request_builder,
                    )
                    .await;
                }
            }
            DataProviderRequestResult::Unauthorized => {
                if clear_datastore_on_unauthorized {
                    Self::notify_observers(
                        request_context,
                        &Arc::new(ResponseContext {
                            result_type: dp_result.result,
                            lcut,
                            request_since_time: lcut,
                            body: Arc::new(ResponsePayload {
                                encoding: Arc::new(CompressionEncoder::PlainText),
                                data: Arc::new(Bytes::new()),
                                use_proto: false,
                            }),
                        }),
                        &request_builder,
                    )
                    .await;
                }
            }
            DataProviderRequestResult::ClientError => {
                Self::notify_observers(
                    request_context,
                    &Arc::new(ResponseContext {
                        result_type: dp_result.result,
                        lcut,
                        request_since_time: lcut,
                        body: Arc::new(ResponsePayload {
                            encoding: Arc::new(CompressionEncoder::PlainText),
                            data: Arc::new(Bytes::new()),
                            use_proto: false,
                        }),
                    }),
                    &request_builder,
                )
                .await;
            }
            _ => {}
        }

        dp_result.result
    }

    async fn notify_observers(
        request_context: &Arc<FullRequestContext>,
        response_context: &Arc<ResponseContext>,
        request_builder: &Arc<dyn RequestBuilderTrait>,
    ) {
        request_builder
            .get_observers()
            .notify_all(request_context, response_context)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::ForegroundFetchLockKey;
    use super::{BackgroundDataProvider, BackgroundPollDispatchConfig};
    use super::{FetchExecutionMode, RequestBackoffController, RequestBackoffPolicy};
    use crate::datastore::data_providers::http_data_provider::HttpDataProvider;
    use crate::datastore::data_providers::{HttpClientConfig, LazyHttpClient};
    use crate::datastore::sdk_key_store::{SdkKeyStore, SdkKeyStoreItem};
    use crate::servers::authorized_request_context::AuthorizedRequestContext;
    use crate::servers::normalized_path::NormalizedPath;
    use crate::utils::compress_encoder::CompressionEncoder;
    use dashmap::DashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::{Mutex, Notify, RwLock};
    use tokio::task::JoinSet;
    use tokio::time::{Duration, Instant};

    fn record_max(max_seen: &AtomicUsize, current: usize) {
        let mut observed = max_seen.load(Ordering::SeqCst);
        while observed < current {
            match max_seen.compare_exchange(observed, current, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return,
                Err(actual) => observed = actual,
            }
        }
    }

    async fn wait_until(counter: &AtomicUsize, expected: usize) {
        for _ in 0..100 {
            if counter.load(Ordering::SeqCst) >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }

        panic!("counter did not reach {expected}");
    }

    async fn run_test_tasks_streaming<F, Fut>(
        items: Vec<u8>,
        max_in_flight: usize,
        launch_spacing: Duration,
        mut spawn_task: F,
    ) -> usize
    where
        F: FnMut(u8) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut in_flight = JoinSet::new();
        let mut pending_items = items.into_iter();
        let mut attempted_count = 0usize;
        let mut next_launch_at = Instant::now();

        loop {
            while in_flight.len() < max_in_flight {
                let Some(item) = pending_items.next() else {
                    break;
                };

                BackgroundDataProvider::wait_for_launch_slot(&mut next_launch_at, launch_spacing)
                    .await;
                in_flight.spawn(spawn_task(item));
                attempted_count += 1;
            }

            if in_flight.is_empty() {
                break;
            }

            let _ = in_flight.join_next().await;
        }

        attempted_count
    }

    #[test]
    fn foreground_fetch_lock_key_is_order_insensitive() {
        let locks: DashMap<ForegroundFetchLockKey, Arc<RwLock<bool>>> = DashMap::new();

        let rc1 = Arc::new(AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::Brotli, CompressionEncoder::Gzip],
        ));
        let rc2 = Arc::new(AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::Gzip, CompressionEncoder::Brotli],
        ));

        let key1 = (Arc::clone(&rc1), None);
        let key2 = (Arc::clone(&rc2), None);
        let lock = Arc::new(RwLock::new(false));
        locks.insert(key1, Arc::clone(&lock));

        let fetched = locks
            .get(&key2)
            .expect("equivalent request context should map to same lock")
            .clone();
        assert!(Arc::ptr_eq(&fetched, &lock));
    }

    #[test]
    fn foreground_fetch_lock_key_reuses_get_id_lists_gzip_variants() {
        let locks: DashMap<ForegroundFetchLockKey, Arc<RwLock<bool>>> = DashMap::new();

        let rc1 = Arc::new(AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1GetIdLists,
            vec![CompressionEncoder::Gzip, CompressionEncoder::PlainText],
        ));
        let rc2 = Arc::new(AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1GetIdLists,
            vec![CompressionEncoder::Identity, CompressionEncoder::Gzip],
        ));

        let key1 = (Arc::clone(&rc1), None);
        let key2 = (Arc::clone(&rc2), None);
        let lock = Arc::new(RwLock::new(false));
        locks.insert(key1, Arc::clone(&lock));

        let fetched = locks
            .get(&key2)
            .expect("equivalent gzip-capable get_id_lists context should map to same lock")
            .clone();
        assert!(Arc::ptr_eq(&fetched, &lock));
    }

    #[test]
    fn foreground_fetch_lock_key_ignores_id_list_file_range_start() {
        let locks: DashMap<ForegroundFetchLockKey, Arc<RwLock<bool>>> = DashMap::new();

        let first_range_context = Arc::new(
            AuthorizedRequestContext::new(
                "secret-key".to_string(),
                NormalizedPath::V1DownloadIdListFile,
                vec![CompressionEncoder::PlainText],
            )
            .with_id_list_request(Some("file_123".to_string()), Some(270), Some(512)),
        );
        let second_range_context = Arc::new(
            AuthorizedRequestContext::new(
                "secret-key".to_string(),
                NormalizedPath::V1DownloadIdListFile,
                vec![CompressionEncoder::PlainText],
            )
            .with_id_list_request(Some("file_123".to_string()), Some(1024), Some(512)),
        );

        let key1 = (Arc::clone(&first_range_context), None);
        let key2 = (Arc::clone(&second_range_context), None);
        let lock = Arc::new(RwLock::new(false));
        locks.insert(key1, Arc::clone(&lock));

        let fetched = locks
            .get(&key2)
            .expect("different ranges for the same required full file should share a fetch lock")
            .clone();
        assert!(Arc::ptr_eq(&fetched, &lock));
    }

    #[test]
    fn foreground_fetch_lock_key_distinguishes_id_list_file_required_size() {
        let locks: DashMap<ForegroundFetchLockKey, Arc<RwLock<bool>>> = DashMap::new();

        let stale_size_context = Arc::new(
            AuthorizedRequestContext::new(
                "secret-key".to_string(),
                NormalizedPath::V1DownloadIdListFile,
                vec![CompressionEncoder::PlainText],
            )
            .with_id_list_request(Some("file_123".to_string()), Some(270), Some(512)),
        );
        let updated_size_context = Arc::new(
            AuthorizedRequestContext::new(
                "secret-key".to_string(),
                NormalizedPath::V1DownloadIdListFile,
                vec![CompressionEncoder::PlainText],
            )
            .with_id_list_request(Some("file_123".to_string()), Some(270), Some(2048)),
        );

        let key1 = (Arc::clone(&stale_size_context), None);
        let key2 = (Arc::clone(&updated_size_context), None);
        let lock = Arc::new(RwLock::new(false));
        locks.insert(key1, lock);

        assert!(
            locks.get(&key2).is_none(),
            "different required file sizes must not share a foreground fetch lock"
        );
    }

    #[test]
    fn background_data_provider_stores_http_client_config() {
        let config = HttpClientConfig::new(45, 20, 5, 30, 25);
        let provider = BackgroundDataProvider::new(
            Arc::new(HttpDataProvider {}),
            10,
            Arc::new(SdkKeyStore::new()),
            false,
            config,
            BackgroundPollDispatchConfig::new(1, 0),
        );

        assert_eq!(provider.http_client_config(), config);
    }

    #[tokio::test]
    async fn impl_foreground_fetch_can_reuse_a_single_http_client_across_cycles() {
        let data_provider = Arc::new(HttpDataProvider {});
        let http_client = LazyHttpClient::new(HttpClientConfig::default());
        let request_backoff_controller = Arc::new(RequestBackoffController::new(
            RequestBackoffPolicy::from_polling_interval(10),
        ));

        let store_items = vec![SdkKeyStoreItem {
            request_context: Arc::new(AuthorizedRequestContext::new(
                "secret-key".to_string(),
                NormalizedPath::RawPath("/tests/noop".to_string()),
                vec![CompressionEncoder::PlainText],
            )),
            lcut: 0,
        }];

        let first_cycle = BackgroundDataProvider::impl_foreground_fetch_with_cached_client(
            store_items.clone(),
            &data_provider,
            false,
            &http_client,
            Arc::clone(&request_backoff_controller),
            FetchExecutionMode::Foreground,
            BackgroundPollDispatchConfig::new(1, 0),
        )
        .await;

        let second_cycle = BackgroundDataProvider::impl_foreground_fetch_with_cached_client(
            store_items,
            &data_provider,
            false,
            &http_client,
            request_backoff_controller,
            FetchExecutionMode::Foreground,
            BackgroundPollDispatchConfig::new(1, 0),
        )
        .await;

        assert!(http_client.is_built());
        assert_eq!(first_cycle.attempted_count, 0);
        assert_eq!(second_cycle.attempted_count, 0);
    }

    #[tokio::test]
    async fn run_streaming_tasks_refills_capacity_as_tasks_finish() {
        let started = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let first_gate = Arc::new(Notify::new());
        let second_gate = Arc::new(Notify::new());

        let task = tokio::spawn({
            let started = Arc::clone(&started);
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            let first_gate = Arc::clone(&first_gate);
            let second_gate = Arc::clone(&second_gate);

            async move {
                run_test_tasks_streaming(vec![0_u8, 1, 2], 2, Duration::ZERO, move |item| {
                    let started = Arc::clone(&started);
                    let active = Arc::clone(&active);
                    let max_active = Arc::clone(&max_active);
                    let first_gate = Arc::clone(&first_gate);
                    let second_gate = Arc::clone(&second_gate);

                    async move {
                        started.fetch_add(1, Ordering::SeqCst);
                        let current_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                        record_max(&max_active, current_active);

                        match item {
                            0 => first_gate.notified().await,
                            1 => second_gate.notified().await,
                            _ => {}
                        }

                        active.fetch_sub(1, Ordering::SeqCst);
                    }
                })
                .await
            }
        });

        wait_until(&started, 2).await;
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(max_active.load(Ordering::SeqCst), 2);

        first_gate.notify_one();
        wait_until(&started, 3).await;
        assert_eq!(max_active.load(Ordering::SeqCst), 2);

        second_gate.notify_one();
        assert_eq!(task.await.unwrap(), 3);
    }

    #[tokio::test]
    async fn run_streaming_tasks_applies_launch_spacing_between_starts() {
        let launch_times = Arc::new(Mutex::new(Vec::new()));

        let task = tokio::spawn({
            let launch_times = Arc::clone(&launch_times);

            async move {
                run_test_tasks_streaming(
                    vec![0_u8, 1, 2],
                    3,
                    Duration::from_millis(50),
                    move |_| {
                        let launch_times = Arc::clone(&launch_times);

                        async move {
                            launch_times.lock().await.push(Instant::now());
                        }
                    },
                )
                .await
            }
        });

        assert_eq!(task.await.unwrap(), 3);

        let launch_times = launch_times.lock().await.clone();
        assert_eq!(launch_times.len(), 3);
        assert!(launch_times[1].duration_since(launch_times[0]) >= Duration::from_millis(45));
        assert!(launch_times[2].duration_since(launch_times[1]) >= Duration::from_millis(45));
    }
}
