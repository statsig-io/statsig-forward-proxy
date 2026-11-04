use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bb8_redis::redis::AsyncCommands;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use crate::datastore::caching::redis_pool::pool_get_err;
use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::{EventStat, OperationType, ProxyEvent, ProxyEventType};

use super::super::redis_syncer::plan_sync_with_reverse_index;
use super::super::schema::{state_fields, IdListKeySchema, ListSyncState};
use super::super::RedisWorkLock;
use super::super::SyncAction;
use super::{IdListSyncWorker, TaskMetadata, TaskOp, WorkerAction, WorkerError, WorkerRunOutcome};

impl IdListSyncWorker {
    pub fn spawn_loop(
        self: Arc<Self>,
        runtime: &Handle,
        cancellation: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        runtime.spawn(async move {
            loop {
                if cancellation.is_cancelled() {
                    break;
                }
                match self.run_once().await {
                    Ok(WorkerRunOutcome::Idle) => {
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms)) => {}
                            _ = cancellation.cancelled() => break,
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!("[SFP] Idlist sync worker task failed: {error}");
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms)) => {}
                            _ = cancellation.cancelled() => break,
                        }
                    }
                }
            }
        })
    }

    pub async fn run_once(&self) -> Result<WorkerRunOutcome, WorkerError> {
        // Popping transfers responsibility to this worker; failures are recovered
        // from Redis state when B2 observes the manifest again and re-enqueues work.
        let popped: Vec<(String, f64)> = {
            let mut conn = self.pool.get().await.map_err(pool_get_err)?;
            bb8_redis::redis::cmd("ZPOPMIN")
                .arg(self.schema.queue_key())
                .arg(1)
                .query_async(&mut *conn)
                .await?
        };
        let Some((task, _score)) = popped.into_iter().next() else {
            return Ok(WorkerRunOutcome::Idle);
        };
        let Some((skh, list)) = IdListKeySchema::decode_task(&task) else {
            metric(ProxyEventType::IdListStaleTaskDropped, 1);
            return Ok(WorkerRunOutcome::Dropped);
        };

        let claim_key = self.schema.claim_key(&skh, &list);
        if !self
            .claim
            .try_acquire(&claim_key, self.config.lock_ttl_s)
            .await?
        {
            metric(ProxyEventType::IdListTaskClaimConflict, 1);
            return Ok(WorkerRunOutcome::ClaimConflict);
        }
        gauge(ProxyEventType::IdListClaimHeld, 1);

        let renewal = ClaimRenewal::start(
            Arc::clone(&self.claim),
            claim_key.clone(),
            self.config.lock_ttl_s,
        );
        let started = Instant::now();
        let result = self.run_claimed(&skh, &list, &renewal).await;
        // Stop renewal before release so the background task cannot extend a lock
        // after ownership has intentionally ended. Drop also cancels (panic /
        // cancelled `run_once`); `released` skips a second best-effort spawn.
        if let Err(error) = renewal.stop_and_release().await {
            eprintln!("[SFP] Failed to release idlist claim: {error}");
        }
        gauge(ProxyEventType::IdListClaimHeld, 0);

        match &result {
            Ok(WorkerRunOutcome::Completed(_)) => {
                metric(ProxyEventType::IdListTaskCompleted, 1);
                timing(
                    ProxyEventType::IdListTaskDurationMs,
                    started.elapsed().as_millis().min(i64::MAX as u128) as i64,
                );
            }
            Err(_) => metric(ProxyEventType::IdListTaskFailed, 1),
            _ => {}
        }
        result
    }

    async fn run_claimed(
        &self,
        skh: &str,
        list: &str,
        renewal: &ClaimRenewal,
    ) -> Result<WorkerRunOutcome, WorkerError> {
        let meta_key = self.schema.task_meta_key(skh, list);
        let state_key = self.schema.state_key(skh, list);
        let (meta_fields, state_fields): (HashMap<String, String>, HashMap<String, String>) = {
            let mut conn = self.pool.get().await.map_err(pool_get_err)?;
            let mut pipe = bb8_redis::redis::pipe();
            pipe.hgetall(&meta_key).hgetall(&state_key);
            pipe.query_async(&mut *conn).await?
        };
        let state_exists = !state_fields.is_empty();
        let state = ListSyncState::from_hash_fields(&state_fields);
        let recovering_removal_cleanup = meta_fields.is_empty()
            && state_fields.contains_key(state_fields::REMOVAL_FANOUT_COMPLETE);
        let meta = if recovering_removal_cleanup {
            // The final cleanup pipeline can disconnect after DEL task_meta but before DEL state.
            // The persisted fan-out marker makes the remaining cleanup intent unambiguous.
            TaskMetadata {
                op: TaskOp::Remove,
                file_id: String::new(),
                creation_time: 0,
                url: String::new(),
                size: 0,
            }
        } else if meta_fields.is_empty() {
            metric(ProxyEventType::IdListStaleTaskDropped, 1);
            return Ok(WorkerRunOutcome::Dropped);
        } else {
            TaskMetadata::from_hash(&meta_fields)?
        };

        let (action, meta) = match meta.op {
            TaskOp::Remove => (WorkerAction::Remove, meta),
            TaskOp::Load => match self
                .plan_work(skh, list, state_exists, &state, &meta)
                .await?
            {
                Some(action) => action,
                None => return Ok(WorkerRunOutcome::Dropped),
            },
        };
        if meta.op == TaskOp::Load {
            gauge(
                ProxyEventType::IdListSyncCheckpointLag,
                meta.size.saturating_sub(state.size).min(i64::MAX as u64) as i64,
            );
            if meta.creation_time > 0 {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(u64::MAX as u128) as u64;
                gauge(
                    ProxyEventType::IdListStalenessSeconds,
                    (now_ms.saturating_sub(meta.creation_time) / 1_000).min(i64::MAX as u64) as i64,
                );
            }
        }

        if !self.config.enable_reverse_index {
            // Record skipped reverse updates before changing any forward data. The per-list
            // marker schedules a full reconciliation if indexing is re-enabled.
            let mut conn = self.pool.get().await.map_err(pool_get_err)?;
            let _: () = conn
                .hset(
                    self.schema.dict_key(skh),
                    super::super::REVERSE_INDEX_DISABLED,
                    1u8,
                )
                .await?;
            let _: () = conn
                .hset(&state_key, super::super::REVERSE_INDEX_DISABLED, 1u8)
                .await?;
        }
        match action {
            WorkerAction::Delta => self.apply_delta(skh, list, &state, &meta, renewal).await?,
            WorkerAction::FullReload => {
                self.apply_full_reload(skh, list, &state, &meta, renewal)
                    .await?
            }
            WorkerAction::Remove => self.apply_remove(skh, list, &state, renewal).await?,
        }
        Ok(WorkerRunOutcome::Completed(action))
    }

    async fn plan_work(
        &self,
        skh: &str,
        list: &str,
        state_exists: bool,
        state: &ListSyncState,
        meta: &TaskMetadata,
    ) -> Result<Option<(WorkerAction, TaskMetadata)>, WorkerError> {
        let forward_exists = if state_exists && !state.version.is_empty() {
            let mut conn = self.pool.get().await.map_err(pool_get_err)?;
            conn.exists::<_, bool>(self.schema.fwd_key(skh, list, &state.version))
                .await?
        } else {
            false
        };
        if state_exists && state.cardinality.unwrap_or(state.size) > 0 && !forward_exists {
            metric(ProxyEventType::IdListRedisStateReset, 1);
        }

        let metadata = meta.as_idlist_metadata();
        let plan = plan_sync_with_reverse_index(
            state_exists.then_some(state),
            &metadata,
            forward_exists,
            self.config.enable_reverse_index,
        );
        if plan.is_none() {
            self.ensure_current_version_persisted(skh, list, state)
                .await?;
        }
        plan.map(|(action, target)| {
            let action = match action {
                SyncAction::FullLoad => WorkerAction::FullReload,
                SyncAction::Delta { .. } => WorkerAction::Delta,
                SyncAction::Remove => {
                    return Err(WorkerError::InvalidTask(
                        "load task resolved to remove".to_string(),
                    ));
                }
            };
            Ok((
                action,
                TaskMetadata {
                    op: TaskOp::Load,
                    file_id: target.file_id,
                    creation_time: target.creation_time,
                    url: target.url,
                    size: target.size,
                },
            ))
        })
        .transpose()
    }

    /// Repair backstop for the publish-before-PERSIST crash window. The B2 producer can call
    /// this on unchanged manifest entries; the worker also calls it for satisfied queued tasks.
    pub async fn ensure_current_version_persisted(
        &self,
        skh: &str,
        list: &str,
        state: &ListSyncState,
    ) -> Result<bool, WorkerError> {
        if state.version.is_empty() {
            return Ok(false);
        }
        let key = self.schema.fwd_key(skh, list, &state.version);
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let ttl: i64 = conn.ttl(&key).await?;
        if ttl < 0 {
            return Ok(false);
        }
        let persisted: bool = conn.persist(&key).await?;
        if persisted {
            metric(ProxyEventType::IdListVersionRepersisted, 1);
        }
        Ok(persisted)
    }
}

