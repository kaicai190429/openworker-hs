//! How a slice of a session's messages becomes HeliconSearch writes.
//!
//! - `Direct`: one `POST /v1/memory/store` per user sentence that passes the rule filters.
//! - `Operator`: the slice (both sides of the conversation) goes to `POST /v1/memory/add`, where
//!   the server's operator extracts the facts.
use super::{
    client::{AddResult, HeliconClient, StoreStatus},
    config::{CaptureMode, MemoryConfig},
    filters::{detect_category, should_capture, strip_relevant_memories_block, CaptureOptions},
    log::Logger,
    source_ref::make_session_ref,
    transcript::render_transcript_turn,
    util::iso_from_ms,
};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

const TAG: &str = "[helicon-memory]";
/// Writes of one slice the direct strategy makes at most: no runaway batches.
const MAX_CAPTURE_PER_TURN: usize = 3;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CaptureReport {
    pub considered: usize,
    pub capturable: usize,
    pub stored: usize,
    pub duplicates: usize,
    pub skipped: usize,
    pub errors: usize,
}

/// The receipt of an archived turn: what ties an extraction message to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArchiveRef {
    pub turn_id: String,
    pub content_hash: String,
}

pub(crate) struct CaptureContext<'a> {
    pub messages: &'a [Option<Value>],
    pub start_index: usize,
    /// The HeliconSearch session id.
    pub session_id: &'a str,
    pub cfg: &'a MemoryConfig,
    pub session_started_at: &'a str,
    /// This session's confirmed archive entries by `turn_index`. A ref is only attached while the
    /// message at that index still is the archived one (see `archive_ref_for`).
    pub archive_refs: &'a HashMap<usize, ArchiveRef>,
}

pub(crate) fn name(mode: CaptureMode) -> &'static str {
    match mode {
        CaptureMode::Direct => "direct",
        CaptureMode::Operator => "operator",
    }
}

pub(crate) async fn capture(
    client: &HeliconClient,
    logger: &Logger,
    ctx: &CaptureContext<'_>,
) -> CaptureReport {
    match ctx.cfg.capture_mode {
        CaptureMode::Direct => capture_direct(client, logger, ctx).await,
        CaptureMode::Operator => capture_operator(client, logger, ctx).await,
    }
}

fn clip(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

/// The text of a user message's text blocks (a string content is one text).
pub(crate) fn user_texts(message: &Value) -> Vec<String> {
    if message["role"] != "user" {
        return vec![];
    }
    match &message["content"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str().map(str::to_owned))
            .collect(),
        _ => vec![],
    }
}

async fn capture_direct(
    client: &HeliconClient,
    logger: &Logger,
    ctx: &CaptureContext<'_>,
) -> CaptureReport {
    let cfg = ctx.cfg;
    let mut report = CaptureReport::default();
    let mut writes = 0;
    for (index, message) in ctx.messages.iter().enumerate().skip(ctx.start_index) {
        let Some(message) = message else { continue };
        let texts = user_texts(message);
        if texts.is_empty() {
            continue;
        }
        report.considered += 1;
        for text in texts {
            logger.info(&format!(
                "{TAG} auto_capture evaluating msgIdx={index} len={} text=\"{}\"",
                text.chars().count(),
                clip(&text, 120)
            ));
            let options = CaptureOptions {
                custom_triggers: cfg.custom_triggers.as_deref().unwrap_or(&[]),
                max_chars: cfg.capture_max_chars,
            };
            if !should_capture(&text, &options) {
                logger.info(&format!(
                    "{TAG} auto_capture no trigger match msgIdx={index} text=\"{}\"",
                    clip(&text, 80)
                ));
                continue;
            }
            report.capturable += 1;
            if writes >= MAX_CAPTURE_PER_TURN {
                report.skipped += 1;
                logger.info(&format!(
                    "{TAG} capture skip msgIdx={index} (MAX_CAPTURE_PER_TURN={MAX_CAPTURE_PER_TURN} reached)"
                ));
                continue;
            }
            let category = detect_category(&text);
            let request = json!({
                "text": text,
                "agent_id": cfg.agent_id,
                "project_id": cfg.project_id,
                "category": category,
                "scope": "project",
                "importance": 0.5,
                "resolution": "fact",
                "extraction_method": "auto_capture_direct",
                "confidence": 0.85,
                "source_refs": [make_session_ref(ctx.session_id)],
            });
            match client.store(request).await {
                Ok((id, status)) => {
                    writes += 1;
                    match status {
                        StoreStatus::Created => {
                            report.stored += 1;
                            logger.info(&format!(
                                "{TAG} capture stored id={id} category={category} text=\"{}\"",
                                clip(&text, 60)
                            ));
                        }
                        StoreStatus::Dedupe => {
                            report.duplicates += 1;
                            logger.info(&format!(
                                "{TAG} capture duplicate id={id} text=\"{}\"",
                                clip(&text, 60)
                            ));
                        }
                    }
                }
                Err(err) => {
                    report.errors += 1;
                    logger.warn(&format!(
                        "{TAG} capture FAILED {} text=\"{}\"",
                        err.describe(),
                        clip(&text, 60)
                    ));
                }
            }
        }
    }
    report
}

