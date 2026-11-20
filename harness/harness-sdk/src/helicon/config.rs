//! The options of the HeliconSearch memory extension and their validation.
//!
//! The file format is the one the TypeScript SDK reads (`helicon-memory.json`), so one file serves an
//! application written in either language.
use serde_json::{Map, Value};

/// The server's token: a string that does not print itself (a `{:?}` of the configuration, in a log or a
/// panic message, must not carry it).
#[derive(Clone)]
pub(crate) struct Secret(String);

impl Secret {
    pub(crate) fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }
}

impl std::ops::Deref for Secret {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

pub(crate) const DEFAULT_AGENT_ID: &str = "openworker";
pub(crate) const DEFAULT_TOP_K: usize = 5;
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 10_000;
pub(crate) const DEFAULT_CAPTURE_MAX_CHARS: usize = 500;
pub(crate) const DEFAULT_RECALL_MAX_CHARS: usize = 1000;
/// A floor, not a relevance cut: HeliconSearch's recall scores are fused and reranked.
pub(crate) const DEFAULT_RECALL_MIN_SCORE: f64 = 0.01;
pub(crate) const DEFAULT_RECALL_TOP_K: usize = 12;
/// Sent when no credential is configured; never a valid token (real ones start with `hs_`).
pub(crate) const UNCONFIGURED_API_KEY: &str = "openclaw-unconfigured-tenant";

pub(crate) const MEMORY_CATEGORIES: [&str; 8] = [
    "preference",
    "fact",
    "decision",
    "entity",
    "instruction",
    "episode",
    "pattern",
    "other",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureMode {
    /// No model: user sentences that match a few keyword rules are stored as they are.
    Direct,
    /// HeliconSearch's model extracts, merges and updates facts from the whole conversation.
    Operator,
}

#[derive(Clone, Debug)]
pub(crate) struct MemoryConfig {
    pub base_url: String,
    pub agent_id: String,
    pub project_id: String,
    pub top_k: usize,
    pub timeout_ms: u64,
    pub auto_recall: bool,
    pub auto_capture: bool,
    /// Whether the agent gets the `memory_store` tool.
    pub memory_store_tool: bool,
    /// Whether the agent gets the media tools (`media_search`, `media_upload`).
    pub auto_media: bool,
    /// Also `media_search_by_image`, `media_libraries`, `media_files` and `media_statistics`.
    pub media_admin_tools: bool,
    /// Results a search returns when the model does not say.
    pub media_max_results: usize,
    /// 0-3; 2 is the VLM + CLIP hybrid.
    pub media_search_mode: i64,
    /// The library `media_upload`, `media_files` and `media_statistics` use when none is named.
    pub media_kb_id: String,
    pub media_rerank_model: String,
    /// Weaker matches are dropped (0: none is).
    pub media_min_similarity: f64,
    /// Read for the file the TypeScript SDK shares; not supported here (the result of an application tool
    /// cannot carry an image).
    pub media_inline_images: bool,
    pub capture_mode: CaptureMode,
    pub capture_max_chars: usize,
    pub store_gate: bool,
    pub recall_gate: bool,
    pub recall_max_chars: usize,
    pub recall_min_score: f64,
    pub recall_top_k: usize,
    pub custom_triggers: Option<Vec<String>>,
    pub api_key: Secret,
    pub debug: bool,
    pub sync_transcript: bool,
    /// Ask HeliconSearch for the session's summary right after ending it (when it did not make one itself).
    pub summary_on_session_end: bool,
    pub transcript_max_chars_per_turn: usize,
    pub session_bridge: bool,
    pub session_bridge_episodic_limit: usize,
    pub session_bridge_pinned_min_importance: f64,
    pub session_bridge_pinned_min_recall: usize,
    pub session_bridge_pinned_top_k: usize,
    pub session_bridge_procedure_limit: usize,
    pub session_bridge_token_budget: usize,
    /// How long a turn may wait for memory before it goes on without it.
    pub hook_timeout_ms: u64,
    /// Agents get the HeliconSearch memory tools instead of the engine's built-in `memory_*` tools.
    pub replace_builtin_memory: bool,
    /// How long closing a session waits for its fact extraction to finish (0: do not wait).
    pub close_wait_ms: u64,
    /// Shutting the harness down ends the sessions this process captured.
    pub end_on_shutdown: bool,
    /// A session untouched for this long is ended, and one left open by a run that died is ended at the
    /// next start; 0: never.
    pub idle_end_ms: u64,
    /// Only agents of these names get memory; `None`: every agent of the harness.
    pub agents: Option<Vec<String>>,
}

const ALLOWED_KEYS: &[&str] = &[
    "baseUrl",
    "agentId",
    "projectId",
    "topK",
    "timeoutMs",
    "autoRecall",
    "autoCapture",
    "memoryStoreTool",
    "captureMode",
    "captureMaxChars",
    "storeGate",
    "recallGate",
    "recallMaxChars",
    "recallMinScore",
    "recallTopK",
    "customTriggers",
    "apiKey",
    "debug",
    "syncTranscript",
    "summaryOnSessionEnd",
    "transcriptMaxCharsPerTurn",
    "autoMedia",
    "mediaAdminTools",
    "mediaMaxResults",
    "mediaSearchMode",
    "mediaKbId",
    "mediaRerankModel",
    "mediaMinSimilarity",
    "mediaInlineImages",
    "sessionBridge",
    "sessionBridgeEpisodicLimit",
    "sessionBridgePinnedMinImportance",
    "sessionBridgePinnedMinRecall",
    "sessionBridgePinnedTopK",
    "sessionBridgeProcedureLimit",
    "sessionBridgeTokenBudget",
    "hookTimeoutMs",
    "replaceBuiltinMemory",
    "closeWaitMs",
    "endOnShutdown",
    "idleEndMs",
    "agents",
];

type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Replaces `${NAME}` with the environment variable NAME; an unset variable is an error.
fn resolve_env(value: &str, env: Env) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let name = &after[..end];
        match env(name) {
            Some(v) => out.push_str(&v),
            None => return Err(format!("Environment variable {name} is not set")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Never fails and never yields "": a `${VAR}` that is unset or empty is an unconfigured credential.
fn resolve_api_key(raw: Option<&Value>, env: Env) -> String {
    let Some(text) = raw.and_then(Value::as_str).filter(|t| !t.trim().is_empty()) else {
        return UNCONFIGURED_API_KEY.into();
    };
    match resolve_env(text, env) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_owned(),
        _ => UNCONFIGURED_API_KEY.into(),
    }
}

fn normalize_base_url(raw: &str, env: Env) -> Result<String, String> {
    let resolved = resolve_env(raw, env)?.trim().to_owned();
    if resolved.is_empty() {
        return Err("baseUrl must not be empty".into());
    }
    let url = reqwest::Url::parse(&resolved)
        .map_err(|_| format!("baseUrl is not a valid URL: {resolved}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("baseUrl is not a valid URL: {resolved}"));
    }
    Ok(format!(
        "{}{}",
        url.origin().ascii_serialization(),
        url.path().trim_end_matches('/')
    ))
}

fn number(raw: Option<&Value>, label: &str) -> Result<Option<f64>, String> {
    match raw {
        None => Ok(None),
        Some(v) => match v.as_f64() {
            Some(n) if n.is_finite() => Ok(Some(n)),
            _ => Err(format!("{label} must be a finite number")),
        },
    }
}

fn int_in_range(
    raw: Option<&Value>,
    label: &str,
    min: i64,
    max: i64,
    fallback: i64,
) -> Result<i64, String> {
    let Some(n) = number(raw, label)? else {
        return Ok(fallback);
    };
    let value = n.floor() as i64;
    if value < min || value > max {
        return Err(format!("{label} must be between {min} and {max}"));
    }
    Ok(value)
}

fn float_in_range(
    raw: Option<&Value>,
    label: &str,
    min: f64,
    max: f64,
    fallback: f64,
) -> Result<f64, String> {
    let Some(n) = number(raw, label)? else {
        return Ok(fallback);
    };
    if n < min || n > max {
        return Err(format!("{label} must be between {min} and {max}"));
    }
    Ok(n)
}

fn boolean(raw: Option<&Value>, fallback: bool) -> Result<bool, String> {
    match raw {
        None => Ok(fallback),
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => Err(format!("expected boolean, got {}", type_name(other))),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn agent_names(raw: Option<&Value>) -> Result<Option<Vec<String>>, String> {
    let Some(raw) = raw else { return Ok(None) };
    let err = || "agents must be an array of agent names (non-empty strings)".to_owned();
    let list = raw.as_array().ok_or_else(err)?;
    let mut out = Vec::new();
    for item in list {
        let name = item.as_str().ok_or_else(err)?;
        if name.trim().is_empty() || name.chars().count() > 100 {
            return Err(err());
        }
        out.push(name.trim().to_owned());
    }
    if out.len() > 100 {
        return Err("agents must list at most 100 names".into());
    }
    Ok(Some(out))
}

fn custom_triggers(raw: Option<&Value>) -> Result<Option<Vec<String>>, String> {
    let Some(raw) = raw else { return Ok(None) };
    let list = raw
        .as_array()
        .ok_or("customTriggers must be an array of strings")?;
    let mut out = Vec::new();
    for (i, item) in list.iter().enumerate() {
        let text = item
            .as_str()
            .ok_or_else(|| format!("customTriggers.{i} must be a string"))?
            .trim();
        if text.is_empty() {
            return Err(format!("customTriggers.{i} must not be empty"));
        }
        if text.chars().count() > 100 {
            return Err(format!("customTriggers.{i} must be at most 100 characters"));
        }
        out.push(text.to_owned());
    }
    if out.len() > 50 {
        return Err("customTriggers must include at most 50 entries".into());
    }
    Ok(Some(out))
}

fn capture_mode(raw: Option<&Value>) -> Result<CaptureMode, String> {
    match raw.map(|v| v.as_str()) {
        None => Ok(CaptureMode::Operator),
        Some(Some("operator")) => Ok(CaptureMode::Operator),
        Some(Some("direct")) => Ok(CaptureMode::Direct),
        Some(_) => Err("captureMode must be one of: direct, operator".into()),
    }
}

impl MemoryConfig {
    /// Validates the options of a configuration file (without its `enabled` flag).
    pub(crate) fn parse(value: &Value, env: Env) -> Result<Self, String> {
        let cfg: &Map<String, Value> = value.as_object().ok_or("helicon-memory config required")?;
        let unknown: Vec<&str> = cfg
            .keys()
            .map(String::as_str)
            .filter(|k| !ALLOWED_KEYS.contains(k))
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "helicon-memory config has unknown keys: {}",
                unknown.join(", ")
            ));
        }
        let get = |k: &str| cfg.get(k);
        let base = get("baseUrl")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or("baseUrl is required")?;
        // (a number or a list here would silently put the memories in another agent's partition)
        for key in ["agentId", "projectId"] {
            if get(key).is_some_and(|v| !v.is_string() && !v.is_null()) {
                return Err(format!("{key} must be a string"));
            }
        }
        let agent_id = match get("agentId").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => resolve_env(s, env)?.trim().to_owned(),
            _ => DEFAULT_AGENT_ID.to_owned(),
        };
        let project_id = match get("projectId").and_then(Value::as_str) {
            Some(s) => resolve_env(s, env)?.trim().to_owned(),
            None => String::new(),
        };
        let int = |k: &str, min: i64, max: i64, d: i64| int_in_range(get(k), k, min, max, d);
        let float = |k: &str, min: f64, max: f64, d: f64| float_in_range(get(k), k, min, max, d);
        let idle_end_ms = if get("idleEndMs").and_then(Value::as_f64) == Some(0.0) {
            0
        } else {
            int("idleEndMs", 1_000, 86_400_000, 1_800_000)? as u64
        };
        Ok(Self {
            base_url: normalize_base_url(base, env)?,
            agent_id,
            project_id,
            top_k: int("topK", 1, 50, DEFAULT_TOP_K as i64)? as usize,
            timeout_ms: int("timeoutMs", 1_000, 120_000, DEFAULT_TIMEOUT_MS as i64)? as u64,
            auto_recall: boolean(get("autoRecall"), true)?,
            auto_capture: boolean(get("autoCapture"), true)?,
            memory_store_tool: boolean(get("memoryStoreTool"), false)?,
            auto_media: boolean(get("autoMedia"), true)?,
            media_admin_tools: boolean(get("mediaAdminTools"), false)?,
            media_max_results: int("mediaMaxResults", 1, 100, 5)? as usize,
            media_search_mode: int("mediaSearchMode", 0, 3, 2)?,
            media_kb_id: get("mediaKbId")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_owned())
                .unwrap_or_default(),
            media_rerank_model: get("mediaRerankModel")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("BGE")
                .to_owned(),
            media_min_similarity: float("mediaMinSimilarity", 0.0, 1.0, 0.05)?,
            media_inline_images: boolean(get("mediaInlineImages"), false)?,
            capture_mode: capture_mode(get("captureMode"))?,
            capture_max_chars: int(
                "captureMaxChars",
                100,
                10_000,
                DEFAULT_CAPTURE_MAX_CHARS as i64,
            )? as usize,
            store_gate: boolean(get("storeGate"), true)?,
            recall_gate: boolean(get("recallGate"), true)?,
            recall_max_chars: int(
                "recallMaxChars",
                100,
                10_000,
                DEFAULT_RECALL_MAX_CHARS as i64,
            )? as usize,
            recall_min_score: float("recallMinScore", 0.0, 1.0, DEFAULT_RECALL_MIN_SCORE)?,
            recall_top_k: int("recallTopK", 1, 20, DEFAULT_RECALL_TOP_K as i64)? as usize,
            custom_triggers: custom_triggers(get("customTriggers"))?,
            api_key: Secret::new(resolve_api_key(get("apiKey"), env)),
            debug: boolean(get("debug"), false)?,
            sync_transcript: boolean(get("syncTranscript"), true)?,
            summary_on_session_end: boolean(get("summaryOnSessionEnd"), true)?,
            transcript_max_chars_per_turn: int("transcriptMaxCharsPerTurn", 100, 50_000, 20_000)?
                as usize,
            session_bridge: boolean(get("sessionBridge"), true)?,
            session_bridge_episodic_limit: int("sessionBridgeEpisodicLimit", 1, 20, 7)? as usize,
            session_bridge_pinned_min_importance: float(
                "sessionBridgePinnedMinImportance",
                0.0,
                1.0,
                0.85,
            )?,
            session_bridge_pinned_min_recall: int("sessionBridgePinnedMinRecall", 1, 100, 5)?
                as usize,
            session_bridge_pinned_top_k: int("sessionBridgePinnedTopK", 1, 50, 20)? as usize,
            session_bridge_procedure_limit: int("sessionBridgeProcedureLimit", 0, 20, 5)? as usize,
            session_bridge_token_budget: int("sessionBridgeTokenBudget", 100, 8_000, 2_500)?
                as usize,
            hook_timeout_ms: int("hookTimeoutMs", 1_000, 120_000, 15_000)? as u64,
            replace_builtin_memory: boolean(get("replaceBuiltinMemory"), true)?,
            close_wait_ms: int("closeWaitMs", 0, 300_000, 0)? as u64,
            end_on_shutdown: boolean(get("endOnShutdown"), true)?,
            idle_end_ms,
            agents: agent_names(get("agents"))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(name: &str) -> Option<String> {
        (name == "HS_TOKEN").then(|| "hs_secret".to_owned())
    }

    #[test]
    fn defaults_and_url_normalisation() {
        let cfg = MemoryConfig::parse(
            &json!({"baseUrl":"http://127.0.0.1:5001/","apiKey":"${HS_TOKEN}"}),
            &env,
        )
        .unwrap();
        assert_eq!(cfg.base_url, "http://127.0.0.1:5001");
        assert_eq!(&*cfg.api_key, "hs_secret");
        assert!(
            !format!("{cfg:?}").contains("hs_secret"),
            "the token is not printed"
        );
        assert_eq!(cfg.agent_id, "openworker");
        assert_eq!(cfg.capture_mode, CaptureMode::Operator);
        assert!(cfg.replace_builtin_memory && cfg.auto_recall && cfg.auto_capture);
        assert_eq!(cfg.idle_end_ms, 1_800_000);
        assert_eq!(cfg.hook_timeout_ms, 15_000);
    }

    #[test]
    fn an_unset_credential_variable_is_an_unconfigured_key_not_an_error() {
        let cfg = MemoryConfig::parse(
            &json!({"baseUrl":"http://hs:5001","apiKey":"${NOT_SET}"}),
            &env,
        )
        .unwrap();
        assert_eq!(&*cfg.api_key, UNCONFIGURED_API_KEY);
    }

    #[test]
    fn unknown_keys_and_bad_values_are_refused() {
        let bad = |v: Value| MemoryConfig::parse(&v, &env).unwrap_err();
        assert!(bad(json!({"baseUrl":"http://h","nope":1})).contains("unknown keys: nope"));
        assert!(bad(json!({})).contains("baseUrl is required"));
        assert!(bad(json!({"baseUrl":"not a url"})).contains("not a valid URL"));
        assert!(
            bad(json!({"baseUrl":"http://h","topK":0})).contains("topK must be between 1 and 50")
        );
        assert!(bad(json!({"baseUrl":"http://h","captureMode":"x"})).contains("captureMode"));
        assert!(bad(json!({"baseUrl":"http://h","autoRecall":"yes"})).contains("expected boolean"));
        // an id that is not a string would silently be another partition
        assert!(
            bad(json!({"baseUrl":"http://h","agentId":42})).contains("agentId must be a string")
        );
        assert!(bad(json!({"baseUrl":"http://h","projectId":["p"]}))
            .contains("projectId must be a string"));
    }

    #[test]
    fn idle_end_zero_means_never_however_it_is_written() {
        for zero in [json!(0), json!(0.0)] {
            let cfg = MemoryConfig::parse(&json!({"baseUrl":"http://h","idleEndMs": zero}), &env)
                .unwrap();
            assert_eq!(cfg.idle_end_ms, 0);
        }
    }

    #[test]
    fn the_media_options_default_to_the_typescript_sdks() {
        let cfg = MemoryConfig::parse(&json!({"baseUrl":"http://h"}), &env).unwrap();
        assert!(cfg.auto_media && !cfg.media_admin_tools && !cfg.media_inline_images);
        assert_eq!((cfg.media_max_results, cfg.media_search_mode), (5, 2));
        assert_eq!(cfg.media_kb_id, "");
        assert_eq!(cfg.media_rerank_model, "BGE");
        assert_eq!(cfg.media_min_similarity, 0.05);
    }

    #[test]
    fn the_media_options_are_read_and_checked() {
        let cfg = MemoryConfig::parse(
            &json!({"baseUrl":"http://h","autoMedia":false,"mediaAdminTools":true,"mediaMaxResults":3,
                "mediaSearchMode":0,"mediaKbId":" kb1 ","mediaRerankModel":" M ","mediaMinSimilarity":0,
                "mediaInlineImages":true,"idleEndMs":0,"agents":["a"]}),
            &env,
        )
        .unwrap();
        assert!(!cfg.auto_media && cfg.media_admin_tools && cfg.media_inline_images);
        assert_eq!((cfg.media_max_results, cfg.media_search_mode), (3, 0));
        assert_eq!(cfg.media_kb_id, "kb1");
        assert_eq!(cfg.media_rerank_model, "M");
        assert_eq!(cfg.media_min_similarity, 0.0);
        assert_eq!(cfg.idle_end_ms, 0);
        assert_eq!(cfg.agents, Some(vec!["a".to_owned()]));
        for (key, bad) in [
            ("mediaMaxResults", json!(0)),
            ("mediaMaxResults", json!(101)),
            ("mediaSearchMode", json!(4)),
            ("mediaMinSimilarity", json!(1.5)),
            ("autoMedia", json!("yes")),
        ] {
            let mut raw = json!({"baseUrl":"http://h"});
            raw[key] = bad.clone();
            assert!(MemoryConfig::parse(&raw, &env).is_err(), "{key}={bad}");
        }
        // a blank rerank model is the default one
        let blank =
            MemoryConfig::parse(&json!({"baseUrl":"http://h","mediaRerankModel":"  "}), &env)
                .unwrap();
        assert_eq!(blank.media_rerank_model, "BGE");
    }
}
