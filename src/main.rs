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
    let cfg = raw
        .validate()
        .map_err(|e| format!("config validation error: {}", e))?;

    info!("loaded config, listen = {}", cfg.listen);
    info!("static_dir = {}", cfg.static_dir.display());
    for (i, b) in cfg.backends.iter().enumerate() {
        info!("backend[{}] = {}", i, b);
    }

    let default_404 = include_str!("../static/404.html").to_string();
    let not_found_html = Arc::new(
        std::fs::read_to_string(cfg.static_dir.join("404.html")).unwrap_or_else(|e| {
            info!(
                "failed to load {}/404.html: {}, falling back to embedded 404.html",
                cfg.static_dir.display(),
                e
            );
            default_404
        }),
    );

    let client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(32)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let response_cache = if let Some(ttl) = cfg.cache_ttl_secs {
        if ttl == 0 {
            tracing::info!("response caching disabled (ttl=0)");
            None
        } else {
            tracing::info!(
                "response caching enabled: ttl={}s, max_size_bytes={:?}",
                ttl,
                cfg.cache_max_size_bytes
            );
            Some(Arc::new(DashMap::new()))
        }
    } else {
        tracing::info!("response caching disabled");
        None
    };

    let state = proxy::AppState {
        client,
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

    let nf = not_found_html.clone();
    let static_service =
        ServeDir::new(&cfg.static_dir).fallback(get(move || async move { Html((*nf).clone()) }));

    let app = Router::new()
        .nest_service("/static", static_service)
        .fallback(proxy::proxy_handler)
        .layer(RequestBodyLimitLayer::new(
            cfg.max_request_size_bytes as usize,
        ))
        .with_state(state);

    let handle = axum_server::Handle::new();
    let shutdown_handle = handle.clone();

    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!("failed to install CTRL+C handler: {}", e);
        }
        info!("shutdown signal received");
        // Wait 10 seconds for requests to finish
        shutdown_handle.graceful_shutdown(Some(Duration::from_secs(10)));
    });

    match cfg.tls {
        Some(tls_files) => {
            info!("TLS enabled");
            info!("loading cert: {}", tls_files.cert.display());
            info!("loading key: {}", tls_files.key.display());

            let tls_config = RustlsConfig::from_pem_file(tls_files.cert, tls_files.key).await?;

            info!("listening securely on https://{}", cfg.listen);

            axum_server::bind_rustls(cfg.listen, tls_config)
                .handle(handle)
                .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .await?;
        }
        None => {
            info!("TLS disabled (cert/key not present in config)");
            info!("listening on http://{}", cfg.listen);

            axum_server::bind(cfg.listen)
                .handle(handle)
                .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .await?;
        }
    }

    Ok(())
}