/// A message's text for the operator, from either side of the conversation: text blocks and the
/// names of tools called (so the operator knows what was done).
fn any_role_text(message: &Value) -> Option<(String, String)> {
    let role = message["role"].as_str()?;
    if role != "user" && role != "assistant" {
        return None;
    }
    let text = match &message["content"] {
        Value::String(s) => s.trim().to_owned(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().map(str::to_owned),
                Some("tool_use") => b["name"].as_str().map(|n| format!("[tool: {n}]")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_owned(),
        _ => return None,
    };
    // the memories we recalled into the prompt are not what the user said
    let cleaned = strip_relevant_memories_block(&text);
    (!cleaned.is_empty()).then(|| (role.to_owned(), cleaned))
}

/// The archive ref for `message` at `index`, only if that message still is the one archived there.
///
/// The message is rendered exactly as the archive path renders it and must hash to the ref's
/// `content_hash` (HeliconSearch hashes the same text). No match, no ref: the server then matches the
/// message by index and text, and processes it as sent if that fails too.
fn archive_ref_for(
    refs: &HashMap<usize, ArchiveRef>,
    message: &Value,
    index: usize,
    max_chars: usize,
) -> Option<ArchiveRef> {
    let reference = refs.get(&index)?;
    let turn = render_transcript_turn(message, index, max_chars, None)?;
    let digest = Sha256::digest(turn["content"].as_str()?.as_bytes());
    let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    (hash == reference.content_hash).then(|| reference.clone())
}

fn first_turns(turns: &[i64]) -> String {
    turns
        .iter()
        .take(10)
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

async fn capture_operator(
    client: &HeliconClient,
    logger: &Logger,
    ctx: &CaptureContext<'_>,
) -> CaptureReport {
    let cfg = ctx.cfg;
    let mut messages: Vec<Value> = Vec::new();
    for (i, message) in ctx.messages.iter().enumerate().skip(ctx.start_index) {
        let Some(message) = message else { continue };
        let Some((role, text)) = any_role_text(message) else {
            continue;
        };
        // Two evidence anchors, both from the same message the archive sees: `turn_index` (the
        // history index, which is what the archive stamps) and `turn_created_at` (the message time).
        let mut entry = Map::new();
        entry.insert("role".into(), json!(role));
        entry.insert("content".into(), json!(text));
        entry.insert("turn_index".into(), json!(i));
        if let Some(ms) = message["timestamp"].as_f64().filter(|ms| *ms > 0.0) {
            entry.insert("turn_created_at".into(), json!(iso_from_ms(ms as u64)));
        }
        if let Some(reference) = archive_ref_for(
            ctx.archive_refs,
            message,
            i,
            cfg.transcript_max_chars_per_turn,
        ) {
            entry.insert(
                "archive_ref".into(),
                json!({"turn_id": reference.turn_id, "content_hash": reference.content_hash}),
            );
        }
        messages.push(Value::Object(entry));
    }
    let mut report = CaptureReport {
        considered: messages.len(),
        capturable: messages.len(),
        ..Default::default()
    };
    if messages.is_empty() {
        return report;
    }
    let sent = messages.len();
    let mut request = json!({
        "messages": messages,
        "agent_id": cfg.agent_id,
        "project_id": cfg.project_id,
        "source_refs": [make_session_ref(ctx.session_id)],
        "session_id": ctx.session_id,
        // the auto-capture path is the one caller that wants the server-side store gate
        "store_gate": cfg.store_gate,
    });
    if !ctx.session_started_at.is_empty() {
        request["session_datetime"] = json!(ctx.session_started_at);
    }
    let session = ctx.session_id;
    let result: AddResult = match client.add(request).await {
        Ok(r) => r,
        Err(err) => {
            report.errors = sent;
            logger.warn(&format!("{TAG} operator capture FAILED {}", err.describe()));
            return report;
        }
    };
    if !result.archive_mismatched.is_empty() {
        logger.warn(&format!(
            "{TAG} operator capture: {} message(s) do not match the archive and were not extracted (turn_index {}) sessionId={session}",
            result.archive_mismatched.len(),
            first_turns(&result.archive_mismatched)
        ));
    }
    if !result.not_archived.is_empty() {
        // The server extracts archived turns only: these were not in its archive (their archive call
        // failed). Count them as not done so the cursor goes back and they are sent again once archived.
        report.errors += result.not_archived.len();
        logger.warn(&format!(
            "{TAG} operator capture: {} message(s) not in the archive yet, not extracted (turn_index {}) sessionId={session} - sent again after archiving",
            result.not_archived.len(),
            first_turns(&result.not_archived)
        ));
    }
    if !result.archive_conflicts.is_empty() {
        logger.warn(&format!(
            "{TAG} operator capture: {} message(s) conflict with the archive and were not extracted (turn_index {}) sessionId={session}",
            result.archive_conflicts.len(),
            first_turns(&result.archive_conflicts)
        ));
    }
    if result.queued {
        logger.info(&format!(
            "{TAG} operator capture: queued on heliconsearch as work unit {} (sessionId={session} msgs={sent})",
            result.work_unit_id.as_deref().unwrap_or("?")
        ));
        return report;
    }
    if result.already_processed {
        logger.info(&format!(
            "{TAG} operator capture: already processed on heliconsearch (sessionId={session} msgs={sent}) - nothing extracted again"
        ));
        return report;
    }
    if result.operations.is_empty() {
        logger.info(&format!(
            "{TAG} operator capture: no operations returned (sessionId={session} msgs={sent} processing={})",
            result.processing_state.as_deref().unwrap_or("none")
        ));
        return report;
    }
    for op in &result.operations {
        match op["action"].as_str() {
            Some("ADD" | "UPDATE" | "DELETE") => report.stored += 1,
            Some("NOOP") => report.skipped += 1,
            _ => report.errors += 1,
        }
        logger.info(&format!(
            "{TAG} capture op={} memoryId={} status={} reason=\"{}\"",
            op["action"].as_str().unwrap_or("?"),
            op["memory_id"].as_str().unwrap_or("?"),
            op["status"].as_str().unwrap_or("?"),
            op["reason"].as_str().unwrap_or("")
        ));
    }
    logger.info(&format!(
        "{TAG} operator capture done: stored={} skipped={} errors={} total={}",
        report.stored,
        report.skipped,
        report.errors,
        result.operations.len()
    ));
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_comes_from_user_messages_only() {
        assert_eq!(user_texts(&json!({"role":"user","content":"a"})), vec!["a"]);
        assert_eq!(
            user_texts(
                &json!({"role":"user","content":[{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}]})
            ),
            vec!["a", "b"]
        );
        assert!(user_texts(&json!({"role":"assistant","content":"a"})).is_empty());
    }

    #[test]
    fn the_operator_sees_both_sides_and_tool_names() {
        let (role, text) = any_role_text(&json!({"role":"assistant","content":[
            {"type":"text","text":"done"},
            {"type":"tool_use","name":"read","input":{}}]}))
        .unwrap();
        assert_eq!(role, "assistant");
        assert_eq!(text, "done [tool: read]");
        assert!(any_role_text(&json!({"role":"tool","content":"x"})).is_none());
        assert!(any_role_text(
            &json!({"role":"user","content":"<relevant-memories>\nx\n</relevant-memories>"})
        )
        .is_none());
    }

    #[test]
    fn a_ref_is_attached_only_while_the_message_hashes_to_it() {
        let message = json!({"role":"user","content":"hello"});
        let digest = Sha256::digest(b"hello");
        let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        let mut refs = HashMap::new();
        refs.insert(
            4,
            ArchiveRef {
                turn_id: "t".into(),
                content_hash: hash,
            },
        );
        assert!(archive_ref_for(&refs, &message, 4, 1000).is_some());
        // the message at that index changed since it was archived
        let other = json!({"role":"user","content":"hello!"});
        assert!(archive_ref_for(&refs, &other, 4, 1000).is_none());
        assert!(archive_ref_for(&refs, &message, 5, 1000).is_none());
    }
}
