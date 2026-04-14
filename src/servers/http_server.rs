use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;

use crate::datastore::config_spec_store::shadow_fetch_json_config_spec;
use crate::datastore::deltas_store::{DeltaProtoBuildResult, DeltasStore};
use crate::datastore::id_list_file_store::IdListFileStore;
use crate::datastore::id_list_store::GetIdListStore;
use crate::datastore::log_event_store::LogEventStore;
use crate::datastore::sdk_key_store::SdkKeyStore;

use crate::datatypes::gzip_data::LoggedBodyJSON;
use crate::datatypes::log_event::LogEventRequest;
use crate::datatypes::log_event::LogEventResponse;
use crate::http_data_provider::ResponsePayload;
use crate::servers::http_apis;
use crate::servers::request_logging_fairing::{
    RequestLoggingRequestFairing, RequestLoggingResponseFairing,
};
use crate::utils::compress_encoder::CompressionEncoder;
use crate::utils::response_compression::{
    compress_response_payload, content_encoding_header_value, preferred_response_compression,
};
use crate::Cli;
use bytes::Bytes;

use rocket::form::{FromForm, Lenient};
use rocket::http::uri::fmt::Path;
use rocket::http::uri::Segments;
use rocket::http::ContentType;
use rocket::http::{Header, Status};
use rocket::post;
use rocket::response::status::Custom;
use rocket::response::Responder;
use rocket::routes;

use rocket::serde::json::Json;

use rocket::Request;
use rocket::Response;
use rocket::State;
use rocket::{catch, catchers, get};
use serde::{Deserialize, Serialize};

use tokio::runtime::Handle;
use tokio::sync::RwLock;

use crate::datastore::config_spec_store::ConfigSpecStore;
use lazy_static::lazy_static;

lazy_static! {
    static ref UNAUTHORIZED_RESPONSE: Arc<ResponsePayload> = Arc::new(ResponsePayload {
        encoding: Arc::new(CompressionEncoder::PlainText),
        data: Arc::from(Bytes::from("Unauthorized")),
        use_proto: false
    });
}
const DCS_NO_UPDATE_JSON: &[u8] = br#"{"has_updates":false}"#;

use crate::servers::authorized_request_context::{
    AuthorizedRequestContext, AuthorizedRequestContextCache, AuthorizedRequestContextWrapper,
    DownloadIdListFileRequestGuard,
};
use crate::servers::sdk_key_normalizer::{sdk_key_normalization_fairing, SdkKeyCache};

use super::normalized_path::NormalizedPath;

#[derive(Serialize, Deserialize)]
pub struct DefaultResponse {
    pub code: u16,
    pub reason: &'static str,
}

#[catch(default)]
fn default_catcher(status: Status, _: &Request<'_>) -> Json<DefaultResponse> {
    Json(DefaultResponse {
        code: status.code,
        reason: match status.reason() {
            Some(reason) => reason,
            None => "No Reason Provided.",
        },
    })
}

#[repr(transparent)]
struct DerefRef<T>(T);

impl<T: std::ops::Deref> AsRef<[u8]> for DerefRef<T>
where
    T::Target: AsRef<[u8]>,
{
    fn as_ref(&self) -> &[u8] {
        self.0.deref().as_ref()
    }
}

