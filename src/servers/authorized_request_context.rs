use dashmap::DashMap;
use rocket::http::Status;
use rocket::request::{self, FromRequest, Outcome, Request};
use std::sync::Arc;

#[derive(Debug)]
pub struct AuthError;

pub struct AuthorizedRequestContextCache(
    Arc<DashMap<(String, String), Arc<AuthorizedRequestContext>>>,
);

impl Default for AuthorizedRequestContextCache {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthorizedRequestContextCache {
    pub fn new() -> Self {
        Self(Arc::new(DashMap::new()))
    }

    pub fn get_or_insert(&self, sdk_key: String, path: String) -> Arc<AuthorizedRequestContext> {
        self.0
            .entry((sdk_key.clone(), path.clone()))
            .or_insert_with(|| Arc::new(AuthorizedRequestContext::new(sdk_key, path)))
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

        match headers.get_one("statsig-api-key") {
            Some(sdk_key) => Outcome::Success(AuthorizedRequestContextWrapper(
                cache.get_or_insert(sdk_key.to_string(), request.uri().path().to_string()),
            )),
            None => Outcome::Error((Status::BadRequest, AuthError)),
        }
    }
}

#[derive(Debug)]
pub struct AuthorizedRequestContext {
    pub sdk_key: String,
    pub path: String,
    pub use_lcut: bool,
}

impl AuthorizedRequestContext {
    pub fn new(sdk_key: String, path: String) -> Self {
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
        }
    }
}

impl std::fmt::Display for AuthorizedRequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}|{}", self.sdk_key, self.path)
    }
}

impl PartialEq for AuthorizedRequestContext {
    fn eq(&self, other: &Self) -> bool {
        self.sdk_key == other.sdk_key && self.path == other.path
    }
}

impl Eq for AuthorizedRequestContext {}

impl std::hash::Hash for AuthorizedRequestContext {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.sdk_key.hash(state);
        self.path.hash(state);
    }
}
