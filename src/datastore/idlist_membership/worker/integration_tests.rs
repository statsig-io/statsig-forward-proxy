use std::sync::{Arc, Mutex};

use bb8_redis::redis::AsyncCommands;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::datastore::caching::redis_pool::{create_redis_pool_from_env, RedisPool};

use super::super::schema::{
    state_fields, task_meta_fields, DictClient, IdListKeySchema, TASK_OP_LOAD, TASK_OP_REMOVE,
};
use super::super::RedisWorkLock;
use super::{IdListSyncWorker, IdListWorkerConfig, WorkerAction, WorkerRunOutcome};

#[path = "oom_tests.rs"]
mod oom_tests;

#[path = "forward_only_tests.rs"]
mod forward_only_tests;

struct MockOrigin {
    url: String,
    body: Arc<RwLock<Vec<u8>>>,
    range_starts: Arc<Mutex<Vec<usize>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockOrigin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_mock_origin(initial_body: &[u8]) -> MockOrigin {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let body = Arc::new(RwLock::new(initial_body.to_vec()));
    let range_starts = Arc::new(Mutex::new(Vec::new()));
    let server_body = Arc::clone(&body);
    let server_ranges = Arc::clone(&range_starts);
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let server_body = Arc::clone(&server_body);
            let server_ranges = Arc::clone(&server_ranges);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request);
                assert_eq!(
                    request
                        .lines()
                        .filter(|line| line.to_ascii_lowercase().starts_with("range:"))
                        .count(),
                    1
                );
                let range_start = request
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("range: bytes=")
                            .and_then(|value| value.split('-').next())
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                server_ranges.lock().unwrap().push(range_start);
                let body = server_body.read().await;
                if range_start > body.len() {
                    let _ = socket
                        .write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
                let response_body = &body[range_start..];
                let response = if range_start == 0 {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response_body.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                        response_body.len(),
                        range_start,
                        body.len().saturating_sub(1),
                        body.len()
                    )
                };
                if socket.write_all(response.as_bytes()).await.is_ok() {
                    let _ = socket.write_all(response_body).await;
                }
            });
        }
    });
    MockOrigin {
        url: format!("http://{address}/idlist"),
        body,
        range_starts,
        task,
    }
}

fn recorded_range_starts(origin: &MockOrigin) -> Vec<usize> {
    origin.range_starts.lock().unwrap().clone()
}

#[allow(clippy::too_many_arguments)]
async fn enqueue_load(
    pool: &RedisPool,
    schema: &IdListKeySchema,
    skh: &str,
    list: &str,
    url: &str,
    file_id: &str,
    creation_time: u64,
    size: u64,
) {
    let mut conn = pool.get().await.unwrap();
    let mut pipe = bb8_redis::redis::pipe();
    pipe.cmd("HSET")
        .arg(schema.task_meta_key(skh, list))
        .arg(task_meta_fields::OP)
        .arg(TASK_OP_LOAD)
        .arg(task_meta_fields::URL)
        .arg(url)
        .arg(task_meta_fields::FILE_ID)
        .arg(file_id)
        .arg(task_meta_fields::CREATION_TIME)
        .arg(creation_time)
        .arg(task_meta_fields::SIZE)
        .arg(size)
        .ignore()
        .cmd("ZADD")
        .arg(schema.queue_key())
        .arg(creation_time)
        .arg(IdListKeySchema::encode_task(skh, list))
        .ignore();
    pipe.query_async::<()>(&mut *conn).await.unwrap();
}

async fn enqueue_remove(pool: &RedisPool, schema: &IdListKeySchema, skh: &str, list: &str) {
    let mut conn = pool.get().await.unwrap();
    let mut pipe = bb8_redis::redis::pipe();
    pipe.cmd("HSET")
        .arg(schema.task_meta_key(skh, list))
        .arg(task_meta_fields::OP)
        .arg(TASK_OP_REMOVE)
        .ignore()
        .cmd("ZADD")
        .arg(schema.queue_key())
        .arg(0)
        .arg(IdListKeySchema::encode_task(skh, list))
        .ignore();
    pipe.query_async::<()>(&mut *conn).await.unwrap();
}

