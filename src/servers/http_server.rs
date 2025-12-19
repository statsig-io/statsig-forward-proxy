use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;

use crate::datastore::config_spec_store::shadow_fetch_json_config_spec;
use crate::datastore::id_list_store::GetIdListStore;
use crate::datastore::log_event_store::LogEventStore;
use crate::datastore::sdk_key_store::SdkKeyStore;

use crate::datatypes::gzip_data::LoggedBodyJSON;
use crate::datatypes::log_event::LogEventRequest;
use crate::datatypes::log_event::LogEventResponse;
use crate::http_data_provider::ResponsePayload;
use crate::observers::EventStat;
use crate::observers::OperationType;
use crate::observers::{ProxyEvent, ProxyEventType};
use crate::servers::http_apis;
use crate::utils::compress_encoder::convert_compression_encodings_from_header_map;
use crate::utils::compress_encoder::CompressionEncoder;
use crate::utils::request_helper::does_request_supports_proto;
use crate::Cli;
use bytes::Bytes;

use rocket::fairing::AdHoc;

use rocket::http::ContentType;
use rocket::http::StatusClass;
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
use tokio::time::Instant;

use crate::datastore::config_spec_store::ConfigSpecStore;
use crate::observers::proxy_event_observer::ProxyEventObserver;
use lazy_static::lazy_static;

lazy_static! {
    static ref UNAUTHORIZED_RESPONSE: Arc<ResponsePayload> = Arc::new(ResponsePayload {
        encoding: Arc::new(CompressionEncoder::PlainText),
        data: Arc::from(Bytes::from("Unauthorized")),
        use_proto: false
    });
}

// Import the new module
use crate::servers::authorized_request_context::{
    AuthorizedRequestContextCache, AuthorizedRequestContextWrapper,
};

use super::normalized_path::NormalizedPath;

pub struct TimerStart(pub Option<Instant>);

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
    Gzipped(Arc<Bytes>, u64),
    Brotli(Arc<Bytes>, u64),
    Proto(Arc<Bytes>, u64),
    Plain(Arc<Bytes>, u64),
    Unauthorized(),
}

impl<'r> Responder<'r, 'static> for RequestPayloads {
    fn respond_to(self, _req: &'r Request) -> Result<Response<'static>, Status> {
        match self {
            RequestPayloads::Gzipped(data, lcut) => Response::build()
                .status(Status::Ok)
                .header(ContentType::JSON)
                .header(rocket::http::Header::new("Content-Encoding", "gzip"))
                .header(rocket::http::Header::new("x-since-time", lcut.to_string()))
                .sized_body(data.len(), Cursor::new(DerefRef(data)))
                .ok(),
            RequestPayloads::Brotli(data, lcut) => Response::build()
                .status(Status::Ok)
                .header(ContentType::JSON)
                .header(rocket::http::Header::new("Content-Encoding", "br"))
                .header(rocket::http::Header::new("x-since-time", lcut.to_string()))
                .sized_body(data.len(), Cursor::new(DerefRef(data)))
                .ok(),
            RequestPayloads::Proto(data, lcut) => Response::build()
                .status(Status::Ok)
                .header(rocket::http::Header::new("Content-Encoding", "statsig-br"))
                .header(
                    match ContentType::parse_flexible("application/octet-stream") {
                        Some(ct) => Header::new("Content-Type", ct.to_string()), // use parsed ContentType
                        None => Header::new("Content-Type", "application/octet-stream"), // fallback
                    },
                )
                .header(rocket::http::Header::new("x-since-time", lcut.to_string()))
                .sized_body(data.len(), Cursor::new(DerefRef(data)))
                .ok(),
            RequestPayloads::Plain(data, lcut) => Response::build()
                .status(Status::Ok)
                .header(ContentType::JSON)
                .header(rocket::http::Header::new("x-since-time", lcut.to_string()))
                .sized_body(data.len(), Cursor::new(DerefRef(data)))
                .ok(),
            RequestPayloads::Unauthorized() => Response::build()
                .status(Status::Unauthorized)
                .header(ContentType::Plain)
                .sized_body(
                    UNAUTHORIZED_RESPONSE.data.len(),
                    Cursor::new(&*UNAUTHORIZED_RESPONSE.data),
                )
                .ok(),
        }
    }
}

#[get("/download_config_specs/<sdk_key_file>?<sinceTime>&<supports_proto>")]
async fn get_download_config_specs(
    config_spec_store: &State<Arc<ConfigSpecStore>>,
    authorized_rc_cache: &State<Arc<AuthorizedRequestContextCache>>,
    #[allow(unused_variables)] sdk_key_file: &str,
    #[allow(non_snake_case)] sinceTime: Option<u64>,
    #[allow(unused_variables)] supports_proto: Option<bool>,
    authorized_rc: AuthorizedRequestContextWrapper,
) -> RequestPayloads {
    if supports_proto.unwrap_or(false) {
        shadow_fetch_json_config_spec(
            authorized_rc_cache.inner().clone(),
            &authorized_rc.inner(),
            config_spec_store.inner(),
            sinceTime.unwrap_or(0),
        );
    }
    match config_spec_store
        .get_config_spec(&authorized_rc.inner(), sinceTime.unwrap_or(0))
        .await
    {
        Some(data) => match &data.config.use_proto {
            true => RequestPayloads::Proto(Arc::clone(&data.config.data), data.lcut),
            false => {
                if *data.config.encoding == CompressionEncoder::Gzip {
                    RequestPayloads::Gzipped(Arc::clone(&data.config.data), data.lcut)
                } else if *data.config.encoding == CompressionEncoder::Brotli {
                    RequestPayloads::Brotli(Arc::clone(&data.config.data), data.lcut)
                } else {
                    RequestPayloads::Plain(Arc::clone(&data.config.data), data.lcut)
                }
            }
        },
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
        Some(data) => {
            if *data.config.encoding == CompressionEncoder::Gzip {
                RequestPayloads::Gzipped(Arc::clone(&data.config.data), data.lcut)
            } else {
                RequestPayloads::Plain(Arc::clone(&data.config.data), data.lcut)
            }
        }
        None => RequestPayloads::Unauthorized(),
    }
}