enum RequestPayloads {
    Gzipped(Arc<Bytes>, u64, bool),
    Brotli(Arc<Bytes>, u64, bool),
    Proto(Arc<Bytes>, u64, CompressionEncoder, bool),
    // JSON payload response type used by config/idlist APIs (includes x-since-time).
    Plain(Arc<Bytes>, u64, bool),
    // Raw text file bytes (no JSON content type, no lcut header), used by download_id_list_file.
    Text {
        data: Arc<Bytes>,
        status: Status,
        content_range: Option<String>,
    },
    RangeNotSatisfiable {
        complete_length: usize,
    },
    // Plain-text error response used by endpoints like download_config_specs_deltas.
    Error(Status, &'static str),
    Unauthorized(),
}

impl<'r> Responder<'r, 'static> for RequestPayloads {
    fn respond_to(self, _req: &'r Request) -> Result<Response<'static>, Status> {
        match self {
            RequestPayloads::Gzipped(data, lcut, delta_used) => {
                let mut builder = Response::build();
                builder
                    .status(Status::Ok)
                    .header(ContentType::JSON)
                    .header(rocket::http::Header::new("Content-Encoding", "gzip"))
                    .header(rocket::http::Header::new("x-since-time", lcut.to_string()));

                attach_delta_used_header(&mut builder, delta_used);

                builder
                    .sized_body(data.len(), Cursor::new(DerefRef(data)))
                    .ok()
            }
            RequestPayloads::Brotli(data, lcut, delta_used) => {
                let mut builder = Response::build();
                builder
                    .status(Status::Ok)
                    .header(ContentType::JSON)
                    .header(rocket::http::Header::new("Content-Encoding", "br"))
                    .header(rocket::http::Header::new("x-since-time", lcut.to_string()));

                attach_delta_used_header(&mut builder, delta_used);

                builder
                    .sized_body(data.len(), Cursor::new(DerefRef(data)))
                    .ok()
            }
            RequestPayloads::Proto(data, lcut, encoding, delta_used) => {
                let mut builder = Response::build();
                builder.status(Status::Ok).header(
                    match ContentType::parse_flexible("application/octet-stream") {
                        Some(ct) => Header::new("Content-Type", ct.to_string()),
                        None => Header::new("Content-Type", "application/octet-stream"),
                    },
                );

                if let Some(content_encoding) = content_encoding_header_value(encoding) {
                    builder.header(rocket::http::Header::new(
                        "Content-Encoding",
                        content_encoding,
                    ));
                }

                attach_delta_used_header(&mut builder, delta_used);

                builder
                    .header(rocket::http::Header::new("x-since-time", lcut.to_string()))
                    .sized_body(data.len(), Cursor::new(DerefRef(data)))
                    .ok()
            }
            RequestPayloads::Text {
                data,
                status,
                content_range,
            } => {
                let mut builder = Response::build();
                builder
                    .status(status)
                    .header(Header::new("Content-Type", "application/octet-stream"))
                    .header(Header::new("Accept-Ranges", "bytes"));

                if let Some(content_range) = content_range {
                    builder.header(Header::new("Content-Range", content_range));
                }

                builder
                    .sized_body(data.len(), Cursor::new(DerefRef(data)))
                    .ok()
            }
            RequestPayloads::RangeNotSatisfiable { complete_length } => Response::build()
                .status(Status::RangeNotSatisfiable)
                .header(Header::new("Content-Type", "application/octet-stream"))
                .header(Header::new("Accept-Ranges", "bytes"))
                .header(Header::new(
                    "Content-Range",
                    format!("bytes */{complete_length}"),
                ))
                .sized_body(0, Cursor::new(""))
                .ok(),
            RequestPayloads::Plain(data, lcut, delta_used) => {
                let mut builder = Response::build();
                builder
                    .status(Status::Ok)
                    .header(ContentType::JSON)
                    .header(rocket::http::Header::new("x-since-time", lcut.to_string()));

                attach_delta_used_header(&mut builder, delta_used);

                // Double-layer SFP compatibility: first-layer DCS update detection relies on this
                // header to identify no-update responses.
                if data.as_ref().as_ref() == DCS_NO_UPDATE_JSON {
                    builder.header(rocket::http::Header::new("x-cache-hit", "true"));
                }

                builder
                    .sized_body(data.len(), Cursor::new(DerefRef(data)))
                    .ok()
            }
            RequestPayloads::Unauthorized() => Response::build()
                .status(Status::Unauthorized)
                .header(ContentType::Plain)
                .sized_body(
                    UNAUTHORIZED_RESPONSE.data.len(),
                    Cursor::new(&*UNAUTHORIZED_RESPONSE.data),
                )
                .ok(),
            RequestPayloads::Error(status, body) => Response::build()
                .status(status)
                .header(ContentType::Plain)
                .sized_body(body.len(), Cursor::new(body))
                .ok(),
        }
    }
}

fn attach_delta_used_header(builder: &mut rocket::response::Builder<'_>, delta_used: bool) {
    if delta_used {
        builder.header(Header::new("x-deltas-used", "true"));
    }
}

fn to_request_payload(payload: &Arc<ResponsePayload>, lcut: u64) -> RequestPayloads {
    to_request_payload_with_delta_used(payload, lcut, false)
}

fn to_request_payload_with_delta_used(
    payload: &Arc<ResponsePayload>,
    lcut: u64,
    delta_used: bool,
) -> RequestPayloads {
    if payload.use_proto {
        RequestPayloads::Proto(
            Arc::clone(&payload.data),
            lcut,
            *payload.encoding,
            delta_used,
        )
    } else if *payload.encoding == CompressionEncoder::Gzip {
        RequestPayloads::Gzipped(Arc::clone(&payload.data), lcut, delta_used)
    } else if *payload.encoding == CompressionEncoder::Brotli {
        RequestPayloads::Brotli(Arc::clone(&payload.data), lcut, delta_used)
    } else {
        RequestPayloads::Plain(Arc::clone(&payload.data), lcut, delta_used)
    }
}

fn build_download_id_list_file_payload(
    request_context: &Arc<AuthorizedRequestContext>,
    data: Arc<Bytes>,
) -> RequestPayloads {
    let complete_length = data.len();
    let Some(range_start) = request_context.range_start else {
        return RequestPayloads::Text {
            data,
            status: Status::Ok,
            content_range: None,
        };
    };

    let start = usize::try_from(range_start).unwrap_or(usize::MAX);
    if start >= complete_length {
        return RequestPayloads::RangeNotSatisfiable { complete_length };
    }

    RequestPayloads::Text {
        data: Arc::new(data.slice(start..)),
        status: Status::PartialContent,
        content_range: Some(format!(
            "bytes {start}-{}/{}",
            complete_length - 1,
            complete_length
        )),
    }
}

#[derive(Debug)]
enum DeltaPayloadError {
    DeltasDisabled,
    UnsupportedPath,
    SupportsProtoRequired,
    MissingSinceTime,
    ZeroSinceTime,
    SdkKeyNotRegistered,
    NoCachedDeltas,
    SinceTimeTooOld {
        since_time: u64,
        earliest_since_time: u64,
    },
    SerializationFailed,
}

impl DeltaPayloadError {
    fn http_status(&self) -> Status {
        match self {
            DeltaPayloadError::DeltasDisabled => Status::ServiceUnavailable,
            DeltaPayloadError::UnsupportedPath => Status::InternalServerError,
            DeltaPayloadError::SupportsProtoRequired => Status::BadRequest,
            DeltaPayloadError::MissingSinceTime => Status::BadRequest,
            DeltaPayloadError::ZeroSinceTime => Status::BadRequest,
            DeltaPayloadError::SdkKeyNotRegistered => Status::Conflict,
            DeltaPayloadError::NoCachedDeltas => Status::Conflict,
            DeltaPayloadError::SinceTimeTooOld { .. } => Status::Conflict,
            DeltaPayloadError::SerializationFailed => Status::InternalServerError,
        }
    }

