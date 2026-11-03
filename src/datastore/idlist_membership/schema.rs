use base64::{prelude::BASE64_STANDARD, Engine};
use bb8_redis::redis::Script;
use bb8_redis::redis::{AsyncCommands, RedisError};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use crate::datastore::caching::redis_pool::{apply_key_prefix, pool_get_err, RedisPool};
use crate::datastore::id_list_manifest::IdListManifestEntry;

/// Metadata describing a single id-list version. Used by [`IdListKeySchema::version_tag`]
/// and by the sync producer/worker (B2/B3) when populating `task_meta`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdListMetadata {
    pub file_id: String,
    pub creation_time: u64,
    pub url: String,
    pub size: u64,
}

impl From<&IdListManifestEntry> for IdListMetadata {
    fn from(entry: &IdListManifestEntry) -> Self {
        Self {
            file_id: entry.file_id.clone(),
            creation_time: entry.creation_time,
            url: entry.url.clone(),
            size: entry.size,
        }
    }
}

/// Hashes an SDK / server key the same way `RedisCache::hash_key` does (SHA-256 → base64).
///
/// Used for per-key authentication metrics. ID-list Redis storage uses
/// [`hash_company_id`] instead; sharing a namespace does not share authorization.
pub fn hash_server_key(sdk_key: &str) -> String {
    BASE64_STANDARD.encode(Sha256::digest(sdk_key))
}

/// Redis ID-list namespace: the first four standard-base64 characters of SHA-256
/// of the company ID (24 bits). All pods sharing Redis must use this same format.
/// Authentication and per-key metrics continue to use the server key separately.
pub fn hash_company_id(company_id: &str) -> String {
    BASE64_STANDARD.encode(Sha256::digest(company_id))[..4].to_owned()
}

// ---------------------------------------------------------------------------
// State-hash field names (single source of truth)
// ---------------------------------------------------------------------------

/// Field names for the per-list sync state HASH (`state_key`).
pub mod state_fields {
    pub const VERSION: &str = "version";
    pub const FILE_ID: &str = "file_id";
    pub const URL: &str = "url";
    pub const CREATION_TIME: &str = "creation_time";
    pub const SIZE: &str = "size";
    /// Member count at the last checkpoint; zero distinguishes a valid empty SET.
    pub const CARDINALITY: &str = "cardinality";
    pub const LOADING_CARDINALITY: &str = "loading_cardinality";
    /// Serving mutations cannot be recovered by diffing the committed forward SET.
    pub const RECONCILE_REQUIRED: &str = "reconcile_required";
    pub const LOADING_VERSION: &str = "loading_version";
    pub const LOADING_SIZE: &str = "loading_size";
    /// Streamed drain-diff resume cursor (rotation takeover resumes, not restarts).
    pub const DIFF_PARTITION: &str = "diff_partition";
    /// Removal fan-out completed; retries may finish final key cleanup if UNLINK already ran.
    pub const REMOVAL_FANOUT_COMPLETE: &str = "removal_fanout_complete";
}

/// Field names for the per-task metadata HASH (`task_meta_key`).
pub mod task_meta_fields {
    pub const OP: &str = "op";
    pub const URL: &str = "url";
    pub const FILE_ID: &str = "file_id";
    pub const CREATION_TIME: &str = "creation_time";
    pub const SIZE: &str = "size";
}

/// Values for the `task_meta` `op` field.
pub const TASK_OP_LOAD: &str = "load";
pub const TASK_OP_REMOVE: &str = "remove";

/// Default per-list worker-claim lock TTL (`IDLIST_LOCK_TTL_IN_S`); renewed every ttl/3.
pub const DEFAULT_WORKER_CLAIM_TTL_S: u64 = 60;

/// Parsed view of the per-list sync state HASH.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ListSyncState {
    pub reverse_index_disabled: bool,
    pub version: String,
    pub file_id: String,
    pub url: String,
    pub creation_time: u64,
    pub size: u64,
    pub cardinality: Option<u64>,
    pub loading_cardinality: Option<u64>,
    pub reconcile_required: bool,
    pub loading_version: Option<String>,
    pub loading_size: Option<u64>,
    pub diff_partition: Option<u32>,
}

