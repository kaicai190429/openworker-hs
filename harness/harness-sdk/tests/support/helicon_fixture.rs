//! What the HeliconSearch integration tests share: a fake HeliconSearch that remembers a fact, archives
//! what it is sent and ends sessions; the extension's configuration file; and small waiting helpers.
#![allow(dead_code)]
use crate::hs_mock::{MockHs, Reply, Request};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

pub const MEMORY_ID: &str = "0123456789abcdef0123456789abcdef";

pub fn runner() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_sdk-test-host"))
}

pub fn workspace(root: &std::path::Path) -> PathBuf {
    let w = root.join("work");
    std::fs::create_dir_all(&w).unwrap();
    w
}

/// A HeliconSearch that remembers a fact, archives what it is sent and ends sessions (writing the summary itself).
pub async fn hs() -> MockHs {
    hs_with(true).await
}

/// The same; `summary_made: false` is one that ends a session without having written its summary (the
/// extension then has to ask for it).
pub async fn hs_with(summary_made: bool) -> MockHs {
    MockHs::start(move |req| match req.path.split('?').next().unwrap() {
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
        "/v1/memory/session/end" if summary_made => Reply::ok(json!({"status": "closed", "memory_id": "summary1"})),
        "/v1/memory/session/end" => Reply::ok(json!({"status": "closed"})),
        "/v1/memory/session/summary" => Reply::ok(json!({"status": "ok", "memory_id": "summary2"})),
        // what memory_get reads: the record, where it was said, and the conversation around it
        "/v1/memory/explain" => Reply::ok(json!({
            "memory": {"id": MEMORY_ID, "text": "likes tea", "memory_type": "semantic",
                "category": "preference", "importance": 0.8},
            "supersedes_chain": [], "source_completeness": "complete"
        })),
        p if p.starts_with("/v1/memory/source/") => Reply::ok(json!({
            "turns": [{"session_id": "openworker/earlier", "turn_index": 1}], "evidence": [], "source_refs": []
        })),
        "/v1/memory/session/turns" => Reply::ok(json!({
            "turns": [
                {"turn_index": 0, "role": "assistant", "content": "What would you like?", "created_at": null, "omitted": false},
                {"turn_index": 1, "role": "user", "content": "I like tea", "created_at": null, "omitted": false}
            ],
            "total": 2, "first_turn": 0, "last_turn": 1, "more": false
        })),
        // what memory_forget, memory_store and skill_get use
        p if p.starts_with("/v1/memory/skill/") => Reply::ok(json!({"content": "# Tea skill\nBrew it."})),
        "/v1/memory/store" => Reply::ok(json!({"memory_id": "fedcba9876543210fedcba9876543210", "action": "created"})),
        p if req.method == "DELETE" && p.starts_with("/v1/memory/") => Reply::ok(json!({"deleted": true})),
        // the multimedia API the media tools use: one library with one hit, answered as {data, retcode, retmsg}
        "/v1/kb/list" => media(json!({"kb_list": [{"id": "kb1", "name": "photos", "fileNum": 2}]})),
        "/v1/kb/resolve_default" => media(json!({"kb_id": "kb1", "name": "photos", "created": false, "doc_count": 2})),
        "/v1/document/search" => media(json!({"data_list": [{"file": "/photos/car.jpg", "type": "image",
            "similarity": 0.8, "start": 0, "end": 0, "image": "/photos/car.jpg"}]})),
        "/v1/document/upload" => media(json!({"batch_id": "b1"})),
        other => panic!("unexpected request to the fake HeliconSearch: {other}"),
    })
    .await
}

fn media(data: Value) -> Reply {
    Reply::ok(json!({"data": data, "retcode": 0, "retmsg": "success"}))
}

/// Writes the extension's configuration into the state directory.
pub fn configure(root: &std::path::Path, hs: &MockHs, extra: Value) {
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

pub async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), f)
        .await
        .unwrap()
}

/// Waits until the fake HeliconSearch has seen a request on `path`.
pub async fn seen(hs: &MockHs, path: &str) -> Request {
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

pub fn all(hs: &MockHs, path: &str) -> Vec<Request> {
    hs.requests()
        .into_iter()
        .filter(|r| r.path.split('?').next() == Some(path))
        .collect()
}
