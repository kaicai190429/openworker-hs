//! The agent-callable memory tools: `memory_recall`, `memory_get`, `memory_forget`, optionally
//! `memory_store`, and `skill_get`. They are registered through [`crate::Harness::register_tools`], so
//! the model calls them like any application tool (as `helicon__<name>`).
//!
//! Every tool answers with text for the model: a failure is a text too (never an error of the call),
//! so the model can say what went wrong instead of the turn failing.
use super::{
    budget::RecallBudgets,
    client::{is_memory_id, pick_addressable_memory_id, HeliconClient, HsError},
    config::{MemoryConfig, MEMORY_CATEGORIES},
    filters::prefix_utf16,
    guidance::{MAX_OPENS_PER_TURN, MAX_SEARCHES_PER_TURN},
    log::Logger,
    memory_context::{
        format_page, parse_turns, source_sessions, source_turns, window_around, Marked, PageMode,
        DEFAULT_CONTEXT_WINDOW, MAX_CONTEXT_WINDOW, SESSION_PAGE_SIZE,
    },
    recall::{format_unextracted_excerpts, pick_unextracted_excerpts, turn_label},
    source_ref::make_manual_ref,
    util::{cut_bytes, now_ms, BoundedMap},
};
use crate::Tool;
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};

const TAG: &str = "[helicon-memory]";

#[derive(Clone, Copy)]
enum Use {
    Searches,
    Opens,
}

#[derive(Default, Clone, Copy)]
struct Counts {
    searches: u32,
    opens: u32,
}

pub(crate) struct Tools {
    cfg: Arc<MemoryConfig>,
    client: HeliconClient,
    logger: Logger,
    budgets: RecallBudgets,
    /// Searches and opens made for one user message, by `session#turn`: a new message starts afresh.
    uses: Mutex<BoundedMap<String, Counts>>,
}

/// The memory id a model passed, or why it cannot be one: HeliconSearch's ids are 32 hex characters.
/// A model easily takes the per-query package id (`pkg:<hex>`) for one; that reference is generated
/// per query and not stored, so a lookup would only be a 404 — stopped before the request, with the
/// way forward.
fn normalize_memory_id(raw: &str) -> Result<String, String> {
    // ids are stored lower-case; a model may restate them in another case
    let id = raw.trim().to_lowercase();
    if id.is_empty() {
        return Err("memoryId is required.".into());
    }
    if id.starts_with("pkg:") {
        return Err(format!(
            "\"{id}\" is a per-query package id, not a memory id — it cannot be looked up. Call memory_recall and use the 32-hex id it reports for that fact."
        ));
    }
    if !is_memory_id(&id) {
        return Err(format!(
            "\"{id}\" is not a valid memory id (expected 32 hex characters, e.g. from memory_recall). Call memory_recall to obtain a valid id."
        ));
    }
    Ok(id)
}

fn arg_str(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned()
}

fn arg_num(args: &Value, key: &str) -> Option<f64> {
    args.get(key)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
}

fn pct(score: f64) -> String {
    format!("{:.0}", score * 100.0)
}

/// A JSON number as JavaScript writes it (`1` for `1.0`).
fn js_number(v: &Value) -> String {
    match v.as_f64() {
        Some(n) if n.fract() == 0.0 && n.abs() < 1e15 => format!("{}", n as i64),
        Some(n) => n.to_string(),
        None => v.as_str().unwrap_or("?").to_owned(),
    }
}

