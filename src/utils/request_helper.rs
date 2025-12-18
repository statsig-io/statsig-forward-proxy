use rocket::Request;

pub fn does_request_supports_proto(request: &Request<'_>) -> bool {
    request
        .query_value::<bool>("supports_proto") // Try to parse as bool
        .and_then(|res| res.ok()) // Ignore parse errors
        .unwrap_or_else(|| request.headers().get_one("statsig-supports-proto") == Some("true"))
}
