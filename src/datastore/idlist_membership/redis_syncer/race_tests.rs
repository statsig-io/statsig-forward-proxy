use super::*;
use crate::datastore::caching::redis_pool::create_redis_pool_from_env;
use crate::datastore::id_list_manifest::{IdListManifest, IdListManifestEntry};
use crate::datastore::idlist_membership::new_holder_id;
use uuid::Uuid;

async fn pool_or_skip() -> Option<RedisPool> {
    create_redis_pool_from_env(5).await.ok()
}

fn test_schema() -> IdListKeySchema {
    IdListKeySchema::new(format!("test-b2-{}", Uuid::new_v4()))
}

fn one_list_manifest(
    name: &str,
    size: u64,
    url: &str,
    creation_time: u64,
    file_id: &str,
) -> IdListManifest {
    let mut manifest = IdListManifest::new();
    manifest.insert(
        name.to_string(),
        IdListManifestEntry {
            name: name.to_string(),
            size,
            url: url.to_string(),
            creation_time,
            file_id: file_id.to_string(),
        },
    );
    manifest
}

fn store_manifest(
    manifests: &Arc<ArcSwap<HashMap<String, ParsedManifest>>>,
    server_key: &str,
    manifest: IdListManifest,
) {
    let mut map = HashMap::new();
    map.insert(server_key.to_string(), manifest);
    manifests.store(Arc::new(map));
}

#[tokio::test]
async fn producer_election_exactly_one_winner_enqueues() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let schema = test_schema();
    let manifests = Arc::new(ArcSwap::from_pointee(HashMap::new()));
    let server_key = format!("secret-test-{}", Uuid::new_v4());
    let skh = hash_company_id(&server_key);

    let manifest = one_list_manifest("list_a", 100, "https://example.com/a", 1, "fid-a");
    store_manifest(&manifests, &server_key, manifest);

    let server_keys = Arc::new(vec![server_key]);
    let n = 8;
    let mut handles = Vec::new();
    for _ in 0..n {
        let producer = Arc::new(IdListSyncProducer::new(
            schema.clone(),
            Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
            pool.clone(),
            Arc::clone(&manifests),
            Arc::clone(&server_keys),
            30,
        ));
        handles.push(tokio::spawn(async move { producer.run_once().await }));
    }

    let outcomes: Vec<_> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap().unwrap())
        .collect();

    let successes = outcomes
        .iter()
        .filter(|o| matches!(o, ProducerTickOutcome::Success(_)))
        .count();
    let skipped = outcomes
        .iter()
        .filter(|o| matches!(o, ProducerTickOutcome::Skipped))
        .count();
    assert_eq!(successes, 1);
    assert_eq!(skipped, n - 1);

    let mut conn = pool.get().await.unwrap();
    let depth: u64 = conn.zcard(schema.queue_key()).await.unwrap();
    assert_eq!(depth, 1);
    let member = IdListKeySchema::encode_task(&skh, "list_a");
    let score: Option<f64> = conn.zscore(schema.queue_key(), &member).await.unwrap();
    assert!(score.is_some());
    let op: Option<String> = conn
        .hget(schema.task_meta_key(&skh, "list_a"), task_meta_fields::OP)
        .await
        .unwrap();
    assert_eq!(op.as_deref(), Some(TASK_OP_LOAD));
}

