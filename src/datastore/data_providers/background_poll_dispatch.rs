//! Background polling dispatch helpers.
//!
//! This module intentionally only contains pure/low-dependency scheduling logic
//! (concurrency caps, launch spacing, poll-cadence jitter, and deterministic
//! per-cycle ordering) so the request execution path in
//! `background_data_provider` stays easier to read.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use tokio::time::Duration;
use uuid::Uuid;

const POLL_INTERVAL_JITTER_DIVISOR: u64 = 10;

#[derive(Clone, Copy, Debug)]
pub struct BackgroundPollDispatchConfig {
    /// Maximum concurrent requests launched by the background poll scheduler.
    pub max_in_flight: usize,
    /// Minimum delay between background request launches within a poll cycle.
    ///
    /// `0` means no launch pacing.
    item_spacing_ms: u64,
    /// Per-process randomization seed used to vary order across pods.
    jitter_seed: u64,
}

impl BackgroundPollDispatchConfig {
    /// Builds a dispatch config from CLI/runtime knobs.
    pub fn new(max_in_flight: u64, item_spacing_ms: u64) -> Self {
        Self {
            max_in_flight: max_in_flight.max(1) as usize,
            item_spacing_ms,
            jitter_seed: (Uuid::new_v4().as_u128() as u64)
                ^ ((Uuid::new_v4().as_u128() >> 64) as u64),
        }
    }

    pub(crate) fn background_dispatch_shaping_enabled(
        &self,
        prepared_request_count: usize,
    ) -> bool {
        self.item_spacing_ms > 0 || self.max_in_flight < prepared_request_count
    }

    pub(crate) fn launch_spacing(&self) -> Duration {
        Duration::from_millis(self.item_spacing_ms)
    }

    /// Returns the jittered base interval to use before the next polling cycle.
    ///
    /// Each process gets its own `jitter_seed`, so pods started at the same time
    /// still drift apart. The interval is bounded to +/-10% of the configured
    /// polling cadence so the overall refresh rate stays close to the user
    /// supplied value.
    pub(crate) fn poll_interval_for_cycle(
        &self,
        base_interval: Duration,
        poll_cycle: u64,
    ) -> Duration {
        let base_interval_ms = base_interval.as_millis().clamp(1, u128::from(u64::MAX)) as u64;
        let jitter_window_ms = (base_interval_ms / POLL_INTERVAL_JITTER_DIVISOR).max(1);
        let jitter_bucket_count = jitter_window_ms.saturating_mul(2).saturating_add(1);
        let jitter_bucket = cycle_jitter_mix(poll_cycle, self.jitter_seed) % jitter_bucket_count;
        let jitter_offset_ms = jitter_bucket as i128 - jitter_window_ms as i128;

        let jittered_interval_ms = if jitter_offset_ms.is_negative() {
            base_interval_ms
                .saturating_sub(jitter_offset_ms.unsigned_abs() as u64)
                .max(1)
        } else {
            base_interval_ms.saturating_add(jitter_offset_ms as u64)
        };

        Duration::from_millis(jittered_interval_ms)
    }

    pub(crate) fn jitter_seed(&self) -> u64 {
        self.jitter_seed
    }
}

/// Produces a deterministic ordering rank for a request key during a poll cycle.
///
/// The same key/cycle/seed gives the same rank (stable in tests), while changing
/// the cycle and/or seed changes the ordering to avoid synchronized request
/// bursts across polling iterations and across pods.
pub(crate) fn dispatch_rank<T: Hash>(request_key: &T, poll_cycle: u64, jitter_seed: u64) -> u64 {
    let mut hasher = DefaultHasher::new();
    request_key.hash(&mut hasher);
    let key_hash = hasher.finish();
    key_hash ^ cycle_jitter_mix(poll_cycle, jitter_seed)
}

fn cycle_jitter_mix(poll_cycle: u64, jitter_seed: u64) -> u64 {
    let cycle_mix = poll_cycle.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    jitter_seed.rotate_left((poll_cycle & 63) as u32) ^ cycle_mix
}

#[cfg(test)]
mod tests {
    use super::{dispatch_rank, BackgroundPollDispatchConfig};
    use crate::servers::authorized_request_context::AuthorizedRequestContext;
    use crate::servers::normalized_path::NormalizedPath;
    use crate::utils::compress_encoder::CompressionEncoder;
    use std::sync::Arc;
    use tokio::time::Duration;

    type ForegroundFetchLockKey = (Arc<AuthorizedRequestContext>, Option<Arc<str>>);

    #[test]
    fn background_poll_dispatch_config_detects_when_dispatch_shaping_is_needed() {
        let config = BackgroundPollDispatchConfig::new(10, 5);
        assert!(config.background_dispatch_shaping_enabled(4));

        let config = BackgroundPollDispatchConfig::new(2, 0);
        assert!(config.background_dispatch_shaping_enabled(4));

        let config = BackgroundPollDispatchConfig::new(4, 0);
        assert!(!config.background_dispatch_shaping_enabled(4));
    }

    #[test]
    fn background_poll_dispatch_config_returns_launch_spacing() {
        let config = BackgroundPollDispatchConfig::new(2, 25);
        assert_eq!(config.launch_spacing(), Duration::from_millis(25));
    }

    #[test]
    fn background_poll_dispatch_rank_is_stable_per_cycle_and_changes_across_cycles() {
        let request_context = Arc::new(AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::Gzip],
        ));
        let key: ForegroundFetchLockKey = (request_context, None);

        let rank1 = dispatch_rank(&key, 7, 12345);
        let rank2 = dispatch_rank(&key, 7, 12345);
        let next_cycle_rank = dispatch_rank(&key, 8, 12345);

        assert_eq!(rank1, rank2);
        assert_ne!(rank1, next_cycle_rank);
    }

    #[test]
    fn background_poll_dispatch_config_jitters_poll_interval_within_expected_bounds() {
        let config = BackgroundPollDispatchConfig {
            max_in_flight: 4,
            item_spacing_ms: 0,
            jitter_seed: 12345,
        };

        let base_interval = Duration::from_secs(10);
        let cycle_1_interval = config.poll_interval_for_cycle(base_interval, 1);
        let cycle_2_interval = config.poll_interval_for_cycle(base_interval, 2);

        assert_eq!(
            cycle_1_interval,
            config.poll_interval_for_cycle(base_interval, 1)
        );
        assert!(cycle_1_interval >= Duration::from_secs(9));
        assert!(cycle_1_interval <= Duration::from_secs(11));
        assert!(cycle_2_interval >= Duration::from_secs(9));
        assert!(cycle_2_interval <= Duration::from_secs(11));
        assert_ne!(cycle_1_interval, cycle_2_interval);
    }
}
