use axum::{
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
};
use reqwest::Client;
use url::Url;

#[derive(Clone)]
pub struct AppState {
    pub backends: Vec<Url>,
}

pub async fn proxy_handler(
    State(state): State<AppState>,
    request: Request<Body>,
) -> Result<Response<Body>, StatusCode> {
    // for now, only use the first backend
    let backend = &state.backends[0];

    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("");

    let url = backend
        .join(path_and_query)
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let client = Client::new();
    let mut req_builder = client.request(request.method().clone(), url);

    // Copy headers, skipping host
    for (key, value) in request.headers() {
        if key != "host" {
            req_builder = req_builder.header(key, value);
        }
    }

    // Collect body
    let body_bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };
    req_builder = req_builder.body(body_bytes);

    let resp = match req_builder.send().await {
        Ok(r) => r,
        Err(_) => return Err(StatusCode::BAD_GATEWAY),
    };

    let status = resp.status();
    let mut response_builder = Response::builder().status(status);

    // Copy response headers
    for (key, value) in resp.headers() {
        response_builder = response_builder.header(key, value);
    }

    let body_bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => return Err(StatusCode::BAD_GATEWAY),
    };

    Ok(response_builder.body(Body::from(body_bytes)).unwrap())
}