fn worker_config() -> IdListWorkerConfig {
    IdListWorkerConfig {
        pipeline_batch_size: 2,
        scan_count: 1,
        diff_partition_max_members: 1,
        ..IdListWorkerConfig::default()
    }
}

async fn worker_or_skip() -> Option<(RedisPool, IdListKeySchema, IdListSyncWorker)> {
    let Ok(pool) = create_redis_pool_from_env(2).await else {
        eprintln!("Skipping worker integration test — Redis unavailable");
        return None;
    };
    let schema = IdListKeySchema::new(format!("test-worker-{}", Uuid::new_v4()));
    let worker = IdListSyncWorker::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), Uuid::new_v4().to_string())),
        pool.clone(),
        reqwest::Client::new(),
        worker_config(),
    );
    Some((pool, schema, worker))
}

#[tokio::test]
async fn single_partition_diff_handles_small_rotations_and_empty_versions() {
    let Some((pool, schema, _)) = worker_or_skip().await else {
        return;
    };
    let worker = IdListSyncWorker::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), Uuid::new_v4().to_string())),
        pool.clone(),
        reqwest::Client::new(),
        IdListWorkerConfig {
            diff_partition_max_members: 100,
            ..worker_config()
        },
    );
    let skh = "company-hash";
    let list = "small-rotation";
    let origin = start_mock_origin(b"+A\n+B\n").await;
    let mut conn = pool.get().await.unwrap();
    let _: usize = conn
        .sadd(schema.member_key(skh, "A"), 999u64)
        .await
        .unwrap();

    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    let token = worker.dict.get_token(skh, list).await.unwrap().unwrap();

    *origin.body.write().await = b"+B\n+C\n".to_vec();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F2", 200, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    let a: Vec<u64> = conn.smembers(schema.member_key(skh, "A")).await.unwrap();
    assert_eq!(a, vec![999]);
    for member in ["B", "C"] {
        assert!(conn
            .sismember::<_, _, bool>(schema.member_key(skh, member), token)
            .await
            .unwrap());
    }

    origin.body.write().await.clear();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F3", 300, 0).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    for member in ["B", "C"] {
        assert!(!conn
            .sismember::<_, _, bool>(schema.member_key(skh, member), token)
            .await
            .unwrap());
    }

    *origin.body.write().await = b"+D\n".to_vec();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F4", 400, 3).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "D"), token)
        .await
        .unwrap());
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "A"), 999u64)
        .await
        .unwrap());
}