impl ListSyncState {
    /// Serializes to `(field, value)` pairs for `HSET`. Only populated optionals are emitted.
    pub fn to_hash_fields(&self) -> Vec<(&'static str, String)> {
        let mut fields = vec![
            (state_fields::VERSION, self.version.clone()),
            (state_fields::FILE_ID, self.file_id.clone()),
            (state_fields::URL, self.url.clone()),
            (state_fields::CREATION_TIME, self.creation_time.to_string()),
            (state_fields::SIZE, self.size.to_string()),
        ];
        if self.reverse_index_disabled {
            fields.push((super::REVERSE_INDEX_DISABLED, "1".to_string()));
        }
        if let Some(v) = self.cardinality {
            fields.push((state_fields::CARDINALITY, v.to_string()));
        }
        if let Some(v) = self.loading_cardinality {
            fields.push((state_fields::LOADING_CARDINALITY, v.to_string()));
        }
        if self.reconcile_required {
            fields.push((state_fields::RECONCILE_REQUIRED, "1".to_string()));
        }
        if let Some(ref v) = self.loading_version {
            fields.push((state_fields::LOADING_VERSION, v.clone()));
        }
        if let Some(v) = self.loading_size {
            fields.push((state_fields::LOADING_SIZE, v.to_string()));
        }
        if let Some(v) = self.diff_partition {
            fields.push((state_fields::DIFF_PARTITION, v.to_string()));
        }
        fields
    }

    /// Reconstructs from an `HGETALL` map. Missing/malformed fields fall back to defaults.
    pub fn from_hash_fields(fields: &HashMap<String, String>) -> Self {
        Self {
            reverse_index_disabled: fields.contains_key(super::REVERSE_INDEX_DISABLED),
            version: fields
                .get(state_fields::VERSION)
                .cloned()
                .unwrap_or_default(),
            file_id: fields
                .get(state_fields::FILE_ID)
                .cloned()
                .unwrap_or_default(),
            url: fields.get(state_fields::URL).cloned().unwrap_or_default(),
            creation_time: fields
                .get(state_fields::CREATION_TIME)
                .and_then(|v| v.parse().ok())
                .unwrap_or_default(),
            size: fields
                .get(state_fields::SIZE)
                .and_then(|v| v.parse().ok())
                .unwrap_or_default(),
            cardinality: fields
                .get(state_fields::CARDINALITY)
                .and_then(|v| v.parse().ok()),
            loading_cardinality: fields
                .get(state_fields::LOADING_CARDINALITY)
                .and_then(|v| v.parse().ok()),
            reconcile_required: fields.contains_key(state_fields::RECONCILE_REQUIRED),
            loading_version: fields.get(state_fields::LOADING_VERSION).cloned(),
            loading_size: fields
                .get(state_fields::LOADING_SIZE)
                .and_then(|v| v.parse().ok()),
            diff_partition: fields
                .get(state_fields::DIFF_PARTITION)
                .and_then(|v| v.parse().ok()),
        }
    }
}

// ---------------------------------------------------------------------------
// Component escaping (list names may contain arbitrary characters)
// ---------------------------------------------------------------------------

/// Percent-encodes `%` and `:` so the `::` delimiters in keys/tasks are unambiguous.
///
/// `%` is escaped first (as `%25`) so decoding is lossless. `:` becomes `%3A`, which means a
/// literal `::` inside a list name can never be mistaken for a structural delimiter.
pub fn escape_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '%' => out.push_str("%25"),
            ':' => out.push_str("%3A"),
            c => out.push(c),
        }
    }
    out
}

