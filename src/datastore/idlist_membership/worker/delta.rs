use bb8_redis::redis::AsyncCommands;
use futures::StreamExt;

use crate::datastore::caching::redis_pool::pool_get_err;
use crate::observers::ProxyEventType;

use super::super::schema::{state_fields, ListSyncState};
use super::core::PENDING_VERSION_TTL_S;
use super::download::download_file;
use super::parser::{CompleteLineDecoder, FileChange};
use super::run::{metric, ClaimRenewal};
use super::{IdListSyncWorker, TaskMetadata, WorkerError};

pub(super) struct StreamApplyResult {
    pub(super) lines: usize,
    pub(super) checkpoint: u64,
    pub(super) partial_trailing_line: bool,
}

pub(super) enum FileApplyMode<'a> {
    Delta {
        token: u64,
        forward_key: &'a str,
        state_key: &'a str,
    },
    Load {
        forward_key: &'a str,
        state_key: &'a str,
    },
}

impl IdListSyncWorker {
    pub(super) async fn apply_delta(
        &self,
        skh: &str,
        list: &str,
        state: &ListSyncState,
        meta: &TaskMetadata,
        renewal: &ClaimRenewal,
    ) -> Result<(), WorkerError> {
        if meta.file_id != state.file_id {
            return Err(WorkerError::InvalidTask(
                "delta file_id does not match state".to_string(),
            ));
        }
        self.ensure_current_version_persisted(skh, list, state)
            .await?;
        let token = self.dict.get_or_allocate_token(skh, list).await?;
        let state_key = self.schema.state_key(skh, list);
        let forward_key = self.schema.fwd_key(skh, list, &state.version);
        let applied = self
            .apply_file_stream(
                skh,
                &meta.url,
                state.size,
                meta.size,
                FileApplyMode::Delta {
                    token,
                    forward_key: &forward_key,
                    state_key: &state_key,
                },
                renewal,
            )
            .await?;
        if applied.checkpoint < meta.size {
            return Err(WorkerError::InvalidFile(format!(
                "download ended at complete-line offset {}, before task size {} (partial trailing line: {})",
                applied.checkpoint, meta.size, applied.partial_trailing_line
            )));
        }
        metric(ProxyEventType::IdListDeltaApplied, applied.lines as i64);
        Ok(())
    }

    pub(super) async fn apply_file_stream(
        &self,
        skh: &str,
        url: &str,
        start: u64,
        file_size: u64,
        mode: FileApplyMode<'_>,
        renewal: &ClaimRenewal,
    ) -> Result<StreamApplyResult, WorkerError> {
        let mut stream = download_file(&self.http, url, start, file_size).await?;
        let mut decoder = CompleteLineDecoder::new(start);
        let mut batch = Vec::with_capacity(self.config.pipeline_batch_size);
        let mut batch_checkpoint = start;
        let mut applied_lines = 0usize;
        let mut first_load_batch = true;
        while let Some(chunk) = stream.next().await {
            renewal.ensure_held().await?;
            for decoded in decoder.push(&chunk?)? {
                batch_checkpoint = decoded.end_offset;
                if let Some(change) = decoded.change {
                    batch.push(change);
                    applied_lines += 1;
                }
                if batch.len() >= self.config.pipeline_batch_size {
                    let load_created_key = matches!(mode, FileApplyMode::Load { .. })
                        && batch.iter().any(|change| change.add);
                    self.apply_file_batch(skh, &mode, &batch, batch_checkpoint, first_load_batch)
                        .await?;
                    if load_created_key {
                        first_load_batch = false;
                    }
                    batch.clear();
                }
            }
        }
        if batch_checkpoint > start {
            self.apply_file_batch(skh, &mode, &batch, batch_checkpoint, first_load_batch)
                .await?;
        }
        Ok(StreamApplyResult {
            lines: applied_lines,
            checkpoint: batch_checkpoint,
            partial_trailing_line: decoder.has_partial_line(),
        })
    }

    async fn apply_file_batch(
        &self,
        skh: &str,
        mode: &FileApplyMode<'_>,
        changes: &[FileChange],
        checkpoint: u64,
        first_load_batch: bool,
    ) -> Result<(), WorkerError> {
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let mut pipe = bb8_redis::redis::pipe();
        match mode {
            FileApplyMode::Delta {
                token, forward_key, ..
            } => {
                for change in changes {
                    // Keep both mutations adjacent and preserve source order. A member
                    // may appear multiple times in one delta, where the last line wins.
                    if self.config.enable_reverse_index {
                        pipe.cmd(if change.add { "SADD" } else { "SREM" })
                            .arg(self.schema.member_key(skh, &change.member))
                            .arg(*token)
                            .ignore();
                    }
                    if change.add {
                        pipe.cmd("SADD")
                            .arg(*forward_key)
                            .arg(&change.member)
                            .ignore();
                    } else {
                        pipe.cmd("SREM")
                            .arg(*forward_key)
                            .arg(&change.member)
                            .ignore();
                    }
                }
            }
            FileApplyMode::Load { forward_key, .. } => {
                // The TTL must be installed atomically with the first SADD; otherwise
                // a crash between those commands can leave an orphan forward set forever.
                pipe.atomic();
                for change in changes {
                    pipe.cmd(if change.add { "SADD" } else { "SREM" })
                        .arg(*forward_key)
                        .arg(&change.member)
                        .ignore();
                }
                if first_load_batch && changes.iter().any(|change| change.add) {
                    pipe.cmd("EXPIRE")
                        .arg(*forward_key)
                        .arg(PENDING_VERSION_TTL_S)
                        .ignore();
                }
            }
        }
        pipe.query_async::<()>(&mut *conn).await?;

        // Deliberately separate from the mutation pipeline: apply first, checkpoint second.
        let (state_key, field) = match mode {
            FileApplyMode::Delta { state_key, .. } => (*state_key, state_fields::SIZE),
            FileApplyMode::Load { state_key, .. } => (*state_key, state_fields::LOADING_SIZE),
        };
        let (forward_key, cardinality_field) = match mode {
            FileApplyMode::Delta { forward_key, .. } => (*forward_key, state_fields::CARDINALITY),
            FileApplyMode::Load { forward_key, .. } => {
                (*forward_key, state_fields::LOADING_CARDINALITY)
            }
        };
        let cardinality: u64 = conn.scard(forward_key).await?;
        let _: () = conn
            .hset_multiple(
                state_key,
                &[(field, checkpoint), (cardinality_field, cardinality)],
            )
            .await?;
        Ok(())
    }
}