#[post("/get_id_lists")]
async fn post_get_id_lists(
    get_id_list_store: &State<Arc<GetIdListStore>>,
    authorized_rc: AuthorizedRequestContextWrapper,
) -> RequestPayloads {
    match get_id_list_store.get_id_lists(&authorized_rc.inner()).await {
        Some(data) => RequestPayloads::Plain(Arc::clone(&data.idlists.data), 0),
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

pub struct SdkKeyCache(pub RwLock<HashMap<String, String>>);

impl HttpServer {
    pub async fn start_server(
        cli: &Cli,
        config_spec_store: Arc<ConfigSpecStore>,
        log_event_store: Arc<LogEventStore>,
        id_list_store: Arc<GetIdListStore>,
        rc_cache: Arc<AuthorizedRequestContextCache>,
        sdk_key_store: Arc<SdkKeyStore>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let sdk_key_cache = SdkKeyCache(RwLock::new(HashMap::new()));

        rocket::build()
            .mount(
                "/v1",
                routes![
                    get_download_config_specs,
                    post_download_config_specs,
                    post_get_id_lists,
                    post_log_event,
                    http_apis::healthchecks::startup,
                    http_apis::healthchecks::ready,
                    http_apis::healthchecks::health
                ],
            )
            .mount(
                "/v2",
                routes![get_download_config_specs, post_download_config_specs,],
            )
            .manage(config_spec_store)
            .manage(log_event_store)
            .manage(id_list_store)
            .manage(rc_cache)
            .manage(sdk_key_store)
            .manage(sdk_key_cache)
            .manage(cli.clone())
            .attach(AdHoc::on_request("Normalize SDK Key", |req, _| {
                Box::pin(async move {
                    req.local_cache(|| TimerStart(Some(Instant::now())));

                    if req.headers().contains("statsig-api-key") {
                        return;
                    }

                    if req.method() == rocket::http::Method::Get {
                        let path = req.uri().path().to_string();
                        let sdk_key_cache = req.rocket().state::<SdkKeyCache>().unwrap();

                        // Try to read from the cache first
                        if let Some(sdk_key) = sdk_key_cache.0.read().await.get(&path).cloned() {
                            req.add_header(Header::new("statsig-api-key", sdk_key));
                            return;
                        }

                        // If not in cache, compute the new key
                        let new_key = path
                            .strip_suffix(".json")
                            .or_else(|| path.strip_suffix(".js"))
                            .unwrap_or(&path)
                            .rsplit_once('/')
                            .map_or(path.clone(), |(_, key)| key.to_string());

                        // Insert the new key into the cache
                        sdk_key_cache.0.write().await.insert(path, new_key.clone());

                        req.add_header(Header::new("statsig-api-key", new_key));
                    }
                })
            }))
            .attach(AdHoc::on_response("Logger", |req, resp| {
                Box::pin(async move {
                    let ms = req
                        .local_cache(|| TimerStart(Some(Instant::now())))
                        .0
                        .map_or(-2, |start| {
                            start.elapsed().as_millis().try_into().unwrap_or(-2)
                        });

                    let cache = req
                        .rocket()
                        .state::<Arc<AuthorizedRequestContextCache>>()
                        .unwrap()
                        .clone();
                    let sdk_key = req
                        .headers()
                        .get_one("statsig-api-key")
                        .unwrap_or("no-key-provided")
                        .to_string();
                    let encodings = convert_compression_encodings_from_header_map(
                        req.headers().get("Accept-Encoding"),
                    );
                    let lcut = resp
                        .headers()
                        .get_one("x-since-time")
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(0);
                    let path = NormalizedPath::from(req.uri().path().as_str());
                    let status_code = resp.status().code;
                    let status_class = resp.status().class();
                    let content_encoding = resp.headers().get("Content-Encoding").collect();
                    let supports_proto = does_request_supports_proto(req);
                    // Spawn a new task to handle logging
                    tokio::spawn(async move {
                        let request_context =
                            cache.get_or_insert(sdk_key, path, encodings, supports_proto);

                        let event = ProxyEvent::new_with_rc(
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
                        .with_stat(EventStat {
                            operation_type: OperationType::Distribution,
                            value: ms,
                        });

                        ProxyEventObserver::publish_event(event);
                    });
                })
            }))
            .register("/", catchers![default_catcher])
            .launch()
            .await?;
        Ok(())
    }
}
