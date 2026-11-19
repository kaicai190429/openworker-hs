//! The SessionBridge as this extension shows it: the text put into the first turn of a session
//! (cross-session context), and what that text shows item by item (HeliconSearch's `bridge.v2`) — so
//! the per-turn recall does not show the same item a second time.
use super::{
    filters::{prefix_utf16, utf16_len},
    util::now_ms,
};
use futures_util::future::{BoxFuture, FutureExt, Shared};
use serde_json::Value;
use std::{cmp::Ordering, collections::HashMap, time::Duration};

/// The SessionBridge protocol read here: items carry versions.
pub(crate) const BRIDGE_PROTOCOL: &str = "bridge.v2";
/// A statement of a recorded conflict is shown up to this many characters.
const CONFLICT_CLAIM_CHARS: usize = 120;

/// An item shown whole: the version shown, and (topic summaries) the coverage version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShownItem {
    pub version: String,
    pub coverage: Option<String>,
}

/// What a bridge text shows: the items shown whole, by key (`memory:<id>`, `topic:<name>`,
/// `session_summary:<memory id>`). A recall block of the same item in the same version is not shown again.
#[derive(Clone, Debug, Default)]
pub(crate) struct BridgeShown {
    pub complete: HashMap<String, ShownItem>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct BridgeRendering {
    pub text: String,
    pub shown: BridgeShown,
}

/// A turn's SessionBridge as it ends up: injected, or omitted (why: off, empty, fetch failed, late).
#[derive(Clone, Debug)]
pub(crate) enum BridgeTurn {
    Injected(BridgeRendering),
    Omitted(String),
}

impl BridgeTurn {
    pub(crate) fn shown(&self) -> Option<&BridgeShown> {
        match self {
            Self::Injected(r) => Some(&r.shown),
            Self::Omitted(_) => None,
        }
    }
}

/// The bridge as it stands at `by_ms`: what it settled to, or omitted ("late") when it has not settled
/// by then. Shared, so the recall's composing and the bridge's injection both take the one answer.
pub(crate) type FrozenBridge = Shared<BoxFuture<'static, BridgeTurn>>;

pub(crate) fn freeze_bridge_by(
    bridge: impl std::future::Future<Output = BridgeTurn> + Send + 'static,
    by_ms: u64,
) -> FrozenBridge {
    async move {
        let wait = Duration::from_millis(by_ms.saturating_sub(now_ms()));
        // (a bridge already settled wins even when the time has passed: it is polled first)
        match tokio::time::timeout(wait, bridge).await {
            Ok(turn) => turn,
            Err(_) => BridgeTurn::Omitted("late".into()),
        }
    }
    .boxed()
    .shared()
}

struct Line {
    line: String,
    /// Shown whole (not cut to the per-item limit).
    whole: bool,
    key: String,
    version: String,
    coverage: Option<String>,
    /// The newest conflicts recorded on it (the server names them per item).
    conflict_ids: Vec<String>,
}

fn text_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn one_line(text: &str, limit: usize) -> String {
    prefix_utf16(text, limit).replace('\n', " ")
}

fn conflict_ids(v: &Value) -> Vec<String> {
    v.get("conflict_ids")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn memory_lines(items: &Value, limit: usize, label: impl Fn(&Value) -> String) -> Vec<Line> {
    items
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let text = text_of(m, "text");
            Line {
                line: format!("- {}{}\n", label(m), one_line(text, limit)),
                whole: utf16_len(text) <= limit,
                key: format!("memory:{}", text_of(m, "id")),
                version: text_of(m, "version").to_owned(),
                coverage: None,
                conflict_ids: conflict_ids(m),
            }
        })
        .collect()
}

fn sum_len(lines: &[Line]) -> i64 {
    lines.iter().map(|l| utf16_len(&l.line) as i64).sum()
}

/// Fills one section; returns `(chars used, the lines shown)`. A title with no item that fits is not
/// emitted.
fn fill<'a>(title: &str, lines: &'a [Line], budget: i64) -> (i64, Vec<&'a Line>) {
    if lines.is_empty() || budget <= 0 {
        return (0, vec![]);
    }
    let mut shown = Vec::new();
    let mut used = utf16_len(title) as i64;
    for line in lines {
        let len = utf16_len(&line.line) as i64;
        if used + len > budget {
            break;
        }
        shown.push(line);
        used += len;
    }
    if shown.is_empty() {
        (0, vec![])
    } else {
        (used, shown)
    }
}

