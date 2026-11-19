//! Against a real HeliconSearch, by hand: `HELICON_LIVE_URL=http://127.0.0.1:5001
//! HELICON_LIVE_TOKEN=hs_... cargo test -p openworker-sdk --lib live -- --ignored --nocapture`.
//!
//! It uses its own agent id per run, so it never touches other memories of the tenant, and removes the
//! memory it stores. Nothing here runs without the two variables.
use super::{
    capture::{CaptureDeps, CaptureEngine},
    config::MemoryConfig,
    history::{HistoryPage, HistorySource},
    session_store::SessionStore,
    source_ref::make_manual_ref,
    util::now_ms,
    Inner,
};
use crate::extension::{SessionInfo, TurnContext};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

struct Fixed(Vec<(usize, Value)>);

impl HistorySource for Fixed {
    fn read<'a>(&'a self, _s: &'a str, after: usize) -> BoxFuture<'a, Result<HistoryPage, String>> {
        Box::pin(async move {
            Ok(HistoryPage {
                items: self
                    .0
                    .iter()
                    .filter(|(i, _)| *i >= after)
                    .cloned()
                    .collect(),
                next: self.0.len(),
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a running HeliconSearch: HELICON_LIVE_URL and HELICON_LIVE_TOKEN"]
async fn live_memory_round_trip() {
    let (Ok(url), Ok(token)) = (
        std::env::var("HELICON_LIVE_URL"),
        std::env::var("HELICON_LIVE_TOKEN"),
    ) else {
        // run on purpose (--ignored): a silent pass would claim a check that did not happen
        panic!("set HELICON_LIVE_URL and HELICON_LIVE_TOKEN to run the live tests");
    };
    let agent = format!("rust-live-{}", now_ms());
    let dir = tempfile::tempdir().unwrap();
    let cfg = Arc::new(
        MemoryConfig::parse(
            &json!({"baseUrl": url, "apiKey": token, "agentId": agent, "projectId": "live",
                "captureMode": "direct", "debug": true}),
            &|_| None,
        )
        .unwrap(),
    );
    let inner = Inner::new(cfg.clone(), dir.path());

    // health: the server's recall budgets
    let health = inner.client.health().await.expect("health");
    inner.budgets.learn_from_health(Some(&health));
    eprintln!("budgets: {:?}", inner.budgets.current());

    // a memory to find
    let (memory_id, status) = inner
        .client
        .store(json!({
            "text": "My favorite tea is jasmine green tea", "agent_id": agent, "project_id": "live",
            "category": "preference", "scope": "project", "importance": 0.9, "resolution": "fact",
            "extraction_method": "auto_capture_direct", "confidence": 0.85,
            "source_refs": [make_manual_ref("rust_live")],
        }))
        .await
        .expect("store");
    eprintln!("stored {memory_id} {status:?}");

    // the explicit search tool, until the memory is searchable
    let mut found = String::new();
    for attempt in 0..40 {
        let text = inner
            .tools
            .memory_recall(
                &json!({"query": "what is my favorite tea?"}),
                &format!("live#{attempt}"),
            )
            .await;
        if text.contains("jasmine") {
            found = text;
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    eprintln!("memory_recall: {found}");
    // (for a freshly stored memory the server returns a grounded fact with an id, or only a topic
    // summary of it: both are the memory, found)
    assert!(found.contains("jasmine"), "the memory is found: {found}");

    // the automatic recall for a turn: the memory is in the addendum
    let session = SessionInfo {
        session_id: format!("live-{}", now_ms()),
        agent: "lead".into(),
    };
    inner.session_created(&session);
    let addendum = inner
        .prepare_turn(&TurnContext {
            session: &session,
            submission_id: "sub-1",
            text: "What is my favorite tea?",
            addendum: None,
        })
        .await
        .expect("an addendum");
    eprintln!("addendum:\n{addendum}");
    assert!(addendum.contains("<relevant-memories>") && addendum.contains("jasmine"));

    // opening it, with its record
    let opened = inner
        .tools
        .memory_get(&json!({"memoryId": memory_id}), "live#open")
        .await;
    eprintln!("memory_get: {opened}");
    assert!(opened.starts_with(&format!(
        "**Memory** id={memory_id}\nText: My favorite tea is jasmine green tea"
    )));

    // the archive: a conversation is sent, stored and readable back
    let hs_session = format!("openworker/{}", session.session_id);
    let history = vec![
        (
            0,
            json!({"role": "user", "content": "I prefer oolong tea in the evening, remember that."}),
        ),
        (
            1,
            json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Noted: oolong in the evening."}]}),
        ),
    ];
    let store = Arc::new(Mutex::new(SessionStore::new(
        dir.path().join("helicon-memory"),
        inner.logger.clone(),
    )));
    let engine = CaptureEngine::new(CaptureDeps {
        cfg: cfg.clone(),
        client: inner.client.clone(),
        store: store.clone(),
        history: Arc::new(Fixed(history)),
        logger: inner.logger.clone(),
        own_tool_prefix: "helicon__",
    });
    engine.flush(&session.session_id, 60_000).await;
    assert_eq!(
        store.lock().unwrap().get(&session.session_id).archive_next,
        2
    );
    let page = inner
        .client
        .get_session_turns(&hs_session, &agent, "live", Some(0), None, Some(10))
        .await
        .expect("session turns");
    eprintln!("archived: {page}");
    assert_eq!(page["total"], 2);
    assert_eq!(
        page["turns"][0]["content"],
        "I prefer oolong tea in the evening, remember that."
    );

    // memory_get by where an excerpt is: the archived messages come back
    let excerpt = inner
        .tools
        .memory_get(
            &json!({"session": hs_session, "turns": "0-1"}),
            "live#excerpt",
        )
        .await;
    eprintln!("excerpt: {excerpt}");
    assert!(excerpt.contains("[#0] user") && excerpt.contains("◀ the recalled excerpt"));

    // the operator is handed the archived messages (the server queues the extraction)
    let added = inner
        .client
        .add(json!({
            "messages": [
                {"role": "user", "content": "I prefer oolong tea in the evening, remember that.", "turn_index": 0},
                {"role": "assistant", "content": "Noted: oolong in the evening.", "turn_index": 1},
            ],
            "agent_id": agent, "project_id": "live", "session_id": hs_session,
            "source_refs": [{"type": "session", "ref_id": hs_session}], "store_gate": false,
        }))
        .await
        .expect("add");
    eprintln!("add: {added:?}");
    assert!(
        added.queued
            || added.already_processed
            || !added.operations.is_empty()
            || added.processing_state.is_some(),
        "the server accepted the slice: {added:?}"
    );

    // a memory the operator made has source turns: memory_get shows the conversation it came from
    let made = added
        .operations
        .iter()
        .find_map(|op| op["memory_id"].as_str())
        .map(str::to_owned);
    if let Some(id) = &made {
        let opened = inner
            .tools
            .memory_get(&json!({"memoryId": id}), "live#source")
            .await;
        eprintln!("memory_get (operator memory): {opened}");
        assert!(
            opened.contains(&format!("Source sessions: {hs_session}")),
            "{opened}"
        );
        assert!(
            opened.contains("source of this memory"),
            "the source message is marked: {opened}"
        );
        assert!(
            opened.contains("I prefer oolong tea in the evening"),
            "{opened}"
        );
        // forgetting by description: one strong match goes, several are listed, none is said
        let forgot = inner
            .tools
            .memory_forget(&json!({"query": "oolong tea in the evening"}))
            .await;
        eprintln!("memory_forget (by query): {forgot}");
        assert!(
            forgot.starts_with("Forgotten:")
                || forgot.starts_with("Found ")
                || forgot == "No matching memories found.",
            "{forgot}"
        );
    }
    // a skill id the server does not have is said so, not an error
    let skill = inner
        .tools
        .skill_get(&json!({"skillId": "0123456789abcdef0123456789abcdef"}))
        .await;
    eprintln!("skill_get: {skill}");
    assert!(skill.contains("no published skill"), "{skill}");

    // what the operator extracted is removed again with the rest
    for op in &added.operations {
        if let Some(id) = op["memory_id"].as_str() {
            let _ = inner.client.delete(id).await;
        }
    }

    // end it
    let ended = inner
        .client
        .session_end(
            json!({"session_id": hs_session, "agent_id": agent, "project_id": "live",
            "source_refs": [{"type": "session", "ref_id": hs_session}], "message_count": 2}),
        )
        .await
        .expect("session end");
    eprintln!("session end: {ended}");

    // ending is not final: the same session takes more turns afterwards, and is ended again
    let more = inner
        .client
        .append_turns(json!({
            "session_id": hs_session, "agent_id": agent, "project_id": "live", "conversation_id": session.session_id,
            "turns": [
                {"turn_index": 2, "role": "user", "content": "One more thing: I take no sugar.", "content_type": "text"},
                {"turn_index": 3, "role": "assistant", "content": "No sugar, noted.", "content_type": "text"},
            ],
        }))
        .await
        .expect("turns after the session was ended");
    eprintln!("turns after end: {more}");
    let results = more["results"].as_array().expect("receipts");
    assert!(
        results
            .iter()
            .all(|r| matches!(r["sql_status"].as_str(), Some("stored" | "replayed"))),
        "HeliconSearch takes turns after session/end: {more}"
    );
    // the receipt's content_hash is the sha256 of the text sent: that is what ties an extraction message to
    // its archived turn (archive_ref), so it must agree with how the extension computes it
    use sha2::{Digest, Sha256};
    let sent = "One more thing: I take no sugar.";
    let hash: String = Sha256::digest(sent.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        results[0]["content_hash"],
        hash.as_str(),
        "HeliconSearch hashes the text as sent: {more}"
    );
    let again = inner
        .client
        .session_end(
            json!({"session_id": hs_session, "agent_id": agent, "project_id": "live",
            "source_refs": [{"type": "session", "ref_id": hs_session}], "message_count": 4}),
        )
        .await
        .expect("session end again");
    eprintln!("session end again: {again}");
    let page = inner
        .client
        .get_session_turns(&hs_session, &agent, "live", Some(0), None, Some(10))
        .await
        .expect("session turns");
    assert_eq!(page["total"], 4, "{page}");

    // forgetting it
    let gone = inner
        .tools
        .memory_forget(&json!({"memoryId": memory_id}))
        .await;
    eprintln!("memory_forget: {gone}");
    assert_eq!(gone, format!("Memory {memory_id} forgotten."));
}

/// Read-only, against an agent that already has memories (set `HELICON_LIVE_READ_AGENT`, e.g. the
/// tenant's default agent id): the cross-session summary and the recall are built from real data.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a running HeliconSearch with memories: HELICON_LIVE_URL, HELICON_LIVE_TOKEN, HELICON_LIVE_READ_AGENT"]
async fn live_the_bridge_and_recall_of_an_agent_with_real_memories() {
    let (Ok(url), Ok(token), Ok(agent)) = (
        std::env::var("HELICON_LIVE_URL"),
        std::env::var("HELICON_LIVE_TOKEN"),
        std::env::var("HELICON_LIVE_READ_AGENT"),
    ) else {
        panic!("set HELICON_LIVE_URL, HELICON_LIVE_TOKEN and HELICON_LIVE_READ_AGENT to run this live test");
    };
    let dir = tempfile::tempdir().unwrap();
    let cfg = Arc::new(
        MemoryConfig::parse(
            &json!({"baseUrl": url, "apiKey": token, "agentId": agent, "debug": true}),
            &|_| None,
        )
        .unwrap(),
    );
    let inner = Inner::new(cfg.clone(), dir.path());
    let health = inner.client.health().await.expect("health");
    inner.budgets.learn_from_health(Some(&health));
    let bridge = inner
        .client
        .get_session_bridge(
            &agent,
            "",
            &[("episodic_limit", "3".into())],
            &super::client::CallOptions::default(),
        )
        .await
        .expect("bridge");
    let episodes = bridge["recent_episodes"].as_array().map_or(0, Vec::len);
    let rendering = super::bridge::format_bridge_context(&bridge, cfg.session_bridge_token_budget);
    eprintln!(
        "bridge: protocol={} episodes={episodes}
{}",
        bridge["protocol"], rendering.text
    );
    assert_eq!(bridge["protocol"], "bridge.v2");
    assert!(
        !rendering.text.is_empty(),
        "an agent with memories has a bridge"
    );
    if episodes > 0 {
        assert!(
            rendering.text.contains("**近期会话摘要**"),
            "the episodes are shown"
        );
        assert!(rendering
            .shown
            .complete
            .keys()
            .any(|k| k.starts_with("session_summary:")));
    }
    // a first turn: the bridge and a recall, the recall not repeating what the bridge shows whole
    let session = SessionInfo {
        session_id: format!("live-read-{}", now_ms()),
        agent: "lead".into(),
    };
    inner.session_created(&session);
    let addendum = inner
        .prepare_turn(&TurnContext {
            session: &session,
            submission_id: "sub-1",
            text: "What do you remember about me and my preferences?",
            addendum: None,
        })
        .await
        .expect("an addendum");
    eprintln!(
        "addendum ({} bytes):
{addendum}",
        addendum.len()
    );
    assert!(addendum.contains("## 跨会话记忆"));
    assert!(addendum.contains("<relevant-memories>"));
    assert!(addendum.len() <= 32_000);
    assert!(!addendum.contains('\0'));
}
