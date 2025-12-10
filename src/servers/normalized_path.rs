use std::fmt::Display;

use rocket::{
    request::{self, FromRequest},
    Request,
};

#[derive(Debug, Clone)]
pub struct PathNormalizerError;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NormalizedPath {
    V1DownloadConfigSpecs,
    V1GetIdLists,
    V1LogEvent,
    V2DownloadConfigSpecs,
    RawPath(String),
}

impl Display for NormalizedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl NormalizedPath {
    pub fn as_str(&self) -> &str {
        match self {
            NormalizedPath::V1DownloadConfigSpecs => "/v1/download_config_specs/",
            NormalizedPath::V1GetIdLists => "/v1/get_id_lists/",
            NormalizedPath::V1LogEvent => "/v1/log_event/",
            NormalizedPath::V2DownloadConfigSpecs => "/v2/download_config_specs/",
            NormalizedPath::RawPath(path) => path,
        }
    }
}

impl From<&str> for NormalizedPath {
    fn from(value: &str) -> Self {
        if value.starts_with("/v1/download_config_specs") {
            NormalizedPath::V1DownloadConfigSpecs
        } else if value.starts_with("/v1/get_id_lists") {
            NormalizedPath::V1GetIdLists
        } else if value.starts_with("/v1/log_event") {
            NormalizedPath::V1LogEvent
        } else if value.starts_with("/v2/download_config_specs") {
            NormalizedPath::V2DownloadConfigSpecs
        } else {
            NormalizedPath::RawPath(value.to_string())
        }
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for NormalizedPath {
    type Error = PathNormalizerError;

    async fn from_request(request: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        request
            .local_cache(|| {
                request::Outcome::Success(NormalizedPath::from(request.uri().path().as_str()))
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! try_from_str_tests {
        ($($name:ident: $input:expr => $expected:pat,)*) => {
        $(
            #[test]
            fn $name() {
                let normalized_path = NormalizedPath::try_from($input)
                    .expect("Path should be able to be normalized");
                assert!(matches!(normalized_path, $expected));
            }
        )*
        }
    }

    try_from_str_tests! {
        test_v1_download_config_specs: "/v1/download_config_specs" => NormalizedPath::V1DownloadConfigSpecs,
        test_v1_get_id_lists: "/v1/get_id_lists" => NormalizedPath::V1GetIdLists,
        test_v1_log_event: "/v1/log_event" => NormalizedPath::V1LogEvent,
        test_v2_download_config_specs: "/v2/download_config_specs" => NormalizedPath::V2DownloadConfigSpecs,
    }

    #[test]
    fn try_from_str_dummy_path_is_raw_path() {
        let dummy_path = "/v1/dummy_path";
        let normalized_path = NormalizedPath::from(dummy_path);
        assert!(normalized_path == NormalizedPath::RawPath(dummy_path.to_string()));
    }

    #[test]
    fn compare_normalized_paths_same_are_equal() {
        let path0 = NormalizedPath::V1DownloadConfigSpecs;
        let path1 = NormalizedPath::V1DownloadConfigSpecs;
        assert_eq!(path0, path1);
    }

    #[test]
    fn compare_normalized_paths_different_are_not_equal() {
        let path0 = NormalizedPath::V1DownloadConfigSpecs;
        let path1 = NormalizedPath::V2DownloadConfigSpecs;
        assert_ne!(path0, path1);
    }
}
