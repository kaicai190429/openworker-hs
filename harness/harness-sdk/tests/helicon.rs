#![cfg(feature = "test-support")]
//! The HeliconSearch memory extension over real engine runs (scripted model, real journal, real
//! history, application tools) against a fake HeliconSearch. No API keys, no network beyond loopback.
//!
//! The extension is switched on by `helicon-memory.json` in the state directory, exactly as an
//! application would.
#[path = "support/helicon_fixture.rs"]
mod helicon_fixture;
#[path = "support/hs_mock.rs"]
mod hs_mock;
#[path = "support/tool.rs"]
mod tool;
use helicon_fixture::*;
use hs_mock::MockHs;
use openworker_sdk::{agents::*, ConnectionState, Error};
use serde_json::{json, Value};
use std::time::Duration;

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
    configure(&root, &hs, json!({"recallPlacement": "system"}));
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
        json!({"baseUrl": hs_mock::nothing_listens(), "apiKey": "k", "timeoutMs": 1000, "closeWaitMs": 0}).to_string(),
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

/// The engine in an SDK-owned child process: the extension lives in this process either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn it_works_the_same_with_the_engine_in_a_child_process() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(
        &root,
        &hs,
        json!({"sessionBridge": false, "recallPlacement": "system"}),
    );
    let h = Harness::builder()
        .state_dir(root.join("state"))
        .executable(runner())
        .tools(tool::registry())
        .sandbox(Sandbox::Off)
        .scripted(true)
        .managed(runner(), openworker_sdk::RestartPolicy::default())
        .await
        .unwrap();
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    let r = within(s.run(SAY)).await.unwrap();
    assert_eq!(r.status, RunStatus::Completed);
    let query = seen(&hs, "/v1/memory/query").await;
    assert_eq!(query.body["query"], SAY);
    let turns = seen(&hs, "/v1/memory/session/turn").await;
    assert_eq!(turns.body["turns"][0]["content"], SAY);
    within(s.close()).await.unwrap();
    seen(&hs, "/v1/memory/session/end").await;
    within(h.shutdown()).await.unwrap();
}

fn ends(hs: &MockHs) -> usize {
    all(hs, "/v1/memory/session/end").len()
}

