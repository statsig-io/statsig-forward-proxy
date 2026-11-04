use bb8_redis::redis::AsyncCommands;

use crate::datastore::caching::redis_pool::pool_get_err;
use crate::observers::ProxyEventType;

use super::super::schema::{state_fields, ListSyncState};
use super::delta::FileApplyMode;
use super::run::{metric, ClaimRenewal};
use super::{IdListSyncWorker, TaskMetadata, WorkerError};

impl IdListSyncWorker {
    pub(super) async fn apply_full_reload(
        &self,
        skh: &str,
        list: &str,
        state: &ListSyncState,
        meta: &TaskMetadata,
        renewal: &ClaimRenewal,
    ) -> Result<(), WorkerError> {
        if self
            .reconcile_committed_forward(skh, list, state, meta, renewal)
            .await?
        {
            metric(ProxyEventType::IdListFullReloadCommitted, 1);
            return Ok(());
        }
        let state_key = self.schema.state_key(skh, list);
        let new_version = meta.version_tag();
        let new_key = self.schema.fwd_key(skh, list, &new_version);
        let old_version = state.version.clone();
        let matching_load = state.loading_version.as_deref() == Some(new_version.as_str());

        // Register before allocating a token or mutating the serving index, including
        // initial loads that have not yet published a committed version.
        let old_missing = !old_version.is_empty()
            && state.cardinality.unwrap_or(state.size) > 0
            && !self
                .key_exists(&self.schema.fwd_key(skh, list, &old_version))
                .await?;
        let pending_missing = matching_load
            && state
                .loading_cardinality
                .unwrap_or(state.loading_size.unwrap_or(0))
                > 0
            && !self.key_exists(&new_key).await?;
        let mut reconcile = state.reconcile_required
            || (self.config.enable_reverse_index && state.reverse_index_disabled)
            || old_missing
            || pending_missing
            || (state.loading_version.is_some() && !matching_load);
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let _: () = conn.sadd(self.schema.all_lists_key(skh), list).await?;
        if reconcile {
            let _: () = conn.del(self.schema.ready_key(skh)).await?;
            // Persist before replacing any checkpoint. Retries must not revert to a
            // diff against the committed set after partial serving mutations.
            let _: () = conn
                .hset(&state_key, state_fields::RECONCILE_REQUIRED, 1u8)
                .await?;
        }
        drop(conn);
        let resume = matching_load && !pending_missing;
        let mut loading_size = if resume {
            metric(ProxyEventType::IdListFullReloadResumed, 1);
            state.loading_size.unwrap_or(0)
        } else {
            let mut conn = self.pool.get().await.map_err(pool_get_err)?;
            let _: () = conn.del(&new_key).await?;
            drop(conn);
            self.initialize_load(&state_key, &new_version).await?;
            metric(ProxyEventType::IdListFullReloadStarted, 1);
            0
        };
        let mut diff_partition = if resume { state.diff_partition } else { None };

        let token = self.dict.get_or_allocate_token(skh, list).await?;
        if diff_partition.is_none() {
            if loading_size < meta.size {
                let applied = self
                    .apply_file_stream(
                        skh,
                        &meta.url,
                        loading_size,
                        meta.size,
                        FileApplyMode::Load {
                            forward_key: &new_key,
                            state_key: &state_key,
                        },
                        renewal,
                    )
                    .await?;
                loading_size = applied.checkpoint;
                if applied.checkpoint < meta.size {
                    return Err(WorkerError::InvalidFile(format!(
                        "full download ended at complete-line offset {}, before task size {} (partial trailing line: {})",
                        applied.checkpoint, meta.size, applied.partial_trailing_line
                    )));
                }
            }
            let mut conn = self.pool.get().await.map_err(pool_get_err)?;
            let _: () = conn
                .hset(&state_key, state_fields::DIFF_PARTITION, 0u32)
                .await?;
            diff_partition = Some(0);
        }

        // A same-version reload means the old forward set was missing/corrupt. The key now
        // contains the in-flight replacement, not a trustworthy old diff input.
        let old_key = (!old_version.is_empty() && old_version != new_version)
            .then(|| self.schema.fwd_key(skh, list, &old_version));
        let old_card = match &old_key {
            Some(key) => self.scard(key).await?,
            None => 0,
        };
        let new_card = self.scard(&new_key).await?;
        {
            let mut conn = self.pool.get().await.map_err(pool_get_err)?;
            let expected: Option<u64> = conn
                .hget(&state_key, state_fields::LOADING_CARDINALITY)
                .await?;
            if expected.is_some_and(|count| count != new_card) {
                return Err(WorkerError::ScanVerification(
                    "pending forward set changed after download".to_string(),
                ));
            }
            if old_key.is_some() && state.cardinality.unwrap_or(state.size) > 0 && old_card == 0 {
                reconcile = true;
                let _: () = conn
                    .hset(&state_key, state_fields::RECONCILE_REQUIRED, 1u8)
                    .await?;
                let _: () = conn.del(self.schema.ready_key(skh)).await?;
            }
        }
        if self.config.enable_reverse_index && reconcile {
            self.clear_reverse_token(skh, token, renewal).await?;
            self.fanout_scan(skh, &new_key, token, true, new_card, renewal)
                .await?;
        } else if self.config.enable_reverse_index {
            self.apply_streamed_diff(
                skh,
                old_key.as_deref(),
                &new_key,
                token,
                &state_key,
                diff_partition.unwrap_or(0) as u64,
                old_card,
                new_card,
                renewal,
            )
            .await?;
        }

        // A resumed diff describes the downloaded snapshot, not newer task metadata.
        // Publish only its checkpoint; the next tick downloads any appended bytes.
        let mut committed_meta = meta.clone();
        committed_meta.size = loading_size;
        renewal.renew_now().await?;
        self.commit_reload(
            skh,
            list,
            &state_key,
            &new_key,
            old_key.as_deref(),
            &committed_meta,
        )
        .await?;
        metric(ProxyEventType::IdListFullReloadCommitted, 1);
        Ok(())
    }