/// Reverses [`escape_component`]. Returns `None` on malformed escape sequences.
pub fn unescape_component(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            match hex.as_str() {
                "25" => out.push('%'),
                "3A" => out.push(':'),
                _ => return None,
            }
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Key schema
// ---------------------------------------------------------------------------

/// Single source of truth for all idlist reverse-index Redis key construction.
///
/// The historically named `skh` arguments carry the company-ID hash. The worker
/// treats this namespace as opaque; queue and key layouts are unchanged.
///
/// Cheap to clone (holds only the composed `REDIS_KEY_PREFIX`).
#[derive(Clone, Debug)]
pub struct IdListKeySchema {
    key_prefix: String,
}

impl IdListKeySchema {
    pub fn new(key_prefix: impl Into<String>) -> Self {
        Self {
            key_prefix: key_prefix.into(),
        }
    }

    fn prefixed(&self, key: String) -> String {
        apply_key_prefix(&self.key_prefix, key)
    }

    /// `idlist::dict::{skh}` — name↔token dictionary (HASH).
    pub fn dict_key(&self, skh: &str) -> String {
        self.prefixed(format!("idlist::dict::{skh}"))
    }

    /// `i:{skh}{lookup_id}` — reverse-index member set (SET of int tokens).
    /// Company namespaces are four characters; member hashes are eight characters.
    ///
    /// This is the read-path key: one SMEMBERS answers "which lists does this identifier belong to".
    pub fn member_key(&self, skh: &str, lookup_id: &str) -> String {
        self.prefixed(format!("i:{skh}{}", escape_component(lookup_id)))
    }

    /// `idlist::fwd::{{skh}::{list}}::{ver}` — forward SET for diffing and forward-only reads.
    ///
    /// The `{skh::list}` hash tag co-locates old/new versions of a list in the same Redis Cluster
    /// slot. Partitioned diffing uses `SSCAN` and does not require co-location; the
    /// existing hash-tag layout is retained for key compatibility.
    pub fn fwd_key(&self, skh: &str, list: &str, ver: &str) -> String {
        self.prefixed(format!(
            "idlist::fwd::{}::{}",
            self.fwd_hash_tag(skh, list),
            escape_component(ver)
        ))
    }

    /// The Redis Cluster hash tag shared by all versions of one list: `{{skh}::{list}}`.
    pub fn fwd_hash_tag(&self, skh: &str, list: &str) -> String {
        format!("{{{skh}::{}}}", escape_component(list))
    }

    /// `idlist::{skh}::{list}::state` — per-list sync state (HASH).
    pub fn state_key(&self, skh: &str, list: &str) -> String {
        self.prefixed(format!("idlist::{skh}::{}::state", escape_component(list)))
    }

    /// `idlist::claim::{skh}::{list}` — per-list worker-claim lock (STRING).
    pub fn claim_key(&self, skh: &str, list: &str) -> String {
        self.prefixed(format!("idlist::claim::{skh}::{}", escape_component(list)))
    }

    /// `idlist::queue::pending` — ordered, deduped task queue (ZSET).
    pub fn queue_key(&self) -> String {
        self.prefixed("idlist::queue::pending".to_string())
    }

    /// `idlist::task_meta::{skh}::{list}` — per-task metadata (HASH), refreshed every tick.
    pub fn task_meta_key(&self, skh: &str, list: &str) -> String {
        self.prefixed(format!(
            "idlist::task_meta::{skh}::{}",
            escape_component(list)
        ))
    }

    /// `idlist::{skh}::__all` — set of all known list names for a server key.
    pub fn all_lists_key(&self, skh: &str) -> String {
        self.prefixed(format!("idlist::{skh}::__all"))
    }

    /// Number of lists in the last fully synchronized manifest; absent means not ready.
    pub fn ready_key(&self, skh: &str) -> String {
        self.prefixed(format!("idlist::{skh}::__ready"))
    }

    /// `idlist::producer_lock` — the per-tick producer lock (STRING); one holder across all pods.
    pub fn producer_lock_key(&self) -> String {
        self.prefixed("idlist::producer_lock".to_string())
    }

    /// Identifies the snapshot when a full load starts. Appends keep the committed
    /// version even when creation_time advances; file_id identifies the origin file.
    pub fn version_tag(meta: &IdListMetadata) -> String {
        if meta.file_id.is_empty() {
            meta.creation_time.to_string()
        } else {
            format!("{}@{}", meta.file_id, meta.creation_time)
        }
    }

    /// Encodes a queue task member: `"{skh}::{list}"` — identity only (for `ZADD NX` dedup).
    /// The list component is escaped so arbitrary list names round-trip.
    pub fn encode_task(skh: &str, list: &str) -> String {
        format!("{skh}::{}", escape_component(list))
    }

    /// Decodes a queue task member into `(skh, list)`. Returns `None` if malformed.
    pub fn decode_task(member: &str) -> Option<(String, String)> {
        let (skh, rest) = member.split_once("::")?;
        let list = unescape_component(rest)?;
        Some((skh.to_string(), list))
    }
}

// ---------------------------------------------------------------------------
// Dictionary client (list name ↔ int token)
// ---------------------------------------------------------------------------

const DICT_COUNTER_FIELD: &str = "__counter";
const DICT_NAME_PREFIX: &str = "n:";
const DICT_TOKEN_PREFIX: &str = "t:";

/// In-memory snapshot of the name↔token dictionary (for the read path, refreshed periodically).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DictSnapshot {
    pub name_to_token: HashMap<String, u64>,
    pub token_to_name: HashMap<u64, String>,
}

