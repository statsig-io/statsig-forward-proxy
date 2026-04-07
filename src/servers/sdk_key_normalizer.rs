use std::collections::HashMap;

use reqwest::Url;
use rocket::fairing::AdHoc;
use rocket::http::Header;
use tokio::sync::RwLock;
use tokio::time::Instant;

use super::normalized_path::NormalizedPath;

pub struct SdkKeyCache(pub RwLock<HashMap<String, String>>);

pub struct TimerStart(pub Option<Instant>);

pub(crate) fn extract_download_id_list_file_sdk_key(query: &str) -> Option<String> {
    Url::parse(&format!("http://dummy/?{query}"))
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == "k")
        .map(|(_, v)| v.into_owned())
}

pub fn sdk_key_normalization_fairing() -> AdHoc {
    AdHoc::on_request("Normalize SDK Key", |req, _| {
        Box::pin(async move {
            req.local_cache(|| TimerStart(Some(Instant::now())));

            if req.headers().contains("statsig-api-key") {
                return;
            }

            if req.method() != rocket::http::Method::Get {
                return;
            }

            let path = req.uri().path().to_string();
            if NormalizedPath::from(path.as_str()) == NormalizedPath::V1DownloadIdListFile {
                if let Some(sdk_key) = req
                    .uri()
                    .query()
                    .map(|query| query.as_str())
                    .and_then(extract_download_id_list_file_sdk_key)
                {
                    req.add_header(Header::new("statsig-api-key", sdk_key));
                }
                return;
            }

            let sdk_key_cache = req.rocket().state::<SdkKeyCache>().unwrap();

            if let Some(sdk_key) = sdk_key_cache.0.read().await.get(&path).cloned() {
                req.add_header(Header::new("statsig-api-key", sdk_key));
                return;
            }

            let new_key = path
                .strip_suffix(".json")
                .or_else(|| path.strip_suffix(".js"))
                .unwrap_or(&path)
                .rsplit_once('/')
                .map_or(path.clone(), |(_, key)| key.to_string());

            sdk_key_cache.0.write().await.insert(path, new_key.clone());

            req.add_header(Header::new("statsig-api-key", new_key));
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{
        extract_download_id_list_file_sdk_key, sdk_key_normalization_fairing, SdkKeyCache,
    };
    use crate::servers::authorized_request_context::DownloadIdListFileRequestGuard;
    use rocket::http::uri::fmt::Path;
    use rocket::http::uri::Segments;
    use rocket::http::Status;
    use rocket::local::blocking::Client;
    use rocket::{get, routes};
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    #[get("/download_id_list_file/<_tail..>")]
    fn download_id_list_file_guard_sdk_key(
        _tail: Segments<'_, Path>,
        guard: DownloadIdListFileRequestGuard,
    ) -> (Status, String) {
        match guard {
            DownloadIdListFileRequestGuard::Authorized(authorized_rc) => {
                (Status::Ok, authorized_rc.sdk_key.clone())
            }
            DownloadIdListFileRequestGuard::Invalid(error) => {
                (Status::BadRequest, error.body().to_string())
            }
        }
    }

    #[test]
    fn extract_download_id_list_file_sdk_key_handles_realistic_signed_query() {
        let query = "sv=2020-10-02&se=2026-01-27T00%3A23%3A42Z&sr=b&sp=r&sig=L6Sl%2BFRaRQVWVC0E%2B7XGuUCi9SRw1kLEK7FJE%2BnHYiY%3D&k=secret-VcKN8Xxxxxxxx";
        let sdk_key = extract_download_id_list_file_sdk_key(query);

        assert_eq!(sdk_key.as_deref(), Some("secret-VcKN8Xxxxxxxx"));
    }

    #[test]
    fn extract_download_id_list_file_sdk_key_returns_none_when_missing() {
        let query = "sv=2020-10-02&sig=abc";

        assert_eq!(extract_download_id_list_file_sdk_key(query), None);
    }

    #[test]
    fn normalize_sdk_key_fairing_skips_download_id_list_file_signed_queries() {
        let rocket = rocket::build()
            .manage(SdkKeyCache(RwLock::new(HashMap::new())))
            .attach(sdk_key_normalization_fairing())
            .mount("/v1", routes![download_id_list_file_guard_sdk_key]);
        let client = Client::tracked(rocket).expect("client should build");

        let response = client
            .get("/v1/download_id_list_file/file_123?k=secret-key")
            .dispatch();

        assert_eq!(response.status(), Status::Ok);
        assert_eq!(response.into_string().as_deref(), Some("secret-key"));
    }

    #[test]
    fn normalize_sdk_key_fairing_preserves_missing_sdk_key_for_download_id_list_file() {
        let rocket = rocket::build()
            .manage(SdkKeyCache(RwLock::new(HashMap::new())))
            .attach(sdk_key_normalization_fairing())
            .mount("/v1", routes![download_id_list_file_guard_sdk_key]);
        let client = Client::tracked(rocket).expect("client should build");

        let response = client.get("/v1/download_id_list_file/file_123").dispatch();

        assert_eq!(response.status(), Status::BadRequest);
        assert_eq!(
            response.into_string().as_deref(),
            Some("Missing statsig-api-key")
        );
    }
}
