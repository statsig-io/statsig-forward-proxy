use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use bb8_redis::redis::RedisError;

use crate::datastore::caching::redis_pool::RedisPool;

use super::super::schema::{
    task_meta_fields, DictClient, IdListKeySchema, IdListMetadata, DEFAULT_WORKER_CLAIM_TTL_S,
    TASK_OP_LOAD, TASK_OP_REMOVE,
};
use super::super::RedisWorkLock;

// Incomplete forward sets must outlive normal retries but eventually self-clean
// if every worker dies before promotion removes the TTL.
pub(super) const PENDING_VERSION_TTL_S: u64 = 24 * 60 * 60;
// Time-based renewal handles downloads; this page bound also prevents a long,
// CPU-heavy scan loop from approaching the claim deadline.
pub(super) const LOCK_RENEWAL_SCAN_PAGES: usize = 512;

#[derive(Clone, Debug)]
pub struct IdListWorkerConfig {
    pub enable_reverse_index: bool,
    pub poll_interval_ms: u64,
    pub lock_ttl_s: u64,
    pub pipeline_batch_size: usize,
    pub scan_count: usize,
    pub diff_partition_max_members: u64,
    pub old_version_ttl_s: u64,
}

impl Default for IdListWorkerConfig {
    fn default() -> Self {
        Self {
            enable_reverse_index: true,
            poll_interval_ms: 2_000,
            lock_ttl_s: DEFAULT_WORKER_CLAIM_TTL_S,
            pipeline_batch_size: 5_000,
            scan_count: 1_000,
            diff_partition_max_members: 2_000_000,
            old_version_ttl_s: 24 * 60 * 60,
        }
    }
}

impl IdListWorkerConfig {
    pub fn from_env() -> Result<Self, String> {
        let defaults = Self::default();
        Ok(Self {
            enable_reverse_index: super::super::reverse_index_enabled_from_env()?,
            poll_interval_ms: env_u64("IDLIST_QUEUE_POLL_INTERVAL_MS", defaults.poll_interval_ms)?,
            lock_ttl_s: env_u64("IDLIST_LOCK_TTL_IN_S", defaults.lock_ttl_s)?,
            pipeline_batch_size: env_usize(
                "IDLIST_PIPELINE_BATCH_SIZE",
                defaults.pipeline_batch_size,
            )?,
            scan_count: env_usize("IDLIST_SCAN_COUNT", defaults.scan_count)?,
            diff_partition_max_members: env_u64(
                "IDLIST_DIFF_PARTITION_MAX_MEMBERS",
                defaults.diff_partition_max_members,
            )?,
            old_version_ttl_s: env_u64("IDLIST_OLD_VERSION_TTL_IN_S", defaults.old_version_ttl_s)?,
        })
        .and_then(|config| {
            if config.poll_interval_ms == 0
                || config.lock_ttl_s < 3
                || config.pipeline_batch_size == 0
                || config.scan_count == 0
                || config.diff_partition_max_members == 0
            {
                Err(
                    "IDLIST worker sizes/intervals must be positive and lock TTL must be >= 3s"
                        .to_string(),
                )
            } else {
                Ok(config)
            }
        })
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| format!("{name} must be a non-negative integer, got '{value}'")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(format!("failed reading {name}: {error}")),
    }
}

