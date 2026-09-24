//! End-to-end check that Rocket's internal `No matching routes` error log,
//! which prints the full request URI, goes through the SDK key redaction.

use std::sync::{Mutex, OnceLock};

use rocket::http::Status;
use rocket::local::blocking::Client;
use rocket::{get, routes};
use statsig_forward_proxy::utils::sdk_key_redacting_logger::{LogSink, SdkKeyRedactingLogger};

const FULL_KEY: &str = "client-abcdefghijklmnopqrstuvwxyz";
const REDACTED_KEY: &str = "client-abcdefghijklm***";

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct CapturingSink;

impl LogSink for CapturingSink {
    fn write_line(&self, line: &str) {
        LINES.lock().unwrap().push(line.to_owned());
    }
}

#[get("/matched")]
fn matched() -> &'static str {
    "ok"
}

fn client() -> Client {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        log::set_boxed_logger(Box::new(SdkKeyRedactingLogger::new(CapturingSink)))
            .expect("no other logger should be installed in this test binary");
        // Same filter Rocket.toml's `log_level = "critical"` produces.
        log::set_max_level(log::LevelFilter::Warn);
    });
    Client::tracked(rocket::build().mount("/", routes![matched])).expect("client should build")
}

fn no_matching_routes_lines_for(path_fragment: &str) -> Vec<String> {
    LINES
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains("No matching routes") && line.contains(path_fragment))
        .cloned()
        .collect()
}

fn assert_no_full_key_logged() {
    let lines = LINES.lock().unwrap();
    assert!(
        lines.iter().all(|line| !line.contains(FULL_KEY)),
        "full SDK key leaked into logs: {lines:#?}"
    );
}

#[test]
fn unmatched_route_log_redacts_keys_in_path_and_query() {
    let client = client();
    let response = client
        .get(format!(
            "/rgstr/{FULL_KEY}?token={FULL_KEY}&st=javascript-client"
        ))
        .dispatch();
    assert_eq!(response.status(), Status::NotFound);

    let lines = no_matching_routes_lines_for("/rgstr/");
    assert_eq!(
        lines,
        vec![format!(
            "   >> No matching routes for GET /rgstr/{REDACTED_KEY}?token={REDACTED_KEY}&st=javascript-client."
        )]
    );
    assert_no_full_key_logged();
}

#[test]
fn head_request_to_get_route_does_not_leak_the_key() {
    // Rocket routes HEAD once (logging "No matching routes") before retrying it as GET.
    let client = client();
    let response = client.head(format!("/matched?k={FULL_KEY}")).dispatch();
    assert_eq!(response.status(), Status::Ok);

    let lines = no_matching_routes_lines_for("/matched?k=");
    assert_eq!(
        lines,
        vec![format!(
            "   >> No matching routes for HEAD /matched?k={REDACTED_KEY}."
        )]
    );
    assert_no_full_key_logged();
}

#[test]
fn unmatched_route_log_redacts_keys_after_percent_escapes() {
    let client = client();
    let response = client
        .get(format!("/v1/download_config_specs%2F{FULL_KEY}.json"))
        .dispatch();
    assert_eq!(response.status(), Status::NotFound);

    let lines = no_matching_routes_lines_for("/v1/download_config_specs%2F");
    assert_eq!(
        lines,
        vec![format!(
            "   >> No matching routes for GET /v1/download_config_specs%2F{REDACTED_KEY}.json."
        )]
    );
    assert_no_full_key_logged();
}
