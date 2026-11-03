//! Pure task planning and producer cadence/configuration helpers.
//!
//! Full vs Delta is decided here (`needs_work`) from Redis sync state + manifest metadata.
//! The producer and worker both use this policy so queue priority and execution agree.

use std::borrow::Cow;
use std::collections::HashSet;

#[cfg(test)]
use super::super::schema::IdListKeySchema;
use super::super::schema::{IdListMetadata, ListSyncState, TASK_OP_LOAD, TASK_OP_REMOVE};

/// Default sync tick / producer-lock TTL (`IDLIST_SYNC_INTERVAL_IN_S`).
pub const DEFAULT_SYNC_INTERVAL_S: u64 = 60;

/// Class multiplier for queue scores: `score = class * CLASS_SCORE_BASE + seq`.
const CLASS_SCORE_BASE: f64 = (1u64 << 40) as f64;

/// Minimum producer lock-attempt cadence (seconds).
const MIN_ATTEMPT_INTERVAL_S: u64 = 5;

/// Work the producer (and later the worker) decides is needed for one list.
///
/// Both [`FullLoad`] and [`Delta`] enqueue as `op=load`. The worker re-derives Full vs
/// Delta from state + `task_meta` (url / file_id / creation_time / size). Queue score
/// class only prioritizes: Full (0) before Delta (1) before Remove (2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncAction {
    /// Download from byte 0 and rebuild this list. Not only “never existed”: also
    /// `file_id` rotation, Redis recovery, or empty state.
    FullLoad,
    /// Same `file_id`; file only grew. Ranged download
    /// from `state.size`. `lag` = `meta.size - state.size`. Steady-state appends, not
    /// crash-recovery-only — an interrupted download looks the same.
    Delta { lag: u64 },
    /// List left the manifest — workers delete membership + keys.
    Remove,
}

impl SyncAction {
    pub fn class(&self) -> u8 {
        match self {
            SyncAction::FullLoad => 0,
            SyncAction::Delta { .. } => 1,
            SyncAction::Remove => 2,
        }
    }

    pub fn lag(&self) -> u64 {
        match self {
            SyncAction::Delta { lag } => *lag,
            _ => 0,
        }
    }

    pub fn op_str(&self) -> &'static str {
        match self {
            SyncAction::Remove => TASK_OP_REMOVE,
            // Full and Delta share `load`; the worker infers Range vs byte-0 from state.
            SyncAction::FullLoad | SyncAction::Delta { .. } => TASK_OP_LOAD,
        }
    }
}

/// One derived queue task before enqueue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivedTask {
    pub skh: String,
    pub list: String,
    pub action: SyncAction,
    pub meta: Option<IdListMetadata>,
}

/// Decides whether a list needs work given Redis state + manifest metadata.
///
/// `fwd_exists` is whether the forward SET for `state.version` exists (flush detection).
/// File identity and byte offsets determine full loads vs appends. A stale rotation
/// cannot replace a newer committed file; reverse repair targets the committed file.
///
/// Full vs Delta:
/// - **Full** for first loads, a changed `file_id` with strictly newer creation time,
///   or recovery of the current file (interrupted load / missing non-empty forward SET).
/// - **Delta** for the same `file_id` when the file grew; creation time is ignored.
/// - **None** for a changed `file_id` with equal/older creation time, or an unchanged
///   file that did not grow. Byte offsets must never carry across different files.
///
/// Re-enabling reverse indexing can still repair the committed snapshot when the
/// manifest advertises an older file; that file never becomes the repair target.
pub fn needs_work(
    state: Option<&ListSyncState>,
    meta: &IdListMetadata,
    fwd_exists: bool,
) -> Option<SyncAction> {
    plan_sync_with_reverse_index(state, meta, fwd_exists, true).map(|(action, _)| action)
}

