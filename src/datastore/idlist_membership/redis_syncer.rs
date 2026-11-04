//! Idlist sync producer (B2) and shared sync infrastructure for the B3 worker.
//!
//! Once per sync-interval window, exactly one pod (the producer-lock winner) turns the
//! `/v1/get_id_lists` **manifest** into an ordered, deduped Redis task queue. Workers (B3)
//! consume the queue; this module does no downloading.
//!
//! Load work is **Full** vs **Delta**. Full is “download this file from byte 0” (never
//! synced, a new `file_id`, or Redis recovery). Delta is the
//! normal append path: same file, `size` grew — resume from `state.size`. `task_meta.op`
//! is only `load` | `remove`; Full vs Delta is not stored, and is re-derived by the worker.

use arc_swap::ArcSwap;
use async_trait::async_trait;
use bb8_redis::redis::{AsyncCommands, RedisError};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use super::namespaces::IdListNamespaces;
#[cfg(test)]
use super::schema::hash_company_id;
use super::schema::{
    task_meta_fields, IdListKeySchema, IdListMetadata, ListSyncState, TASK_OP_LOAD, TASK_OP_REMOVE,
};
use super::work_lock::RedisWorkLock;
use crate::datastore::caching::redis_pool::{pool_get_err, RedisPool};
use crate::datastore::config_spec_store::ConfigSpecForCompany;
use crate::datastore::data_providers::{
    DataProviderRequestResult, FullRequestContext, ResponseContext,
};
use crate::datastore::id_list_manifest::{parse_id_list_manifest, IdListManifest};
use crate::datastore::id_list_store::GetIdListStore;
use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::{
    EventStat, HttpDataProviderObserverTrait, OperationType, ProxyEvent, ProxyEventType,
};
use crate::servers::authorized_request_context::{
    AuthorizedRequestContext, AuthorizedRequestContextCache,
};
use crate::servers::normalized_path::NormalizedPath;
use crate::utils::compress_encoder::CompressionEncoder;

mod planning;
pub(crate) use planning::plan_sync_with_reverse_index;
pub use planning::{
    idlist_sync_keys_from_env, needs_work, order_and_dedupe_tasks, producer_attempt_interval_s,
    sync_interval_from_env, task_score, DerivedTask, SyncAction, DEFAULT_SYNC_INTERVAL_S,
    IDLIST_SYNC_KEYS_ENV,
};

/// Parsed `/v1/get_id_lists` snapshot keyed by list name.
pub(crate) type ParsedManifest = IdListManifest;

/// Observer that keeps an in-memory snapshot of `/v1/get_id_lists` manifests for
/// configured server keys. Read by the producer only.
pub struct IdListManifestObserver {
    server_keys: HashSet<String>,
    manifests: Arc<ArcSwap<HashMap<String, ParsedManifest>>>,
}

impl IdListManifestObserver {
    /// Constructed from `server.rs` when `SFP_IDLIST_SYNC_KEYS_JSON` is set.
    #[allow(dead_code)] // used by the server binary (separate crate root from lib)
    pub(crate) fn new(
        server_keys: impl IntoIterator<Item = String>,
        manifests: Arc<ArcSwap<HashMap<String, ParsedManifest>>>,
    ) -> Self {
        Self {
            server_keys: server_keys.into_iter().collect(),
            manifests,
        }
    }
}

#[async_trait]
impl HttpDataProviderObserverTrait for IdListManifestObserver {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        false
    }

    async fn update(
        &self,
        request_context: &Arc<FullRequestContext>,
        response_context: &Arc<ResponseContext>,
    ) {
        let rc = &request_context.authorized_request_context;
        if rc.path != NormalizedPath::V1GetIdLists
            || !self.server_keys.contains(rc.sdk_key.as_str())
        {
            return;
        }

        if response_context.result_type == DataProviderRequestResult::Unauthorized {
            self.manifests.rcu(|current| {
                let mut next = (**current).clone();
                next.remove(&rc.sdk_key);
                next
            });
            return;
        }
        if response_context.result_type != DataProviderRequestResult::DataAvailable {
            return;
        }
        let parsed = match parse_id_list_manifest(&response_context.body) {
            Ok(m) => m,
            Err(e) => {
                eprintln!(
                    "[SFP] IdListManifestObserver: failed to parse get_id_lists for {}: {e}",
                    truncate_key(&rc.sdk_key)
                );
                return;
            }
        };

        self.manifests.rcu(|current| {
            let mut next = (**current).clone();
            next.insert(rc.sdk_key.clone(), parsed.clone());
            next
        });
    }

    async fn get(
        &self,
        _request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<Arc<ConfigSpecForCompany>> {
        None
    }
}