fn said(f: &Value) -> String {
    let said_at = f.get("said_at").and_then(Value::as_str);
    match f
        .get("first_said_at")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(first) => format!("{first}，最近一次 {}", said_at.unwrap_or("未知")),
        None => said_at.unwrap_or("未知").to_owned(),
    }
}

fn said_order(a: &Value, b: &Value) -> Ordering {
    let ts = |f: &Value| f.get("said_ts").and_then(Value::as_f64).unwrap_or(0.0);
    ts(a)
        .partial_cmp(&ts(b))
        .unwrap_or(Ordering::Equal)
        .then_with(|| {
            let at = |f: &Value| {
                f.get("said_at")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned()
            };
            at(a).cmp(&at(b))
        })
}

/// Formats SessionBridge data into one text within a token budget, and says what it shows.
///
/// Content-proportional allocation: when everything fits it is all shown; otherwise each section gets
/// its share of the available characters by how much it would use, and what a section leaves unused
/// rolls over to the next (episodes, then facts or topic summaries, then procedures). Then the
/// conflicts recorded on what is shown: each pair is listed as its two statements in the order they
/// were said, each with when, for the agent to judge (no verdict is passed on). They are not cut by
/// the budget — that would leave the fact standing alone — but are bounded: the newest
/// `conflict_limit`, each statement cut to a line. Empty text when the data has nothing useful.
pub(crate) fn format_bridge_context(bridge: &Value, token_budget: usize) -> BridgeRendering {
    if bridge["is_empty"] == Value::Bool(true) {
        return BridgeRendering::default();
    }
    // 2.0 chars/token: conservative for mixed Chinese/English content
    let char_budget = (token_budget as f64 * 2.0).floor() as i64;

    const MAIN_HEADER: &str = "## 跨会话记忆\n";
    const EP_TITLE: &str = "**近期会话摘要**\n";
    const PIN_TITLE: &str = "**关于你/项目的已知事实**\n";
    const PROC_TITLE: &str = "**当前项目工作流**\n";
    const RELATED_TITLE: &str = "**相关的已存陈述（各附说的时间）**\n";
    let len = |s: &str| utf16_len(s) as i64;

    // ── Phase 1: the formatted lines, each cut to its per-item limit ──
    // A summary made before its session went on says so: the turns after it are not in it.
    let ep_lines: Vec<Line> = bridge["recent_episodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|ep| {
            let info = &ep["session_summary"];
            let later = if info["stale"] == Value::Bool(true) {
                info["later_turns"].as_i64().unwrap_or(0)
            } else {
                0
            };
            let note = if later > 0 {
                format!("（此摘要之后该会话又有 {later} 条消息，未计入）")
            } else {
                String::new()
            };
            let text = text_of(ep, "text");
            Line {
                line: format!("- {}{note}\n", one_line(text, 1200)),
                whole: utf16_len(text) <= 1200,
                key: format!("session_summary:{}", text_of(ep, "id")),
                version: text_of(ep, "version").to_owned(),
                coverage: None,
                conflict_ids: vec![],
            }
        })
        .collect();
    // The topic summaries when the scope has any; until it does, the pinned facts.
    let summaries = bridge["category_summaries"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let pin_lines: Vec<Line> = if !summaries.is_empty() {
        summaries
            .iter()
            .map(|cs| {
                let summary = text_of(cs, "summary");
                let coverage = cs["coverage"]["version"].as_str().map(str::to_owned);
                Line {
                    line: format!(
                        "- [{}] {}\n",
                        text_of(cs, "topic_name"),
                        one_line(summary, 200)
                    ),
                    whole: utf16_len(summary) <= 200,
                    key: format!("topic:{}", text_of(cs, "topic_name")),
                    version: text_of(cs, "version").to_owned(),
                    coverage,
                    conflict_ids: conflict_ids(cs),
                }
            })
            .collect()
    } else {
        memory_lines(&bridge["pinned_semantics"], 300, |m| {
            format!("[{}] ", text_of(m, "category"))
        })
    };
    let proc_lines = memory_lines(&bridge["procedures"], 500, |_| String::new());

    // Desired size = title + all lines (0 for an empty section: no wasted budget).
    let desired = |title: &str, lines: &[Line]| {
        if lines.is_empty() {
            0
        } else {
            len(title) + sum_len(lines)
        }
    };
    let ep_desired = desired(EP_TITLE, &ep_lines);
    let pin_desired = desired(PIN_TITLE, &pin_lines);
    let proc_desired = desired(PROC_TITLE, &proc_lines);
    let content_desired = ep_desired + pin_desired + proc_desired;
    let total_desired = len(MAIN_HEADER) + content_desired;
    if content_desired == 0 {
        return BridgeRendering::default();
    }

    // ── Phase 2: the per-section budget ──
    let available = char_budget - len(MAIN_HEADER);
    let (ep_alloc, pin_alloc, proc_alloc) = if total_desired <= char_budget {
        (ep_desired, pin_desired, proc_desired)
    } else {
        let ep = ((ep_desired as f64 / content_desired as f64) * available as f64).floor() as i64;
        let pin = ((pin_desired as f64 / content_desired as f64) * available as f64).floor() as i64;
        (ep, pin, available - ep - pin)
    };

    // ── Phase 3: fill the sections, cascading what one leaves unused ──
    let (ep_used, ep_shown) = fill(EP_TITLE, &ep_lines, ep_alloc);
    let (pin_used, pin_shown) = fill(PIN_TITLE, &pin_lines, pin_alloc + (ep_alloc - ep_used));
    let (_, proc_shown) = fill(
        PROC_TITLE,
        &proc_lines,
        proc_alloc + (pin_alloc + (ep_alloc - ep_used) - pin_used),
    );

    let mut sections: Vec<String> = Vec::new();
    for (title, shown) in [
        (EP_TITLE, &ep_shown),
        (PIN_TITLE, &pin_shown),
        (PROC_TITLE, &proc_shown),
    ] {
        if !shown.is_empty() {
            sections.push(title.to_owned());
            sections.extend(shown.iter().map(|l| l.line.clone()));
        }
    }
    if sections.is_empty() {
        return BridgeRendering::default();
    }

    let all: Vec<&Line> = ep_shown
        .iter()
        .chain(&pin_shown)
        .chain(&proc_shown)
        .copied()
        .collect();
    // The conflicts recorded on what is shown: of those its items name, the newest (by when recorded,
    // then id — the order the server checks them in), as many as the server says to show.
    let named: std::collections::HashSet<&str> = all
        .iter()
        .flat_map(|l| l.conflict_ids.iter().map(String::as_str))
        .collect();
    let mut conflicts: Vec<&Value> = bridge["conflicts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| named.contains(text_of(c, "relation_id")))
        .collect();
    conflicts.sort_by(|a, b| {
        let at = |c: &Value| c.get("recorded_at").and_then(Value::as_f64).unwrap_or(0.0);
        at(b)
            .partial_cmp(&at(a))
            .unwrap_or(Ordering::Equal)
            .then_with(|| text_of(a, "relation_id").cmp(text_of(b, "relation_id")))
    });
    conflicts.truncate(bridge["conflict_limit"].as_u64().unwrap_or(0) as usize);
    if !conflicts.is_empty() {
        sections.push(RELATED_TITLE.to_owned());
        for c in &conflicts {
            let mut facts: Vec<&Value> = c["facts"].as_array().into_iter().flatten().collect();
            facts.sort_by(|a, b| said_order(a, b));
            let statements: Vec<String> = facts
                .iter()
                .map(|f| {
                    format!(
                        "「{}」（说于 {}）",
                        one_line(text_of(f, "claim"), CONFLICT_CLAIM_CHARS),
                        said(f)
                    )
                })
                .collect();
            sections.push(format!("- {}\n", statements.join("；")));
        }
    }
    let text = format!("{MAIN_HEADER}{}", sections.concat());

    let mut complete = HashMap::new();
    for l in &all {
        if l.whole {
            complete.insert(
                l.key.clone(),
                ShownItem {
                    version: l.version.clone(),
                    coverage: l.coverage.clone(),
                },
            );
        }
    }
    BridgeRendering {
        text,
        shown: BridgeShown { complete },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "protocol": "bridge.v2",
            "is_empty": false,
            "conflict_limit": 3,
            "recent_episodes": [{
                "id": "e1", "text": "talked about trips\nand food", "version": "v1",
                "session_summary": {"session_id": "s1", "generation": 2, "stale": true, "later_turns": 4}
            }],
            "category_summaries": [],
            "pinned_semantics": [{
                "id": "m1", "text": "likes tea", "category": "preference", "version": "mv1",
                "conflict_ids": ["r1"]
            }],
            "procedures": [{"id": "p1", "text": "run the tests", "version": "pv1", "conflict_ids": []}],
            "conflicts": [{
                "relation_id": "r1", "recorded_at": 5,
                "facts": [
                    {"memory_id": "m1", "claim": "likes coffee", "said_at": "2025-02", "said_ts": 2},
                    {"memory_id": "m0", "claim": "likes tea", "said_at": "2025-01", "said_ts": 1}
                ]
            }]
        })
    }

    #[test]
    fn everything_that_fits_is_shown_with_its_notes() {
        let r = format_bridge_context(&sample(), 2000);
        assert!(r.text.starts_with("## 跨会话记忆\n**近期会话摘要**\n- talked about trips and food（此摘要之后该会话又有 4 条消息，未计入）\n"));
        assert!(r
            .text
            .contains("**关于你/项目的已知事实**\n- [preference] likes tea\n"));
        assert!(r.text.contains("**当前项目工作流**\n- run the tests\n"));
        // the two statements, in the order they were said
        assert!(r
            .text
            .contains("- 「likes tea」（说于 2025-01）；「likes coffee」（说于 2025-02）\n"));
        assert!(r.shown.complete.contains_key("memory:m1"));
        assert!(r.shown.complete.contains_key("session_summary:e1"));
    }

    #[test]
    fn a_small_budget_drops_sections_that_do_not_fit_and_their_titles() {
        let r = format_bridge_context(&sample(), 60);
        assert!(!r.text.is_empty());
        assert!(!r.text.contains("**近期会话摘要**"), "{}", r.text);
        assert!(r.text.contains("**关于你/项目的已知事实**"));
    }

    #[test]
    fn an_empty_bridge_is_nothing() {
        assert!(format_bridge_context(&json!({"is_empty": true}), 1000)
            .text
            .is_empty());
        let none = json!({"is_empty": false, "recent_episodes": [], "category_summaries": [],
            "pinned_semantics": [], "procedures": [], "conflicts": [], "conflict_limit": 3});
        assert!(format_bridge_context(&none, 1000).text.is_empty());
    }

    #[test]
    fn topic_summaries_stand_in_for_the_pinned_facts() {
        let mut b = sample();
        b["category_summaries"] = json!([{
            "topic_name": "travel", "summary": "likes trains", "version": "tv1",
            "coverage": {"version": "cv1"}, "conflict_ids": []
        }]);
        let r = format_bridge_context(&b, 2000);
        assert!(r.text.contains("- [travel] likes trains\n"));
        assert!(!r.text.contains("likes tea\n**"));
        assert_eq!(
            r.shown.complete["topic:travel"],
            ShownItem {
                version: "tv1".into(),
                coverage: Some("cv1".into())
            }
        );
    }

    #[tokio::test]
    async fn a_bridge_that_is_late_is_omitted() {
        let slow = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            BridgeTurn::Injected(BridgeRendering::default())
        };
        let frozen = freeze_bridge_by(slow, now_ms() + 20);
        assert!(matches!(frozen.clone().await, BridgeTurn::Omitted(why) if why == "late"));
        let quick = freeze_bridge_by(async { BridgeTurn::Omitted("empty".into()) }, now_ms());
        assert!(matches!(quick.await, BridgeTurn::Omitted(why) if why == "empty"));
    }
}
