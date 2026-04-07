use std::{env, fmt, sync::Arc};

use serde::Deserialize;
use serde_json::Value;

use crate::{
    datastore::sdk_key_store::SdkKeyStoreItem,
    servers::{
        authorized_request_context::AuthorizedRequestContext, normalized_path::NormalizedPath,
    },
    utils::compress_encoder::{CompressionEncoder, ParsedAcceptEncoding},
};

pub const STARTUP_WARMUP_KEYS_ENV: &str = "SFP_STARTUP_WARMUP_KEYS_JSON";
const VALID_STARTUP_WARMUP_SDK_KEY_PREFIXES: [&str; 3] = ["client-", "secret-", "server-"];

#[derive(Debug, Deserialize)]
struct StartupWarmupKeyConfig {
    sdk_key: String,
    path: String,
    encodings: Option<Vec<String>>,
}

#[derive(Debug)]
pub struct StartupWarmupParseResult {
    pub configured_count: usize,
    pub invalid_count: usize,
    pub store_items: Vec<SdkKeyStoreItem>,
}

#[derive(Debug)]
pub enum StartupWarmupParseError {
    InvalidUtf8Env,
    InvalidJson(serde_json::Error),
    EmptySdkKey,
    InvalidSdkKeyPrefix,
    UnsupportedPath(String),
    InvalidEncodings(String),
}

impl fmt::Display for StartupWarmupParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartupWarmupParseError::InvalidUtf8Env => {
                write!(f, "{STARTUP_WARMUP_KEYS_ENV} is not valid UTF-8")
            }
            StartupWarmupParseError::InvalidJson(err) => {
                write!(f, "invalid JSON for {STARTUP_WARMUP_KEYS_ENV}: {err}")
            }
            StartupWarmupParseError::EmptySdkKey => write!(f, "sdk_key cannot be empty"),
            StartupWarmupParseError::InvalidSdkKeyPrefix => {
                write!(
                    f,
                    "sdk_key must start with one of: {}",
                    VALID_STARTUP_WARMUP_SDK_KEY_PREFIXES.join(", ")
                )
            }
            StartupWarmupParseError::UnsupportedPath(path) => {
                write!(f, "unsupported path `{path}`")
            }
            StartupWarmupParseError::InvalidEncodings(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for StartupWarmupParseError {}

fn parse_startup_warmup_path(path: &str) -> Result<NormalizedPath, StartupWarmupParseError> {
    let raw_path = path.trim().trim_end_matches('/');
    let parsed = NormalizedPath::from(raw_path);

    if matches!(
        parsed,
        NormalizedPath::V1DownloadConfigSpecs
            | NormalizedPath::V2DownloadConfigSpecs
            | NormalizedPath::V1GetIdLists
    ) {
        Ok(parsed)
    } else {
        Err(StartupWarmupParseError::UnsupportedPath(path.to_string()))
    }
}

fn parse_startup_warmup_encodings(
    encodings: Option<Vec<String>>,
) -> Result<Vec<CompressionEncoder>, StartupWarmupParseError> {
    let Some(encodings) = encodings else {
        return Ok(Vec::new());
    };

    if encodings.is_empty() {
        return Err(StartupWarmupParseError::InvalidEncodings(
            "encodings cannot be empty".into(),
        ));
    }

    Ok(ParsedAcceptEncoding::from_raw_value(&encodings.join(",")).acceptable_encodings())
}

fn validate_startup_warmup_sdk_key(sdk_key: &str) -> Result<(), StartupWarmupParseError> {
    if sdk_key.is_empty() {
        return Err(StartupWarmupParseError::EmptySdkKey);
    }

    if !VALID_STARTUP_WARMUP_SDK_KEY_PREFIXES
        .iter()
        .any(|prefix| sdk_key.starts_with(prefix))
    {
        return Err(StartupWarmupParseError::InvalidSdkKeyPrefix);
    }

    Ok(())
}

fn parse_startup_warmup_item(
    config: StartupWarmupKeyConfig,
) -> Result<SdkKeyStoreItem, StartupWarmupParseError> {
    let sdk_key = config.sdk_key.trim().to_string();
    validate_startup_warmup_sdk_key(&sdk_key)?;

    let path = parse_startup_warmup_path(&config.path)?;
    let encodings = parse_startup_warmup_encodings(config.encodings)?;
    // Reuse current request-context normalization so warm-up entries align with live cache keys.
    let request_context = Arc::new(AuthorizedRequestContext::new(sdk_key, path, encodings));

    Ok(SdkKeyStoreItem {
        request_context,
        lcut: 0,
    })
}

fn parse_startup_warmup_keys_json(
    raw: &str,
) -> Result<StartupWarmupParseResult, StartupWarmupParseError> {
    // Deserialize as raw values first so one invalid element does not discard the rest.
    let raw_values: Vec<Value> =
        serde_json::from_str(raw).map_err(StartupWarmupParseError::InvalidJson)?;
    let configured_count = raw_values.len();
    let mut invalid_count = 0;
    let mut store_items = Vec::with_capacity(configured_count);

    for (index, value) in raw_values.into_iter().enumerate() {
        let config = match serde_json::from_value::<StartupWarmupKeyConfig>(value) {
            Ok(config) => config,
            Err(err) => {
                invalid_count += 1;
                eprintln!(
                    "[SFP] Ignoring startup warm-up entry at index {index}: schema invalid: {err}"
                );
                continue;
            }
        };

        match parse_startup_warmup_item(config) {
            Ok(item) => store_items.push(item),
            Err(err) => {
                invalid_count += 1;
                eprintln!("[SFP] Ignoring startup warm-up entry at index {index}: {err}");
            }
        }
    }

    Ok(StartupWarmupParseResult {
        configured_count,
        invalid_count,
        store_items,
    })
}

pub fn load_startup_warmup_keys_from_env(
) -> Result<Option<StartupWarmupParseResult>, StartupWarmupParseError> {
    let raw = match env::var(STARTUP_WARMUP_KEYS_ENV) {
        Ok(raw) => raw,
        Err(env::VarError::NotPresent) => return Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(StartupWarmupParseError::InvalidUtf8Env);
        }
    };

    if raw.trim().is_empty() {
        return Ok(None);
    }

    parse_startup_warmup_keys_json(&raw).map(Some)
}

