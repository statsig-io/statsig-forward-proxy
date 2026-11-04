use std::collections::HashSet;

use bb8_redis::redis::AsyncCommands;

use crate::datastore::caching::redis_pool::pool_get_err;
use crate::observers::ProxyEventType;

use super::super::schema::state_fields;
use super::core::LOCK_RENEWAL_SCAN_PAGES;
use super::parser::partition_for;
use super::run::{metric, ClaimRenewal};
use super::{IdListSyncWorker, WorkerError};

impl IdListSyncWorker {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn apply_streamed_diff(
        &self,
        skh: &str,
        old_key: Option<&str>,
        new_key: &str,
        token: u64,
        state_key: &str,
        resume_partition: u64,
        old_card: u64,
        new_card: u64,
        renewal: &ClaimRenewal,
    ) -> Result<(), WorkerError> {
        if old_card == 0 {
            self.fanout_scan(skh, new_key, token, true, new_card, renewal)
                .await?;
            return Ok(());
        }
        let old_key = old_key.ok_or_else(|| {
            WorkerError::ScanVerification("old cardinality is non-zero without a key".to_string())
        })?;
        // Derive P from Redis cardinality, not manifest bytes. This keeps the
        // partition plan deterministic on takeover and bounds each in-memory old set.
        let partitions = old_card
            .max(new_card)
            .max(1)
            .div_ceil(self.config.diff_partition_max_members);
        if resume_partition > partitions {
            return Err(WorkerError::ScanVerification(format!(
                "resume partition {resume_partition} exceeds partition count {partitions}"
            )));
        }

        let mut distinct_old_total = 0u64;
        // Re-verify completed partitions after takeover without replaying their fan-out.
        for partition in 0..resume_partition {
            distinct_old_total += self
                .scan_partition(old_key, old_card, partition, partitions, renewal)
                .await?
                .len() as u64;
        }
        for partition in resume_partition..partitions {
            let mut existing = self
                .scan_partition(old_key, old_card, partition, partitions, renewal)
                .await?;
            distinct_old_total += existing.len() as u64;

            let mut cursor = 0u64;
            let mut scanned = 0u64;
            let mut pages = 0usize;
            let mut additions = Vec::with_capacity(self.config.scan_count);
            self.verify_scan_key_before(new_key, new_card).await?;
            loop {
                let (next, members) = self.sscan_page(new_key, cursor).await?;
                scanned += members.len() as u64;
                for member in members {
                    if partition_for(&member, partitions) != partition {
                        continue;
                    }
                    // Drain intersections from `existing`. New-only members fan out
                    // immediately; whatever remains after the scan is old-only.
                    if !existing.remove(&member) {
                        additions.push(member);
                        if additions.len() >= self.config.scan_count {
                            self.fanout_members(skh, &additions, token, true).await?;
                            additions.clear();
                        }
                    }
                }
                pages += 1;
                if pages.is_multiple_of(LOCK_RENEWAL_SCAN_PAGES) {
                    renewal.renew_now().await?;
                }
                cursor = next;
                if cursor == 0 {
                    break;
                }
            }
            self.verify_scan_complete(new_key, new_card, scanned)
                .await?;
            if !additions.is_empty() {
                self.fanout_members(skh, &additions, token, true).await?;
            }
            let removals: Vec<String> = existing.into_iter().collect();
            for chunk in removals.chunks(self.config.scan_count) {
                self.fanout_members(skh, chunk, token, false).await?;
            }
            let mut conn = self.pool.get().await.map_err(pool_get_err)?;
            // Advance only after both addition and removal fan-out complete. Replaying
            // a crashed partition is safe because Redis set mutations are idempotent.
            let _: () = conn
                .hset(
                    state_key,
                    state_fields::DIFF_PARTITION,
                    partition.saturating_add(1),
                )
                .await?;
            metric(ProxyEventType::IdListDiffPartitionsDone, 1);
            renewal.renew_now().await?;
        }
        if distinct_old_total != old_card {
            metric(ProxyEventType::IdListScanVerifyFailed, 1);
            return Err(WorkerError::ScanVerification(format!(
                "old distinct total {distinct_old_total} != SCARD {old_card}"
            )));
        }
        Ok(())
    }

    async fn scan_partition(
        &self,
        key: &str,
        expected_card: u64,
        partition: u64,
        partitions: u64,
        renewal: &ClaimRenewal,
    ) -> Result<HashSet<String>, WorkerError> {
        self.verify_scan_key_before(key, expected_card).await?;
        let mut members = HashSet::new();
        let mut cursor = 0u64;
        let mut scanned = 0u64;
        let mut pages = 0usize;
        loop {
            let (next, page) = self.sscan_page(key, cursor).await?;
            scanned += page.len() as u64;
            members.extend(
                page.into_iter()
                    .filter(|member| partition_for(member, partitions) == partition),
            );
            pages += 1;
            if pages.is_multiple_of(LOCK_RENEWAL_SCAN_PAGES) {
                renewal.renew_now().await?;
            }
            cursor = next;
            if cursor == 0 {
                break;
            }
        }
        self.verify_scan_complete(key, expected_card, scanned)
            .await?;
        Ok(members)
    }