#[tokio::test]
async fn full_delta_streamed_rotation_and_remove_update_reverse_index() {
    let Ok(pool) = create_redis_pool_from_env(2).await else {
        eprintln!("Skipping worker integration test — Redis unavailable");
        return;
    };
    let schema = IdListKeySchema::new(format!("test-worker-{}", Uuid::new_v4()));
    let skh = "server-key-hash";
    let list = "customers";
    let origin = start_mock_origin(b"+A\n+B\n").await;
    let config = IdListWorkerConfig {
        pipeline_batch_size: 2,
        scan_count: 1,
        diff_partition_max_members: 1,
        ..IdListWorkerConfig::default()
    };
    let worker = IdListSyncWorker::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), Uuid::new_v4().to_string())),
        pool.clone(),
        reqwest::Client::new(),
        config,
    );

    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    let token = worker.dict.get_token(skh, list).await.unwrap().unwrap();
    let mut conn = pool.get().await.unwrap();
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "A"), token)
        .await
        .unwrap());
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "B"), token)
        .await
        .unwrap());

    *origin.body.write().await = b"+A\n+B\n-A\n+A\n+C\n".to_vec();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 15).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::Delta)
    );
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "A"), token)
        .await
        .unwrap());
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "C"), token)
        .await
        .unwrap());

    *origin.body.write().await = b"+B\n+D\n".to_vec();
    let _: () = conn
        .hset_multiple(
            schema.state_key(skh, list),
            &[
                (state_fields::LOADING_VERSION, "F2@200"),
                (state_fields::LOADING_SIZE, "3"),
            ],
        )
        .await
        .unwrap();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F2", 200, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    assert!(!conn
        .sismember::<_, _, bool>(schema.member_key(skh, "C"), token)
        .await
        .unwrap());
    assert!(!conn
        .sismember::<_, _, bool>(schema.member_key(skh, "A"), token)
        .await
        .unwrap());
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "D"), token)
        .await
        .unwrap());

    enqueue_remove(&pool, &schema, skh, list).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::Remove)
    );
    assert!(!conn
        .sismember::<_, _, bool>(schema.member_key(skh, "B"), token)
        .await
        .unwrap());
    assert!(!conn
        .sismember::<_, _, bool>(schema.member_key(skh, "D"), token)
        .await
        .unwrap());
    assert!(!conn
        .exists::<_, bool>(schema.state_key(skh, list))
        .await
        .unwrap());

    // Simulate a disconnect in the final cleanup pipeline after task_meta was deleted.
    let partial_list = "partial-removal";
    let partial_token = worker
        .dict
        .get_or_allocate_token(skh, partial_list)
        .await
        .unwrap();
    let partial_state_key = schema.state_key(skh, partial_list);
    let mut pipe = bb8_redis::redis::pipe();
    pipe.cmd("HSET")
        .arg(&partial_state_key)
        .arg(state_fields::VERSION)
        .arg("F3")
        .arg(state_fields::SIZE)
        .arg(10)
        .arg(state_fields::REMOVAL_FANOUT_COMPLETE)
        .arg(1)
        .ignore()
        .cmd("SADD")
        .arg(schema.all_lists_key(skh))
        .arg(partial_list)
        .ignore()
        .cmd("ZADD")
        .arg(schema.queue_key())
        .arg(0)
        .arg(IdListKeySchema::encode_task(skh, partial_list))
        .ignore();
    pipe.query_async::<()>(&mut *conn).await.unwrap();
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::Remove)
    );
    assert!(!conn
        .hexists::<_, _, bool>(schema.dict_key(skh), DictClient::token_field(partial_token))
        .await
        .unwrap());
    assert!(!conn.exists::<_, bool>(partial_state_key).await.unwrap());
}

#[tokio::test]
async fn empty_queue_is_idle() {
    let Some((_, _, worker)) = worker_or_skip().await else {
        return;
    };
    assert_eq!(worker.run_once().await.unwrap(), WorkerRunOutcome::Idle);
}

#[tokio::test]
async fn changed_file_id_with_equal_or_older_creation_time_is_dropped() {
    let Some((pool, schema, worker)) = worker_or_skip().await else {
        return;
    };
    let skh = "skh";
    let list = "stale";
    let origin = start_mock_origin(b"+NEW\n").await;
    let mut conn = pool.get().await.unwrap();
    let _: () = conn
        .hset_multiple(
            schema.state_key(skh, list),
            &[
                (state_fields::VERSION, "F2"),
                (state_fields::FILE_ID, "F2"),
                (state_fields::CREATION_TIME, "200"),
                (state_fields::SIZE, "6"),
            ],
        )
        .await
        .unwrap();
    for creation_time in [100, 200] {
        // A larger stale task must neither full-load nor append at the current offset.
        enqueue_load(
            &pool,
            &schema,
            skh,
            list,
            &origin.url,
            "F1",
            creation_time,
            50,
        )
        .await;
        assert_eq!(worker.run_once().await.unwrap(), WorkerRunOutcome::Dropped);
        let version: Option<String> = conn
            .hget(schema.state_key(skh, list), state_fields::VERSION)
            .await
            .unwrap();
        assert_eq!(version.as_deref(), Some("F2"));
    }
    assert!(recorded_range_starts(&origin).is_empty());
}

