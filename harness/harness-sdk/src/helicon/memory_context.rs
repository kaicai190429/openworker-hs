//! `memory_get`'s conversation part: where a memory was said (its source turns, from
//! `/v1/memory/source/<id>`), the archived messages around that (`/v1/memory/session/turns`) — both
//! sides of the exchange, the assistant's replies too, which recall does not show — or the whole
//! conversation page by page; and how that is put in front of the model.
use regex::Regex;
use serde_json::Value;
use std::{collections::HashSet, sync::OnceLock};

/// Messages shown on each side of the source by default.
pub(crate) const DEFAULT_CONTEXT_WINDOW: i64 = 3;
/// At most this many on each side: more of the conversation is read with `context="session"`.
pub(crate) const MAX_CONTEXT_WINDOW: i64 = 20;
/// Messages per page when the whole conversation is read.
pub(crate) const SESSION_PAGE_SIZE: i64 = 40;

/// `{session id: the turn_index of each message the memory was taken from}`, sessions in source order.
pub(crate) fn source_turns(source: Option<&Value>) -> Vec<(String, Vec<i64>)> {
    let mut out: Vec<(String, Vec<i64>)> = Vec::new();
    let mut add = |sid: &Value, idx: &Value| {
        let (Some(sid), Some(idx)) = (sid.as_str().filter(|s| !s.is_empty()), idx.as_i64()) else {
            return;
        };
        match out.iter_mut().find(|(s, _)| s == sid) {
            Some((_, list)) => {
                if !list.contains(&idx) {
                    list.push(idx);
                }
            }
            None => out.push((sid.to_owned(), vec![idx])),
        }
    };
    if let Some(source) = source {
        for key in ["turns", "evidence"] {
            for t in source[key].as_array().into_iter().flatten() {
                add(&t["session_id"], &t["turn_index"]);
            }
        }
    }
    for (_, list) in &mut out {
        list.sort_unstable();
    }
    out
}

/// The sessions a memory names as its source (`source_refs`), in order, whether or not a turn is known.
pub(crate) fn source_sessions(source: Option<&Value>, turns: &[(String, Vec<i64>)]) -> Vec<String> {
    let mut out: Vec<String> = turns.iter().map(|(s, _)| s.clone()).collect();
    if let Some(source) = source {
        for r in source["source_refs"].as_array().into_iter().flatten() {
            if r["type"] == "session" {
                if let Some(id) = r["ref_id"].as_str().filter(|s| !s.is_empty()) {
                    if !out.iter().any(|s| s == id) {
                        out.push(id.to_owned());
                    }
                }
            }
        }
    }
    out
}

/// A turn spec as recall's RAW_REFS lists it — `"12"`, `"12-14"` (or a number) — as `[first, last]`.
pub(crate) fn parse_turns(spec: &Value) -> Option<(i64, i64)> {
    static R: OnceLock<Regex> = OnceLock::new();
    let re = R.get_or_init(|| {
        Regex::new(r"^\s*([0-9]+)\s*(?:[-–~]\s*([0-9]+))?\s*$").expect("static pattern")
    });
    match spec {
        Value::Number(n) => n.as_i64().filter(|n| *n >= 0).map(|n| (n, n)),
        Value::String(s) => {
            let m = re.captures(s)?;
            let a: i64 = m[1].parse().ok()?;
            let b: i64 = match m.get(2) {
                Some(b) => b.as_str().parse().ok()?,
                None => a,
            };
            Some(if a <= b { (a, b) } else { (b, a) })
        }
        _ => None,
    }
}

/// The window shown around `[first, last]`: `window` messages on each side, clamped to `[0, MAX]`.
pub(crate) fn window_around(first: i64, last: i64, window: i64) -> (i64, i64) {
    let w = window.clamp(0, MAX_CONTEXT_WINDOW);
    ((first - w).max(0), last.saturating_add(w))
}

/// What the marked messages are: where the memory was taken from, or the excerpt asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Marked {
    MemorySource,
    Excerpt,
}

