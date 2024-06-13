use rocket::{get, http::Status, serde::json::Json};
use serde::{Deserialize, Serialize};

// Endpoints for reporting the health of the service
// These endpoints represent the kubernetes healthcheck API
// which is described here: https://kubernetes.io/docs/tasks/configure-pod-container/configure-liveness-readiness-startup-probes/

#[derive(Serialize, Deserialize)]
pub struct StartupResponse {}

#[get("/startup")]
pub async fn startup() -> Result<Json<StartupResponse>, Status> {
    Ok(Json(StartupResponse {}))
}

#[derive(Serialize, Deserialize)]
pub struct ReadyResponse {}

#[get("/ready")]
pub async fn ready() -> Result<Json<ReadyResponse>, Status> {
    Ok(Json(ReadyResponse {}))
}

#[derive(Serialize, Deserialize)]
pub struct HealthResponse {}

#[get("/health")]
pub async fn health() -> Result<Json<HealthResponse>, Status> {
    Ok(Json(HealthResponse {}))
}