#[tokio::test]
async fn zadd_nx_dedup_refreshes_task_meta() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let schema = test_schema();
    let manifests = Arc::new(ArcSwap::from_pointee(HashMap::new()));
    let server_key = format!("secret-test-{}", Uuid::new_v4());
    let skh = hash_company_id(&server_key);

    let manifest = one_list_manifest("list_b", 10, "https://example.com/v1", 1, "fid-1");
    store_manifest(&manifests, &server_key, manifest);

    let lock = Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id()));
    // Short TTL so the second run can re-acquire after expiry.
    let producer = IdListSyncProducer::new(
        schema.clone(),
        Arc::clone(&lock),
        pool.clone(),
        Arc::clone(&manifests),
        Arc::new(vec![server_key.clone()]),
        2,
    );
    let first = producer.run_once().await.unwrap();
    assert!(matches!(first, ProducerTickOutcome::Success(_)));

    // Update manifest metadata while task still queued.
    let manifest2 = one_list_manifest("list_b", 20, "https://example.com/v2", 1, "fid-1");
    store_manifest(&manifests, &server_key, manifest2);

    // Wait for producer lock TTL to expire.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let second = producer.run_once().await.unwrap();
    match second {
        ProducerTickOutcome::Success(stats) => {
            // Already queued → ZADD NX adds nothing.
            assert_eq!(stats.tasks_enqueued, 0);
            assert_eq!(stats.queue_depth, 1);
        }
        other => panic!("expected success, got {other:?}"),
    }

    let mut conn = pool.get().await.unwrap();
    let url: Option<String> = conn
        .hget(schema.task_meta_key(&skh, "list_b"), task_meta_fields::URL)
        .await
        .unwrap();
    assert_eq!(url.as_deref(), Some("https://example.com/v2"));
    let size: Option<String> = conn
        .hget(schema.task_meta_key(&skh, "list_b"), task_meta_fields::SIZE)
        .await
        .unwrap();
    assert_eq!(size.as_deref(), Some("20"));
}

#[tokio::test]
async fn steady_state_enqueues_zero() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let schema = test_schema();
    let manifests = Arc::new(ArcSwap::from_pointee(HashMap::new()));
    let server_key = format!("secret-test-{}", Uuid::new_v4());
    let skh = hash_company_id(&server_key);

    // Manifest matches existing state + forward set → no work.
    let mut conn = pool.get().await.unwrap();
    let st = state("fid", 5, 100, "fid");
    let _: () = conn
        .hset_multiple(schema.state_key(&skh, "stable"), &st.to_hash_fields())
        .await
        .unwrap();
    let _: () = conn
        .sadd(schema.fwd_key(&skh, "stable", "fid"), "member")
        .await
        .unwrap();
    let _: () = conn
        .sadd(schema.all_lists_key(&skh), "stable")
        .await
        .unwrap();

    let manifest = one_list_manifest("stable", 100, "https://example.com/list", 5, "fid");
    store_manifest(&manifests, &server_key, manifest);

    let producer = IdListSyncProducer::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool.clone(),
        manifests,
        Arc::new(vec![server_key]),
        2,
    );

    for _ in 0..3 {
        // Release lock between ticks by waiting out TTL.
        let outcome = producer.run_once().await.unwrap();
        match outcome {
            ProducerTickOutcome::Success(stats) => {
                assert_eq!(stats.tasks_enqueued, 0);
                assert_eq!(stats.tasks_derived, 0);
                assert_eq!(stats.queue_depth, 0);
            }
            ProducerTickOutcome::Skipped => {}
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

#[tokio::test]
async fn removal_enqueued_when_list_leaves_manifest() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let schema = test_schema();
    let manifests = Arc::new(ArcSwap::from_pointee(HashMap::new()));
    let server_key = format!("secret-test-{}", Uuid::new_v4());
    let skh = hash_company_id(&server_key);

    let mut conn = pool.get().await.unwrap();
    let _: () = conn
        .sadd(schema.all_lists_key(&skh), "gone_list")
        .await
        .unwrap();

    // Empty manifest — gone_list should become a Remove task.
    store_manifest(&manifests, &server_key, IdListManifest::new());

    let producer = IdListSyncProducer::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool.clone(),
        manifests,
        Arc::new(vec![server_key]),
        30,
    );
    let outcome = producer.run_once().await.unwrap();
    match outcome {
        ProducerTickOutcome::Success(stats) => {
            assert_eq!(stats.removals_scheduled, 1);
            assert_eq!(stats.queue_depth, 1);
        }
        other => panic!("expected success, got {other:?}"),
    }

    let op: Option<String> = conn
        .hget(
            schema.task_meta_key(&skh, "gone_list"),
            task_meta_fields::OP,
        )
        .await
        .unwrap();
    assert_eq!(op.as_deref(), Some(TASK_OP_REMOVE));
}

