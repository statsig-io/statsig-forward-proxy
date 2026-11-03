use bb8_redis::redis::AsyncCommands;

use crate::datastore::caching::redis_pool::pool_get_err;
use crate::observers::ProxyEventType;

use super::super::schema::{state_fields, DictClient, ListSyncState};
use super::run::{metric, ClaimRenewal};
use super::{IdListSyncWorker, WorkerError};

impl IdListSyncWorker {
    pub(super) async fn apply_remove(
        &self,
        skh: &str,
        list: &str,
        state: &ListSyncState,
        renewal: &ClaimRenewal,
    ) -> Result<(), WorkerError> {
        let state_key = self.schema.state_key(skh, list);
        let forward_key = self.schema.fwd_key(skh, list, &state.version);
        let fanout_complete = {
            let mut conn = self.pool.get().await.map_err(pool_get_err)?;
            conn.hget::<_, _, Option<u8>>(&state_key, state_fields::REMOVAL_FANOUT_COMPLETE)
                .await?
                .is_some()
        };
        let token = self.dict.get_token(skh, list).await?;

        if self.config.enable_reverse_index
            && !fanout_complete
            && token.is_none()
            && !state.version.is_empty()
        {
            return Err(WorkerError::MissingDictionaryToken);
        }
        if self.config.enable_reverse_index && !fanout_complete && token.is_some() {
            // Member keys are the serving index, so remove the token from every
            // member before deleting the forward set that enumerates those members.
            let token = token.ok_or(WorkerError::MissingDictionaryToken)?;
            let missing = state.cardinality.unwrap_or(state.size) > 0
                && !self.key_exists(&forward_key).await?;
            if missing
                || state.reverse_index_disabled
                || state.version.is_empty()
                || state.loading_version.is_some()
                || state.reconcile_required
            {
                self.clear_reverse_token(skh, token, renewal).await?;
            } else {
                let card = self.scard(&forward_key).await?;
                self.fanout_scan(skh, &forward_key, token, false, card, renewal)
                    .await?;
            }
        }
        if !fanout_complete {
            // Makes a partially-applied final deletion pipeline safely resumable.
            let mut conn = self.pool.get().await.map_err(pool_get_err)?;
            let _: () = conn
                .hset(&state_key, state_fields::REMOVAL_FANOUT_COMPLETE, 1u8)
                .await?;
        }

        let meta_key = self.schema.task_meta_key(skh, list);
        let dict_key = self.schema.dict_key(skh);
        let all_key = self.schema.all_lists_key(skh);
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let mut pipe = bb8_redis::redis::pipe();
        if let Some(version) = &state.loading_version {
            pipe.cmd("UNLINK")
                .arg(self.schema.fwd_key(skh, list, version))
                .ignore();
        }
        // State is deleted last. If the connection drops earlier, its fan-out marker
        // leaves enough intent for a later worker to retry this idempotent cleanup.
        let mut dict_fields = vec![DictClient::name_field(list)];
        if let Some(token) = token {
            dict_fields.push(DictClient::token_field(token));
        }
        pipe.cmd("UNLINK")
            .arg(&forward_key)
            .ignore()
            .cmd("DEL")
            .arg(meta_key)
            .ignore()
            .cmd("HDEL")
            .arg(dict_key)
            .arg(dict_fields)
            .ignore()
            .cmd("SREM")
            .arg(all_key)
            .arg(list)
            .ignore()
            .cmd("DEL")
            .arg(state_key)
            .ignore();
        pipe.query_async::<()>(&mut *conn).await?;
        metric(ProxyEventType::IdListRemoved, 1);
        Ok(())
    }
}
