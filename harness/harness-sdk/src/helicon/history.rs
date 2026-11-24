//! Reads a session's durable history from the engine (`history/read`) and turns it into the message
//! shape the archive and capture code walks.
//!
//! The engine's history is append-only and each item has a dense, increasing `index`: that index is
//! what HeliconSearch gets as `turn_index`. It never shifts (compaction trims what the model sees, not
//! this list). Items as the engine stores them:
//!   user            `{role:"user", content: string | blocks}`
//!   assistant text  `{type:"message", role:"assistant", content:[{type:"output_text", text}]}`
//!   tool call       `{type:"function_call", call_id, name, arguments}`
//!   tool result     `{type:"function_call_output", call_id, output}`
//! Reasoning items are never returned. The per-turn addendum (recalled memory) is not part of the
//! history. With `recallPlacement: user` the recall is in front of the user's message and so is in the history: the
//! archive and capture cut the `<relevant-memories>` envelope out of a message (`transcript.rs`, `strategy.rs`).
use crate::HarnessClient;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::collections::HashSet;

const MAX_PAGES: usize = 200;

pub(crate) struct HistoryPage {
    pub items: Vec<(usize, Value)>,
    /// The next index to read from.
    pub next: usize,
    /// The read stopped (a very long history) with more left: `next` is not the end of the history.
    pub more: bool,
}

/// Where a session's history is read from: the engine, or a stand-in in tests.
pub(crate) trait HistorySource: Send + Sync + 'static {
    /// Every history item from index `after` on.
    fn read<'a>(
        &'a self,
        session_id: &'a str,
        after: usize,
    ) -> BoxFuture<'a, Result<HistoryPage, String>>;
}

impl HistorySource for HarnessClient {
    fn read<'a>(
        &'a self,
        session_id: &'a str,
        after: usize,
    ) -> BoxFuture<'a, Result<HistoryPage, String>> {
        Box::pin(read_history(self, session_id, after))
    }
}

/// The engine's history held weakly: the extension must not be what keeps an engine running, so a
/// harness dropped without `shutdown` still stops (the extension's tasks end with it).
pub(crate) struct WeakEngine(pub std::sync::Weak<crate::Handle>);

impl HistorySource for WeakEngine {
    fn read<'a>(
        &'a self,
        session_id: &'a str,
        after: usize,
    ) -> BoxFuture<'a, Result<HistoryPage, String>> {
        Box::pin(async move {
            match self.0.upgrade() {
                Some(inner) => read_history(&HarnessClient { inner }, session_id, after).await,
                None => Err("the harness is gone".into()),
            }
        })
    }
}

async fn read_history(
    client: &HarnessClient,
    session_id: &str,
    after: usize,
) -> Result<HistoryPage, String> {
    let mut items = Vec::new();
    let mut cursor = after;
    let mut more_left = false;
    for _ in 0..MAX_PAGES {
        let page = client
            .request(
                "history/read",
                json!({"session_id": session_id, "after": cursor}),
            )
            .await
            .map_err(|e| format!("history/read: {e}"))?;
        for entry in page["items"].as_array().into_iter().flatten() {
            if let (Some(index), true) = (entry["index"].as_u64(), entry["item"].is_object()) {
                items.push((index as usize, entry["item"].clone()));
            }
        }
        let next = page["cursor"]
            .as_u64()
            .map(|c| c as usize)
            .unwrap_or(cursor);
        let more = page["has_more"] == Value::Bool(true) && next > cursor;
        cursor = next;
        more_left = more;
        if !more {
            break;
        }
    }
    Ok(HistoryPage {
        items,
        next: cursor,
        more: more_left,
    })
}

/// What `adapt_item` needs to know.
pub(crate) struct AdaptContext<'a> {
    /// The model-facing name prefix of this adapter's own tools (`helicon__`): never archived.
    pub own_tool_prefix: &'a str,
    /// Call ids of this adapter's tool calls seen so far (their results are skipped too).
    pub own_call_ids: &'a mut HashSet<String>,
    /// Stamped on every message: the history carries no per-item time.
    pub now: u64,
}

fn blocks_of(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) => vec![json!({"type": "text", "text": text})],
        Value::Array(items) => items
            .iter()
            .filter_map(|b| {
                let kind = b["type"].as_str().unwrap_or("");
                match kind {
                    "input_text" | "output_text" | "text" => b["text"]
                        .as_str()
                        .map(|t| json!({"type": "text", "text": t})),
                    "input_image" | "image" => Some(json!({"type": "image"})),
                    "input_file" | "document" => Some(match b["filename"].as_str() {
                        Some(name) => json!({"type": "document", "name": name}),
                        None => json!({"type": "document"}),
                    }),
                    _ => None,
                }
            })
            .collect(),
        _ => vec![],
    }
}

