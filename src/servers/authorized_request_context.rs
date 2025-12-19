use parking_lot::RwLock;
use rocket::http::Status;
use rocket::request::{self, FromRequest, Outcome, Request};
use std::collections::HashMap;
use std::sync::Arc;

use crate::servers::normalized_path::NormalizedPath;
use crate::utils::compress_encoder::{
    convert_compression_encodings_from_header_map, CompressionEncoder,
};
use crate::utils::request_helper::does_request_supports_proto;

#[derive(Debug)]
pub struct AuthError;

type CacheKey = (
    String,
    NormalizedPath,
    Vec<CompressionEncoder>,
    bool, /* supports_proto */
);
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
        encodings: Vec<CompressionEncoder>,
        supports_proto: bool,
    ) -> Arc<AuthorizedRequestContext> {
        let key = (
            sdk_key.clone(),
            path.clone(),
            encodings.clone(),
            supports_proto,
        );
        {
            let read_lock = self.0.read();
            if let Some(context) = read_lock.get(&key) {
                return context.clone();
            }
        }

        let mut write_lock = self.0.write();
        write_lock
            .entry(key)
            .or_insert_with(|| {
                Arc::new(AuthorizedRequestContext::new(
                    sdk_key,
                    path,
                    encodings,
                    supports_proto,
                ))
            })
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

        let cache = request
            .rocket()
            .state::<Arc<AuthorizedRequestContextCache>>()
            .unwrap();
        let headers = request.headers();
        let encodings =
            convert_compression_encodings_from_header_map(headers.get("Accept-Encoding"));
        let supports_proto = does_request_supports_proto(request);

        match headers.get_one("statsig-api-key") {
            Some(sdk_key) => {
                Outcome::Success(AuthorizedRequestContextWrapper(cache.get_or_insert(
                    sdk_key.to_string(),
                    normalized_path,
                    encodings,
                    supports_proto,
                )))
            }
            None => Outcome::Error((Status::BadRequest, AuthError)),
        }
    }
}

#[derive(Debug)]
pub struct AuthorizedRequestContext {
    pub sdk_key: String,
    pub path: NormalizedPath,
    pub use_lcut: bool,
    pub supports_proto: bool,
    pub encodings: Vec<CompressionEncoder>,
}

impl AuthorizedRequestContext {
    pub fn new(
        sdk_key: String,
        path: NormalizedPath,
        encodings: Vec<CompressionEncoder>,
        supports_proto: bool,
    ) -> Self {
        let use_lcut = path == NormalizedPath::V1DownloadConfigSpecs
            || path == NormalizedPath::V2DownloadConfigSpecs;

        AuthorizedRequestContext {
            sdk_key,
            path,
            use_lcut,
            supports_proto,
            encodings,
        }
    }
}

impl std::fmt::Display for AuthorizedRequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}|{}|{:?}|{}",
            self.sdk_key,
            self.path.as_str(),
            self.encodings,
            self.supports_proto
        )
    }
}

impl PartialEq for AuthorizedRequestContext {
    fn eq(&self, other: &Self) -> bool {
        self.sdk_key == other.sdk_key
            && self.path == other.path
            && self.encodings == other.encodings
            && self.supports_proto == other.supports_proto
    }
}

impl Eq for AuthorizedRequestContext {}

impl std::hash::Hash for AuthorizedRequestContext {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.sdk_key.hash(state);
        self.path.hash(state);
        self.encodings.hash(state);
        self.supports_proto.hash(state);
    }
}
