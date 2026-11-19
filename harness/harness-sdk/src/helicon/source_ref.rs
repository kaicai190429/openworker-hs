//! Chain-of-evidence source references for HeliconSearch memories, in the wire shape of its
//! `services/memory/source_ref.py` so the JSON we post decodes server-side without translation.
use super::util::iso_now;
use serde_json::{json, Value};

/// A session anchor: `ref_id` is the HeliconSearch session id (already carrying the `openworker/` prefix).
pub(crate) fn make_session_ref(session_id: &str) -> Value {
    json!({"type": "session", "ref_id": session_id, "extracted_at": iso_now()})
}

/// A reference for a write nobody extracted from a message (`label_<8 hex>`).
pub(crate) fn make_manual_ref(label: &str) -> Value {
    let unique = openworker_runtime::id("");
    json!({
        "type": "manual",
        "ref_id": format!("{label}_{}", &unique[1..9]),
        "extracted_at": iso_now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_have_the_server_shape() {
        let s = make_session_ref("openworker/s1");
        assert_eq!(s["type"], "session");
        assert_eq!(s["ref_id"], "openworker/s1");
        let m = make_manual_ref("agent_tool");
        let id = m["ref_id"].as_str().unwrap();
        assert!(id.starts_with("agent_tool_") && id.len() == "agent_tool_".len() + 8);
        assert_ne!(
            make_manual_ref("a")["ref_id"],
            make_manual_ref("a")["ref_id"]
        );
    }
}
