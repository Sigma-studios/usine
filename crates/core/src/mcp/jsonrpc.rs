//! The sliver of JSON-RPC 2.0 that MCP over stdio actually uses: one request
//! object per line, one response object per line, notifications answered with
//! silence. Four methods (`initialize`, `ping`, `tools/list`, `tools/call`) do
//! not justify an SDK, so this is hand-rolled over `serde_json` — a dependency
//! the crate already carries.

use serde_json::{json, Value};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

/// The MCP protocol revisions we can speak. `initialize` echoes the client's
/// version when it is one of these, and otherwise answers with our newest —
/// which is what the spec asks a server to do when it can't match.
pub const PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

pub fn latest_protocol() -> &'static str {
    PROTOCOL_VERSIONS[PROTOCOL_VERSIONS.len() - 1]
}

/// A parsed incoming message. `id` is absent for notifications, which must not
/// be answered at all.
pub struct Request {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

/// Parse one line. `Err` carries a ready-to-send error response (never `None`,
/// since a line we can't parse has no id to stay silent about).
pub fn parse(line: &str) -> std::result::Result<Request, Value> {
    let v: Value = serde_json::from_str(line)
        .map_err(|e| err(Value::Null, PARSE_ERROR, format!("invalid JSON: {e}")))?;
    let id = v.get("id").cloned().filter(|i| !i.is_null());
    let method = match v.get("method").and_then(Value::as_str) {
        Some(m) => m.to_string(),
        None => {
            return Err(err(
                id.unwrap_or(Value::Null),
                INVALID_REQUEST,
                "missing `method`",
            ))
        }
    };
    let params = v.get("params").cloned().unwrap_or_else(|| json!({}));
    Ok(Request { id, method, params })
}

pub fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn err(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() },
    })
}
