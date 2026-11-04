use super::*;
use crate::datastore::idlist_membership::{
    hash_company_id, IdListMembershipConfig, IdListSyncProducer, ProducerTickOutcome,
    RedisIdListMembershipFetcher, REVERSE_INDEX_DISABLED,
};
use std::collections::{HashMap, HashSet};

#[tokio::test]
async fn forward_only_full_delta_rotation_recovery_and_removal_leave_reverse_keys_untouched() {
    let Some((pool, schema, mut worker)) = worker_or_skip().await else {
        return;
    };
    worker.config.enable_reverse_index = false;
    let skh = "company";
    let list = "customers";
    let origin = start_mock_origin(b"+A\n+B\n").await;
    let mut conn = pool.get().await.unwrap();
    // Deliberately not a SET: touching the reverse key would fail the worker.
    let _: () = conn
        .set(schema.member_key(skh, "A"), "existing-reverse-data")
        .await
        .unwrap();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );

    *origin.body.write().await = b"+A\n+B\n-A\n+C\n+C\n".to_vec();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 15).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::Delta)
    );
    let version: String = conn
        .hget(schema.state_key(skh, list), state_fields::VERSION)
        .await
        .unwrap();
    let members: HashSet<String> = conn
        .smembers(schema.fwd_key(skh, list, &version))
        .await
        .unwrap();
    assert_eq!(members, HashSet::from(["B".into(), "C".into()]));
    let cardinality: u64 = conn
        .hget(schema.state_key(skh, list), state_fields::CARDINALITY)
        .await
        .unwrap();
    assert_eq!(cardinality, 2);

    *origin.body.write().await = b"+C\n+D\n".to_vec();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F2", 200, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    let version: String = conn
        .hget(schema.state_key(skh, list), state_fields::VERSION)
        .await
        .unwrap();
    let forward = schema.fwd_key(skh, list, &version);
    let _: usize = conn.del(&forward).await.unwrap();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F2", 200, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    let members: HashSet<String> = conn.smembers(&forward).await.unwrap();
    assert_eq!(members, HashSet::from(["C".into(), "D".into()]));
    assert_eq!(conn.ttl::<_, i64>(&forward).await.unwrap(), -1);
    assert_eq!(
        conn.hget::<_, _, u8>(schema.state_key(skh, list), REVERSE_INDEX_DISABLED)
            .await
            .unwrap(),
        1
    );
    for member in ["B", "C", "D"] {
        assert!(!conn
            .exists::<_, bool>(schema.member_key(skh, member))
            .await
            .unwrap());
    }
    enqueue_remove(&pool, &schema, skh, list).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::Remove)
    );
    assert!(!conn.exists::<_, bool>(&forward).await.unwrap());
    assert!(!conn
        .exists::<_, bool>(schema.state_key(skh, list))
        .await
        .unwrap());
    assert_eq!(
        conn.get::<_, String>(schema.member_key(skh, "A"))
            .await
            .unwrap(),
        "existing-reverse-data"
    );
}