#[cfg(test)]
mod tests {
    use super::parse_startup_warmup_keys_json;
    use crate::{
        servers::normalized_path::NormalizedPath, utils::compress_encoder::CompressionEncoder,
    };

    #[test]
    fn parse_startup_warmup_keys_json_parses_valid_entries() {
        let raw = r#"
        [
          {
            "sdk_key": "client-a",
            "path": "/v1/download_config_specs"
          },
          {
            "sdk_key": "client-b",
            "path": "/v2/download_config_specs",
            "encodings": ["gzip"]
          }
        ]
        "#;

        let parsed = parse_startup_warmup_keys_json(raw).expect("Expected valid warm-up config");
        assert_eq!(parsed.configured_count, 2);
        assert_eq!(parsed.invalid_count, 0);
        assert_eq!(parsed.store_items.len(), 2);

        let plain_item = parsed
            .store_items
            .iter()
            .find(|item| item.request_context.path == NormalizedPath::V1DownloadConfigSpecs)
            .expect("Expected /v1/download_config_specs entry");
        assert_eq!(plain_item.request_context.sdk_key, "client-a");
        assert_eq!(
            plain_item.request_context.encodings,
            vec![CompressionEncoder::PlainText]
        );
        assert_eq!(plain_item.lcut, 0);

        let gzip_item = parsed
            .store_items
            .iter()
            .find(|item| item.request_context.path == NormalizedPath::V2DownloadConfigSpecs)
            .expect("Expected /v2/download_config_specs entry");
        assert_eq!(gzip_item.request_context.sdk_key, "client-b");
        assert_eq!(
            gzip_item.request_context.encodings,
            vec![CompressionEncoder::Gzip]
        );
        assert_eq!(gzip_item.lcut, 0);
    }

    #[test]
    fn parse_startup_warmup_keys_json_accepts_current_statsig_sdk_key_prefixes() {
        let raw = r#"
        [
          {
            "sdk_key": "client-key",
            "path": "/v1/download_config_specs"
          },
          {
            "sdk_key": "secret-key",
            "path": "/v2/download_config_specs"
          },
          {
            "sdk_key": "server-key",
            "path": "/v1/get_id_lists"
          }
        ]
        "#;

        let parsed = parse_startup_warmup_keys_json(raw).expect("Expected parsed result");

        assert_eq!(parsed.configured_count, 3);
        assert_eq!(parsed.invalid_count, 0);
        assert_eq!(parsed.store_items.len(), 3);
        assert_eq!(parsed.store_items[0].request_context.sdk_key, "client-key");
        assert_eq!(parsed.store_items[1].request_context.sdk_key, "secret-key");
        assert_eq!(parsed.store_items[2].request_context.sdk_key, "server-key");
    }

