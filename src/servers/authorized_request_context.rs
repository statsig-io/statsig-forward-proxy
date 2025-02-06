use parking_lot::RwLock;
use rocket::http::Status;
use rocket::request::{self, FromRequest, Outcome, Request};
use std::collections::HashMap;
use std::sync::Arc;

use crate::servers::normalized_path::NormalizedPath;
use crate::utils::compress_encoder::CompressionEncoder;

#[derive(Debug)]
pub struct AuthError;

type CacheKey = (String, NormalizedPath, CompressionEncoder);
pub struct AuthorizedRequestContextCache(
    Arc<RwLock<HashMap<CacheKey, Arc<AuthorizedRequestContext>>>>,
);

impl Default for AuthorizedRequestContextCache {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthorizedRequestContextCache {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(HashMap::new())))
    }

    pub fn get_or_insert(
        &self,
        sdk_key: String,
        path: NormalizedPath,
        encoding: CompressionEncoder,
    ) -> Arc<AuthorizedRequestContext> {
        let key = (sdk_key.clone(), path.clone(), encoding);
        {
            let read_lock = self.0.read();
            if let Some(context) = read_lock.get(&key) {
                return context.clone();
            }
        }

        let mut write_lock = self.0.write();
        write_lock
            .entry(key)
            .or_insert_with(|| Arc::new(AuthorizedRequestContext::new(sdk_key, path, encoding)))
            .clone()
    }
}

pub struct AuthorizedRequestContextWrapper(pub Arc<AuthorizedRequestContext>);

impl AuthorizedRequestContextWrapper {
    pub fn inner(&self) -> Arc<AuthorizedRequestContext> {
        self.0.clone()
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for AuthorizedRequestContextWrapper {
    type Error = AuthError;

    async fn from_request(request: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        let normalized_path = match request.guard::<NormalizedPath>().await {
            Outcome::Success(path) => path,
            Outcome::Error((status, _)) => return Outcome::Error((status, AuthError)),
            Outcome::Forward(status) => return Outcome::Forward(status),
        };

        let headers = request.headers();
        let cache = request
            .rocket()
            .state::<Arc<AuthorizedRequestContextCache>>()
            .unwrap();

        let encoding = if headers
            .get("Accept-Encoding")
            .any(|v| v.to_lowercase().contains("gzip"))
        {
            CompressionEncoder::Gzip
        } else {
            CompressionEncoder::PlainText
        };

        match headers.get_one("statsig-api-key") {
            Some(sdk_key) => Outcome::Success(AuthorizedRequestContextWrapper(
                cache.get_or_insert(sdk_key.to_string(), normalized_path, encoding),
            )),
            None => Outcome::Error((Status::BadRequest, AuthError)),
        }
    }
}

#[derive(Debug)]
pub struct AuthorizedRequestContext {
    pub sdk_key: String,
    pub path: NormalizedPath,
    pub use_lcut: bool,
    pub use_dict_id: bool,
    pub encoding: CompressionEncoder,
}

impl AuthorizedRequestContext {
    pub fn new(sdk_key: String, path: NormalizedPath, encoding: CompressionEncoder) -> Self {
        let use_lcut = path == NormalizedPath::V1DownloadConfigSpecs
            || path == NormalizedPath::V2DownloadConfigSpecs
            || path == NormalizedPath::V2DownloadConfigSpecsWithSharedDict;

        let use_dict_id = path == NormalizedPath::V2DownloadConfigSpecsWithSharedDict;

        AuthorizedRequestContext {
            sdk_key,
            path,
            use_lcut,
            use_dict_id,
            encoding,
        }
    }
}

impl std::fmt::Display for AuthorizedRequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}|{}|{}",
            self.sdk_key,
            self.path.as_str(),
            self.encoding
        )
    }
}

impl PartialEq for AuthorizedRequestContext {
    fn eq(&self, other: &Self) -> bool {
        self.sdk_key == other.sdk_key && self.path == other.path && self.encoding == other.encoding
    }
}

impl Eq for AuthorizedRequestContext {}

impl std::hash::Hash for AuthorizedRequestContext {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.sdk_key.hash(state);
        self.path.hash(state);
        self.encoding.hash(state);
    }
}
