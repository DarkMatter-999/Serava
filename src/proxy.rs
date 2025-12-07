use axum::{
    body::Body,
    extract::State,
    http::{
        Method, Request, Response, StatusCode,
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

use bytes::Bytes;
use dashmap::DashMap;
use std::net::IpAddr;
use std::time::Instant;

/// Cached response entry (stored in the in-memory cache)
#[derive(Clone)]
pub struct CacheEntry {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Bytes,
    pub expires_at: Instant,
    pub size: usize,
}

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

    // Response cache using DashMap for simple concurrent in-memory caching
    pub response_cache: Option<Arc<DashMap<String, CacheEntry>>>,
    pub cache_ttl_secs: Option<u64>,
    pub cache_max_size_bytes: Option<usize>,
    // Current approximate cache size (sum of stored body sizes). Used for eviction.
    pub cache_current_size: Arc<AtomicUsize>,
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

    // Build a simple cache key using method + absolute URI (includes query)
    let cache_key = format!("{} {}", req.method(), req.uri().to_string());

    // If a response cache is configured (DashMap), check it first.
    if let Some(cache) = &state.response_cache {
        if let Some(entry_ref) = cache.get(&cache_key) {
            // If cached and still fresh, serve it immediately.
            if Instant::now() < entry_ref.expires_at {
                let mut response_builder = Response::builder().status(entry_ref.status);
                for (name, val) in &entry_ref.headers {
                    if let Ok(hn) = HeaderName::from_bytes(name.as_bytes()) {
                        if let Ok(hv) = HeaderValue::from_bytes(val) {
                            response_builder = response_builder.header(hn, hv);
                        }
                    }
                }
                let resp = response_builder
                    .body(Body::from(entry_ref.body.clone()))
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                return Ok(resp);
            } else {
                // expired -> remove it
                cache.remove(&cache_key);
            }
        }
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

    let is_get = req.method() == &Method::GET;

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

    let mut resp_headers: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, value) in resp.headers() {
        if !is_hop_by_hop(name.as_str()) {
            response_builder = response_builder.header(name, value);
            resp_headers.push((name.to_string(), value.as_bytes().to_vec()));
        }
    }

    // Helper: robustly parse Cache-Control header bytes and return (s_maxage, max_age, no_store_or_no_cache)
    fn parse_cache_control_bytes(hv: &[u8]) -> (Option<u64>, Option<u64>, bool) {
        if let Ok(s) = std::str::from_utf8(hv) {
            let mut s_maxage: Option<u64> = None;
            let mut maxage: Option<u64> = None;
            let mut no_store_or_no_cache = false;
            for part in s.split(',') {
                let p = part.trim();
                // accept quoted values and spaces: split on '=' only once
                if p.eq_ignore_ascii_case("no-store") || p.eq_ignore_ascii_case("no-cache") {
                    no_store_or_no_cache = true;
                    continue;
                }
                if let Some(rest) = p.splitn(2, '=').nth(1) {
                    let k = p.splitn(2, '=').next().unwrap_or("").trim();
                    let v = rest.trim().trim_matches('"');
                    if k.eq_ignore_ascii_case("s-maxage") {
                        if let Ok(n) = v.parse::<u64>() {
                            s_maxage = Some(n);
                        }
                    } else if k.eq_ignore_ascii_case("max-age") {
                        if let Ok(n) = v.parse::<u64>() {
                            maxage = Some(n);
                        }
                    }
                }
            }
            return (s_maxage, maxage, no_store_or_no_cache);
        }
        (None, None, false)
    }

    // Resolve TTL and cacheability from headers and config.
    // Prefer s-maxage, then max-age, then configured default TTL.
    let mut ttl_seconds: Option<u64> = None;
    let mut backend_forbids_cache = false;
    if let Some((_, hv)) = resp_headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("cache-control"))
    {
        let (s_max, max_a, no_store) = parse_cache_control_bytes(hv);
        backend_forbids_cache = no_store;
        ttl_seconds = s_max.or(max_a);
    }
    if ttl_seconds.is_none() {
        ttl_seconds = state.cache_ttl_secs;
    }

    // Only consider caching for GET requests, successful 200 responses, cache enabled, and not forbidden.
    let should_cache = is_get
        && resp.status().as_u16() == 200
        && !backend_forbids_cache
        && ttl_seconds.is_some()
        && state.response_cache.is_some();

    if should_cache {
        // We need to buffer the body for caching
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("error reading upstream body for caching: {}", e);
                return Err(StatusCode::BAD_GATEWAY);
            }
        };

        // Build response to return to client
        let response = response_builder
            .body(Body::from(bytes.clone()))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        // Insert into cache
        if let (Some(cache), Some(ttl)) = (state.response_cache.as_ref(), ttl_seconds) {
            let size = bytes.len();
            let expires_at = Instant::now() + Duration::from_secs(ttl);
            let entry = CacheEntry {
                status: response.status().as_u16(),
                headers: resp_headers.clone(),
                body: Bytes::from(bytes.clone()),
                expires_at,
                size,
            };
            cache.insert(cache_key.clone(), entry);
            state.cache_current_size.fetch_add(size, Ordering::Relaxed);

            // Evict if cache exceeds configured max size (best-effort).
            if let Some(max_bytes) = state.cache_max_size_bytes {
                // Collect items and evict oldest expirations first.
                let mut items: Vec<(String, Instant, usize)> = cache
                    .iter()
                    .map(|r| (r.key().clone(), r.value().expires_at, r.value().size))
                    .collect();
                items.sort_by_key(|t| t.1);
                let mut cur_total = state.cache_current_size.load(Ordering::Relaxed);
                for (k, _exp, _sz) in items {
                    if cur_total as u64 <= max_bytes as u64 {
                        break;
                    }
                    if let Some(removed) = cache.remove(&k) {
                        cur_total = cur_total.saturating_sub(removed.1.size);
                        state
                            .cache_current_size
                            .fetch_sub(removed.1.size, Ordering::Relaxed);
                    }
                }
            }
        }
        return Ok(response);
    } else {
        let upstream_stream = resp
            .bytes_stream()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e));
        let streamed = response_builder
            .body(Body::from_stream(upstream_stream))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Ok(streamed);
    }
}
