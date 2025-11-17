use std::{
    collections::HashMap,
    fmt::{Display, Formatter},
    io::{Cursor, Result},
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Clone)]
pub struct Response<S: AsyncRead + Unpin> {
    pub status: Status,
    pub headers: HashMap<String, String>,
    pub data: S,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum Status {
    InternalServerError,
    BadRequest,
    NotFound,
}

impl<S: AsyncRead + Unpin> Response<S> {
    pub fn status_and_headers(&self) -> String {
        let headers = self
            .headers
            .iter()
            .map(|(k, v)| format!("{}: {}", k, v))
            .collect::<Vec<_>>()
            .join("\r\n");

        format!("HTTP/1.1 {}\r\n{headers}\r\n\r\n", self.status)
    }

    pub async fn write<O: AsyncWrite + Unpin>(mut self, stream: &mut O) -> Result<()> {
        stream
            .write_all(self.status_and_headers().as_bytes())
            .await?;

        tokio::io::copy(&mut self.data, stream).await?;

        Ok(())
    }
}

impl Display for Status {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::NotFound => write!(f, "404 Not Found"),
            Status::BadRequest => write!(f, "400 Bad Request"),
            Status::InternalServerError => write!(f, "500 Internal Server Error"),
        }
    }
}

impl Response<Cursor<Vec<u8>>> {
    pub fn from_html(status: Status, data: impl ToString) -> Self {
        let bytes = data.to_string().into_bytes();

        let headers = HashMap::from([
            ("Content-Type".to_string(), "text/html".to_string()),
            ("Content-Length".to_string(), bytes.len().to_string()),
        ]);

        Self {
            status,
            headers,
            data: Cursor::new(bytes),
        }
    }
}