/// Per-tick producer: elects via Redis lock, derives tasks from manifests + state, enqueues.
pub struct IdListSyncProducer {
    enable_reverse_index: bool,
    schema: IdListKeySchema,
    lock: Arc<RedisWorkLock>,
    pool: RedisPool,
    namespaces: Arc<IdListNamespaces>,
    interval_s: u64,
}

impl IdListSyncProducer {
    pub fn with_namespaces(
        schema: IdListKeySchema,
        lock: Arc<RedisWorkLock>,
        pool: RedisPool,
        namespaces: Arc<IdListNamespaces>,
        interval_s: u64,
    ) -> Self {
        Self {
            enable_reverse_index: true,
            schema,
            lock,
            pool,
            namespaces,
            interval_s: interval_s.max(1),
        }
    }

    /// Existing worker/planner fixtures use a separate synthetic company per key.
    #[cfg(test)]
    pub(crate) fn new(
        schema: IdListKeySchema,
        lock: Arc<RedisWorkLock>,
        pool: RedisPool,
        manifests: Arc<ArcSwap<HashMap<String, ParsedManifest>>>,
        server_keys: Arc<Vec<String>>,
        interval_s: u64,
    ) -> Self {
        let mut namespaces = IdListNamespaces::new((*server_keys).clone());
        namespaces.manifests = manifests;
        for key in server_keys.iter() {
            namespaces.company_ids.set_for_test(key, key);
        }
        Self::with_namespaces(schema, lock, pool, Arc::new(namespaces), interval_s)
    }

    pub fn interval_s(&self) -> u64 {
        self.interval_s
    }

    pub fn with_reverse_index_enabled(mut self, enabled: bool) -> Self {
        self.enable_reverse_index = enabled;
        self
    }

    #[cfg(test)]
    fn manifests_for_test(&self) -> Arc<ArcSwap<HashMap<String, ParsedManifest>>> {
        Arc::clone(&self.namespaces.manifests)
    }

    /// One producer attempt. Returns whether this pod won the lock and ran successfully.
    pub async fn run_once(&self) -> Result<ProducerTickOutcome, RedisError> {
        let started = Instant::now();
        let lock_key = self.schema.producer_lock_key();
        let acquired = self.lock.try_acquire(&lock_key, self.interval_s).await?;
        if !acquired {
            emit_producer_run(ProxyEventType::IdListProducerRunSkipped, started, None);
            return Ok(ProducerTickOutcome::Skipped);
        }

        let produce_started = Instant::now();
        match self.produce_locked().await {
            Ok(stats) => {
                emit_producer_duration(produce_started);
                emit_producer_run(
                    ProxyEventType::IdListProducerRunSuccess,
                    started,
                    Some(stats.tasks_enqueued),
                );
                if stats.tasks_enqueued > 0 {
                    ProxyEventObserver::publish_event(
                        ProxyEvent::new(ProxyEventType::IdListTasksEnqueued).with_stat(EventStat {
                            operation_type: OperationType::IncrByValue,
                            value: stats.tasks_enqueued as i64,
                        }),
                    );
                }
                if stats.removals_scheduled > 0 {
                    ProxyEventObserver::publish_event(
                        ProxyEvent::new(ProxyEventType::IdListRemoved).with_stat(EventStat {
                            operation_type: OperationType::IncrByValue,
                            value: stats.removals_scheduled as i64,
                        }),
                    );
                }
                ProxyEventObserver::publish_event(
                    ProxyEvent::new(ProxyEventType::IdListQueueDepth).with_stat(EventStat {
                        operation_type: OperationType::Gauge,
                        value: stats.queue_depth as i64,
                    }),
                );
                Ok(ProducerTickOutcome::Success(stats))
            }
            Err(e) => {
                emit_producer_duration(produce_started);
                emit_producer_run(ProxyEventType::IdListProducerRunFailed, started, None);
                Err(e)
            }
        }
    }