    /// Re-enabling indexing can repair the committed snapshot without a download.
    /// Keep the repair marker until every reverse write and scan has succeeded so
    /// a failed or interrupted attempt can retry against the same forward set.
    async fn reconcile_committed_forward(
        &self,
        skh: &str,
        list: &str,
        state: &ListSyncState,
        meta: &TaskMetadata,
        renewal: &ClaimRenewal,
    ) -> Result<bool, WorkerError> {
        if !self.config.enable_reverse_index
            || !state.reverse_index_disabled
            || state.version.is_empty()
            || state.file_id != meta.file_id
            || state.size != meta.size
            || state.loading_version.is_some()
        {
            return Ok(false);
        }
        // Same-file appends retain the committed generation even when the manifest
        // timestamp changes. File identity and checkpoint determine reuse.
        let forward_key = self.schema.fwd_key(skh, list, &state.version);
        let cardinality = self.scard(&forward_key).await?;
        // Unknown or mismatched cardinality requires the existing download recovery
        // path. An absent set is valid only for a checkpointed empty list.
        if state.cardinality != Some(cardinality) {
            return Ok(false);
        }
        self.ensure_current_version_persisted(skh, list, state)
            .await?;
        let state_key = self.schema.state_key(skh, list);
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let _: () = bb8_redis::redis::pipe()
            .hset(&state_key, state_fields::RECONCILE_REQUIRED, 1u8)
            .ignore()
            .del(self.schema.ready_key(skh))
            .ignore()
            .query_async(&mut *conn)
            .await?;
        drop(conn);

        let token = self.dict.get_or_allocate_token(skh, list).await?;
        self.clear_reverse_token(skh, token, renewal).await?;
        self.fanout_scan(skh, &forward_key, token, true, cardinality, renewal)
            .await?;
        renewal.renew_now().await?;
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let _: usize = conn
            .hdel(
                &state_key,
                &[
                    super::super::REVERSE_INDEX_DISABLED,
                    state_fields::RECONCILE_REQUIRED,
                ],
            )
            .await?;
        Ok(true)
    }

    async fn initialize_load(&self, state_key: &str, version: &str) -> Result<u64, WorkerError> {
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let mut pipe = bb8_redis::redis::pipe();
        pipe.cmd("HSET")
            .arg(state_key)
            .arg(state_fields::LOADING_VERSION)
            .arg(version)
            .arg(state_fields::LOADING_SIZE)
            .arg(0)
            .ignore()
            .cmd("HDEL")
            .arg(state_key)
            .arg(state_fields::DIFF_PARTITION)
            .arg(state_fields::LOADING_CARDINALITY)
            .arg(state_fields::REMOVAL_FANOUT_COMPLETE)
            .ignore();
        pipe.query_async::<()>(&mut *conn).await?;
        Ok(0)
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_reload(
        &self,
        skh: &str,
        list: &str,
        state_key: &str,
        new_key: &str,
        old_key: Option<&str>,
        meta: &TaskMetadata,
    ) -> Result<(), WorkerError> {
        let version = meta.version_tag();
        let cardinality = self.scard(new_key).await?;
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let mut pipe = bb8_redis::redis::pipe();
        pipe.cmd("HSET")
            .arg(state_key)
            .arg(state_fields::VERSION)
            .arg(&version)
            .arg(state_fields::FILE_ID)
            .arg(&meta.file_id)
            .arg(state_fields::URL)
            .arg(&meta.url)
            .arg(state_fields::CREATION_TIME)
            .arg(meta.creation_time)
            .arg(state_fields::SIZE)
            .arg(meta.size)
            .arg(state_fields::CARDINALITY)
            .arg(cardinality)
            .ignore()
            .cmd("HDEL")
            .arg(state_key)
            .arg(state_fields::LOADING_VERSION)
            .arg(state_fields::LOADING_SIZE)
            .arg(state_fields::RECONCILE_REQUIRED)
            .arg(state_fields::DIFF_PARTITION)
            .arg(state_fields::LOADING_CARDINALITY)
            .arg(state_fields::REMOVAL_FANOUT_COMPLETE)
            .ignore()
            .cmd("SADD")
            .arg(self.schema.all_lists_key(skh))
            .arg(list)
            .ignore();
        pipe.query_async::<()>(&mut *conn).await?;

        if self.config.enable_reverse_index {
            let _: () = conn
                .hdel(state_key, super::super::REVERSE_INDEX_DISABLED)
                .await?;
        }
        // Publish state before removing the pending TTL. If the process dies here,
        // ensure_current_version_persisted repairs the published version on takeover.
        let _: bool = conn.persist(new_key).await?;
        if let Some(old_key) = old_key.filter(|old| *old != new_key) {
            let _: bool = conn
                .expire(old_key, self.config.old_version_ttl_s as i64)
                .await?;
        }
        Ok(())
    }
}