#[tokio::test]
async fn reenable_rebuilds_timestamp_changed_appends_from_committed_forward() {
    use crate::datastore::id_list_manifest::IdListManifestEntry;
    use arc_swap::ArcSwap;
    let Some((pool, schema, mut worker)) = worker_or_skip().await else {
        return;
    };
    let key = "configured-key";
    let skh = hash_company_id(key);
    let list = "customers";
    let origin = start_mock_origin(b"+A\n+B\n").await;
    enqueue_load(&pool, &schema, &skh, list, &origin.url, "F1", 100, 6).await;
    worker.run_once().await.unwrap();
    let token = worker.dict.get_token(&skh, list).await.unwrap().unwrap();
    worker.config.enable_reverse_index = false;
    *origin.body.write().await = b"+A\n+B\n-A\n+C\n".to_vec();
    enqueue_load(&pool, &schema, &skh, list, &origin.url, "F1", 200, 12).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::Delta)
    );

    let manifests = Arc::new(ArcSwap::from_pointee(HashMap::from([(
        key.to_string(),
        HashMap::from([(
            list.to_string(),
            IdListManifestEntry {
                name: list.into(),
                size: 12,
                url: origin.url.clone(),
                file_id: "F1".into(),
                creation_time: 200,
            },
        )]),
    )])));
    let producer = IdListSyncProducer::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), "producer")),
        pool.clone(),
        manifests.clone(),
        Arc::new(vec![key.into()]),
        1,
    )
    .with_reverse_index_enabled(false);
    assert!(
        matches!(producer.run_once().await.unwrap(), ProducerTickOutcome::Success(stats) if stats.tasks_derived == 0)
    );
    let mut conn = pool.get().await.unwrap();
    let state_key = schema.state_key(&skh, list);
    let before: HashMap<String, String> = conn.hgetall(&state_key).await.unwrap();
    assert_eq!(before[state_fields::VERSION], "F1@100");
    assert_eq!(before[state_fields::CREATION_TIME], "100");
    assert_eq!(before[state_fields::SIZE], "12");
    assert_eq!(before[state_fields::CARDINALITY], "2");
    let requests_before = recorded_range_starts(&origin);
    assert_eq!(requests_before, vec![0, 6]);
    // The forward-only append is caught up. Repair must use its verified set even
    // when the timestamp differs and the source is no longer reachable.
    origin.task.abort();
    tokio::task::yield_now().await;
    assert_eq!(conn.get::<_, u64>(schema.ready_key(&skh)).await.unwrap(), 1);
    let fetcher = RedisIdListMembershipFetcher::new(
        schema.clone(),
        skh.clone(),
        pool.clone(),
        IdListMembershipConfig::default(),
    )
    .await
    .unwrap();
    // Forward-only updates leave the old reverse mappings and a repair marker.
    for (member, expected) in [("A", true), ("B", true), ("C", false)] {
        assert_eq!(
            conn.sismember::<_, _, bool>(schema.member_key(&skh, member), token)
                .await
                .unwrap(),
            expected
        );
    }
    assert_eq!(
        conn.hget::<_, _, u8>(schema.state_key(&skh, list), REVERSE_INDEX_DISABLED)
            .await
            .unwrap(),
        1
    );

    let _: usize = conn.del(schema.producer_lock_key()).await.unwrap();
    let producer = IdListSyncProducer::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), "producer-enabled")),
        pool.clone(),
        manifests,
        Arc::new(vec![key.into()]),
        1,
    );
    assert!(
        matches!(producer.run_once().await.unwrap(), ProducerTickOutcome::Success(stats) if stats.tasks_derived == 1)
    );
    let queued: HashMap<String, String> = conn
        .hgetall(schema.task_meta_key(&skh, list))
        .await
        .unwrap();
    assert_eq!(queued[task_meta_fields::CREATION_TIME], "200");
    worker.config.enable_reverse_index = true;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    let after: HashMap<String, String> = conn.hgetall(&state_key).await.unwrap();
    let mut expected = before;
    expected.remove(REVERSE_INDEX_DISABLED);
    assert_eq!(after, expected);
    assert_eq!(recorded_range_starts(&origin), requests_before);
    assert!(!conn
        .sismember::<_, _, bool>(schema.member_key(&skh, "A"), token)
        .await
        .unwrap());
    for member in ["B", "C"] {
        assert!(conn
            .sismember::<_, _, bool>(schema.member_key(&skh, member), token)
            .await
            .unwrap());
    }
    assert!(!conn
        .hexists::<_, _, bool>(schema.state_key(&skh, list), REVERSE_INDEX_DISABLED)
        .await
        .unwrap());
    // The producer clears the company marker after all list repairs finish.
    assert!(conn
        .hexists::<_, _, bool>(schema.dict_key(&skh), REVERSE_INDEX_DISABLED)
        .await
        .unwrap());
    let _: usize = conn.del(schema.producer_lock_key()).await.unwrap();
    producer.run_once().await.unwrap();
    assert_eq!(
        fetcher.fetch(["A", "B", "C"]).await.unwrap().as_map(),
        &HashMap::from([
            ("A".into(), HashSet::new()),
            ("B".into(), HashSet::from([list.into()])),
            ("C".into(), HashSet::from([list.into()])),
        ])
    );
}

