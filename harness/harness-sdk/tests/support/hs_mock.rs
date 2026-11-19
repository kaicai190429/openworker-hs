//! A fake HeliconSearch for the integration tests: a loopback HTTP server that answers with what a
//! closure says and records every request it gets. (The unit tests have their own copy in the crate.)
#![allow(dead_code)]
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    /// The path with its query string, as sent.
    pub path: String,
    pub body: Value,
    pub auth: Option<String>,
}

pub struct Reply {
    status: u16,
    body: Value,
}

impl Reply {
    /// HTTP 200 with this JSON as it is.
    pub fn ok(body: Value) -> Self {
        Self { status: 200, body }
    }
    pub fn status(status: u16, body: Value) -> Self {
        Self { status, body }
    }
}

pub struct MockHs {
    port: u16,
    seen: Arc<Mutex<Vec<Request>>>,
}

impl MockHs {
    pub async fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<Request>>> = Arc::default();
        let handler = Arc::new(handler);
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let handler = handler.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let mut buf: Vec<u8> = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (head_end, content_length) = loop {
                        let n = stream.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..at]).to_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            break (at + 4, len);
                        }
                    };
                    while buf.len() < head_end + content_length {
                        let n = stream.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                    let mut first = head.lines().next().unwrap_or("").split_whitespace();
                    let method = first.next().unwrap_or("").to_owned();
                    let path = first.next().unwrap_or("").to_owned();
                    let auth = head.lines().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("authorization")
                            .then(|| v.trim().to_owned())
                    });
                    let body = serde_json::from_slice(&buf[head_end..head_end + content_length])
                        .unwrap_or(Value::Null);
                    let request = Request {
                        method,
                        path,
                        body,
                        auth,
                    };
                    log.lock().unwrap().push(request.clone());
                    let reply = handler(&request);
                    let payload = reply.body.to_string();
                    let response = format!(
                        "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                        reply.status,
                        payload.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self { port, seen }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }
}
