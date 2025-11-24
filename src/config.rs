use serde::Deserialize;
use std::{net::SocketAddr, path::PathBuf};
use url::Url;

#[derive(Debug, Deserialize)]
pub struct RawConfig {
    pub server: RawServer,
    pub proxy: RawProxy,
}

#[derive(Debug, Deserialize)]
pub struct RawServer {
    pub listen: String,
    pub static_dir: PathBuf,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
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
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Validated runtime config
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub static_dir: PathBuf,
    pub backends: Vec<Url>,
    pub tls: Option<TlsConfig>,
}

#[derive(Debug)]
pub enum ValidationError {
    InvalidListenAddress(String),
    StaticDirDoesNotExist(String),
    StaticDirNotADirectory(String),
    NoBackendsConfigured,
    InvalidBackendUrl(String, String),
    UnsupportedBackendScheme(String),
    TlsFileNotFound(String),
    IncompleteTlsConfig,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::InvalidListenAddress(e) => write!(f, "invalid listen address: {}", e),
            ValidationError::StaticDirDoesNotExist(path) => {
                write!(f, "static_dir does not exist: {}", path)
            }
            ValidationError::StaticDirNotADirectory(path) => {
                write!(f, "static_dir is not a directory: {}", path)
            }
            ValidationError::NoBackendsConfigured => write!(f, "no backends configured in [proxy]"),
            ValidationError::InvalidBackendUrl(url, e) => {
                write!(f, "invalid backend URL '{}': {}", url, e)
            }
            ValidationError::UnsupportedBackendScheme(scheme) => write!(
                f,
                "unsupported backend scheme '{}', only http/https allowed",
                scheme
            ),
            ValidationError::TlsFileNotFound(path) => {
                write!(f, "TLS file not found: {}", path)
            }
            ValidationError::IncompleteTlsConfig => {
                write!(f, "Both 'cert' and 'key' must be provided for TLS")
            }
        }
    }
}

impl std::error::Error for ValidationError {}

impl RawConfig {
    pub fn validate(self) -> Result<Config, ValidationError> {
        let listen = match self.server.listen.parse::<SocketAddr>() {
            Ok(addr) => addr,
            Err(e) => return Err(ValidationError::InvalidListenAddress(e.to_string())),
        };

        let static_dir = self.server.static_dir;
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

        let tls = match (self.server.cert, self.server.key) {
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
            _ => return Err(ValidationError::IncompleteTlsConfig),
        };

        let backend_strings: Vec<String> = match self.proxy.backend {
            BackendField::Single(s) => vec![s],
            BackendField::Multiple(v) => v,
        };

        if backend_strings.is_empty() {
            return Err(ValidationError::NoBackendsConfigured);
        }

        // parse each backend string into Url and ensure http/https
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

        Ok(Config {
            listen,
            static_dir,
            backends,
            tls,
        })
    }
}
