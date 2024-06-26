use std::sync::Arc;

use crate::datastore::get_id_list_store::GetIdListStore;
use crate::observers::EventStat;
use crate::observers::OperationType;
use crate::observers::{ProxyEvent, ProxyEventType};
use crate::servers::http_apis;
use rocket::fairing::AdHoc;
use rocket::http::{Header, Status};
use rocket::post;
use rocket::request::Outcome;
use rocket::request::{self, FromRequest, Request};
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
#[derive(Clone, Debug)]
pub struct AuthHeader {
    sdk_key: String,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for AuthHeader {
    type Error = AuthError;

    async fn from_request(request: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        let headers = request.headers();

        match headers.get_one("statsig-api-key") {
            Some(sdk_key) => Outcome::Success(AuthHeader {
                sdk_key: sdk_key.to_string(),
            }),
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
    auth_header: AuthHeader,
) -> Result<String, Status> {
    match config_spec_store
        .get_config_spec(&auth_header.sdk_key, sinceTime.unwrap_or(0))
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
    auth_header: AuthHeader,
) -> Result<String, Status> {
    let dcs_request = dcs_request_json.into_inner();
    match config_spec_store
        .get_config_spec(&auth_header.sdk_key, dcs_request.since_time.unwrap_or(0))
        .await
    {
        Some(data) => Ok(data.read().await.config.to_string()),
        None => Err(Status::Unauthorized),
    }
}

#[post("/get_id_lists")]
async fn post_get_id_lists(
    get_id_list_store: &State<Arc<GetIdListStore>>,
    auth_header: AuthHeader,
) -> Result<String, Status> {
    match get_id_list_store.get_id_lists(&auth_header.sdk_key).await {
        Some(data) => Ok(data.read().await.idlists.to_string()),
        None => Err(Status::Unauthorized),
    }
}

pub struct HttpServer {}

impl HttpServer {
    pub async fn start_server(
        config_spec_store: Arc<ConfigSpecStore>,
        id_list_store: Arc<GetIdListStore>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        rocket::build()
            .mount(
                "/v1",
                routes![
                    get_download_config_specs,
                    post_download_config_specs,
                    post_get_id_lists,
                    http_apis::healthchecks::startup,
                    http_apis::healthchecks::ready,
                    http_apis::healthchecks::health,
                ],
            )
            .manage(config_spec_store)
            .manage(id_list_store)
            .attach(AdHoc::on_request("Normalize SDK Key", |req, _| {
                Box::pin(async move {
                    req.local_cache(|| TimerStart(Some(Instant::now())));

                    if req.headers().contains("statsig-api-key") {
                        return;
                    }

                    if req.method() == rocket::http::Method::Get {
                        let path = req.uri().path();
                        let sdk_key_file = path
                            .strip_prefix("/v1/download_config_specs/")
                            .unwrap_or("no-key-provided".into());
                        let sdk_key = sdk_key_file.strip_suffix(".json").unwrap_or_else(|| {
                            sdk_key_file.strip_suffix(".js").unwrap_or(sdk_key_file)
                        });
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

                    if resp.status() == Status::Ok {
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new(
                                ProxyEventType::HttpServerRequestSuccess,
                                req.headers()
                                    .get_one("statsig-api-key")
                                    .unwrap_or("no-key-provided")
                                    .to_string(),
                            )
                            .with_path(req.uri().path().to_string())
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
                                req.headers()
                                    .get_one("statsig-api-key")
                                    .unwrap_or("no-key-provided")
                                    .to_string(),
                            )
                            .with_path(req.uri().path().to_string())
                            .with_stat(EventStat {
                                operation_type: OperationType::Distribution,
                                value: ms,
                            }),
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
