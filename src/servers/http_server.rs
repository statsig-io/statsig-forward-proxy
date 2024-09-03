use std::sync::Arc;

use crate::datastore::get_id_list_store::GetIdListStore;

use crate::datastore::log_event_store::LogEventStore;
use crate::datatypes::gzip_data::LoggedBodyJSON;
use crate::datatypes::log_event::LogEventRequest;
use crate::datatypes::log_event::LogEventResponse;
use crate::observers::EventStat;
use crate::observers::OperationType;
use crate::observers::{ProxyEvent, ProxyEventType};
use crate::servers::http_apis;
use rocket::fairing::AdHoc;

use rocket::http::StatusClass;
use rocket::http::{Header, Status};
use rocket::post;
use rocket::request::Outcome;
use rocket::request::{self, FromRequest, Request};
use rocket::response::status::Custom;
use rocket::routes;

use rocket::serde::json::Json;

use rocket::State;
use rocket::{catch, catchers, get};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::datastore::config_spec_store::ConfigSpecStore;
use crate::observers::proxy_event_observer::ProxyEventObserver;

pub struct TimerStart(pub Option<Instant>);

#[derive(Debug)]
pub struct AuthError;

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
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
            path: normalized_path.clone(),
            use_lcut,
        }
    }
}

impl std::fmt::Display for AuthorizedRequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}|{}", self.sdk_key, self.path)
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for AuthorizedRequestContext {
    type Error = AuthError;

    async fn from_request(request: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        let headers = request.headers();

        match headers.get_one("statsig-api-key") {
            Some(sdk_key) => Outcome::Success(AuthorizedRequestContext::new(
                sdk_key.to_string(),
                request.uri().path().to_string(),
            )),
            None => Outcome::Error((Status::BadRequest, AuthError)),
        }
    }
}

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

#[get("/download_config_specs/<sdk_key_file>?<sinceTime>")]
async fn get_download_config_specs(
    config_spec_store: &State<Arc<ConfigSpecStore>>,
    #[allow(unused_variables)] sdk_key_file: &str,
    #[allow(non_snake_case)] sinceTime: Option<u64>,
    authorized_rc: AuthorizedRequestContext,
) -> Result<String, Status> {
    match config_spec_store
        .get_config_spec(&authorized_rc, sinceTime.unwrap_or(0))
        .await
    {
        Some(data) => Ok(data.read().await.config.to_string()),
        None => Err(Status::Unauthorized),
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
    authorized_rc: AuthorizedRequestContext,
) -> Result<String, Status> {
    let dcs_request = dcs_request_json.into_inner();
    match config_spec_store
        .get_config_spec(&authorized_rc, dcs_request.since_time.unwrap_or(0))
        .await
    {
        Some(data) => Ok(data.read().await.config.to_string()),
        None => Err(Status::Unauthorized),
    }
}

#[post("/get_id_lists")]
async fn post_get_id_lists(
    get_id_list_store: &State<Arc<GetIdListStore>>,
    authorized_rc: AuthorizedRequestContext,
) -> Result<String, Status> {
    match get_id_list_store.get_id_lists(&authorized_rc).await {
        Some(data) => Ok(data.read().await.idlists.to_string()),
        None => Err(Status::Unauthorized),
    }
}

#[post("/log_event", data = "<request_body>")]
async fn post_log_event(
    log_event_store: &State<Arc<LogEventStore>>,
    request_body: LoggedBodyJSON<LogEventRequest>,
    auth_header: AuthorizedRequestContext,
) -> Custom<Json<LogEventResponse>> {
    let store = log_event_store.inner().clone();
    tokio::spawn(async move {
        let body = request_body.into_inner();
        let _ = store.log_event(&auth_header.sdk_key, body).await;
    });
    Custom(Status::Accepted, Json(LogEventResponse { success: true }))
}

pub struct HttpServer {}

impl HttpServer {
    pub async fn start_server(
        config_spec_store: Arc<ConfigSpecStore>,
        id_list_store: Arc<GetIdListStore>,
        log_event_store: Arc<LogEventStore>,
    ) -> Result<(), Box<dyn std::error::Error>> {
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
                routes![get_download_config_specs, post_download_config_specs],
            )
            .manage(config_spec_store)
            .manage(id_list_store)
            .manage(log_event_store)
            .attach(AdHoc::on_request("Normalize SDK Key", |req, _| {
                Box::pin(async move {
                    req.local_cache(|| TimerStart(Some(Instant::now())));

                    if req.headers().contains("statsig-api-key") {
                        return;
                    }

                    if req.method() == rocket::http::Method::Get {
                        let mut path = req.uri().path().to_string();
                        if path.ends_with(".json") || path.ends_with(".js") {
                            if let Some(pos) = path.rfind('/') {
                                path = path[pos..].to_string();
                            }
                        }
                        let sdk_key = path
                            .strip_suffix(".json")
                            .unwrap_or_else(|| path.strip_suffix(".js").unwrap_or(&path));
                        req.add_header(Header::new("statsig-api-key", sdk_key.to_string()));
                    }
                })
            }))
            .attach(AdHoc::on_response("Logger", |req, resp| {
                Box::pin(async move {
                    let start_time = req.local_cache(|| TimerStart(None));
                    let mut ms = -2;
                    if let Some(duration) = start_time.0.map(|st| st.elapsed()) {
                        ms = match i64::try_from(
                            duration.as_secs() * 1000 + (duration.subsec_millis() as u64),
                        ) {
                            Ok(ms) => ms,
                            Err(_err) => -2,
                        };
                    }

                    let request_context = AuthorizedRequestContext::new(
                        req.headers()
                            .get_one("statsig-api-key")
                            .unwrap_or("no-key-provided")
                            .to_string(),
                        req.uri().path().to_string(),
                    );
                    if resp.status().class() == StatusClass::Success {
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new(
                                ProxyEventType::HttpServerRequestSuccess,
                                &request_context,
                            )
                            .with_stat(EventStat {
                                operation_type: OperationType::Distribution,
                                value: ms,
                            }),
                        )
                        .await;
                    } else {
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new(
                                ProxyEventType::HttpServerRequestFailed,
                                &request_context,
                            )
                            .with_stat(EventStat {
                                operation_type: OperationType::Distribution,
                                value: ms,
                            })
                            .with_status_code(resp.status().code),
                        )
                        .await;
                    }
                })
            }))
            .register("/", catchers![default_catcher])
            .launch()
            .await?;
        Ok(())
    }
}