pub(super) struct ClaimRenewal {
    lock: Arc<RedisWorkLock>,
    key: String,
    ttl_s: u64,
    stop: CancellationToken,
    lost: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
}

impl ClaimRenewal {
    fn start(lock: Arc<RedisWorkLock>, key: String, ttl_s: u64) -> Self {
        let stop = CancellationToken::new();
        let lost = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));
        let task_stop = stop.clone();
        let task_lost = Arc::clone(&lost);
        let task_lock = Arc::clone(&lock);
        let task_key = key.clone();
        tokio::spawn(async move {
            let interval = Duration::from_secs((ttl_s / 3).max(1));
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        match task_lock.renew(&task_key, ttl_s).await {
                            Ok(true) => {}
                            Ok(false) | Err(_) => {
                                task_lost.store(true, Ordering::Release);
                                break;
                            }
                        }
                    }
                    _ = task_stop.cancelled() => break,
                }
            }
        });
        Self {
            lock,
            key,
            ttl_s,
            stop,
            lost,
            released,
        }
    }

    fn stop(&self) {
        self.stop.cancel();
    }

    async fn stop_and_release(&self) -> Result<(), bb8_redis::redis::RedisError> {
        self.stop.cancel();
        let result = self.lock.release(&self.key).await;
        self.released.store(true, Ordering::Release);
        result
    }

    pub(super) async fn ensure_held(&self) -> Result<(), WorkerError> {
        if self.lost.load(Ordering::Acquire) {
            Err(WorkerError::LockLost)
        } else {
            Ok(())
        }
    }

    pub(super) async fn renew_now(&self) -> Result<(), WorkerError> {
        self.ensure_held().await?;
        if self.lock.renew(&self.key, self.ttl_s).await? {
            Ok(())
        } else {
            self.lost.store(true, Ordering::Release);
            Err(WorkerError::LockLost)
        }
    }
}

impl Drop for ClaimRenewal {
    fn drop(&mut self) {
        // `run_once` may panic or be cancelled at an await inside apply_*; those
        // paths never reach `stop_and_release`. Cancel is sync; Redis DEL is not,
        // so we spawn a best-effort release if the happy path did not already run.
        self.stop();
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(handle) = Handle::try_current() {
            let lock = Arc::clone(&self.lock);
            let key = self.key.clone();
            handle.spawn(async move {
                let _ = lock.release(&key).await;
            });
        }
    }
}

pub(super) fn metric(event_type: ProxyEventType, value: i64) {
    publish_metric(event_type, value, OperationType::IncrByValue);
}

fn gauge(event_type: ProxyEventType, value: i64) {
    publish_metric(event_type, value, OperationType::Gauge);
}

fn timing(event_type: ProxyEventType, value: i64) {
    publish_metric(event_type, value, OperationType::Timing);
}

fn publish_metric(event_type: ProxyEventType, value: i64, operation_type: OperationType) {
    ProxyEventObserver::publish_event(ProxyEvent::new(event_type).with_stat(EventStat {
        operation_type,
        value,
    }));
}