async fn until<F: Fn() -> bool>(what: &str, seconds: u64, done: F) {
    for _ in 0..seconds * 20 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("gave up waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_session_is_ended_without_close_and_ended_again_after_it_is_used_again() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(
        &root,
        &hs,
        json!({"autoRecall": false, "sessionBridge": false, "idleEndMs": 3000}),
    );
    let h = start(&root).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    within(s.run(SAY)).await.unwrap();
    // nobody closes it: the idle sweep does, once
    until("the idle end", 15, || ends(&hs) == 1).await;
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(
        ends(&hs),
        1,
        "an ended session is not ended again while it stays quiet"
    );
    // used again: archived into the same HeliconSearch session, ended again when quiet again
    within(s.run(SAY)).await.unwrap();
    until("the second end", 15, || ends(&hs) == 2).await;
    let sessions: std::collections::BTreeSet<String> = all(&hs, "/v1/memory/session/end")
        .iter()
        .map(|r| r.body["session_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(sessions.len(), 1, "both ends are of the one session");
    let last_end = &all(&hs, "/v1/memory/session/end")[1];
    assert!(
        last_end.body["message_count"].as_i64().unwrap() >= 4,
        "{}",
        last_end.body
    );
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_an_earlier_run_left_open_is_ended_at_the_next_start() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    // the first run: endOnShutdown off, so shutting down leaves the session open (as a killed run would)
    configure(
        &root,
        &hs,
        json!({"autoRecall": false, "sessionBridge": false, "idleEndMs": 600000, "endOnShutdown": false}),
    );
    let h = start(&root).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    within(s.run(SAY)).await.unwrap();
    seen(&hs, "/v1/memory/session/turn").await;
    within(h.shutdown()).await.unwrap();
    assert_eq!(ends(&hs), 0, "the first run ended nothing");

    // a later run on the same state: the orphan is found, caught up and ended
    configure(
        &root,
        &hs,
        json!({"autoRecall": false, "sessionBridge": false, "idleEndMs": 1000}),
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let h = start(&root).await;
    until("the orphan's end", 15, || ends(&hs) == 1).await;
    let end = &all(&hs, "/v1/memory/session/end")[0];
    assert_eq!(end.body["session_id"], format!("openworker/{}", s.id()));
    assert!(end.body["message_count"].as_i64().unwrap() >= 2);
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_summary_is_asked_for_when_the_server_did_not_make_one_and_summaryonsessionend_can_turn_that_off(
) {
    for ask in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let hs = hs_with(false).await;
        configure(
            &root,
            &hs,
            json!({"autoRecall": false, "sessionBridge": false, "summaryOnSessionEnd": ask}),
        );
        let h = start(&root).await;
        let a = h.create_agent(agent()).await.unwrap();
        let s = a.new_session_in(workspace(&root)).await.unwrap();
        within(s.run(SAY)).await.unwrap();
        within(s.close()).await.unwrap();
        let end = seen(&hs, "/v1/memory/session/end").await;
        if ask {
            let summary = seen(&hs, "/v1/memory/session/summary").await;
            assert_eq!(summary.body["session_id"], end.body["session_id"]);
            assert_eq!(summary.body["agent_id"], "tester");
        } else {
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert!(
                all(&hs, "/v1/memory/session/summary").is_empty(),
                "summaryOnSessionEnd is off"
            );
        }
        within(h.shutdown()).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_nobody_spoke_in_is_not_ended_on_heliconsearch() {
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
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
    // (the one thing it does on its own is look at the server's recall budgets)
    let sent: Vec<String> = hs
        .requests()
        .into_iter()
        .map(|r| r.path)
        .filter(|p| p != "/v1/memory/health")
        .collect();
    assert!(sent.is_empty(), "{sent:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_that_predates_the_configuration_is_archived_from_its_first_message_when_it_next_has_a_turn(
) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    // before memory: a run without any configuration
    let h = start(&root).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    within(s.run(SAY)).await.unwrap();
    let reference = s.reference().clone();
    within(h.shutdown()).await.unwrap();

    // memory is switched on; the old session is resumed
    let hs = hs().await;
    configure(
        &root,
        &hs,
        json!({"autoRecall": false, "sessionBridge": false, "endOnShutdown": false}),
    );
    // (the agent was registered before memory existed and keeps its definition: an agent id is immutable, so
    // memory for it would need a new id; its sessions still get recall and capture, just no memory tools)
    let h = start(&root).await;
    let s = h.resume(reference).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let sent: Vec<String> = hs
        .requests()
        .into_iter()
        .map(|r| r.path)
        .filter(|p| p != "/v1/memory/health")
        .collect();
    assert!(
        sent.is_empty(),
        "attaching an old session alone archives nothing: {sent:?}"
    );
    // its next turn sends the whole history, from the very first message
    within(s.run(SAY)).await.unwrap();
    let turns = seen(&hs, "/v1/memory/session/turn").await;
    let indexes: Vec<i64> = turns.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["turn_index"].as_i64().unwrap())
        .collect();
    assert_eq!(indexes.first(), Some(&0));
    assert!(
        indexes.len() >= 3,
        "the old turn and the new one: {indexes:?}"
    );
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changing_the_memory_configuration_changes_the_agent_definition_so_a_new_agent_id_is_needed(
) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(&root, &hs, json!({}));
    let h = start(&root).await;
    h.create_agent(agent()).await.unwrap();
    within(h.shutdown()).await.unwrap();

    // the same configuration: registering again is safe
    let h = start(&root).await;
    h.create_agent(agent()).await.unwrap();
    within(h.shutdown()).await.unwrap();

    // another one (here: the optional store tool): the definition differs, and an agent id is immutable
    configure(&root, &hs, json!({"memoryStoreTool": true}));
    let h = start(&root).await;
    match h.create_agent(agent()).await {
        Err(Error::AgentDefinitionConflict(conflict)) => {
            assert_eq!(conflict.agent_id, "assistant");
            assert!(
                conflict.conflicting_fields.contains(&"tools".to_owned()),
                "{conflict:?}"
            );
        }
        other => panic!(
            "expected a definition conflict, got {:?}",
            other.map(|a| a.id().to_owned())
        ),
    }
    // a new id works
    h.create_agent(
        Agent::new("assistant-v2")
            .instructions("Be helpful.")
            .memory(true)
            .approvals(Approvals::Never),
    )
    .await
    .unwrap();
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_option_that_changes_a_tools_text_does_not_switch_the_memory_off_at_the_next_start() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(&root, &hs, json!({"recallPlacement": "system"}));
    let h = start(&root).await;
    h.create_agent(agent()).await.unwrap();
    within(h.shutdown()).await.unwrap();

    // topK and autoRecall are written into the descriptions of the memory tools, which the engine keeps
    // (a tool of the same name must not change)
    configure(
        &root,
        &hs,
        json!({"topK": 7, "autoRecall": false, "recallPlacement": "system"}),
    );
    let h = start(&root).await;
    let a = h
        .create_agent(
            Agent::new("assistant-v2")
                .instructions("Be helpful.")
                .memory(true)
                .approvals(Approvals::Never),
        )
        .await
        .unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    within(s.run(SAY)).await.unwrap();
    let turns = seen(&hs, "/v1/memory/session/turn").await;
    assert_eq!(
        turns.body["turns"][0]["content"], SAY,
        "capture is still on"
    );
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_that_died_moments_ago_is_ended_by_the_idle_sweep_once_it_has_been_quiet_long_enough()
{
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(
        &root,
        &hs,
        json!({"autoRecall": false, "sessionBridge": false, "idleEndMs": 600000, "endOnShutdown": false}),
    );
    let h = start(&root).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    within(s.run(SAY)).await.unwrap();
    seen(&hs, "/v1/memory/session/turn").await;
    within(h.shutdown()).await.unwrap();

    // restarted at once, with a short idle limit: it is too recent to be an orphan, and is not forgotten.
    // The extension looks at it once the engine is up, however long that took. Starting the engine takes
    // about ten seconds on a slow machine alone and fifteen with the rest of this file running beside it, so
    // its last activity is set 25 seconds ahead (the idle limit is the 1 s minimum): it counts as recent
    // for any start under 26 s, and the sweep ends it 26 s after it was written, which is the time this
    // test takes whatever the load.
    let record = root.join(format!("state/helicon-memory/sessions/{}.json", s.id()));
    let mut saved: Value =
        serde_json::from_str(&std::fs::read_to_string(&record).unwrap()).unwrap();
    saved["lastActive"] = json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 25_000
    );
    std::fs::write(&record, saved.to_string()).unwrap();
    configure(
        &root,
        &hs,
        json!({"autoRecall": false, "sessionBridge": false, "idleEndMs": 1000}),
    );
    let h = start(&root).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        ends(&hs),
        0,
        "still recent: {:?}",
        hs.requests()
            .iter()
            .map(|r| r.path.clone())
            .collect::<Vec<_>>()
    );
    until("the sweep to end it", 60, || ends(&hs) == 1).await;
    assert_eq!(
        all(&hs, "/v1/memory/session/end")[0].body["session_id"],
        format!("openworker/{}", s.id())
    );
    within(h.shutdown()).await.unwrap();
}

fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&format!(" {pid} "))
    }
    #[cfg(unix)]
    {
        // a zombie is dead for this purpose
        if std::path::Path::new("/proc/self/stat").exists() {
            // (the field after the command in /proc/<pid>/stat is its state)
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(stat) => {
                    stat.rsplit(')')
                        .next()
                        .and_then(|rest| rest.trim().chars().next())
                        != Some('Z')
                }
                Err(_) => false,
            }
        } else {
            // no /proc (macOS): ps prints the state, or nothing for a process that is gone
            let out = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            !state.is_empty() && !state.starts_with('Z')
        }
    }
}