/// One history item as a conversation message, or `None` when it is not part of what is archived
/// (reasoning, runtime notices, this adapter's own tool traffic).
pub(crate) fn adapt_item(item: &Value, ctx: &mut AdaptContext<'_>) -> Option<Value> {
    let timestamp = ctx.now;
    match item["type"].as_str() {
        Some("function_call") => {
            let name = item["name"].as_str().unwrap_or("unknown_tool");
            let call_id = item["call_id"].as_str().unwrap_or("");
            if name.starts_with(ctx.own_tool_prefix) {
                if !call_id.is_empty() {
                    ctx.own_call_ids.insert(call_id.to_owned());
                }
                return None;
            }
            let input = match &item["arguments"] {
                Value::String(text) => {
                    serde_json::from_str(text).unwrap_or_else(|_| json!({"arguments": text}))
                }
                Value::Null => json!({}),
                other => other.clone(),
            };
            return Some(json!({
                "role": "assistant",
                "content": [{"type": "tool_use", "id": call_id, "name": name, "input": input}],
                "timestamp": timestamp,
            }));
        }
        Some("function_call_output") => {
            let call_id = item["call_id"].as_str().unwrap_or("");
            if ctx.own_call_ids.contains(call_id) {
                return None;
            }
            let output = match &item["output"] {
                Value::String(text) => text.clone(),
                Value::Null => "\"\"".into(),
                other => other.to_string(),
            };
            return Some(json!({
                "role": "tool",
                "content": [{"type": "tool_result", "tool_use_id": call_id, "content": output}],
                "timestamp": timestamp,
            }));
        }
        _ => {}
    }
    let role = item["role"].as_str()?;
    if role != "user" && role != "assistant" {
        return None;
    }
    // injected by the runtime, not said by anyone
    if !matches!(
        item.get("runtime_notice"),
        None | Some(Value::Null | Value::Bool(false))
    ) {
        return None;
    }
    let blocks = blocks_of(&item["content"]);
    if blocks.is_empty() {
        return None;
    }
    Some(json!({"role": role, "content": blocks, "timestamp": timestamp}))
}

/// A sparse message list indexed by history index (holes for what is not archived).
pub(crate) fn to_messages(page: &HistoryPage, ctx: &mut AdaptContext<'_>) -> Vec<Option<Value>> {
    let mut messages: Vec<Option<Value>> = vec![None; page.next];
    for (index, item) in &page.items {
        if let Some(m) = adapt_item(item, ctx) {
            if *index < messages.len() {
                messages[*index] = Some(m);
            }
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(own: &'a mut HashSet<String>) -> AdaptContext<'a> {
        AdaptContext {
            own_tool_prefix: "helicon__",
            own_call_ids: own,
            now: 1_000,
        }
    }

    #[test]
    fn text_items_become_blocks() {
        let mut own = HashSet::new();
        let m = adapt_item(&json!({"role":"user","content":"hi"}), &mut ctx(&mut own)).unwrap();
        assert_eq!(m["content"][0], json!({"type":"text","text":"hi"}));
        assert_eq!(m["timestamp"], 1_000);
        let a = adapt_item(
            &json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"yo"}]}),
            &mut ctx(&mut own),
        )
        .unwrap();
        assert_eq!(a["content"][0]["text"], "yo");
    }

    #[test]
    fn attachments_and_notices() {
        let mut own = HashSet::new();
        let m = adapt_item(
            &json!({"role":"user","content":[{"type":"input_image"},{"type":"input_file","filename":"a.pdf"}]}),
            &mut ctx(&mut own),
        )
        .unwrap();
        assert_eq!(m["content"][0], json!({"type":"image"}));
        assert_eq!(m["content"][1], json!({"type":"document","name":"a.pdf"}));
        assert!(adapt_item(
            &json!({"role":"user","content":"x","runtime_notice":true}),
            &mut ctx(&mut own)
        )
        .is_none());
        assert!(adapt_item(&json!({"type":"reasoning"}), &mut ctx(&mut own)).is_none());
    }

    #[test]
    fn tool_calls_are_kept_except_our_own() {
        let mut own = HashSet::new();
        let call = adapt_item(
            &json!({"type":"function_call","call_id":"c1","name":"read","arguments":"{\"p\":1}"}),
            &mut ctx(&mut own),
        )
        .unwrap();
        assert_eq!(call["content"][0]["input"], json!({"p":1}));
        let bad = adapt_item(
            &json!({"type":"function_call","call_id":"c2","name":"read","arguments":"{oops"}),
            &mut ctx(&mut own),
        )
        .unwrap();
        assert_eq!(bad["content"][0]["input"], json!({"arguments":"{oops"}));
        assert!(adapt_item(
            &json!({"type":"function_call","call_id":"c3","name":"helicon__memory_recall","arguments":"{}"}),
            &mut ctx(&mut own)
        )
        .is_none());
        assert!(adapt_item(
            &json!({"type":"function_call_output","call_id":"c3","output":"x"}),
            &mut ctx(&mut own)
        )
        .is_none());
        let out = adapt_item(
            &json!({"type":"function_call_output","call_id":"c1","output":{"a":1}}),
            &mut ctx(&mut own),
        )
        .unwrap();
        assert_eq!(out["role"], "tool");
        assert_eq!(out["content"][0]["content"], "{\"a\":1}");
    }

    #[test]
    fn the_message_list_is_sparse_by_history_index() {
        let mut own = HashSet::new();
        let page = HistoryPage {
            items: vec![
                (0, json!({"role":"user","content":"a"})),
                (1, json!({"type":"reasoning"})),
                (2, json!({"role":"user","content":"b"})),
            ],
            next: 3,
            more: false,
        };
        let messages = to_messages(&page, &mut ctx(&mut own));
        assert_eq!(messages.len(), 3);
        assert!(messages[0].is_some() && messages[1].is_none() && messages[2].is_some());
    }
}