#[tokio::test]
async fn phase_shift_one_production_per_window() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let schema = test_schema();
    let manifests = Arc::new(ArcSwap::from_pointee(HashMap::new()));
    let server_key = format!("secret-test-{}", Uuid::new_v4());

    // Empty server-key list → success with zero work, but still exercises the lock.
    store_manifest(&manifests, &server_key, IdListManifest::new());
    let server_keys = Arc::new(vec![server_key]);

    let interval_s = 4u64;
    let windows = 3u64;
    let producers = 4usize;
    let cancel = CancellationToken::new();

    let success = Arc::new(std::sync::atomic::AtomicU64::new(0));
    for i in 0..producers {
        let producer = Arc::new(IdListSyncProducer::new(
            schema.clone(),
            Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
            pool.clone(),
            Arc::clone(&manifests),
            Arc::clone(&server_keys),
            interval_s,
        ));
        let cancel = cancel.clone();
        let success = Arc::clone(&success);
        let offset_ms = (i as u64) * 300;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(offset_ms)).await;
            let attempt_s = producer_attempt_interval_s(interval_s);
            let mut ticker = tokio::time::interval(Duration::from_secs(attempt_s));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        if let Ok(ProducerTickOutcome::Success(_)) = producer.run_once().await {
                            success.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                }
            }
        });
    }

    tokio::time::sleep(Duration::from_secs(interval_s * windows + 2)).await;
    cancel.cancel();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let total = success.load(std::sync::atomic::Ordering::SeqCst);
    // At most one success per interval-length window (±1 for boundary jitter).
    assert!(
        total >= windows.saturating_sub(1) && total <= windows + 1,
        "expected ~{windows} successes over phase-shifted producers, got {total}"
    );
}

#[tokio::test]
async fn dead_producer_lock_expiry_lets_peer_win_next_tick() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let schema = test_schema();
    let lock_key = schema.producer_lock_key();
    let crashed = RedisWorkLock::new(pool.clone(), new_holder_id());
    assert!(crashed.try_acquire(&lock_key, 2).await.unwrap());

    let (producer, server_key) = producer_with_one_list(pool.clone(), schema.clone(), 2).await;
    let while_held = producer.run_once().await.unwrap();
    assert!(matches!(while_held, ProducerTickOutcome::Skipped));
    let mut conn = pool.get().await.unwrap();
    let depth_while_held: u64 = conn.zcard(schema.queue_key()).await.unwrap();
    assert_eq!(depth_while_held, 0);

    tokio::time::sleep(Duration::from_secs(3)).await;
    let after_expiry = producer.run_once().await.unwrap();
    assert!(matches!(after_expiry, ProducerTickOutcome::Success(_)));
    let depth: u64 = conn.zcard(schema.queue_key()).await.unwrap();
    assert_eq!(depth, 1);
    let _ = server_key;
}

#[tokio::test]
async fn double_production_zadd_nx_keeps_one_member() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let schema = test_schema();
    let (producer_a, server_key) = producer_with_one_list(pool.clone(), schema.clone(), 30).await;
    let first = producer_a.run_once().await.unwrap();
    assert!(matches!(first, ProducerTickOutcome::Success(_)));

    // Simulate a lock flush (Redis lost the lock but not the queue).
    let mut conn = pool.get().await.unwrap();
    let _: () = conn.del(schema.producer_lock_key()).await.unwrap();

    let producer_b = IdListSyncProducer::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool.clone(),
        producer_a.manifests_for_test(),
        Arc::new(vec![server_key.clone()]),
        30,
    );
    let second = producer_b.run_once().await.unwrap();
    match second {
        ProducerTickOutcome::Success(stats) => {
            assert_eq!(stats.tasks_enqueued, 0);
            assert_eq!(stats.queue_depth, 1);
        }
        other => panic!("expected success, got {other:?}"),
    }
    let depth: u64 = conn.zcard(schema.queue_key()).await.unwrap();
    assert_eq!(depth, 1);
}

