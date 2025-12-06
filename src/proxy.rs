use axum::{
    body::Body,
    extract::State,
    http::{
        Request, Response, StatusCode,
        header::{HeaderName, HeaderValue},
    },
};
use futures::TryStreamExt;
use reqwest::{Body as ReqwestBody, Client};
use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::time::timeout;
use url::Url;

use dashmap::DashMap;
use std::net::IpAddr;
use std::time::Instant;

/// Application shared state.
#[derive(Clone)]
pub struct AppState {
    pub client: Client,
    pub backends: Vec<Url>,
    pub counter: Arc<AtomicUsize>,
    pub backend_timeout: Duration,

    // Per-IP in-memory token buckets (tokens, last_seen)
    // This is used as an in-process rate limiter.
    pub rate_limit_map: Arc<DashMap<IpAddr, (f64, Instant)>>,
    pub rate_limit_per_minute: Option<f64>,
    pub rate_limit_burst: Option<f64>,
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
    HOP_BY_HOP_HEADERS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(name))
}

fn sanitize_and_forward_headers(
    req_builder: reqwest::RequestBuilder,
    headers: &axum::http::HeaderMap,
) -> reqwest::RequestBuilder {
    let mut rb = req_builder;

    for (name, value) in headers.iter() {
        let name_str = name.as_str();

        // Always drop hop-by-hop headers
        if is_hop_by_hop(name_str) {
            tracing::debug!("dropping hop-by-hop header: {}", name_str);
            continue;
        }

        // Validate header name length
        if name_str.is_empty() || name_str.len() > 256 {
            tracing::warn!("dropping header with invalid name length: {}", name_str);
            continue;
        }

        let raw = value.as_bytes();

        // Validate header value length
        if raw.len() > 16 * 1024 {
            tracing::warn!(
                "dropping header {}: value too long ({} bytes)",
                name_str,
                raw.len()
            );
            continue;
        }

        // Ensure value is UTF-8 (best-effort); if not, drop it
        let vstr = match std::str::from_utf8(raw) {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!("dropping header {}: non-UTF8 value", name_str);
                continue;
            }
        };

        // Drop headers with disallowed control characters (allow HT, SP, and visible chars)
        if vstr.chars().any(|c| c.is_control() && c != '\t') {
            tracing::warn!("dropping header {}: contains control characters", name_str);
            continue;
        }

        // Normalize value by trimming whitespace
        let sanitized_value = vstr.trim();

        // Drop sensitive headers explicitly
        if let Ok(hn) = HeaderName::from_bytes(name_str.as_bytes()) {
            if hn == HeaderName::from_static("authorization")
                || hn == HeaderName::from_static("proxy-authorization")
            {
                tracing::debug!("dropping sensitive header: {}", name_str);
                continue;
            }

            // Finally attempt to create a HeaderValue from sanitized string
            match HeaderValue::from_str(sanitized_value) {
                Ok(hv) => {
                    rb = rb.header(hn, hv);
                }
                Err(_) => {
                    tracing::warn!(
                        "dropping header {}: invalid value after sanitization",
                        name_str
                    );
                    continue;
                }
            }
        } else {
            tracing::warn!("dropping header with invalid name: {}", name_str);
            continue;
        }
    }

    rb
}

fn check_rate_limit(state: &AppState, req: &Request<Body>) -> Result<(), StatusCode> {
    // Check if rate limiting is disabled.
    if state.rate_limit_per_minute.is_none() {
        return Ok(());
    }

    let mut client_ip_opt: Option<IpAddr> = None;

    // 1) X-Forwarded-For header (take the first IP)
    if let Some(xff) = req.headers().get("x-forwarded-for") {
        if let Ok(s) = std::str::from_utf8(xff.as_bytes()) {
            if let Some(first) = s.split(',').next() {
                if let Ok(ip) = first.trim().parse::<IpAddr>() {
                    client_ip_opt = Some(ip);
                }
            }
        }
    }

    // 2) axum ConnectInfo (if present)
    if client_ip_opt.is_none() {
        if let Some(ci) = req
            .extensions()
            .get::<axum::extract::connect_info::ConnectInfo<std::net::SocketAddr>>()
        {
            client_ip_opt = Some(ci.0.ip());
        }
    }

    // 3) fallback to raw SocketAddr in extensions
    if client_ip_opt.is_none() {
        if let Some(sock) = req.extensions().get::<std::net::SocketAddr>() {
            client_ip_opt = Some(sock.ip());
        }
    }

    let ip = match client_ip_opt {
        Some(ip) => ip,
        None => return Ok(()), // Can't attribute an IP; allow the request
    };

    let now = Instant::now();

    let per_min = state.rate_limit_per_minute.unwrap();
    let rate_per_sec = per_min / 60.0;
    let burst = state.rate_limit_burst.unwrap_or(per_min);

    // Update or insert token bucket for this IP
    // Initialize new entries with 0 tokens to avoid allowing a large initial burst.
    let mut allowed = false;
    {
        // When inserting a fresh bucket, start with 0.0 tokens and last-seen = now.
        // Existing entries will be topped up based on elapsed time below.
        let mut entry = state.rate_limit_map.entry(ip).or_insert((0.0, now));
        let elapsed = now.duration_since(entry.1).as_secs_f64();
        entry.0 = (entry.0 + elapsed * rate_per_sec).min(burst);
        entry.1 = now;
        if entry.0 >= 1.0 {
            entry.0 -= 1.0;
            allowed = true;
        }
    }

    if allowed {
        Ok(())
    } else {
        tracing::debug!("rate limit exceeded for {}", ip);
        Err(StatusCode::TOO_MANY_REQUESTS)
    }
}

pub async fn proxy_handler(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Result<Response<Body>, StatusCode> {
    // Relaxed ordering is fine and fastest here.
    if state.backends.is_empty() {
        return Err(StatusCode::BAD_GATEWAY);
    }

    if let Err(status) = check_rate_limit(&state, &req) {
        tracing::warn!("rate limited request from client");
        return Err(status);
    }

    let idx = state.counter.fetch_add(1, Ordering::Relaxed) % state.backends.len();
    let backend = &state.backends[idx];

    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let url = backend
        .join(path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let method = req.method().clone();
    let mut req_builder = state.client.request(method, url);

    // Sanitize and forward headers from the incoming request
    req_builder = sanitize_and_forward_headers(req_builder, req.headers());

    // Convert Axum Body to Reqwest Body.
    let client_body = req.into_body();
    let stream = client_body
        .into_data_stream()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e));
    req_builder = req_builder.body(ReqwestBody::wrap_stream(stream));

    // Send request to backend with a configured timeout. Map errors appropriately.
    let send_future = req_builder.send();
    let resp = match timeout(state.backend_timeout, send_future).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::error!("Upstream error: {}", e);
            return Err(StatusCode::BAD_GATEWAY);
        }
        Err(_) => {
            tracing::warn!(
                "upstream request timed out after {:?}",
                state.backend_timeout
            );
            return Err(StatusCode::GATEWAY_TIMEOUT);
        }
    };

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
