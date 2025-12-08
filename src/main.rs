use axum::{Router, response::Html, routing::get};
use axum_server::tls_rustls::RustlsConfig;
use dashmap::DashMap;
use reqwest::Client;
use std::sync::{Arc, atomic::AtomicUsize};
use std::time::Duration;
use tower_http::{limit::RequestBodyLimitLayer, services::ServeDir};
use tracing::info;

mod config;
mod proxy;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt().init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());

    let toml_str = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("failed to read config file '{}': {}", config_path, e))?;
    let raw: config::RawConfig = toml::from_str(&toml_str)
        .map_err(|e| format!("failed to parse TOML '{}': {}", config_path, e))?;
    let server_cfgs = raw
        .validate()
        .map_err(|e| format!("config validation error: {}", e))?;

    info!("loaded config: {} server(s)", server_cfgs.len());
    for (i, s) in server_cfgs.iter().enumerate() {
        info!("server[{}] listen = {}", i, s.listen);
        info!("server[{}] static_dir = {}", i, s.static_dir.display());
        for (j, b) in s.backends.iter().enumerate() {
            info!("server[{}] backend[{}] = {}", i, j, b);
        }
    }

    // shared HTTP client across servers
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(32)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let global_handle = axum_server::Handle::new();

    let shutdown_handle = global_handle.clone();
    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!("failed to install CTRL+C handler: {}", e);
        }
        info!("shutdown signal received");
        // Wait 10 seconds for requests to finish
        shutdown_handle.graceful_shutdown(Some(Duration::from_secs(10)));
    });

    // Spawn one axum server per config entry.
    let mut server_tasks = Vec::with_capacity(server_cfgs.len());

    for cfg in server_cfgs.into_iter() {
        info!("preparing server on {}", cfg.listen);

        // load per-server 404.html (fall back to embedded)
        let default_404 = include_str!("../static/404.html").to_string();
        let not_found_html = Arc::new(
            std::fs::read_to_string(cfg.static_dir.join("404.html")).unwrap_or_else(|e| {
                info!(
                    "failed to load {}/404.html: {}, falling back to embedded 404.html",
                    cfg.static_dir.display(),
                    e
                );
                default_404.clone()
            }),
        );

        // per-server response cache (optional)
        let response_cache = if let Some(ttl) = cfg.cache_ttl_secs {
            if ttl == 0 {
                tracing::info!("response caching disabled (ttl=0) for {}", cfg.listen);
                None
            } else {
                tracing::info!(
                    "response caching enabled for {}: ttl={}s, max_size_bytes={:?}",
                    cfg.listen,
                    ttl,
                    cfg.cache_max_size_bytes
                );
                Some(Arc::new(DashMap::new()))
            }
        } else {
            tracing::info!("response caching disabled for {}", cfg.listen);
            None
        };

        // Build per-server AppState (client is cloned)
        let state = proxy::AppState {
            client: client.clone(),
            backends: cfg.backends.clone(),
            counter: Arc::new(AtomicUsize::new(0)),
            backend_timeout: cfg.backend_timeout,
            rate_limit_map: Arc::new(DashMap::new()),
            rate_limit_per_minute: cfg.rate_limit_per_minute.map(|v| v as f64),
            rate_limit_burst: cfg
                .rate_limit_burst
                .map(|v| v as f64)
                .or(cfg.rate_limit_per_minute.map(|p| p as f64)),
            response_cache,
            cache_ttl_secs: cfg.cache_ttl_secs,
            cache_max_size_bytes: cfg.cache_max_size_bytes.map(|v| v as usize),
            cache_current_size: Arc::new(AtomicUsize::new(0)),
        };

        // static service per server
        let nf = not_found_html.clone();
        let static_service = ServeDir::new(&cfg.static_dir)
            .fallback(get(move || async move { Html((*nf).clone()) }));

        let app = Router::new()
            .nest_service("/static", static_service)
            .fallback(proxy::proxy_handler)
            .layer(RequestBodyLimitLayer::new(
                cfg.max_request_size_bytes as usize,
            ))
            .with_state(state);

        let handle_clone = global_handle.clone();
        let listen_addr = cfg.listen;

        // If TLS configured for this server, load it
        if let Some(tls_files) = cfg.tls {
            info!("TLS enabled for {}", listen_addr);
            info!("loading cert: {}", tls_files.cert.display());
            info!("loading key: {}", tls_files.key.display());

            let tls_config = RustlsConfig::from_pem_file(tls_files.cert, tls_files.key).await?;

            // spawn the server task
            server_tasks.push(tokio::spawn(async move {
                info!("listening securely on https://{}", listen_addr);
                if let Err(e) = axum_server::bind_rustls(listen_addr, tls_config)
                    .handle(handle_clone)
                    .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                    .await
                {
                    tracing::error!("server {} failed: {}", listen_addr, e);
                }
            }));
        } else {
            tracing::info!("TLS disabled for {} (no cert/key)", listen_addr);
            server_tasks.push(tokio::spawn(async move {
                info!("listening on http://{}", listen_addr);
                if let Err(e) = axum_server::bind(listen_addr)
                    .handle(handle_clone)
                    .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                    .await
                {
                    tracing::error!("server {} failed: {}", listen_addr, e);
                }
            }));
        }
    }

    // Wait for all spawned server tasks to complete
    for t in server_tasks {
        let _ = t.await;
    }

    Ok(())
}