#[tokio::test]
async fn worker_uses_sdk_generation_and_size_policy() {
    let Some((pool, schema, worker)) = worker_or_skip().await else {
        return;
    };
    let skh = "skh";
    let list = "sdk-policy";
    let origin = start_mock_origin(b"+A\n").await;

    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 3).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    let token = worker.dict.get_token(skh, list).await.unwrap().unwrap();
    let mut conn = pool.get().await.unwrap();

    // Appends update creationTime but retain fileID and the original forward set.
    *origin.body.write().await = b"+A\n+B\n".to_vec();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 200, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::Delta)
    );
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "B"), token)
        .await
        .unwrap());
    assert!(!conn
        .exists::<_, bool>(schema.fwd_key(skh, list, "F1@200"))
        .await
        .unwrap());
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 300, 6).await;
    assert_eq!(worker.run_once().await.unwrap(), WorkerRunOutcome::Dropped);
    assert_eq!(recorded_range_starts(&origin), vec![0, 3]);
}

#[tokio::test]
async fn claim_conflict_does_not_apply() {
    let Some((pool, schema, worker)) = worker_or_skip().await else {
        return;
    };
    let skh = "skh";
    let list = "claimed";
    let origin = start_mock_origin(b"+A\n").await;
    let other = RedisWorkLock::new(pool.clone(), Uuid::new_v4().to_string());
    assert!(other
        .try_acquire(&schema.claim_key(skh, list), 30)
        .await
        .unwrap());
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 3).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::ClaimConflict
    );
    let mut conn = pool.get().await.unwrap();
    assert!(!conn
        .exists::<_, bool>(schema.state_key(skh, list))
        .await
        .unwrap());
}

#[tokio::test]
async fn remove_without_state_cleans_up_tracking() {
    let Some((pool, schema, worker)) = worker_or_skip().await else {
        return;
    };
    enqueue_remove(&pool, &schema, "skh", "ghost").await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::Remove)
    );
}

#[tokio::test]
async fn caught_up_task_persists_pending_forward_ttl() {
    let Some((pool, schema, worker)) = worker_or_skip().await else {
        return;
    };
    let skh = "skh";
    let list = "caught-up";
    let origin = start_mock_origin(b"+A\n+B\n").await;
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    let mut conn = pool.get().await.unwrap();
    let fwd = schema.fwd_key(skh, list, "F1@100");
    let _: bool = conn.expire(&fwd, 60).await.unwrap();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 6).await;
    assert_eq!(worker.run_once().await.unwrap(), WorkerRunOutcome::Dropped);
    let ttl: i64 = conn.ttl(&fwd).await.unwrap();
    assert_eq!(ttl, -1);
}

#[tokio::test]
async fn missing_forward_set_forces_full_reload() {
    let Some((pool, schema, worker)) = worker_or_skip().await else {
        return;
    };
    let skh = "abc1";
    let other_company = "abc2";
    let list = "missing-fwd";
    let origin = start_mock_origin(b"+A\n+B\n").await;
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    let token = worker.dict.get_token(skh, list).await.unwrap().unwrap();
    let mut conn = pool.get().await.unwrap();
    // Recovery scans the compact company prefix. A neighboring company can have
    // the same dictionary token and member without being affected by that scan.
    let _: () = conn
        .sadd(schema.member_key(other_company, "B"), token)
        .await
        .unwrap();
    let _: () = conn.del(schema.fwd_key(skh, list, "F1@100")).await.unwrap();
    *origin.body.write().await = b"+A\n+B\n-B\n".to_vec();
    let _: () = conn
        .sadd(schema.member_key(skh, "B"), 999u64)
        .await
        .unwrap();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "A"), token)
        .await
        .unwrap());
    assert!(conn
        .exists::<_, bool>(schema.fwd_key(skh, list, "F1@100"))
        .await
        .unwrap());
    assert!(!conn
        .sismember::<_, _, bool>(schema.member_key(skh, "B"), token)
        .await
        .unwrap());
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "B"), 999u64)
        .await
        .unwrap());
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(other_company, "B"), token)
        .await
        .unwrap());
}