/// The pid of the engine child of a managed harness.
fn child_pid(h: &Harness) -> u32 {
    match h.client().connection().borrow().clone() {
        ConnectionState::Ready {
            process_id: Some(pid),
            ..
        } => pid,
        other => panic!("no engine child: {other:?}"),
    }
}

async fn managed(root: &std::path::Path) -> Harness {
    Harness::builder()
        .state_dir(root.join("state"))
        .executable(runner())
        .tools(tool::registry())
        .sandbox(Sandbox::Off)
        .scripted(true)
        .managed(runner(), openworker_sdk::RestartPolicy::default())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_harness_without_shutdown_still_stops_the_engine_with_memory_on_or_off() {
    for memory in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let hs = hs().await;
        if memory {
            configure(&root, &hs, json!({"idleEndMs": 60000}));
        }
        let h = managed(&root).await;
        let pid = child_pid(&h);
        assert!(process_alive(pid));
        let a = h.create_agent(agent()).await.unwrap();
        let s = a.new_session_in(workspace(&root)).await.unwrap();
        within(s.run(SAY)).await.unwrap();
        drop((s, a, h));
        let mut gone = false;
        for _ in 0..200 {
            if !process_alive(pid) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            gone,
            "the engine child {pid} outlived its harness (memory on: {memory})"
        );
    }
}
