use std::sync::Arc;

use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::{EventStat, OperationType, ProxyEvent, ProxyEventType};
use crate::servers::authorized_request_context::{
    parse_id_list_file_id, AuthorizedRequestContext, AuthorizedRequestContextCache,
};
use crate::servers::normalized_path::NormalizedPath;
use crate::utils::compress_encoder::ParsedAcceptEncoding;
use crate::utils::request_helper::{does_request_accept_deltas, does_request_supports_proto};
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::StatusClass;
use rocket::{Request, Response};
use tokio::time::Instant;

const MISSING_TIMER_SENTINEL_MS: i64 = -2;

struct RequestTimerStart(Option<Instant>);

pub struct RequestLoggingRequestFairing;
pub struct RequestLoggingResponseFairing;

struct RequestContextInput {
    sdk_key: String,
    path: NormalizedPath,
    parsed_encoding: ParsedAcceptEncoding,
    supports_proto: bool,
    accept_deltas: bool,
    raw_path: Option<String>,
    raw_query: Option<String>,
    file_id: Option<String>,
    range_start: Option<u64>,
    id_list_size: Option<u64>,
}

#[rocket::async_trait]
impl Fairing for RequestLoggingRequestFairing {
    fn info(&self) -> Info {
        Info {
            name: "Request Logger Timer Start",
            kind: Kind::Request,
        }
    }

    async fn on_request(&self, req: &mut Request<'_>, _: &mut rocket::Data<'_>) {
        req.local_cache(|| RequestTimerStart(Some(Instant::now())));
    }
}

