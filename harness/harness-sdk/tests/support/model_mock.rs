//! A fake model for the integration tests: an OpenAI-compatible chat-completions server on loopback. It
//! records every request body the engine sends it and answers with text or one tool call, streamed.
#![allow(dead_code)]
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// What the fake model says to a request.
pub enum Answer {
    Text(String),
    /// Call this tool (its model-facing name) with these arguments.
    Tool {
        name: String,
        arguments: Value,
    },
    /// The same answer after this many milliseconds (a turn that is still running meanwhile).
    Slow {
        ms: u64,
        then: Box<Answer>,
    },
}

pub struct MockModel {
    port: u16,
    seen: Arc<Mutex<Vec<Value>>>,
}

fn sse(answer: &Answer, call: usize) -> String {
    let frame = |v: Value| format!("data: {v}\n\n");
    match answer {
        Answer::Slow { then, .. } => sse(then, call),
        Answer::Text(text) => {
            frame(
                json!({"id": "fixture", "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]}),
            ) + &frame(
                json!({"id": "fixture", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            ) + "data: [DONE]\n\n"
        }
        Answer::Tool { name, arguments } => {
            frame(
                json!({"id": "fixture", "choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": 0, "id": format!("call_fixture_{call}"), "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}
            }]}, "finish_reason": null}]}),
            ) + &frame(
                json!({"id": "fixture", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            ) + "data: [DONE]\n\n"
        }
    }
}

impl MockModel {
    pub async fn start(respond: impl Fn(&Value) -> Answer + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let respond = Arc::new(respond);
        let log = seen.clone();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let (respond, log, counter) = (respond.clone(), log.clone(), counter.clone());
                tokio::spawn(async move {
                    let mut buf: Vec<u8> = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (head_end, length) = loop {
                        let n = stream.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..at]).to_lowercase();
                            let length = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            break (at + 4, length);
                        }
                    };
                    while buf.len() < head_end + length {
                        let n = stream.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let body: Value = serde_json::from_slice(&buf[head_end..head_end + length])
                        .unwrap_or(Value::Null);
                    log.lock().unwrap().push(body.clone());
                    let call = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mut answer = respond(&body);
                    if let Answer::Slow { ms, then } = answer {
                        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                        answer = *then;
                    }
                    let payload = sse(&answer, call);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self { port, seen }
    }

    /// The base URL to give the provider (the engine appends `/chat/completions`).
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// Every request body the model has been sent, oldest first.
    pub fn requests(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }
}

/// The system instructions of a chat-completions request.
pub fn system_of(request: &Value) -> String {
    request["messages"][0]["content"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

/// The names of the tools a request offers.
pub fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
        .collect()
}

/// Every message of a request except the system instructions, as one text.
pub fn conversation_of(request: &Value) -> String {
    request["messages"]
        .as_array()
        .map(|m| {
            m.iter()
                .skip(1)
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}
