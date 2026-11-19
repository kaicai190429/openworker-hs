//! A fake HeliconSearch for tests: a loopback HTTP server that answers with what a closure says and
//! records every request it gets.
//!
//! A handler that panics (an `assert!`, or `panic!("unexpected ...")` for a path it does not know) does
//! not vanish into a dropped connection: the server answers 500, remembers the message, and the test
//! fails when the server is dropped.
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// The URL of a port that nothing listens on (one that was free a moment ago), for "the server is down".
pub(crate) fn nothing_listens() -> String {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    format!("http://127.0.0.1:{port}")
}

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub method: String,
    /// The path with its query string, as sent.
    pub path: String,
    pub body: Value,
    pub auth: Option<String>,
}

pub(crate) struct Reply {
    status: u16,
    body: Value,
    delay: Duration,
}

impl Reply {
    /// HTTP 200 with this JSON as it is.
    pub(crate) fn ok(body: Value) -> Self {
        Self {
            status: 200,
            body,
            delay: Duration::ZERO,
        }
    }
    pub(crate) fn status(status: u16, body: Value) -> Self {
        Self {
            status,
            body,
            delay: Duration::ZERO,
        }
    }
    /// The same answer, after this long (a slow server).
    pub(crate) fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

pub(crate) struct MockHs {
    port: u16,
    seen: Arc<Mutex<Vec<Request>>>,
    failures: Arc<Mutex<Vec<String>>>,
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "a handler panicked".into())
}

impl MockHs {
    pub(crate) async fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<Request>>> = Arc::default();
        let failures: Arc<Mutex<Vec<String>>> = Arc::default();
        let handler = Arc::new(handler);
        let (log, problems) = (seen.clone(), failures.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let (handler, log, problems) = (handler.clone(), log.clone(), problems.clone());
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
                    let reply = match catch_unwind(AssertUnwindSafe(|| handler(&request))) {
                        Ok(reply) => reply,
                        Err(payload) => {
                            let text = format!(
                                "{} {}: {}",
                                request.method,
                                request.path,
                                panic_text(payload.as_ref())
                            );
                            problems.lock().unwrap().push(text.clone());
                            Reply::status(500, serde_json::json!({"error": text}))
                        }
                    };
                    if !reply.delay.is_zero() {
                        tokio::time::sleep(reply.delay).await;
                    }
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
        Self {
            port,
            seen,
            failures,
        }
    }

    pub(crate) fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub(crate) fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for MockHs {
    fn drop(&mut self) {
        let failures = self.failures.lock().unwrap().clone();
        if !failures.is_empty() && !std::thread::panicking() {
            panic!(
                "the fake HeliconSearch's handler failed: {}",
                failures.join("; ")
            );
        }
    }
}

use crate::helicon::history::{HistoryPage, HistorySource};

/// A history the test controls, in place of the engine's.
pub(crate) struct FakeHistory {
    pub items: Mutex<Vec<(usize, Value)>>,
    /// At most this many items per read (the rest is `more`), like a history longer than one read covers.
    pub per_read: Mutex<Option<usize>>,
    /// How many times the history was read (one per capture cycle).
    pub reads: std::sync::atomic::AtomicUsize,
}

impl FakeHistory {
    pub(crate) fn new(items: Vec<(usize, Value)>) -> Self {
        Self {
            items: Mutex::new(items),
            per_read: Mutex::new(None),
            reads: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl HistorySource for FakeHistory {
    fn read<'a>(
        &'a self,
        _session_id: &'a str,
        after: usize,
    ) -> BoxFuture<'a, Result<HistoryPage, String>> {
        Box::pin(async move {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let items = self.items.lock().unwrap();
            let mut wanted: Vec<(usize, Value)> =
                items.iter().filter(|(i, _)| *i >= after).cloned().collect();
            let more = match *self.per_read.lock().unwrap() {
                Some(n) if wanted.len() > n => {
                    wanted.truncate(n);
                    true
                }
                _ => false,
            };
            let next = if more {
                wanted.last().map_or(after, |(i, _)| i + 1)
            } else {
                items.len().max(after)
            };
            Ok(HistoryPage {
                items: wanted,
                next,
                more,
            })
        })
    }
}