#[rocket::async_trait]
impl Fairing for RequestLoggingResponseFairing {
    fn info(&self) -> Info {
        Info {
            name: "Request Logger Response",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, req: &'r Request<'_>, resp: &mut Response<'r>) {
        let ms = request_duration_ms(req);
        let cache = req
            .rocket()
            .state::<Arc<AuthorizedRequestContextCache>>()
            .expect("AuthorizedRequestContextCache state must be managed")
            .clone();
        let sdk_key = req
            .headers()
            .get_one("statsig-api-key")
            .unwrap_or("no-key-provided")
            .to_string();
        let path = NormalizedPath::from(req.uri().path().as_str());
        let parsed_encoding =
            ParsedAcceptEncoding::from_header_values(req.headers().get("Accept-Encoding"));
        let lcut = resp
            .headers()
            .get_one("x-since-time")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let status_code = resp.status().code;
        let status_class = resp.status().class();
        let content_encoding = resp
            .headers()
            .get("Content-Encoding")
            // Join with '+' so Datadog doesn't split comma-separated encoding tags.
            .collect::<Vec<_>>()
            .join("+");
        let sdk_type = req
            .headers()
            .get_one("statsig-sdk-type")
            .unwrap_or("unknown")
            .to_string();
        let sdk_version = req
            .headers()
            .get_one("statsig-sdk-version")
            // SDK versions are typically semver strings (e.g. "1.2.3"), not numeric.
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("unknown")
            .to_string();
        let service = req
            .headers()
            .get_one("x-request-service")
            .unwrap_or("unknown")
            .to_string();
        let supports_proto = does_request_supports_proto(req);
        let accept_deltas = does_request_accept_deltas(req);
        let response_payload_type = should_log_response_payload_type(&path, status_class)
            .then(|| classify_response_payload_type(resp).to_string());
        let range_start = parse_logged_id_list_file_range_start(req.headers());
        let id_list_size = parse_logged_id_list_file_size(req.headers());
        let accept_encoding_tag = parsed_encoding.logger_tag();
        let raw_path =
            (path == NormalizedPath::V1DownloadIdListFile).then(|| req.uri().path().to_string());
        let raw_query = (path == NormalizedPath::V1DownloadIdListFile)
            .then(|| req.uri().query().map(|query| query.to_string()))
            .flatten();
        let file_id = raw_path.as_deref().and_then(parse_id_list_file_id);

        tokio::spawn(async move {
            let request_context = build_request_context(
                cache,
                RequestContextInput {
                    sdk_key,
                    path,
                    parsed_encoding,
                    supports_proto,
                    accept_deltas,
                    raw_path,
                    raw_query,
                    file_id,
                    range_start,
                    id_list_size,
                },
            );

            let mut event = ProxyEvent::new_with_rc(
                if status_class == StatusClass::Success {
                    ProxyEventType::HttpServerRequestSuccess
                } else {
                    ProxyEventType::HttpServerRequestFailed
                },
                &request_context,
            )
            .with_status_code(status_code)
            .with_lcut(lcut)
            .with_response_encoding(content_encoding)
            .with_sdk_type(sdk_type)
            .with_sdk_version(sdk_version)
            .with_service(service)
            .with_stat(EventStat {
                operation_type: OperationType::Distribution,
                value: ms,
            });

            if let Some(accept_encoding) = accept_encoding_tag {
                event = event.with_accept_encoding(accept_encoding);
            }

            if let Some(payload_type) = response_payload_type {
                event = event.with_response_payload_type(payload_type);
            }

            ProxyEventObserver::publish_event(event);
        });
    }
}

fn request_duration_ms(req: &Request<'_>) -> i64 {
    req.local_cache(|| RequestTimerStart(None))
        .0
        .map_or(MISSING_TIMER_SENTINEL_MS, |start| {
            start
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(MISSING_TIMER_SENTINEL_MS)
        })
}

fn should_log_response_payload_type(path: &NormalizedPath, status_class: StatusClass) -> bool {
    status_class == StatusClass::Success
        && matches!(
            path,
            NormalizedPath::V1DownloadConfigSpecs
                | NormalizedPath::V2DownloadConfigSpecs
                | NormalizedPath::V2DownloadConfigSpecsDeltas
        )
}

fn classify_response_payload_type(resp: &Response<'_>) -> &'static str {
    let no_update = header_is_true(resp, "x-cache-hit");
    let delta = header_is_true(resp, "x-deltas-used");

    match (delta, no_update) {
        (true, true) => "delta_no_update",
        (true, false) => "delta",
        (false, true) => "no_update",
        (false, false) => "full",
    }
}

fn header_is_true(resp: &Response<'_>, header_name: &str) -> bool {
    match resp.headers().get_one(header_name) {
        Some(value) => value.eq_ignore_ascii_case("true"),
        None => false,
    }
}

fn build_request_context(
    cache: Arc<AuthorizedRequestContextCache>,
    input: RequestContextInput,
) -> Arc<AuthorizedRequestContext> {
    if input.path == NormalizedPath::V1DownloadIdListFile {
        Arc::new(
            AuthorizedRequestContext::new(
                input.sdk_key,
                input.path,
                input.parsed_encoding.acceptable_encodings(),
            )
            .with_request_capabilities(input.supports_proto, input.accept_deltas)
            .with_raw_request(input.raw_path, input.raw_query)
            .with_id_list_request(input.file_id, input.range_start, input.id_list_size),
        )
    } else {
        cache.get_or_insert(
            input.sdk_key,
            input.path,
            input.parsed_encoding.acceptable_encodings(),
            input.supports_proto,
            input.accept_deltas,
            input.file_id,
        )
    }
}

fn parse_logged_id_list_file_size(headers: &rocket::http::HeaderMap<'_>) -> Option<u64> {
    headers
        .get_one("statsig-id-list-file-size")
        .and_then(|value| value.trim().parse::<u64>().ok())
}

fn parse_logged_id_list_file_range_start(headers: &rocket::http::HeaderMap<'_>) -> Option<u64> {
    headers.get_one("Range").and_then(|value| {
        value
            .trim()
            .strip_prefix("bytes=")
            .and_then(|value| value.split_once('-').map(|(start, _)| start))
            .or_else(|| value.trim().strip_prefix("bytes="))
            .and_then(|start| start.parse::<u64>().ok())
    })
}

#[cfg(test)]
mod tests {
    use super::{
        classify_response_payload_type, parse_logged_id_list_file_range_start,
        parse_logged_id_list_file_size, should_log_response_payload_type,
    };
    use crate::servers::normalized_path::NormalizedPath;
    use rocket::http::{Header, Status, StatusClass};
    use rocket::Response;

    #[test]
    fn parse_logged_id_list_file_size_reads_explicit_header() {
        let mut headers = rocket::http::HeaderMap::new();
        headers.add(Header::new("statsig-id-list-file-size", "270"));

        assert_eq!(parse_logged_id_list_file_size(&headers), Some(270));
    }

    #[test]
    fn parse_logged_id_list_file_range_start_reads_range_header() {
        let mut headers = rocket::http::HeaderMap::new();
        headers.add(Header::new("Range", "bytes=270-"));

        assert_eq!(parse_logged_id_list_file_range_start(&headers), Some(270));
    }

    #[test]
    fn parse_logged_id_list_file_helpers_return_none_for_invalid_headers() {
        let mut headers = rocket::http::HeaderMap::new();
        headers.add(Header::new("statsig-id-list-file-size", "not-a-number"));
        headers.add(Header::new("Range", "items=270-"));

        assert_eq!(parse_logged_id_list_file_size(&headers), None);
        assert_eq!(parse_logged_id_list_file_range_start(&headers), None);
    }

    #[test]
    fn classify_response_payload_type_uses_existing_headers() {
        let response = Response::build()
            .status(Status::Ok)
            .header(Header::new("x-cache-hit", "true"))
            .finalize();

        assert_eq!(classify_response_payload_type(&response), "no_update");

        let response = Response::build()
            .status(Status::Ok)
            .header(Header::new("x-cache-hit", "true"))
            .header(Header::new("x-deltas-used", "true"))
            .finalize();
        assert_eq!(classify_response_payload_type(&response), "delta_no_update");
    }

    #[test]
    fn response_payload_type_is_only_logged_for_successful_dcs_requests() {
        assert!(should_log_response_payload_type(
            &NormalizedPath::V2DownloadConfigSpecs,
            StatusClass::Success
        ));
        assert!(!should_log_response_payload_type(
            &NormalizedPath::V1LogEvent,
            StatusClass::Success
        ));
        assert!(!should_log_response_payload_type(
            &NormalizedPath::V2DownloadConfigSpecs,
            StatusClass::ClientError
        ));
    }
}