/// Redis-backed dictionary allocating compact integer tokens for list names.
///
/// Member sets store tokens (1–2 bytes) rather than list-name strings, keeping billion-member
/// SETs listpack/intset-encoded.
pub struct DictClient {
    pool: RedisPool,
    schema: IdListKeySchema,
}

/// Atomically allocate a token: return the existing token if present, else bump `__counter`
/// and write both `n:{name} → id` and `t:{id} → name`. Runs as a single Lua script so
/// concurrent allocations of the same new name always converge on one token.
static ALLOC_TOKEN_SCRIPT: &str = r#"
local dict_key = KEYS[1]
local name_field = ARGV[1]
local list_name = ARGV[2]
local counter_field = ARGV[3]

local existing = redis.call('HGET', dict_key, name_field)
if existing then
    return existing
end

local token = redis.call('HINCRBY', dict_key, counter_field, 1)
redis.call('HSET', dict_key, name_field, token, 't:' .. token, list_name)
return tostring(token)
"#;

impl DictClient {
    pub fn new(pool: RedisPool, schema: IdListKeySchema) -> Self {
        Self { pool, schema }
    }

    pub(crate) fn name_field(list_name: &str) -> String {
        format!("{DICT_NAME_PREFIX}{list_name}")
    }

    pub(crate) fn token_field(token: u64) -> String {
        format!("{DICT_TOKEN_PREFIX}{token}")
    }

    /// Returns the existing token for `list_name`, or atomically allocates a new one.
    pub async fn get_or_allocate_token(
        &self,
        skh: &str,
        list_name: &str,
    ) -> Result<u64, RedisError> {
        let dict_key = self.schema.dict_key(skh);
        let name_field = Self::name_field(list_name);

        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let token: String = Script::new(ALLOC_TOKEN_SCRIPT)
            .key(&dict_key)
            .arg(&name_field)
            .arg(list_name)
            .arg(DICT_COUNTER_FIELD)
            .invoke_async(&mut *conn)
            .await?;
        token.parse().map_err(|_| {
            RedisError::from((
                bb8_redis::redis::ErrorKind::TypeError,
                "dictionary token was not an integer",
            ))
        })
    }

    /// Returns the existing token without allocating one. Removal must use this so a
    /// corrupted/missing dictionary entry cannot silently create a new token whose
    /// reverse-index memberships were never written.
    pub async fn get_token(&self, skh: &str, list_name: &str) -> Result<Option<u64>, RedisError> {
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let value: Option<String> = conn
            .hget(self.schema.dict_key(skh), Self::name_field(list_name))
            .await?;
        value
            .map(|token| {
                token.parse().map_err(|_| {
                    RedisError::from((
                        bb8_redis::redis::ErrorKind::TypeError,
                        "dictionary token was not an integer",
                    ))
                })
            })
            .transpose()
    }

