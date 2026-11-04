use super::*;
use crate::datastore::id_list_manifest::IdListManifestEntry;

fn list(name: &str, file: &str, time: u64, size: u64) -> IdListManifestEntry {
    IdListManifestEntry {
        name: name.into(),
        file_id: file.into(),
        creation_time: time,
        size,
        url: format!("https://example.com/{file}"),
    }
}

fn fixture() -> IdListNamespaces {
    let ns = IdListNamespaces::new(vec!["key-b".into(), "key-a".into()]);
    for key in ["key-a", "key-b"] {
        ns.company_ids.set_for_test(key, "company");
    }
    ns.manifests.store(Arc::new(HashMap::from([
        (
            "key-a".into(),
            HashMap::from([
                ("shared".into(), list("shared", "old", 10, 50)),
                ("only-a".into(), list("only-a", "a", 1, 10)),
            ]),
        ),
        (
            "key-b".into(),
            HashMap::from([
                ("shared".into(), list("shared", "new", 20, 20)),
                ("only-b".into(), list("only-b", "b", 1, 10)),
            ]),
        ),
    ])));
    ns
}

#[test]
fn unions_company_lists_but_preserves_key_visibility() {
    let ns = fixture();
    let (companies, unavailable) = ns.company_manifests().unwrap();
    assert_eq!(unavailable, 0);
    assert_eq!(companies.len(), 1);
    let company = &companies[&hash_company_id("company")];
    assert!(!company.incomplete);
    assert_eq!(company.lists.len(), 3);
    assert_eq!(company.lists["shared"].file_id, "new");
    assert!(!ns.visible_lists("key-a").unwrap().contains("only-b"));
    assert!(!ns.visible_lists("key-b").unwrap().contains("only-a"));
}

#[test]
fn missing_manifest_or_unresolved_company_prevents_removal_and_readiness() {
    let ns = fixture();
    ns.manifests.rcu(|current| {
        let mut next = (**current).clone();
        next.remove("key-b");
        next
    });
    let (companies, unavailable) = ns.company_manifests().unwrap();
    assert_eq!(unavailable, 1);
    assert!(companies[&hash_company_id("company")].incomplete);
    assert!(ns.visible_lists("key-b").is_none());

    let ns = IdListNamespaces::new(vec!["known".into(), "unknown".into()]);
    ns.company_ids.set_for_test("known", "company");
    ns.manifests
        .store(Arc::new(HashMap::from([("known".into(), HashMap::new())])));
    let (companies, unavailable) = ns.company_manifests().unwrap();
    assert_eq!(unavailable, 1);
    assert!(companies[&hash_company_id("company")].incomplete);
}

#[test]
fn empty_manifest_is_authoritative_and_companies_stay_separate() {
    let ns = fixture();
    ns.company_ids.set_for_test("key-b", "other-company");
    ns.manifests.rcu(|current| {
        let mut next = (**current).clone();
        next.insert("key-b".into(), HashMap::new());
        next
    });
    let (companies, unavailable) = ns.company_manifests().unwrap();
    assert_eq!(unavailable, 0);
    assert_eq!(companies.len(), 2);
    assert!(companies[&hash_company_id("other-company")]
        .lists
        .is_empty());
    assert!(!companies[&hash_company_id("other-company")].incomplete);
}

#[test]
fn same_file_uses_largest_observed_offset() {
    let ns = fixture();
    ns.manifests.rcu(|current| {
        let mut next = (**current).clone();
        next.get_mut("key-b")
            .unwrap()
            .insert("shared".into(), list("shared", "old", 11, 40));
        next
    });
    let (companies, _) = ns.company_manifests().unwrap();
    assert_eq!(
        companies[&hash_company_id("company")].lists["shared"].size,
        50
    );
}

#[test]
fn reports_all_collisions_even_without_manifests() {
    let ns = IdListNamespaces::new(vec![
        "key-a".into(),
        "key-b".into(),
        "key-c".into(),
        "key-d".into(),
        "same-company-key".into(),
        "unresolved-key".into(),
    ]);
    for (key, company) in [
        ("key-a", "test-company-86"),
        ("key-b", "test-company-5385"),
        ("key-c", "test-company-1010"),
        ("key-d", "test-company-8444"),
        ("same-company-key", "test-company-86"),
    ] {
        ns.company_ids.set_for_test(key, company);
    }
    assert_eq!(
        ns.company_manifests().unwrap_err(),
        vec![
            NamespaceCollision {
                namespace: "ezlk".into(),
                company_ids: vec!["test-company-5385".into(), "test-company-86".into()],
            },
            NamespaceCollision {
                namespace: "iww4".into(),
                company_ids: vec!["test-company-1010".into(), "test-company-8444".into()],
            },
        ]
    );
}

