use super::*;
use bytes::Bytes;
use flate2::{write::GzEncoder, Compression};
use std::io::Write;

fn payload(body: &[u8], encoding: CompressionEncoder) -> Arc<ResponsePayload> {
    let data = match encoding {
        CompressionEncoder::Gzip => {
            let mut writer = GzEncoder::new(Vec::new(), Compression::default());
            writer.write_all(body).unwrap();
            writer.finish().unwrap()
        }
        CompressionEncoder::Brotli | CompressionEncoder::StatsigBrotli => {
            let mut data = Vec::new();
            {
                let mut writer = brotli::CompressorWriter::new(&mut data, 4096, 4, 22);
                writer.write_all(body).unwrap();
            }
            data
        }
        _ => body.to_vec(),
    };
    Arc::new(ResponsePayload {
        encoding: Arc::new(encoding),
        use_proto: false,
        data: Arc::new(Bytes::from(data)),
    })
}

#[test]
fn extracts_identity_from_full_json_in_supported_encodings() {
    for encoding in [
        CompressionEncoder::PlainText,
        CompressionEncoder::Identity,
        CompressionEncoder::Gzip,
        CompressionEncoder::Brotli,
        CompressionEncoder::StatsigBrotli,
    ] {
        let body = payload(
            br#"{"has_updates":true,"company_id":"company-a","feature_gates":[]}"#,
            encoding,
        );
        assert_eq!(parse_company_id(&body).as_deref(), Some("company-a"));
    }
}

#[test]
fn rejects_missing_invalid_and_no_update_identity() {
    for body in [
        r#"{"has_updates":false}"#,
        r#"{"has_updates":false,"company_id":"company-a"}"#,
        r#"{"has_updates":true}"#,
        r#"{"has_updates":true,"company_id":null}"#,
        r#"{"has_updates":true,"company_id":"  "}"#,
        r#"{"has_updates":true,"company_id":42}"#,
        "not JSON",
    ] {
        assert_eq!(
            parse_company_id(&payload(body.as_bytes(), CompressionEncoder::PlainText)),
            None
        );
    }
    let mut body = payload(
        br#"{"has_updates":true,"company_id":"company-a"}"#,
        CompressionEncoder::PlainText,
    );
    Arc::get_mut(&mut body).unwrap().use_proto = true;
    assert_eq!(parse_company_id(&body), None);
}

async fn notify(
    store: &CompanyIdStore,
    key: &str,
    path: NormalizedPath,
    result_type: DataProviderRequestResult,
    body: &[u8],
) {
    let context = AuthorizedRequestContextCache::new().get_or_insert(
        key.to_string(),
        path,
        vec![CompressionEncoder::Gzip],
        false,
        false,
        None,
    );
    store
        .update(
            &Arc::new(FullRequestContext {
                authorized_request_context: context,
            }),
            &Arc::new(ResponseContext {
                result_type,
                lcut: 1,
                request_since_time: 0,
                body: payload(body, CompressionEncoder::Gzip),
            }),
        )
        .await;
}

#[tokio::test]
async fn preserves_identity_until_upstream_rejects_key() {
    let store = CompanyIdStore::new(["secret-a".to_string()]);
    let path = NormalizedPath::V1DownloadConfigSpecs;
    notify(
        &store,
        "secret-a",
        path.clone(),
        DataProviderRequestResult::DataAvailable,
        br#"{"has_updates":true,"company_id":"company-a"}"#,
    )
    .await;
    for result in [
        DataProviderRequestResult::NoDataAvailable,
        DataProviderRequestResult::Error,
        DataProviderRequestResult::DataAvailable,
    ] {
        notify(
            &store,
            "secret-a",
            path.clone(),
            result,
            br#"{"has_updates":false}"#,
        )
        .await;
        assert_eq!(
            store.get_company_id("secret-a").as_deref(),
            Some("company-a")
        );
    }
    notify(
        &store,
        "secret-a",
        path,
        DataProviderRequestResult::Unauthorized,
        b"{}",
    )
    .await;
    assert_eq!(store.get_company_id("secret-a"), None);
}

