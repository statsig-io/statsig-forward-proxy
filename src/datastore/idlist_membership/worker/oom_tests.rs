use super::*;

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::datastore::caching::redis_pool::{create_redis_pool, RedisEnvConfig};
use crate::datastore::id_list_manifest::{IdListManifest, IdListManifestEntry};
use crate::datastore::idlist_membership::namespaces::IdListNamespaces;
use crate::datastore::idlist_membership::{hash_company_id, IdListSyncProducer};

struct DisposableRedis {
    child: Child,
    _directory: tempfile::TempDir,
}

impl Drop for DisposableRedis {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl DisposableRedis {
    async fn start() -> (Self, RedisPool) {
        let directory = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let executable = std::env::var("REDIS_SERVER_BIN").unwrap_or("redis-server".into());
        let child = Command::new(executable)
            .args([
                "--bind",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--save",
                "",
                "--appendonly",
                "no",
                "--protected-mode",
                "yes",
            ])
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("this opt-in test requires redis-server on PATH or REDIS_SERVER_BIN");
        let mut server = Self {
            child,
            _directory: directory,
        };
        // Explicit local configuration: never inherit credentials or a live Redis address.
        let config: RedisEnvConfig = envy::from_iter(
            [
                ("REDIS_ENTERPRISE_HOST", "127.0.0.1".to_string()),
                ("REDIS_ENTERPRISE_PORT", port.to_string()),
                ("REDIS_CONNECTION_POOL_MIN_SIZE", "1".to_string()),
                ("REDIS_CONNECTION_POOL_MAX_SIZE", "4".to_string()),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value)),
        )
        .unwrap();
        for _ in 0..20 {
            if let Ok(pool) = create_redis_pool(&config, 1).await {
                return (server, pool);
            }
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "disposable Redis exited"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("disposable Redis did not become ready");
    }
}

/// CONFIG SET is used only on a child process owned by this test, never the configured test pool.
#[tokio::test]
#[ignore = "starts a disposable redis-server and injects a real OOM; run explicitly"]
async fn compact_keys_recover_from_real_oom_in_isolated_redis() {
    for (prefix, enable_reverse_index) in [
        ("", true),
        ("prefixed-test", true),
        ("", false),
        ("prefixed-test", false),
    ] {
        let (_server, pool) = DisposableRedis::start().await;
        let schema = IdListKeySchema::new(prefix);
        let company = "oom-company-a";
        let namespace = hash_company_id(company);
        let neighbor = hash_company_id("oom-company-b");
        assert_eq!(namespace.len(), 4);
        assert_ne!(namespace, neighbor);
        let origin = start_mock_origin(b"+00000001\n+00000002\n").await;
        let worker = IdListSyncWorker::new(
            schema.clone(),
            Arc::new(RedisWorkLock::new(pool.clone(), "oom-worker")),
            pool.clone(),
            reqwest::Client::new(),
            IdListWorkerConfig {
                enable_reverse_index,
                pipeline_batch_size: 128,
                scan_count: 1024,
                diff_partition_max_members: 2000,
                ..IdListWorkerConfig::default()
            },
        );
        enqueue_load(
            &pool,
            &schema,
            &namespace,
            "list",
            &origin.url,
            "F1",
            100,
            20,
        )
        .await;
        worker.run_once().await.unwrap();
        let token = worker
            .dict
            .get_token(&namespace, "list")
            .await
            .unwrap()
            .unwrap();
        let mut conn = pool.get().await.unwrap();
        let _: () = conn.set(schema.ready_key(&namespace), 1).await.unwrap();
        let _: () = conn
            .sadd(schema.member_key(&neighbor, "00000001"), token)
            .await
            .unwrap();
        if prefix.is_empty() {
            assert_eq!(
                schema.member_key(&namespace, "00000001"),
                format!("i:{namespace}00000001")
            );
            assert_eq!(schema.member_key(&namespace, "00000001").len(), 14);
        }
        // Missing-forward recovery must disable readiness and clear only this company's token.
        let _: () = conn
            .unlink(schema.fwd_key(&namespace, "list", "F1@100"))
            .await
            .unwrap();
        let body: Vec<u8> = (100_000..150_000)
            .flat_map(|i| format!("+{i:08x}\n").into_bytes())
            .collect();
        *origin.body.write().await = body.clone();
        enqueue_load(
            &pool,
            &schema,
            &namespace,
            "list",
            &origin.url,
            "F2",
            200,
            body.len() as u64,
        )
        .await;
        let filler = "owned-oom-test-filler";
        let _: () = conn.set(filler, vec![0u8; 16 * 1024 * 1024]).await.unwrap();
        let memory: String = bb8_redis::redis::cmd("INFO")
            .arg("memory")
            .query_async(&mut *conn)
            .await
            .unwrap();
        let used: u64 = memory
            .lines()
            .find_map(|line| line.strip_prefix("used_memory:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let _: () = bb8_redis::redis::cmd("CONFIG")
            .arg(&["SET", "maxmemory-policy", "noeviction"])
            .query_async(&mut *conn)
            .await
            .unwrap();
        let _: () = bb8_redis::redis::cmd("CONFIG")
            .arg(&["SET", "maxmemory"])
            .arg(used + 512 * 1024)
            .query_async(&mut *conn)
            .await
            .unwrap();
        let oom_error = worker
            .run_once()
            .await
            .expect_err("replacement should hit the real memory limit");
        eprintln!("OOM case: prefix={prefix:?}, enable_reverse_index={enable_reverse_index}, error={oom_error}");
        let errors: String = bb8_redis::redis::cmd("INFO")
            .arg("errorstats")
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert!(
            errors.contains("errorstat_OOM:") || oom_error.to_string().contains("OOM"),
            "expected Redis OOM evidence: error={oom_error}; stats={errors}"
        );
        let fields: HashMap<String, String> = conn
            .hgetall(schema.state_key(&namespace, "list"))
            .await
            .unwrap();
        assert_eq!(fields[state_fields::VERSION], "F1@100");
        assert_eq!(fields[state_fields::FILE_ID], "F1");
        assert_eq!(fields[state_fields::CREATION_TIME], "100");
        assert_eq!(fields[state_fields::SIZE], "20");
        assert_eq!(fields[state_fields::CARDINALITY], "2");
        assert_eq!(fields[state_fields::URL], origin.url);
        assert_eq!(fields[state_fields::LOADING_VERSION], "F2@200");
        assert_eq!(fields[state_fields::RECONCILE_REQUIRED], "1");
        assert!(!conn
            .exists::<_, bool>(schema.ready_key(&namespace))
            .await
            .unwrap());
        assert!(!conn
            .exists::<_, bool>(schema.claim_key(&namespace, "list"))
            .await
            .unwrap());
        assert!(
            conn.ttl::<_, i64>(schema.fwd_key(&namespace, "list", "F2@200"))
                .await
                .unwrap()
                > 0
        );
        assert!(conn
            .sismember::<_, _, bool>(schema.member_key(&neighbor, "00000001"), token)
            .await
            .unwrap());
        // Free only the owned filler; retain the same limit and resume from the saved checkpoint.
        let _: () = conn.del(filler).await.unwrap();
        let namespaces = Arc::new(IdListNamespaces::new(vec!["test-key".into()]));
        namespaces.company_ids.set_for_test("test-key", company);
        namespaces.manifests.store(Arc::new(HashMap::from([(
            "test-key".into(),
            IdListManifest::from([(
                "list".into(),
                IdListManifestEntry {
                    name: "list".into(),
                    file_id: "F2".into(),
                    creation_time: 200,
                    size: body.len() as u64,
                    url: origin.url.clone(),
                },
            )]),
        )])));
        let producer_lock = Arc::new(RedisWorkLock::new(pool.clone(), "oom-producer"));
        let producer = IdListSyncProducer::with_namespaces(
            schema.clone(),
            producer_lock.clone(),
            pool.clone(),
            namespaces,
            30,
        )
        .with_reverse_index_enabled(enable_reverse_index);
        producer.run_once().await.unwrap();
        assert_eq!(
            worker.run_once().await.unwrap(),
            WorkerRunOutcome::Completed(WorkerAction::FullReload)
        );
        let fields: HashMap<String, String> = conn
            .hgetall(schema.state_key(&namespace, "list"))
            .await
            .unwrap();
        assert_eq!(fields[state_fields::VERSION], "F2@200");
        assert_eq!(fields[state_fields::FILE_ID], "F2");
        assert_eq!(fields[state_fields::CREATION_TIME], "200");
        assert_eq!(fields[state_fields::URL], origin.url);
        assert_eq!(fields[state_fields::SIZE], body.len().to_string());
        assert_eq!(fields[state_fields::CARDINALITY], "50000");
        assert_eq!(
            fields.len(),
            if enable_reverse_index { 6 } else { 7 },
            "only committed state and the storage-mode marker may remain"
        );
        assert_eq!(
            fields.contains_key(crate::datastore::idlist_membership::REVERSE_INDEX_DISABLED),
            !enable_reverse_index
        );
        for field in [
            state_fields::LOADING_VERSION,
            state_fields::LOADING_SIZE,
            state_fields::LOADING_CARDINALITY,
            state_fields::DIFF_PARTITION,
            state_fields::RECONCILE_REQUIRED,
            state_fields::REMOVAL_FANOUT_COMPLETE,
        ] {
            assert!(!fields.contains_key(field), "stale recovery field {field}");
        }
        let forward = schema.fwd_key(&namespace, "list", "F2@200");
        assert_eq!(conn.scard::<_, u64>(&forward).await.unwrap(), 50_000);
        assert_eq!(conn.ttl::<_, i64>(&forward).await.unwrap(), -1);
        assert!(!conn
            .exists::<_, bool>(schema.member_key(&namespace, "00000001"))
            .await
            .unwrap());
        assert!(conn
            .sismember::<_, _, bool>(schema.member_key(&neighbor, "00000001"), token)
            .await
            .unwrap());
        let members: Vec<String> = conn.smembers(&forward).await.unwrap();
        for chunk in members.chunks(512) {
            let mut pipe = bb8_redis::redis::pipe();
            for member in chunk {
                pipe.cmd("SMEMBERS")
                    .arg(schema.member_key(&namespace, member));
            }
            let mappings: Vec<Vec<u64>> = pipe.query_async(&mut *conn).await.unwrap();
            if enable_reverse_index {
                assert!(mappings.iter().all(|tokens| tokens == &[token]));
            } else {
                assert!(mappings.iter().all(Vec::is_empty));
            }
        }
        // Permit the next producer tick without waiting for our election lock to expire.
        producer_lock
            .release(&schema.producer_lock_key())
            .await
            .unwrap();
        producer.run_once().await.unwrap();
        assert_eq!(
            conn.get::<_, u64>(schema.ready_key(&namespace))
                .await
                .unwrap(),
            1
        );
        assert_eq!(conn.zcard::<_, u64>(schema.queue_key()).await.unwrap(), 0);
        let reverse_keys: Vec<String> = conn
            .keys(format!("{}*", schema.member_key(&namespace, "")))
            .await
            .unwrap();
        assert_eq!(
            reverse_keys.len(),
            if enable_reverse_index { 50_000 } else { 0 },
            "reverse keys must match the configured storage mode"
        );
        assert!(
            recorded_range_starts(&origin)
                .iter()
                .any(|&start| start > 0),
            "recovery should resume a partial download"
        );
    }
}
