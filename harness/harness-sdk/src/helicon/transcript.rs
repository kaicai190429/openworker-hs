//! A session's messages as the turns HeliconSearch archives (`POST /v1/memory/session/turn`).
//!
//! Content fidelity first: the text of user, assistant and tool messages is kept whole, tool input and
//! output included. `max_chars` is a safety cap on the finished turn text, not a per-block limit.
//! Messages are `serde_json::Value`s in the block shape `history::adapt_item` produces.
use super::{
    filters::{prefix_utf16, strip_relevant_memories_block, utf16_len},
    util::{iso_from_ms, iso_now},
};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

/// A value as text: a string as it is, a number or boolean as written, an object or array as JSON, nothing as "".
pub(crate) fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn timestamp_of(msg: &Value) -> String {
    match msg.get("timestamp") {
        Some(Value::Number(n)) => match n.as_f64() {
            Some(ms) if ms > 0.0 => iso_from_ms(ms as u64),
            _ => iso_now(),
        },
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => iso_now(),
    }
}

fn timestamp_prefix() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // "[Thu 2026-05-21 15:45 GMT+8] " is put in front of user text by some front ends: it is not what the user typed
    R.get_or_init(|| {
        Regex::new(
            r"^\[[A-Za-z0-9_]{3} [0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2} GMT[+-][0-9]+\] ",
        )
        .expect("static pattern")
    })
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|c| {
                if c.get("type").and_then(Value::as_str) == Some("text") {
                    value_text(c.get("text").unwrap_or(&Value::Null))
                } else {
                    String::new()
                }
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        other => value_text(other),
    }
}

/// A message's content as one text; blocks are joined by blank lines.
pub(crate) fn content_to_text(content: &Value) -> String {
    let blocks = match content {
        Value::String(s) => return s.clone(),
        Value::Array(blocks) => blocks,
        other => return value_text(other),
    };
    let mut parts: Vec<String> = Vec::new();
    for b in blocks {
        let Some(kind) = b.get("type").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "text" => {
                if let Some(text) = b.get("text").and_then(Value::as_str) {
                    parts.push(text.to_owned());
                }
            }
            "tool_use" => {
                let name = b
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown_tool");
                let input = b
                    .get("input")
                    .map(|i| serde_json::to_string_pretty(i).unwrap_or_else(|_| value_text(i)))
                    .unwrap_or_else(|| "{}".into());
                parts.push(format!("[tool_use: {name}]\n{input}"));
            }
            "toolCall" => {
                // the name only: the arguments can be kilobytes and would drown the conversation
                let rec = b.get("toolCall").filter(|v| v.is_object()).unwrap_or(b);
                let name = rec
                    .get("name")
                    .and_then(Value::as_str)
                    .or_else(|| b.get("name").and_then(Value::as_str))
                    .unwrap_or("unknown_tool");
                parts.push(format!("[tool: {name}]"));
            }
            "tool_result" => {
                let id = b
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(|s| format!(" id={s}"))
                    .unwrap_or_default();
                let out = tool_result_text(b.get("content").unwrap_or(&Value::Null));
                parts.push(format!("[tool_result{id}]\n{out}"));
            }
            "image" => parts.push("[image attachment]".into()),
            "document" => {
                let name = b
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|n| format!(" \"{n}\""))
                    .unwrap_or_default();
                parts.push(format!("[document{name}]"));
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

/// The turn text cut to `max_chars` UTF-16 units, never inside a character.
fn cap_text(text: &str, max_chars: usize) -> String {
    if utf16_len(text) <= max_chars {
        return text.to_owned();
    }
    format!("{}\n…[truncated]", prefix_utf16(text, max_chars))
}

fn has_block(msg: &Value, kind: &str) -> bool {
    msg.get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some(kind))
        })
}

