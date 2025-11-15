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

/// Validated runtime config
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub static_dir: PathBuf,
    pub backends: Vec<Url>,
}

#[derive(Debug)]
pub enum ValidationError {
    InvalidListenAddress(String),
    StaticDirDoesNotExist(String),
    StaticDirNotADirectory(String),
    NoBackendsConfigured,
    InvalidBackendUrl(String, String),
    UnsupportedBackendScheme(String),
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
        })
    }
}
