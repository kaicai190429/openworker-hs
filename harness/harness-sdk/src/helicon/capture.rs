//! Capture: after each completed turn, archive the new part of the session's history on
//! HeliconSearch (`turns.v1`) and hand it to fact extraction.
//!
//! It is driven by the engine's journal, not by the application: a `turn_completed` event of an
//! attached session schedules a cycle, so it works the same whether the application used `run`, `send`
//! or a stream, and also catches turns the application never awaited. The history is the durable queue
//! — a cycle reads from the cursors saved in the session record, so what could not be delivered
//! (HeliconSearch down, process killed) is sent by the next cycle, in the next run.
//!
//! The order is the OpenClaw plugin's: archive first (the verbatim text is retrievable at once), then
//! extraction of what was archived — extraction takes tens of seconds to minutes, so it runs detached
//! and never holds up the next archive.
use super::{
    client::HeliconClient,
    config::MemoryConfig,
    history::{to_messages, AdaptContext, HistorySource},
    log::Logger,
    session_store::SessionStore,
    strategy::{self, ArchiveRef, CaptureContext},
    transcript::extract_turns,
    util::now_ms,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle};

const TAG: &str = "[helicon-memory]";
const DEBOUNCE_MS: u64 = 300;

pub(crate) const SESSION_PREFIX: &str = "openworker";
pub(crate) fn to_helicon_session_id(session_id: &str) -> String {
    format!("{SESSION_PREFIX}/{session_id}")
}

#[derive(Default)]
struct LiveState {
    /// Archived turns' receipts by history index: extraction messages carry them as `archive_ref`.
    refs: HashMap<usize, ArchiveRef>,
    conflicts: HashMap<usize, String>,
    own_call_ids: HashSet<String>,
    hs_started: bool,
    /// Extraction in flight or done up to here (>= the saved `extract_next`); lowered when one fails.
    extracted_to: Option<usize>,
}

struct Live {
    timer: Mutex<Option<JoinHandle<()>>>,
    /// Held by the cycle loop: concurrent callers queue on it and find their work done.
    run_lock: tokio::sync::Mutex<()>,
    dirty: AtomicBool,
    state: Mutex<LiveState>,
    /// Extractions of one session go one after another, in the order they were made.
    extraction_lock: Arc<tokio::sync::Mutex<()>>,
    /// How many extractions are queued or running.
    pending: watch::Sender<usize>,
}

impl Live {
    fn new() -> Self {
        Self {
            timer: Mutex::new(None),
            run_lock: tokio::sync::Mutex::new(()),
            dirty: AtomicBool::new(false),
            state: Mutex::new(LiveState::default()),
            extraction_lock: Arc::new(tokio::sync::Mutex::new(())),
            pending: watch::channel(0).0,
        }
    }
}

pub(crate) struct CaptureDeps {
    pub cfg: Arc<MemoryConfig>,
    pub client: HeliconClient,
    pub store: Arc<Mutex<SessionStore>>,
    pub history: Arc<dyn HistorySource>,
    pub logger: Logger,
    /// The model-facing name prefix of the extension's own tools, e.g. `helicon__`.
    pub own_tool_prefix: &'static str,
}

pub(crate) struct CaptureEngine {
    d: CaptureDeps,
    live: Mutex<HashMap<String, Arc<Live>>>,
}

impl CaptureEngine {
    pub(crate) fn new(d: CaptureDeps) -> Arc<Self> {
        Arc::new(Self {
            d,
            live: Mutex::new(HashMap::new()),
        })
    }

