use std::{collections::HashMap, convert::TryFrom};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub version: Version,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum Method {
    Get,
}

impl TryFrom<&str> for Method {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "GET" => Ok(Method::Get),
            m => Err(format!("unsupported method: {}", m)),
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum Version {
    Http1_0,
    Http1_1,
}

impl TryFrom<&str> for Version {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "HTTP/1.0" => Ok(Version::Http1_0),
            "HTTP/1.1" => Ok(Version::Http1_1),
            v => Err(format!("unsupported HTTP version: {}", v)),
        }
    }
}

#[derive(Debug)]
pub enum ParseError {
    Io(std::io::Error),
    MissingMethod,
    MissingPath,
    MissingVersion,
    InvalidMethod(String),
    InvalidVersion(String),
    MissingHeaderName,
    MissingHeaderValue,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Io(e) => write!(f, "I/O error: {}", e),
            ParseError::MissingMethod => write!(f, "missing method"),
            ParseError::MissingPath => write!(f, "missing path"),
            ParseError::MissingVersion => write!(f, "missing HTTP version"),
            ParseError::InvalidMethod(m) => write!(f, "invalid method: {}", m),
            ParseError::InvalidVersion(v) => write!(f, "invalid HTTP version: {}", v),
            ParseError::MissingHeaderName => write!(f, "missing header name"),
            ParseError::MissingHeaderValue => write!(f, "missing header value"),
        }
    }
}

impl std::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ParseError::Io(e) => Some(e),
            _ => None,
        }
    }
}

// Allow automatic conversion from std::io::Error -> ParseError
impl From<std::io::Error> for ParseError {
    fn from(err: std::io::Error) -> Self {
        ParseError::Io(err)
    }
}

pub async fn parse_request(mut stream: impl AsyncBufRead + Unpin) -> Result<Request, ParseError> {
    let mut line_buffer = String::new();
    stream.read_line(&mut line_buffer).await?;

    let mut parts = line_buffer.split_whitespace();

    let method: Method = parts
        .next()
        .ok_or(ParseError::MissingMethod)
        .and_then(|m| m.try_into().map_err(ParseError::InvalidMethod))?;

    let path: String = parts
        .next()
        .ok_or(ParseError::MissingPath)
        .map(Into::into)?;

    let version: Version = parts
        .next()
        .ok_or(ParseError::MissingVersion)
        .and_then(|v| v.try_into().map_err(ParseError::InvalidVersion))?;

    let mut headers = HashMap::new();

    loop {
        line_buffer.clear();
        stream.read_line(&mut line_buffer).await?;

        if line_buffer.is_empty() || line_buffer == "\n" || line_buffer == "\r\n" {
            break;
        }

        let mut comps = line_buffer.splitn(2, ':');
        let key = comps.next().ok_or(ParseError::MissingHeaderName)?;
        let value = comps.next().ok_or(ParseError::MissingHeaderValue)?.trim();

        headers.insert(key.to_string(), value.to_string());
    }

    Ok(Request {
        method,
        path,
        headers,
        version,
    })
}