#[tokio::test]
async fn only_trusts_config_responses_for_configured_keys() {
    let store = CompanyIdStore::new(["secret-a".to_string(), "secret-b".to_string()]);
    let body = br#"{"has_updates":true,"company_id":"company-a"}"#;
    for (key, path, result) in [
        (
            "secret-other",
            NormalizedPath::V1DownloadConfigSpecs,
            DataProviderRequestResult::DataAvailable,
        ),
        (
            "secret-a",
            NormalizedPath::V1GetIdLists,
            DataProviderRequestResult::DataAvailable,
        ),
        (
            "secret-a",
            NormalizedPath::V1DownloadConfigSpecs,
            DataProviderRequestResult::Error,
        ),
    ] {
        notify(&store, key, path, result, body).await;
        assert_eq!(store.get_company_id(key), None);
    }
    for key in ["secret-a", "secret-b"] {
        notify(
            &store,
            key,
            NormalizedPath::V2DownloadConfigSpecs,
            DataProviderRequestResult::DataAvailable,
            body,
        )
        .await;
        assert_eq!(store.get_company_id(key).as_deref(), Some("company-a"));
    }
}

#[tokio::test]
async fn discovery_fetches_full_json_without_an_sdk_request() {
    use crate::datastore::caching::disabled_cache::DisabledCache;
    use crate::datastore::data_providers::background_poll_dispatch::BackgroundPollDispatchConfig;
    use crate::datastore::data_providers::http_data_provider::HttpDataProvider;
    use crate::datastore::data_providers::request_builder::{
        CachedRequestBuilders, DcsRequestBuilder,
    };
    use crate::datastore::data_providers::HttpClientConfig;
    use crate::datastore::sdk_key_store::SdkKeyStore;
    use crate::observers::http_data_provider_observer::HttpDataProviderObserver;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let http = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut buffer = [0; 1024];
            let count = stream.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buffer[..count]);
            if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("GET /v2/download_config_specs/secret-discovery.json"));
        assert!(!request.contains("supports_proto"));
        assert!(!request.contains("sinceTime"));
        let body = r#"{"has_updates":true,"company_id":"company-discovered","time":123}"#;
        stream.write_all(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nx-since-time: 123\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(), body,
        ).as_bytes()).await.unwrap();
    });
    let identities = Arc::new(CompanyIdStore::new(["secret-discovery".to_string()]));
    let keys = Arc::new(SdkKeyStore::new());
    let observers = Arc::new(HttpDataProviderObserver::new());
    observers.add_observer(identities.clone()).await;
    observers.add_observer(keys.clone()).await;
    CachedRequestBuilders::add_request_builder(
        NormalizedPath::V2DownloadConfigSpecs,
        Arc::new(DcsRequestBuilder::new(
            format!("http://{address}"),
            observers,
            Arc::new(DisabledCache::default()),
            None,
        )),
    );
    let provider = Arc::new(BackgroundDataProvider::new(
        Arc::new(HttpDataProvider {}),
        10,
        keys.clone(),
        false,
        HttpClientConfig::new(2, 2, 2, 2, 1),
        BackgroundPollDispatchConfig::new(1, 0),
    ));
    let cancel = CancellationToken::new();
    identities.clone().spawn_discovery(
        &tokio::runtime::Handle::current(),
        Arc::new(AuthorizedRequestContextCache::new()),
        provider,
        cancel.clone(),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if identities.get_company_id("secret-discovery").is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    http.await.unwrap();
    assert_eq!(
        identities.get_company_id("secret-discovery").as_deref(),
        Some("company-discovered")
    );
    assert_eq!(keys.get_registered_store().len(), 1);
}
