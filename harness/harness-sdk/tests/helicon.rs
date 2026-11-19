#![cfg(feature = "test-support")]
//! The HeliconSearch memory extension over real engine runs (scripted model, real journal, real
//! history, application tools) against a fake HeliconSearch. No API keys, no network beyond loopback.
//!
//! The extension is switched on by `helicon-memory.json` in the state directory, exactly as an
//! application would.
#[path = "support/hs_mock.rs"]
mod hs_mock;
#[path = "support/tool.rs"]
mod tool;
use hs_mock::{MockHs, Reply, Request};
use openworker_sdk::agents::*;
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

const MEMORY_ID: &str = "0123456789abcdef0123456789abcdef";

fn runner() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_sdk-test-host"))
}

fn workspace(root: &std::path::Path) -> PathBuf {
    let w = root.join("work");
    std::fs::create_dir_all(&w).unwrap();
    w
}

/// A HeliconSearch that remembers a fact, archives what it is sent and ends sessions.
async fn hs() -> MockHs {
    MockHs::start(|req| match req.path.split('?').next().unwrap() {
        "/v1/memory/health" => Reply::ok(json!({"overall": true})),
        "/v1/memory/query" => {
            let text = "- likes tea\n";
            Reply::ok(json!({
                "grounded_facts": [{"id": "pkg:1", "claim": "likes tea", "score": 0.9,
                    "support_level": "grounded", "source_memory_ids": [MEMORY_ID]}],
                "prompt_text": text,
                "prompt_blocks": {"protocol": "blocks.v1", "blocks": [
                    {"text": text, "ref": {"type": "memory", "memory_id": MEMORY_ID, "version": "v1", "extras": []}}
                ]},
            }))
        }
        "/v1/memory/session_bridge" => Reply::ok(json!({
            "protocol": "bridge.v2", "is_empty": false, "conflict_limit": 3, "conflicts": [],
            "recent_episodes": [], "category_summaries": [], "procedures": [],
            "pinned_semantics": [{"id": "pin1", "text": "lives in Oslo", "category": "fact",
                "version": "v1", "conflict_ids": []}],
        })),
        "/v1/memory/session/start" => Reply::ok(json!({"status": "started"})),
        "/v1/memory/session/turn" => {
            let results: Vec<Value> = req.body["turns"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| {
                    json!({"turn_index": t["turn_index"], "sql_status": "stored",
                        "turn_id": format!("t{}", t["turn_index"]), "content_hash": "h"})
                })
                .collect();
            Reply::ok(json!({"archived": results.len(), "results": results}))
        }
        "/v1/memory/add" => Reply::ok(json!({"code": 202, "queued": true, "work_unit_id": "w1"})),
        "/v1/memory/session/end" => Reply::ok(json!({"status": "closed", "memory_id": "summary1"})),
        other => panic!("unexpected request to the fake HeliconSearch: {other}"),
    })
    .await
}

/// Writes the extension's configuration into the state directory.
fn configure(root: &std::path::Path, hs: &MockHs, extra: Value) {
    let mut cfg = json!({
        "baseUrl": hs.url(), "apiKey": "hs_test", "agentId": "tester", "projectId": "proj",
        "captureMode": "operator", "closeWaitMs": 5000,
    });
    for (k, v) in extra.as_object().unwrap() {
        cfg[k] = v.clone();
    }
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("helicon-memory.json"), cfg.to_string()).unwrap();
}

async fn start(root: &std::path::Path) -> Harness {
    Harness::builder()
        .state_dir(root.join("state"))
        .executable(runner())
        .tools(tool::registry())
        .sandbox(Sandbox::Off)
        .scripted(true)
        .embedded()
        .await
        .unwrap()
}

/// What the scripted model needs to answer with text and no tool call (any other text makes it
/// write an example file and ask for approval).
const SAY: &str = "TEST_RESOURCE_GRANTS []";

fn agent() -> Agent {
    Agent::new("assistant")
        .instructions("Be helpful.")
        .memory(true)
        .approvals(Approvals::Never)
}

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), f)
        .await
        .unwrap()
}

