use parking_lot::RwLock;
use rocket::http::Status;
use rocket::request::{self, FromRequest, Outcome, Request};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug)]
pub struct AuthError;

pub struct AuthorizedRequestContextCache(
    Arc<RwLock<HashMap<(String, String, bool), Arc<AuthorizedRequestContext>>>>,
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
        path: String,
        use_gzip: bool,
    ) -> Arc<AuthorizedRequestContext> {
        let key = (sdk_key.clone(), path.clone(), use_gzip);
        {
            let read_lock = self.0.read();
            if let Some(context) = read_lock.get(&key) {
                return context.clone();
            }
        }

        let mut write_lock = self.0.write();
        write_lock
            .entry(key)
            .or_insert_with(|| Arc::new(AuthorizedRequestContext::new(sdk_key, path, use_gzip)))
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
        let headers = request.headers();
        let cache = request
            .rocket()
            .state::<Arc<AuthorizedRequestContextCache>>()
            .unwrap();

        let use_gzip = headers
            .get("Accept-Encoding")
            .any(|v| v.to_lowercase().contains("gzip"));

        match headers.get_one("statsig-api-key") {
            Some(sdk_key) => {
                Outcome::Success(AuthorizedRequestContextWrapper(cache.get_or_insert(
                    sdk_key.to_string(),
                    request.uri().path().to_string(),
                    use_gzip,
                )))
            }
            None => Outcome::Error((Status::BadRequest, AuthError)),
        }
    }
}

#[derive(Debug)]
pub struct AuthorizedRequestContext {
    pub sdk_key: String,
    pub path: String,
    pub use_lcut: bool,
    pub use_gzip: bool,
}

impl AuthorizedRequestContext {
    pub fn new(sdk_key: String, path: String, use_gzip: bool) -> Self {
        let mut normalized_path = path;
        if normalized_path.ends_with(".json") || normalized_path.ends_with(".js") {
            if let Some(pos) = normalized_path.rfind('/') {
                normalized_path = normalized_path[..pos + 1].to_string();
            }
        }

        if !normalized_path.ends_with('/') {
            normalized_path.push('/');
        }

        let use_lcut = normalized_path.ends_with("download_config_specs");
        AuthorizedRequestContext {
            sdk_key,
            path: normalized_path,
            use_lcut,
            use_gzip,
        }
    }
}

impl std::fmt::Display for AuthorizedRequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}|{}|{}", self.sdk_key, self.path, self.use_gzip)
    }
}

impl PartialEq for AuthorizedRequestContext {
    fn eq(&self, other: &Self) -> bool {
        self.sdk_key == other.sdk_key && self.path == other.path && self.use_gzip == other.use_gzip
    }
}

impl Eq for AuthorizedRequestContext {}

impl std::hash::Hash for AuthorizedRequestContext {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.sdk_key.hash(state);
        self.path.hash(state);
        self.use_gzip.hash(state);
    }
}