#[tokio::test]
async fn resumes_matching_load_from_loading_size() {
    let Some((pool, schema, worker)) = worker_or_skip().await else {
        return;
    };
    let skh = "skh";
    let list = "resume-load";
    let origin = start_mock_origin(b"+A\n+B\n").await;
    let mut conn = pool.get().await.unwrap();
    let fwd = schema.fwd_key(skh, list, "F1@100");
    let _: () = conn
        .hset_multiple(
            schema.state_key(skh, list),
            &[
                (state_fields::LOADING_VERSION, "F1@100"),
                (state_fields::LOADING_SIZE, "3"),
            ],
        )
        .await
        .unwrap();
    let _: () = conn.sadd(&fwd, "A").await.unwrap();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    let token = worker.dict.get_token(skh, list).await.unwrap().unwrap();
    assert_eq!(recorded_range_starts(&origin), vec![3]);
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "A"), token)
        .await
        .unwrap());
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "B"), token)
        .await
        .unwrap());
}

#[tokio::test]
async fn missing_pending_fwd_resets_loading_checkpoint() {
    let Some((pool, schema, worker)) = worker_or_skip().await else {
        return;
    };
    let skh = "skh";
    let list = "reset-load";
    let origin = start_mock_origin(b"+A\n+B\n").await;
    let mut conn = pool.get().await.unwrap();
    let _: () = conn
        .hset_multiple(
            schema.state_key(skh, list),
            &[
                (state_fields::LOADING_VERSION, "F1@100"),
                (state_fields::LOADING_SIZE, "3"),
            ],
        )
        .await
        .unwrap();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    assert_eq!(recorded_range_starts(&origin), vec![0]);
    let token = worker.dict.get_token(skh, list).await.unwrap().unwrap();
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "A"), token)
        .await
        .unwrap());
}

#[tokio::test]
async fn newer_generation_abandons_interrupted_load() {
    let Some((pool, schema, worker)) = worker_or_skip().await else {
        return;
    };
    let skh = "skh";
    let list = "abandon";
    let origin = start_mock_origin(b"+A\n+B\n").await;
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F1", 100, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    let token = worker.dict.get_token(skh, list).await.unwrap().unwrap();
    let mut conn = pool.get().await.unwrap();
    let leftover = schema.fwd_key(skh, list, "F2@200");
    let _: () = conn.sadd(&leftover, "LEFTOVER").await.unwrap();
    let _: () = conn
        .hset_multiple(
            schema.state_key(skh, list),
            &[
                (state_fields::LOADING_VERSION, "F2@200"),
                (state_fields::LOADING_SIZE, "3"),
                (state_fields::DIFF_PARTITION, "1"),
            ],
        )
        .await
        .unwrap();
    let _: () = conn
        .sadd(schema.member_key(skh, "LEFTOVER"), token)
        .await
        .unwrap();
    let _: () = conn.srem(schema.member_key(skh, "A"), token).await.unwrap();
    origin.range_starts.lock().unwrap().clear();
    *origin.body.write().await = b"+A\n+Z\n".to_vec();
    enqueue_load(&pool, &schema, skh, list, &origin.url, "F3", 300, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    assert_eq!(recorded_range_starts(&origin), vec![0]);
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "Z"), token)
        .await
        .unwrap());
    assert!(conn
        .sismember::<_, _, bool>(schema.member_key(skh, "A"), token)
        .await
        .unwrap());
    let state: std::collections::HashMap<String, String> =
        conn.hgetall(schema.state_key(skh, list)).await.unwrap();
    assert_eq!(state.get("version").map(String::as_str), Some("F3@300"));
    assert_eq!(state.get("file_id").map(String::as_str), Some("F3"));
    assert_eq!(state.get("size").map(String::as_str), Some("6"));
    assert!(!state.contains_key(state_fields::LOADING_VERSION));
    assert!(!state.contains_key(state_fields::DIFF_PARTITION));
    assert!(!conn
        .sismember::<_, _, bool>(schema.member_key(skh, "LEFTOVER"), token)
        .await
        .unwrap());
}