/// Keep the action and its authoritative file metadata together. A stale manifest
/// must not suppress reverse-index repair or become the source of a rollback.
pub(crate) fn plan_sync_with_reverse_index(
    state: Option<&ListSyncState>,
    meta: &IdListMetadata,
    fwd_exists: bool,
    enable_reverse_index: bool,
) -> Option<(SyncAction, IdListMetadata)> {
    let target = match state {
        Some(state)
            if enable_reverse_index
                && state.reverse_index_disabled
                && !state.version.is_empty()
                && meta.file_id != state.file_id
                && meta.creation_time <= state.creation_time =>
        {
            Cow::Owned(IdListMetadata {
                file_id: state.file_id.clone(),
                creation_time: state.creation_time,
                url: state.url.clone(),
                size: state.size,
            })
        }
        _ => Cow::Borrowed(meta),
    };
    needs_work_with_reverse_index(state, &target, fwd_exists, enable_reverse_index)
        .map(|action| (action, target.into_owned()))
}

fn needs_work_with_reverse_index(
    state: Option<&ListSyncState>,
    meta: &IdListMetadata,
    fwd_exists: bool,
    enable_reverse_index: bool,
) -> Option<SyncAction> {
    let Some(state) = state else {
        return Some(SyncAction::FullLoad); // never synced
    };

    // The worker cannot use a state without a committed version.
    if state.version.is_empty() {
        return Some(SyncAction::FullLoad);
    }

    // A different file is authoritative only when its creation time is newer.
    // Do not resume from the current file's byte offset, even if the stale file grew.
    if meta.file_id != state.file_id {
        return (meta.creation_time > state.creation_time).then_some(SyncAction::FullLoad);
    }

    if enable_reverse_index && state.reverse_index_disabled {
        return Some(SyncAction::FullLoad);
    }

    // Resume an interrupted full load. apply_full_reload decides whether its version
    // matches the current manifest or must be restarted from byte zero.
    if state.loading_version.is_some() {
        return Some(SyncAction::FullLoad);
    }

    // Redis deletes empty sets. Use member cardinality, falling back conservatively
    // to file size for checkpoints written before cardinality was tracked.
    if state.cardinality.unwrap_or(state.size) > 0 && !fwd_exists {
        return Some(SyncAction::FullLoad);
    }

    // Same file, more bytes: normal append (also covers resume after a partial apply).
    if meta.size > state.size {
        return Some(SyncAction::Delta {
            lag: meta.size - state.size,
        });
    }

    None
}

/// Sorts tasks: class 0 → class 1 (lag desc) → class 2, then assigns queue scores.
///
/// Returns `(score, task)` pairs ready for `ZADD`. In-tick dedupe by `(skh, list)` —
/// first occurrence wins (after sort, so the highest-priority action is kept).
pub fn order_and_dedupe_tasks(mut tasks: Vec<DerivedTask>) -> Vec<(f64, DerivedTask)> {
    tasks.sort_by(|a, b| {
        a.action
            .class()
            .cmp(&b.action.class())
            .then_with(|| b.action.lag().cmp(&a.action.lag()))
            .then_with(|| a.skh.cmp(&b.skh))
            .then_with(|| a.list.cmp(&b.list))
    });

    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(tasks.len());
    let mut seq_by_class: [u64; 3] = [0, 0, 0];

    for task in tasks {
        let key = (task.skh.clone(), task.list.clone());
        if !seen.insert(key) {
            continue;
        }
        let class = task.action.class() as usize;
        let seq = seq_by_class[class];
        seq_by_class[class] = seq + 1;
        let score = (task.action.class() as f64) * CLASS_SCORE_BASE + seq as f64;
        out.push((score, task));
    }
    out
}

/// Queue score for a task given its class and within-class sequence index.
pub fn task_score(class: u8, seq: u64) -> f64 {
    (class as f64) * CLASS_SCORE_BASE + seq as f64
}

/// Attempt interval: `interval/4`, floored at [`MIN_ATTEMPT_INTERVAL_S`].
pub fn producer_attempt_interval_s(interval_s: u64) -> u64 {
    (interval_s / 4).max(MIN_ATTEMPT_INTERVAL_S)
}
const SYNC_INTERVAL_ENV: &str = "IDLIST_SYNC_INTERVAL_IN_S";

/// Reads `IDLIST_SYNC_INTERVAL_IN_S` (default [`DEFAULT_SYNC_INTERVAL_S`]).
pub fn sync_interval_from_env() -> u64 {
    parse_sync_interval(std::env::var(SYNC_INTERVAL_ENV).ok().as_deref())
}

