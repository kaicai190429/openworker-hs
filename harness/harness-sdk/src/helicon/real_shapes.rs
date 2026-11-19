//! The extension against answers HeliconSearch really gave: `tests/fixtures/helicon/*.json` were recorded
//! from a running server (with a throw-away agent id; the bridge's texts are replaced by placeholders).
//! The fake servers of the other tests answer what their author expects; these answer what the server does,
//! so a change of the server's shapes that the code does not follow shows up here.
use super::{
    budget::{BudgetSource, RecallBudgets},
    capture::{CaptureDeps, CaptureEngine},
    client::{CallOptions, HeliconClient},
    config::MemoryConfig,
    log::Logger,
    session_store::SessionStore,
    test_support::{FakeHistory, MockHs, Reply},
    tools::Tools,
    Inner,
};
use crate::extension::{SessionInfo, TurnContext};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn fixture(name: &str) -> Value {
    let text = match name {
        "query_full" => include_str!("../../tests/fixtures/helicon/query_full.json"),
        "query_none" => include_str!("../../tests/fixtures/helicon/query_none.json"),
        "explain" => include_str!("../../tests/fixtures/helicon/explain.json"),
        "source" => include_str!("../../tests/fixtures/helicon/source.json"),
        "session_turns" => include_str!("../../tests/fixtures/helicon/session_turns.json"),
        "session_turn_answer" => {
            include_str!("../../tests/fixtures/helicon/session_turn_answer.json")
        }
        "add_answer" => include_str!("../../tests/fixtures/helicon/add_answer.json"),
        "health" => include_str!("../../tests/fixtures/helicon/health.json"),
        "bridge" => include_str!("../../tests/fixtures/helicon/session_bridge_scrubbed.json"),
        other => panic!("no fixture {other}"),
    };
    serde_json::from_str(text).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

/// A server that answers each endpoint with the recorded answer.
async fn recorded_server() -> MockHs {
    MockHs::start(|req| {
        let path = req.path.split('?').next().unwrap();
        match path {
            "/v1/memory/health" => Reply::ok(fixture("health")),
            "/v1/memory/query" => Reply::ok(fixture(if req.body["render"] == "none" {
                "query_none"
            } else {
                "query_full"
            })),
            "/v1/memory/session_bridge" => Reply::ok(fixture("bridge")),
            "/v1/memory/explain" => Reply::ok(fixture("explain")),
            "/v1/memory/session/turns" => Reply::ok(fixture("session_turns")),
            "/v1/memory/session/start" => {
                Reply::ok(json!({"code": 200, "data": {"status": "open"}}))
            }
            "/v1/memory/session/turn" => Reply::ok(fixture("session_turn_answer")),
            "/v1/memory/add" => Reply::ok(fixture("add_answer")),
            p if p.starts_with("/v1/memory/source/") => Reply::ok(fixture("source")),
            other => panic!("unexpected {other}"),
        }
    })
    .await
}

fn config(hs: &MockHs, extra: Value) -> Arc<MemoryConfig> {
    let mut raw = json!({"baseUrl": hs.url(), "apiKey": "hs_test", "agentId": "fixture-agent"});
    for (k, v) in extra.as_object().unwrap() {
        raw[k] = v.clone();
    }
    Arc::new(MemoryConfig::parse(&raw, &|_| None).unwrap())
}

#[test]
fn the_recorded_answers_have_the_shapes_the_code_relies_on() {
    // (if the server changes one of these, this says which, before anything else fails confusingly)
    let query = fixture("query_full");
    assert_eq!(query["prompt_blocks"]["protocol"], "blocks.v1");
    let joined: String = query["prompt_blocks"]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        joined,
        query["prompt_text"].as_str().unwrap(),
        "the blocks join to exactly prompt_text"
    );
    assert!(query["execution"]["config_fingerprint"].is_string());
    let facts = fixture("query_none");
    assert!(facts["grounded_facts"].is_array() && facts["topic_rollups"].is_array());
    let explain = &fixture("explain")["data"];
    assert!(explain["memory"]["id"].is_string() && explain["source_completeness"].is_string());
    let source = &fixture("source")["data"];
    assert!(
        source["turns"][0]["session_id"].is_string() && source["turns"][0]["turn_index"].is_i64()
    );
    assert!(fixture("session_turn_answer")["data"]["results"][0]["content_hash"].is_string());
    assert_eq!(fixture("bridge")["data"]["protocol"], "bridge.v2");
}

#[tokio::test]
async fn the_servers_recall_budgets_are_read_from_its_health() {
    let hs = recorded_server().await;
    let cfg = config(&hs, json!({}));
    let logger = Logger::new(std::env::temp_dir().join("helicon-real-shapes.log"), false);
    let client = HeliconClient::new(&cfg.base_url, &cfg.api_key, 5_000, logger.clone(), false);
    let budgets = RecallBudgets::new(&cfg.base_url, 15_000, logger);
    budgets.learn_from_health(Some(&client.health().await.unwrap()));
    let current = budgets.current();
    assert_eq!(current.source, BudgetSource::Health);
    assert!(
        current.auto_ms > 0 && current.active_ms >= current.auto_ms,
        "{current:?}"
    );
}