    /// Loads the full dictionary into memory for read-side token→name / name→token translation.
    pub async fn snapshot(&self, skh: &str) -> Result<DictSnapshot, RedisError> {
        let dict_key = self.schema.dict_key(skh);
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let fields: HashMap<String, String> = conn.hgetall(&dict_key).await?;

        let mut snap = DictSnapshot::default();
        for (field, value) in fields {
            if let Some(name) = field.strip_prefix(DICT_NAME_PREFIX) {
                if let Ok(token) = value.parse::<u64>() {
                    snap.name_to_token.insert(name.to_string(), token);
                }
            } else if let Some(token_str) = field.strip_prefix(DICT_TOKEN_PREFIX) {
                if let Ok(token) = token_str.parse::<u64>() {
                    snap.token_to_name.insert(token, value);
                }
            }
        }
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_server_key_is_deterministic_and_matches_redis_cache_scheme() {
        let h1 = hash_server_key("secret-test-key");
        let h2 = hash_server_key("secret-test-key");
        assert_eq!(h1, h2);
        assert_ne!(h1, hash_server_key("other-key"));
        // SHA-256 → base64 std is always 44 chars.
        assert_eq!(h1.len(), 44);
    }

    #[test]
    fn company_namespace_is_four_standard_base64_characters() {
        assert_eq!(hash_company_id("5Dj0kAs2vYQrpRKkfqm5pD"), "yrb1");
        for id in ["company", "other-company", "", "会社"] {
            let namespace = hash_company_id(id);
            assert_eq!(namespace.len(), 4);
            assert_eq!(namespace, &hash_server_key(id)[..4]);
        }
    }

    #[test]
    fn reverse_key_concatenates_fixed_width_namespace_and_member_hash() {
        let schema = IdListKeySchema::new("");
        let namespace = hash_company_id("5Dj0kAs2vYQrpRKkfqm5pD");
        let key = schema.member_key(&namespace, "Ab+/1234");
        assert_eq!(key, "i:yrb1Ab+/1234");
        assert_eq!(key.len(), 14);
        assert_eq!(schema.member_key(&namespace, ""), "i:yrb1");
        assert!(!schema.member_key("yrb2", "Ab+/1234").starts_with("i:yrb1"));
    }

    #[test]
    fn keys_have_no_prefix_by_default() {
        let schema = IdListKeySchema::new("");
        assert_eq!(schema.queue_key(), "idlist::queue::pending");
        assert_eq!(schema.dict_key("abc"), "idlist::dict::abc");
        assert_eq!(schema.all_lists_key("abc"), "idlist::abc::__all");
        assert_eq!(schema.producer_lock_key(), "idlist::producer_lock");
    }

    #[test]
    fn key_prefix_is_applied() {
        let schema = IdListKeySchema::new("sfp-np");
        assert_eq!(schema.queue_key(), "sfp-np:idlist::queue::pending");
        assert_eq!(schema.dict_key("abc"), "sfp-np:idlist::dict::abc");
        assert_eq!(schema.member_key("abc", "hash1"), "sfp-np:i:abchash1");
        assert_eq!(
            schema.task_meta_key("abc", "list1"),
            "sfp-np:idlist::task_meta::abc::list1"
        );
    }

    #[test]
    fn key_prefix_with_trailing_colon_is_not_doubled() {
        let schema = IdListKeySchema::new("sfp-np:");
        assert_eq!(schema.queue_key(), "sfp-np:idlist::queue::pending");
    }

    #[test]
    fn fwd_key_hash_tag_co_locates_versions() {
        let schema = IdListKeySchema::new("");
        let (skh, list) = ("hashed_sk", "my_list");
        let tag = schema.fwd_hash_tag(skh, list);
        let v1 = schema.fwd_key(skh, list, "file_v1");
        let v2 = schema.fwd_key(skh, list, "file_v2");
        // Both versions embed the identical hash-tag substring → identical cluster slot.
        assert!(v1.contains(&tag), "{v1} should contain {tag}");
        assert!(v2.contains(&tag), "{v2} should contain {tag}");
        assert_eq!(cluster_tag(&v1), cluster_tag(&v2));
        assert_ne!(v1, v2);
    }

    #[test]
    fn fwd_key_hash_tag_survives_special_list_names() {
        let schema = IdListKeySchema::new("");
        let list = "weird::name/with:chars";
        let tag = schema.fwd_hash_tag("skh", list);
        // No unescaped `::` inside the tag body → tag stays a single, well-formed hash tag.
        assert!(tag.contains("%3A%3A"));
        let key = schema.fwd_key("skh", list, "v1");
        assert!(key.contains(&tag));
        assert_eq!(key.matches('{').count(), 1);
        assert_eq!(key.matches('}').count(), 1);
    }

    /// Extracts the `{...}` cluster hash-tag substring the way Redis Cluster does.
    fn cluster_tag(key: &str) -> Option<&str> {
        let start = key.find('{')?;
        let end = key[start + 1..].find('}')? + start + 1;
        if end > start + 1 {
            Some(&key[start..=end])
        } else {
            None
        }
    }

    #[test]
    fn escape_round_trip() {
        for name in [
            "simple",
            "with::colons",
            "percent%sign",
            "mix::%::all",
            "",
            "unicode-名前",
            "%3A-literal-looks-like-escape",
        ] {
            let escaped = escape_component(name);
            let back = unescape_component(&escaped).expect("round-trip");
            assert_eq!(back, name, "failed for {name:?}");
        }
    }

    #[test]
    fn encode_decode_task_round_trip() {
        for (skh, list) in [
            ("abc123", "my_list"),
            ("abc123", "list::with::colons"),
            ("abc123", "percent%here"),
            ("abc123", ""),
        ] {
            let encoded = IdListKeySchema::encode_task(skh, list);
            let (dec_skh, dec_list) = IdListKeySchema::decode_task(&encoded).expect("decode");
            assert_eq!(dec_skh, skh);
            assert_eq!(dec_list, list);
        }
    }

    #[test]
    fn decode_task_rejects_malformed() {
        assert!(IdListKeySchema::decode_task("no_delimiter").is_none());
        assert!(IdListKeySchema::decode_task("skh::%ZZ").is_none());
    }

    #[test]
    fn version_tag_combines_file_id_and_creation_time() {
        let meta = IdListMetadata {
            file_id: "fid-123".to_string(),
            creation_time: 999,
            url: String::new(),
            size: 0,
        };
        assert_eq!(IdListKeySchema::version_tag(&meta), "fid-123@999");
    }

    #[test]
    fn version_tag_falls_back_to_creation_time() {
        let meta = IdListMetadata {
            file_id: String::new(),
            creation_time: 1772477815000,
            url: String::new(),
            size: 0,
        };
        assert_eq!(IdListKeySchema::version_tag(&meta), "1772477815000");
    }

    #[test]
    fn list_sync_state_round_trip_full() {
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
        let parsed = round_trip_state(&state);
        assert_eq!(parsed, state);
    }

    #[test]
    fn list_sync_state_round_trip_minimal() {
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
        let parsed = round_trip_state(&state);
        assert_eq!(parsed, state);
        // Optional fields absent → not serialized.
        assert!(state
            .to_hash_fields()
            .iter()
            .all(|(k, _)| *k != state_fields::LOADING_VERSION));
    }

    fn round_trip_state(state: &ListSyncState) -> ListSyncState {
        let fields: HashMap<String, String> = state
            .to_hash_fields()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        ListSyncState::from_hash_fields(&fields)
    }

    #[test]
    fn keys_escape_list_name() {
        let schema = IdListKeySchema::new("");
        let list = "a::b";
        assert!(schema.state_key("skh", list).contains("%3A%3A"));
        assert!(schema.claim_key("skh", list).contains("%3A%3A"));
        assert!(schema.task_meta_key("skh", list).contains("%3A%3A"));
        assert!(schema.fwd_key("skh", list, "v").contains("%3A%3A"));
    }

    #[test]
    fn id_list_metadata_from_manifest_entry() {
        let entry = IdListManifestEntry {
            name: "list".to_string(),
            size: 5,
            url: "https://x".to_string(),
            creation_time: 123,
            file_id: "fid".to_string(),
        };
        let meta = IdListMetadata::from(&entry);
        assert_eq!(meta.file_id, "fid");
        assert_eq!(meta.creation_time, 123);
        assert_eq!(meta.size, 5);
        assert_eq!(meta.url, "https://x");
    }
}
