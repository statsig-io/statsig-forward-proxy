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

#[tokio::test]
async fn missing_manifest_after_lock_enqueues_nothing() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let schema = test_schema();
    let server_key = format!("secret-test-{}", Uuid::new_v4());
    let manifests = Arc::new(ArcSwap::from_pointee(HashMap::new()));
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
            assert_eq!(stats.manifest_unavailable, 1);
            assert_eq!(stats.tasks_enqueued, 0);
            assert_eq!(stats.tasks_derived, 0);
            assert_eq!(stats.queue_depth, 0);
        }
        other => panic!("expected success, got {other:?}"),
    }
}

#[tokio::test]
async fn failure_before_acquire_when_redis_unreachable() {
    let config = fake_pool_config(1); // nothing listens on 127.0.0.1:1
    let pool = match crate::datastore::caching::redis_pool::create_redis_pool(&config, 1).await {
        Ok(pool) => pool,
        Err(_) => {
            // Pool build itself failed — still a before-acquire failure.
            return;
        }
    };
    let schema = test_schema();
    let producer = IdListSyncProducer::new(
        schema,
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool,
        Arc::new(ArcSwap::from_pointee(HashMap::new())),
        Arc::new(vec!["secret-x".to_string()]),
        30,
    );
    let err = producer.run_once().await.expect_err("unreachable redis");
    assert!(!format!("{err}").is_empty());
}

#[tokio::test]
async fn lock_nx_miss_is_skipped_not_an_error() {
    let redis = crate::datastore::caching::fake_redis::FakeRedis::start_with(
        crate::datastore::caching::fake_redis::FakeRedisMode::SetNxMiss,
    )
    .await;
    let pool = pool_to_fake(redis.port).await;
    let schema = test_schema();
    let producer = IdListSyncProducer::new(
        schema,
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool,
        Arc::new(ArcSwap::from_pointee(HashMap::new())),
        Arc::new(vec!["secret-x".to_string()]),
        30,
    );
    let outcome = producer.run_once().await.unwrap();
    assert!(matches!(outcome, ProducerTickOutcome::Skipped));
}

#[tokio::test]
async fn failure_before_produce_when_set_errors() {
    let redis = crate::datastore::caching::fake_redis::FakeRedis::start_with(
        crate::datastore::caching::fake_redis::FakeRedisMode::ErrorOn("SET".to_string()),
    )
    .await;
    let pool = pool_to_fake(redis.port).await;
    let schema = test_schema();
    let producer = IdListSyncProducer::new(
        schema,
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool,
        Arc::new(ArcSwap::from_pointee(HashMap::new())),
        Arc::new(vec!["secret-x".to_string()]),
        30,
    );
    assert!(producer.run_once().await.is_err());
}

#[tokio::test]
async fn failure_after_acquire_when_hgetall_errors() {
    let redis = crate::datastore::caching::fake_redis::FakeRedis::start_with(
        crate::datastore::caching::fake_redis::FakeRedisMode::ErrorOn("HGETALL".to_string()),
    )
    .await;
    let pool = pool_to_fake(redis.port).await;
    let schema = test_schema();
    let server_key = "secret-after-lock".to_string();
    let mut manifest = IdListManifest::new();
    manifest.insert(
        "list_a".to_string(),
        IdListManifestEntry {
            name: "list_a".to_string(),
            size: 1,
            url: "https://example.com/a".to_string(),
            creation_time: 1,
            file_id: "fid".to_string(),
        },
    );
    let mut map = HashMap::new();
    map.insert(server_key.clone(), manifest);
    let producer = IdListSyncProducer::new(
        schema,
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool,
        Arc::new(ArcSwap::from_pointee(map)),
        Arc::new(vec![server_key]),
        30,
    );
    assert!(producer.run_once().await.is_err());
    assert!(redis.received_command("SET"));
    assert!(redis.received_command("HGETALL"));
}

#[tokio::test]
async fn failure_after_acquire_holds_lock_until_ttl() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let schema = test_schema();
    let lock_key = schema.producer_lock_key();
    // Winner acquires then "crashes" before enqueue (same as a Failed tick leaving TTL).
    let winner = RedisWorkLock::new(pool.clone(), new_holder_id());
    assert!(winner.try_acquire(&lock_key, 2).await.unwrap());

    let peer = IdListSyncProducer::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool.clone(),
        Arc::new(ArcSwap::from_pointee(HashMap::new())),
        Arc::new(vec!["secret-x".to_string()]),
        2,
    );
    assert!(matches!(
        peer.run_once().await.unwrap(),
        ProducerTickOutcome::Skipped
    ));
    tokio::time::sleep(Duration::from_secs(3)).await;
    match peer.run_once().await.unwrap() {
        ProducerTickOutcome::Success(stats) => {
            assert_eq!(stats.manifest_unavailable, 1);
            assert_eq!(stats.tasks_enqueued, 0);
        }
        other => panic!("expected success after TTL, got {other:?}"),
    }
}

#[tokio::test]
async fn spawn_loop_continues_after_run_once_error() {
    let redis = crate::datastore::caching::fake_redis::FakeRedis::start_with(
        crate::datastore::caching::fake_redis::FakeRedisMode::ErrorOn("SET".to_string()),
    )
    .await;
    let pool = pool_to_fake(redis.port).await;
    let producer = Arc::new(IdListSyncProducer::new(
        test_schema(),
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool,
        Arc::new(ArcSwap::from_pointee(HashMap::new())),
        Arc::new(vec!["secret-x".to_string()]),
        30,
    ));
    let cancel = CancellationToken::new();
    producer.clone().spawn_loop(cancel.clone());
    tokio::time::sleep(Duration::from_millis(300)).await;
    cancel.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let set_count = redis
        .commands()
        .iter()
        .filter(|c| c.first().is_some_and(|n| n.eq_ignore_ascii_case("SET")))
        .count();
    assert!(
        set_count >= 1,
        "loop should attempt at least one tick, got {set_count} SET commands"
    );
}
fn fake_pool_config(port: u16) -> crate::datastore::caching::redis_pool::RedisEnvConfig {
    crate::datastore::caching::redis_pool::RedisEnvConfig {
        redis_enterprise_user: None,
        redis_enterprise_password: None,
        redis_enterprise_host: "127.0.0.1".to_string(),
        redis_enterprise_port: port.to_string(),
        redis_connection_pool_max_size: Some(2),
        redis_connection_pool_min_size: Some(0),
        redis_memorystore: None,
        redis_tls: Some(false),
        redis_key_prefix: None,
        redis_auth_mode: None,
        redis_oauth_tenant_id: None,
        redis_oauth_client_id: None,
        redis_oauth_client_secret: None,
        redis_oauth_scope: None,
        redis_oauth_username: None,
        redis_oauth_refresh_margin_in_s: None,
        redis_oauth_authority_host: None,
        redis_oauth_allow_plaintext: None,
    }
}

async fn pool_to_fake(port: u16) -> RedisPool {
    crate::datastore::caching::redis_pool::create_redis_pool(&fake_pool_config(port), 2)
        .await
        .expect("pool to fake redis")
}