#[tokio::test]
async fn a_turn_is_recalled_into_from_the_real_organized_answer_and_the_real_bridge() {
    let hs = recorded_server().await;
    let dir = tempfile::tempdir().unwrap();
    let inner = Inner::new(config(&hs, json!({})), dir.path());
    let session = SessionInfo {
        session_id: "real-1".into(),
        agent: "lead".into(),
    };
    inner.session_created(&session);
    let addendum = inner
        .prepare_turn(&TurnContext {
            session: &session,
            submission_id: "s1",
            text: "What is my favorite tea?",
            addendum: None,
        })
        .await
        .expect("an addendum from what the server said");
    // the first turn: the summary of earlier sessions (real bridge), then the recall (real organized answer)
    assert!(addendum.starts_with("<relevant-memories>"), "{addendum}");
    assert!(addendum.contains("## 跨会话记忆"), "{addendum}");
    assert!(
        addendum.contains("jasmine green tea"),
        "the recalled fact is in it: {addendum}"
    );
    assert_eq!(
        addendum.matches("</relevant-memories>").count(),
        2,
        "{addendum}"
    );
    assert!(addendum.len() <= 32_000 && !addendum.contains('\0'));
    // the next turn: only the recall
    let second = inner
        .prepare_turn(&TurnContext {
            session: &session,
            submission_id: "s2",
            text: "And with sugar?",
            addendum: None,
        })
        .await
        .unwrap();
    assert!(
        !second.contains("## 跨会话记忆") && second.contains("jasmine green tea"),
        "{second}"
    );
}

#[tokio::test]
async fn the_memory_tools_read_the_real_answers() {
    let hs = recorded_server().await;
    let cfg = config(&hs, json!({}));
    let logger = Logger::new(std::env::temp_dir().join("helicon-real-shapes.log"), false);
    let client = HeliconClient::new(&cfg.base_url, &cfg.api_key, 5_000, logger.clone(), false);
    let budgets = RecallBudgets::new(&cfg.base_url, 15_000, logger.clone());
    let tools = Tools::new(cfg.clone(), client, logger, budgets);

    let found = tools
        .memory_recall(&json!({"query": "what is my favorite tea"}), "real#1")
        .await;
    assert!(found.contains("jasmine green tea"), "{found}");
    assert!(!found.contains("failed"), "{found}");

    let id = fixture("explain")["data"]["memory"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let opened = tools.memory_get(&json!({"memoryId": id}), "real#2").await;
    assert!(
        opened.starts_with(&format!("**Memory** id={id}\n")),
        "{opened}"
    );
    assert!(
        opened.contains("Source sessions: openworker/fixture-capture-"),
        "{opened}"
    );
    // the source message of the memory is found in the conversation and marked
    assert!(opened.contains("◀ source of this memory"), "{opened}");
    assert!(
        opened.contains("My favorite tea is jasmine green tea"),
        "{opened}"
    );
}

#[tokio::test]
async fn a_conversation_is_archived_and_extracted_against_the_real_answers() {
    let hs = recorded_server().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&hs, json!({"captureMode": "operator"}));
    let logger = Logger::new(dir.path().join("log"), false);
    let store = Arc::new(Mutex::new(SessionStore::new(
        dir.path().to_path_buf(),
        logger.clone(),
    )));
    let engine = CaptureEngine::new(CaptureDeps {
        cfg: cfg.clone(),
        client: HeliconClient::new(&cfg.base_url, &cfg.api_key, 5_000, logger.clone(), false),
        store: store.clone(),
        history: Arc::new(FakeHistory::new(vec![
            (
                0,
                json!({"role": "user", "content": "My favorite tea is jasmine green tea, and I take it without sugar."}),
            ),
            (
                1,
                json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Noted: jasmine green tea, no sugar."}]}),
            ),
        ])),
        logger,
        own_tool_prefix: "helicon__",
    });
    assert!(engine.flush("real-1", 10_000).await);
    let mut store = store.lock().unwrap();
    let rec = store.get("real-1");
    assert_eq!(
        (rec.archive_next, rec.extract_next),
        (2, 2),
        "archived and extracted"
    );
    let add = hs
        .requests()
        .into_iter()
        .find(|r| r.path == "/v1/memory/add")
        .unwrap()
        .body;
    // the first message is the one the server archived (same text, so the same hash): it is tied to its turn
    let receipt = &fixture("session_turn_answer")["data"]["results"][0];
    assert_eq!(
        add["messages"][0]["archive_ref"]["turn_id"], receipt["turn_id"],
        "{add}"
    );
    assert_eq!(
        add["messages"][0]["archive_ref"]["content_hash"],
        receipt["content_hash"]
    );
}

#[tokio::test]
async fn the_real_operator_answer_is_understood() {
    let hs = recorded_server().await;
    let cfg = config(&hs, json!({}));
    let logger = Logger::new(std::env::temp_dir().join("helicon-real-shapes.log"), false);
    let client = HeliconClient::new(&cfg.base_url, &cfg.api_key, 5_000, logger, false);
    let added = client
        .add(json!({"messages": [], "session_id": "s"}))
        .await
        .unwrap();
    assert!(!added.queued && !added.already_processed);
    assert!(
        added
            .operations
            .iter()
            .any(|op| op["action"] == "ADD" && op["memory_id"].is_string()),
        "{added:?}"
    );
    assert!(added.processing_state.is_some(), "{added:?}");
    // and the query path takes the recorded answer whole
    let answer = client
        .query(
            json!({"query": "q", "render": "full"}),
            &CallOptions::default(),
        )
        .await
        .unwrap();
    assert!(answer["grounded_facts"].is_array());
}
