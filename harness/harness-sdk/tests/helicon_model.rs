#![cfg(feature = "test-support")]
//! The memory extension against an engine that talks to a (fake) model over HTTP. This is the check that
//! recall really reaches the model: what the model is actually sent — the recalled memories in its
//! system instructions, the tools it can call, the history — and the model calling the memory tools
//! itself. No API keys, no network beyond loopback.
#[path = "support/helicon_fixture.rs"]
mod helicon_fixture;
#[path = "support/hs_mock.rs"]
mod hs_mock;
#[path = "support/model_mock.rs"]
mod model_mock;
use helicon_fixture::*;
use hs_mock::Request;
use model_mock::{conversation_of, system_of, tool_names, Answer, MockModel};
use openworker_sdk::{
    agents::*, ApprovalDecision, Credentials, InputContent, Model, ModelProvider, TurnOptions,
    UserInput,
};
use serde_json::{json, Value};

const HELICON_TOOLS: [&str; 4] = [
    "helicon__memory_recall",
    "helicon__memory_get",
    "helicon__memory_forget",
    "helicon__skill_get",
];
const BUILTIN_MEMORY_TOOLS: [&str; 2] = ["memory_remember", "memory_search"];

/// A fake model that says "Tea it is.", or makes the tool calls the latest user message asks for:
/// `CALL_TOOL:<name>|<json>` (one) or `CALLS:[{"name":..,"arguments":{..}}, ..]` (in order, one per request).
/// After the last tool result it answers "done".
async fn model() -> MockModel {
    MockModel::start(|request| {
        let messages = request["messages"].as_array().cloned().unwrap_or_default();
        let text_of = |m: &Value| {
            m["content"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| m["content"].to_string())
        };
        // the latest user message, and the tool results that came after it
        let at = messages.iter().rposition(|m| m["role"] == "user");
        let user = at.map(|i| text_of(&messages[i])).unwrap_or_default();
        let results = at.map_or(0, |i| {
            messages[i + 1..]
                .iter()
                .filter(|m| m["role"] == "tool")
                .count()
        });
        let calls: Vec<Value> = if let Some((_, rest)) = user.split_once("CALLS:") {
            serde_json::from_str(rest.trim()).unwrap_or_default()
        } else if let Some((_, rest)) = user.split_once("CALL_TOOL:") {
            let (name, arguments) = rest.split_once('|').unwrap_or((rest, "{}"));
            let arguments: Value = serde_json::from_str(arguments.trim()).unwrap_or(json!({}));
            vec![json!({"name": name.trim(), "arguments": arguments})]
        } else {
            vec![]
        };
        match calls.get(results) {
            Some(call) => Answer::Tool {
                name: call["name"].as_str().unwrap_or("").to_owned(),
                arguments: call["arguments"].clone(),
            },
            None if !calls.is_empty() => Answer::Text("done".into()),
            None => Answer::Text("Tea it is.".into()),
        }
    })
    .await
}

async fn start(root: &std::path::Path, model: &MockModel) -> Harness {
    let harness = Harness::builder()
        .state_dir(root.join("state"))
        .executable(runner())
        .sandbox(Sandbox::Off)
        .start()
        .await
        .unwrap();
    harness
        .set_model_provider(
            ModelProvider::chat_completions("Fixture")
                .base_url(model.url())
                .credentials(Credentials::secure("fixture-key").memory_only()),
        )
        .await
        .unwrap();
    harness
}

