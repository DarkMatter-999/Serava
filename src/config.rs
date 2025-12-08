use serde::Deserialize;
use std::{net::SocketAddr, path::PathBuf, time::Duration};
use url::Url;

#[derive(Debug, Deserialize)]
pub struct RawConfig {
    pub servers: Vec<RawServer>,
}

#[derive(Debug, Deserialize)]
pub struct RawServer {
    pub listen: String,
    pub static_dir: PathBuf,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub proxy: RawProxy,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum BackendField {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Debug, Deserialize)]
pub struct RawProxy {
    pub backend: BackendField,
    pub backend_timeout_secs: Option<u64>,
    pub rate_limit_per_minute: Option<u64>,
    pub rate_limit_burst: Option<u64>,
    pub max_request_size_bytes: Option<u64>,
    pub cache_ttl_secs: Option<u64>,
    pub cache_max_size_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Validated per-server config returned from `RawConfig::validate`.
#[derive(Debug, Clone)]
pub struct ConfigEntry {
    pub listen: SocketAddr,
    pub static_dir: PathBuf,
    pub backends: Vec<Url>,
    pub tls: Option<TlsConfig>,
    pub backend_timeout: Duration,
    pub rate_limit_per_minute: Option<u64>,
    pub rate_limit_burst: Option<u64>,
    pub max_request_size_bytes: u64,
    pub cache_ttl_secs: Option<u64>,
    pub cache_max_size_bytes: Option<u64>,
}

#[derive(Debug)]
pub enum ValidationError {
    InvalidListenAddress(String),
    StaticDirDoesNotExist(String),
    StaticDirNotADirectory(String),
    NoServersConfigured,
    NoBackendsConfigured(String),
    InvalidBackendUrl(String, String),
    UnsupportedBackendScheme(String),
    TlsFileNotFound(String),
    IncompleteTlsConfig(String),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use ValidationError::*;
        match self {
            InvalidListenAddress(e) => write!(f, "invalid listen address: {}", e),
            StaticDirDoesNotExist(path) => write!(f, "static_dir does not exist: {}", path),
            StaticDirNotADirectory(path) => write!(f, "static_dir is not a directory: {}", path),
            NoServersConfigured => write!(f, "no servers configured"),
            NoBackendsConfigured(srv) => {
                write!(f, "no backends configured in [proxy] for server '{}'", srv)
            }
            InvalidBackendUrl(url, e) => write!(f, "invalid backend URL '{}': {}", url, e),
            UnsupportedBackendScheme(scheme) => write!(
                f,
                "unsupported backend scheme '{}', only http/https allowed",
                scheme
            ),
            TlsFileNotFound(path) => write!(f, "TLS file not found: {}", path),
            IncompleteTlsConfig(srv) => write!(
                f,
                "Both 'cert' and 'key' must be provided for TLS in server '{}'",
                srv
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

impl RawConfig {
    pub fn validate(self) -> Result<Vec<ConfigEntry>, ValidationError> {
        if self.servers.is_empty() {
            return Err(ValidationError::NoServersConfigured);
        }

        let mut out: Vec<ConfigEntry> = Vec::with_capacity(self.servers.len());

        for (idx, raw_srv) in self.servers.into_iter().enumerate() {
            // Human-readable server id for error messages
            let server_id = format!("server[{}] {}", idx, raw_srv.listen);

            let listen = match raw_srv.listen.parse::<SocketAddr>() {
                Ok(addr) => addr,
                Err(e) => return Err(ValidationError::InvalidListenAddress(e.to_string())),
            };

            let static_dir = raw_srv.static_dir;
            if !static_dir.exists() {
                return Err(ValidationError::StaticDirDoesNotExist(
                    static_dir.display().to_string(),
                ));
            }
            if !static_dir.is_dir() {
                return Err(ValidationError::StaticDirNotADirectory(
                    static_dir.display().to_string(),
                ));
            }

            // TLS: both cert and key must be present if any is provided
            let tls = match (raw_srv.cert, raw_srv.key) {
                (Some(cert), Some(key)) => {
                    if !cert.exists() {
                        return Err(ValidationError::TlsFileNotFound(cert.display().to_string()));
                    }
                    if !key.exists() {
                        return Err(ValidationError::TlsFileNotFound(key.display().to_string()));
                    }
                    Some(TlsConfig { cert, key })
                }
                (None, None) => None,
                _ => return Err(ValidationError::IncompleteTlsConfig(server_id.clone())),
            };

            // backends: allow single or multiple
            let backend_strings: Vec<String> = match raw_srv.proxy.backend {
                BackendField::Single(s) => vec![s],
                BackendField::Multiple(v) => v,
            };

            if backend_strings.is_empty() {
                return Err(ValidationError::NoBackendsConfigured(server_id.clone()));
            }

            // parse and validate backend URLs
            let mut backends: Vec<Url> = Vec::with_capacity(backend_strings.len());
            for b in backend_strings {
                let url = Url::parse(&b)
                    .map_err(|e| ValidationError::InvalidBackendUrl(b.clone(), e.to_string()))?;
                match url.scheme() {
                    "http" | "https" => backends.push(url),
                    other => {
                        return Err(ValidationError::UnsupportedBackendScheme(other.to_string()));
                    }
                }
            }

            let backend_timeout =
                Duration::from_secs(raw_srv.proxy.backend_timeout_secs.unwrap_or(30));
            let rate_limit_per_minute = raw_srv.proxy.rate_limit_per_minute;
            let rate_limit_burst = raw_srv.proxy.rate_limit_burst;
            let max_request_size_bytes = raw_srv
                .proxy
                .max_request_size_bytes
                .unwrap_or(10 * 1024 * 1024);
            let cache_ttl_secs = raw_srv.proxy.cache_ttl_secs;
            let cache_max_size_bytes = raw_srv.proxy.cache_max_size_bytes;

            out.push(ConfigEntry {
                listen,
                static_dir,
                backends,
                tls,
                backend_timeout,
                rate_limit_per_minute,
                rate_limit_burst,
                max_request_size_bytes,
                cache_ttl_secs,
                cache_max_size_bytes,
            });
        }

        Ok(out)
    }
}
