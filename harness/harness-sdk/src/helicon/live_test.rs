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
        eprintln!("skipped: HELICON_LIVE_URL / HELICON_LIVE_TOKEN not set");
        return;
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
    assert!(
        found.contains(&format!("id={memory_id}")),
        "the tool shows the whole id: {found}"
    );

    // the automatic recall for a turn: the memory is in the addendum, with an id index
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
    assert!(
        addendum.contains(&format!("- {memory_id} — ")),
        "the id index"
    );

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

    // forgetting it
    let gone = inner
        .tools
        .memory_forget(&json!({"memoryId": memory_id}))
        .await;
    eprintln!("memory_forget: {gone}");
    assert_eq!(gone, format!("Memory {memory_id} forgotten."));
}
