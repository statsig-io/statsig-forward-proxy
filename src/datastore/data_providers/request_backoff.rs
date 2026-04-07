use std::cmp::min;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::servers::authorized_request_context::AuthorizedRequestContext;

use super::DataProviderRequestResult;

pub(in crate::datastore::data_providers) type RequestBackoffKey =
    (Arc<AuthorizedRequestContext>, Option<Arc<str>>);

#[derive(Clone)]
pub(in crate::datastore::data_providers) struct RequestBackoffPolicy {
    pub(super) initial_delay: Duration,
    pub(super) max_delay: Duration,
}

impl RequestBackoffPolicy {
    pub(in crate::datastore::data_providers) fn from_polling_interval(
        polling_interval_in_s: u64,
    ) -> Self {
        let base_delay = Duration::from_secs(polling_interval_in_s.max(1));
        Self {
            initial_delay: base_delay,
            max_delay: Duration::from_secs(min(base_delay.as_secs().saturating_mul(32), 300)),
        }
    }

    pub(super) fn delay_for_failure(&self, failure_count: u32) -> Duration {
        let exponent = failure_count.saturating_sub(1).min(31);
        let factor = 2_u32.saturating_pow(exponent);

        let scaled_delay = self
            .initial_delay
            .checked_mul(factor)
            .unwrap_or(self.max_delay);
        min(scaled_delay, self.max_delay)
    }
}

#[derive(Clone)]
struct RequestBackoffState {
    consecutive_failures: u32,
    retry_after: Instant,
    last_seen: Instant,
}

pub(in crate::datastore::data_providers) struct RequestBackoffController {
    policy: RequestBackoffPolicy,
    state_by_key: DashMap<RequestBackoffKey, RequestBackoffState>,
}

impl RequestBackoffController {
    pub(in crate::datastore::data_providers) fn new(policy: RequestBackoffPolicy) -> Self {
        Self {
            policy,
            state_by_key: DashMap::new(),
        }
    }

    pub(in crate::datastore::data_providers) fn can_attempt(
        &self,
        key: &RequestBackoffKey,
    ) -> bool {
        self.can_attempt_at(key, Instant::now())
    }

    fn can_attempt_at(&self, key: &RequestBackoffKey, now: Instant) -> bool {
        let Some(mut state) = self.state_by_key.get_mut(key) else {
            return true;
        };

        // Refresh last-seen so active keys don't get pruned while backing off.
        state.last_seen = now;
        now >= state.retry_after
    }

    pub(in crate::datastore::data_providers) fn record_result(
        &self,
        key: &RequestBackoffKey,
        result: DataProviderRequestResult,
    ) {
        match result {
            DataProviderRequestResult::Unauthorized => self.record_success(key),
            DataProviderRequestResult::Error => self.record_failure(key),
            _ => self.record_success(key),
        }
    }

    fn record_success(&self, key: &RequestBackoffKey) {
        self.state_by_key.remove(key);
    }

    pub(in crate::datastore::data_providers) fn record_failure(&self, key: &RequestBackoffKey) {
        self.record_failure_at(key, Instant::now());
    }

    fn record_failure_at(&self, key: &RequestBackoffKey, now: Instant) {
        self.state_by_key
            .entry(key.clone())
            .and_modify(|state| {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                state.retry_after = now + self.policy.delay_for_failure(state.consecutive_failures);
                state.last_seen = now;
            })
            .or_insert_with(|| RequestBackoffState {
                consecutive_failures: 1,
                retry_after: now + self.policy.delay_for_failure(1),
                last_seen: now,
            });
    }

    pub(in crate::datastore::data_providers) fn prune_stale(&self) {
        self.prune_stale_at(Instant::now());
    }

    fn prune_stale_at(&self, now: Instant) {
        // Any key that hasn't been referenced in a while is assumed inactive and is removed.
        // This prevents unbounded growth when keys churn (e.g. Unauthorized removes from store).
        let stale_after = self
            .policy
            .max_delay
            .checked_mul(2)
            .unwrap_or(self.policy.max_delay);

        let mut to_remove: HashSet<RequestBackoffKey> = HashSet::new();
        for entry in self.state_by_key.iter() {
            if now
                .saturating_duration_since(entry.value().last_seen)
                .gt(&stale_after)
            {
                to_remove.insert(entry.key().clone());
            }
        }

        for key in to_remove {
            self.state_by_key.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::servers::authorized_request_context::AuthorizedRequestContext;
    use crate::servers::normalized_path::NormalizedPath;
    use crate::utils::compress_encoder::CompressionEncoder;

    use super::{RequestBackoffController, RequestBackoffKey, RequestBackoffPolicy};

    #[test]
    fn backoff_policy_doubles_delay_and_caps_at_max() {
        let policy = RequestBackoffPolicy {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
        };

        assert_eq!(policy.delay_for_failure(1), Duration::from_secs(1));
        assert_eq!(policy.delay_for_failure(2), Duration::from_secs(2));
        assert_eq!(policy.delay_for_failure(3), Duration::from_secs(4));
        assert_eq!(policy.delay_for_failure(4), Duration::from_secs(8));
        assert_eq!(policy.delay_for_failure(5), Duration::from_secs(8));
    }

    #[test]
    fn request_backoff_controller_blocks_until_retry_window_expires() {
        let controller = RequestBackoffController::new(RequestBackoffPolicy {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
        });

        let request_context = Arc::new(AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::Gzip],
        ));
        let key: RequestBackoffKey = (request_context, None);

        let now = Instant::now();
        assert!(controller.can_attempt_at(&key, now));

        controller.record_failure_at(&key, now);
        assert!(!controller.can_attempt_at(&key, now + Duration::from_millis(900)));
        assert!(controller.can_attempt_at(&key, now + Duration::from_secs(1)));

        controller.record_failure_at(&key, now + Duration::from_secs(1));
        assert!(!controller.can_attempt_at(
            &key,
            now + Duration::from_secs(2) + Duration::from_millis(900)
        ));
        assert!(controller.can_attempt_at(&key, now + Duration::from_secs(3)));

        controller.record_success(&key);
        assert!(controller.can_attempt_at(&key, now + Duration::from_secs(3)));
    }

    #[test]
    fn request_backoff_controller_clears_state_on_unauthorized() {
        let controller = RequestBackoffController::new(RequestBackoffPolicy {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
        });

        let request_context = Arc::new(AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::Gzip],
        ));
        let key: RequestBackoffKey = (request_context, None);

        let now = Instant::now();
        controller.record_failure_at(&key, now);
        assert!(!controller.can_attempt_at(&key, now));

        controller.record_result(&key, super::DataProviderRequestResult::Unauthorized);
        assert!(controller.can_attempt_at(&key, now));
    }

    #[test]
    fn request_backoff_controller_prunes_stale_entries() {
        let controller = RequestBackoffController::new(RequestBackoffPolicy {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
        });

        let request_context = Arc::new(AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::Gzip],
        ));
        let key: RequestBackoffKey = (request_context, None);

        let now = Instant::now();
        controller.record_failure_at(&key, now);

        // stale_after = max_delay * 2 = 16s
        let later = now + Duration::from_secs(17);
        controller.prune_stale_at(later);

        // If the entry wasn't pruned, this would be the 2nd failure and the next delay would be 2s.
        // If it was pruned, the next delay will reset to 1s.
        controller.record_failure_at(&key, later);
        assert!(
            controller.can_attempt_at(&key, later + Duration::from_millis(1100)),
            "expected pruned entry to reset backoff to initial delay"
        );
    }
}
