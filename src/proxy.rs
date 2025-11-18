use axum::{
    body::Body,
    extract::State,
    http::{HeaderName, Request, Response, StatusCode},
};
use reqwest::Client;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use url::Url;

#[derive(Clone)]
pub struct AppState {
    pub client: Client,
    pub backends: Vec<Url>,
    pub counter: Arc<AtomicUsize>,
}

/// Hop-by-hop headers that must not be forwarded (RFC 7230 §6.1)
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
    )
}

pub async fn proxy_handler(
    State(state): State<AppState>,
    request: Request<Body>,
) -> Result<Response<Body>, StatusCode> {
    // choose backend via round-robin
    if state.backends.is_empty() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let idx = state
        .counter
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_rem(state.backends.len());
    let backend = &state.backends[idx];

    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let url = backend
        .join(path_and_query)
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let method = request.method().clone();
    let mut req_builder = state.client.request(method, url);

    // Copy headers, skip hop-by-hop.
    for (name, value) in request.headers().iter() {
        if !is_hop_by_hop(name) {
            req_builder = req_builder.header(name, value.clone());
        }
    }

    // Collect body.
    let body_bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    req_builder = req_builder.body(body_bytes);

    let resp = req_builder
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    // Build response.
    let status = resp.status();
    let mut response_builder = Response::builder().status(status.as_u16());

    // Copy response headers.
    for (name, value) in resp.headers().iter() {
        if let Ok(header_name) = HeaderName::from_bytes(name.as_str().as_bytes()) {
            if !is_hop_by_hop(&header_name) {
                response_builder = response_builder.header(name, value.clone());
            }
        } else {
            // Skip invalid header names.
            continue;
        }
    }

    let body_bytes = resp.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    let response = response_builder
        .body(Body::from(body_bytes.to_vec()))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(response)
}
