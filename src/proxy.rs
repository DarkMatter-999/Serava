use axum::{
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
};
use reqwest::{Client, Body as ReqwestBody};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use url::Url;
use futures::TryStreamExt;
use std::io;

#[derive(Clone)]
pub struct AppState {
    pub client: Client,
    pub backends: Vec<Url>,
    pub counter: Arc<AtomicUsize>,
}

// Use a static array for fast checking without allocating strings
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
];

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(name))
}

pub async fn proxy_handler(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Result<Response<Body>, StatusCode> {
    // Relaxed ordering is fine and fastest here.
    if state.backends.is_empty() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let idx = state.counter.fetch_add(1, Ordering::Relaxed) % state.backends.len();
    let backend = &state.backends[idx];

    let path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = backend.join(path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let method = req.method().clone();
    let mut req_builder = state.client.request(method, url);

    for (name, value) in req.headers() {
        let name_str = name.as_str();
        if !is_hop_by_hop(name_str) {
            req_builder = req_builder.header(name, value);
        }
    }

    // Convert Axum Body to Reqwest Body.
    let client_body = req.into_body();
    let stream = client_body.into_data_stream().map_err(|e| {
        io::Error::new(io::ErrorKind::Other, e)
    });
    req_builder = req_builder.body(ReqwestBody::wrap_stream(stream));

    let resp = req_builder.send().await.map_err(|e| {
        tracing::error!("Upstream error: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    let mut response_builder = Response::builder().status(resp.status());

    for (name, value) in resp.headers() {
        if !is_hop_by_hop(name.as_str()) {
            response_builder = response_builder.header(name, value);
        }
    }

    let upstream_stream = resp
        .bytes_stream()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e));

    Ok(response_builder
        .body(Body::from_stream(upstream_stream))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?)
}
