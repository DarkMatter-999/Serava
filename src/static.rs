use crate::req;
use crate::resp;
use std::{
    io,
    path::{Path, PathBuf},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};

pub async fn handle<S>(
    stream: &mut S,
    req: &req::Request,
    static_dir: PathBuf,
    not_found_html: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    S: AsyncWrite + AsyncRead + Unpin + Send,
{
    let rel = req.path.strip_prefix("/static/").unwrap_or("");

    // Basic traversal protection: reject any path with ".." or absolute-root components.
    if rel.contains("..") || rel.starts_with('/') {
        let resp = resp::Response::from_html(resp::Status::BadRequest, "Invalid path");
        resp.write(stream).await?;
        stream.flush().await?;
        return Ok(());
    }

    let mut path = static_dir.join(rel);

    // If path is a directory, try index.html
    match fs::metadata(&path).await {
        Ok(md) if md.is_dir() => {
            path = path.join("index.html");
        }
        _ => {}
    }

    match fs::File::open(&path).await {
        Ok(mut f) => {
            // Read file into memory (small files are fine)
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).await?;

            let ct = guess_mime(&path);

            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {}\r\n\r\n",
                buf.len(),
                ct
            );
            stream.write_all(header.as_bytes()).await?;
            stream.write_all(&buf).await?;
            stream.flush().await?;
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // 404 - use existing not_found_html
            let resp = resp::Response::from_html(resp::Status::NotFound, not_found_html);
            resp.write(stream).await?;
            stream.flush().await?;
            Ok(())
        }
        Err(e) => {
            // Other IO error -> 500
            Err(Box::new(e))
        }
    }
}

fn guess_mime(path: &Path) -> &'static str {
    // Tiny minimal mapping for common types
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}
