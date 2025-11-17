use crate::req;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use url::Url;

pub async fn handle<S>(
    stream: &mut S,
    req: &req::Request,
    _backends: &Vec<Url>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    S: AsyncWrite + AsyncRead + Unpin + Send,
{
    let body = format!("Not implemented: {}", req.path);
    let header = format!(
        "HTTP/1.1 501 Not Implemented\r\nContent-Length: {}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}