fn score_of(v: &Value) -> f64 {
    v.get("score").and_then(Value::as_f64).unwrap_or(0.0)
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

impl Tools {
    pub(crate) fn new(
        cfg: Arc<MemoryConfig>,
        client: HeliconClient,
        logger: Logger,
        budgets: RecallBudgets,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            client,
            logger,
            budgets,
            uses: Mutex::new(BoundedMap::new(512)),
        })
    }

    /// Takes one of this user message's searches / opens: `true` when one was left (and it is now
    /// used), `false` when the limit is reached.
    fn take(&self, key: &str, what: Use) -> bool {
        let mut uses = self.uses.lock().unwrap();
        let mut c = uses.get(&key.to_owned()).copied().unwrap_or_default();
        let (n, max) = match what {
            Use::Searches => (&mut c.searches, MAX_SEARCHES_PER_TURN),
            Use::Opens => (&mut c.opens, MAX_OPENS_PER_TURN),
        };
        if *n >= max {
            return false;
        }
        *n += 1;
        uses.insert(key.to_owned(), c);
        true
    }

    /// Returns one that `take` took, for a search that never got an answer (the server was not
    /// reachable): it must not count against the message, nor make the agent tell the user "no record".
    fn give_back(&self, key: &str, what: Use) {
        let mut uses = self.uses.lock().unwrap();
        let mut c = uses.get(&key.to_owned()).copied().unwrap_or_default();
        let n = match what {
            Use::Searches => &mut c.searches,
            Use::Opens => &mut c.opens,
        };
        *n = n.saturating_sub(1);
        uses.insert(key.to_owned(), c);
    }

    fn used(&self, key: &str, what: Use) -> u32 {
        let uses = self.uses.lock().unwrap();
        let c = uses.get(&key.to_owned()).copied().unwrap_or_default();
        match what {
            Use::Searches => c.searches,
            Use::Opens => c.opens,
        }
    }

    // ── memory_recall ───────────────────────────────────────────────────────

    pub(crate) async fn memory_recall(&self, args: &Value, turn_key: &str) -> String {
        let cfg = &self.cfg;
        let query = arg_str(args, "query");
        let limit = arg_num(args, "limit")
            .map(|n| n.clamp(1.0, 20.0).floor() as i64)
            .unwrap_or(cfg.top_k as i64);
        let started = now_ms();
        self.logger.info(&format!(
            "{TAG} tool=memory_recall query=\"{}\" limit={limit} agent={}",
            prefix_utf16(&query, 80),
            cfg.agent_id
        ));
        if query.is_empty() {
            return "memory_recall: query is required".into();
        }
        if !self.take(turn_key, Use::Searches) {
            let made = self.used(turn_key, Use::Searches);
            self.logger.info(&format!(
                "{TAG} tool=memory_recall -> not searched: {made} searches already for this message (session={turn_key})"
            ));
            return format!(
                "memory_recall: not searched — memory was already searched {made} times for this message, besides the memories shown with it. Answer with what those gave you; if they did not have it, tell the user you have no record of it."
            );
        }
        // an explicit search: only the structure is wanted (no rendering); the facts are the derived,
        // traceable ones, shown at or above recallMinScore
        let request = json!({
            "query": query,
            "agent_id": cfg.agent_id,
            "project_id": cfg.project_id,
            "purpose": "active",
            "top_k": limit,
            "render": "none",
        });
        let opts = super::client::CallOptions::deadline(self.budgets.active_deadline_ms());
        let resp = match self.client.query(request, &opts).await {
            Ok(r) => r,
            Err(err) => {
                self.give_back(turn_key, Use::Searches);
                return self.failed("memory_recall", &err);
            }
        };
        self.budgets.learn_from_response(resp.get("execution"));
        let min = cfg.recall_min_score;
        let grounded = resp["grounded_facts"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        // Facts the server has not verified yet (`needs_verification`: a fact just extracted can sit there on
        // some builds) are facts too: listed after the grounded ones, and said to be unverified.
        let unverified = resp["needs_verification"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let filtered: Vec<(&Value, &str)> = grounded
            .iter()
            .map(|f| (f, s(f, "support_level")))
            .chain(unverified.iter().map(|f| (f, "needs_verification")))
            .filter(|(f, _)| score_of(f) >= min)
            .collect();
        let ids: Vec<String> = filtered
            .iter()
            .map(|(f, _)| pick_addressable_memory_id(f).unwrap_or_else(|| "-".into()))
            .collect();
        self.logger.info(&format!(
            "{TAG} tool=memory_recall -> memories={} (grounded={}, needsVerification={}) elapsedMs={} ids=[{}]",
            filtered.len(),
            grounded.len(),
            unverified.len(),
            now_ms().saturating_sub(started),
            ids.join(",")
        ));
        if filtered.is_empty() {
            // no grounded fact is not "no relevant memory": topic summaries and verbatim turns are the
            // other two bands of trust; they carry no memory id but can answer the question
            let limit = limit as usize;
            let rollups: Vec<&Value> = resp["topic_rollups"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|r| score_of(r) >= min)
                .take(limit)
                .collect();
            let raw_turns: Vec<(&str, &Value)> = resp["raw_context"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|sess| {
                    sess["turns"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(move |t| (s(sess, "session_id"), t))
                })
                .filter(|(_, t)| score_of(t) >= min)
                .take(limit)
                .collect();
            if rollups.is_empty() && raw_turns.is_empty() {
                return "No relevant memories found.".into();
            }
            let mut sections: Vec<String> = Vec::new();
            if !rollups.is_empty() {
                let lines: Vec<String> = rollups
                    .iter()
                    .enumerate()
                    .map(|(i, r)| {
                        let topic = s(r, "topic");
                        format!(
                            "{}. {}{} ({}%)",
                            i + 1,
                            if topic.is_empty() {
                                String::new()
                            } else {
                                format!("[{topic}] ")
                            },
                            s(r, "summary"),
                            pct(score_of(r))
                        )
                    })
                    .collect();
                sections.push(format!(
                    "Topic summaries (aggregated — no memory id, cannot be expanded):\n{}",
                    lines.join("\n")
                ));
            }
            if !raw_turns.is_empty() {
                // no memory id: memory_get opens each by where it is (session + turns)
                let lines: Vec<String> = raw_turns
                    .iter()
                    .enumerate()
                    .map(|(i, (session, t))| {
                        let speaker = s(t, "speaker");
                        let place = match turn_label(&t["turn"]) {
                            Some(turn) => format!(" (session=\"{session}\", turns=\"{turn}\")"),
                            None => format!(" (session=\"{session}\")"),
                        };
                        format!(
                            "{}. {}{}{place}",
                            i + 1,
                            if speaker.is_empty() {
                                String::new()
                            } else {
                                format!("{speaker}: ")
                            },
                            prefix_utf16(s(t, "text"), 300)
                        )
                    })
                    .collect();
                sections.push(format!(
                    "Verbatim excerpts (memory_get with the session + turns given opens each with the rest of its exchange):\n{}",
                    lines.join("\n")
                ));
            }
            self.logger.info(&format!(
                "{TAG} tool=memory_recall -> no grounded facts, fell back to rollups={} rawTurns={}",
                rollups.len(),
                raw_turns.len()
            ));
            return format!(
                "No directly attributable memory, but related context exists:\n\n{}",
                sections.join("\n\n")
            );
        }
        let lines: Vec<String> = filtered
            .iter()
            .enumerate()
            .map(|(idx, (f, label))| {
                let pct = pct(score_of(f));
                // the id must be whole: memory_get matches exactly, a cut one is a 404
                match pick_addressable_memory_id(f) {
                    Some(id) => {
                        format!("{}. [{label}] {} ({pct}% id={id})", idx + 1, s(f, "claim"))
                    }
                    None => format!(
                        "{}. [{label}] {} ({pct}%, no id — cannot be expanded)",
                        idx + 1,
                        s(f, "claim")
                    ),
                }
            })
            .collect();
        // the newest turns, not extracted yet, sit in raw_context beside the facts: show the best of them too
        let excerpts = pick_unextracted_excerpts(
            &resp["raw_context"],
            min,
            super::recall::UNEXTRACTED_EXCERPTS_MAX,
        );
        let excerpt_text = format_unextracted_excerpts(&excerpts);
        if !excerpts.is_empty() {
            self.logger.info(&format!(
                "{TAG} tool=memory_recall -> plus {} unextracted excerpt(s)",
                excerpts.len()
            ));
        }
        format!(
            "Found {} memories:\n\n{}{}",
            filtered.len(),
            lines.join("\n"),
            if excerpt_text.is_empty() {
                String::new()
            } else {
                format!("\n\n{excerpt_text}")
            }
        )
    }

    fn failed(&self, tool: &str, err: &HsError) -> String {
        let detail = err.describe();
        self.logger
            .warn(&format!("{TAG} tool={tool} FAILED {detail}"));
        format!("{tool} failed: {detail}")
    }

    // ── memory_store ────────────────────────────────────────────────────────

    pub(crate) async fn memory_store(&self, args: &Value) -> String {
        let cfg = &self.cfg;
        let text = arg_str(args, "text");
        let category = args
            .get("category")
            .and_then(Value::as_str)
            .filter(|c| MEMORY_CATEGORIES.contains(c))
            .unwrap_or("other");
        let importance = arg_num(args, "importance").map_or(0.7, |n| n.clamp(0.0, 1.0));
        self.logger.info(&format!(
            "{TAG} tool=memory_store text=\"{}\" category={category} importance={importance} agent={}",
            prefix_utf16(&text, 80),
            cfg.agent_id
        ));
        if text.is_empty() {
            return "memory_store: text is required".into();
        }
        let request = json!({
            "text": text,
            "agent_id": cfg.agent_id,
            "project_id": cfg.project_id,
            "category": category,
            "importance": importance,
            "scope": "project",
            "extraction_method": "agent_tool",
            "confidence": 0.9,
            "source_refs": [make_manual_ref("agent_tool")],
        });
        match self.client.store(request).await {
            Ok((id, status)) => {
                self.logger.info(&format!(
                    "{TAG} tool=memory_store -> status={status:?} id={id}"
                ));
                match status {
                    super::client::StoreStatus::Dedupe => {
                        format!("Similar memory already exists (id={id}).")
                    }
                    super::client::StoreStatus::Created => {
                        format!("Stored: \"{}\" (id={id})", prefix_utf16(&text, 100))
                    }
                }
            }
            Err(err) => self.failed("memory_store", &err),
        }
    }

    // ── memory_forget ───────────────────────────────────────────────────────

    pub(crate) async fn memory_forget(&self, args: &Value) -> String {
        let cfg = &self.cfg;
        let raw_id = arg_str(args, "memoryId");
        let query = arg_str(args, "query");
        // deleting is destructive: the same id check as memory_get, and an early error rather than a
        // DELETE of an id that cannot be addressed
        let mut memory_id = String::new();
        if !raw_id.is_empty() {
            match normalize_memory_id(&raw_id) {
                Ok(id) => memory_id = id,
                Err(hint) if query.is_empty() => {
                    self.logger.info(&format!(
                        "{TAG} tool=memory_forget rejected unusable id=\"{}\"",
                        prefix_utf16(&raw_id, 24)
                    ));
                    return format!("memory_forget: {hint}");
                }
                Err(_) => {}
            }
        }
        self.logger.info(&format!(
            "{TAG} tool=memory_forget memoryId={} query=\"{}\"",
            if memory_id.is_empty() {
                "?"
            } else {
                &memory_id
            },
            prefix_utf16(&query, 60)
        ));
        if !memory_id.is_empty() {
            return match self.client.delete(&memory_id).await {
                Ok(ok) => {
                    self.logger.info(&format!(
                        "{TAG} tool=memory_forget -> deleted id={memory_id} ok={ok}"
                    ));
                    if ok {
                        format!("Memory {memory_id} forgotten.")
                    } else {
                        format!("Memory {memory_id} could not be forgotten (the server did not delete it).")
                    }
                }
                Err(err) => self.failed("memory_forget", &err),
            };
        }
        if query.is_empty() {
            return "memory_forget: provide either memoryId or query".into();
        }
        let request = json!({
            "query": query,
            "agent_id": cfg.agent_id,
            "project_id": cfg.project_id,
            "purpose": "active",
            "top_k": 5,
            "render": "none",
        });
        let opts = super::client::CallOptions::deadline(self.budgets.active_deadline_ms());
        let resp = match self.client.query(request, &opts).await {
            Ok(r) => r,
            Err(err) => return self.failed("memory_forget", &err),
        };
        self.budgets.learn_from_response(resp.get("execution"));
        // only a band with a stored memory under it can be deleted (grounded facts, needs-verification);
        // the id is the addressable one — an entry's own id is a per-query reference and deleting by it
        // would fail silently
        struct Candidate {
            memory_id: String,
            text: String,
            score: f64,
        }
        let mut candidates: Vec<Candidate> = Vec::new();
        for key in ["grounded_facts", "needs_verification"] {
            for f in resp[key].as_array().into_iter().flatten() {
                let Some(id) = pick_addressable_memory_id(f) else {
                    continue;
                };
                let score = score_of(f);
                if score < 0.05 {
                    continue;
                }
                candidates.push(Candidate {
                    memory_id: id,
                    text: s(f, "claim").to_owned(),
                    score,
                });
            }
        }
        if candidates.is_empty() {
            self.logger.info(&format!(
                "{TAG} tool=memory_forget -> no candidates for query"
            ));
            return "No matching memories found.".into();
        }
        // A search never deletes: a score says how well a memory matched the words, not that it is the one
        // meant, and a deletion cannot be undone. The agent picks one and calls again with its id.
        let list: Vec<String> = candidates
            .iter()
            .map(|c| {
                format!(
                    "- [{}] {} ({}%)",
                    c.memory_id,
                    prefix_utf16(&c.text, 60),
                    pct(c.score)
                )
            })
            .collect();
        self.logger.info(&format!(
            "{TAG} tool=memory_forget -> returned {} candidates",
            candidates.len()
        ));
        format!(
            "Found {} candidate{}. Nothing was deleted: call memory_forget again with the memoryId of the one to forget:\n{}",
            candidates.len(),
            if candidates.len() == 1 { "" } else { "s" },
            list.join("\n")
        )
    }

    // ── memory_get ──────────────────────────────────────────────────────────

    /// The conversation part, once the session and the source turns are known.
    async fn conversation(
        &self,
        session_id: &str,
        src: &[i64],
        mode: PageMode,
        window: i64,
        from_turn: Option<i64>,
        marked: Marked,
    ) -> String {
        let cfg = &self.cfg;
        let page = if mode == PageMode::Session {
            self.client
                .get_session_turns(
                    session_id,
                    &cfg.agent_id,
                    &cfg.project_id,
                    Some(from_turn.unwrap_or(0)),
                    None,
                    Some(SESSION_PAGE_SIZE),
                )
                .await
        } else if !src.is_empty() {
            // at most a page: sources spread far apart are shown from the first, and the page says
            // the conversation goes on
            let (from, to) = window_around(src[0], src[src.len() - 1], window);
            self.client
                .get_session_turns(
                    session_id,
                    &cfg.agent_id,
                    &cfg.project_id,
                    Some(from),
                    Some(to),
                    Some(
                        to.saturating_sub(from)
                            .saturating_add(1)
                            .min(SESSION_PAGE_SIZE),
                    ),
                )
                .await
        } else {
            // a source session with no turn known: its opening page
            self.client
                .get_session_turns(
                    session_id,
                    &cfg.agent_id,
                    &cfg.project_id,
                    Some(0),
                    None,
                    Some(SESSION_PAGE_SIZE),
                )
                .await
        };
        match page {
            Ok(page) if page["total"].as_i64().unwrap_or(0) > 0 => {
                let mode = if src.is_empty() {
                    PageMode::Session
                } else {
                    mode
                };
                format_page(session_id, &page, src, mode, window, marked)
            }
            Ok(_) => format!("(no archived conversation found for session {session_id})"),
            Err(err) if err.is_not_found() => {
                format!("(no archived conversation found for session {session_id})")
            }
            Err(err) => {
                let detail = err.describe();
                self.logger.warn(&format!(
                    "{TAG} tool=memory_get conversation FAILED session={session_id} {detail}"
                ));
                format!("(the conversation of session {session_id} could not be read: {detail})")
            }
        }
    }

    fn open_limit(&self, turn_key: &str) -> String {
        let made = self.used(turn_key, Use::Opens);
        self.logger.info(&format!(
            "{TAG} tool=memory_get -> not opened: {made} opened already for this message (session={turn_key})"
        ));
        format!(
            "memory_get: not opened — {made} memories were opened already for this message. Answer with what you have; if it does not settle the question, say what is missing."
        )
    }

    pub(crate) async fn memory_get(&self, args: &Value, turn_key: &str) -> String {
        let cfg = &self.cfg;
        let raw_id = arg_str(args, "memoryId");
        let query = arg_str(args, "query");
        let want_session = arg_str(args, "session");
        let mode = if args.get("context").and_then(Value::as_str) == Some("session") {
            PageMode::Session
        } else {
            PageMode::Around
        };
        let window = arg_num(args, "window")
            .map(|n| (n.floor() as i64).clamp(0, MAX_CONTEXT_WINDOW))
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        let from_turn = arg_num(args, "fromTurn").map(|n| (n.floor() as i64).max(0));

        // ── an excerpt by where it is: session + turns ──
        if raw_id.is_empty() && query.is_empty() {
            if want_session.is_empty() {
                return "memory_get: provide memoryId (32 hex chars), query, or session + turns."
                    .into();
            }
            let turns_arg = args.get("turns").filter(|t| !t.is_null());
            let span = turns_arg.and_then(parse_turns);
            if turns_arg.is_some() && span.is_none() {
                return "memory_get: turns must be a message index (\"12\") or range (\"12-14\"), as RAW_REFS lists them.".into();
            }
            if !self.take(turn_key, Use::Opens) {
                return self.open_limit(turn_key);
            }
            self.logger.info(&format!(
                "{TAG} tool=memory_get session={want_session} turns={} context={} window={window}",
                turns_arg.map_or("-".to_owned(), |t| turn_label(t).unwrap_or_default()),
                if mode == PageMode::Session {
                    "session"
                } else {
                    "around"
                }
            ));
            // (HeliconSearch returns at most 200 messages a call: a wider range is read as its first 200)
            let asked: Vec<i64> = span
                .map(|(a, b)| (a..=a + (b - a).min(199)).collect())
                .unwrap_or_default();
            return self
                .conversation(
                    &want_session,
                    &asked,
                    mode,
                    window,
                    from_turn,
                    Marked::Excerpt,
                )
                .await;
        }

        // ── a memory: by id, else by query ──
        let mut memory_id = String::new();
        let mut via = "id";
        if !raw_id.is_empty() {
            match normalize_memory_id(&raw_id) {
                Ok(id) => memory_id = id,
                Err(hint) if query.is_empty() => {
                    self.logger.info(&format!(
                        "{TAG} tool=memory_get rejected unusable id=\"{}\"",
                        prefix_utf16(&raw_id, 24)
                    ));
                    return format!("memory_get: {hint}");
                }
                Err(_) => self.logger.info(&format!(
                    "{TAG} tool=memory_get unusable id=\"{}\" -> resolving by query",
                    prefix_utf16(&raw_id, 24)
                )),
            }
        }
        // finding the memory by query is a memory search: it counts with memory_recall's
        if memory_id.is_empty() && self.used(turn_key, Use::Searches) >= MAX_SEARCHES_PER_TURN {
            let made = self.used(turn_key, Use::Searches);
            self.logger.info(&format!(
                "{TAG} tool=memory_get -> not searched: {made} searches already for this message (session={turn_key})"
            ));
            return format!(
                "memory_get: not searched — memory was already searched {made} times for this message. Open a memory by an id you have (MEMORY_IDS, memory_recall) or answer with what you have."
            );
        }
        if !self.take(turn_key, Use::Opens) {
            return self.open_limit(turn_key);
        }
        if memory_id.is_empty() {
            self.take(turn_key, Use::Searches);
            // the recalled context does not always carry an id: open what is described
            self.logger.info(&format!(
                "{TAG} tool=memory_get resolving query=\"{}\"",
                prefix_utf16(&query, 60)
            ));
            let request = json!({
                "query": query,
                "agent_id": cfg.agent_id,
                "project_id": cfg.project_id,
                "purpose": "active",
                "top_k": 5,
                "render": "none",
            });
            let opts = super::client::CallOptions::deadline(self.budgets.active_deadline_ms());
            let resp = match self.client.query(request, &opts).await {
                Ok(r) => r,
                Err(err) => {
                    self.give_back(turn_key, Use::Searches);
                    self.give_back(turn_key, Use::Opens);
                    return self.failed("memory_get", &err);
                }
            };
            self.budgets.learn_from_response(resp.get("execution"));
            let mut candidates: Vec<(String, f64)> = Vec::new();
            for key in ["grounded_facts", "needs_verification"] {
                for f in resp[key].as_array().into_iter().flatten() {
                    if let Some(id) = pick_addressable_memory_id(f) {
                        if score_of(f) >= cfg.recall_min_score {
                            candidates.push((id, score_of(f)));
                        }
                    }
                }
            }
            candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let Some((id, _)) = candidates.into_iter().next() else {
                self.logger.info(&format!(
                    "{TAG} tool=memory_get -> no addressable match for query"
                ));
                return "memory_get: no stored memory matched that query. A recalled verbatim excerpt has no memory id — open it with session + turns from RAW_REFS.".into();
            };
            memory_id = id;
            via = "query";
        }
        let detail = match self.client.explain(&memory_id).await {
            Ok(d) => d,
            Err(err) => return self.failed("memory_get", &err),
        };
        let Some(d) = detail.filter(|d| d["memory"].is_object()) else {
            return format!(
                "memory_get: no memory with id {memory_id} (it may have been deleted or superseded). Call memory_recall for a current id."
            );
        };
        let mem = &d["memory"];
        let chain = d["supersedes_chain"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let source = match self.client.get_source(&memory_id).await {
            Ok(v) => Some(v),
            Err(err) => {
                self.logger.warn(&format!(
                    "{TAG} tool=memory_get source FAILED id={memory_id} {}",
                    err.describe()
                ));
                None
            }
        };
        let by_session = source_turns(source.as_ref());
        let sessions = source_sessions(source.as_ref(), &by_session);
        let turns_of = |sid: &str| -> Vec<i64> {
            by_session
                .iter()
                .find(|(s, _)| s == sid)
                .map(|(_, t)| t.clone())
                .unwrap_or_default()
        };
        let source_line = if sessions.is_empty() {
            "none".to_owned()
        } else {
            sessions
                .iter()
                .map(|sid| {
                    let t = turns_of(sid);
                    if t.is_empty() {
                        sid.clone()
                    } else {
                        format!(
                            "{sid} (messages {})",
                            t.iter()
                                .map(|i| format!("#{i}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        let mut lines: Vec<String> = vec![
            format!("**Memory** id={}", s(mem, "id")),
            format!("Text: {}", s(mem, "text")),
            format!(
                "Type: {} | Category: {} | Importance: {}",
                s(mem, "memory_type"),
                s(mem, "category"),
                js_number(&mem["importance"])
            ),
            format!("Source completeness: {}", s(&d, "source_completeness")),
            format!("Source sessions: {source_line}"),
        ];
        if !chain.is_empty() {
            lines.push(format!(
                "\nVersion history ({} older version{}):",
                chain.len(),
                if chain.len() > 1 { "s" } else { "" }
            ));
            for (i, c) in chain.iter().enumerate() {
                lines.push(format!(
                    "  {}. id={} ({}) {}",
                    i + 1,
                    s(c, "memory_id"),
                    s(c, "lifecycle_state"),
                    s(c, "text")
                ));
            }
        }
        let target: Option<String> = if !want_session.is_empty() && sessions.contains(&want_session)
        {
            Some(want_session.clone())
        } else {
            sessions.first().cloned()
        };
        if !want_session.is_empty() && target.as_deref() != Some(want_session.as_str()) {
            lines.push(format!(
                "(session {want_session} is not a source of this memory{})",
                target
                    .as_ref()
                    .map(|t| format!(" — showing {t}"))
                    .unwrap_or_default()
            ));
        }
        match &target {
            Some(t) => {
                let conv = self
                    .conversation(
                        t,
                        &turns_of(t),
                        mode,
                        window,
                        from_turn,
                        Marked::MemorySource,
                    )
                    .await;
                lines.push(String::new());
                lines.push(conv);
                if sessions.len() > 1 {
                    lines.push(format!(
                        "(also said in: {} — pass session to show one of them)",
                        sessions
                            .iter()
                            .filter(|x| *x != t)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            None => {
                lines.push(String::new());
                lines.push("(no source conversation is recorded for this memory)".into());
            }
        }
        self.logger.info(&format!(
            "{TAG} tool=memory_get id={memory_id} via={via} chain={} sessions={} session={} context={} window={window}",
            chain.len(),
            sessions.len(),
            target.as_deref().unwrap_or("-"),
            if mode == PageMode::Session { "session" } else { "around" }
        ));
        lines.join("\n")
    }

    // ── skill_get ───────────────────────────────────────────────────────────

    pub(crate) async fn skill_get(&self, args: &Value) -> String {
        let skill_id = arg_str(args, "skillId");
        // the id is a 32-hex value on the server; a URL or path is refused so a skill id mistaken
        // for a <location> cannot make us leak the HeliconSearch host
        if skill_id.is_empty() {
            return "skill_get: skillId is required.".into();
        }
        let suspicious = skill_id.contains(['/', '?', '#'])
            || skill_id.chars().any(char::is_whitespace)
            || skill_id.split_once("://").is_some_and(|(scheme, _)| {
                !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphabetic())
            });
        if suspicious {
            self.logger.info(&format!(
                "{TAG} tool=skill_get rejected suspicious skillId=\"{}\"",
                prefix_utf16(&skill_id, 40)
            ));
            return "skill_get: skillId must be the 32-hex <skill_id> value from <recalled_skills>, not a URL or path.".into();
        }
        self.logger
            .info(&format!("{TAG} tool=skill_get skillId={skill_id}"));
        match self.client.get_skill(&skill_id).await {
            Ok(None) => format!(
                "skill_get: no published skill with id {skill_id} (it may have been retracted or renamed)."
            ),
            Ok(Some(body)) if body.is_empty() => {
                format!("skill_get: skill {skill_id} returned an empty body.")
            }
            Ok(Some(body)) => {
                self.logger.info(&format!(
                    "{TAG} tool=skill_get skillId={skill_id} bytes={}",
                    body.len()
                ));
                // a short header so the model can tell several skills apart
                format!("**Skill** id={skill_id}\n\n{body}")
            }
            Err(err) => self.failed("skill_get", &err),
        }
    }
}

// ── registration ─────────────────────────────────────────────────────────────

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
}

fn turn_key(ctx: &crate::ToolContext) -> String {
    format!("{}#{}", ctx.session_id, ctx.turn_id)
}

/// The tools of this extension, as registered under the `helicon` namespace.
/// What a tool hands the model at most (UTF-8 bytes): a page of a long conversation, or a skill, can be
/// far larger than is useful, and it would all be paid for in the context of every later step.
pub(crate) const MAX_TOOL_OUTPUT_BYTES: usize = 60_000;

pub(crate) fn capped(text: String) -> Value {
    if text.len() <= MAX_TOOL_OUTPUT_BYTES {
        return Value::String(text);
    }
    let kept = cut_bytes(&text, MAX_TOOL_OUTPUT_BYTES);
    Value::String(format!(
        "{kept}\n(cut here: the answer was {} bytes, more than the {MAX_TOOL_OUTPUT_BYTES} a tool result is given. Ask for less: a narrower query, a smaller window, or the next page.)",
        text.len()
    ))
}

pub(crate) fn build_tools(tools: &Arc<Tools>) -> Vec<Tool> {
    let cfg = tools.cfg.clone();
    let mut out: Vec<Tool> = Vec::new();

    let t = tools.clone();
    out.push(
        Tool::new("memory_recall")
            // (The text of a tool must not depend on the configuration: the engine keeps the descriptor of a
            // tool name and refuses a changed one, which would switch the memory off at the next start.)
            .description("Search long-term memory. Memories relevant to the user's message may already be shown in <relevant-memories>; use this for what they do not cover. One search per user message: make the query cover what is missing.")
            .input_schema(schema(
                json!({
                    "query": {"type": "string", "description": "Search query (natural language)"},
                    "limit": {"type": "number", "description": "Max results to return (1-20; a default is used when omitted)"},
                }),
                &["query"],
            ))
            .handler(move |args, ctx| {
                let t = t.clone();
                async move { Ok(capped(t.memory_recall(&args, &turn_key(&ctx)).await)) }
            }),
    );

    // The agent's own writes: only when memoryStoreTool is on (off by default: such a write has no
    // source turn, and the fact it states is stored again beside what capture extracts).
    if cfg.memory_store_tool {
        let t = tools.clone();
        out.push(
            Tool::new("memory_store")
                .description("Save important information into the cross-session long-term semantic memory in HeliconSearch. Use for preferences, facts, decisions, named entities, or instructions worth remembering across sessions. Duplicates are detected on the server side.")
                .input_schema(schema(
                    json!({
                        "text": {"type": "string", "description": "Information to remember (free text)"},
                        "category": {"type": "string", "enum": MEMORY_CATEGORIES, "description": "Memory category (default: other)"},
                        "importance": {"type": "number", "description": "Importance 0-1 (default 0.7)"},
                    }),
                    &["text"],
                ))
                .handler(move |args, _ctx| {
                    let t = t.clone();
                    async move { Ok(capped(t.memory_store(&args).await)) }
                }),
        );
    }

    let t = tools.clone();
    out.push(
        Tool::new("memory_forget")
            .description("Delete a memory from HeliconSearch by id. When the id is unknown, give `query` instead: that only lists the candidate memories (nothing is deleted); call again with the `memoryId` of the one to forget.")
            .input_schema(schema(
                json!({
                    "memoryId": {"type": "string", "description": "32-character hex memory id from memory_recall or the MEMORY_IDS block. Never a `pkg:…` id — those are per-query references and cannot be deleted."},
                    "query": {"type": "string", "description": "Search query to locate memory"},
                }),
                &[],
            ))
            .handler(move |args, _ctx| {
                let t = t.clone();
                async move { Ok(capped(t.memory_forget(&args).await)) }
            }),
    );

    let t = tools.clone();
    let mut get_props = Map::new();
    get_props.insert("memoryId".into(), json!({"type": "string", "description": "32-character hex memory id from memory_recall or the MEMORY_IDS block. Never a `pkg:…` id — those are per-query references and cannot be looked up."}));
    get_props.insert("query".into(), json!({"type": "string", "description": "Natural-language description of the memory, used when its id is unknown. The best-matching memory is opened."}));
    get_props.insert("session".into(), json!({"type": "string", "description": "A session id: with turns, the excerpt to open (from RAW_REFS); with memoryId, which of the memory's source sessions to show."}));
    get_props.insert("turns".into(), json!({"type": "string", "description": "With session (no memoryId): the message(s) to open, as RAW_REFS lists them — \"12\" or \"12-14\"."}));
    get_props.insert("window".into(), json!({"type": "number", "description": format!("Messages on each side of the source (default {DEFAULT_CONTEXT_WINDOW}, at most {MAX_CONTEXT_WINDOW}). Widen only when the messages shown stop short of what you need.")}));
    get_props.insert("context".into(), json!({"type": "string", "enum": ["around", "session"], "description": format!("\"around\" (default): the messages around the source. \"session\": the whole conversation, {SESSION_PAGE_SIZE} messages a page (fromTurn continues) — only when the user asks to go through that conversation.")}));
    get_props.insert("fromTurn".into(), json!({"type": "number", "description": "With context=\"session\": the message to start the page at (as the previous page says)."}));
    out.push(
        Tool::new("memory_get")
            .description("Open one memory — by its 32-hex id (MEMORY_IDS block or memory_recall), or by query when it has no id — with its full text, older versions, and the source message with a few messages around it (what the user said and what the assistant replied). Use it when you need that and the shown memory does not give it; open only the memory you need. session + turns opens a verbatim excerpt (RAW_REFS) the same way. Never pass a `pkg:…` id or a `<skill_id>` (use skill_get for those).")
            .input_schema(schema(Value::Object(get_props), &[]))
            .handler(move |args, ctx| {
                let t = t.clone();
                async move { Ok(capped(t.memory_get(&args, &turn_key(&ctx)).await)) }
            }),
    );

    let t = tools.clone();
    out.push(
        Tool::new("skill_get")
            .description("Retrieve the SKILL.md body of a published skill matched by HeliconSearch auto-recall. Pass the 32-hex `skill_id` from the `<recalled_skills>` block in injected context (e.g. `2e24aed6c17248949926b5bfe285781f`). These are remote records, not local files — never read or guess a file path for them. Returns the markdown guidance body — apply it to the current task. Do not call web_fetch or curl: the host URL is private and must not be exposed to the model.")
            .input_schema(schema(
                json!({"skillId": {"type": "string", "description": "The 32-hex skill_id from the <recalled_skills> block in injected recall context (e.g. `2e24aed6c17248949926b5bfe285781f`). Use the <skill_id> value verbatim, NOT the <name> slug and NOT a filesystem path."}}),
                &["skillId"],
            ))
            .handler(move |args, _ctx| {
                let t = t.clone();
                async move { Ok(capped(t.skill_get(&args).await)) }
            }),
    );
    out
}

/// The short names of the tools `build_tools` makes for this configuration.
pub(crate) fn tool_names(cfg: &MemoryConfig) -> Vec<&'static str> {
    let mut names = vec!["memory_recall", "memory_get", "memory_forget", "skill_get"];
    if cfg.memory_store_tool {
        names.push("memory_store");
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helicon::test_support::{MockHs, Reply};

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn tools(hs: &MockHs, extra: Value) -> Arc<Tools> {
        let mut raw =
            json!({"baseUrl": hs.url(), "apiKey": "hs_test", "agentId": "a1", "projectId": "p1"});
        for (k, v) in extra.as_object().unwrap() {
            raw[k] = v.clone();
        }
        let cfg = Arc::new(MemoryConfig::parse(&raw, &|_| None).unwrap());
        let logger = Logger::new(std::env::temp_dir().join("helicon-tools-test.log"), false);
        let client = HeliconClient::new(&cfg.base_url, &cfg.api_key, 2_000, logger.clone(), false);
        let budgets = RecallBudgets::new(&cfg.base_url, 15_000, logger.clone());
        Tools::new(cfg, client, logger, budgets)
    }

    #[test]
    fn a_package_id_is_not_a_memory_id() {
        assert_eq!(
            normalize_memory_id(&format!(" {} ", ID.to_uppercase())),
            Ok(ID.to_owned())
        );
        assert!(normalize_memory_id("pkg:abc")
            .unwrap_err()
            .contains("per-query package id"));
        assert!(normalize_memory_id("xyz")
            .unwrap_err()
            .contains("not a valid memory id"));
        assert_eq!(
            normalize_memory_id("  ").unwrap_err(),
            "memoryId is required."
        );
    }

    #[tokio::test]
    async fn recall_lists_facts_with_whole_ids_and_searches_once_per_message() {
        let hs = MockHs::start(|req| {
            assert_eq!(req.path, "/v1/memory/query");
            assert_eq!(req.body["purpose"], "active");
            assert_eq!(req.body["render"], "none");
            assert_eq!(req.body["agent_id"], "a1");
            assert_eq!(req.auth.as_deref(), Some("Bearer hs_test"));
            Reply::ok(json!({"grounded_facts": [
                {"id": "pkg:1", "claim": "likes tea", "score": 0.9, "support_level": "grounded", "source_memory_ids": [ID]},
                {"id": "pkg:2", "claim": "no id", "score": 0.5, "support_level": "grounded", "source_memory_ids": []},
                {"id": "pkg:3", "claim": "weak", "score": 0.0001, "support_level": "grounded", "source_memory_ids": []},
            ]}))
        })
        .await;
        let t = tools(&hs, json!({}));
        let text = t
            .memory_recall(&json!({"query": "what do I like?"}), "s#t1")
            .await;
        assert!(text.starts_with("Found 2 memories:"), "{text}");
        assert!(text.contains(&format!("1. [grounded] likes tea (90% id={ID})")));
        assert!(text.contains("2. [grounded] no id (50%, no id — cannot be expanded)"));
        // a second search for the same message is refused without a request
        let again = t.memory_recall(&json!({"query": "more"}), "s#t1").await;
        assert!(again.contains("not searched"), "{again}");
        assert_eq!(hs.requests().len(), 1);
        // another message searches again
        assert!(t
            .memory_recall(&json!({"query": "more"}), "s#t2")
            .await
            .starts_with("Found"));
    }

    #[test]
    fn a_tool_result_is_cut_at_a_limit_and_says_so() {
        let small = capped("short".into());
        assert_eq!(small, Value::String("short".into()));
        // multi-byte text is cut on a character boundary
        let big = capped("é".repeat(MAX_TOOL_OUTPUT_BYTES));
        let text = big.as_str().unwrap();
        assert!(text.contains("(cut here:"), "{}", &text[text.len() - 200..]);
        assert!(text.len() < MAX_TOOL_OUTPUT_BYTES + 400);
        assert!(text.starts_with("éé"));
    }

    #[tokio::test]
    async fn recall_lists_facts_the_server_has_not_verified_yet() {
        // some builds of the server keep a fact that was just extracted in `needs_verification`
        let hs = MockHs::start(|_| {
            Reply::ok(json!({"grounded_facts": [], "needs_verification": [
                {"id": "pkg:9", "claim": "likes jasmine tea", "score": 0.7, "source_memory_ids": [ID]},
                {"id": "pkg:8", "claim": "weak", "score": 0.0001, "source_memory_ids": []},
            ]}))
        })
        .await;
        let t = tools(&hs, json!({}));
        let text = t.memory_recall(&json!({"query": "tea?"}), "s#t1").await;
        assert!(text.starts_with("Found 1 memories:"), "{text}");
        assert!(
            text.contains(&format!(
                "1. [needs_verification] likes jasmine tea (70% id={ID})"
            )),
            "{text}"
        );
        // with grounded facts too: those first
        let hs = MockHs::start(|_| {
            Reply::ok(json!({"grounded_facts": [
                {"id": "pkg:1", "claim": "likes tea", "score": 0.9, "support_level": "grounded", "source_memory_ids": [ID]},
            ], "needs_verification": [
                {"id": "pkg:9", "claim": "likes jasmine", "score": 0.7, "source_memory_ids": []},
            ]}))
        })
        .await;
        let text = tools(&hs, json!({}))
            .memory_recall(&json!({"query": "tea?"}), "s#t2")
            .await;
        assert!(text.contains("1. [grounded] likes tea"), "{text}");
        assert!(
            text.contains("2. [needs_verification] likes jasmine"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_search_the_server_never_answered_does_not_use_up_the_messages_search() {
        let calls = Arc::new(Mutex::new(0));
        let c = calls.clone();
        let hs = MockHs::start(move |_| {
            let mut n = c.lock().unwrap();
            *n += 1;
            if *n == 1 {
                Reply::status(503, json!({"retmsg": "down"}))
            } else {
                Reply::ok(json!({"grounded_facts": [
                    {"id": "pkg:1", "claim": "likes tea", "score": 0.9, "source_memory_ids": [ID]},
                ]}))
            }
        })
        .await;
        let t = tools(&hs, json!({}));
        let failed = t.memory_recall(&json!({"query": "tea?"}), "s#t1").await;
        assert!(!failed.starts_with("Found"), "{failed}");
        // the one search for this message is still there (and the agent was not told "no record")
        let retried = t.memory_recall(&json!({"query": "tea?"}), "s#t1").await;
        assert!(retried.starts_with("Found 1 memor"), "{retried}");
        // and now it is used
        assert!(t
            .memory_recall(&json!({"query": "more"}), "s#t1")
            .await
            .contains("not searched"));
    }

    #[tokio::test]
    async fn recall_falls_back_to_summaries_and_verbatim_turns() {
        let hs = MockHs::start(|_| {
            Reply::ok(json!({"grounded_facts": [], "topic_rollups": [
                {"topic": "trips", "summary": "went to Rome", "score": 0.4}
            ], "raw_context": [{"session_id": "openworker/s1", "turns": [
                {"speaker": "user", "text": "I flew to Rome", "turn": 7, "score": 0.3}
            ]}]}))
        })
        .await;
        let t = tools(&hs, json!({}));
        let text = t.memory_recall(&json!({"query": "trips"}), "k").await;
        assert!(text.starts_with("No directly attributable memory, but related context exists:"));
        assert!(text.contains("1. [trips] went to Rome (40%)"));
        assert!(text.contains("1. user: I flew to Rome (session=\"openworker/s1\", turns=\"7\")"));
        let hs = MockHs::start(|_| Reply::ok(json!({"grounded_facts": []}))).await;
        assert_eq!(
            tools(&hs, json!({}))
                .memory_recall(&json!({"query": "x"}), "k")
                .await,
            "No relevant memories found."
        );
    }

    #[tokio::test]
    async fn a_failed_call_is_a_text_for_the_model() {
        let hs =
            MockHs::start(|_| Reply::status(503, json!({"retcode": 5, "retmsg": "busy"}))).await;
        let text = tools(&hs, json!({}))
            .memory_recall(&json!({"query": "x"}), "k")
            .await;
        assert_eq!(
            text,
            "memory_recall failed: HeliconApiError status=503 retcode=5 retmsg=busy"
        );
        assert_eq!(
            tools(&hs, json!({})).memory_recall(&json!({}), "k").await,
            "memory_recall: query is required"
        );
    }

    #[tokio::test]
    async fn forget_deletes_by_id_and_rejects_ids_that_cannot_be_addressed() {
        let hs = MockHs::start(|req| {
            assert_eq!(req.method, "DELETE");
            assert_eq!(req.path, format!("/v1/memory/{ID}"));
            Reply::ok(json!({"deleted": true}))
        })
        .await;
        let t = tools(&hs, json!({}));
        assert_eq!(
            t.memory_forget(&json!({"memoryId": ID})).await,
            format!("Memory {ID} forgotten.")
        );
        let bad = t.memory_forget(&json!({"memoryId": "pkg:abc"})).await;
        assert!(bad.starts_with("memory_forget: \"pkg:abc\" is a per-query package id"));
        assert_eq!(hs.requests().len(), 1);
        assert_eq!(
            t.memory_forget(&json!({})).await,
            "memory_forget: provide either memoryId or query"
        );
    }

    #[tokio::test]
    async fn forget_by_query_only_lists_candidates_however_strong_the_match() {
        let strong = MockHs::start(|req| {
            assert_ne!(req.method, "DELETE", "a search must not delete");
            Reply::ok(json!({"grounded_facts": [
                {"claim": "likes tea", "score": 1.2, "support_level": "g", "source_memory_ids": [ID]},
                {"claim": "no id", "score": 0.9, "source_memory_ids": []},
            ]}))
        })
        .await;
        let text = tools(&strong, json!({}))
            .memory_forget(&json!({"query": "tea"}))
            .await;
        assert!(
            text.starts_with("Found 1 candidate. Nothing was deleted"),
            "{text}"
        );
        assert!(
            text.contains(&format!("- [{ID}] likes tea (120%)")),
            "{text}"
        );
        assert_eq!(strong.requests().len(), 1, "one search, no delete");
        let other = "fedcba9876543210fedcba9876543210";
        let several = MockHs::start(move |_| {
            Reply::ok(json!({"grounded_facts": [
                {"claim": "likes tea", "score": 0.8, "source_memory_ids": [ID]},
                {"claim": "likes coffee", "score": 0.6, "source_memory_ids": [other]},
            ]}))
        })
        .await;
        let text = tools(&several, json!({}))
            .memory_forget(&json!({"query": "likes"}))
            .await;
        assert!(
            text.starts_with("Found 2 candidates. Nothing was deleted"),
            "{text}"
        );
        assert!(text.contains(&format!("- [{ID}] likes tea (80%)")));
        assert_eq!(several.requests().len(), 1, "nothing was deleted");
    }

    #[tokio::test]
    async fn store_writes_with_a_manual_source() {
        let hs = MockHs::start(|req| {
            assert_eq!(req.path, "/v1/memory/store");
            assert_eq!(req.body["extraction_method"], "agent_tool");
            assert_eq!(req.body["category"], "other");
            assert_eq!(req.body["source_refs"][0]["type"], "manual");
            Reply::ok(json!({"memory_id": ID, "action": "created"}))
        })
        .await;
        let t = tools(&hs, json!({"memoryStoreTool": true}));
        assert_eq!(
            t.memory_store(&json!({"text": "likes tea", "category": "nonsense"}))
                .await,
            format!("Stored: \"likes tea\" (id={ID})")
        );
        assert_eq!(
            t.memory_store(&json!({"text": " "})).await,
            "memory_store: text is required"
        );
    }

    #[tokio::test]
    async fn get_opens_a_memory_with_the_conversation_around_its_source() {
        let hs = MockHs::start(|req| match req.path.as_str() {
            p if p.starts_with("/v1/memory/explain") => Reply::ok(json!({
                "memory": {"id": ID, "text": "likes tea", "memory_type": "semantic", "category": "preference", "importance": 1.0},
                "supersedes_chain": [{"memory_id": "old", "text": "likes coffee", "lifecycle_state": "superseded"}],
                "source_completeness": "complete"
            })),
            p if p.starts_with("/v1/memory/source/") => Reply::ok(json!({
                "turns": [{"session_id": "openworker/s1", "turn_index": 4}], "evidence": [], "source_refs": []
            })),
            p if p.starts_with("/v1/memory/session/turns") => {
                assert!(p.contains("session_id=openworker%2Fs1"), "{p}");
                assert!(p.contains("from_turn=1") && p.contains("to_turn=7"), "{p}");
                Reply::ok(json!({"turns": [
                    {"turn_index": 4, "role": "user", "content": "I like tea", "created_at": null, "omitted": false}
                ], "total": 9, "first_turn": 0, "last_turn": 8, "more": false}))
            }
            other => panic!("unexpected {other}"),
        })
        .await;
        let t = tools(&hs, json!({}));
        let text = t.memory_get(&json!({"memoryId": ID}), "s#t").await;
        assert!(text.starts_with(&format!("**Memory** id={ID}\nText: likes tea\nType: semantic | Category: preference | Importance: 1\n")), "{text}");
        assert!(text.contains("Source sessions: openworker/s1 (messages #4)"));
        assert!(text
            .contains("Version history (1 older version):\n  1. id=old (superseded) likes coffee"));
        assert!(
            text.contains("[#4] user  ◀ source of this memory\nI like tea"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn get_opens_at_most_three_memories_per_message() {
        let hs = MockHs::start(|_| Reply::status(404, json!({"retmsg": "none"}))).await;
        let t = tools(&hs, json!({}));
        for _ in 0..3 {
            assert!(t
                .memory_get(&json!({"memoryId": ID}), "k")
                .await
                .contains("no memory with id"));
        }
        assert!(t
            .memory_get(&json!({"memoryId": ID}), "k")
            .await
            .contains("not opened — 3 memories"));
        assert_eq!(
            t.memory_get(&json!({}), "k2").await,
            "memory_get: provide memoryId (32 hex chars), query, or session + turns."
        );
        assert!(t
            .memory_get(&json!({"session": "s", "turns": "x"}), "k2")
            .await
            .contains("turns must be a message index"));
    }

    #[tokio::test]
    async fn skill_get_refuses_urls_and_returns_the_body() {
        let hs = MockHs::start(|req| {
            assert_eq!(req.path, format!("/v1/memory/skill/{ID}"));
            Reply::ok(json!({"content": "# Skill\nDo it."}))
        })
        .await;
        let t = tools(&hs, json!({}));
        assert_eq!(
            t.skill_get(&json!({"skillId": ID})).await,
            format!("**Skill** id={ID}\n\n# Skill\nDo it.")
        );
        for bad in ["http://h/x", "a/b", "a b", "a?b"] {
            assert!(
                t.skill_get(&json!({"skillId": bad}))
                    .await
                    .contains("not a URL or path"),
                "{bad}"
            );
        }
        assert_eq!(hs.requests().len(), 1);
    }

    #[test]
    fn the_tool_set_follows_the_options() {
        let cfg = |extra: Value| {
            let mut raw = json!({"baseUrl": "http://127.0.0.1:5001", "apiKey": "k"});
            for (k, v) in extra.as_object().unwrap() {
                raw[k] = v.clone();
            }
            MemoryConfig::parse(&raw, &|_| None).unwrap()
        };
        assert_eq!(
            tool_names(&cfg(json!({}))),
            vec!["memory_recall", "memory_get", "memory_forget", "skill_get"]
        );
        assert!(tool_names(&cfg(json!({"memoryStoreTool": true}))).contains(&"memory_store"));
    }
}