    pub(super) async fn fanout_scan(
        &self,
        skh: &str,
        key: &str,
        token: u64,
        add: bool,
        expected_card: u64,
        renewal: &ClaimRenewal,
    ) -> Result<(), WorkerError> {
        self.verify_scan_key_before(key, expected_card).await?;
        let mut cursor = 0u64;
        let mut scanned = 0u64;
        let mut pages = 0usize;
        loop {
            let (next, members) = self.sscan_page(key, cursor).await?;
            scanned += members.len() as u64;
            self.fanout_members(skh, &members, token, add).await?;
            pages += 1;
            if pages.is_multiple_of(LOCK_RENEWAL_SCAN_PAGES) {
                renewal.renew_now().await?;
            }
            cursor = next;
            if cursor == 0 {
                break;
            }
        }
        self.verify_scan_complete(key, expected_card, scanned).await
    }

    async fn fanout_members(
        &self,
        skh: &str,
        members: &[String],
        token: u64,
        add: bool,
    ) -> Result<(), WorkerError> {
        for batch in members.chunks(self.config.pipeline_batch_size) {
            let mut conn = self.pool.get().await.map_err(pool_get_err)?;
            let mut pipe = bb8_redis::redis::pipe();
            for member in batch {
                pipe.cmd(if add { "SADD" } else { "SREM" })
                    .arg(self.schema.member_key(skh, member))
                    .arg(token)
                    .ignore();
            }
            pipe.query_async::<()>(&mut *conn).await?;
        }
        Ok(())
    }

    async fn sscan_page(&self, key: &str, cursor: u64) -> Result<(u64, Vec<String>), WorkerError> {
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        Ok(bb8_redis::redis::cmd("SSCAN")
            .arg(key)
            .arg(cursor)
            .arg("COUNT")
            .arg(self.config.scan_count)
            .query_async(&mut *conn)
            .await?)
    }

    async fn verify_scan_key_before(
        &self,
        key: &str,
        expected_card: u64,
    ) -> Result<(), WorkerError> {
        if expected_card > 0 && !self.key_exists(key).await? {
            metric(ProxyEventType::IdListScanVerifyFailed, 1);
            return Err(WorkerError::ScanVerification(format!(
                "{key} disappeared before scan"
            )));
        }
        Ok(())
    }

    async fn verify_scan_complete(
        &self,
        key: &str,
        expected_card: u64,
        scanned: u64,
    ) -> Result<(), WorkerError> {
        // SSCAN can return duplicate entries, so `scanned` may exceed SCARD. Falling
        // below the starting cardinality proves the traversal was incomplete.
        if scanned < expected_card || (expected_card > 0 && !self.key_exists(key).await?) {
            metric(ProxyEventType::IdListScanVerifyFailed, 1);
            return Err(WorkerError::ScanVerification(format!(
                "{key}: scanned {scanned}, expected at least {expected_card}"
            )));
        }
        Ok(())
    }

    /// Exceptional recovery when a forward set cannot describe all serving writes
    /// (lost Redis data or an abandoned partial rotation). SCAN bounds memory and
    /// touches only this company's member sets; other lists' tokens are preserved.
    pub(super) async fn clear_reverse_token(
        &self,
        skh: &str,
        token: u64,
        renewal: &ClaimRenewal,
    ) -> Result<(), WorkerError> {
        let prefix = self.schema.member_key(skh, "");
        let mut pattern = String::new();
        for ch in prefix.chars() {
            if matches!(ch, '*' | '?' | '[' | ']' | '\\') {
                pattern.push('\\');
            }
            pattern.push(ch);
        }
        pattern.push('*');
        let mut cursor = 0u64;
        loop {
            renewal.renew_now().await?;
            let mut conn = self.pool.get().await.map_err(pool_get_err)?;
            let (next, keys): (u64, Vec<String>) = bb8_redis::redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(self.config.scan_count)
                .query_async(&mut *conn)
                .await?;
            for batch in keys.chunks(self.config.pipeline_batch_size) {
                let mut pipe = bb8_redis::redis::pipe();
                for key in batch {
                    pipe.cmd("SREM").arg(key).arg(token).ignore();
                }
                pipe.query_async::<()>(&mut *conn).await?;
            }
            cursor = next;
            if cursor == 0 {
                break;
            }
        }
        Ok(())
    }

    pub(super) async fn key_exists(&self, key: &str) -> Result<bool, WorkerError> {
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        Ok(conn.exists(key).await?)
    }

    pub(super) async fn scard(&self, key: &str) -> Result<u64, WorkerError> {
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        Ok(conn.scard(key).await?)
    }
}