#[tokio::test]
async fn reenable_reconciles_committed_generation_while_manifest_stays_stale() {
    use crate::datastore::id_list_manifest::IdListManifestEntry;
    use arc_swap::ArcSwap;

    // Cover an intact set, a valid empty set, missing-set recovery, and a failed
    // reverse-write retry. Every case keeps the manifest on the older file.
    for (body, remove_forward, fail_repair) in [
        (b"+B\n+C\n".as_slice(), false, false),
        (b"".as_slice(), false, false),
        (b"+B\n+C\n".as_slice(), true, false),
        (b"+B\n+C\n".as_slice(), false, true),
    ] {
        let Some((pool, schema, mut worker)) = worker_or_skip().await else {
            return;
        };
        let key = "configured-key";
        let skh = hash_company_id(key);
        let list = "customers";
        let older = start_mock_origin(b"+A\n+B\n").await;
        let current = start_mock_origin(body).await;
        enqueue_load(&pool, &schema, &skh, list, &older.url, "F1", 100, 6).await;
        worker.run_once().await.unwrap();
        let token = worker.dict.get_token(&skh, list).await.unwrap().unwrap();
        let other_token = worker
            .dict
            .get_or_allocate_token(&skh, "unrelated")
            .await
            .unwrap();
        let mut conn = pool.get().await.unwrap();
        let _: usize = conn
            .sadd(schema.member_key(&skh, "A"), other_token)
            .await
            .unwrap();
        worker.config.enable_reverse_index = false;
        enqueue_load(
            &pool,
            &schema,
            &skh,
            list,
            &current.url,
            "F2",
            200,
            body.len() as u64,
        )
        .await;
        worker.run_once().await.unwrap();
        let state_key = schema.state_key(&skh, list);
        let before: HashMap<String, String> = conn.hgetall(&state_key).await.unwrap();
        let forward_key = schema.fwd_key(&skh, list, "F2@200");
        let requests_before = recorded_range_starts(&current).len();
        if remove_forward {
            let _: usize = conn.del(&forward_key).await.unwrap();
        } else {
            // Intact and empty committed snapshots need no reachable source URL.
            current.task.abort();
        }
        let manifests = Arc::new(ArcSwap::from_pointee(HashMap::from([(
            key.to_string(),
            HashMap::from([(
                list.to_string(),
                IdListManifestEntry {
                    name: list.into(),
                    size: 6,
                    url: older.url.clone(),
                    file_id: "F1".into(),
                    creation_time: 100,
                },
            )]),
        )])));
        let producer = IdListSyncProducer::new(
            schema.clone(),
            Arc::new(RedisWorkLock::new(pool.clone(), "producer-enabled")),
            pool.clone(),
            manifests,
            Arc::new(vec![key.into()]),
            1,
        );
        assert!(matches!(
            producer.run_once().await.unwrap(),
            ProducerTickOutcome::Success(stats) if stats.tasks_derived == 1
        ));
        let queued: HashMap<String, String> = conn
            .hgetall(schema.task_meta_key(&skh, list))
            .await
            .unwrap();
        assert_eq!(queued[task_meta_fields::FILE_ID], "F2");
        assert_eq!(queued[task_meta_fields::CREATION_TIME], "200");
        assert_eq!(queued[task_meta_fields::URL], current.url);
        assert_eq!(queued[task_meta_fields::SIZE], body.len().to_string());

        // Independently exercise worker revalidation of already-queued old metadata.
        enqueue_load(&pool, &schema, &skh, list, &older.url, "F1", 100, 6).await;
        worker.config.enable_reverse_index = true;
        if fail_repair {
            let _: () = conn
                .set(schema.member_key(&skh, "C"), "wrong-type")
                .await
                .unwrap();
            assert!(worker.run_once().await.is_err());
            assert!(conn
                .hexists::<_, _, bool>(&state_key, REVERSE_INDEX_DISABLED)
                .await
                .unwrap());
            assert!(conn
                .hexists::<_, _, bool>(&state_key, state_fields::RECONCILE_REQUIRED)
                .await
                .unwrap());
            let _: usize = conn.del(schema.member_key(&skh, "C")).await.unwrap();
            let _: usize = conn.del(schema.producer_lock_key()).await.unwrap();
            assert!(matches!(
                producer.run_once().await.unwrap(),
                ProducerTickOutcome::Success(stats) if stats.tasks_derived == 1
            ));
        }
        assert_eq!(
            worker.run_once().await.unwrap(),
            WorkerRunOutcome::Completed(WorkerAction::FullReload)
        );
        let after: HashMap<String, String> = conn.hgetall(&state_key).await.unwrap();
        let mut expected = before;
        expected.remove(REVERSE_INDEX_DISABLED);
        assert_eq!(after, expected);
        assert_eq!(recorded_range_starts(&older), vec![0]);
        assert_eq!(
            recorded_range_starts(&current).len(),
            requests_before + usize::from(remove_forward)
        );
        let members: HashSet<String> = conn.smembers(&forward_key).await.unwrap();
        let expected_members = if body.is_empty() {
            HashSet::new()
        } else {
            HashSet::from(["B".into(), "C".into()])
        };
        assert_eq!(members, expected_members);
        if !body.is_empty() {
            assert_eq!(conn.ttl::<_, i64>(&forward_key).await.unwrap(), -1);
        }
        for member in ["A", "B", "C"] {
            assert_eq!(
                conn.sismember::<_, _, bool>(schema.member_key(&skh, member), token)
                    .await
                    .unwrap(),
                expected_members.contains(member)
            );
        }
        assert!(conn
            .sismember::<_, _, bool>(schema.member_key(&skh, "A"), other_token)
            .await
            .unwrap());
        let _: usize = conn.del(schema.producer_lock_key()).await.unwrap();
        assert!(matches!(
            producer.run_once().await.unwrap(),
            ProducerTickOutcome::Success(stats) if stats.tasks_derived == 0
        ));
        assert_eq!(conn.get::<_, u64>(schema.ready_key(&skh)).await.unwrap(), 1);
        assert!(!conn
            .hexists::<_, _, bool>(schema.dict_key(&skh), REVERSE_INDEX_DISABLED)
            .await
            .unwrap());
        drop(conn);
        let fetcher =
            RedisIdListMembershipFetcher::new(schema, skh, pool, IdListMembershipConfig::default())
                .await
                .unwrap();
        let memberships = fetcher.fetch(["A", "B", "C"]).await.unwrap();
        for member in ["A", "B", "C"] {
            assert_eq!(
                memberships.get(member).unwrap().contains(list),
                expected_members.contains(member)
            );
        }
    }
}