    fn body(&self) -> &'static str {
        match self {
            DeltaPayloadError::DeltasDisabled => "Delta serving disabled",
            DeltaPayloadError::UnsupportedPath => "Delta payload requested for unsupported path",
            DeltaPayloadError::SupportsProtoRequired => "supports_proto=true is required",
            DeltaPayloadError::MissingSinceTime => "Missing sinceTime parameter",
            DeltaPayloadError::ZeroSinceTime => {
                "sinceTime must be greater than 0 for download_config_specs_deltas"
            }
            DeltaPayloadError::SdkKeyNotRegistered => "SDK key not registered for delta tracking",
            DeltaPayloadError::NoCachedDeltas => "No cached deltas available yet",
            DeltaPayloadError::SinceTimeTooOld { .. } => {
                "sinceTime is older than the retained delta history"
            }
            DeltaPayloadError::SerializationFailed => "Failed to serialize delta payload",
        }
    }
}

fn validate_direct_delta_request(
    rc: &Arc<crate::servers::authorized_request_context::AuthorizedRequestContext>,
    since_time: Option<u64>,
) -> Result<u64, DeltaPayloadError> {
    if !rc.supports_proto {
        return Err(DeltaPayloadError::SupportsProtoRequired);
    }

    match since_time {
        Some(0) => Err(DeltaPayloadError::ZeroSinceTime),
        Some(since_time) => Ok(since_time),
        None => Err(DeltaPayloadError::MissingSinceTime),
    }
}