#[tokio::test]
async fn remove_then_reappear_overwrites_task_meta_to_load() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let schema = test_schema();
    let manifests = Arc::new(ArcSwap::from_pointee(HashMap::new()));
    let server_key = format!("secret-test-{}", Uuid::new_v4());
    let skh = hash_company_id(&server_key);

    let mut conn = pool.get().await.unwrap();
    let _: () = conn
        .sadd(schema.all_lists_key(&skh), "maybe")
        .await
        .unwrap();
    store_manifest(&manifests, &server_key, IdListManifest::new());

    let producer = IdListSyncProducer::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool.clone(),
        Arc::clone(&manifests),
        Arc::new(vec![server_key.clone()]),
        2,
    );
    let removed = producer.run_once().await.unwrap();
    match removed {
        ProducerTickOutcome::Success(stats) => {
            assert_eq!(stats.removals_scheduled, 1);
            assert_eq!(stats.queue_depth, 1);
        }
        other => panic!("expected success, got {other:?}"),
    }
    let op: Option<String> = conn
        .hget(schema.task_meta_key(&skh, "maybe"), task_meta_fields::OP)
        .await
        .unwrap();
    assert_eq!(op.as_deref(), Some(TASK_OP_REMOVE));

    let back = one_list_manifest("maybe", 10, "https://example.com/maybe", 9, "fid-reappear");
    store_manifest(&manifests, &server_key, back);

    tokio::time::sleep(Duration::from_secs(3)).await;
    let reappeared = producer.run_once().await.unwrap();
    match reappeared {
        ProducerTickOutcome::Success(stats) => {
            assert_eq!(stats.tasks_enqueued, 0);
            assert_eq!(stats.queue_depth, 1);
        }
        other => panic!("expected success, got {other:?}"),
    }
    let op: Option<String> = conn
        .hget(schema.task_meta_key(&skh, "maybe"), task_meta_fields::OP)
        .await
        .unwrap();
    assert_eq!(op.as_deref(), Some(TASK_OP_LOAD));
    let fid: Option<String> = conn
        .hget(
            schema.task_meta_key(&skh, "maybe"),
            task_meta_fields::FILE_ID,
        )
        .await
        .unwrap();
    assert_eq!(fid.as_deref(), Some("fid-reappear"));
}

