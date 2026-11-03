use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use bb8_redis::redis::AsyncCommands;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

use super::redis_syncer::{IdListSyncProducer, ParsedManifest};
use super::schema::{hash_company_id, state_fields, task_meta_fields};
use super::{
    new_holder_id, DictClient, IdListKeySchema, IdListSyncWorker, IdListWorkerConfig,
    ProducerTickOutcome, RedisWorkLock, WorkerAction, WorkerRunOutcome,
};
use crate::datastore::caching::redis_pool::{create_redis_pool_from_env, RedisPool};
use crate::datastore::id_list_manifest::{IdListManifest, IdListManifestEntry};
use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::{OperationType, ProxyEvent, ProxyEventType};

const CHILD_ENV: &str = "SFP_MULTI_INSTANCE_TEST_CHILD";
const TEST_NAME: &str =
    "datastore::idlist_membership::multi_instance_tests::multiple_proxy_instances_share_work_exactly_once";
const INSTANCE_COUNT: usize = 6;

#[derive(Default)]
struct MetricCollector {
    events: Vec<(ProxyEventType, OperationType, i64)>,
}

impl MetricCollector {
    fn record(&mut self, event: &ProxyEvent) {
        if let Some(stat) = &event.stat {
            self.events
                .push((event.event_type, stat.operation_type, stat.value));
        }
    }

    fn values(&self, event_type: ProxyEventType) -> Vec<(OperationType, i64)> {
        self.events
            .iter()
            .filter_map(|(actual, operation, value)| {
                (*actual == event_type).then_some((*operation, *value))
            })
            .collect()
    }

    fn assert_values(&self, event_type: ProxyEventType, expected: &[(OperationType, i64)]) {
        assert_eq!(self.values(event_type), expected, "{event_type}");
    }
}

struct MockOrigin {
    base_url: String,
    requests: Arc<Mutex<HashMap<String, usize>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockOrigin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_mock_origin(files: HashMap<String, Vec<u8>>) -> MockOrigin {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let files = Arc::new(files);
    let requests = Arc::new(Mutex::new(HashMap::new()));
    let server_requests = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let files = Arc::clone(&files);
            let requests = Arc::clone(&server_requests);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let read = socket.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                let Some(body) = files.get(path) else {
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    return;
                };
                *requests
                    .lock()
                    .unwrap()
                    .entry(path.to_string())
                    .or_default() += 1;
                let response = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes 0-{}/{}\r\nConnection: close\r\n\r\n",
                    body.len(),
                    body.len().saturating_sub(1),
                    body.len()
                );
                if socket.write_all(response.as_bytes()).await.is_ok() {
                    let _ = socket.write_all(body).await;
                }
            });
        }
    });
    MockOrigin {
        base_url: format!("http://{address}"),
        requests,
        task,
    }
}

fn manifest(
    origin: &MockOrigin,
    files: &[(&str, &str, &[u8])],
    creation_time: u64,
) -> IdListManifest {
    files
        .iter()
        .map(|(list, file_id, body)| {
            (
                (*list).to_string(),
                IdListManifestEntry {
                    name: (*list).to_string(),
                    size: body.len() as u64,
                    url: format!("{}/{}", origin.base_url, list),
                    creation_time,
                    file_id: (*file_id).to_string(),
                },
            )
        })
        .collect()
}

fn new_worker(pool: &RedisPool, schema: &IdListKeySchema) -> Arc<IdListSyncWorker> {
    Arc::new(IdListSyncWorker::new(
        schema.clone(),
        Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
        pool.clone(),
        reqwest::Client::new(),
        IdListWorkerConfig {
            poll_interval_ms: 10,
            pipeline_batch_size: 2,
            scan_count: 1,
            ..IdListWorkerConfig::default()
        },
    ))
}

async fn collect_metrics(
    mut receiver: tokio::sync::broadcast::Receiver<Arc<ProxyEvent>>,
) -> MetricCollector {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut collector = MetricCollector::default();
        loop {
            let event = receiver.recv().await.expect("metric sender remained open");
            collector.record(&event);
            if collector.values(ProxyEventType::IdListTaskDurationMs).len() == 3 {
                return collector;
            }
        }
    })
    .await
    .expect("timed out waiting for metrics")
}

