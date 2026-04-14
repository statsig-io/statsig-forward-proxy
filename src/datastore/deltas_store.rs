use super::config_spec_store::ConfigSpecForCompany;
use super::data_providers::background_data_provider::{foreground_fetch, BackgroundDataProvider};
use super::data_providers::http_data_provider::ResponsePayload;
use super::data_providers::{DataProviderRequestResult, FullRequestContext, ResponseContext};
use super::sdk_key_store::SdkKeyStore;
use crate::datatypes::config_specs_proto::statsig_config_specs::{
    SpecsEnvelope, SpecsEnvelopeKind, SpecsTopLevel,
};
use crate::observers::HttpDataProviderObserverTrait;
use crate::servers::authorized_request_context::AuthorizedRequestContext;
use crate::servers::normalized_path::NormalizedPath;
use crate::utils::compress_encoder::CompressionEncoder;
use async_trait::async_trait;
use dashmap::DashMap;
use flate2::read::GzDecoder;
use prost::Message;
use std::collections::VecDeque;
use std::io::Read;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct DeltaCacheEntry {
    pub since_time_requested: u64,
    pub lcut: u64,
    pub top_level: Arc<SpecsTopLevel>,
    pub envelopes: Arc<Vec<SpecsEnvelope>>,
}

#[derive(Debug, PartialEq)]
pub enum DeltaProtoBuildResult {
    SdkKeyNotRegistered,
    NoCachedDeltas,
    SinceTimeTooOld { earliest_since_time: u64 },
    NoUpdates { lcut: u64 },
    Combined { lcut: u64, payload: Vec<u8> },
    SerializationFailed,
}

pub struct DeltasStore {
    store: Arc<DashMap<String, VecDeque<Arc<DeltaCacheEntry>>>>,
    sdk_key_store: Arc<SdkKeyStore>,
    background_data_provider: Arc<BackgroundDataProvider>,
    max_responses_to_persist: usize,
}

impl DeltasStore {
    pub fn new(
        sdk_key_store: Arc<SdkKeyStore>,
        background_data_provider: Arc<BackgroundDataProvider>,
        max_responses_to_persist: usize,
    ) -> Self {
        DeltasStore {
            store: Arc::new(DashMap::new()),
            sdk_key_store,
            background_data_provider,
            max_responses_to_persist: max_responses_to_persist.max(1),
        }
    }

