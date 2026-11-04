pub mod company_ids;
pub mod namespaces;
pub mod redis_membership;
pub mod redis_syncer;
pub mod schema;
pub mod work_lock;
pub mod worker;

pub const ENABLE_REVERSE_INDEX_ENV: &str = "IDLIST_ENABLE_REVERSE_INDEX";
// Stored in the dictionary and per-list state before forward-only mutations.
pub(crate) const REVERSE_INDEX_DISABLED: &str = "__reverse_index_disabled";

pub fn reverse_index_enabled_from_env() -> Result<bool, String> {
    match std::env::var(ENABLE_REVERSE_INDEX_ENV) {
        Ok(value) => parse_reverse_index_enabled(Some(&value)),
        Err(std::env::VarError::NotPresent) => Ok(true),
        Err(error) => Err(format!(
            "failed reading {ENABLE_REVERSE_INDEX_ENV}: {error}"
        )),
    }
}

fn parse_reverse_index_enabled(value: Option<&str>) -> Result<bool, String> {
    match value {
        None | Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(_) => Err(format!("{ENABLE_REVERSE_INDEX_ENV} must be true or false")),
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn reverse_index_option_defaults_on_and_rejects_invalid_values() {
        assert_eq!(parse_reverse_index_enabled(None), Ok(true));
        assert_eq!(parse_reverse_index_enabled(Some("true")), Ok(true));
        assert_eq!(parse_reverse_index_enabled(Some("false")), Ok(false));
        for value in ["", "0", "1", "False", "invalid"] {
            assert!(parse_reverse_index_enabled(Some(value)).is_err());
        }
        assert!(IdListWorkerConfig::default().enable_reverse_index);
        assert!(IdListMembershipConfig::default().enable_reverse_index);
    }
}

#[cfg(test)]
mod multi_instance_tests;

pub use redis_membership::{
    create_idlist_read_pool_from_env, IdListMembershipConfig, IdListMembershipError,
    RedisIdListMembershipFetcher, ResolvedMemberships, DEFAULT_IDLIST_DICT_REFRESH_S,
    DEFAULT_IDLIST_FETCH_TIMEOUT_MS, DEFAULT_IDLIST_REDIS_POOL_SIZE,
};
pub use redis_syncer::{
    idlist_sync_keys_from_env, needs_work, order_and_dedupe_tasks, producer_attempt_interval_s,
    register_server_keys_for_manifest_polling, sync_interval_from_env, DerivedTask,
    IdListManifestObserver, IdListSyncProducer, ProducerTickOutcome, ProducerTickStats, SyncAction,
    DEFAULT_SYNC_INTERVAL_S, IDLIST_SYNC_KEYS_ENV,
};
pub use schema::{
    hash_company_id, hash_server_key, DictClient, DictSnapshot, IdListKeySchema, IdListMetadata,
    ListSyncState,
};
pub use work_lock::RedisWorkLock;
pub use worker::{
    IdListSyncWorker, IdListWorkerConfig, TaskMetadata, TaskOp, WorkerAction, WorkerError,
    WorkerRunOutcome,
};

/// Creates a process-scoped holder ID: `"{millis}-{uuid}"`.
pub fn new_holder_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("{millis}-{}", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::datastore::caching::redis_pool::{create_redis_pool_from_env, RedisPool};
    use crate::datastore::idlist_membership::schema::{
        state_fields, task_meta_fields, TASK_OP_LOAD,
    };
    use bb8_redis::redis::AsyncCommands;
    use std::collections::HashMap;
    use uuid::Uuid;

    async fn pool_or_skip() -> Option<RedisPool> {
        create_redis_pool_from_env(5).await.ok()
    }

    async fn cluster_keyslot(pool: &RedisPool, key: &str) -> i32 {
        let mut conn = pool.get().await.unwrap();
        bb8_redis::redis::cmd("CLUSTER")
            .arg("KEYSLOT")
            .arg(key)
            .query_async(&mut *conn)
            .await
            .unwrap_or(-1)
    }

    #[tokio::test]
    async fn dict_concurrent_allocate_exactly_one_token() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let skh = format!("skh_{}", Uuid::new_v4());
        let list_name = format!("concurrent_list_{}", Uuid::new_v4());

        let n = 10;
        let mut handles = Vec::new();
        for _ in 0..n {
            let dict = DictClient::new(pool.clone(), schema.clone());
            let name = list_name.clone();
            let skh = skh.clone();
            handles.push(tokio::spawn(async move {
                dict.get_or_allocate_token(&skh, &name).await.unwrap()
            }));
        }
        let tokens: Vec<u64> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert!(tokens.iter().all(|&t| t == tokens[0]));
        assert!(tokens[0] > 0);
    }

    #[tokio::test]
    async fn dict_name_token_round_trip_and_snapshot() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let prefix = format!("test-{}", Uuid::new_v4());
        let schema = IdListKeySchema::new(prefix);
        let skh = format!("skh_{}", Uuid::new_v4());
        let dict = DictClient::new(pool.clone(), schema);

        let token_a = dict
            .get_or_allocate_token(&skh, "list_alpha")
            .await
            .unwrap();
        let token_b = dict.get_or_allocate_token(&skh, "list_beta").await.unwrap();
        assert_ne!(token_a, token_b);

        // Same name → same token
        let token_a2 = dict
            .get_or_allocate_token(&skh, "list_alpha")
            .await
            .unwrap();
        assert_eq!(token_a, token_a2);

        let snap = dict.snapshot(&skh).await.unwrap();
        assert_eq!(snap.name_to_token.get("list_alpha"), Some(&token_a));
        assert_eq!(snap.name_to_token.get("list_beta"), Some(&token_b));
        assert_eq!(
            snap.token_to_name.get(&token_a),
            Some(&"list_alpha".to_string())
        );
        assert_eq!(
            snap.token_to_name.get(&token_b),
            Some(&"list_beta".to_string())
        );
    }

    #[tokio::test]
    async fn dict_snapshot_empty_dict_is_empty() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let dict = DictClient::new(pool, schema);
        let snap = dict
            .snapshot(&format!("skh_{}", Uuid::new_v4()))
            .await
            .unwrap();
        assert!(snap.name_to_token.is_empty());
        assert!(snap.token_to_name.is_empty());
    }

    #[tokio::test]
    async fn dict_distinct_names_get_monotonic_tokens() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let skh = format!("skh_{}", Uuid::new_v4());
        let dict = DictClient::new(pool, schema);

        let a = dict.get_or_allocate_token(&skh, "first").await.unwrap();
        let b = dict.get_or_allocate_token(&skh, "second").await.unwrap();
        let c = dict.get_or_allocate_token(&skh, "third").await.unwrap();
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(c, 3);
    }

    #[tokio::test]
    async fn dict_snapshot_omits_counter_field() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let skh = format!("skh_{}", Uuid::new_v4());
        let dict = DictClient::new(pool.clone(), schema.clone());
        dict.get_or_allocate_token(&skh, "only").await.unwrap();

        let mut conn = pool.get().await.unwrap();
        let raw: HashMap<String, String> = conn.hgetall(schema.dict_key(&skh)).await.unwrap();
        assert!(
            raw.contains_key("__counter"),
            "Lua must persist the counter"
        );

        let snap = dict.snapshot(&skh).await.unwrap();
        assert!(!snap.name_to_token.contains_key("__counter"));
        assert_eq!(snap.name_to_token.len(), 1);
        assert_eq!(snap.token_to_name.len(), 1);
    }

    #[tokio::test]
    async fn dict_special_list_names_round_trip() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let skh = format!("skh_{}", Uuid::new_v4());
        let dict = DictClient::new(pool, schema);
        for name in ["a::b", "percent%here", "unicode-名前", ""] {
            let token = dict.get_or_allocate_token(&skh, name).await.unwrap();
            let snap = dict.snapshot(&skh).await.unwrap();
            assert_eq!(snap.name_to_token.get(name), Some(&token), "{name}");
            assert_eq!(
                snap.token_to_name.get(&token).map(String::as_str),
                Some(name)
            );
        }
    }

    #[tokio::test]
    async fn dict_tokens_are_isolated_per_skh() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let dict = DictClient::new(pool, schema);
        let skh_a = format!("skh_{}", Uuid::new_v4());
        let skh_b = format!("skh_{}", Uuid::new_v4());
        let a = dict.get_or_allocate_token(&skh_a, "shared").await.unwrap();
        let b = dict.get_or_allocate_token(&skh_b, "shared").await.unwrap();
        assert_eq!(a, 1);
        assert_eq!(b, 1);
    }

    #[tokio::test]
    async fn cluster_hash_tag_same_slot() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new("");
        let skh = "cluster_test_skh";
        let list = "my_list";
        let v1 = schema.fwd_key(skh, list, "version_a");
        let v2 = schema.fwd_key(skh, list, "version_b");
        let slot1 = cluster_keyslot(&pool, &v1).await;
        let slot2 = cluster_keyslot(&pool, &v2).await;

        if slot1 >= 0 && slot2 >= 0 {
            assert_eq!(
                slot1, slot2,
                "fwd keys for different versions must share a cluster slot"
            );
        } else {
            eprintln!(
                "CLUSTER KEYSLOT unavailable (standalone mode); tag equality covered by unit tests"
            );
        }
    }

    #[tokio::test]
    async fn cluster_hash_tag_same_slot_with_escaped_list_name() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new("");
        let list = "weird::name/with:chars";
        let v1 = schema.fwd_key("skh", list, "v1");
        let v2 = schema.fwd_key("skh", list, "v2");
        let slot1 = cluster_keyslot(&pool, &v1).await;
        let slot2 = cluster_keyslot(&pool, &v2).await;
        if slot1 >= 0 && slot2 >= 0 {
            assert_eq!(slot1, slot2);
        }
    }

    #[tokio::test]
    async fn state_hash_round_trips_through_redis() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let key = schema.state_key("skh", "list::special");
        let state = ListSyncState {
            version: "v1".to_string(),
            file_id: "f1".to_string(),
            url: "https://example.com".to_string(),
            creation_time: 1772477815000,
            size: 99990,
            loading_version: Some("v2".to_string()),
            loading_size: Some(100),
            diff_partition: Some(42),
            cardinality: Some(20),
            loading_cardinality: Some(2),
            reconcile_required: true,
            reverse_index_disabled: true,
        };

        let mut conn = pool.get().await.unwrap();
        let _: () = conn
            .hset_multiple(&key, &state.to_hash_fields())
            .await
            .unwrap();
        let raw: HashMap<String, String> = conn.hgetall(&key).await.unwrap();
        assert_eq!(ListSyncState::from_hash_fields(&raw), state);
        assert_eq!(
            raw.get(state_fields::CREATION_TIME).unwrap(),
            "1772477815000"
        );
    }

    #[tokio::test]
    async fn state_hash_omits_optional_fields_on_redis() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let key = schema.state_key("skh", "list");
        let state = ListSyncState {
            version: "v1".to_string(),
            file_id: "f1".to_string(),
            url: "u".to_string(),
            creation_time: 1,
            size: 2,
            loading_version: None,
            loading_size: None,
            diff_partition: None,
            ..ListSyncState::default()
        };

        let mut conn = pool.get().await.unwrap();
        let _: () = conn
            .hset_multiple(&key, &state.to_hash_fields())
            .await
            .unwrap();
        let raw: HashMap<String, String> = conn.hgetall(&key).await.unwrap();
        assert!(!raw.contains_key(state_fields::LOADING_VERSION));
        assert!(!raw.contains_key(state_fields::LOADING_SIZE));
        assert!(!raw.contains_key(state_fields::DIFF_PARTITION));
        assert_eq!(ListSyncState::from_hash_fields(&raw), state);
    }

    #[tokio::test]
    async fn task_meta_hash_round_trips_through_redis() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let key = schema.task_meta_key("skh", "list::x");
        let fields = [
            (task_meta_fields::OP, TASK_OP_LOAD),
            (task_meta_fields::URL, "https://example.com"),
            (task_meta_fields::FILE_ID, "fid"),
            (task_meta_fields::CREATION_TIME, "1772477815000"),
            (task_meta_fields::SIZE, "12"),
        ];

        let mut conn = pool.get().await.unwrap();
        let _: () = conn.hset_multiple(&key, &fields).await.unwrap();
        let raw: HashMap<String, String> = conn.hgetall(&key).await.unwrap();
        assert_eq!(raw.get(task_meta_fields::OP).unwrap(), TASK_OP_LOAD);
        assert_eq!(raw.get(task_meta_fields::FILE_ID).unwrap(), "fid");
        assert_eq!(
            raw.get(task_meta_fields::CREATION_TIME).unwrap(),
            "1772477815000"
        );
    }

    #[tokio::test]
    async fn queue_zadd_nx_dedups_encoded_task() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let queue = schema.queue_key();
        let member = IdListKeySchema::encode_task("skh", "list::with:colons");

        let mut conn = pool.get().await.unwrap();
        let first: i32 = bb8_redis::redis::cmd("ZADD")
            .arg(&queue)
            .arg("NX")
            .arg(1)
            .arg(&member)
            .query_async(&mut *conn)
            .await
            .unwrap();
        let second: i32 = bb8_redis::redis::cmd("ZADD")
            .arg(&queue)
            .arg("NX")
            .arg(99)
            .arg(&member)
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 0);

        let members: Vec<String> = conn.zrange(&queue, 0, -1).await.unwrap();
        assert_eq!(members, vec![member.clone()]);
        let (skh, list) = IdListKeySchema::decode_task(&members[0]).unwrap();
        assert_eq!(skh, "skh");
        assert_eq!(list, "list::with:colons");

        let score: f64 = conn.zscore(&queue, &member).await.unwrap();
        assert_eq!(score, 1.0);
    }

    #[tokio::test]
    async fn member_and_all_lists_sets_round_trip() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let skh = "skh";
        let member_key = schema.member_key(skh, "lookup::id");
        let all_key = schema.all_lists_key(skh);

        let mut conn = pool.get().await.unwrap();
        let _: () = conn.sadd(&member_key, &[1u64, 2u64]).await.unwrap();
        let _: () = conn.sadd(&all_key, "list::a").await.unwrap();

        let tokens: Vec<u64> = conn.smembers(&member_key).await.unwrap();
        let mut tokens = tokens;
        tokens.sort();
        assert_eq!(tokens, vec![1, 2]);
        let lists: Vec<String> = conn.smembers(&all_key).await.unwrap();
        assert_eq!(lists, vec!["list::a".to_string()]);
    }

    #[tokio::test]
    async fn schema_lock_keys_work_with_work_lock() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let producer = RedisWorkLock::new(pool.clone(), new_holder_id());
        let claim = RedisWorkLock::new(pool, new_holder_id());
        assert!(producer
            .try_acquire(&schema.producer_lock_key(), 30)
            .await
            .unwrap());
        assert!(claim
            .try_acquire(&schema.claim_key("skh", "list::x"), 30)
            .await
            .unwrap());
        producer.release(&schema.producer_lock_key()).await.unwrap();
        claim
            .release(&schema.claim_key("skh", "list::x"))
            .await
            .unwrap();
    }
}