fn try_build_delta_payload(
    cli: &Cli,
    deltas_store: &Arc<DeltasStore>,
    rc: &Arc<crate::servers::authorized_request_context::AuthorizedRequestContext>,
    since_time: u64,
    no_update_payload: &Arc<ResponsePayload>,
) -> Result<RequestPayloads, DeltaPayloadError> {
    if !cli.deltas_background_loop_enabled {
        return Err(DeltaPayloadError::DeltasDisabled);
    }

    if !matches!(
        rc.path,
        NormalizedPath::V2DownloadConfigSpecs | NormalizedPath::V2DownloadConfigSpecsDeltas
    ) {
        return Err(DeltaPayloadError::UnsupportedPath);
    }

    if !rc.supports_proto {
        return Err(DeltaPayloadError::SupportsProtoRequired);
    }

    match deltas_store.build_combined_delta_proto_payload(rc, since_time) {
        DeltaProtoBuildResult::SdkKeyNotRegistered => Err(DeltaPayloadError::SdkKeyNotRegistered),
        DeltaProtoBuildResult::NoCachedDeltas => Err(DeltaPayloadError::NoCachedDeltas),
        DeltaProtoBuildResult::SinceTimeTooOld {
            earliest_since_time,
        } => Err(DeltaPayloadError::SinceTimeTooOld {
            since_time,
            earliest_since_time,
        }),
        DeltaProtoBuildResult::NoUpdates { lcut } => Ok(to_request_payload_with_delta_used(
            no_update_payload,
            lcut,
            true,
        )),
        DeltaProtoBuildResult::SerializationFailed => {
            let mut sdk_key = rc.sdk_key.clone();
            sdk_key.truncate(20);
            eprintln!(
                "Failed to serialize combined delta proto envelopes for sdk_key_prefix={sdk_key} at since_time={since_time}"
            );
            Err(DeltaPayloadError::SerializationFailed)
        }
        DeltaProtoBuildResult::Combined { lcut, payload } => {
            let compression = preferred_response_compression(&rc.encodings);
            let compressed = match compress_response_payload(&payload, compression) {
                Some(payload) => payload,
                None => {
                    return Ok(to_request_payload_with_delta_used(
                        no_update_payload,
                        lcut,
                        true,
                    ))
                }
            };

            Ok(RequestPayloads::Proto(
                Arc::new(Bytes::from(compressed)),
                lcut,
                compression,
                true,
            ))
        }
    }
}

#[derive(Default, FromForm)]
struct DownloadConfigSpecsDeltasQuery {
    #[field(name = "sinceTime")]
    since_time: Option<u64>,
}

