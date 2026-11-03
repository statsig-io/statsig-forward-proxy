//! Company identity discovery for the configured ID-list server keys.
//!
//! Discovery explicitly requests full v2 JSON config specs. Company identity is not
//! an authorization scope: keys in the same company can see different ID lists.

use std::collections::HashSet;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use flate2::read::GzDecoder;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::datastore::config_spec_store::ConfigSpecForCompany;
use crate::datastore::data_providers::background_data_provider::{
    foreground_fetch, BackgroundDataProvider,
};
use crate::datastore::data_providers::http_data_provider::ResponsePayload;
use crate::datastore::data_providers::{
    DataProviderRequestResult, FullRequestContext, ResponseContext,
};
use crate::observers::HttpDataProviderObserverTrait;
use crate::servers::authorized_request_context::{
    AuthorizedRequestContext, AuthorizedRequestContextCache,
};
use crate::servers::normalized_path::NormalizedPath;
use crate::utils::compress_encoder::CompressionEncoder;

pub struct CompanyIdStore {
    server_keys: HashSet<String>,
    company_ids: DashMap<String, String>,
}

impl CompanyIdStore {
    pub fn new(server_keys: impl IntoIterator<Item = String>) -> Self {
        Self {
            server_keys: server_keys.into_iter().collect(),
            company_ids: DashMap::new(),
        }
    }

    pub fn get_company_id(&self, server_key: &str) -> Option<String> {
        self.company_ids.get(server_key).map(|id| id.clone())
    }

    #[cfg(test)]
    pub(crate) fn set_for_test(&self, key: &str, company_id: &str) {
        self.company_ids
            .insert(key.to_string(), company_id.to_string());
    }

    /// Retry missing identities without waiting for an SDK to request config specs.
    /// `sinceTime=0` is required: no-update responses do not contain company_id.
    /// The shared HTTP provider preserves configured upstreams, backoff and timeouts.
    pub fn spawn_discovery(
        self: Arc<Self>,
        runtime: &tokio::runtime::Handle,
        contexts: Arc<AuthorizedRequestContextCache>,
        provider: Arc<BackgroundDataProvider>,
        cancel: CancellationToken,
    ) {
        runtime.spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(30));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        for key in &self.server_keys {
                            if self.get_company_id(key).is_some() {
                                continue;
                            }
                            let context = contexts.get_or_insert(
                                key.clone(),
                                NormalizedPath::V2DownloadConfigSpecs,
                                vec![CompressionEncoder::Gzip],
                                false,
                                false,
                                None,
                            );
                            tokio::select! {
                                _ = cancel.cancelled() => return,
                                _ = foreground_fetch(Arc::clone(&provider), &context, 0, true) => {}
                            }
                        }
                    }
                }
            }
        });
    }
}

#[derive(Deserialize)]
struct ConfigIdentity {
    #[serde(default)]
    has_updates: bool,
    company_id: Option<String>,
}

fn parse_company_id(payload: &ResponsePayload) -> Option<String> {
    if payload.use_proto {
        return None; // Discovery always requests JSON; ignore other clients' protobuf responses.
    }
    let mut decoded = Vec::new();
    let bytes = payload.data.as_ref().as_ref();
    match payload.encoding.as_ref() {
        CompressionEncoder::PlainText | CompressionEncoder::Identity => {
            decoded.extend_from_slice(bytes);
        }
        CompressionEncoder::Gzip => {
            GzDecoder::new(bytes).read_to_end(&mut decoded).ok()?;
        }
        CompressionEncoder::Brotli | CompressionEncoder::StatsigBrotli => {
            brotli::Decompressor::new(bytes, 4096)
                .read_to_end(&mut decoded)
                .ok()?;
        }
        _ => return None,
    }
    let identity: ConfigIdentity = serde_json::from_slice(&decoded).ok()?;
    identity
        .company_id
        .filter(|id| identity.has_updates && !id.trim().is_empty())
}

#[async_trait]
impl HttpDataProviderObserverTrait for CompanyIdStore {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        true
    }

    async fn update(&self, request: &Arc<FullRequestContext>, response: &Arc<ResponseContext>) {
        let context = &request.authorized_request_context;
        if !self.server_keys.contains(&context.sdk_key)
            || !matches!(
                context.path,
                NormalizedPath::V1DownloadConfigSpecs | NormalizedPath::V2DownloadConfigSpecs
            )
        {
            return;
        }
        match response.result_type {
            DataProviderRequestResult::Unauthorized => {
                self.company_ids.remove(&context.sdk_key);
            }
            DataProviderRequestResult::DataAvailable => {
                if let Some(company_id) = parse_company_id(&response.body) {
                    self.company_ids.insert(context.sdk_key.clone(), company_id);
                }
            }
            _ => {} // No-update responses and transient failures preserve a known identity.
        }
    }

    async fn get(&self, _: &Arc<AuthorizedRequestContext>) -> Option<Arc<ConfigSpecForCompany>> {
        None
    }
}

#[cfg(test)]
mod tests;
