//! Auto-recall: before a turn is sent, ask HeliconSearch for the memories relevant to the user's
//! message and return the block to add to that turn.
//!
//! Fail-closed: every error path yields no block, so a turn is never held up by an offline or slow
//! HeliconSearch. Responses are `serde_json::Value`s in the server's "organized" shape.
use super::{
    bridge::{BridgeShown, FrozenBridge},
    budget::{HookBudget, RecallBudgets, MIN_RECALL_BUDGET_MS},
    client::{is_memory_id, pick_addressable_memory_id, CallOptions, HeliconClient},
    config::MemoryConfig,
    filters::{
        is_automated_turn, normalize_recall_query, prefix_utf16, wrap_relevant_memories_block,
    },
    log::Logger,
    util::now_ms,
};
use serde_json::{json, Value};
use std::{
    collections::{hash_map::DefaultHasher, HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

const TAG: &str = "[helicon-memory]";
/// Max ids listed in the MEMORY_IDS index appended to injected context (keeps the block ~5 short lines).
const MEMORY_ID_INDEX_LIMIT: usize = 5;
const MEMORY_ID_INDEX_CLAIM_CHARS: usize = 60;
/// The `prompt_blocks` protocol read here.
pub(crate) const BLOCKS_PROTOCOL: &str = "blocks.v1";
/// Excerpts `memory_recall` adds beside the facts: windows of turns whose facts are not extracted yet.
pub(crate) const UNEXTRACTED_EXCERPTS_MAX: usize = 5;

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn score_of(v: &Value) -> f64 {
    v.get("score").and_then(Value::as_f64).unwrap_or(0.0)
}

/// A `<recalled_skills>` block of the skills HeliconSearch returned, with the instruction to load one
/// with `helicon__skill_get` only. They are remote records, not local files: the tag differs from a
/// host's own skill list so the model does not look for a path, and the server URL is never shown.
fn build_skills_block(skills: &[Value]) -> (String, usize) {
    if skills.is_empty() {
        return (String::new(), 0);
    }
    let mut lines: Vec<String> = vec![
        String::new(),
        "The following skills were recalled by HeliconSearch for the current query.".into(),
        "These are REMOTE records, NOT local files. Do NOT try to read them from disk and do NOT guess a filesystem path: no such file exists.".into(),
        "The ONLY way to load a recalled skill's SKILL.md body is the `helicon__skill_get(skillId)` tool, passing the 32-hex `<skill_id>` verbatim. NEVER call `helicon__memory_get` for a `<skill_id>` — it returns 404; only `helicon__skill_get` accepts skill ids.".into(),
        "Do NOT fetch these skills via `web_fetch` or `curl` — the server URL is private and exposing it would leak the host.".into(),
        "helicon__skill_get returns the SKILL.md markdown body. Parse it and follow the skill's guidance.".into(),
        String::new(),
        "If one clearly applies, call helicon__skill_get with its <skill_id>, then follow the skill.".into(),
        "If several apply, choose the most specific and only fetch that one.".into(),
        "If none clearly apply, fetch none.".into(),
        "One skill up front max — do not speculatively call helicon__skill_get for multiple skills in parallel.".into(),
        "Never invent skill ids — only use the <skill_id> values listed in <recalled_skills> below.".into(),
        String::new(),
        "<recalled_skills>".into(),
    ];
    for s in skills {
        lines.push("  <recalled_skill>".into());
        lines.push(format!(
            "    <skill_id>{}</skill_id>",
            escape_xml(str_of(s, "skill_id"))
        ));
        lines.push(format!(
            "    <name>{}</name>",
            escape_xml(str_of(s, "name"))
        ));
        if let Some(score) = s.get("score").and_then(Value::as_f64) {
            lines.push(format!("    <score>{score:.3}</score>"));
        }
        let desc = str_of(s, "description").trim();
        if !desc.is_empty() {
            lines.push(format!(
                "    <description>{}</description>",
                escape_xml(desc)
            ));
        }
        lines.push("  </recalled_skill>".into());
    }
    lines.push("</recalled_skills>".into());
    (format!("\n\n{}", lines.join("\n")), skills.len())
}

/// A compact id index for the injected block: the addressable memory ids (32 hex) of the strongest
/// facts, so `memory_get` is reachable without a prior `memory_recall`. Worded as a data label, not
/// an instruction (it sits inside the untrusted envelope); the trusted system text says what to do.
/// The (id, claim) rows the index lists: the first few distinct, valid ids.
fn indexed_ids(rows: &[(String, String)]) -> Vec<(String, String)> {
    let mut taken: Vec<(String, String)> = Vec::new();
    let mut seen = HashSet::new();
    for (id, claim) in rows {
        // several facts can derive from one memory: an id takes one slot
        let id = id.trim().to_lowercase();
        if !is_memory_id(&id) || !seen.insert(id.clone()) {
            continue;
        }
        taken.push((id, claim.clone()));
        if taken.len() >= MEMORY_ID_INDEX_LIMIT {
            break;
        }
    }
    taken
}

fn memory_id_index(rows: &[(String, String)]) -> (String, usize) {
    let mut out: Vec<String> = Vec::new();
    for (id, claim) in indexed_ids(rows) {
        let claim = claim.split_whitespace().collect::<Vec<_>>().join(" ");
        out.push(format!(
            "- {id} — {}",
            prefix_utf16(&claim, MEMORY_ID_INDEX_CLAIM_CHARS).trim()
        ));
    }
    if out.is_empty() {
        return (String::new(), 0);
    }
    (
        format!(
            "\n\n## MEMORY_IDS  (memory ids of the {} facts above)\n{}",
            out.len(),
            out.join("\n")
        ),
        out.len(),
    )
}

/// Where each verbatim excerpt of the injected block is (its `[Rn]` label → session + turns): an
/// excerpt has no memory id, and `memory_get` opens it by where it is.
fn raw_ref_index(refs: &[(String, String, Option<String>)]) -> (String, usize) {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (label, session, turns) in refs {
        if label.is_empty() || session.is_empty() || !seen.insert(label.clone()) {
            continue;
        }
        out.push(format!(
            "- {label} — session=\"{session}\"{}",
            turns
                .as_ref()
                .map(|t| format!(", turns=\"{t}\""))
                .unwrap_or_default()
        ));
    }
    if out.is_empty() {
        return (String::new(), 0);
    }
    (
        format!(
            "\n\n## RAW_REFS  (where the {} verbatim excerpt(s) above are)\n{}",
            out.len(),
            out.join("\n")
        ),
        out.len(),
    )
}

pub(crate) struct Excerpt {
    pub session_id: String,
    pub turn: Value,
}

/// The best-scored unextracted raw turns at or above `min_score` (stable: equal scores keep the
/// server's order).
pub(crate) fn pick_unextracted_excerpts(raw: &Value, min_score: f64, max: usize) -> Vec<Excerpt> {
    let mut all: Vec<Excerpt> = Vec::new();
    for s in raw.as_array().into_iter().flatten() {
        for t in s["turns"].as_array().into_iter().flatten() {
            if t["is_orphan"] == Value::Bool(true)
                && score_of(t) >= min_score
                && t["text"].as_str().is_some_and(|x| !x.trim().is_empty())
            {
                all.push(Excerpt {
                    session_id: str_of(s, "session_id").to_owned(),
                    turn: t.clone(),
                });
            }
        }
    }
    all.sort_by(|a, b| {
        score_of(&b.turn)
            .partial_cmp(&score_of(&a.turn))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    all.truncate(max);
    all
}

/// A turn marker as shown: `12` or `"12-14"`.
pub(crate) fn turn_label(turn: &Value) -> Option<String> {
    match turn {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

pub(crate) fn format_unextracted_excerpts(items: &[Excerpt]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = items
        .iter()
        .enumerate()
        .map(|(i, x)| {
            let place = match turn_label(&x.turn["turn"]) {
                Some(t) => format!(" (session=\"{}\", turns=\"{t}\")", x.session_id),
                None => format!(" (session=\"{}\")", x.session_id),
            };
            format!("{}. {}{place}", i + 1, str_of(&x.turn, "text"))
        })
        .collect();
    format!(
        "Verbatim excerpts from the newest turns (not yet extracted into memories, so they carry no id; helicon__memory_get with the session + turns given opens each with the rest of its exchange):\n{}",
        lines.join("\n")
    )
}

/// The key a recall block's item is shown under in the SessionBridge, if it can be shown there.
fn shown_key(r: &Value) -> Option<String> {
    let non_empty = |k: &str| r.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    match r["type"].as_str()? {
        "memory" => non_empty("memory_id").map(|id| format!("memory:{id}")),
        "topic" => non_empty("topic_name").map(|n| format!("topic:{n}")),
        "session_summary" => non_empty("memory_id").map(|id| format!("session_summary:{id}")),
        _ => None,
    }
}

/// An id-only reference to an item shown whole in the SessionBridge already.
fn reference(r: &Value) -> String {
    match r["type"].as_str() {
        Some("topic") => format!(
            "\n• (topic \"{}\": shown above under 跨会话记忆)",
            str_of(r, "topic_name")
        ),
        Some("session_summary") => format!(
            "\n• (summary of {}: shown above under 跨会话记忆)",
            str_of(r, "session_id")
        ),
        Some("memory") => format!(
            "\n• (memory {}: shown above under 跨会话记忆)",
            str_of(r, "memory_id")
        ),
        _ => "\n• (memory : shown above under 跨会话记忆)".to_owned(),
    }
}

pub(crate) struct ComposedRecall {
    /// What to inject (before the MEMORY_IDS index and the skills block).
    pub text: String,
    pub id_index: (String, usize),
    /// Where the verbatim excerpts shown are (RAW_REFS).
    pub raw_index: (String, usize),
    /// Blocks left out: the SessionBridge shows the same item whole, in the same version.
    pub removed: usize,
    /// The same item in the same version, kept: the block shows more (a quote, a time, a note).
    pub kept_more: usize,
    /// The same item in another version, kept.
    pub other_version: usize,
    /// The blocks shown in full this time that have an identity (`memory:<id>`, `topic:<name>`, `legend`), with
    /// the hash of their text: what the session's history holds of the recall once this turn is in it.
    pub injected: Vec<(String, SeenBlock)>,
    /// Blocks left out because the session's history already holds exactly their text.
    pub repeated: usize,
}

/// What a session's history already holds of the recall, block by block: the block's identity, and the hash of the
/// text that was shown for it. (`recallReference`.)
pub(crate) type SeenBlocks = HashMap<String, SeenBlock>;

/// One recall block the conversation holds: the hash of its text, and whether a pointer to it has to carry the start of its
/// text because the memory's id was not listed (MEMORY_IDS lists a few ids only): without it the model could not tell which
/// item an id it never saw stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SeenBlock {
    pub hash: u64,
    pub needs_label: bool,
}

pub(crate) fn text_hash(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// The identity of a recall block, if it has one: the items the SessionBridge can show, and the legend (the
/// same text every time).
fn block_key(r: &Value) -> Option<String> {
    if r["type"] == "legend" {
        return Some("legend".to_owned());
    }
    shown_key(r)
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// The start of a fact's line as its block shows it, what MEMORY_IDS shows of a claim: an item whose id was not
/// listed is found by this.
fn block_snippet(text: &str) -> String {
    let line = text
        .trim_start()
        .trim_start_matches('•')
        .trim_start()
        .lines()
        .next()
        .unwrap_or("");
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    prefix_utf16(&line, MEMORY_ID_INDEX_CLAIM_CHARS)
        .trim()
        .to_owned()
}

/// What names an item in a pointer: the first 8 characters of a memory id (the full ids were listed when the item was
/// first shown), and with `label` the start of its text; a topic's name; a session summary's session.
fn item_name(r: &Value, label: Option<&str>) -> String {
    match r["type"].as_str() {
        Some("topic") => format!("topic \"{}\"", str_of(r, "topic_name")),
        Some("session_summary") => format!("summary of {}", str_of(r, "session_id")),
        _ => match label {
            Some(label) => format!("{} \"{label}\"", short_id(str_of(r, "memory_id"))),
            None => short_id(str_of(r, "memory_id")),
        },
    }
}

/// A pointer to an item whose text the session's history already holds, exactly.
fn seen_pointer(r: &Value, label: Option<&str>) -> String {
    match r["type"].as_str() {
        Some("topic") | Some("session_summary") => format!(
            "\n• ({}: shown earlier in this conversation)",
            item_name(r, None)
        ),
        _ => format!(
            "\n• (memory {}: shown earlier in this conversation)",
            item_name(r, label)
        ),
    }
}

/// The recalled context to inject this turn, from the server's blocks (`prompt_blocks`, protocol
/// `blocks.v1`, joining to exactly `prompt_text`): every block as it is, except one whose item the
/// SessionBridge shows whole in the same version while the block adds nothing to it — that one
/// becomes an id-only reference. MEMORY_IDS then lists the facts left in view.
///
/// `None` when the blocks are missing, of another protocol, or do not make up `prompt_text`: the
/// server sends them with every `prompt_text`, so that is its fault — not passed over by injecting
/// `prompt_text` without them.
#[cfg(test)]
pub(crate) fn compose_recall(
    resp: &Value,
    shown: Option<&BridgeShown>,
    min_score: f64,
) -> Option<ComposedRecall> {
    compose_recall_with(resp, shown, min_score, None)
}

/// `compose_recall`, and with `seen` (`recallReference`) also: a block whose text the session's history already
/// holds, exactly, is not shown again; a pointer stands for it. When nothing of the recall is new, the whole
/// recall is one line saying so.
pub(crate) fn compose_recall_with(
    resp: &Value,
    shown: Option<&BridgeShown>,
    min_score: f64,
    seen: Option<&SeenBlocks>,
) -> Option<ComposedRecall> {
    let prompt_text = str_of(resp, "prompt_text");
    let pb = resp.get("prompt_blocks")?;
    let blocks = pb["blocks"].as_array()?;
    if pb["protocol"] != BLOCKS_PROTOCOL
        || blocks.iter().map(|b| str_of(b, "text")).collect::<String>() != prompt_text
    {
        return None;
    }
    // memory id → (claim, listed): the facts the query found, and every fact of a conflict unit
    let mut facts: HashMap<String, (String, bool)> = HashMap::new();
    for key in ["grounded_facts", "needs_verification"] {
        for f in resp[key].as_array().into_iter().flatten() {
            if let Some(id) = pick_addressable_memory_id(f) {
                facts
                    .entry(id)
                    .or_insert_with(|| (str_of(f, "claim").to_owned(), score_of(f) >= min_score));
            }
        }
    }
    for unit in resp["conflicts"].as_array().into_iter().flatten() {
        for f in unit["facts"].as_array().into_iter().flatten() {
            let id = str_of(f, "memory_id").to_owned();
            let claim = facts
                .get(&id)
                .map(|(c, _)| c.clone())
                .unwrap_or_else(|| str_of(f, "claim").to_owned());
            facts.insert(id, (claim, true));
        }
    }
    let mut out = String::new();
    let mut visible: Vec<(String, String)> = Vec::new();
    let mut raws: Vec<(String, String, Option<String>)> = Vec::new();
    let (mut removed, mut kept_more, mut other_version) = (0, 0, 0);
    // (identity, hash of the text, the memory id of a memory block): the id decides, once the index is known, who needs a label
    let mut injected_raw: Vec<(String, u64, Option<String>)> = Vec::new();
    let mut repeated_names: Vec<String> = Vec::new();
    let mut new_content = 0usize;
    for b in blocks {
        let r = &b["ref"];
        let there = match (shown, shown_key(r)) {
            (Some(s), Some(key)) => s.complete.get(&key),
            _ => None,
        };
        if let Some(there) = there {
            let version = r.get("version").and_then(Value::as_str);
            let same = version == Some(there.version.as_str())
                && (r["type"] != "topic"
                    || there.coverage.as_deref() == r.get("coverage").and_then(Value::as_str));
            let no_extras = r
                .get("extras")
                .and_then(Value::as_array)
                .is_none_or(|e| e.is_empty());
            if same && no_extras {
                out.push_str(&reference(r));
                removed += 1;
                continue;
            }
            if same {
                kept_more += 1;
            } else {
                other_version += 1;
            }
        }
        let own_key = block_key(r);
        let own_hash = text_hash(str_of(b, "text"));
        if let (Some(seen), Some(key)) = (seen, own_key.as_ref()) {
            if let Some(entry) = seen.get(key).filter(|e| e.hash == own_hash) {
                // the same text is in the conversation already: the legend is not said again, an item is pointed to
                if r["type"] != "legend" {
                    let label = entry.needs_label.then(|| block_snippet(str_of(b, "text")));
                    out.push_str(&seen_pointer(r, label.as_deref()));
                    repeated_names.push(item_name(r, label.as_deref()));
                }
                continue;
            }
        }
        if let Some(key) = own_key {
            let memory_id = (r["type"] == "memory")
                .then(|| r["memory_id"].as_str().map(|s| s.trim().to_lowercase()))
                .flatten();
            injected_raw.push((key, own_hash, memory_id));
        }
        if !matches!(r["type"].as_str(), Some("legend") | Some("header")) {
            new_content += 1;
        }
        out.push_str(str_of(b, "text"));
        match r["type"].as_str() {
            Some("memory") => {
                if let Some(id) = r["memory_id"].as_str().filter(|s| !s.is_empty()) {
                    if let Some((claim, true)) = facts.get(id) {
                        visible.push((id.to_owned(), claim.clone()));
                    }
                }
            }
            Some("raw") => {
                if let Some(label) = r["label"].as_str().filter(|s| !s.is_empty()) {
                    raws.push((
                        label.to_owned(),
                        str_of(r, "session_id").to_owned(),
                        turn_label(&r["turns"]),
                    ));
                }
            }
            _ => {}
        }
    }
    // a memory the index does not list is pointed to with the start of its text, from now on
    let indexed: HashSet<String> = indexed_ids(&visible)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let injected: Vec<(String, SeenBlock)> = injected_raw
        .into_iter()
        .map(|(key, hash, memory_id)| {
            let needs_label = memory_id.is_some_and(|id| !indexed.contains(&id));
            (key, SeenBlock { hash, needs_label })
        })
        .collect();
    let repeated = repeated_names.len();
    if repeated > 0 && new_content == 0 {
        // nothing of this recall is new to the session
        return Some(ComposedRecall {
            text: format!(
                "MEMORY RECALL — the memories found for this message ({}) were all shown earlier in this conversation.",
                repeated_names.join(", ")
            ),
            id_index: (String::new(), 0),
            raw_index: (String::new(), 0),
            removed,
            kept_more,
            other_version,
            injected: Vec::new(),
            repeated,
        });
    }
    Some(ComposedRecall {
        text: out.trim().to_owned(),
        id_index: memory_id_index(&visible),
        raw_index: raw_ref_index(&raws),
        removed,
        kept_more,
        other_version,
        injected,
        repeated,
    })
}

/// The injection gate: something at or above `min_score` among the facts and topic summaries, or a
/// block of raw turns not extracted yet, or a session-summary block. Low-scored facts alone are not
/// injected; a qualified session summary alone is.
pub(crate) fn passes_injection_gate(resp: &Value, min_score: f64) -> (bool, String) {
    let top = ["grounded_facts", "topic_rollups"]
        .iter()
        .flat_map(|k| resp[*k].as_array().into_iter().flatten())
        .map(score_of)
        .fold(0.0, f64::max);
    if top >= min_score {
        return (true, format!("topScore {top:.4}"));
    }
    let blocks: Vec<&Value> = resp["prompt_blocks"]["blocks"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    let orphan_raw = blocks
        .iter()
        .filter(|b| b["ref"]["type"] == "raw" && b["ref"]["orphan"] == Value::Bool(true))
        .count();
    let summaries = blocks
        .iter()
        .filter(|b| b["ref"]["type"] == "session_summary")
        .count();
    if orphan_raw > 0 || summaries > 0 {
        return (
            true,
            format!("topScore {top:.4} below minScore; unextracted raw blocks={orphan_raw} session summaries={summaries}"),
        );
    }
    (
        false,
        format!("topScore {top:.4} below minScore {min_score}, no unextracted raw or session summary block"),
    )
}

/// What one turn's automatic recall did, for the turn's summary line.
#[derive(Clone, Debug, Default)]
pub(crate) struct AutoRecallReport {
    /// `injected`, `not_injected` (nothing at or above the gate, or empty), `skipped` (off, automated
    /// turn, short or empty prompt), `no_budget` (too little time left), `failed`, `bad_blocks`.
    pub outcome: &'static str,
}

pub(crate) struct AutoRecallResult {
    pub prepend_context: Option<String>,
    pub report: AutoRecallReport,
    /// The recall blocks put into this turn in full, by identity and text hash (`recallReference`): the session's
    /// history holds them once the turn is in it.
    pub shown: Vec<(String, SeenBlock)>,
}

fn skipped(outcome: &'static str) -> AutoRecallResult {
    AutoRecallResult {
        prepend_context: None,
        report: AutoRecallReport { outcome },
        shown: Vec::new(),
    }
}

pub(crate) struct RecallParams<'a> {
    pub client: &'a HeliconClient,
    pub cfg: &'a MemoryConfig,
    pub prompt: &'a str,
    pub logger: &'a Logger,
    /// This turn's time (made at the start of preparing it).
    pub budget: &'a HookBudget,
    /// Where the server's budgets are kept (updated from the response's `execution`).
    pub budgets: &'a RecallBudgets,
    pub session_key: &'a str,
    /// `heartbeat` / `cron` mean nobody is waiting.
    pub trigger: Option<&'a str>,
    /// What the SessionBridge injected this turn shows, as frozen for this turn: items it shows whole
    /// are not shown again. Asked only when composing; the same answer decides the bridge's injection.
    pub bridge: Option<FrozenBridge>,
    /// `recallReference`: what the session's history already holds of the recall; blocks with the same text
    /// are pointed to, not shown again.
    pub seen: Option<&'a SeenBlocks>,
}

pub(crate) async fn perform_auto_recall(p: RecallParams<'_>) -> AutoRecallResult {
    let started = now_ms();
    let (client, cfg, logger) = (p.client, p.cfg, p.logger);
    logger.info(&format!(
        "{TAG} before_send session={} promptLen={} autoRecall={}",
        p.session_key,
        p.prompt.chars().count(),
        cfg.auto_recall
    ));
    if !cfg.auto_recall {
        return skipped("skipped");
    }
    if is_automated_turn(p.trigger) {
        logger.info(&format!(
            "{TAG} before_send -> skipped (trigger={}, not a user turn)",
            p.trigger.unwrap_or("")
        ));
        return skipped("skipped");
    }
    if p.prompt.chars().count() < 5 {
        logger.info(&format!("{TAG} before_send -> skipped (prompt too short)"));
        return skipped("skipped");
    }
    let query = normalize_recall_query(p.prompt, cfg.recall_max_chars);
    if query.is_empty() {
        logger.info(&format!(
            "{TAG} before_send -> empty query, skipping recall"
        ));
        return skipped("skipped");
    }
    // what the server may take: the turn's deadline less the transport margin (it only shortens its
    // own budget to this); too little left and the recall could not end in time: not sent
    let budget_ms = p.budget.recall_budget_ms();
    if budget_ms < MIN_RECALL_BUDGET_MS {
        logger.warn(&format!(
            "{TAG} auto_recall not sent: {budget_ms}ms left for it (hook timeout {}ms, automatic budget {}ms from {})",
            p.budget.hook_timeout_ms,
            p.budget.budgets.auto_ms,
            p.budget.budgets.source.name()
        ));
        return skipped("no_budget");
    }
    logger.info(&format!(
        "{TAG} auto_recall querying memories for: \"{query}\" agent={} topK={} budgetMs={budget_ms}",
        cfg.agent_id, cfg.recall_top_k
    ));
    let bytes = Arc::new(AtomicUsize::new(0));
    let request = json!({
        "query": query,
        "agent_id": cfg.agent_id,
        "project_id": cfg.project_id,
        // the automatic per-turn recall: the server runs its speed tier within budget_ms
        "purpose": "auto",
        "budget_ms": budget_ms,
        // turns on the server-side RecallGate; only this call site sets it, so explicit
        // memory_recall tool calls always retrieve
        "recall_gate": cfg.recall_gate,
        "top_k": cfg.recall_top_k,
        "render": "full",
    });
    let opts = CallOptions {
        deadline_ms: Some(p.budget.deadline_ms),
        bytes: Some(bytes.clone()),
    };
    let resp = match client.query(request, &opts).await {
        Ok(r) => r,
        Err(err) => {
            logger.warn(&format!(
                "{TAG} before_send -> recall FAILED ({}) elapsedMs={} budgetMs={budget_ms} err={err}",
                err.kind(),
                now_ms().saturating_sub(started)
            ));
            return skipped("failed");
        }
    };
    p.budgets.learn_from_response(resp.get("execution"));
    let server = match resp.get("execution").filter(|e| e.is_object()) {
        Some(ex) => format!(
            "{}/{}/{}",
            ex["outcome"].as_str().unwrap_or("?"),
            ex["tier"].as_str().unwrap_or("?"),
            ex["retrieval_mode"].as_str().unwrap_or("?")
        ),
        None => "none".to_owned(),
    };
    let grounded = resp["grounded_facts"].as_array().map_or(0, Vec::len);
    let raw_turns: usize = resp["raw_context"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| s["turns"].as_array().map_or(0, Vec::len))
        .sum();
    let prompt_text = str_of(&resp, "prompt_text").trim();
    let elapsed = now_ms().saturating_sub(started);
    let blocks_usable = resp["prompt_blocks"]["protocol"] == BLOCKS_PROTOCOL
        && resp["prompt_blocks"]["blocks"].is_array();
    let (pass, why) = passes_injection_gate(&resp, cfg.recall_min_score);
    logger.info(&format!(
        "{TAG} auto_recall grounded={grounded} rawTurns={raw_turns} minScore={} promptTextLen={} server={server} bytes={} gate={} ({why})",
        cfg.recall_min_score,
        prompt_text.chars().count(),
        bytes.load(Ordering::Relaxed),
        if pass { "pass" } else { "fail" }
    ));
    // the gate: no rendered content, or (blocks usable) nothing at or above minScore and no
    // unextracted raw or session summary block: not injected (unusable blocks fail composing below)
    if prompt_text.is_empty() || (blocks_usable && !pass) {
        logger.info(&format!(
            "{TAG} auto_recall no memories to inject ({}) elapsedMs={elapsed}",
            if prompt_text.is_empty() {
                "empty prompt_text"
            } else {
                &why
            }
        ));
        return skipped("not_injected");
    }
    let frozen = match &p.bridge {
        Some(b) => b.clone().await,
        None => super::bridge::BridgeTurn::Omitted("not asked".into()),
    };
    let Some(composed) = compose_recall_with(&resp, frozen.shown(), cfg.recall_min_score, p.seen)
    else {
        logger.warn(&format!(
            "{TAG} auto_recall not injected: prompt_text came without usable prompt_blocks (protocol={}, expected {BLOCKS_PROTOCOL}) elapsedMs={elapsed}",
            resp["prompt_blocks"]["protocol"].as_str().unwrap_or("none")
        ));
        return skipped("bad_blocks");
    };
    let related: Vec<Value> = resp["related_skills"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let (skills_text, skills_count) = build_skills_block(&related);
    let prepend = wrap_relevant_memories_block(&format!(
        "{}{}{}{skills_text}",
        composed.text, composed.id_index.0, composed.raw_index.0
    ));
    logger.info(&format!(
        "{TAG} auto_recall injected organized memory (grounded={grounded} rawTurns={raw_turns} idIndex={} rawRefs={} skillsBlock={skills_count} shownInBridge={} keptMore={} otherVersion={} repeated={}) elapsedMs={elapsed}",
        composed.id_index.1, composed.raw_index.1, composed.removed, composed.kept_more, composed.other_version, composed.repeated
    ));
    // the injected block is otherwise invisible: without this "the model said it has no such memory"
    // cannot be told apart from "the memory was never injected"
    logger.debug(&format!(
        "{TAG} auto_recall injected block: {}",
        prepend.replace('\n', "↵")
    ));
    AutoRecallResult {
        prepend_context: Some(prepend),
        report: AutoRecallReport {
            outcome: "injected",
        },
        shown: composed.injected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helicon::bridge::ShownItem;

    const ID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn blocks(list: Vec<Value>) -> Value {
        let text: String = list.iter().map(|b| str_of(b, "text").to_owned()).collect();
        json!({"prompt_text": text, "prompt_blocks": {"protocol": "blocks.v1", "blocks": list}})
    }

    fn block(text: &str, r: Value) -> Value {
        json!({"text": text, "ref": r})
    }

    #[test]
    fn blocks_that_do_not_make_up_the_prompt_are_refused() {
        let mut resp = blocks(vec![block("a", json!({"type": "legend"}))]);
        resp["prompt_text"] = json!("different");
        assert!(compose_recall(&resp, None, 0.01).is_none());
        let mut resp = blocks(vec![block("a", json!({"type": "legend"}))]);
        resp["prompt_blocks"]["protocol"] = json!("blocks.v9");
        assert!(compose_recall(&resp, None, 0.01).is_none());
        assert!(compose_recall(&json!({"prompt_text": "x"}), None, 0.01).is_none());
    }

    #[test]
    fn the_memory_index_lists_facts_in_view_and_the_raw_index_where_excerpts_are() {
        let mut resp = blocks(vec![
            block("FACTS\n", json!({"type": "header"})),
            block(
                "- likes tea\n",
                json!({"type": "memory", "memory_id": ID_A, "version": "v1", "extras": []}),
            ),
            block(
                "- low score\n",
                json!({"type": "memory", "memory_id": ID_B, "version": "v1", "extras": []}),
            ),
            block(
                "[R1] said hi\n",
                json!({"type": "raw", "session_id": "openworker/s1", "turns": "3-4", "orphan": false, "label": "R1"}),
            ),
        ]);
        resp["grounded_facts"] = json!([
            {"id": "pkg:1", "claim": "likes tea", "score": 0.9, "source_memory_ids": [ID_A]},
            {"id": "pkg:2", "claim": "low score", "score": 0.001, "source_memory_ids": [ID_B]},
        ]);
        let c = compose_recall(&resp, None, 0.01).unwrap();
        assert_eq!(c.removed, 0);
        assert_eq!(c.id_index.1, 1);
        assert!(c.id_index.0.contains(&format!("- {ID_A} — likes tea")));
        assert!(!c.id_index.0.contains(ID_B));
        assert_eq!(c.raw_index.1, 1);
        assert!(c
            .raw_index
            .0
            .contains("- R1 — session=\"openworker/s1\", turns=\"3-4\""));
    }

    #[test]
    fn an_item_the_bridge_shows_whole_in_the_same_version_becomes_a_reference() {
        let resp = blocks(vec![
            block(
                "- likes tea\n",
                json!({"type": "memory", "memory_id": ID_A, "version": "v1", "extras": []}),
            ),
            block(
                "- likes tea (said 2025)\n",
                json!({"type": "memory", "memory_id": ID_B, "version": "v1", "extras": ["time"]}),
            ),
            block(
                "- other\n",
                json!({"type": "memory", "memory_id": "cccccccccccccccccccccccccccccccc", "version": "v2", "extras": []}),
            ),
        ]);
        let mut shown = BridgeShown::default();
        for (id, version) in [
            (ID_A, "v1"),
            (ID_B, "v1"),
            ("cccccccccccccccccccccccccccccccc", "v1"),
        ] {
            shown.complete.insert(
                format!("memory:{id}"),
                ShownItem {
                    version: version.into(),
                    coverage: None,
                },
            );
        }
        let c = compose_recall(&resp, Some(&shown), 0.01).unwrap();
        assert_eq!((c.removed, c.kept_more, c.other_version), (1, 1, 1));
        assert!(c
            .text
            .contains(&format!("(memory {ID_A}: shown above under 跨会话记忆)")));
        assert!(c.text.contains("- likes tea (said 2025)"));
        assert!(c.text.contains("- other"));
    }

    fn memory_block(text: &str, id: &str) -> Value {
        block(
            text,
            json!({"type": "memory", "memory_id": id, "version": "v1", "extras": []}),
        )
    }

    /// A recall of the legend, a header and two memories (the first one with the given text), and more blocks.
    fn recall_of(tea: &str, more: Vec<Value>) -> Value {
        let mut list = vec![
            block("MEMORY RECALL legend\n", json!({"type": "legend"})),
            block(
                "\n\n## FACTS\n",
                json!({"type": "header", "section": "FACTS"}),
            ),
            memory_block(tea, ID_A),
            memory_block("\n• likes coffee\n", ID_B),
        ];
        list.extend(more);
        // (the two memories have facts, so the index lists their ids)
        let mut resp = blocks(list);
        resp["grounded_facts"] = json!([
            {"id": "pkg:a", "claim": "likes tea", "score": 0.9, "source_memory_ids": [ID_A]},
            {"id": "pkg:b", "claim": "likes coffee", "score": 0.9, "source_memory_ids": [ID_B]},
        ]);
        resp
    }

    #[test]
    fn a_block_whose_text_the_conversation_holds_is_pointed_to_and_only_new_text_is_shown_in_full()
    {
        let first =
            compose_recall_with(&recall_of("\n• likes tea\n", vec![]), None, 0.01, None).unwrap();
        assert_eq!(first.repeated, 0);
        let keys: Vec<String> = first.injected.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(
            keys,
            vec![
                "legend".to_owned(),
                format!("memory:{ID_A}"),
                format!("memory:{ID_B}")
            ],
            "the legend and the two memories have an identity, the header has none"
        );
        let seen: SeenBlocks = first.injected.iter().cloned().collect();

        // the same recall again: nothing of it is new, one line says so
        let again = compose_recall_with(
            &recall_of("\n• likes tea\n", vec![]),
            None,
            0.01,
            Some(&seen),
        )
        .unwrap();
        assert_eq!(again.repeated, 2);
        assert_eq!(
            again.text,
            format!(
                "MEMORY RECALL — the memories found for this message ({}, {}) were all shown earlier in this conversation.",
                &ID_A[..8],
                &ID_B[..8]
            )
        );
        assert!(again.injected.is_empty());
        assert_eq!((again.id_index.1, again.raw_index.1), (0, 0));

        // a changed text and a new item are shown in full, the unchanged one is pointed to, the legend is not said again
        let newer = recall_of(
            "\n• likes tea (said 2025)\n",
            vec![memory_block(
                "\n• new fact\n",
                "cccccccccccccccccccccccccccccccc",
            )],
        );
        let c = compose_recall_with(&newer, None, 0.01, Some(&seen)).unwrap();
        assert_eq!(c.repeated, 1);
        assert!(
            c.text.contains("• likes tea (said 2025)") && c.text.contains("• new fact"),
            "{}",
            c.text
        );
        assert!(
            c.text.contains(&format!(
                "(memory {}: shown earlier in this conversation)",
                &ID_B[..8]
            )),
            "{}",
            c.text
        );
        assert!(
            !c.text.contains("MEMORY RECALL legend") && c.text.contains("## FACTS"),
            "{}",
            c.text
        );
        assert_eq!(
            c.injected.len(),
            2,
            "the changed item and the new one: {:?}",
            c.injected
        );

        // an excerpt of the conversation is something new: not one line, the memories it sits with are pointed to
        let with_raw = recall_of(
            "\n• likes tea\n",
            vec![block(
                "\n[R1] said hi\n",
                json!({"type": "raw", "session_id": "openworker/s1", "turns": "3-4", "orphan": false, "label": "R1"}),
            )],
        );
        let c = compose_recall_with(&with_raw, None, 0.01, Some(&seen)).unwrap();
        assert_eq!(c.repeated, 2);
        assert!(
            c.text.contains("[R1] said hi")
                && c.text.contains("shown earlier in this conversation"),
            "{}",
            c.text
        );
    }

    /// A recall of `n` memories, each with a fact so that its id can be listed in MEMORY_IDS (the ids differ in their first 8 characters).
    fn many_memories(n: usize, score: f64) -> Value {
        let id = |i: usize| format!("{:08x}{}", i + 1, "0".repeat(24));
        let claim = |i: usize| {
            format!(
                "CFO, Kestrel Labs · item{i}.key: {}",
                "long text ".repeat(12)
            )
        };
        let list: Vec<Value> = (0..n)
            .map(|i| memory_block(&format!("\n• {}\n", claim(i)), &id(i)))
            .collect();
        let mut resp = blocks(list);
        resp["grounded_facts"] = json!((0..n)
            .map(|i| json!({"id": format!("pkg:{i}"), "claim": claim(i), "score": score, "source_memory_ids": [id(i)]}))
            .collect::<Vec<_>>());
        resp
    }

    #[test]
    fn a_memory_the_index_did_not_list_is_pointed_to_with_the_start_of_its_text() {
        let resp = many_memories(7, 0.9);
        let first = compose_recall_with(&resp, None, 0.01, None).unwrap();
        // the index lists 5 ids: the 6th and the 7th memory have no id in the text the conversation holds
        assert_eq!(first.id_index.1, MEMORY_ID_INDEX_LIMIT);
        let needs: Vec<bool> = first.injected.iter().map(|(_, s)| s.needs_label).collect();
        assert_eq!(needs, vec![false, false, false, false, false, true, true]);
        let seen: SeenBlocks = first.injected.iter().cloned().collect();

        // all of it again: one line; the listed memories by their ids, the others with the start of their text too
        let again = compose_recall_with(&resp, None, 0.01, Some(&seen)).unwrap();
        assert_eq!(again.repeated, 7);
        let snippet = block_snippet(&format!(
            "\n• CFO, Kestrel Labs · item5.key: {}\n",
            "long text ".repeat(12)
        ));
        assert!(
            snippet.chars().count() <= 60 && snippet.starts_with("CFO, Kestrel Labs · item5.key:"),
            "{snippet}"
        );
        assert!(
            again.text.contains(&format!("00000006 \"{snippet}\"")),
            "{}",
            again.text
        );
        assert!(
            again.text.contains("00000001,") && !again.text.contains("00000001 \""),
            "{}",
            again.text
        );

        // with something new beside them: a pointer per memory, and the label where it is needed
        let mut next = many_memories(8, 0.9);
        next["grounded_facts"].as_array_mut().unwrap().truncate(8);
        let c = compose_recall_with(&next, None, 0.01, Some(&seen)).unwrap();
        assert_eq!(c.repeated, 7);
        assert!(
            c.text.contains(&format!(
                "• (memory 00000007 \"{}\": shown earlier in this conversation)",
                block_snippet(&format!(
                    "\n• CFO, Kestrel Labs · item6.key: {}\n",
                    "long text ".repeat(12)
                ))
            )),
            "{}",
            c.text
        );
        assert!(
            c.text
                .contains("• (memory 00000001: shown earlier in this conversation)"),
            "{}",
            c.text
        );
    }

    #[test]
    fn a_memory_below_the_score_the_index_needs_is_labelled_too() {
        // a fact under min_score is not listed in MEMORY_IDS, so its id was never in the text
        let resp = many_memories(2, 0.001);
        let first = compose_recall_with(&resp, None, 0.01, None).unwrap();
        assert_eq!(first.id_index.1, 0);
        assert!(first.injected.iter().all(|(_, s)| s.needs_label));
    }

    #[test]
    fn the_snippet_is_the_first_line_of_the_fact_without_its_bullet_and_cut_at_the_index_width() {
        assert_eq!(
            block_snippet("\n•  a   b\n    ⤷ \"quote\" — operator\n"),
            "a b"
        );
        let long = format!("\n• {}\n", "word ".repeat(40));
        assert!(block_snippet(&long).chars().count() <= MEMORY_ID_INDEX_CLAIM_CHARS);
    }

    #[test]
    fn without_a_record_of_the_conversation_nothing_is_pointed_to() {
        let c = compose_recall_with(
            &recall_of("\n• likes tea\n", vec![]),
            None,
            0.01,
            Some(&SeenBlocks::new()),
        )
        .unwrap();
        assert_eq!(c.repeated, 0);
        assert!(c.text.contains("• likes tea") && c.text.contains("MEMORY RECALL legend"));
        assert!(!c.text.contains("shown earlier"));
    }

    #[test]
    fn the_gate_wants_a_score_an_unextracted_window_or_a_session_summary() {
        let low = json!({"grounded_facts": [{"score": 0.001}], "topic_rollups": []});
        assert!(!passes_injection_gate(&low, 0.01).0);
        let high = json!({"grounded_facts": [{"score": 0.5}]});
        assert!(passes_injection_gate(&high, 0.01).0);
        let mut raw = low.clone();
        raw["prompt_blocks"] = json!({"blocks": [{"ref": {"type": "raw", "orphan": true}}]});
        assert!(passes_injection_gate(&raw, 0.01).0);
        let mut summary = low.clone();
        summary["prompt_blocks"] = json!({"blocks": [{"ref": {"type": "session_summary"}}]});
        assert!(passes_injection_gate(&summary, 0.01).0);
    }

    #[test]
    fn unextracted_excerpts_are_best_first_and_bounded() {
        let raw = json!([{"session_id": "s", "turns": [
            {"text": "a", "score": 0.2, "is_orphan": true, "turn": 1},
            {"text": "b", "score": 0.9, "is_orphan": true, "turn": "2-3"},
            {"text": "c", "score": 0.5, "is_orphan": false, "turn": 4},
            {"text": "  ", "score": 0.9, "is_orphan": true, "turn": 5},
            {"text": "d", "score": 0.001, "is_orphan": true, "turn": 6},
        ]}]);
        let picked = pick_unextracted_excerpts(&raw, 0.01, 5);
        assert_eq!(picked.len(), 2);
        assert_eq!(str_of(&picked[0].turn, "text"), "b");
        let text = format_unextracted_excerpts(&picked);
        assert!(text.contains("1. b (session=\"s\", turns=\"2-3\")"));
        assert!(text.contains("2. a (session=\"s\", turns=\"1\")"));
        assert_eq!(pick_unextracted_excerpts(&raw, 0.01, 1).len(), 1);
    }

    #[test]
    fn skills_are_listed_with_escaping_and_never_with_their_location() {
        let (text, n) = build_skills_block(&[json!({
            "skill_id": ID_A, "name": "a<b", "score": 0.5, "description": " does \"x\" ",
            "location": "http://secret-host/skill"
        })]);
        assert_eq!(n, 1);
        assert!(text.contains("<name>a&lt;b</name>"));
        assert!(text.contains("<score>0.500</score>"));
        assert!(text.contains("<description>does &quot;x&quot;</description>"));
        assert!(!text.contains("secret-host"));
        assert_eq!(build_skills_block(&[]), (String::new(), 0));
    }
}