// The child process isolates the process-global metric bus from every other parallel test.
// It also makes exact zero/count assertions deterministic under the default test harness.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multiple_proxy_instances_share_work_exactly_once() {
    if std::env::var_os(CHILD_ENV).is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg(TEST_NAME)
            .arg("--exact")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_ENV, "1")
            .status()
            .expect("start isolated multi-instance test");
        assert!(status.success(), "isolated multi-instance test failed");
        return;
    }

    let Ok(pool) = create_redis_pool_from_env(2).await else {
        eprintln!("Skipping multi-instance test — Redis unavailable");
        return;
    };
    let schema = IdListKeySchema::new(format!("test-multi-instance-{}", Uuid::new_v4()));
    let server_key = format!("secret-test-{}", Uuid::new_v4());
    let skh = hash_company_id(&server_key);
    let alpha = b"+alpha-1\n+shared\n".as_slice();
    let beta = b"+beta-1\n+beta-2\n".as_slice();
    let gamma = b"+gamma-1\n+shared\n".as_slice();
    let files = [
        ("alpha", "file-alpha", alpha),
        ("beta", "file-beta", beta),
        ("gamma", "file-gamma", gamma),
    ];
    let origin = start_mock_origin(HashMap::from([
        ("/alpha".to_string(), alpha.to_vec()),
        ("/beta".to_string(), beta.to_vec()),
        ("/gamma".to_string(), gamma.to_vec()),
    ]))
    .await;
    let creation_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let manifests = Arc::new(ArcSwap::from_pointee(
        HashMap::<String, ParsedManifest>::from([(
            server_key.clone(),
            manifest(&origin, &files, creation_time),
        )]),
    ));
    let metrics = ProxyEventObserver::subscribe();

    // Model three proxy pods: all attempt production, but the Redis election allows one.
    let producers: Vec<_> = (0..3)
        .map(|_| {
            Arc::new(IdListSyncProducer::new(
                schema.clone(),
                Arc::new(RedisWorkLock::new(pool.clone(), new_holder_id())),
                pool.clone(),
                Arc::clone(&manifests),
                Arc::new(vec![server_key.clone()]),
                30,
            ))
        })
        .collect();
    let producer_outcomes = futures::future::join_all(
        producers
            .into_iter()
            .map(|producer| async move { producer.run_once().await.unwrap() }),
    )
    .await;
    assert_eq!(
        producer_outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ProducerTickOutcome::Success(_)))
            .count(),
        1
    );
    assert_eq!(
        producer_outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ProducerTickOutcome::Skipped))
            .count(),
        2
    );
    let mut conn = pool.get().await.unwrap();
    assert_eq!(conn.zcard::<_, u64>(schema.queue_key()).await.unwrap(), 3);

    // Model six worker pods competing for three list tasks.
    let worker_outcomes = futures::future::join_all(
        (0..INSTANCE_COUNT)
            .map(|_| new_worker(&pool, &schema))
            .map(|worker| async move { worker.run_once().await.unwrap() }),
    )
    .await;
    assert_eq!(
        worker_outcomes
            .iter()
            .filter(|outcome| {
                matches!(
                    outcome,
                    WorkerRunOutcome::Completed(WorkerAction::FullReload)
                )
            })
            .count(),
        3
    );
    assert_eq!(
        worker_outcomes
            .iter()
            .filter(|outcome| matches!(outcome, WorkerRunOutcome::Idle))
            .count(),
        INSTANCE_COUNT - 3
    );
    assert_eq!(conn.zcard::<_, u64>(schema.queue_key()).await.unwrap(), 0);

    // Each list was downloaded exactly once despite six competing worker instances.
    let requests = origin.requests.lock().unwrap().clone();
    assert_eq!(requests.get("/alpha"), Some(&1));
    assert_eq!(requests.get("/beta"), Some(&1));
    assert_eq!(requests.get("/gamma"), Some(&1));

    let dict = DictClient::new(pool.clone(), schema.clone());
    for (list, file_id, body) in files {
        let state: HashMap<String, String> =
            conn.hgetall(schema.state_key(&skh, list)).await.unwrap();
        assert_eq!(
            state.get(state_fields::VERSION).map(String::as_str),
            Some(format!("{file_id}@{creation_time}").as_str())
        );
        assert_eq!(
            state.get(state_fields::FILE_ID).map(String::as_str),
            Some(file_id)
        );
        assert_eq!(
            state
                .get(state_fields::SIZE)
                .and_then(|size| size.parse::<usize>().ok()),
            Some(body.len())
        );
        assert!(!state.contains_key(state_fields::LOADING_VERSION));
        assert_eq!(
            conn.scard::<_, u64>(schema.fwd_key(&skh, list, &format!("{file_id}@{creation_time}")))
                .await
                .unwrap(),
            2
        );
        let meta_file_id: Option<String> = conn
            .hget(schema.task_meta_key(&skh, list), task_meta_fields::FILE_ID)
            .await
            .unwrap();
        assert_eq!(meta_file_id.as_deref(), Some(file_id));
        assert!(!conn
            .exists::<_, bool>(schema.claim_key(&skh, list))
            .await
            .unwrap());
    }
    let alpha_token = dict.get_token(&skh, "alpha").await.unwrap().unwrap();
    let gamma_token = dict.get_token(&skh, "gamma").await.unwrap().unwrap();
    let shared_tokens: Vec<u64> = conn
        .smembers(schema.member_key(&skh, "shared"))
        .await
        .unwrap();
    assert_eq!(shared_tokens.len(), 2);
    assert!(shared_tokens.contains(&alpha_token));
    assert!(shared_tokens.contains(&gamma_token));

    let collector = collect_metrics(metrics).await;
    let producer_success = collector.values(ProxyEventType::IdListProducerRunSuccess);
    assert_eq!(producer_success.len(), 1);
    assert_eq!(producer_success[0].0, OperationType::Distribution);
    let producer_skipped = collector.values(ProxyEventType::IdListProducerRunSkipped);
    assert_eq!(producer_skipped.len(), 2);
    assert!(producer_skipped
        .iter()
        .all(|(operation, _)| *operation == OperationType::Distribution));
    let producer_durations = collector.values(ProxyEventType::IdListProducerDurationMs);
    assert_eq!(producer_durations.len(), 1);
    assert_eq!(producer_durations[0].0, OperationType::Distribution);
    collector.assert_values(
        ProxyEventType::IdListTasksEnqueued,
        &[(OperationType::IncrByValue, 3)],
    );
    collector.assert_values(
        ProxyEventType::IdListQueueDepth,
        &[(OperationType::Gauge, 3)],
    );
    assert_eq!(
        collector.values(ProxyEventType::IdListFullReloadStarted),
        vec![(OperationType::IncrByValue, 1); 3]
    );
    assert_eq!(
        collector.values(ProxyEventType::IdListFullReloadCommitted),
        vec![(OperationType::IncrByValue, 1); 3]
    );
    assert_eq!(
        collector.values(ProxyEventType::IdListTaskCompleted),
        vec![(OperationType::IncrByValue, 1); 3]
    );
    assert_eq!(
        collector.values(ProxyEventType::IdListTaskDurationMs).len(),
        3
    );
    assert!(collector
        .values(ProxyEventType::IdListTaskDurationMs)
        .iter()
        .all(|(operation, value)| *operation == OperationType::Timing && *value >= 0));
    let mut checkpoint_lag = collector.values(ProxyEventType::IdListSyncCheckpointLag);
    checkpoint_lag.sort_by_key(|(_, value)| *value);
    let mut expected_lag = files
        .iter()
        .map(|(_, _, body)| (OperationType::Gauge, body.len() as i64))
        .collect::<Vec<_>>();
    expected_lag.sort_by_key(|(_, value)| *value);
    assert_eq!(checkpoint_lag, expected_lag);
    assert_eq!(
        collector
            .values(ProxyEventType::IdListStalenessSeconds)
            .len(),
        3
    );
    let claims = collector.values(ProxyEventType::IdListClaimHeld);
    assert_eq!(claims.len(), 6);
    assert_eq!(
        claims
            .iter()
            .filter(|event| **event == (OperationType::Gauge, 1))
            .count(),
        3
    );
    assert_eq!(
        claims
            .iter()
            .filter(|event| **event == (OperationType::Gauge, 0))
            .count(),
        3
    );
    assert!(collector
        .values(ProxyEventType::IdListTaskFailed)
        .is_empty());
    assert!(collector
        .values(ProxyEventType::IdListTaskClaimConflict)
        .is_empty());
    assert!(collector
        .values(ProxyEventType::IdListProducerRunFailed)
        .is_empty());
    assert!(collector
        .values(ProxyEventType::IdListStaleTaskDropped)
        .is_empty());
    assert!(collector
        .values(ProxyEventType::IdListFullReloadResumed)
        .is_empty());
    assert!(collector
        .values(ProxyEventType::IdListDeltaApplied)
        .is_empty());
}
