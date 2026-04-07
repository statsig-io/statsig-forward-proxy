use rocket::Request;

pub fn does_request_supports_proto(request: &Request<'_>) -> bool {
    request
        .query_value::<bool>("supports_proto") // Try to parse as bool
        .and_then(|res| res.ok()) // Ignore parse errors
        .unwrap_or_else(|| request.headers().get_one("statsig-supports-proto") == Some("true"))
}

pub fn does_request_accept_deltas(request: &Request<'_>) -> bool {
    request
        .uri()
        .query()
        .is_some_and(|q| query_param_is_true(q.as_str(), "accept_deltas"))
}

fn query_param_is_true(query: &str, key: &str) -> bool {
    query.split('&').any(|pair| {
        if let Some((k, v)) = pair.split_once('=') {
            k == key && (v.eq_ignore_ascii_case("true") || v == "1")
        } else {
            pair == key
        }
    })
}
