//! Newline-delimited JSON-RPC 2.0 for the stdio MCP server.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.7. The server accepts one JSON
//! object per line and answers requests with one JSON object per line. Notifications
//! (objects without an `id` member) never get a response.

use serde_json::{Value, json};

/// JSON-RPC parse error.
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC invalid request.
pub const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC method not found.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC invalid params.
pub const INVALID_PARAMS: i64 = -32602;

/// One parsed request or notification.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// Request id; `None` marks a notification. `Some(Value::Null)` is still a request.
    pub id: Option<Value>,
    /// Method name.
    pub method: String,
    /// Method parameters; `Null` when the member is absent.
    pub params: Value,
}

impl Request {
    /// Returns whether this line is a notification (no `id` member).
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }

    /// Returns the id to echo, using JSON null for a malformed request.
    pub fn id_value(&self) -> Value {
        self.id.clone().unwrap_or(Value::Null)
    }
}

/// Parses one line, returning a ready-to-write error response on malformed input.
pub fn parse(line: &str) -> Result<Request, Value> {
    let value: Value = serde_json::from_str(line)
        .map_err(|error| error_response(Value::Null, PARSE_ERROR, &format!("parse error: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| error_response(Value::Null, INVALID_REQUEST, "invalid request: not an object"))?;
    let id = object.get("id").cloned();
    let response_id = id.clone().unwrap_or(Value::Null);

    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(error_response(
            response_id,
            INVALID_REQUEST,
            "invalid request: jsonrpc must be \"2.0\"",
        ));
    }
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty())
        .ok_or_else(|| error_response(response_id.clone(), INVALID_REQUEST, "invalid request: missing method"))?
        .to_owned();
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    if !params.is_null() && !params.is_object() {
        return Err(error_response(
            response_id,
            INVALID_PARAMS,
            "invalid params: expected an object",
        ));
    }
    Ok(Request { id, method, params })
}

/// Builds a successful JSON-RPC response.
pub fn success(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Builds a JSON-RPC error response.
pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_and_notification() {
        let request = parse(r#"{"jsonrpc":"2.0","id":7,"method":"ping","params":{}}"#).expect("request");
        assert_eq!(request.id, Some(json!(7)));
        assert!(!request.is_notification());
        assert_eq!(request.method, "ping");

        let notification = parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).expect("notification");
        assert!(notification.is_notification());
        assert_eq!(notification.params, Value::Null);
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        let response = parse("{").expect_err("parse error");
        assert_eq!(response["error"]["code"], PARSE_ERROR);
        assert_eq!(response["id"], Value::Null);
    }

    #[test]
    fn wrong_version_is_an_invalid_request() {
        let response = parse(r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#).expect_err("invalid");
        assert_eq!(response["error"]["code"], INVALID_REQUEST);
        assert_eq!(response["id"], 1);
    }
}