    pub async fn get_deltas(&self, rc: Arc<AuthorizedRequestContext>) -> Vec<Arc<DeltaCacheEntry>> {
        if !self.sdk_key_store.has_key(&rc) {
            self.sdk_key_store
                .upsert(Self::to_delta_request_context(&rc), 0);
            foreground_fetch(Arc::clone(&self.background_data_provider), &rc, 0, false).await;
        }

        self.store
            .get(&rc.sdk_key)
            .map(|records| records.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn has_registered_key(&self, rc: &Arc<AuthorizedRequestContext>) -> bool {
        self.sdk_key_store
            .has_key(&Self::to_delta_request_context(rc))
    }

    pub fn register_sdk_key(&self, rc: &Arc<AuthorizedRequestContext>, since_time: u64) {
        self.sdk_key_store
            .upsert(Self::to_delta_request_context(rc), since_time);
    }

    pub fn get_cached_deltas(
        &self,
        rc: &Arc<AuthorizedRequestContext>,
    ) -> Vec<Arc<DeltaCacheEntry>> {
        self.store
            .get(&rc.sdk_key)
            .map(|records| records.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn build_combined_delta_proto_payload(
        &self,
        rc: &Arc<AuthorizedRequestContext>,
        since_time: u64,
    ) -> DeltaProtoBuildResult {
        let mapped_rc = Self::to_delta_request_context(rc);
        if !self.sdk_key_store.has_key(&mapped_rc) {
            return DeltaProtoBuildResult::SdkKeyNotRegistered;
        }

        let deltas = self.get_cached_deltas(&mapped_rc);
        if deltas.is_empty() {
            return DeltaProtoBuildResult::NoCachedDeltas;
        }

        let earliest_requested_time = deltas
            .iter()
            .map(|d| d.since_time_requested)
            .min()
            .unwrap_or(0);
        let latest_lcut = deltas.last().map(|d| d.lcut).unwrap_or(0);

        if since_time < earliest_requested_time {
            return DeltaProtoBuildResult::SinceTimeTooOld {
                earliest_since_time: earliest_requested_time,
            };
        }

        // Explicitly check for GTE since_time - DO NOT CHANGE. A single LCUT can record multiple
        // versions, which means we do not know what state precisely the client is in when providing
        // a since time.
        // This could probably be improved throught the addition of checksums to improve the average
        // case data streamed - but this is a simple and safe approach to ensure we do not miss updates.
        let selected_deltas: Vec<_> = deltas
            .into_iter()
            .filter(|d| d.lcut >= since_time)
            .collect();

        // Explicitly handle the case of `since_time == latest_lcut` to avoid unnecessary re-streaming
        // of the most recent update continuously. Doing this is not guaranteeed to be 100% correct -
        // but in practice configs change often enough that it will eventually become correct.
        if selected_deltas.is_empty() || since_time == latest_lcut {
            return DeltaProtoBuildResult::NoUpdates { lcut: latest_lcut };
        }

        match Self::assemble_envelopes_in_delta_order(&selected_deltas) {
            Some(payload) => DeltaProtoBuildResult::Combined {
                lcut: latest_lcut,
                payload,
            },
            None => DeltaProtoBuildResult::SerializationFailed,
        }
    }

    fn remove_sdk_key(&self, request_context: &Arc<AuthorizedRequestContext>) {
        self.sdk_key_store.remove_key(request_context);
        self.store.remove(&request_context.sdk_key);
    }

    fn to_delta_request_context(
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> Arc<AuthorizedRequestContext> {
        Arc::new(
            AuthorizedRequestContext::new(
                request_context.sdk_key.clone(),
                NormalizedPath::V2DownloadConfigSpecsDeltas,
                request_context.encodings.clone(),
            )
            .with_request_capabilities(
                request_context.supports_proto,
                request_context.accept_deltas,
            ),
        )
    }

    fn is_meta_envelope(kind: i32) -> bool {
        kind == SpecsEnvelopeKind::Done as i32
            || kind == SpecsEnvelopeKind::CopyPrev as i32
            || kind == SpecsEnvelopeKind::TopLevel as i32
            || kind == SpecsEnvelopeKind::Checksums as i32
    }

    fn assemble_envelopes_in_delta_order(
        selected_deltas: &[Arc<DeltaCacheEntry>],
    ) -> Option<Vec<u8>> {
        if selected_deltas.is_empty() {
            return Some(Vec::new());
        }

        let latest_index = selected_deltas.len() - 1;
        let last_delta = &selected_deltas[latest_index];
        let mut combined_payload = Vec::new();

        let mut write_envelope = |envelope: &SpecsEnvelope| -> Option<()> {
            envelope.encode_length_delimited(&mut combined_payload).ok()
        };

        // 1) Write control envelopes from latest delta, except Checksums and Done.
        for envelope in last_delta.envelopes.iter().filter(|envelope| {
            Self::is_meta_envelope(envelope.kind)
                && envelope.kind != SpecsEnvelopeKind::Checksums as i32
                && envelope.kind != SpecsEnvelopeKind::Done as i32
        }) {
            write_envelope(envelope)?;
        }

        // 2) Then append non-meta envelopes, starting from the earliest deltas.
        for delta in selected_deltas.iter() {
            for envelope in delta
                .envelopes
                .iter()
                .filter(|envelope| !Self::is_meta_envelope(envelope.kind))
            {
                write_envelope(envelope)?;
            }
        }

        // 3) Finally, write Checksums and Done envelope(s) from latest delta.
        for envelope in last_delta.envelopes.iter().filter(|envelope| {
            envelope.kind == SpecsEnvelopeKind::Checksums as i32
                || envelope.kind == SpecsEnvelopeKind::Done as i32
        }) {
            write_envelope(envelope)?;
        }

        Some(combined_payload)
    }

    fn persist_delta(
        &self,
        request_context: &Arc<AuthorizedRequestContext>,
        payload: &Arc<ResponsePayload>,
        since_time_requested: u64,
    ) {
        let parsed_payload = Self::parse_payload_to_protos(payload);
        let (envelopes, top_level) = match parsed_payload {
            Some(value) => value,
            None => return,
        };
        let sdk_key = request_context.sdk_key.clone();

        let lcut = top_level.time;
        let new_entry = Arc::new(DeltaCacheEntry {
            since_time_requested,
            lcut,
            top_level: Arc::new(top_level),
            envelopes: Arc::new(envelopes),
        });

        let mut records = self.store.entry(sdk_key.clone()).or_default();
        let idx = records
            .iter()
            .position(|entry| entry.lcut > lcut)
            .unwrap_or(records.len());

        records.insert(idx, new_entry);
        while records.len() > self.max_responses_to_persist {
            records.pop_front();
        }
    }

    fn parse_payload_to_protos(
        payload: &Arc<ResponsePayload>,
    ) -> Option<(Vec<SpecsEnvelope>, SpecsTopLevel)> {
        if !payload.use_proto {
            return None;
        }

        let decompressed = Self::decompress(payload)?;
        let envelopes = match Self::decode_envelopes(&decompressed) {
            Ok(value) => value,
            Err(e) => {
                eprintln!("Failed to decode delta response into SpecsEnvelope: {e}");
                return None;
            }
        };

        let maybe_top_level = envelopes
            .iter()
            .find(|envelope| envelope.kind == SpecsEnvelopeKind::TopLevel as i32)
            .and_then(|envelope| envelope.data.as_ref())
            .and_then(|data| SpecsTopLevel::decode(data.as_slice()).ok())
            .or_else(|| SpecsTopLevel::decode(decompressed.as_slice()).ok());

        maybe_top_level.map(|top_level| (envelopes, top_level))
    }

    fn decode_envelopes(payload: &[u8]) -> Result<Vec<SpecsEnvelope>, prost::DecodeError> {
        let mut envelopes = Vec::new();
        let mut cursor = payload;
        while !cursor.is_empty() {
            let previous_len = cursor.len();
            match SpecsEnvelope::decode_length_delimited(&mut cursor) {
                Ok(envelope) => envelopes.push(envelope),
                Err(e) => {
                    if envelopes.is_empty() {
                        return Ok(vec![SpecsEnvelope::decode(payload)?]);
                    }
                    return Err(e);
                }
            }

            if cursor.len() == previous_len {
                break;
            }
        }

        Ok(envelopes)
    }

    fn decompress(payload: &Arc<ResponsePayload>) -> Option<Vec<u8>> {
        match payload.encoding.as_ref() {
            CompressionEncoder::Brotli | CompressionEncoder::StatsigBrotli => {
                let mut decoder = brotli::Decompressor::new(payload.data.as_ref().as_ref(), 4096);
                let mut decompressed = Vec::new();
                if decoder.read_to_end(&mut decompressed).is_ok() {
                    Some(decompressed)
                } else {
                    eprintln!("Failed to brotli-decompress delta payload");
                    None
                }
            }
            CompressionEncoder::Gzip => {
                let mut decoder = GzDecoder::new(payload.data.as_ref().as_ref());
                let mut decompressed = Vec::new();
                if decoder.read_to_end(&mut decompressed).is_ok() {
                    Some(decompressed)
                } else {
                    eprintln!("Failed to gzip-decompress delta payload");
                    None
                }
            }
            CompressionEncoder::PlainText => Some(payload.data.to_vec()),
            _ => None,
        }
    }
}

#[async_trait]
impl HttpDataProviderObserverTrait for DeltasStore {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        true
    }

    async fn update(
        &self,
        request_context: &Arc<FullRequestContext>,
        response_context: &Arc<ResponseContext>,
    ) {
        let rc = &request_context.authorized_request_context;
        match rc.path {
            NormalizedPath::V2DownloadConfigSpecs => {
                // Deltas use protos for serialization - so ignore all clients that do not
                // support it
                if !rc.supports_proto || !rc.accept_deltas {
                    return;
                }

                match response_context.result_type {
                    DataProviderRequestResult::DataAvailable => {
                        // This listens to the primary DCS background loop - which means this will be called
                        // repeatedly with new LCUTS. To prevent race conditions between the two stores, early
                        // return to ensure the delta is always processed.
                        let delta_request_context = Self::to_delta_request_context(rc);
                        if self.sdk_key_store.has_key(&delta_request_context) {
                            return;
                        }

                        self.sdk_key_store
                            .upsert(delta_request_context, response_context.lcut);
                    }
                    DataProviderRequestResult::Unauthorized => {
                        let delta_request_context = Self::to_delta_request_context(rc);
                        self.remove_sdk_key(&delta_request_context);
                    }
                    DataProviderRequestResult::ClientError => {
                        let delta_request_context = Self::to_delta_request_context(rc);
                        self.remove_sdk_key(&delta_request_context);
                    }
                    DataProviderRequestResult::Error => {}
                    DataProviderRequestResult::NoDataAvailable => {}
                }
            }
            NormalizedPath::V2DownloadConfigSpecsDeltas => match response_context.result_type {
                DataProviderRequestResult::DataAvailable => {
                    self.persist_delta(
                        rc,
                        &response_context.body,
                        response_context.request_since_time,
                    );
                    self.sdk_key_store.upsert(
                        Arc::clone(&request_context.authorized_request_context),
                        response_context.lcut,
                    );
                }
                DataProviderRequestResult::Unauthorized => {
                    self.remove_sdk_key(rc);
                }
                DataProviderRequestResult::ClientError => {
                    self.remove_sdk_key(rc);
                }
                DataProviderRequestResult::Error => {}
                DataProviderRequestResult::NoDataAvailable => {}
            },
            _ => {}
        }
    }

    async fn get(
        &self,
        _request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<Arc<ConfigSpecForCompany>> {
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datastore::data_providers::background_data_provider::BackgroundDataProvider;
    use crate::datastore::data_providers::http_data_provider::HttpDataProvider;
    use crate::datastore::data_providers::HttpClientConfig;
    use crate::servers::normalized_path::NormalizedPath;

    fn make_store() -> DeltasStore {
        let sdk_store = Arc::new(SdkKeyStore::new());
        let http_provider = Arc::new(HttpDataProvider {});
        let bdp = Arc::new(BackgroundDataProvider::new(
            http_provider,
            1,
            Arc::clone(&sdk_store),
            false,
            HttpClientConfig::default(),
            crate::datastore::data_providers::background_data_provider::BackgroundPollDispatchConfig::new(
                1,
                0,
            ),
        ));
        DeltasStore::new(sdk_store, bdp, 10)
    }

    fn make_rc() -> Arc<AuthorizedRequestContext> {
        Arc::new(
            AuthorizedRequestContext::new(
                "sdk-key-test".to_string(),
                NormalizedPath::V2DownloadConfigSpecsDeltas,
                vec![CompressionEncoder::StatsigBrotli],
            )
            .with_request_capabilities(true, true),
        )
    }

    fn make_envelope(kind: SpecsEnvelopeKind, name: &str) -> SpecsEnvelope {
        SpecsEnvelope {
            kind: kind as i32,
            name: name.to_string(),
            checksum: String::new(),
            data: Some(vec![kind as i32 as u8]),
        }
    }

    fn decode_length_delimited_envelopes(payload: &[u8]) -> Vec<SpecsEnvelope> {
        let mut envelopes = Vec::new();
        let mut cursor = payload;
        while !cursor.is_empty() {
            match SpecsEnvelope::decode_length_delimited(&mut cursor) {
                Ok(envelope) => envelopes.push(envelope),
                Err(e) => panic!("decode_length_delimited failed in test: {e}"),
            }
        }
        envelopes
    }

    #[test]
    fn assemble_envelopes_in_delta_order_returns_empty_payload_for_empty_input() {
        let payload = DeltasStore::assemble_envelopes_in_delta_order(&[])
            .expect("empty input should serialize to an empty payload");
        assert!(payload.is_empty());
    }

    #[test]
    fn assemble_envelopes_in_delta_order_filters_meta_envelopes_from_earlier_deltas() {
        let deltas = vec![
            Arc::new(DeltaCacheEntry {
                since_time_requested: 10,
                lcut: 30,
                top_level: Arc::new(SpecsTopLevel::default()),
                envelopes: Arc::new(vec![
                    make_envelope(SpecsEnvelopeKind::TopLevel, "old-top"),
                    make_envelope(SpecsEnvelopeKind::CopyPrev, "old-copy-prev"),
                    make_envelope(SpecsEnvelopeKind::FeatureGate, "old-data"),
                    make_envelope(SpecsEnvelopeKind::Checksums, "old-checksums"),
                    make_envelope(SpecsEnvelopeKind::Done, "old-done"),
                ]),
            }),
            Arc::new(DeltaCacheEntry {
                since_time_requested: 30,
                lcut: 50,
                top_level: Arc::new(SpecsTopLevel::default()),
                envelopes: Arc::new(vec![
                    make_envelope(SpecsEnvelopeKind::TopLevel, "latest-top"),
                    make_envelope(SpecsEnvelopeKind::CopyPrev, "latest-copy-prev"),
                    make_envelope(SpecsEnvelopeKind::DynamicConfig, "latest-data"),
                    make_envelope(SpecsEnvelopeKind::Checksums, "latest-checksums"),
                    make_envelope(SpecsEnvelopeKind::Done, "latest-done"),
                ]),
            }),
        ];

        let payload = DeltasStore::assemble_envelopes_in_delta_order(&deltas)
            .expect("multiple deltas should serialize");
        let decoded = decode_length_delimited_envelopes(&payload);
        let names: Vec<&str> = decoded.iter().map(|e| e.name.as_str()).collect();

        assert_eq!(
            names,
            vec![
                "latest-top",
                "latest-copy-prev",
                "old-data",
                "latest-data",
                "latest-checksums",
                "latest-done"
            ]
        );
    }

    #[test]
    fn build_combined_delta_proto_payload_returns_key_not_registered_when_key_is_missing() {
        let store = make_store();
        let rc = make_rc();

        let result = store.build_combined_delta_proto_payload(&rc, 5);
        assert_eq!(result, DeltaProtoBuildResult::SdkKeyNotRegistered);
    }

    #[test]
    fn build_combined_delta_proto_payload_returns_since_time_too_old() {
        let store = make_store();
        let rc = make_rc();
        store.sdk_key_store.upsert(Arc::clone(&rc), 0);

        store.store.insert(
            rc.sdk_key.clone(),
            VecDeque::from(vec![Arc::new(DeltaCacheEntry {
                since_time_requested: 10,
                lcut: 30,
                top_level: Arc::new(SpecsTopLevel::default()),
                envelopes: Arc::new(vec![make_envelope(SpecsEnvelopeKind::TopLevel, "t1")]),
            })]),
        );

        let result = store.build_combined_delta_proto_payload(&rc, 5);
        assert_eq!(
            result,
            DeltaProtoBuildResult::SinceTimeTooOld {
                earliest_since_time: 10
            }
        );
    }

    #[test]
    fn build_combined_delta_proto_payload_returns_no_updates_when_selected_deltas_empty() {
        let store = make_store();
        let rc = make_rc();
        store.sdk_key_store.upsert(Arc::clone(&rc), 0);

        store.store.insert(
            rc.sdk_key.clone(),
            VecDeque::from(vec![
                Arc::new(DeltaCacheEntry {
                    since_time_requested: 10,
                    lcut: 30,
                    top_level: Arc::new(SpecsTopLevel::default()),
                    envelopes: Arc::new(vec![make_envelope(SpecsEnvelopeKind::TopLevel, "t1")]),
                }),
                Arc::new(DeltaCacheEntry {
                    since_time_requested: 30,
                    lcut: 50,
                    top_level: Arc::new(SpecsTopLevel::default()),
                    envelopes: Arc::new(vec![make_envelope(SpecsEnvelopeKind::TopLevel, "t2")]),
                }),
            ]),
        );

        let result = store.build_combined_delta_proto_payload(&rc, 50);
        assert_eq!(result, DeltaProtoBuildResult::NoUpdates { lcut: 50 });
    }

    #[test]
    fn build_combined_delta_proto_payload_returns_no_updates_when_since_time_equals_latest_lcut() {
        let store = make_store();
        let rc = make_rc();
        store.sdk_key_store.upsert(Arc::clone(&rc), 0);

        store.store.insert(
            rc.sdk_key.clone(),
            VecDeque::from(vec![
                Arc::new(DeltaCacheEntry {
                    since_time_requested: 10,
                    lcut: 30,
                    top_level: Arc::new(SpecsTopLevel::default()),
                    envelopes: Arc::new(vec![make_envelope(SpecsEnvelopeKind::TopLevel, "t1")]),
                }),
                Arc::new(DeltaCacheEntry {
                    since_time_requested: 30,
                    lcut: 50,
                    top_level: Arc::new(SpecsTopLevel::default()),
                    envelopes: Arc::new(vec![make_envelope(SpecsEnvelopeKind::TopLevel, "t2")]),
                }),
            ]),
        );

        let result = store.build_combined_delta_proto_payload(&rc, 50);
        assert_eq!(result, DeltaProtoBuildResult::NoUpdates { lcut: 50 });
    }

    #[test]
    fn build_combined_delta_proto_payload_assembles_in_required_order() {
        let store = make_store();
        let rc = make_rc();
        store.sdk_key_store.upsert(Arc::clone(&rc), 0);

        store.store.insert(
            rc.sdk_key.clone(),
            VecDeque::from(vec![
                Arc::new(DeltaCacheEntry {
                    since_time_requested: 60,
                    lcut: 80,
                    top_level: Arc::new(SpecsTopLevel::default()),
                    envelopes: Arc::new(vec![
                        make_envelope(SpecsEnvelopeKind::TopLevel, "oldest-top"),
                        make_envelope(SpecsEnvelopeKind::CopyPrev, "oldest-copy-prev"),
                        make_envelope(SpecsEnvelopeKind::DynamicConfig, "oldest-data"),
                        make_envelope(SpecsEnvelopeKind::Done, "oldest-done"),
                    ]),
                }),
                Arc::new(DeltaCacheEntry {
                    since_time_requested: 80,
                    lcut: 100,
                    top_level: Arc::new(SpecsTopLevel::default()),
                    envelopes: Arc::new(vec![
                        make_envelope(SpecsEnvelopeKind::TopLevel, "older-top"),
                        make_envelope(SpecsEnvelopeKind::CopyPrev, "older-copy-prev"),
                        make_envelope(SpecsEnvelopeKind::DynamicConfig, "older-data"),
                        make_envelope(SpecsEnvelopeKind::Checksums, "older-checksum"),
                        make_envelope(SpecsEnvelopeKind::Done, "older-done"),
                    ]),
                }),
                Arc::new(DeltaCacheEntry {
                    since_time_requested: 100,
                    lcut: 120,
                    top_level: Arc::new(SpecsTopLevel::default()),
                    envelopes: Arc::new(vec![
                        make_envelope(SpecsEnvelopeKind::TopLevel, "latest-top"),
                        make_envelope(SpecsEnvelopeKind::CopyPrev, "latest-copy-prev"),
                        make_envelope(SpecsEnvelopeKind::FeatureGate, "latest-data"),
                        make_envelope(SpecsEnvelopeKind::Checksums, "latest-checksum"),
                        make_envelope(SpecsEnvelopeKind::Done, "latest-done"),
                    ]),
                }),
            ]),
        );

        let result = store.build_combined_delta_proto_payload(&rc, 60);
        let (lcut, payload) = match result {
            DeltaProtoBuildResult::Combined { lcut, payload } => (lcut, payload),
            other => panic!("expected combined payload, got {other:?}"),
        };
        assert_eq!(lcut, 120);

        let decoded = decode_length_delimited_envelopes(&payload);
        let names: Vec<&str> = decoded.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "latest-top",
                "latest-copy-prev",
                "oldest-data",
                "older-data",
                "latest-data",
                "latest-checksum",
                "latest-done",
            ]
        );
    }
}