#[get("/download_config_specs_deltas/<sdk_key_file>?<query..>")]
#[allow(clippy::too_many_arguments)]
async fn get_download_config_specs_deltas(
    cli: &State<Cli>,
    config_spec_store: &State<Arc<ConfigSpecStore>>,
    deltas_store: &State<Option<Arc<DeltasStore>>>,
    authorized_rc_cache: &State<Arc<AuthorizedRequestContextCache>>,
    #[allow(unused_variables)] sdk_key_file: &str,
    query: Lenient<DownloadConfigSpecsDeltasQuery>,
    authorized_rc: AuthorizedRequestContextWrapper,
) -> RequestPayloads {
    let query = query.into_inner();
    let rc = authorized_rc.inner();
    let since_time = match validate_direct_delta_request(&rc, query.since_time) {
        Ok(since_time) => since_time,
        Err(error) => return RequestPayloads::Error(error.http_status(), error.body()),
    };

    shadow_fetch_json_config_spec(
        authorized_rc_cache.inner().clone(),
        &rc,
        config_spec_store.inner(),
        since_time,
    );

    if let Some(deltas_store) = deltas_store.inner().as_ref() {
        deltas_store.register_sdk_key(&rc, since_time);

        return match try_build_delta_payload(
            cli.inner(),
            deltas_store,
            &rc,
            since_time,
            &config_spec_store.no_update_config,
        ) {
            Ok(delta_payload) => delta_payload,
            Err(error) => {
                if let DeltaPayloadError::SinceTimeTooOld {
                    since_time,
                    earliest_since_time,
                } = error
                {
                    eprintln!(
                        "Delta request sinceTime too old for sdk_key_prefix={} (since_time={}, earliest_retained_since_time={})",
                        &rc.sdk_key.chars().take(20).collect::<String>(),
                        since_time,
                        earliest_since_time
                    );
                }
                RequestPayloads::Error(error.http_status(), error.body())
            }
        };
    }

    RequestPayloads::Error(Status::ServiceUnavailable, "Deltas store unavailable")
}

#[derive(Default, FromForm)]
struct DownloadConfigSpecsQuery {
    #[field(name = "sinceTime")]
    since_time: Option<u64>,
    supports_proto: Option<bool>,
    accept_deltas: Option<bool>,
    #[allow(dead_code)]
    checksum: Option<String>,
}

