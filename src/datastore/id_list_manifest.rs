use crate::datastore::data_providers::http_data_provider::ResponsePayload;
use crate::utils::compress_encoder::CompressionEncoder;
use flate2::read::GzDecoder;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

pub(crate) type IdListManifest = HashMap<String, IdListManifestEntry>;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct IdListManifestEntry {
    pub name: String,
    pub size: u64,
    pub url: String,
    #[serde(rename = "creationTime")]
    pub creation_time: u64,
    #[serde(rename = "fileID")]
    pub file_id: String,
}

pub(crate) fn parse_id_list_manifest(
    payload: &Arc<ResponsePayload>,
) -> Result<IdListManifest, String> {
    let decoded = decode_response_payload(payload)?;
    let entries_by_manifest_key: HashMap<String, IdListManifestEntry> =
        serde_json::from_slice(&decoded)
            .map_err(|e| format!("get_id_lists JSON could not be parsed: {e}"))?;

    Ok(entries_by_manifest_key
        .into_values()
        .map(|entry| (entry.name.clone(), entry))
        .collect())
}

fn decode_response_payload(payload: &Arc<ResponsePayload>) -> Result<Vec<u8>, String> {
    match payload.encoding.as_ref() {
        CompressionEncoder::PlainText => Ok(payload.data.to_vec()),
        CompressionEncoder::Gzip => {
            let mut decoder = GzDecoder::new(payload.data.as_ref().as_ref());
            let mut decoded = Vec::new();
            decoder
                .read_to_end(&mut decoded)
                .map_err(|e| format!("get_id_lists gzip payload could not be decoded: {e}"))?;
            Ok(decoded)
        }
        CompressionEncoder::Brotli | CompressionEncoder::StatsigBrotli => {
            let mut decoder = brotli::Decompressor::new(payload.data.as_ref().as_ref(), 4096);
            let mut decoded = Vec::new();
            decoder
                .read_to_end(&mut decoded)
                .map_err(|e| format!("get_id_lists brotli payload could not be decoded: {e}"))?;
            Ok(decoded)
        }
        encoding => Err(format!(
            "get_id_lists payload uses unsupported encoding: {encoding}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn plaintext_payload(data: &'static str) -> Arc<ResponsePayload> {
        Arc::new(ResponsePayload {
            encoding: Arc::new(CompressionEncoder::PlainText),
            use_proto: false,
            data: Arc::new(Bytes::from_static(data.as_bytes())),
        })
    }

    fn gzip_payload(data: &'static str) -> Arc<ResponsePayload> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(data.as_bytes())
            .expect("test gzip payload should encode");
        let encoded = encoder.finish().expect("test gzip payload should finish");

        Arc::new(ResponsePayload {
            encoding: Arc::new(CompressionEncoder::Gzip),
            use_proto: false,
            data: Arc::new(Bytes::from(encoded)),
        })
    }

    #[test]
    fn parses_object_shaped_manifest_and_keys_by_entry_name() {
        let payload = plaintext_payload(
            r#"{
                "manifest_key": {
                    "name": "stable_name",
                    "size": 99990,
                    "url": "https://api.statsigcdn.com/v1/download_id_list_file/company%2Ffile?sig=signed&k=secret-test",
                    "creationTime": 1772477815000,
                    "fileID": "file"
                }
            }"#,
        );

        let parsed = parse_id_list_manifest(&payload).expect("manifest should parse");

        assert!(parsed.contains_key("stable_name"));
        assert_eq!(parsed["stable_name"].file_id, "file");
        assert_eq!(parsed["stable_name"].size, 99990);
    }

    #[test]
    fn parses_gzip_manifest_payload() {
        let payload = gzip_payload(
            r#"{
                "list": {
                    "name": "list",
                    "size": 1,
                    "url": "https://api.statsigcdn.com/v1/download_id_list_file/company%2Ffile?sig=signed&k=secret-test",
                    "creationTime": 2,
                    "fileID": "file"
                }
            }"#,
        );

        let parsed = parse_id_list_manifest(&payload).expect("gzip manifest should parse");

        assert_eq!(parsed["list"].creation_time, 2);
    }

    #[test]
    fn malformed_manifest_json_is_rejected() {
        let payload = plaintext_payload(r#"{"list": "not_an_entry"}"#);

        let error = parse_id_list_manifest(&payload).unwrap_err();

        assert!(error.contains("get_id_lists JSON could not be parsed"));
    }
}