    #[test]
    fn parse_startup_warmup_keys_json_applies_current_encoding_normalization() {
        let raw = r#"
        [
          {
            "sdk_key": "client-a",
            "path": "/v1/download_config_specs",
            "encodings": ["br"]
          },
          {
            "sdk_key": "client-b",
            "path": "/v1/get_id_lists",
            "encodings": ["statsig-br", "gzip"]
          }
        ]
        "#;

        let parsed = parse_startup_warmup_keys_json(raw).expect("Expected parsed result");
        assert_eq!(parsed.configured_count, 2);
        assert_eq!(parsed.invalid_count, 0);
        assert_eq!(parsed.store_items.len(), 2);
        assert_eq!(
            parsed.store_items[0].request_context.encodings,
            vec![CompressionEncoder::PlainText]
        );
        assert_eq!(
            parsed.store_items[1].request_context.encodings,
            vec![CompressionEncoder::StatsigBrotli, CompressionEncoder::Gzip]
        );
    }

    #[test]
    fn parse_startup_warmup_keys_json_skips_invalid_entries() {
        let raw = r#"
        [
          {
            "sdk_key": "",
            "path": "/v1/download_config_specs"
          },
          {
            "sdk_key": "client-a",
            "path": "/v1/unknown"
          },
          {
            "sdk_key": "client-b",
            "path": "/v2/download_config_specs",
            "encodings": []
          },
          {
            "sdk_key": "client-c",
            "path": "/v1/get_id_lists"
          }
        ]
        "#;

        let parsed = parse_startup_warmup_keys_json(raw).expect("Expected parsed result");
        assert_eq!(parsed.configured_count, 4);
        assert_eq!(parsed.invalid_count, 3);
        assert_eq!(parsed.store_items.len(), 1);
        assert_eq!(parsed.store_items[0].request_context.sdk_key, "client-c");
        assert_eq!(
            parsed.store_items[0].request_context.path,
            NormalizedPath::V1GetIdLists
        );
        assert_eq!(
            parsed.store_items[0].request_context.encodings,
            vec![CompressionEncoder::PlainText]
        );
    }

    #[test]
    fn parse_startup_warmup_keys_json_skips_entries_with_invalid_sdk_key_prefixes() {
        let raw = r#"
        [
          {
            "sdk_key": "invalid-key",
            "path": "/v1/download_config_specs"
          },
          {
            "sdk_key": "not-a-client-secret-or-server-key",
            "path": "/v2/download_config_specs"
          },
          {
            "sdk_key": " server-key ",
            "path": "/v1/get_id_lists"
          }
        ]
        "#;

        let parsed = parse_startup_warmup_keys_json(raw).expect("Expected parsed result");

        assert_eq!(parsed.configured_count, 3);
        assert_eq!(parsed.invalid_count, 2);
        assert_eq!(parsed.store_items.len(), 1);
        assert_eq!(parsed.store_items[0].request_context.sdk_key, "server-key");
    }

    #[test]
    fn parse_startup_warmup_keys_json_skips_schema_invalid_entries_without_dropping_valid_ones() {
        let raw = r#"
        [
          {
            "sdk_key": "client-a",
            "path": "/v1/download_config_specs"
          },
          {
            "sdk_key": "client-b"
          },
          {
            "sdk_key": "client-c",
            "path": 123
          },
          {
            "sdk_key": "client-d",
            "path": "/v2/download_config_specs"
          }
        ]
        "#;

        let parsed = parse_startup_warmup_keys_json(raw).expect("Expected parsed result");
        assert_eq!(parsed.configured_count, 4);
        assert_eq!(parsed.invalid_count, 2);
        assert_eq!(parsed.store_items.len(), 2);
        assert_eq!(parsed.store_items[0].request_context.sdk_key, "client-a");
        assert_eq!(parsed.store_items[1].request_context.sdk_key, "client-d");
    }
}
