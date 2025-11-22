use axum::{Router, response::Html, routing::get, serve};
use reqwest::Client;
use std::sync::{Arc, atomic::AtomicUsize};
use std::time::Duration;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;
use tracing::info;

mod config;
mod proxy;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

    let state = proxy::AppState {
        client,
        backends: cfg.backends.clone(),
        counter: Arc::new(AtomicUsize::new(0)),
    };

    let nf = not_found_html.clone();
    let static_service =
        ServeDir::new(&cfg.static_dir).fallback(get(move || async move { Html((*nf).clone()) }));

    let app = Router::new()
        .nest_service("/static", static_service)
        .fallback(proxy::proxy_handler)
        .with_state(state);

    let listener = TcpListener::bind(cfg.listen).await?;
    info!("listening on: {}", listener.local_addr()?);

    let shutdown_signal = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!("failed to install CTRL+C handler: {}", e);
        }
        info!("shutdown signal received");
    };

    serve(listener, app)
        .with_graceful_shutdown(shutdown_signal)
        .await?;

    Ok(())
}