/// Every `needs_work` branch, plus mixed-class queue order, through real Redis produce/enqueue.
#[tokio::test]
async fn needs_work_cases_enqueue_class_scores_and_full_task_meta() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let schema = test_schema();
    let manifests = Arc::new(ArcSwap::from_pointee(HashMap::new()));
    let server_key = format!("secret-test-{}", Uuid::new_v4());
    let skh = hash_company_id(&server_key);
    let mut conn = pool.get().await.unwrap();

    async fn write_state(
        conn: &mut impl bb8_redis::redis::AsyncCommands,
        schema: &IdListKeySchema,
        skh: &str,
        list: &str,
        st: &ListSyncState,
        with_fwd: bool,
    ) {
        let _: () = conn
            .hset_multiple(schema.state_key(skh, list), &st.to_hash_fields())
            .await
            .unwrap();
        if with_fwd && !st.version.is_empty() {
            let _: () = conn
                .sadd(schema.fwd_key(skh, list, &st.version), "member")
                .await
                .unwrap();
        }
        let _: () = conn.sadd(schema.all_lists_key(skh), list).await.unwrap();
    }

    // Full: uninitialized hash, flush (no fwd), file_id rotation.
    write_state(
        &mut *conn,
        &schema,
        &skh,
        "empty_state",
        &ListSyncState::default(),
        false,
    )
    .await;
    write_state(
        &mut *conn,
        &schema,
        &skh,
        "flushed",
        &state("f1", 100, 50, "f1"),
        false,
    )
    .await;
    write_state(
        &mut *conn,
        &schema,
        &skh,
        "rotated",
        &state("old-fid", 100, 50, "old-fid"),
        true,
    )
    .await;
    write_state(
        &mut *conn,
        &schema,
        &skh,
        "newer_ct",
        &state("f1", 100, 50, "f1"),
        true,
    )
    .await;
    write_state(
        &mut *conn,
        &schema,
        &skh,
        "shrunk",
        &state("f1", 100, 50, "f1"),
        true,
    )
    .await;
    // Delta: same generation, size grew (bigger lag first).
    write_state(
        &mut *conn,
        &schema,
        &skh,
        "grown_more",
        &state("f1", 100, 10, "f1"),
        true,
    )
    .await;
    write_state(
        &mut *conn,
        &schema,
        &skh,
        "grown",
        &state("f1", 100, 50, "f1"),
        true,
    )
    .await;
    write_state(
        &mut *conn,
        &schema,
        &skh,
        "stable",
        &state("f1", 100, 50, "f1"),
        true,
    )
    .await;
    // Remove: known to Redis, absent from manifest.
    let _: () = conn.sadd(schema.all_lists_key(&skh), "gone").await.unwrap();

    let mut manifest = IdListManifest::new();
    for (name, size, creation_time, file_id) in [
        ("new_list", 10u64, 1u64, "f-new"), // no Redis state → Full
        ("empty_state", 50, 100, "f1"),
        ("flushed", 50, 100, "f1"),
        ("rotated", 50, 200, "new-fid"),
        ("newer_ct", 50, 200, "f1"),
        ("shrunk", 40, 100, "f1"),
        ("grown_more", 200, 100, "f1"),
        ("grown", 80, 100, "f1"),
        ("stable", 50, 100, "f1"),
    ] {
        manifest.insert(
            name.to_string(),
            IdListManifestEntry {
                name: name.to_string(),
                size,
                url: format!("https://example.com/{name}"),
                creation_time,
                file_id: file_id.to_string(),
            },
        );
    }
    store_manifest(&manifests, &server_key, manifest);

    let producer = IdListSyncProducer::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool.clone(),
        manifests,
        Arc::new(vec![server_key]),
        30,
    );
    match producer.run_once().await.unwrap() {
        ProducerTickOutcome::Success(stats) => {
            assert_eq!(stats.tasks_derived, 7); // timestamp-only changes and shrink are skipped
            assert_eq!(stats.tasks_enqueued, 7);
            assert_eq!(stats.removals_scheduled, 1);
            assert_eq!(stats.queue_depth, 7);
        }
        other => panic!("expected success, got {other:?}"),
    }

    let ranked: Vec<(String, f64)> = conn
        .zrange_withscores(schema.queue_key(), 0, -1)
        .await
        .unwrap();
    let names: Vec<String> = ranked
        .iter()
        .map(|(member, _)| IdListKeySchema::decode_task(member).unwrap().1)
        .collect();
    assert_eq!(
        names,
        vec![
            "empty_state",
            "flushed",
            "new_list",
            "rotated",
            "grown_more",
            "grown",
            "gone",
        ]
    );
    let class_boundary_delta = task_score(1, 0);
    let class_boundary_remove = task_score(2, 0);
    for name in ["empty_state", "flushed", "new_list", "rotated"] {
        let member = IdListKeySchema::encode_task(&skh, name);
        let score: Option<f64> = conn.zscore(schema.queue_key(), &member).await.unwrap();
        assert!(
            score.unwrap() < class_boundary_delta,
            "{name} should be Full class"
        );
    }
    let grown_more: Option<f64> = conn
        .zscore(
            schema.queue_key(),
            IdListKeySchema::encode_task(&skh, "grown_more"),
        )
        .await
        .unwrap();
    let grown: Option<f64> = conn
        .zscore(
            schema.queue_key(),
            IdListKeySchema::encode_task(&skh, "grown"),
        )
        .await
        .unwrap();
    let grown_more = grown_more.unwrap();
    let grown = grown.unwrap();
    assert!(grown_more >= class_boundary_delta && grown_more < class_boundary_remove);
    assert!(grown >= class_boundary_delta && grown < class_boundary_remove);
    assert!(
        grown_more < grown,
        "larger lag must sort before smaller lag"
    );
    let gone: Option<f64> = conn
        .zscore(
            schema.queue_key(),
            IdListKeySchema::encode_task(&skh, "gone"),
        )
        .await
        .unwrap();
    assert!(gone.unwrap() >= class_boundary_remove);
    let stable: Option<f64> = conn
        .zscore(
            schema.queue_key(),
            IdListKeySchema::encode_task(&skh, "stable"),
        )
        .await
        .unwrap();
    assert!(stable.is_none());
    let shrunk: Option<f64> = conn
        .zscore(
            schema.queue_key(),
            IdListKeySchema::encode_task(&skh, "shrunk"),
        )
        .await
        .unwrap();
    assert!(shrunk.is_none());

    let grown_fields: HashMap<String, String> = conn
        .hgetall(schema.task_meta_key(&skh, "grown"))
        .await
        .unwrap();
    assert_eq!(
        grown_fields.get(task_meta_fields::OP).map(String::as_str),
        Some(TASK_OP_LOAD)
    );
    assert_eq!(
        grown_fields.get(task_meta_fields::URL).map(String::as_str),
        Some("https://example.com/grown")
    );
    assert_eq!(
        grown_fields
            .get(task_meta_fields::FILE_ID)
            .map(String::as_str),
        Some("f1")
    );
    assert_eq!(
        grown_fields
            .get(task_meta_fields::CREATION_TIME)
            .map(String::as_str),
        Some("100")
    );
    assert_eq!(
        grown_fields.get(task_meta_fields::SIZE).map(String::as_str),
        Some("80")
    );
    let gone_op: Option<String> = conn
        .hget(schema.task_meta_key(&skh, "gone"), task_meta_fields::OP)
        .await
        .unwrap();
    assert_eq!(gone_op.as_deref(), Some(TASK_OP_REMOVE));
}