#[tokio::test]
async fn collision_aborts_entire_producer_tick_before_redis_changes() {
    use crate::datastore::caching::redis_pool::create_redis_pool_from_env;
    use crate::datastore::idlist_membership::{IdListKeySchema, IdListSyncProducer, RedisWorkLock};
    use bb8_redis::redis::AsyncCommands;

    let Ok(pool) = create_redis_pool_from_env(5).await else {
        eprintln!("Skipping collision integration test: test Redis unavailable");
        return;
    };
    let ns = Arc::new(IdListNamespaces::new(vec![
        "key-a".into(),
        "key-b".into(),
        "healthy-load".into(),
        "healthy-empty".into(),
    ]));
    for (key, company) in [
        ("key-a", "test-company-86"),
        ("key-b", "test-company-5385"),
        ("healthy-load", "healthy-load-company"),
        ("healthy-empty", "healthy-empty-company"),
    ] {
        ns.company_ids.set_for_test(key, company);
    }
    ns.manifests.store(Arc::new(HashMap::from([
        (
            "key-a".into(),
            HashMap::from([("first".into(), list("first", "a", 1, 10))]),
        ),
        (
            "key-b".into(),
            HashMap::from([("second".into(), list("second", "b", 1, 10))]),
        ),
        (
            "healthy-load".into(),
            HashMap::from([("new".into(), list("new", "new", 2, 20))]),
        ),
        ("healthy-empty".into(), HashMap::new()),
    ])));
    let prefix = format!("collision-tick-{}", uuid::Uuid::new_v4());
    let schema = IdListKeySchema::new(&prefix);
    let healthy = hash_company_id("healthy-load-company");
    let mut conn = pool.get().await.unwrap();
    let _: () = conn
        .sadd(schema.all_lists_key(&healthy), "gone")
        .await
        .unwrap();
    let _: () = conn
        .zadd(
            schema.queue_key(),
            IdListKeySchema::encode_task("ezlk", "already-queued"),
            123,
        )
        .await
        .unwrap();
    let _: () = conn
        .hset(schema.task_meta_key("ezlk", "already-queued"), "op", "load")
        .await
        .unwrap();
    for namespace in ["ezlk".to_string(), hash_company_id("healthy-empty-company")] {
        let _: () = conn.set(schema.ready_key(&namespace), 99).await.unwrap();
    }
    let mut before = BTreeMap::new();
    let keys: Vec<String> = conn.keys(format!("{prefix}:*")).await.unwrap();
    assert_eq!(keys.len(), 5);
    for key in keys {
        let bytes: Vec<u8> = bb8_redis::redis::cmd("DUMP")
            .arg(&key)
            .query_async(&mut *conn)
            .await
            .unwrap();
        before.insert(key, bytes);
    }
    let producer = IdListSyncProducer::with_namespaces(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), "collision-test")),
        pool.clone(),
        ns,
        30,
    );
    let error = producer.run_once().await.unwrap_err();
    assert!(error.to_string().contains("namespace collision"));
    let mut after = BTreeMap::new();
    let keys: Vec<String> = conn.keys(format!("{prefix}:*")).await.unwrap();
    assert!(keys.contains(&schema.producer_lock_key()));
    for key in keys {
        if key == schema.producer_lock_key() {
            continue;
        }
        let bytes: Vec<u8> = bb8_redis::redis::cmd("DUMP")
            .arg(&key)
            .query_async(&mut *conn)
            .await
            .unwrap();
        after.insert(key, bytes);
    }
    assert_eq!(
        before, after,
        "failed generation must not mutate existing data"
    );
}

#[tokio::test]
async fn producer_removes_only_lists_absent_from_complete_company_union() {
    use crate::datastore::caching::redis_pool::create_redis_pool_from_env;
    use crate::datastore::idlist_membership::{IdListKeySchema, IdListSyncProducer, RedisWorkLock};
    use bb8_redis::redis::AsyncCommands;
    let Ok(pool) = create_redis_pool_from_env(5).await else {
        return;
    };
    for missing in [false, true] {
        let ns = fixture();
        if missing {
            ns.manifests.rcu(|current| {
                let mut next = (**current).clone();
                next.remove("key-b");
                next
            });
        }
        let schema = IdListKeySchema::new(format!("company-union-{}", uuid::Uuid::new_v4()));
        let hash = hash_company_id("company");
        let mut conn = pool.get().await.unwrap();
        let _: usize = conn
            .sadd(schema.all_lists_key(&hash), &["shared", "only-b", "gone"])
            .await
            .unwrap();
        let producer = IdListSyncProducer::with_namespaces(
            schema.clone(),
            Arc::new(RedisWorkLock::new(pool.clone(), "test")),
            pool.clone(),
            Arc::new(ns),
            30,
        );
        producer.run_once().await.unwrap();
        let shared_op: String = conn
            .hget(schema.task_meta_key(&hash, "shared"), "op")
            .await
            .unwrap();
        assert_eq!(shared_op, "load");
        let gone_op: Option<String> = conn
            .hget(schema.task_meta_key(&hash, "gone"), "op")
            .await
            .unwrap();
        assert_eq!(
            gone_op.as_deref(),
            if missing { None } else { Some("remove") }
        );
        let only_b_op: Option<String> = conn
            .hget(schema.task_meta_key(&hash, "only-b"), "op")
            .await
            .unwrap();
        assert_eq!(
            only_b_op.as_deref(),
            if missing { None } else { Some("load") }
        );
    }
}