#[tokio::test]
async fn resumed_diff_commits_downloaded_size_then_fetches_append() {
    let Some((pool, schema, worker)) = worker_or_skip().await else {
        return;
    };
    let origin = start_mock_origin(b"+B\n+C\n").await;
    let mut conn = pool.get().await.unwrap();
    let _: () = conn
        .hset_multiple(
            schema.state_key("skh", "list"),
            &[
                (state_fields::LOADING_VERSION, "F1@100"),
                (state_fields::LOADING_SIZE, "3"),
                (state_fields::LOADING_CARDINALITY, "1"),
                (state_fields::DIFF_PARTITION, "0"),
            ],
        )
        .await
        .unwrap();
    let _: () = conn
        .sadd(schema.fwd_key("skh", "list", "F1@100"), "B")
        .await
        .unwrap();
    enqueue_load(&pool, &schema, "skh", "list", &origin.url, "F1", 100, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::FullReload)
    );
    assert_eq!(
        conn.hget::<_, _, u64>(schema.state_key("skh", "list"), state_fields::SIZE)
            .await
            .unwrap(),
        3
    );
    enqueue_load(&pool, &schema, "skh", "list", &origin.url, "F1", 200, 6).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::Delta)
    );
    let token = worker.dict.get_token("skh", "list").await.unwrap().unwrap();
    for member in ["B", "C"] {
        assert!(conn
            .sismember::<_, _, bool>(schema.member_key("skh", member), token)
            .await
            .unwrap());
    }
    assert_eq!(recorded_range_starts(&origin), vec![3]);
}

#[tokio::test]
async fn positive_byte_empty_lists_stay_synced_and_can_be_removed() {
    let Some((pool, schema, worker)) = worker_or_skip().await else {
        return;
    };
    let origin = start_mock_origin(b"+A\n-A\n").await;
    enqueue_load(&pool, &schema, "skh", "list", &origin.url, "F1", 100, 6).await;
    worker.run_once().await.unwrap();
    enqueue_load(&pool, &schema, "skh", "list", &origin.url, "F1", 100, 6).await;
    assert_eq!(worker.run_once().await.unwrap(), WorkerRunOutcome::Dropped);
    *origin.body.write().await = b"+A\n-A\n+B\n".to_vec();
    enqueue_load(&pool, &schema, "skh", "list", &origin.url, "F1", 200, 9).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::Delta)
    );
    *origin.body.write().await = b"+A\n-A\n+B\n-B\n".to_vec();
    enqueue_load(&pool, &schema, "skh", "list", &origin.url, "F1", 300, 12).await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::Delta)
    );
    enqueue_load(&pool, &schema, "skh", "list", &origin.url, "F1", 300, 12).await;
    assert_eq!(worker.run_once().await.unwrap(), WorkerRunOutcome::Dropped);
    enqueue_remove(&pool, &schema, "skh", "list").await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::Remove)
    );
    assert_eq!(recorded_range_starts(&origin), vec![0, 6, 9]);
}

#[tokio::test]
async fn initial_load_is_discovered_for_removal_even_after_pending_set_expires() {
    use super::super::{hash_company_id, IdListSyncProducer};
    use arc_swap::ArcSwap;
    use std::collections::HashMap;
    let Some((pool, schema, worker)) = worker_or_skip().await else {
        return;
    };
    let skh = hash_company_id("secret-test");
    let origin = start_mock_origin(b"+A\n+B").await;
    enqueue_load(&pool, &schema, &skh, "list", &origin.url, "F1", 100, 6).await;
    assert!(worker.run_once().await.is_err());
    let token = worker.dict.get_token(&skh, "list").await.unwrap().unwrap();
    let mut conn = pool.get().await.unwrap();
    assert!(conn
        .sismember::<_, _, bool>(schema.all_lists_key(&skh), "list")
        .await
        .unwrap());
    // Reproduce a later interruption after partial fan-out, followed by pending TTL expiry.
    let _: () = conn
        .sadd(schema.member_key(&skh, "A"), token)
        .await
        .unwrap();
    let _: () = conn
        .del(schema.fwd_key(&skh, "list", "F1@100"))
        .await
        .unwrap();
    let manifests = Arc::new(ArcSwap::from_pointee(HashMap::from([(
        "secret-test".to_string(),
        crate::datastore::id_list_manifest::IdListManifest::new(),
    )])));
    let producer = IdListSyncProducer::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), Uuid::new_v4().to_string())),
        pool.clone(),
        manifests,
        Arc::new(vec!["secret-test".to_string()]),
        30,
    );
    producer.run_once().await.unwrap();
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::Remove)
    );
    assert!(!conn
        .sismember::<_, _, bool>(schema.member_key(&skh, "A"), token)
        .await
        .unwrap());
    assert!(!conn
        .exists::<_, bool>(schema.state_key(&skh, "list"))
        .await
        .unwrap());
    assert!(worker.dict.get_token(&skh, "list").await.unwrap().is_none());
}