    fn live_of(&self, session_id: &str) -> Arc<Live> {
        self.live
            .lock()
            .unwrap()
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(Live::new()))
            .clone()
    }

    /// A turn finished: look at the session's history soon (turns arriving together share one cycle).
    pub(crate) fn schedule(self: &Arc<Self>, session_id: &str) {
        if !self.d.cfg.auto_capture {
            return;
        }
        let live = self.live_of(session_id);
        live.dirty.store(true, Ordering::SeqCst);
        let engine = self.clone();
        let id = session_id.to_owned();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;
            // the cycle is its own task: rescheduling cancels the wait, never a cycle in progress
            tokio::spawn(async move { engine.run(&id).await });
        });
        let old = live.timer.lock().unwrap().replace(handle);
        if let Some(old) = old {
            old.abort();
        }
    }

    /// Runs cycles until nothing new is left; concurrent callers share the running one.
    pub(crate) async fn run(&self, session_id: &str) {
        let live = self.live_of(session_id);
        live.dirty.store(true, Ordering::SeqCst);
        let _running = live.run_lock.lock().await;
        // (a caller that queued behind a runner finds its dirty flag already taken)
        while live.dirty.swap(false, Ordering::SeqCst) {
            if let Err(err) = self.cycle(session_id, &live).await {
                self.d.logger.warn(&format!(
                    "{TAG} capture cycle failed session={session_id}: {err}"
                ));
            }
        }
    }

    /// Archives everything pending now and waits (bounded) for the extraction of it.
    pub(crate) async fn flush(&self, session_id: &str, wait_for_extraction_ms: u64) {
        let live = self.live_of(session_id);
        if let Some(timer) = live.timer.lock().unwrap().take() {
            timer.abort();
        }
        self.run(session_id).await;
        if wait_for_extraction_ms > 0 {
            let mut rx = live.pending.subscribe();
            let idle = async {
                let _ = rx.wait_for(|n| *n == 0).await;
            };
            let _ = tokio::time::timeout(Duration::from_millis(wait_for_extraction_ms), idle).await;
        }
    }

    /// Every session this process has captured for (shutdown flushes them).
    pub(crate) fn sessions(&self) -> Vec<String> {
        self.live.lock().unwrap().keys().cloned().collect()
    }

    /// Forgets the in-memory state of a session (its saved cursors stay for a later resume).
    pub(crate) fn release(&self, session_id: &str) {
        if let Some(live) = self.live.lock().unwrap().remove(session_id) {
            if let Some(timer) = live.timer.lock().unwrap().take() {
                timer.abort();
            }
        }
        self.d.store.lock().unwrap().forget(session_id);
    }

    async fn ensure_started(&self, session_id: &str, live: &Live) -> Result<(), String> {
        if live.state.lock().unwrap().hs_started {
            return Ok(());
        }
        let started_at = self
            .d
            .store
            .lock()
            .unwrap()
            .get(session_id)
            .started_at
            .clone();
        self.d
            .client
            .session_start(json!({
                "session_id": to_helicon_session_id(session_id),
                "agent_id": self.d.cfg.agent_id,
                "project_id": self.d.cfg.project_id,
                "conversation_id": session_id,
                "channel": "openworker",
                "session_datetime": started_at,
            }))
            .await
            .map_err(|e| e.describe())?;
        live.state.lock().unwrap().hs_started = true;
        Ok(())
    }

    async fn cycle(&self, session_id: &str, live: &Arc<Live>) -> Result<(), String> {
        let d = &self.d;
        let (extract_from, from) = {
            let mut store = d.store.lock().unwrap();
            let rec = store.get(session_id);
            let extract_from = rec
                .extract_next
                .max(live.state.lock().unwrap().extracted_to.unwrap_or(0));
            (extract_from, rec.archive_next.min(extract_from))
        };
        let page = d.history.read(session_id, from).await?;
        if page.next <= from {
            return Ok(());
        }
        let messages = {
            let mut state = live.state.lock().unwrap();
            let mut own = std::mem::take(&mut state.own_call_ids);
            let messages = to_messages(
                &page,
                &mut AdaptContext {
                    own_tool_prefix: d.own_tool_prefix,
                    own_call_ids: &mut own,
                    now: now_ms(),
                },
            );
            state.own_call_ids = own;
            messages
        };
        let hs_id = to_helicon_session_id(session_id);

        // ── 1. archive ──
        let archive_next = d.store.lock().unwrap().get(session_id).archive_next;
        if archive_next < page.next {
            let turns = extract_turns(
                &messages,
                archive_next,
                d.cfg.transcript_max_chars_per_turn,
                None,
            );
            if turns.is_empty() {
                // nothing archivable in this stretch (e.g. only our own tool traffic)
                let mut store = d.store.lock().unwrap();
                store.get(session_id).archive_next = page.next;
                store.save(session_id);
            } else {
                self.ensure_started(session_id, live).await?;
                let sent = turns.len();
                let answer = d
                    .client
                    .append_turns(json!({
                        "session_id": hs_id,
                        "agent_id": d.cfg.agent_id,
                        "project_id": d.cfg.project_id,
                        "conversation_id": session_id,
                        "turns": turns,
                    }))
                    .await
                    .map_err(|e| e.describe())?;
                let Some(results) = answer["results"].as_array() else {
                    return Err(format!(
                        "session/turn answered without per-entry receipts session={hs_id}"
                    ));
                };
                let mut retry_from: Option<usize> = None;
                {
                    let mut state = live.state.lock().unwrap();
                    for e in results {
                        let Some(index) = e["turn_index"].as_u64().map(|i| i as usize) else {
                            continue;
                        };
                        let status = e["sql_status"].as_str().unwrap_or("");
                        let turn_id = e["turn_id"].as_str().filter(|s| !s.is_empty());
                        let hash = e["content_hash"].as_str().filter(|s| !s.is_empty());
                        if let (true, Some(turn_id), Some(hash)) =
                            (matches!(status, "stored" | "replayed"), turn_id, hash)
                        {
                            state.refs.insert(
                                index,
                                ArchiveRef {
                                    turn_id: turn_id.to_owned(),
                                    content_hash: hash.to_owned(),
                                },
                            );
                        } else if status == "revision_conflict" {
                            state.refs.remove(&index);
                            let incoming = e["incoming_hash"].as_str().unwrap_or("").to_owned();
                            if state.conflicts.get(&index) != Some(&incoming) {
                                d.logger.warn(&format!(
                                    "{TAG} turn {index} conflicts with the archive (hs keeps {}, sent {}); kept as unresolved session={hs_id}",
                                    e["content_hash"].as_str().unwrap_or("").chars().take(8).collect::<String>(),
                                    incoming.chars().take(8).collect::<String>()
                                ));
                                state.conflicts.insert(index, incoming);
                            }
                        } else {
                            retry_from = Some(retry_from.map_or(index, |r| r.min(index)));
                        }
                    }
                }
                let next = retry_from.unwrap_or(page.next);
                {
                    let mut store = d.store.lock().unwrap();
                    store.get(session_id).archive_next = next;
                    store.save(session_id);
                }
                d.logger.info(&format!(
                    "{TAG} archived turns={sent} from={from} next={next} conflicts={} errors={} session={hs_id}",
                    answer["conflicts"].as_i64().unwrap_or(0),
                    answer["errors"].as_i64().unwrap_or(0)
                ));
            }
        }

        // ── 2. extraction of what is archived, detached ──
        let upto = d.store.lock().unwrap().get(session_id).archive_next;
        if extract_from < upto {
            live.state.lock().unwrap().extracted_to = Some(upto);
            let slice: Vec<Option<Value>> = messages.into_iter().take(upto).collect();
            self.spawn_extraction(session_id, live.clone(), slice, extract_from, upto);
        }
        Ok(())
    }

    fn spawn_extraction(
        &self,
        session_id: &str,
        live: Arc<Live>,
        messages: Vec<Option<Value>>,
        start_index: usize,
        upto: usize,
    ) {
        let d = &self.d;
        let (cfg, client, logger, store) = (
            d.cfg.clone(),
            d.client.clone(),
            d.logger.clone(),
            d.store.clone(),
        );
        let id = session_id.to_owned();
        live.pending.send_modify(|n| *n += 1);
        let lock = live.extraction_lock.clone();
        tokio::spawn(async move {
            {
                let _turn = lock.lock().await;
                extract(
                    &cfg,
                    &client,
                    &logger,
                    &store,
                    &live,
                    &id,
                    &messages,
                    start_index,
                    upto,
                )
                .await;
            }
            live.pending.send_modify(|n| *n = n.saturating_sub(1));
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn extract(
    cfg: &MemoryConfig,
    client: &HeliconClient,
    logger: &Logger,
    store: &Mutex<SessionStore>,
    live: &Live,
    session_id: &str,
    messages: &[Option<Value>],
    start_index: usize,
    upto: usize,
) {
    let failed = |why: &str| {
        let mut state = live.state.lock().unwrap();
        state.extracted_to = Some(state.extracted_to.unwrap_or(start_index).min(start_index));
        logger.warn(&format!(
            "{TAG} extraction will be retried from {start_index} ({why}) session={session_id}"
        ));
    };
    let started_at = store.lock().unwrap().get(session_id).started_at.clone();
    let refs = live.state.lock().unwrap().refs.clone();
    let hs_id = to_helicon_session_id(session_id);
    let report = strategy::capture(
        client,
        logger,
        &CaptureContext {
            messages,
            start_index,
            session_id: &hs_id,
            cfg,
            session_started_at: &started_at,
            archive_refs: &refs,
        },
    )
    .await;
    logger.info(&format!(
        "{TAG} capture done mode={} considered={} stored={} dedupe={} skipped={} errors={} session={session_id}",
        strategy::name(cfg.capture_mode),
        report.considered,
        report.stored,
        report.duplicates,
        report.skipped,
        report.errors
    ));
    if report.errors > 0 {
        failed(&format!("errors={}", report.errors));
        return;
    }
    // The saved cursor is the end of what is extracted without a gap. When an earlier stretch failed
    // (its extraction is retried from `extracted_to`), this one does not move it past that stretch.
    let mut store = store.lock().unwrap();
    let rec = store.get(session_id);
    if start_index <= rec.extract_next {
        rec.extract_next = rec.extract_next.max(upto);
        store.save(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helicon::{
        history::HistoryPage,
        test_support::{MockHs, Reply},
    };
    use futures_util::future::BoxFuture;

    /// A history the test controls.
    struct FakeHistory {
        items: Mutex<Vec<(usize, Value)>>,
    }

    impl HistorySource for FakeHistory {
        fn read<'a>(
            &'a self,
            _session_id: &'a str,
            after: usize,
        ) -> BoxFuture<'a, Result<HistoryPage, String>> {
            Box::pin(async move {
                let items = self.items.lock().unwrap();
                Ok(HistoryPage {
                    items: items.iter().filter(|(i, _)| *i >= after).cloned().collect(),
                    next: items.len().max(after),
                })
            })
        }
    }

    fn setup(
        hs: &MockHs,
        mode: &str,
        history: Vec<(usize, Value)>,
    ) -> (
        Arc<CaptureEngine>,
        Arc<Mutex<SessionStore>>,
        tempfile::TempDir,
        Arc<FakeHistory>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Arc::new(
            MemoryConfig::parse(
                &json!({"baseUrl": hs.url(), "apiKey": "hs_test", "captureMode": mode, "agentId": "a1", "projectId": "p1"}),
                &|_| None,
            )
            .unwrap(),
        );
        let logger = Logger::new(dir.path().join("log"), false);
        let store = Arc::new(Mutex::new(SessionStore::new(dir.path().to_path_buf())));
        let history = Arc::new(FakeHistory {
            items: Mutex::new(history),
        });
        let engine = CaptureEngine::new(CaptureDeps {
            cfg: cfg.clone(),
            client: HeliconClient::new(&cfg.base_url, &cfg.api_key, 2_000, logger.clone(), false),
            store: store.clone(),
            history: history.clone(),
            logger,
            own_tool_prefix: "helicon__",
        });
        (engine, store, dir, history)
    }

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": text})
    }

    fn assistant(text: &str) -> Value {
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]})
    }

    /// An HS that archives every turn it is sent and answers `add` as queued.
    async fn archiving_hs() -> MockHs {
        MockHs::start(|req| match req.path.as_str() {
            "/v1/memory/session/start" => Reply::ok(json!({"status": "started"})),
            "/v1/memory/session/turn" => {
                let results: Vec<Value> = req.body["turns"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| {
                        json!({
                            "turn_index": t["turn_index"], "sql_status": "stored",
                            "turn_id": format!("t{}", t["turn_index"]),
                            "content_hash": "unused",
                        })
                    })
                    .collect();
                Reply::ok(json!({"archived": results.len(), "results": results}))
            }
            "/v1/memory/add" => {
                Reply::ok(json!({"code": 202, "queued": true, "work_unit_id": "w1"}))
            }
            other => panic!("unexpected {other}"),
        })
        .await
    }

    #[tokio::test]
    async fn a_cycle_archives_then_extracts_and_moves_both_cursors() {
        let hs = archiving_hs().await;
        let (engine, store, _dir, _history) = setup(
            &hs,
            "operator",
            vec![
                (0, user("I prefer dark mode")),
                (1, assistant("Noted.")),
                (2, json!({"type": "reasoning"})),
            ],
        );
        engine.flush("s1", 5_000).await;
        let reqs = hs.requests();
        let paths: Vec<&str> = reqs.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/v1/memory/session/start",
                "/v1/memory/session/turn",
                "/v1/memory/add"
            ]
        );
        let turn = &reqs[1].body;
        assert_eq!(turn["session_id"], "openworker/s1");
        assert_eq!(turn["conversation_id"], "s1");
        assert_eq!(turn["turns"].as_array().unwrap().len(), 2);
        assert_eq!(turn["turns"][0]["content"], "I prefer dark mode");
        let add = &reqs[2].body;
        assert_eq!(add["session_id"], "openworker/s1");
        assert_eq!(add["messages"].as_array().unwrap().len(), 2);
        assert_eq!(add["messages"][0]["turn_index"], 0);
        assert_eq!(
            add["messages"][0]["archive_ref"],
            Value::Null,
            "the hash is not the real one: no ref"
        );
        let mut store = store.lock().unwrap();
        let rec = store.get("s1");
        assert_eq!((rec.archive_next, rec.extract_next), (3, 3));
    }

    #[tokio::test]
    async fn a_second_cycle_sends_only_what_is_new() {
        let hs = archiving_hs().await;
        let (engine, _store, _dir, history) =
            setup(&hs, "operator", vec![(0, user("I prefer dark mode"))]);
        engine.flush("s1", 5_000).await;
        history.items.lock().unwrap().push((1, assistant("ok")));
        history.items.lock().unwrap().push((2, user("I like tea")));
        engine.flush("s1", 5_000).await;
        let turns: Vec<Value> = hs
            .requests()
            .into_iter()
            .filter(|r| r.path == "/v1/memory/session/turn")
            .map(|r| r.body)
            .collect();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1]["turns"][0]["turn_index"], 1);
        assert_eq!(turns[1]["turns"].as_array().unwrap().len(), 2);
        // the session was started once
        assert_eq!(
            hs.requests()
                .iter()
                .filter(|r| r.path == "/v1/memory/session/start")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_failed_archive_is_resent_by_the_next_cycle() {
        let calls = Arc::new(Mutex::new(0));
        let c = calls.clone();
        let hs = MockHs::start(move |req| match req.path.as_str() {
            "/v1/memory/session/start" => Reply::ok(json!({})),
            "/v1/memory/session/turn" => {
                let mut n = c.lock().unwrap();
                *n += 1;
                let index = req.body["turns"][0]["turn_index"].clone();
                if *n == 1 {
                    Reply::ok(json!({"results": [{"turn_index": index, "sql_status": "error"}]}))
                } else {
                    Reply::ok(json!({"results": [{"turn_index": index, "sql_status": "stored", "turn_id": "t", "content_hash": "h"}]}))
                }
            }
            "/v1/memory/add" => Reply::ok(json!({"code": 202, "queued": true, "work_unit_id": "w"})),
            other => panic!("unexpected {other}"),
        })
        .await;
        let (engine, store, _dir, _h) =
            setup(&hs, "operator", vec![(0, user("I prefer dark mode"))]);
        engine.flush("s1", 0).await;
        assert_eq!(
            store.lock().unwrap().get("s1").archive_next,
            0,
            "not archived: the cursor stays"
        );
        engine.flush("s1", 5_000).await;
        assert_eq!(store.lock().unwrap().get("s1").archive_next, 1);
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn direct_mode_stores_only_the_sentences_that_pass_the_rules() {
        let hs = MockHs::start(|req| match req.path.as_str() {
            "/v1/memory/session/start" => Reply::ok(json!({})),
            "/v1/memory/session/turn" => {
                let r: Vec<Value> = req.body["turns"].as_array().unwrap().iter()
                    .map(|t| json!({"turn_index": t["turn_index"], "sql_status": "stored", "turn_id": "t", "content_hash": "h"}))
                    .collect();
                Reply::ok(json!({"results": r}))
            }
            "/v1/memory/store" => Reply::ok(json!({"memory_id": "m1", "action": "created"})),
            other => panic!("unexpected {other}"),
        })
        .await;
        let (engine, _s, _d, _h) = setup(
            &hs,
            "direct",
            vec![
                (0, user("I prefer dark mode in every editor")),
                (1, assistant("Sure.")),
                (2, user("what is the time?")),
            ],
        );
        engine.flush("s1", 5_000).await;
        let stores: Vec<Value> = hs
            .requests()
            .into_iter()
            .filter(|r| r.path == "/v1/memory/store")
            .map(|r| r.body)
            .collect();
        assert_eq!(stores.len(), 1);
        assert_eq!(stores[0]["text"], "I prefer dark mode in every editor");
        assert_eq!(stores[0]["category"], "preference");
        assert_eq!(stores[0]["source_refs"][0]["ref_id"], "openworker/s1");
    }

    #[tokio::test]
    async fn the_extensions_own_tool_calls_are_not_archived() {
        let hs = archiving_hs().await;
        let (engine, _s, _d, _h) = setup(
            &hs,
            "operator",
            vec![
                (0, user("remember my name is Kai")),
                (
                    1,
                    json!({"type": "function_call", "call_id": "c1", "name": "helicon__memory_recall", "arguments": "{}"}),
                ),
                (
                    2,
                    json!({"type": "function_call_output", "call_id": "c1", "output": "Found 1 memories"}),
                ),
                (3, assistant("Hello Kai")),
            ],
        );
        engine.flush("s1", 5_000).await;
        let turn = hs
            .requests()
            .into_iter()
            .find(|r| r.path == "/v1/memory/session/turn")
            .unwrap()
            .body;
        let indexes: Vec<i64> = turn["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["turn_index"].as_i64().unwrap())
            .collect();
        assert_eq!(indexes, vec![0, 3]);
    }

    #[tokio::test]
    async fn scheduling_debounces_into_one_cycle() {
        let hs = archiving_hs().await;
        let (engine, _s, _d, _h) = setup(&hs, "operator", vec![(0, user("I prefer dark mode"))]);
        for _ in 0..5 {
            engine.schedule("s1");
        }
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert_eq!(
            hs.requests()
                .iter()
                .filter(|r| r.path == "/v1/memory/session/turn")
                .count(),
            1
        );
        assert_eq!(engine.sessions(), vec!["s1".to_owned()]);
        engine.release("s1");
        assert!(engine.sessions().is_empty());
    }
}