#[get("/download_config_specs/<sdk_key_file>?<query..>")]
async fn get_download_config_specs(
    cli: &State<Cli>,
    config_spec_store: &State<Arc<ConfigSpecStore>>,
    deltas_store: &State<Option<Arc<DeltasStore>>>,
    #[allow(unused_variables)] sdk_key_file: &str,
    query: Lenient<DownloadConfigSpecsQuery>,
    authorized_rc: AuthorizedRequestContextWrapper,
) -> RequestPayloads {
    let query = query.into_inner();
    let since_time = query.since_time.unwrap_or(0);
    let _supports_proto = query.supports_proto;
    let rc = authorized_rc.inner();

    if query.accept_deltas.unwrap_or(false) {
        if let Some(deltas_store) = deltas_store.inner().as_ref() {
            if let Ok(delta_payload) = try_build_delta_payload(
                cli.inner(),
                deltas_store,
                &rc,
                since_time,
                &config_spec_store.no_update_config,
            ) {
                return delta_payload;
            }
        }
    }

    match config_spec_store.get_config_spec(&rc, since_time).await {
        Some(data) => to_request_payload(&data.config, data.lcut),
        None => RequestPayloads::Unauthorized(),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DcsRequest {
    pub since_time: Option<u64>,
}

#[post("/download_config_specs", format = "json", data = "<dcs_request_json>")]
async fn post_download_config_specs(
    dcs_request_json: Json<DcsRequest>,
    config_spec_store: &State<Arc<ConfigSpecStore>>,
    authorized_rc: AuthorizedRequestContextWrapper,
) -> RequestPayloads {
    let dcs_request = dcs_request_json.into_inner();
    match config_spec_store
        .get_config_spec(&authorized_rc.inner(), dcs_request.since_time.unwrap_or(0))
        .await
    {
        Some(data) => to_request_payload(&data.config, data.lcut),
        None => RequestPayloads::Unauthorized(),
    }
}

#[post("/get_id_lists")]
async fn post_get_id_lists(
    get_id_list_store: &State<Arc<GetIdListStore>>,
    authorized_rc: AuthorizedRequestContextWrapper,
) -> RequestPayloads {
    match get_id_list_store.get_id_lists(&authorized_rc.inner()).await {
        Some(data) => to_request_payload(&data.idlists, 0),
        None => RequestPayloads::Unauthorized(),
    }
}

#[get("/download_id_list_file/<_tail..>")]
async fn get_download_id_list_file(
    // Use raw URI path/query from AuthorizedRequestContext; avoid PathBuf segment sanitization on encoded file ids (%2F).
    _tail: Segments<'_, Path>,
    id_list_file_store: &State<Arc<IdListFileStore>>,
    download_id_list_file_request: DownloadIdListFileRequestGuard,
) -> RequestPayloads {
    let authorized_rc = match download_id_list_file_request {
        DownloadIdListFileRequestGuard::Authorized(authorized_rc) => authorized_rc,
        DownloadIdListFileRequestGuard::Invalid(error) => {
            return RequestPayloads::Error(Status::BadRequest, error.body());
        }
    };

    match id_list_file_store.get_or_fetch(&authorized_rc).await {
        Some(bytes) => build_download_id_list_file_payload(&authorized_rc, bytes),
        None => RequestPayloads::Unauthorized(),
    }
}

#[post("/log_event", data = "<request_body>")]
async fn post_log_event(
    log_event_store: &State<Arc<LogEventStore>>,
    request_body: LoggedBodyJSON<LogEventRequest>,
    auth_header: AuthorizedRequestContextWrapper,
) -> Custom<Json<LogEventResponse>> {
    let store_copy = log_event_store.inner().clone();
    tokio::task::spawn_blocking(move || {
        Handle::current().block_on(async move {
            let _ = store_copy
                .log_event(request_body.into_inner(), &auth_header.inner())
                .await;
        });
    });

    Custom(
        Status::Accepted,
        Json(LogEventResponse {
            success: true,
            message: None,
        }),
    )
}

pub struct HttpServer {}

pub struct HttpServerDependencies {
    pub config_spec_store: Arc<ConfigSpecStore>,
    pub deltas_store: Option<Arc<DeltasStore>>,
    pub log_event_store: Arc<LogEventStore>,
    pub id_list_store: Arc<GetIdListStore>,
    pub id_list_file_store: Arc<IdListFileStore>,
    pub rc_cache: Arc<AuthorizedRequestContextCache>,
    pub sdk_key_store: Arc<SdkKeyStore>,
}
impl HttpServer {
    pub async fn start_server(
        cli: &Cli,
        deps: HttpServerDependencies,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let HttpServerDependencies {
            config_spec_store,
            deltas_store,
            log_event_store,
            id_list_store,
            id_list_file_store,
            rc_cache,
            sdk_key_store,
        } = deps;
        let sdk_key_cache = SdkKeyCache(RwLock::new(HashMap::new()));

        rocket::build()
            .mount(
                "/v1",
                routes![
                    get_download_config_specs,
                    post_download_config_specs,
                    post_get_id_lists,
                    get_download_id_list_file,
                    post_log_event,
                    http_apis::healthchecks::startup,
                    http_apis::healthchecks::ready,
                    http_apis::healthchecks::health
                ],
            )
            .mount(
                "/v2",
                routes![
                    get_download_config_specs,
                    get_download_config_specs_deltas,
                    post_download_config_specs,
                ],
            )
            .manage(config_spec_store)
            .manage(deltas_store)
            .manage(log_event_store)
            .manage(id_list_store)
            .manage(id_list_file_store)
            .manage(rc_cache)
            .manage(sdk_key_store)
            .manage(sdk_key_cache)
            .manage(cli.clone())
            .attach(RequestLoggingRequestFairing)
            .attach(sdk_key_normalization_fairing())
            .attach(RequestLoggingResponseFairing)
            .register("/", catchers![default_catcher])
            .launch()
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_download_id_list_file_payload, to_request_payload,
        to_request_payload_with_delta_used, validate_direct_delta_request,
        DownloadConfigSpecsDeltasQuery, DownloadConfigSpecsQuery, RequestPayloads,
    };
    use crate::datastore::data_providers::http_data_provider::ResponsePayload;
    use crate::servers::authorized_request_context::AuthorizedRequestContext;
    use crate::servers::normalized_path::NormalizedPath;
    use crate::utils::compress_encoder::CompressionEncoder;
    use bytes::Bytes;
    use rocket::form::Lenient;
    use rocket::http::Status;
    use rocket::local::blocking::Client;
    use rocket::{get, routes};
    use std::sync::Arc;

    fn make_id_list_rc(range_start: Option<u64>) -> Arc<AuthorizedRequestContext> {
        Arc::new(
            AuthorizedRequestContext::new(
                "secret-key".to_string(),
                NormalizedPath::V1DownloadIdListFile,
                vec![CompressionEncoder::PlainText],
            )
            .with_raw_request(
                Some("/v1/download_id_list_file/file_123".to_string()),
                Some("k=secret-key".to_string()),
            )
            .with_id_list_request(Some("file_123".to_string()), range_start, Some(6)),
        )
    }

    fn make_delta_rc(supports_proto: bool) -> Arc<AuthorizedRequestContext> {
        Arc::new(
            AuthorizedRequestContext::new(
                "secret-key".to_string(),
                NormalizedPath::V2DownloadConfigSpecsDeltas,
                vec![CompressionEncoder::Gzip],
            )
            .with_request_capabilities(supports_proto, true),
        )
    }

    #[test]
    fn build_download_id_list_file_payload_returns_partial_content_for_valid_range() {
        let payload = build_download_id_list_file_payload(
            &make_id_list_rc(Some(2)),
            Arc::new(Bytes::from_static(b"abcdef")),
        );

        match payload {
            RequestPayloads::Text {
                data,
                status,
                content_range,
            } => {
                assert_eq!(status, Status::PartialContent);
                assert_eq!(data.as_ref(), &Bytes::from_static(b"cdef"));
                assert_eq!(content_range.as_deref(), Some("bytes 2-5/6"));
            }
            _ => panic!("expected partial text payload"),
        }
    }

    #[test]
    fn build_download_id_list_file_payload_returns_416_for_unsatisfiable_range() {
        let payload = build_download_id_list_file_payload(
            &make_id_list_rc(Some(6)),
            Arc::new(Bytes::from_static(b"abcdef")),
        );

        match payload {
            RequestPayloads::RangeNotSatisfiable { complete_length } => {
                assert_eq!(complete_length, 6);
            }
            _ => panic!("expected range not satisfiable payload"),
        }
    }

    #[test]
    fn to_request_payload_preserves_gzip_response_payloads() {
        let payload = Arc::new(ResponsePayload {
            encoding: Arc::new(CompressionEncoder::Gzip),
            use_proto: false,
            data: Arc::new(Bytes::from_static(b"compressed")),
        });

        match to_request_payload(&payload, 0) {
            RequestPayloads::Gzipped(data, lcut, delta_used) => {
                assert_eq!(lcut, 0);
                assert!(!delta_used);
                assert_eq!(data.as_ref(), &Bytes::from_static(b"compressed"));
            }
            _ => panic!("expected gzipped request payload"),
        }
    }

    #[test]
    fn direct_delta_request_requires_supports_proto_before_since_time() {
        let rc = make_delta_rc(false);
        let error = validate_direct_delta_request(&rc, None).expect_err("expected error");

        assert_eq!(error.http_status(), Status::BadRequest);
        assert_eq!(error.body(), "supports_proto=true is required");
    }

    #[test]
    fn direct_delta_request_rejects_missing_since_time() {
        let rc = make_delta_rc(true);
        let error = validate_direct_delta_request(&rc, None).expect_err("expected error");

        assert_eq!(error.http_status(), Status::BadRequest);
        assert_eq!(error.body(), "Missing sinceTime parameter");
    }

    #[test]
    fn direct_delta_request_rejects_zero_since_time() {
        let rc = make_delta_rc(true);
        let error = validate_direct_delta_request(&rc, Some(0)).expect_err("expected error");

        assert_eq!(error.http_status(), Status::BadRequest);
        assert_eq!(
            error.body(),
            "sinceTime must be greater than 0 for download_config_specs_deltas"
        );
    }

    #[test]
    fn direct_delta_request_accepts_positive_since_time() {
        let rc = make_delta_rc(true);

        assert_eq!(
            validate_direct_delta_request(&rc, Some(123)).expect("expected sinceTime"),
            123
        );
    }

    #[get("/delta")]
    fn delta_payload_route() -> RequestPayloads {
        let payload = Arc::new(ResponsePayload {
            encoding: Arc::new(CompressionEncoder::PlainText),
            use_proto: false,
            data: Arc::new(Bytes::from_static(br#"{"has_updates":false}"#)),
        });

        to_request_payload_with_delta_used(&payload, 123, true)
    }

    #[get("/standard")]
    fn standard_payload_route() -> RequestPayloads {
        let payload = Arc::new(ResponsePayload {
            encoding: Arc::new(CompressionEncoder::PlainText),
            use_proto: false,
            data: Arc::new(Bytes::from_static(br#"{"has_updates":false}"#)),
        });

        to_request_payload(&payload, 123)
    }

    #[get("/parse_dcs_query?<query..>")]
    fn parse_dcs_query(query: Lenient<DownloadConfigSpecsQuery>) -> String {
        let query = query.into_inner();
        format!(
            "{:?}|{:?}|{:?}|{:?}",
            query.since_time, query.supports_proto, query.accept_deltas, query.checksum
        )
    }

    #[get("/parse_dcs_delta_query?<query..>")]
    fn parse_dcs_delta_query(query: Lenient<DownloadConfigSpecsDeltasQuery>) -> String {
        format!("{:?}", query.into_inner().since_time)
    }

    #[test]
    fn delta_payloads_include_x_deltas_used_header() {
        let client = Client::tracked(
            rocket::build().mount("/", routes![delta_payload_route, standard_payload_route]),
        )
        .expect("client");

        let response = client.get("/delta").dispatch();
        assert_eq!(response.headers().get_one("x-deltas-used"), Some("true"));
    }

    #[test]
    fn standard_payloads_do_not_include_x_deltas_used_header() {
        let client = Client::tracked(
            rocket::build().mount("/", routes![delta_payload_route, standard_payload_route]),
        )
        .expect("client");

        let response = client.get("/standard").dispatch();
        assert_eq!(response.headers().get_one("x-deltas-used"), None);
    }

    #[test]
    fn download_config_specs_query_ignores_unknown_checksum_field() {
        let client =
            Client::tracked(rocket::build().mount("/", routes![parse_dcs_query])).expect("client");

        let response = client
            .get("/parse_dcs_query?sinceTime=123&supports_proto=true&checksum=abc&accept_deltas=true")
            .dispatch();

        assert_eq!(
            response.into_string().as_deref(),
            Some("Some(123)|Some(true)|Some(true)|Some(\"abc\")")
        );
    }

    #[test]
    fn download_config_specs_deltas_query_parses_since_time() {
        let client = Client::tracked(rocket::build().mount("/", routes![parse_dcs_delta_query]))
            .expect("client");

        let response = client
            .get("/parse_dcs_delta_query?sinceTime=123&supports_proto=true")
            .dispatch();

        assert_eq!(response.into_string().as_deref(), Some("Some(123)"));
    }

    #[test]
    fn download_config_specs_deltas_query_treats_malformed_since_time_as_missing() {
        let client = Client::tracked(rocket::build().mount("/", routes![parse_dcs_delta_query]))
            .expect("client");

        let response = client
            .get("/parse_dcs_delta_query?sinceTime=not-a-number&supports_proto=true")
            .dispatch();

        assert_eq!(response.into_string().as_deref(), Some("None"));
    }
}