    async fn produce_locked(&self) -> Result<ProducerTickStats, RedisError> {
        let (companies, manifest_unavailable) =
            self.namespaces.company_manifests().map_err(|collisions| {
                for collision in collisions {
                    eprintln!(
                        "[SFP] IdListSyncProducer error: namespace collision: namespace={:?}, company_ids={:?}; task generation aborted",
                        collision.namespace, collision.company_ids
                    );
                }
                RedisError::from((
                    bb8_redis::redis::ErrorKind::TypeError,
                    "ID-list namespace collision; task generation aborted",
                ))
            })?;
        let mut derived: Vec<DerivedTask> = Vec::new();
        let mut removals_scheduled = 0u64;
        if manifest_unavailable > 0 {
            ProxyEventObserver::publish_event(
                ProxyEvent::new(ProxyEventType::IdListManifestUnavailable).with_stat(EventStat {
                    operation_type: OperationType::IncrByValue,
                    value: manifest_unavailable as i64,
                }),
            );
        }
        for (skh, company) in companies {
            let manifest = &company.lists;
            let mut conn = self.pool.get().await.map_err(pool_get_err)?;

            let tasks_before = derived.len();
            let mut reverse_rebuild_pending = false;
            for (list_name, entry) in manifest.iter() {
                let meta = IdListMetadata::from(entry);
                let state_key = self.schema.state_key(&skh, list_name);
                let fields: HashMap<String, String> = conn.hgetall(&state_key).await?;
                let state = if fields.is_empty() {
                    None
                } else {
                    Some(ListSyncState::from_hash_fields(&fields))
                };

                let fwd_exists = if let Some(ref s) = state {
                    if s.version.is_empty() {
                        false
                    } else {
                        let fwd = self.schema.fwd_key(&skh, list_name, &s.version);
                        conn.exists::<_, bool>(&fwd).await?
                    }
                } else {
                    false
                };

                reverse_rebuild_pending |= state.as_ref().is_some_and(|s| s.reverse_index_disabled);
                if let Some((action, target)) = plan_sync_with_reverse_index(
                    state.as_ref(),
                    &meta,
                    fwd_exists,
                    self.enable_reverse_index,
                ) {
                    derived.push(DerivedTask {
                        skh: skh.clone(),
                        list: list_name.clone(),
                        action,
                        meta: Some(target),
                    });
                }
            }

            // Removals: lists in __all that left the manifest.
            let all_key = self.schema.all_lists_key(&skh);
            let known: HashSet<String> = conn.smembers(&all_key).await?;
            for list_name in known {
                if !company.incomplete && !manifest.contains_key(&list_name) {
                    removals_scheduled += 1;
                    derived.push(DerivedTask {
                        skh: skh.clone(),
                        list: list_name,
                        action: SyncAction::Remove,
                        meta: None,
                    });
                }
            }
            // An empty manifest is a valid synchronized result. Preserve readiness
            // during ordinary updates; recovery explicitly clears it while rebuilding.
            if !company.incomplete && derived.len() == tasks_before {
                if self.enable_reverse_index && reverse_rebuild_pending {
                    let _: () = conn.del(self.schema.ready_key(&skh)).await?;
                    continue;
                }
                if self.enable_reverse_index {
                    let _: () = conn
                        .hdel(self.schema.dict_key(&skh), super::REVERSE_INDEX_DISABLED)
                        .await?;
                }
                let _: () = conn
                    .set(self.schema.ready_key(&skh), manifest.len())
                    .await?;
            }
        }

        let ordered = order_and_dedupe_tasks(derived);
        let tasks_enqueued = self.enqueue_tasks(&ordered).await?;

        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let queue_depth: u64 = conn.zcard(self.schema.queue_key()).await?;

        Ok(ProducerTickStats {
            tasks_enqueued,
            removals_scheduled,
            queue_depth,
            manifest_unavailable,
            tasks_derived: ordered.len() as u64,
        })
    }