/// The archive turn for one message (what `session/turn` stores at this index), or `None` when the
/// message is not archived. HeliconSearch hashes exactly this content, which is what ties an
/// extraction message to its archived turn.
pub(crate) fn render_transcript_turn(
    message: &Value,
    index: usize,
    max_chars: usize,
    run_id: Option<&str>,
) -> Option<Value> {
    let msg = message.as_object().map(|_| message)?;
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("unknown");
    if !matches!(role, "user" | "assistant" | "tool" | "toolResult") {
        return None;
    }
    let converted =
        strip_relevant_memories_block(&content_to_text(msg.get("content").unwrap_or(&Value::Null)));
    let raw = if role == "user" {
        timestamp_prefix().replace(&converted, "").into_owned()
    } else {
        converted
    };
    let text = cap_text(&raw, max_chars);
    // an assistant turn that said nothing keeps its place in the sequence
    let final_text = if text.trim().is_empty() && role == "assistant" {
        "[no response]".to_owned()
    } else {
        text
    };
    if final_text.trim().is_empty() {
        return None;
    }
    let content_type = if role == "tool" || has_block(msg, "tool_result") {
        "tool_result"
    } else if has_block(msg, "tool_use") {
        "tool_use"
    } else {
        "text"
    };
    let mut meta = Map::new();
    if let Some(run) = run_id {
        meta.insert("run_id".into(), json!(run));
    }
    if content_type != "text" {
        meta.insert("content_type".into(), json!(content_type));
    }
    let mut turn = json!({
        "turn_index": index,
        "role": role,
        "content": final_text,
        "content_type": content_type,
        "created_at": timestamp_of(msg),
    });
    if !meta.is_empty() {
        turn["metadata"] = Value::Object(meta);
    }
    Some(turn)
}

/// The archive turns of `messages[start..]`, holes (`None`) skipped.
pub(crate) fn extract_turns(
    messages: &[Option<Value>],
    start: usize,
    max_chars: usize,
    run_id: Option<&str>,
) -> Vec<Value> {
    messages
        .iter()
        .enumerate()
        .skip(start)
        .filter_map(|(i, m)| {
            m.as_ref()
                .and_then(|m| render_transcript_turn(m, i, max_chars, run_id))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(msg: Value) -> Option<Value> {
        render_transcript_turn(&msg, 3, 20_000, None)
    }

    #[test]
    fn user_text_is_kept_and_the_timestamp_prefix_removed() {
        let t = turn(json!({"role":"user","content":[{"type":"text","text":"[Thu 2026-05-21 15:45 GMT+8] hello"}],"timestamp":1_748_773_800_000u64})).unwrap();
        assert_eq!(t["content"], "hello");
        assert_eq!(t["turn_index"], 3);
        assert_eq!(t["content_type"], "text");
        assert_eq!(t["created_at"], "2025-06-01T10:30:00.000Z");
        assert!(t.get("metadata").is_none());
    }

    #[test]
    fn tool_traffic_is_rendered_whole() {
        let t = turn(json!({"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"read","input":{"path":"a"}}]})).unwrap();
        assert_eq!(t["content"], "[tool_use: read]\n{\n  \"path\": \"a\"\n}");
        assert_eq!(t["content_type"], "tool_use");
        assert_eq!(t["metadata"]["content_type"], "tool_use");
        let r = turn(json!({"role":"tool","content":[{"type":"tool_result","tool_use_id":"c1","content":"ok"}]})).unwrap();
        assert_eq!(r["content"], "[tool_result id=c1]\nok");
        assert_eq!(r["content_type"], "tool_result");
    }

    #[test]
    fn an_empty_assistant_turn_keeps_its_place_but_an_empty_user_turn_does_not() {
        let a = turn(json!({"role":"assistant","content":[{"type":"image"}]})).unwrap();
        assert_eq!(a["content"], "[image attachment]");
        assert!(turn(json!({"role":"user","content":[]})).is_none());
        let a = turn(json!({"role":"assistant","content":[]})).unwrap();
        assert_eq!(a["content"], "[no response]");
        assert!(turn(json!({"role":"system","content":"x"})).is_none());
    }

    #[test]
    fn recalled_memories_never_reach_the_archive() {
        let t = turn(json!({"role":"user","content":"<relevant-memories>\nx\n</relevant-memories>\n\nreal words"})).unwrap();
        assert_eq!(t["content"], "real words");
    }

    #[test]
    fn long_text_is_cut_with_a_marker() {
        let msg = json!({"role":"user","content":"a".repeat(30)});
        let t = render_transcript_turn(&msg, 0, 10, Some("r1")).unwrap();
        assert_eq!(t["content"], format!("{}\n…[truncated]", "a".repeat(10)));
        assert_eq!(t["metadata"]["run_id"], "r1");
    }

    #[test]
    fn turns_are_taken_from_a_start_index_over_holes() {
        let messages = vec![
            Some(json!({"role":"user","content":"a"})),
            None,
            Some(json!({"role":"assistant","content":"b"})),
        ];
        let turns = extract_turns(&messages, 1, 100, None);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0]["turn_index"], 2);
    }
}