/// Waits until the fake HeliconSearch has seen a request on `path`.
async fn seen(hs: &MockHs, path: &str) -> Request {
    for _ in 0..200 {
        if let Some(r) = hs
            .requests()
            .into_iter()
            .find(|r| r.path.split('?').next() == Some(path))
        {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "no request to {path}; saw {:?}",
        hs.requests()
            .iter()
            .map(|r| r.path.clone())
            .collect::<Vec<_>>()
    );
}

fn all(hs: &MockHs, path: &str) -> Vec<Request> {
    hs.requests()
        .into_iter()
        .filter(|r| r.path.split('?').next() == Some(path))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_configuration_nothing_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let h = start(&root).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    let r = within(s.run(SAY)).await.unwrap();
    assert_eq!(r.status, RunStatus::Completed);
    s.close().await.unwrap();
    h.shutdown().await.unwrap();
    assert!(
        !root.join("state/helicon-memory").exists(),
        "no adapter state, no log"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_configuration_is_off() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(&root, &hs, json!({"enabled": false}));
    let h = start(&root).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    within(s.run(SAY)).await.unwrap();
    s.close().await.unwrap();
    h.shutdown().await.unwrap();
    assert!(hs.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conversation_is_recalled_archived_extracted_and_ended() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(&root, &hs, json!({}));
    let h = start(&root).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();

    let r = within(s.run(SAY)).await.unwrap();
    assert_eq!(r.status, RunStatus::Completed);

    // before the turn: the cross-session bridge (new session) and a recall of the user's message
    let bridge = seen(&hs, "/v1/memory/session_bridge").await;
    assert!(bridge.path.contains("agent_id=tester") && bridge.path.contains("project_id=proj"));
    assert_eq!(bridge.auth.as_deref(), Some("Bearer hs_test"));
    let query = seen(&hs, "/v1/memory/query").await;
    assert_eq!(query.body["query"], SAY);
    assert_eq!(query.body["purpose"], "auto");
    assert_eq!(query.body["agent_id"], "tester");

    // after it: the history is archived (both sides, verbatim), then extracted
    let start_req = seen(&hs, "/v1/memory/session/start").await;
    let hs_session = start_req.body["session_id"].as_str().unwrap().to_owned();
    assert_eq!(hs_session, format!("openworker/{}", s.id()));
    assert_eq!(start_req.body["conversation_id"], s.id());
    let turns = seen(&hs, "/v1/memory/session/turn").await;
    let archived = turns.body["turns"].as_array().unwrap();
    assert_eq!(archived[0]["role"], "user");
    assert_eq!(archived[0]["content"], SAY);
    assert_eq!(archived[0]["turn_index"], 0);
    assert!(archived.iter().any(|t| t["role"] == "assistant"));
    let add = seen(&hs, "/v1/memory/add").await;
    assert_eq!(add.body["session_id"], hs_session);
    assert_eq!(add.body["messages"][0]["content"], SAY);

    // a second turn: no bridge again, a new recall, and only the new part is archived
    within(s.run(SAY)).await.unwrap();
    for _ in 0..200 {
        if all(&hs, "/v1/memory/session/turn").len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        all(&hs, "/v1/memory/session_bridge").len(),
        1,
        "the bridge is for the first turn only"
    );
    assert_eq!(all(&hs, "/v1/memory/query").len(), 2);
    let second = &all(&hs, "/v1/memory/session/turn")[1];
    assert_eq!(second.body["turns"][0]["content"], SAY);
    assert!(second.body["turns"][0]["turn_index"].as_i64().unwrap() > 0);

    // closing the session ends it on HeliconSearch, with the number of archived messages
    within(s.close()).await.unwrap();
    let end = seen(&hs, "/v1/memory/session/end").await;
    assert_eq!(end.body["session_id"], hs_session);
    assert_eq!(end.body["agent_id"], "tester");
    assert!(end.body["message_count"].as_i64().unwrap() >= 4);
    within(h.shutdown()).await.unwrap();

    // the cursors are saved: a later run would send only what is new
    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(
            root.join(format!("state/helicon-memory/sessions/{}.json", s.id())),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(saved["archiveNext"].as_i64().unwrap() >= 4);
    assert_eq!(saved["ended"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_model_can_call_the_memory_tools_and_their_traffic_is_not_archived() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(
        &root,
        &hs,
        json!({"autoRecall": false, "sessionBridge": false}),
    );
    let h = start(&root).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();

    let plan = format!(
        "TEST_RESOURCE_GRANTS {}",
        json!([{"name": "helicon__memory_recall", "arguments": {"query": "what tea do I like"}}])
    );
    let r = within(s.run(&plan)).await.unwrap();
    assert_eq!(r.status, RunStatus::Completed);

    // the tool searched with the active purpose, and answered the model with the fact and its whole id
    let query = seen(&hs, "/v1/memory/query").await;
    assert_eq!(query.body["purpose"], "active");
    assert_eq!(query.body["render"], "none");
    assert_eq!(query.body["query"], "what tea do I like");
    let history = h
        .client()
        .request("history/read", json!({"session_id": s.id(), "after": 0}))
        .await
        .unwrap();
    let output = history["items"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|i| {
            (i["item"]["type"] == "function_call_output").then(|| i["item"]["output"].clone())
        })
        .expect("the tool's answer is in the history");
    let output = output
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| output.to_string());
    assert!(output.contains("Found 1 memories"), "{output}");
    assert!(output.contains(&format!("id={MEMORY_ID}")), "{output}");

    // what is archived is the conversation, not the extension's own tool traffic
    let turns = seen(&hs, "/v1/memory/session/turn").await;
    let archived = turns.body["turns"].as_array().unwrap();
    let roles: Vec<(i64, &str)> = archived
        .iter()
        .map(|t| {
            (
                t["turn_index"].as_i64().unwrap(),
                t["role"].as_str().unwrap(),
            )
        })
        .collect();
    // history: 0 user, 1 call, 2 result, 3 assistant — the call and its result are ours and are left out
    assert_eq!(roles, vec![(0, "user"), (3, "assistant")]);
    assert!(!turns.body.to_string().contains("Found 1 memories"));
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutting_the_harness_down_ends_the_sessions_still_open() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(
        &root,
        &hs,
        json!({"autoRecall": false, "sessionBridge": false}),
    );
    let h = start(&root).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    within(s.run(SAY)).await.unwrap();
    within(h.shutdown()).await.unwrap();
    // nothing closed the session: shutdown archived what was pending and ended it
    let end = seen(&hs, "/v1/memory/session/end").await;
    assert_eq!(end.body["session_id"], format!("openworker/{}", s.id()));
    assert!(!all(&hs, "/v1/memory/session/turn").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_heliconsearch_never_fails_a_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    // nothing listens on this port
    std::fs::write(
        state.join("helicon-memory.json"),
        json!({"baseUrl": "http://127.0.0.1:9", "apiKey": "k", "timeoutMs": 1000, "closeWaitMs": 0}).to_string(),
    )
    .unwrap();
    let h = start(&root).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    let r = within(s.run(SAY)).await.unwrap();
    assert_eq!(r.status, RunStatus::Completed);
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
}