#[tokio::test]
async fn forward_only_interrupted_downloads_resume_from_complete_line_checkpoints() {
    let Some((pool, schema, mut worker)) = worker_or_skip().await else {
        return;
    };
    worker.config.enable_reverse_index = false;
    let skh = "company";
    let list = "customers";
    let origin = start_mock_origin(b"+A\n+B").await;
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 5).await;
    assert!(worker.run_once().await.is_err());
    let mut conn = pool.get().await.unwrap();
    assert_eq!(
        conn.hget::<_, _, u64>(schema.state_key(skh, list), state_fields::LOADING_SIZE)
            .await
            .unwrap(),
        3
    );
    let version: String = conn
        .hget(schema.state_key(skh, list), state_fields::LOADING_VERSION)
        .await
        .unwrap();
    assert!(
        conn.ttl::<_, i64>(schema.fwd_key(skh, list, &version))
            .await
            .unwrap()
            > 0
    );
    *origin.body.write().await = b"+A\n+B\n".to_vec();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 6).await;
    worker.run_once().await.unwrap();
    assert_eq!(recorded_range_starts(&origin), vec![0, 3]);
    *origin.body.write().await = b"+A\n+B\n-A\n+C".to_vec();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 11).await;
    assert!(worker.run_once().await.is_err());
    assert_eq!(
        conn.hget::<_, _, u64>(schema.state_key(skh, list), state_fields::SIZE)
            .await
            .unwrap(),
        9
    );
    *origin.body.write().await = b"+A\n+B\n-A\n+C\n".to_vec();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 12).await;
    worker.run_once().await.unwrap();
    assert_eq!(recorded_range_starts(&origin), vec![0, 3, 6, 9]);
    let members: HashSet<String> = conn
        .smembers(schema.fwd_key(skh, list, &version))
        .await
        .unwrap();
    assert_eq!(members, HashSet::from(["B".into(), "C".into()]));
    for member in ["A", "B", "C"] {
        assert!(!conn
            .exists::<_, bool>(schema.member_key(skh, member))
            .await
            .unwrap());
    }
}
