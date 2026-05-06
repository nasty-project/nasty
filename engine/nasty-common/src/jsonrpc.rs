use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    pub id: serde_json::Value,
}

/// JSON-RPC 2.0 successful response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
    pub id: serde_json::Value,
}

/// JSON-RPC 2.0 notification (no id, no response expected)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// JSON-RPC 2.0 error object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Error {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Standard JSON-RPC 2.0 error codes
#[derive(Debug, Clone, Copy)]
pub enum ErrorCode {
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,
}

impl Response {
    pub fn success(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            result: Some(result),
            error: None,
            id,
        }
    }

    pub fn error(id: serde_json::Value, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            result: None,
            error: Some(Error {
                code: code as i64,
                message: message.into(),
                data: None,
            }),
            id,
        }
    }
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    // ── error code spec ─────────────────────────────────────────────

    #[test]
    fn error_codes_match_jsonrpc_2_spec() {
        // These numbers are part of the JSON-RPC 2.0 wire contract; clients
        // compare against them. Pinning them prevents accidental drift.
        assert_eq!(ErrorCode::ParseError as i64, -32700);
        assert_eq!(ErrorCode::InvalidRequest as i64, -32600);
        assert_eq!(ErrorCode::MethodNotFound as i64, -32601);
        assert_eq!(ErrorCode::InvalidParams as i64, -32602);
        assert_eq!(ErrorCode::InternalError as i64, -32603);
    }

    // ── Response envelope shape ─────────────────────────────────────

    #[test]
    fn response_success_serializes_with_only_result() {
        let r = Response::success(json!(1), json!({"ok": true}));
        let v: Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"], json!({"ok": true}));
        assert!(
            v.get("error").is_none(),
            "successful response must omit `error`"
        );
    }

    #[test]
    fn response_error_serializes_with_only_error_object() {
        let r = Response::error(json!("req-1"), ErrorCode::MethodNotFound, "no such method");
        let v: Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], "req-1");
        assert_eq!(v["error"]["code"], -32601);
        assert_eq!(v["error"]["message"], "no such method");
        assert!(
            v.get("result").is_none(),
            "error response must omit `result`"
        );
        // `data` is optional and absent unless explicitly set.
        assert!(v["error"].get("data").is_none());
    }

    #[test]
    fn response_error_carries_numeric_code_for_each_variant() {
        for (code, expected) in [
            (ErrorCode::ParseError, -32700),
            (ErrorCode::InvalidRequest, -32600),
            (ErrorCode::InvalidParams, -32602),
            (ErrorCode::InternalError, -32603),
        ] {
            let r = Response::error(json!(0), code, "x");
            let v: Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
            assert_eq!(v["error"]["code"], expected);
        }
    }

    #[test]
    fn response_round_trips_through_serde() {
        let original = Response::success(json!(42), json!([1, 2, 3]));
        let bytes = serde_json::to_string(&original).unwrap();
        let parsed: Response = serde_json::from_str(&bytes).unwrap();
        assert_eq!(parsed.jsonrpc, "2.0");
        assert_eq!(parsed.id, json!(42));
        assert_eq!(parsed.result, Some(json!([1, 2, 3])));
        assert!(parsed.error.is_none());
    }

    // ── Request parsing ─────────────────────────────────────────────

    #[test]
    fn request_parses_with_string_id() {
        let req: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"fs.list","params":{},"id":"abc"}"#)
                .unwrap();
        assert_eq!(req.id, json!("abc"));
        assert_eq!(req.method, "fs.list");
    }

    #[test]
    fn request_parses_with_numeric_id() {
        let req: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"fs.list","id":7}"#).unwrap();
        assert_eq!(req.id, json!(7));
    }

    #[test]
    fn request_parses_with_null_id() {
        let req: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"fs.list","id":null}"#).unwrap();
        assert_eq!(req.id, Value::Null);
    }

    #[test]
    fn request_parses_without_params() {
        let req: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"system.health","id":1}"#).unwrap();
        assert!(req.params.is_none());
    }

    #[test]
    fn request_rejects_missing_method() {
        let res: Result<Request, _> = serde_json::from_str(r#"{"jsonrpc":"2.0","id":1}"#);
        assert!(res.is_err());
    }

    #[test]
    fn request_rejects_missing_id() {
        // Without id, this would be a Notification — Request parse should fail.
        let res: Result<Request, _> = serde_json::from_str(r#"{"jsonrpc":"2.0","method":"x"}"#);
        assert!(res.is_err());
    }

    // ── Notification ────────────────────────────────────────────────

    #[test]
    fn notification_new_omits_id_field_in_serialized_form() {
        let n = Notification::new("alerts.changed", Some(json!({"count": 3})));
        let v: Value = serde_json::from_str(&serde_json::to_string(&n).unwrap()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "alerts.changed");
        assert_eq!(v["params"], json!({"count": 3}));
        assert!(
            v.get("id").is_none(),
            "notifications must not carry an id field"
        );
    }

    #[test]
    fn notification_new_without_params_omits_params_field() {
        let n = Notification::new("ping", None);
        let v: Value = serde_json::from_str(&serde_json::to_string(&n).unwrap()).unwrap();
        assert!(v.get("params").is_none());
    }
}
