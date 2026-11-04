//! Tiny loopback-only static file server, used by `rustplay test --serve DIR`
//! and the integration tests so suites never need external websites.
//! GET/HEAD only, no directory listing, paths confined to the root.
//! Extra routes: `/__slow` never responds (for timeout tests).

use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

pub struct StaticServer {
    pub addr: SocketAddr,
    handle: JoinHandle<()>,
}

impl StaticServer {
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for StaticServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub async fn serve(root: impl Into<PathBuf>) -> std::io::Result<StaticServer> {
    let root = std::fs::canonicalize(root.into())?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { continue };
            let root = root.clone();
            tokio::spawn(async move {
                let _ = handle(sock, &root).await;
            });
        }
    });
    Ok(StaticServer { addr, handle })
}

async fn handle(mut sock: TcpStream, root: &Path) -> std::io::Result<()> {
    let mut buf = vec![0u8; 8192];
    let mut n = 0;
    while n < buf.len() {
        let r = sock.read(&mut buf[n..]).await?;
        if r == 0 {
            return Ok(());
        }
        n += r;
        if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    if method != "GET" && method != "HEAD" {
        return respond(&mut sock, 405, "text/plain", b"method not allowed", false).await;
    }
    let path = target.split(['?', '#']).next().unwrap_or("/");
    if path == "/__slow" {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        return Ok(());
    }
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() || rel.ends_with('/') { format!("{rel}index.html") } else { rel.to_string() };
    let p = Path::new(&rel);
    if p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return respond(&mut sock, 400, "text/plain", b"bad path", method == "HEAD").await;
    }
    let mut full = root.join(p);
    if full.is_dir() {
        full = full.join("index.html");
    }
    if full.extension().is_none() && !full.exists() {
        full.set_extension("html");
    }
    match std::fs::canonicalize(&full) {
        Ok(real) if real.starts_with(root) && real.is_file() => {
            let body = tokio::fs::read(&real).await?;
            respond(&mut sock, 200, mime(&real), &body, method == "HEAD").await
        }
        _ => respond(&mut sock, 404, "text/html", b"<h1>404 Not Found</h1>", method == "HEAD").await,
    }
}

fn mime(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn respond(sock: &mut TcpStream, code: u16, ctype: &str, body: &[u8], head_only: bool) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Method Not Allowed",
    };
    let hdr = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(hdr.as_bytes()).await?;
    if !head_only {
        sock.write_all(body).await?;
    }
    sock.shutdown().await
}