fn env_usize(name: &str, default: usize) -> Result<usize, String> {
    env_u64(name, default as u64).and_then(|value| {
        usize::try_from(value).map_err(|_| format!("{name} is too large for this platform"))
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskMetadata {
    pub op: TaskOp,
    pub file_id: String,
    pub creation_time: u64,
    pub url: String,
    pub size: u64,
}

impl TaskMetadata {
    pub(super) fn from_hash(fields: &HashMap<String, String>) -> Result<Self, WorkerError> {
        let op = match fields.get(task_meta_fields::OP).map(String::as_str) {
            Some(TASK_OP_LOAD) => TaskOp::Load,
            Some(TASK_OP_REMOVE) => TaskOp::Remove,
            Some(other) => {
                return Err(WorkerError::InvalidTask(format!(
                    "unsupported task op '{other}'"
                )))
            }
            None => return Err(WorkerError::InvalidTask("task op is missing".to_string())),
        };
        let parse_number = |field: &'static str| -> Result<u64, WorkerError> {
            fields
                .get(field)
                .ok_or_else(|| WorkerError::InvalidTask(format!("{field} is missing")))?
                .parse()
                .map_err(|_| WorkerError::InvalidTask(format!("{field} is not an integer")))
        };
        if op == TaskOp::Remove {
            // B2 intentionally emits minimal remove metadata. Redis state, rather
            // than a stale manifest snapshot, identifies the version to delete.
            return Ok(Self {
                op,
                file_id: fields
                    .get(task_meta_fields::FILE_ID)
                    .cloned()
                    .unwrap_or_default(),
                creation_time: fields
                    .get(task_meta_fields::CREATION_TIME)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_default(),
                url: fields
                    .get(task_meta_fields::URL)
                    .cloned()
                    .unwrap_or_default(),
                size: fields
                    .get(task_meta_fields::SIZE)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_default(),
            });
        }
        Ok(Self {
            op,
            file_id: fields
                .get(task_meta_fields::FILE_ID)
                .cloned()
                .unwrap_or_default(),
            creation_time: parse_number(task_meta_fields::CREATION_TIME)?,
            url: fields
                .get(task_meta_fields::URL)
                .filter(|url| !url.is_empty())
                .cloned()
                .ok_or_else(|| WorkerError::InvalidTask("url is missing".to_string()))?,
            size: parse_number(task_meta_fields::SIZE)?,
        })
    }

    pub(super) fn version_tag(&self) -> String {
        IdListKeySchema::version_tag(&self.as_idlist_metadata())
    }

    pub(super) fn as_idlist_metadata(&self) -> IdListMetadata {
        IdListMetadata {
            file_id: self.file_id.clone(),
            creation_time: self.creation_time,
            url: self.url.clone(),
            size: self.size,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskOp {
    Load,
    Remove,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerAction {
    Delta,
    FullReload,
    Remove,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerRunOutcome {
    Idle,
    Dropped,
    ClaimConflict,
    Completed(WorkerAction),
}

#[derive(Debug)]
pub enum WorkerError {
    Redis(RedisError),
    Http(reqwest::Error),
    InvalidTask(String),
    InvalidFile(String),
    OriginStatus(reqwest::StatusCode),
    RangeIgnored,
    LockLost,
    ScanVerification(String),
    MissingDictionaryToken,
}

impl fmt::Display for WorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redis(error) => write!(f, "Redis error: {error}"),
            Self::Http(error) => {
                write!(
                    f,
                    "origin request failed (timeout={}, connect={}): {error}",
                    error.is_timeout(),
                    error.is_connect()
                )?;
                let mut source = std::error::Error::source(error);
                while let Some(cause) = source {
                    write!(f, ": {cause}")?;
                    source = cause.source();
                }
                Ok(())
            }
            Self::InvalidTask(error) => write!(f, "invalid task: {error}"),
            Self::InvalidFile(error) => write!(f, "invalid idlist file: {error}"),
            Self::OriginStatus(status) => write!(f, "origin returned {status}"),
            Self::RangeIgnored => f.write_str("origin ignored a non-zero Range request"),
            Self::LockLost => f.write_str("idlist claim lock was lost"),
            Self::ScanVerification(error) => write!(f, "SSCAN verification failed: {error}"),
            Self::MissingDictionaryToken => f.write_str("idlist dictionary token is missing"),
        }
    }
}

impl std::error::Error for WorkerError {}

impl From<RedisError> for WorkerError {
    fn from(value: RedisError) -> Self {
        Self::Redis(value)
    }
}

impl From<reqwest::Error> for WorkerError {
    fn from(value: reqwest::Error) -> Self {
        Self::Http(value.without_url())
    }
}

pub struct IdListSyncWorker {
    pub(super) schema: IdListKeySchema,
    pub(super) dict: DictClient,
    pub(super) claim: Arc<RedisWorkLock>,
    pub(super) pool: RedisPool,
    pub(super) http: reqwest::Client,
    pub(super) config: IdListWorkerConfig,
}

impl IdListSyncWorker {
    pub fn new(
        schema: IdListKeySchema,
        claim: Arc<RedisWorkLock>,
        pool: RedisPool,
        http: reqwest::Client,
        config: IdListWorkerConfig,
    ) -> Self {
        let dict = DictClient::new(pool.clone(), schema.clone());
        Self {
            schema,
            dict,
            claim,
            pool,
            http,
            config,
        }
    }
}