fn parse_sync_interval(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_SYNC_INTERVAL_S)
}

pub const IDLIST_SYNC_KEYS_ENV: &str = "SFP_IDLIST_SYNC_KEYS_JSON";

/// Server keys shared by id-list synchronization and the enrichment endpoint.
///
/// `Ok(None)` when [`IDLIST_SYNC_KEYS_ENV`] is unset, which leaves both features off.
/// A present value turns both on and must be a non-empty JSON array of keys.
pub fn idlist_sync_keys_from_env() -> Result<Option<Vec<String>>, String> {
    match std::env::var(IDLIST_SYNC_KEYS_ENV) {
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(format!("failed reading {IDLIST_SYNC_KEYS_ENV}: {error}")),
        Ok(raw) => idlist_sync_keys_from_json(&raw).map(Some),
    }
}

pub(crate) fn idlist_sync_keys_from_json(raw: &str) -> Result<Vec<String>, String> {
    let mut keys: Vec<String> = serde_json::from_str(raw)
        .map_err(|error| format!("invalid JSON for {IDLIST_SYNC_KEYS_ENV}: {error}"))?;
    if keys.iter().any(|key| key.trim().is_empty()) {
        return Err(format!("{IDLIST_SYNC_KEYS_ENV} cannot contain empty keys"));
    }
    keys.sort();
    keys.dedup();
    if keys.is_empty() {
        return Err(format!(
            "{IDLIST_SYNC_KEYS_ENV} must contain at least one key"
        ));
    }
    Ok(keys)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn meta(file_id: &str, creation_time: u64, size: u64) -> IdListMetadata {
        IdListMetadata {
            file_id: file_id.to_string(),
            creation_time,
            url: "https://example.com/list".to_string(),
            size,
        }
    }

    fn state(file_id: &str, creation_time: u64, size: u64, version: &str) -> ListSyncState {
        ListSyncState {
            version: version.to_string(),
            file_id: file_id.to_string(),
            url: "https://example.com/list".to_string(),
            creation_time,
            size,
            loading_version: None,
            loading_size: None,
            diff_partition: None,
            ..ListSyncState::default()
        }
    }

    #[test]
    fn needs_work_no_state_is_full_load() {
        let m = meta("f1", 100, 50);
        assert_eq!(needs_work(None, &m, false), Some(SyncAction::FullLoad));
    }

    #[test]
    fn needs_work_empty_state_is_full_load() {
        let m = meta("f1", 100, 50);
        let s = ListSyncState::default();
        assert_eq!(needs_work(Some(&s), &m, false), Some(SyncAction::FullLoad));
    }

    #[test]
    fn needs_work_missing_forward_set_is_full_load() {
        let m = meta("f1", 100, 50);
        let s = state("f1", 100, 50, "f1");
        assert_eq!(needs_work(Some(&s), &m, false), Some(SyncAction::FullLoad));
    }

    #[test]
    fn needs_work_file_id_change_is_full_load() {
        let m = meta("f2", 200, 50);
        let s = state("f1", 100, 50, "f1");
        assert_eq!(needs_work(Some(&s), &m, true), Some(SyncAction::FullLoad));
    }

    #[test]
    fn needs_work_newer_creation_time_without_growth_is_unchanged() {
        let m = meta("f1", 200, 50);
        let s = state("f1", 100, 50, "f1");
        assert_eq!(needs_work(Some(&s), &m, true), None);
    }

    #[test]
    fn parse_sync_interval_uses_positive_seconds_or_default() {
        assert_eq!(parse_sync_interval(Some("5")), 5);
        assert_eq!(parse_sync_interval(Some("0")), DEFAULT_SYNC_INTERVAL_S);
        assert_eq!(
            parse_sync_interval(Some("invalid")),
            DEFAULT_SYNC_INTERVAL_S
        );
        assert_eq!(parse_sync_interval(None), DEFAULT_SYNC_INTERVAL_S);
    }

    #[test]
    fn needs_work_newer_creation_time_with_growth_is_delta() {
        let m = meta("f1", 200, 80);
        let s = state("f1", 100, 50, "f1");
        assert_eq!(
            needs_work(Some(&s), &m, true),
            Some(SyncAction::Delta { lag: 30 })
        );
    }

    #[test]
    fn needs_work_older_creation_time_with_growth_is_delta_like_sdk() {
        let m = meta("f1", 50, 80);
        let s = state("f1", 100, 50, "f1");
        assert_eq!(
            needs_work(Some(&s), &m, true),
            Some(SyncAction::Delta { lag: 30 })
        );
    }

    #[test]
    fn needs_work_stale_rotation_is_ignored_even_if_larger_or_recovery_is_pending() {
        for creation_time in [50, 100] {
            for size in [40, 50, 80] {
                for recovery in [false, true] {
                    let m = meta("f2", creation_time, size);
                    let mut s = state("f1", 100, 50, "f1");
                    if recovery {
                        s.loading_version = Some("f1@100".to_string());
                    }
                    for forward_exists in [false, true] {
                        assert_eq!(needs_work(Some(&s), &m, forward_exists), None);
                    }
                }
            }
        }
    }

    #[test]
    fn reenabled_reverse_repair_targets_committed_metadata_for_stale_rotations() {
        let mut current = state("current", 100, 50, "current@100");
        current.url = "https://example.com/current".into();
        current.reverse_index_disabled = true;
        let committed = IdListMetadata {
            file_id: current.file_id.clone(),
            creation_time: current.creation_time,
            url: current.url.clone(),
            size: current.size,
        };
        for creation_time in [50, 100] {
            for size in [40, 80] {
                let stale = meta("older", creation_time, size);
                for forward_exists in [false, true] {
                    assert_eq!(
                        plan_sync_with_reverse_index(Some(&current), &stale, forward_exists, true),
                        Some((SyncAction::FullLoad, committed.clone()))
                    );
                    assert_eq!(
                        plan_sync_with_reverse_index(Some(&current), &stale, forward_exists, false),
                        None
                    );
                }
            }
        }
    }

    #[test]
    fn reverse_repair_keeps_newer_manifest_and_initial_load_metadata() {
        let mut current = state("current", 100, 50, "current@100");
        current.reverse_index_disabled = true;
        let newer = meta("newer", 200, 80);
        assert_eq!(
            plan_sync_with_reverse_index(Some(&current), &newer, true, true),
            Some((SyncAction::FullLoad, newer.clone()))
        );
        current.version.clear();
        current.creation_time = 300;
        assert_eq!(
            plan_sync_with_reverse_index(Some(&current), &newer, false, true),
            Some((SyncAction::FullLoad, newer))
        );
    }

    #[test]
    fn needs_work_same_file_ignores_creation_time_including_empty_file_id() {
        for file_id in ["f1", ""] {
            for creation_time in [50, 100, 200] {
                let s = state(file_id, 100, 50, "committed");
                assert_eq!(
                    needs_work(Some(&s), &meta(file_id, creation_time, 50), true),
                    None
                );
                assert_eq!(
                    needs_work(Some(&s), &meta(file_id, creation_time, 80), true),
                    Some(SyncAction::Delta { lag: 30 })
                );
            }
        }
    }

    #[test]
    fn needs_work_resumes_loading_before_sdk_size_checks() {
        let m = meta("f1", 100, 50);
        let mut s = state("f1", 100, 50, "f1");
        s.loading_version = Some("f1".to_string());
        assert_eq!(needs_work(Some(&s), &m, true), Some(SyncAction::FullLoad));
    }

    #[test]
    fn needs_work_zero_byte_list_does_not_require_a_forward_set() {
        let m = meta("f1", 100, 0);
        let s = state("f1", 100, 0, "f1");
        assert_eq!(needs_work(Some(&s), &m, false), None);
    }

    #[test]
    fn needs_work_size_growth_is_delta_with_lag() {
        let m = meta("f1", 100, 80);
        let s = state("f1", 100, 50, "f1");
        assert_eq!(
            needs_work(Some(&s), &m, true),
            Some(SyncAction::Delta { lag: 30 })
        );
    }

    #[test]
    fn needs_work_same_generation_size_shrink_is_skipped_like_sdk() {
        let m = meta("f1", 100, 40);
        let s = state("f1", 100, 50, "f1");
        assert_eq!(needs_work(Some(&s), &m, true), None);
    }

    #[test]
    fn needs_work_up_to_date_returns_none() {
        let m = meta("f1", 100, 50);
        let s = state("f1", 100, 50, "f1");
        assert_eq!(needs_work(Some(&s), &m, true), None);
    }

    #[test]
    fn order_full_loads_before_deltas_before_removals() {
        let tasks = vec![
            DerivedTask {
                skh: "skh".into(),
                list: "delta_big".into(),
                action: SyncAction::Delta { lag: 100 },
                meta: None,
            },
            DerivedTask {
                skh: "skh".into(),
                list: "remove_me".into(),
                action: SyncAction::Remove,
                meta: None,
            },
            DerivedTask {
                skh: "skh".into(),
                list: "full_a".into(),
                action: SyncAction::FullLoad,
                meta: None,
            },
            DerivedTask {
                skh: "skh".into(),
                list: "delta_small".into(),
                action: SyncAction::Delta { lag: 10 },
                meta: None,
            },
        ];
        let ordered = order_and_dedupe_tasks(tasks);
        let names: Vec<_> = ordered.iter().map(|(_, t)| t.list.as_str()).collect();
        assert_eq!(
            names,
            vec!["full_a", "delta_big", "delta_small", "remove_me"]
        );
        assert!(ordered[0].0 < ordered[1].0);
        assert!(ordered[1].0 < ordered[2].0);
        assert!(ordered[2].0 < ordered[3].0);
        // Class boundaries
        assert!(ordered[0].0 < CLASS_SCORE_BASE);
        assert!(ordered[1].0 >= CLASS_SCORE_BASE && ordered[1].0 < 2.0 * CLASS_SCORE_BASE);
        assert!(ordered[3].0 >= 2.0 * CLASS_SCORE_BASE);
    }

    #[test]
    fn in_tick_dedupe_keeps_highest_priority_action() {
        // Same (skh, list) appears as FullLoad and Delta — after sort FullLoad wins.
        let tasks = vec![
            DerivedTask {
                skh: "skh".into(),
                list: "x".into(),
                action: SyncAction::Delta { lag: 5 },
                meta: None,
            },
            DerivedTask {
                skh: "skh".into(),
                list: "x".into(),
                action: SyncAction::FullLoad,
                meta: None,
            },
        ];
        let ordered = order_and_dedupe_tasks(tasks);
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].1.action, SyncAction::FullLoad);
    }

    #[test]
    fn task_encoding_round_trip_via_schema() {
        let encoded = IdListKeySchema::encode_task("abc", "list::weird");
        let (skh, list) = IdListKeySchema::decode_task(&encoded).unwrap();
        assert_eq!(skh, "abc");
        assert_eq!(list, "list::weird");
    }

    #[test]
    fn producer_attempt_interval_floors_at_five() {
        assert_eq!(producer_attempt_interval_s(60), 15);
        assert_eq!(producer_attempt_interval_s(8), 5);
        assert_eq!(producer_attempt_interval_s(4), 5);
    }

    #[test]
    fn task_score_orders_classes() {
        assert!(task_score(0, 999) < task_score(1, 0));
        assert!(task_score(1, 999) < task_score(2, 0));
    }

    #[test]
    fn sync_keys_json_dedupes_server_keys() {
        let keys = idlist_sync_keys_from_json(r#"["secret-2","secret-1","secret-1"]"#).unwrap();
        assert_eq!(keys, vec!["secret-1".to_string(), "secret-2".to_string()]);
    }

    #[test]
    fn sync_keys_json_rejects_malformed_or_empty_values() {
        let err = idlist_sync_keys_from_json("not-json").unwrap_err();
        assert!(err.contains("invalid JSON"));
        assert!(idlist_sync_keys_from_json("[]").is_err());
        assert!(idlist_sync_keys_from_json(r#"[""]"#).is_err());
    }
}