#[tokio::test]
async fn producer_round_trips_colon_and_percent_in_list_name() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let schema = test_schema();
    let manifests = Arc::new(ArcSwap::from_pointee(HashMap::new()));
    let server_key = format!("secret-test-{}", Uuid::new_v4());
    let skh = hash_company_id(&server_key);
    let list = "ns:list%alpha";
    store_manifest(
        &manifests,
        &server_key,
        one_list_manifest(list, 7, "https://example.com/weird", 3, "fid-weird"),
    );

    let producer = IdListSyncProducer::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool.clone(),
        manifests,
        Arc::new(vec![server_key]),
        30,
    );
    match producer.run_once().await.unwrap() {
        ProducerTickOutcome::Success(stats) => {
            assert_eq!(stats.tasks_enqueued, 1);
            assert_eq!(stats.queue_depth, 1);
        }
        other => panic!("expected success, got {other:?}"),
    }

    let mut conn = pool.get().await.unwrap();
    let member = IdListKeySchema::encode_task(&skh, list);
    let decoded = IdListKeySchema::decode_task(&member).unwrap();
    assert_eq!(decoded.1, list);
    let score: Option<f64> = conn.zscore(schema.queue_key(), &member).await.unwrap();
    assert!(score.is_some());
    let url: Option<String> = conn
        .hget(schema.task_meta_key(&skh, list), task_meta_fields::URL)
        .await
        .unwrap();
    assert_eq!(url.as_deref(), Some("https://example.com/weird"));
}