    /// Writes `task_meta` (always) + `ZADD NX` for each task. Returns how many ZADD NX added.
    ///
    /// `task_meta` does not record Full vs Delta (`op` is `load` or `remove`). Full vs
    /// Delta is only the queue score class; the worker re-derives from state + this meta.
    async fn enqueue_tasks(&self, tasks: &[(f64, DerivedTask)]) -> Result<u64, RedisError> {
        if tasks.is_empty() {
            return Ok(0);
        }

        let meta_ttl = self.interval_s.saturating_mul(10).max(60);
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let mut added = 0u64;

        // Pipeline in chunks to bound memory / reply size.
        const CHUNK: usize = 64;
        for chunk in tasks.chunks(CHUNK) {
            let mut pipe = bb8_redis::redis::pipe();
            for (score, task) in chunk {
                let meta_key = self.schema.task_meta_key(&task.skh, &task.list);
                let member = IdListKeySchema::encode_task(&task.skh, &task.list);

                match (&task.action, &task.meta) {
                    (SyncAction::Remove, _) => {
                        pipe.hset(&meta_key, task_meta_fields::OP, TASK_OP_REMOVE)
                            .ignore();
                    }
                    (_, Some(meta)) => {
                        pipe.hset(&meta_key, task_meta_fields::OP, TASK_OP_LOAD)
                            .ignore()
                            .hset(&meta_key, task_meta_fields::URL, &meta.url)
                            .ignore()
                            .hset(&meta_key, task_meta_fields::FILE_ID, &meta.file_id)
                            .ignore()
                            .hset(
                                &meta_key,
                                task_meta_fields::CREATION_TIME,
                                meta.creation_time.to_string(),
                            )
                            .ignore()
                            .hset(&meta_key, task_meta_fields::SIZE, meta.size.to_string())
                            .ignore();
                    }
                    (_, None) => {
                        pipe.hset(&meta_key, task_meta_fields::OP, TASK_OP_LOAD)
                            .ignore();
                    }
                }
                pipe.expire(&meta_key, meta_ttl as i64).ignore();
                // NX: only add if not already queued; meta refresh happens regardless.
                // Non-ignored so the reply is exactly the ZADD NX add-count (0 or 1).
                pipe.cmd("ZADD")
                    .arg(self.schema.queue_key())
                    .arg("NX")
                    .arg(*score)
                    .arg(&member);
            }

            let replies: Vec<i64> = pipe.query_async(&mut *conn).await?;
            added += replies.iter().filter(|&&n| n > 0).count() as u64;
        }

        Ok(added)
    }

    /// Spawns the attempt-often / hold-long producer loop until `cancel` is cancelled.
    pub fn spawn_loop(self: Arc<Self>, cancel: CancellationToken) {
        let attempt_s = producer_attempt_interval_s(self.interval_s);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(attempt_s));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        if let Err(e) = self.run_once().await {
                            eprintln!("[SFP] IdListSyncProducer tick failed: {e}");
                        }
                    }
                }
            }
        });
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProducerTickStats {
    /// How many `ZADD NX` actually inserted a new queue member.
    pub tasks_enqueued: u64,
    pub removals_scheduled: u64,
    pub queue_depth: u64,
    pub manifest_unavailable: u64,
    pub tasks_derived: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProducerTickOutcome {
    Skipped,
    Success(ProducerTickStats),
}

/// Registers each server key into the existing get_id_lists polling loop (same pattern as
/// `shadow_fetch_json_config_spec`): insert a programmatic context and prime once via
/// `get_id_lists` so the key lands in `SdkKeyStore` and background polling continues.
pub async fn register_server_keys_for_manifest_polling(
    server_keys: &[String],
    rc_cache: &AuthorizedRequestContextCache,
    get_id_list_store: &GetIdListStore,
) {
    for server_key in server_keys {
        let rc = rc_cache.get_or_insert(
            server_key.clone(),
            NormalizedPath::V1GetIdLists,
            vec![CompressionEncoder::Gzip],
            false,
            false,
            None,
        );
        let _ = get_id_list_store.get_id_lists(&rc).await;
    }
}

fn emit_producer_run(event_type: ProxyEventType, started: Instant, _enqueued: Option<u64>) {
    let millis = elapsed_ms(started);
    ProxyEventObserver::publish_event(ProxyEvent::new(event_type).with_stat(EventStat {
        operation_type: OperationType::Distribution,
        value: millis,
    }));
}

/// Wall time of `produce_locked` only (lock acquire excluded). Compare against lock TTL.
fn emit_producer_duration(started: Instant) {
    ProxyEventObserver::publish_event(
        ProxyEvent::new(ProxyEventType::IdListProducerDurationMs).with_stat(EventStat {
            operation_type: OperationType::Distribution,
            value: elapsed_ms(started),
        }),
    );
}

fn elapsed_ms(started: Instant) -> i64 {
    started.elapsed().as_millis().min(i64::MAX as u128) as i64
}

fn truncate_key(key: &str) -> String {
    if key.len() > 8 {
        format!("{}…", &key[..8])
    } else {
        key.to_string()
    }
}

#[cfg(test)]
mod failure_tests;
#[cfg(test)]
mod race_tests;