fn agent() -> Agent {
    Agent::new("assistant")
        .instructions("Be helpful.")
        .memory(true)
        .approvals(Approvals::Never)
        .default_model(Model::new("Fixture", "fixture"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_model_is_sent_the_recall_and_the_first_turn_summary_once_and_never_in_the_history() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(&root, &hs, json!({}));
    let model = model().await;
    let h = start(&root, &model).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();

    let r = within(s.run("What tea do I like?")).await.unwrap();
    assert_eq!(r.status, RunStatus::Completed);
    assert_eq!(r.final_output.as_deref(), Some("Tea it is."));
    let first = &model.requests()[0];
    let system = system_of(first);
    assert!(system.starts_with("Be helpful."), "{system}");
    assert!(
        system.contains("## Long-term Memory"),
        "the usage rules are in the instructions"
    );
    assert!(system.contains("Application instructions for this turn:"));
    // the recall, in its untrusted envelope, with the id index; and the cross-session summary (first turn)
    assert!(
        system.contains("<relevant-memories>") && system.contains("- likes tea"),
        "{system}"
    );
    assert!(system.contains(&format!("- {MEMORY_ID} — likes tea")));
    assert!(system.contains("## 跨会话记忆") && system.contains("lives in Oslo"));
    // the tools: ours are offered, the engine's own memory tools are not
    let tools = tool_names(first);
    for name in HELICON_TOOLS {
        assert!(tools.contains(&name.to_owned()), "{name} in {tools:?}");
    }
    for name in BUILTIN_MEMORY_TOOLS {
        assert!(
            !tools.contains(&name.to_owned()),
            "{name} must be off in {tools:?}"
        );
    }
    // what the user said is in the history as they said it; the recalled text is not
    let said = conversation_of(first);
    assert!(said.contains("What tea do I like?"));
    assert!(
        !said.contains("relevant-memories") && !said.contains("likes tea"),
        "{said}"
    );

    // the second turn: recall again, the summary not again, the first answer in the history
    within(s.run("And coffee?")).await.unwrap();
    let second = &model.requests()[1];
    let system = system_of(second);
    assert!(system.contains("<relevant-memories>"));
    assert!(
        !system.contains("## 跨会话记忆"),
        "the summary is for the first turn only"
    );
    let said = conversation_of(second);
    assert!(said.contains("Tea it is.") && said.contains("And coffee?"));
    assert!(
        !said.contains("relevant-memories"),
        "recalled text never enters the history"
    );

    // HeliconSearch was asked with the user's words, once for the bridge, and holds the conversation
    let queries = all(&hs, "/v1/memory/query");
    assert_eq!(queries.len(), 2);
    assert_eq!(queries[0].body["query"], "What tea do I like?");
    assert_eq!(all(&hs, "/v1/memory/session_bridge").len(), 1);
    let turns = seen(&hs, "/v1/memory/session/turn").await;
    let archived: Vec<&Value> = turns.body["turns"].as_array().unwrap().iter().collect();
    assert_eq!(archived[0]["content"], "What tea do I like?");
    assert!(archived
        .iter()
        .any(|t| t["content"] == "Tea it is." && t["role"] == "assistant"));
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_model_calls_a_memory_tool_itself_and_its_traffic_is_not_archived() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(&root, &hs, json!({"sessionBridge": false}));
    let model = model().await;
    let h = start(&root, &model).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();

    let r = within(s.run(r#"CALL_TOOL:helicon__memory_recall|{"query":"which tea"}"#))
        .await
        .unwrap();
    assert_eq!(r.status, RunStatus::Completed);
    assert_eq!(r.final_output.as_deref(), Some("done"));
    // the model's second request carries the tool's answer
    let requests = model.requests();
    let with_result = requests
        .iter()
        .find(|r| {
            r["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "tool")
        })
        .expect("a request with the tool result");
    let tool_message = with_result["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap();
    let answer = tool_message["content"].to_string();
    assert!(answer.contains("Found 1 memories"), "{answer}");
    assert!(answer.contains(&format!("id={MEMORY_ID}")), "{answer}");
    // the explicit search is an active retrieval of its own, with the model's query
    let active: Vec<Request> = all(&hs, "/v1/memory/query")
        .into_iter()
        .filter(|q| q.body["purpose"] == "active")
        .collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].body["query"], "which tea");
    // the archive holds the conversation, not the extension's own tool call and its result
    let turns = seen(&hs, "/v1/memory/session/turn").await;
    let archived = turns.body["turns"].to_string();
    assert!(
        !archived.contains("Found 1 memories") && !archived.contains("call_fixture"),
        "{archived}"
    );
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_configuration_the_engine_keeps_its_own_memory_and_the_model_sees_no_recall() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let model = model().await;
    let h = start(&root, &model).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    within(s.run("What tea do I like?")).await.unwrap();
    let request = &model.requests()[0];
    let tools = tool_names(request);
    for name in BUILTIN_MEMORY_TOOLS {
        assert!(tools.contains(&name.to_owned()), "{name} in {tools:?}");
    }
    assert!(tools.iter().all(|t| !t.starts_with("helicon__")));
    let system = system_of(request);
    assert!(
        !system.contains("relevant-memories") && !system.contains("Long-term Memory"),
        "{system}"
    );
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replace_builtin_memory_false_gives_the_agent_both_and_agents_not_listed_get_neither() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(
        &root,
        &hs,
        json!({"replaceBuiltinMemory": false, "agents": ["assistant"], "sessionBridge": false}),
    );
    let model = model().await;
    let h = start(&root, &model).await;

    let both = h.create_agent(agent()).await.unwrap();
    let s = both.new_session_in(workspace(&root)).await.unwrap();
    let both_id = s.id().to_owned();
    within(s.run("What tea do I like?")).await.unwrap();
    let tools = tool_names(&model.requests()[0]);
    assert!(
        tools.contains(&"memory_remember".to_owned())
            && tools.contains(&"helicon__memory_recall".to_owned()),
        "{tools:?}"
    );
    within(s.close()).await.unwrap();

    // an agent the configuration does not list: the engine's memory, no recall, nothing sent to HeliconSearch
    let before = hs.requests().len();
    let other = h
        .create_agent(
            Agent::new("other")
                .instructions("Other.")
                .memory(true)
                .approvals(Approvals::Never)
                .default_model(Model::new("Fixture", "fixture")),
        )
        .await
        .unwrap();
    let s = other.new_session_in(workspace(&root)).await.unwrap();
    within(s.run("What tea do I like?")).await.unwrap();
    let request = model.requests().last().cloned().unwrap();
    let tools = tool_names(&request);
    assert!(tools.contains(&"memory_remember".to_owned()));
    assert!(
        tools.iter().all(|t| !t.starts_with("helicon__")),
        "{tools:?}"
    );
    assert!(!system_of(&request).contains("relevant-memories"));
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
    let after: Vec<String> = hs
        .requests()
        .into_iter()
        .skip(before)
        .map(|r| r.path)
        .collect();
    assert!(
        after.iter().all(|p| !p.contains("session")),
        "nothing of the other agent's session was sent: {after:?} others={} first={} all={:?}",
        s.id(),
        both_id,
        hs.requests()
            .iter()
            .map(|r| format!("{} {}", r.path, r.body["session_id"]))
            .collect::<Vec<_>>()
    );
}

/// The tool messages (their texts) of a model request, in order.
fn tool_results(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "tool")
        .map(|m| {
            let content = m["content"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| m["content"].to_string());
            // the engine wraps a tool's text as {"content":[{"text":..}],"structuredContent":..}: the text itself
            serde_json::from_str::<Value>(&content)
                .ok()
                .and_then(|v| v["structuredContent"].as_str().map(str::to_owned))
                .unwrap_or(content)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_way_of_sending_a_message_carries_the_recall() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(&root, &hs, json!({"sessionBridge": false}));
    let model = model().await;
    let h = start(&root, &model).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();

    within(s.run("via run")).await.unwrap();
    within(async { s.send("via send").await.unwrap().result().await.unwrap() }).await;
    within(async {
        let mut stream = s.run_streamed("via run_streamed").await.unwrap();
        while stream.next().await.is_some() {}
    })
    .await;
    within(s.run_input(UserInput::new(vec![InputContent::Text(
        "via run_input".into(),
    )])))
    .await
    .unwrap();
    let text_input = |text: &str| UserInput::new(vec![InputContent::Text(text.into())]);
    within(async {
        s.send_input(text_input("via send_input"), TurnOptions::default())
            .await
            .unwrap()
            .result()
            .await
            .unwrap()
    })
    .await;
    within(async {
        let mut stream = s
            .run_input_streamed(text_input("via run_input_streamed"))
            .await
            .unwrap();
        while stream.next().await.is_some() {}
    })
    .await;
    within(s.run_with_submission("fixed-1", "via run_with_submission"))
        .await
        .unwrap();
    within(async {
        s.send_with_id("fixed-2", "via send_with_id")
            .await
            .unwrap()
            .result()
            .await
            .unwrap()
    })
    .await;

    let requests = model.requests();
    assert_eq!(requests.len(), 8);
    for (request, how) in requests.iter().zip([
        "run",
        "send",
        "run_streamed",
        "run_input",
        "send_input",
        "run_input_streamed",
        "run_with_submission",
        "send_with_id",
    ]) {
        let system = system_of(request);
        assert!(
            system.contains("<relevant-memories>") && system.contains("- likes tea"),
            "{how}: {system}"
        );
        assert!(
            conversation_of(request).contains(&format!("via {how}")),
            "{how}"
        );
    }
    // HeliconSearch was asked with each message's own words
    let queries: Vec<String> = all(&hs, "/v1/memory/query")
        .iter()
        .map(|q| q.body["query"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        queries,
        [
            "via run",
            "via send",
            "via run_streamed",
            "via run_input",
            "via send_input",
            "via run_input_streamed",
            "via run_with_submission",
            "via send_with_id"
        ]
    );
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_model_uses_every_memory_tool_and_the_server_is_asked_for_each() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(
        &root,
        &hs,
        json!({"sessionBridge": false, "autoRecall": false, "memoryStoreTool": true}),
    );
    let model = model().await;
    let h = start(&root, &model).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    let skill = "2e24aed6c17248949926b5bfe285781f";
    let calls = json!([
        {"name": "helicon__memory_get", "arguments": {"memoryId": MEMORY_ID}},
        {"name": "helicon__memory_forget", "arguments": {"memoryId": MEMORY_ID}},
        {"name": "helicon__memory_store", "arguments": {"text": "likes oolong", "category": "preference"}},
        {"name": "helicon__skill_get", "arguments": {"skillId": skill}},
    ]);
    let r = within(s.run(&format!("CALLS:{calls}"))).await.unwrap();
    assert_eq!(
        r.status,
        RunStatus::Completed,
        "{r:?} requests={}",
        model.requests().len()
    );
    assert_eq!(r.final_output.as_deref(), Some("done"));
    // the tool list the model had includes the optional store tool
    assert!(tool_names(&model.requests()[0]).contains(&"helicon__memory_store".to_owned()));

    let results = tool_results(model.requests().last().unwrap());
    assert_eq!(results.len(), 4, "{results:?}");
    assert!(
        results[0].starts_with(&format!("**Memory** id={MEMORY_ID}\nText: likes tea")),
        "{}",
        results[0]
    );
    assert!(results[0].contains("Source sessions: openworker/earlier (messages #1)"));
    assert!(
        results[0].contains("[#1] user  ◀ source of this memory\nI like tea"),
        "{}",
        results[0]
    );
    assert_eq!(results[1], format!("Memory {MEMORY_ID} forgotten."));
    assert_eq!(
        results[2],
        "Stored: \"likes oolong\" (id=fedcba9876543210fedcba9876543210)"
    );
    assert_eq!(
        results[3],
        format!("**Skill** id={skill}\n\n# Tea skill\nBrew it.")
    );

    // each reached HeliconSearch the way the tool says it does
    let paths: Vec<(String, String)> = hs
        .requests()
        .iter()
        .map(|r| {
            (
                r.method.clone(),
                r.path.split('?').next().unwrap().to_owned(),
            )
        })
        .collect();
    for want in [
        ("GET".to_owned(), "/v1/memory/explain".to_owned()),
        ("GET".to_owned(), format!("/v1/memory/source/{MEMORY_ID}")),
        ("GET".to_owned(), "/v1/memory/session/turns".to_owned()),
        ("DELETE".to_owned(), format!("/v1/memory/{MEMORY_ID}")),
        ("POST".to_owned(), "/v1/memory/store".to_owned()),
        ("GET".to_owned(), format!("/v1/memory/skill/{skill}")),
    ] {
        assert!(paths.contains(&want), "{want:?} not in {paths:?}");
    }
    let turns_request = all(&hs, "/v1/memory/session/turns").remove(0);
    assert!(
        turns_request
            .path
            .contains("session_id=openworker%2Fearlier")
            && turns_request.path.contains("agent_id=tester")
    );
    let store = all(&hs, "/v1/memory/store").remove(0);
    assert_eq!(store.body["extraction_method"], "agent_tool");
    assert_eq!(store.body["category"], "preference");
    assert_eq!(store.body["text"], "likes oolong");

    // none of that tool traffic is archived: the user's message and the final answer only
    within(s.close()).await.unwrap();
    let archived = all(&hs, "/v1/memory/session/turn");
    let sent: Vec<(i64, String)> = archived
        .iter()
        .flat_map(|r| r.body["turns"].as_array().unwrap().clone())
        .map(|t| {
            (
                t["turn_index"].as_i64().unwrap(),
                t["role"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(sent, [(0, "user".to_owned()), (9, "assistant".to_owned())]);
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_the_default_approvals_the_user_is_asked_before_a_memory_tool_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(
        &root,
        &hs,
        json!({"sessionBridge": false, "autoRecall": false}),
    );
    let model = model().await;
    let h = start(&root, &model).await;
    // no `.approvals(..)`: the default is that every tool call waits for a decision
    let a = h
        .create_agent(
            Agent::new("careful")
                .instructions("Be helpful.")
                .default_model(Model::new("Fixture", "fixture")),
        )
        .await
        .unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    let r = within(s.run(r#"CALL_TOOL:helicon__memory_recall|{"query":"which tea"}"#))
        .await
        .unwrap();
    assert_eq!(r.status, RunStatus::Interrupted);
    let interruption = r.interruptions[0].clone();
    assert!(
        matches!(&interruption.kind, InterruptionKind::Approval { tool, .. } if tool == "helicon__memory_recall"),
        "{interruption:?}"
    );
    assert!(
        all(&hs, "/v1/memory/query").is_empty(),
        "nothing was searched before the user said yes"
    );

    s.respond(&interruption, Response::Approve(ApprovalDecision::Once))
        .await
        .unwrap();
    let r = within(s.resume()).await.unwrap();
    assert_eq!(r.status, RunStatus::Completed);
    let active: Vec<_> = all(&hs, "/v1/memory/query")
        .into_iter()
        .filter(|q| q.body["purpose"] == "active")
        .collect();
    assert_eq!(active.len(), 1);
    // (the engine hands the model a tool's text inside its own result object)
    let results = tool_results(model.requests().last().unwrap());
    assert!(
        results
            .first()
            .is_some_and(|r| r.contains("Found 1 memories")),
        "{results:?}"
    );
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_given_to_the_builder_loses_the_builtin_memory_but_is_still_recalled_into() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(&root, &hs, json!({"sessionBridge": false}));
    let model = model().await;
    let h = Harness::builder()
        .state_dir(root.join("state"))
        .executable(runner())
        .sandbox(Sandbox::Off)
        .model_provider(
            ModelProvider::chat_completions("Fixture")
                .base_url(model.url())
                .credentials(Credentials::secure("fixture-key").memory_only()),
        )
        .agent(agent())
        .start()
        .await
        .unwrap();
    let s = h.session("assistant", workspace(&root)).await.unwrap();
    within(s.run("What tea do I like?")).await.unwrap();
    let request = &model.requests()[0];
    let tools = tool_names(request);
    // the host compiles such an agent before application tools exist: it cannot name them, and says nothing of them
    assert!(
        tools.iter().all(|t| !t.starts_with("helicon__")),
        "{tools:?}"
    );
    assert!(!system_of(request).contains("Long-term Memory"));
    // ... but the engine's own memory is off, and the turn is recalled into and archived all the same
    for name in BUILTIN_MEMORY_TOOLS {
        assert!(!tools.contains(&name.to_owned()), "{name} in {tools:?}");
    }
    assert!(system_of(request).contains("<relevant-memories>"));
    seen(&hs, "/v1/memory/session/turn").await;
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_model_searches_and_uploads_media_through_the_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    configure(
        &root,
        &hs,
        json!({"sessionBridge": false, "autoRecall": false}),
    );
    let model = model().await;
    let h = start(&root, &model).await;
    let a = h.create_agent(agent()).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    let calls = json!([
        {"name": "helicon__media_search", "arguments": {"query": "a red car"}},
        {"name": "helicon__media_upload", "arguments": {"file_names": ["/photos/new.jpg"]}},
    ]);
    let r = within(s.run(&format!("CALLS:{calls}"))).await.unwrap();
    assert_eq!(r.status, RunStatus::Completed, "{r:?}");
    assert_eq!(r.final_output.as_deref(), Some("done"));

    // the media tools are on by default, with their rules; the admin tools are not
    let first = &model.requests()[0];
    let tools = tool_names(first);
    for name in ["helicon__media_search", "helicon__media_upload"] {
        assert!(tools.contains(&name.to_owned()), "{name} in {tools:?}");
    }
    assert!(
        !tools.contains(&"helicon__media_files".to_owned()),
        "{tools:?}"
    );
    assert!(system_of(first).contains("## Media Library (images and videos)"));

    // what the model got back: a hit with its file path, and the upload's receipt
    let results = tool_results(model.requests().last().unwrap());
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(
        results[0].contains("/photos/car.jpg") && results[0].contains("similarity"),
        "{}",
        results[0]
    );
    assert!(
        results[1].contains("Submitted 1 file(s) for indexing into library kb1 (batch b1)"),
        "{}",
        results[1]
    );

    // each reached HeliconSearch the way the tool says, with the tenant credential
    let paths: Vec<String> = hs
        .requests()
        .iter()
        .map(|r| r.path.split('?').next().unwrap().to_owned())
        .collect();
    for want in [
        "/v1/kb/list",
        "/v1/document/search",
        "/v1/kb/resolve_default",
        "/v1/document/upload",
    ] {
        assert!(paths.iter().any(|p| p == want), "{want} in {paths:?}");
    }
    let search = &all(&hs, "/v1/document/search")[0];
    assert_eq!(search.body["message"], "a red car");
    assert_eq!(search.auth.as_deref(), Some("Bearer hs_test"));
    assert_eq!(
        all(&hs, "/v1/document/upload")[0].body["file_names"],
        json!(["/photos/new.jpg"])
    );

    // the archive holds the conversation (the user's message names the tools: it is a script), not what the
    // tools answered
    let turns = seen(&hs, "/v1/memory/session/turn").await;
    let archived = turns.body["turns"].to_string();
    assert!(
        !archived.contains("/photos/car.jpg") && !archived.contains("Submitted 1 file(s)"),
        "{archived}"
    );
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_registered_before_the_media_tools_conflicts_once_they_are_on_unless_they_are_switched_off(
) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let hs = hs().await;
    let model = model().await;

    // a version without the media tools: the agent is registered with the memory tools only
    configure(
        &root,
        &hs,
        json!({"sessionBridge": false, "autoMedia": false}),
    );
    let h = start(&root, &model).await;
    h.create_agent(agent()).await.unwrap();
    within(h.shutdown()).await.unwrap();

    // the same state directory, this version's default (the media tools on): the id is taken by the old definition
    configure(&root, &hs, json!({"sessionBridge": false}));
    let h = start(&root, &model).await;
    let refused = h.create_agent(agent()).await;
    let message = refused.err().map(|e| e.to_string());
    eprintln!("create_agent for the old id with the media tools on: {message:?}");
    assert!(
        message.is_some(),
        "the documented definition conflict did not happen"
    );
    // under a new id it works, with the media tools
    let renamed = Agent::new("assistant-v2")
        .instructions("Be helpful.")
        .memory(true)
        .approvals(Approvals::Never)
        .default_model(Model::new("Fixture", "fixture"));
    let a = h.create_agent(renamed).await.unwrap();
    let s = a.new_session_in(workspace(&root)).await.unwrap();
    within(s.run("hello")).await.unwrap();
    assert!(tool_names(&model.requests()[0]).contains(&"helicon__media_search".to_owned()));
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();

    // with autoMedia switched off the old agent is accepted as it was
    configure(
        &root,
        &hs,
        json!({"sessionBridge": false, "autoMedia": false}),
    );
    let h = start(&root, &model).await;
    h.create_agent(agent())
        .await
        .expect("the old agent id with autoMedia off");
    within(h.shutdown()).await.unwrap();
}