#[tokio::test]
async fn producer_enqueues_across_multiple_server_keys() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let schema = test_schema();
    let manifests = Arc::new(ArcSwap::from_pointee(HashMap::new()));
    let key_a = format!("secret-a-{}", Uuid::new_v4());
    let key_b = format!("secret-b-{}", Uuid::new_v4());
    let mut map = (**manifests.load()).clone();
    map.insert(
        key_a.clone(),
        one_list_manifest("list_a", 1, "https://example.com/a", 1, "fa"),
    );
    map.insert(
        key_b.clone(),
        one_list_manifest("list_b", 2, "https://example.com/b", 2, "fb"),
    );
    manifests.store(Arc::new(map));

    let producer = IdListSyncProducer::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool.clone(),
        manifests,
        Arc::new(vec![key_a.clone(), key_b.clone()]),
        30,
    );
    match producer.run_once().await.unwrap() {
        ProducerTickOutcome::Success(stats) => {
            assert_eq!(stats.tasks_enqueued, 2);
            assert_eq!(stats.queue_depth, 2);
            assert_eq!(stats.manifest_unavailable, 0);
        }
        other => panic!("expected success, got {other:?}"),
    }

    let mut conn = pool.get().await.unwrap();
    for (key, list) in [(&key_a, "list_a"), (&key_b, "list_b")] {
        let skh = hash_company_id(key);
        let member = IdListKeySchema::encode_task(&skh, list);
        let score: Option<f64> = conn.zscore(schema.queue_key(), &member).await.unwrap();
        assert!(score.is_some(), "missing queued task for {list}");
    }
}

#[tokio::test]
async fn producer_enqueues_newer_generation_while_loading_in_flight() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let schema = test_schema();
    let manifests = Arc::new(ArcSwap::from_pointee(HashMap::new()));
    let server_key = format!("secret-test-{}", Uuid::new_v4());
    let skh = hash_company_id(&server_key);
    let mut conn = pool.get().await.unwrap();
    let mut st = state("fid-1", 10, 50, "fid-1");
    st.loading_version = Some("fid-2".to_string());
    st.loading_size = Some(4);
    st.diff_partition = Some(1);
    let _: () = conn
        .hset_multiple(schema.state_key(&skh, "rotating"), &st.to_hash_fields())
        .await
        .unwrap();
    let _: () = conn
        .sadd(schema.fwd_key(&skh, "rotating", "fid-1"), "old")
        .await
        .unwrap();
    store_manifest(
        &manifests,
        &server_key,
        one_list_manifest("rotating", 9, "https://example.com/v3", 30, "fid-3"),
    );
    let producer = IdListSyncProducer::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool.clone(),
        manifests,
        Arc::new(vec![server_key]),
        30,
    );
    match producer.run_once().await.unwrap() {
        ProducerTickOutcome::Success(stats) => {
            assert_eq!(stats.tasks_enqueued, 1);
            assert_eq!(stats.queue_depth, 1);
        }
        other => panic!("expected success, got {other:?}"),
    }
    let meta_key = schema.task_meta_key(&skh, "rotating");
    let file_id: Option<String> = conn
        .hget(&meta_key, task_meta_fields::FILE_ID)
        .await
        .unwrap();
    let size: Option<String> = conn.hget(&meta_key, task_meta_fields::SIZE).await.unwrap();
    assert_eq!(file_id.as_deref(), Some("fid-3"));
    assert_eq!(size.as_deref(), Some("9"));
    let loading: Option<String> = conn
        .hget(
            schema.state_key(&skh, "rotating"),
            crate::datastore::idlist_membership::schema::state_fields::LOADING_VERSION,
        )
        .await
        .unwrap();
    assert_eq!(loading.as_deref(), Some("fid-2"));
}

async fn producer_with_one_list(
    pool: RedisPool,
    schema: IdListKeySchema,
    interval_s: u64,
) -> (IdListSyncProducer, String) {
    let manifests = Arc::new(ArcSwap::from_pointee(HashMap::new()));
    let server_key = format!("secret-test-{}", Uuid::new_v4());
    let manifest = one_list_manifest("list_a", 100, "https://example.com/a", 1, "fid-a");
    store_manifest(&manifests, &server_key, manifest);
    let producer = IdListSyncProducer::new(
        schema,
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool,
        manifests,
        Arc::new(vec![server_key.clone()]),
        interval_s,
    );
    (producer, server_key)
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
