use axum::{response::Html, routing::get, Router, serve};
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;
use tracing::info;

mod config;
mod proxy;

#[tokio::main]
async fn main() -> Result<(), Box<(dyn std::error::Error + 'static)>> {
    tracing_subscriber::fmt::init();

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

    let default_404 = include_str!("../static/404.html");
    let not_found_html = Arc::new(match std::fs::read_to_string(cfg.static_dir.join("404.html")) {
        Ok(s) => s,
        Err(e) => {
            info!(
                "failed to load {}/404.html: {}, falling back to embedded 404.html",
                cfg.static_dir.display(),
                e
            );
            default_404.to_string()
        }
    });

    let state = proxy::AppState {
        backends: cfg.backends.clone(),
    };

    let app = Router::new()
        .nest_service("/static", ServeDir::new(&cfg.static_dir).fallback(get(move || async move { Html((*not_found_html).clone()) })))
        .fallback(proxy::proxy_handler)
        .with_state(state);

    let listener = TcpListener::bind(cfg.listen).await?;
    info!("listening on: {}", listener.local_addr()?);

    serve(listener, app).await?;

    Ok(())
}