#[tokio::test]
async fn lost_committed_forward_set_does_not_block_removal() {
    let Some((pool, schema, worker)) = worker_or_skip().await else {
        return;
    };
    let origin = start_mock_origin(b"+A\n").await;
    enqueue_load(&pool, &schema, "skh", "list", &origin.url, "F1", 100, 3).await;
    worker.run_once().await.unwrap();
    let token = worker.dict.get_token("skh", "list").await.unwrap().unwrap();
    let mut conn = pool.get().await.unwrap();
    let _: () = conn
        .del(schema.fwd_key("skh", "list", "F1@100"))
        .await
        .unwrap();
    enqueue_remove(&pool, &schema, "skh", "list").await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::Completed(WorkerAction::Remove)
    );
    assert!(!conn
        .sismember::<_, _, bool>(schema.member_key("skh", "A"), token)
        .await
        .unwrap());
}

#[tokio::test]
async fn claim_is_acquired_before_reading_state() {
    let Some((pool, schema, worker)) = worker_or_skip().await else {
        return;
    };
    let other = RedisWorkLock::new(pool.clone(), Uuid::new_v4().to_string());
    let key = schema.claim_key("skh", "list");
    assert!(other.try_acquire(&key, 30).await.unwrap());
    let mut conn = pool.get().await.unwrap();
    // Reading before claiming would fail with WRONGTYPE instead of ClaimConflict.
    let _: () = conn
        .set(schema.state_key("skh", "list"), "being-replaced")
        .await
        .unwrap();
    enqueue_remove(&pool, &schema, "skh", "list").await;
    assert_eq!(
        worker.run_once().await.unwrap(),
        WorkerRunOutcome::ClaimConflict
    );
    other.release(&key).await.unwrap();
}

#[tokio::test]
async fn worker_errors_wait_before_retry_and_cancel_promptly() {
    use crate::datastore::caching::{
        fake_redis::{FakeRedis, FakeRedisMode},
        redis_pool::{create_redis_pool, RedisEnvConfig},
    };
    use std::time::Duration;
    let redis = FakeRedis::start_with(FakeRedisMode::ErrorOn("ZPOPMIN".into())).await;
    let config: RedisEnvConfig = envy::from_iter([
        ("REDIS_ENTERPRISE_HOST".to_string(), "127.0.0.1".to_string()),
        ("REDIS_ENTERPRISE_PORT".to_string(), redis.port.to_string()),
    ])
    .unwrap();
    let pool = create_redis_pool(&config, 2).await.unwrap();
    let worker = Arc::new(IdListSyncWorker::new(
        IdListKeySchema::new("retry"),
        Arc::new(RedisWorkLock::new(pool.clone(), "test")),
        pool,
        reqwest::Client::new(),
        IdListWorkerConfig {
            poll_interval_ms: 60_000,
            ..worker_config()
        },
    ));
    let cancellation = tokio_util::sync::CancellationToken::new();
    let task = worker.spawn_loop(&tokio::runtime::Handle::current(), cancellation.clone());
    tokio::time::timeout(Duration::from_secs(2), async {
        while !redis.received_command("ZPOPMIN") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        redis
            .commands()
            .iter()
            .filter(|c| c[0] == "ZPOPMIN")
            .count(),
        1
    );
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
}