/// One archived message as shown: its index, who said it, when, and all it says.
fn format_turn(t: &Value, marked: &HashSet<i64>, what: Marked) -> String {
    let index = t["turn_index"].as_i64().unwrap_or(-1);
    let mut head = format!("[#{index}] {}", t["role"].as_str().unwrap_or("?"));
    if let Some(at) = t["created_at"].as_str().filter(|s| !s.is_empty()) {
        head.push_str(&format!(" · {at}"));
    }
    if marked.contains(&index) {
        head.push_str(match what {
            Marked::Excerpt => "  ◀ the recalled excerpt",
            Marked::MemorySource => "  ◀ source of this memory",
        });
    }
    let body = if t["omitted"] == Value::Bool(true) {
        "(removed — its user asked for it to be forgotten)"
    } else {
        t["content"].as_str().unwrap_or("")
    };
    format!("{head}\n{body}")
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageMode {
    Around,
    Session,
}

/// A page of a conversation as shown, with where it lies in the session and how to read on.
pub(crate) fn format_page(
    session_id: &str,
    page: &Value,
    source_turns: &[i64],
    mode: PageMode,
    window: i64,
    marked: Marked,
) -> String {
    let turns = page["turns"].as_array().cloned().unwrap_or_default();
    let total = page["total"].as_i64().unwrap_or(0);
    let first_turn = page["first_turn"].as_i64();
    let last_turn = page["last_turn"].as_i64();
    if turns.is_empty() {
        let range = match (first_turn, last_turn) {
            (Some(a), Some(b)) => format!(", turns #{a}–#{b}"),
            (Some(a), None) => format!(", turns #{a}–#"),
            _ => String::new(),
        };
        return format!(
            "(no archived messages of session {session_id} in that range — it holds {total}{range})"
        );
    }
    let a = turns[0]["turn_index"].as_i64().unwrap_or(0);
    let b = turns[turns.len() - 1]["turn_index"].as_i64().unwrap_or(0);
    let more = page["more"] == Value::Bool(true);
    let span = match (first_turn, last_turn) {
        (Some(f), Some(l)) => format!(" (the session holds {total} messages, #{f}–#{l})"),
        _ => String::new(),
    };
    let head = match mode {
        PageMode::Around => format!(
            "--- Conversation around it: session {session_id}, messages #{a}–#{b}{span} ---"
        ),
        PageMode::Session => {
            format!("--- Conversation: session {session_id}, messages #{a}–#{b}{span} ---")
        }
    };
    let mut tail: Vec<String> = Vec::new();
    if mode == PageMode::Session && more {
        tail.push(format!(
            "(more follows: call helicon__memory_get again with context=\"session\", session=\"{session_id}\", fromTurn={})",
            b.saturating_add(1)
        ));
    }
    if mode == PageMode::Around {
        let before = first_turn.is_some_and(|f| a > f);
        let after = more || last_turn.is_some_and(|l| b < l);
        if before || after {
            let side = match (before, after) {
                (true, true) => "before and after",
                (true, false) => "before",
                _ => "after",
            };
            let widen = if window < MAX_CONTEXT_WINDOW {
                format!(
                    "window={} shows more around it, ",
                    (window * 2).clamp(1, MAX_CONTEXT_WINDOW)
                )
            } else {
                String::new()
            };
            tail.push(format!(
                "(the conversation goes on {side} this: {widen}context=\"session\" reads all of it)"
            ));
        }
    }
    let src: HashSet<i64> = source_turns.iter().copied().collect();
    let mut parts = vec![head];
    parts.extend(turns.iter().map(|t| format_turn(t, &src, marked)));
    parts.extend(tail);
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn source_turns_group_by_session_in_source_order() {
        let source = json!({
            "turns": [{"session_id": "s2", "turn_index": 5}, {"session_id": "s1", "turn_index": 9}],
            "evidence": [{"session_id": "s2", "turn_index": 3}, {"session_id": "s2", "turn_index": 5}, {"session_id": "", "turn_index": 1}],
            "source_refs": [{"type": "session", "ref_id": "s3"}, {"type": "session", "ref_id": "s1"}, {"type": "file", "ref_id": "f"}]
        });
        let turns = source_turns(Some(&source));
        assert_eq!(
            turns,
            vec![("s2".to_owned(), vec![3, 5]), ("s1".to_owned(), vec![9])]
        );
        assert_eq!(
            source_sessions(Some(&source), &turns),
            vec!["s2", "s1", "s3"]
        );
        assert!(source_turns(None).is_empty());
    }

    #[test]
    fn turn_specs() {
        assert_eq!(parse_turns(&json!("12")), Some((12, 12)));
        assert_eq!(parse_turns(&json!(" 12 – 14 ")), Some((12, 14)));
        assert_eq!(parse_turns(&json!("14-12")), Some((12, 14)));
        assert_eq!(parse_turns(&json!(7)), Some((7, 7)));
        assert_eq!(parse_turns(&json!("x")), None);
        assert_eq!(parse_turns(&json!(-1)), None);
        assert_eq!(parse_turns(&json!(null)), None);
    }

    #[test]
    fn numbers_near_the_limit_do_not_overflow() {
        assert_eq!(
            window_around(i64::MAX - 1, i64::MAX, 20),
            (i64::MAX - 21, i64::MAX)
        );
        assert_eq!(window_around(0, i64::MAX, 3).1, i64::MAX);
    }

    #[test]
    fn the_window_is_clamped() {
        assert_eq!(window_around(10, 12, 3), (7, 15));
        assert_eq!(window_around(1, 2, 3), (0, 5));
        assert_eq!(window_around(100, 100, 99), (80, 120));
    }

    #[test]
    fn a_page_marks_its_source_and_says_how_to_read_on() {
        let page = json!({
            "turns": [
                {"turn_index": 4, "role": "user", "content": "I like tea", "created_at": "2025-01-01T00:00:00Z", "omitted": false},
                {"turn_index": 5, "role": "assistant", "content": "", "created_at": null, "omitted": true},
            ],
            "total": 20, "first_turn": 0, "last_turn": 19, "more": true
        });
        let text = format_page("s1", &page, &[4], PageMode::Around, 3, Marked::MemorySource);
        assert!(text.starts_with("--- Conversation around it: session s1, messages #4–#5 (the session holds 20 messages, #0–#19) ---"));
        assert!(
            text.contains("[#4] user · 2025-01-01T00:00:00Z  ◀ source of this memory\nI like tea")
        );
        assert!(text.contains("[#5] assistant\n(removed — its user asked for it to be forgotten)"));
        assert!(text.contains("the conversation goes on before and after this: window=6 shows more around it, context=\"session\" reads all of it)"));
        let whole = format_page("s1", &page, &[], PageMode::Session, 3, Marked::Excerpt);
        assert!(whole.contains("(more follows: call helicon__memory_get again with context=\"session\", session=\"s1\", fromTurn=6)"));
    }

    #[test]
    fn an_empty_page_says_what_the_session_holds() {
        let page = json!({"turns": [], "total": 3, "first_turn": 0, "last_turn": 2, "more": false});
        assert_eq!(
            format_page("s1", &page, &[], PageMode::Around, 3, Marked::MemorySource),
            "(no archived messages of session s1 in that range — it holds 3, turns #0–#2)"
        );
    }
}
