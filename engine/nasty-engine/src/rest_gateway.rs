//! REST gateway in front of the JSON-RPC dispatcher.
//!
//! Each registered method `foo.bar.baz` is reachable at `/api/v1/foo/bar/baz`
//! with the HTTP verb [`super::registry::translate`] assigns. The handler is
//! a thin envelope translator:
//!
//!   1. Extract the session token from `nasty_session` cookie or
//!      `Authorization: Bearer` header (same path `/ws` uses).
//!   2. Validate the token via `AuthService` → `Session`.
//!   3. Convert the URL tail (`foo/bar/baz`) back to the method name.
//!   4. Build a JSON-RPC `Request` with `params` taken from the query string
//!      (GET) or the JSON body (everything else).
//!   5. Call [`crate::router::handle_rpc_request`] — the *same* function the
//!      WebSocket path uses, so role enforcement, audit logging, slow-call
//!      tracing, and post-mutation event broadcasts all fire identically.
//!   6. Unwrap the JSON-RPC envelope: `result` → 200 with the inner value,
//!      `error` → HTTP status mapped from the error message + code.
//!
//! Streaming endpoints (log_stream, terminal, vm_console, telemetry pushes)
//! stay on the WebSocket — they're not in the method registry and the
//! gateway doesn't try to model them.

use axum::{
    Json, Router,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Map, Value};
use std::sync::{Arc, OnceLock};
use tracing::debug;

use crate::AppState;
use crate::auth::AuthError;
use crate::registry::{HttpVerb, method_from_segments, translate};
use crate::router::handle_rpc_request;
use crate::token_from_headers;

/// Build the REST gateway sub-router. Mounted as a sibling of `/ws` and
/// `/api/login`.
pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route(
        "/api/v1/{*path}",
        get(gateway_get)
            .post(gateway_post)
            .put(gateway_put)
            .delete(gateway_delete),
    )
}

async fn gateway_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let params = query_to_json(query.as_deref().unwrap_or(""));
    dispatch(state, headers, &path, HttpVerb::Get, params).await
}

async fn gateway_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    dispatch(
        state,
        headers,
        &path,
        HttpVerb::Post,
        body.map(|j| j.0).unwrap_or(Value::Null),
    )
    .await
}

async fn gateway_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    dispatch(
        state,
        headers,
        &path,
        HttpVerb::Put,
        body.map(|j| j.0).unwrap_or(Value::Null),
    )
    .await
}

async fn gateway_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    dispatch(
        state,
        headers,
        &path,
        HttpVerb::Delete,
        body.map(|j| j.0).unwrap_or(Value::Null),
    )
    .await
}

async fn dispatch(
    state: Arc<AppState>,
    headers: HeaderMap,
    path_tail: &str,
    verb: HttpVerb,
    params: Value,
) -> Response {
    // 1. Path → method name + verb sanity check.
    let method = method_from_segments(path_tail);
    let Some(canonical_verb) = canonical_verb_for(&method) else {
        return (
            StatusCode::NOT_FOUND,
            json_error(format!("unknown RPC method: {method}")),
        )
            .into_response();
    };
    if canonical_verb != verb {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            json_error(format!(
                "method `{method}` is exposed as {}, not {}",
                canonical_verb.as_str().to_uppercase(),
                verb.as_str().to_uppercase()
            )),
        )
            .into_response();
    }

    // 2. Auth: pull token, validate → Session.
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let Some(token) = token_from_headers(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            json_error("missing session token"),
        )
            .into_response();
    };
    let session = match state.auth.validate(&token, &client_ip).await {
        Ok(s) => s,
        Err(e) => return auth_error_to_response(e),
    };

    // 3. Build JSON-RPC envelope. `id` is a synthesized UUID so audit/slow-
    //    call logs in `handle_rpc_request` have something to correlate on.
    let request_id = uuid::Uuid::new_v4().to_string();
    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": method,
        "params": params,
    });
    let raw = serde_json::to_string(&envelope).expect("envelope must serialize");

    debug!(
        "REST→RPC: {} {} (user: {})",
        verb.as_str().to_uppercase(),
        method,
        session.username
    );
    let response_str = handle_rpc_request(&raw, &state, &session).await;

    // 4. Unwrap.
    let response: Value = match serde_json::from_str(&response_str) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                json_error(format!("malformed engine response: {e}")),
            )
                .into_response();
        }
    };
    if let Some(err) = response.get("error") {
        let message = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-32603);
        let status = map_error_to_status(code, message);
        return (
            status,
            Json(serde_json::json!({
                "error": {"code": code, "message": message}
            })),
        )
            .into_response();
    }
    let result = response.get("result").cloned().unwrap_or(Value::Null);
    // Endpoints with no result (e.g. delete arms returning literal "ok") get
    // 204 No Content; otherwise 200 + body.
    if matches!(&result, Value::String(s) if s == "ok") || matches!(&result, Value::Null) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::OK, Json(result)).into_response()
    }
}

fn auth_error_to_response(err: AuthError) -> Response {
    let (status, msg) = match err {
        AuthError::InvalidToken => (StatusCode::UNAUTHORIZED, "invalid token"),
        AuthError::TokenExpired => (StatusCode::UNAUTHORIZED, "token expired"),
        _ => (StatusCode::UNAUTHORIZED, "authentication failed"),
    };
    (status, json_error(msg)).into_response()
}

/// Map JSON-RPC error code + message → HTTP status.
///
/// JSON-RPC's error model is one big -32603 bucket for everything outside the
/// parse/invalid-request range, so the message itself is the only signal for
/// most cases. We pattern-match on the well-known message prefixes the engine
/// emits.
fn map_error_to_status(code: i64, message: &str) -> StatusCode {
    if code == -32700 || code == -32600 || code == -32602 {
        return StatusCode::BAD_REQUEST;
    }
    if code == -32601 {
        return StatusCode::NOT_FOUND;
    }
    let lower = message.to_ascii_lowercase();
    if lower.contains("permission denied") || lower.contains("access denied") {
        return StatusCode::FORBIDDEN;
    }
    if lower.contains("not found") {
        return StatusCode::NOT_FOUND;
    }
    if lower.contains("missing params")
        || lower.contains("missing field")
        || lower.contains("invalid")
    {
        return StatusCode::BAD_REQUEST;
    }
    StatusCode::INTERNAL_SERVER_ERROR
}

/// Parse a URL-encoded query string into a JSON object, treating each value
/// as a JSON literal where it parses (so `?limit=200` becomes `{"limit": 200}`)
/// and as a plain string otherwise (so `?name=foo` becomes `{"name": "foo"}`).
fn query_to_json(query: &str) -> Value {
    if query.is_empty() {
        return Value::Null;
    }
    let mut obj = Map::new();
    for pair in query.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(t) => t,
            None => (pair, ""),
        };
        let k = url_decode(k);
        let v = url_decode(v);
        let parsed = serde_json::from_str::<Value>(&v).unwrap_or(Value::String(v));
        obj.insert(k, parsed);
    }
    Value::Object(obj)
}

fn url_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("k={s}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| s.to_string())
}

fn json_error(msg: impl Into<String>) -> Json<Value> {
    Json(serde_json::json!({"error": msg.into()}))
}

/// Cheap lookup: method name → canonical HTTP verb from the registry.
/// Built once at first call so REST requests don't pay the schemars cost.
fn canonical_verb_for(method: &str) -> Option<HttpVerb> {
    use std::collections::HashMap;
    static TABLE: OnceLock<HashMap<String, HttpVerb>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let (_g, groups) = crate::registry::build_full_registry();
        groups
            .into_iter()
            .flat_map(|(_, ms)| {
                ms.into_iter()
                    .map(|m| (m.name.to_string(), translate(m.name).0))
            })
            .collect()
    });
    table.get(method).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn query_to_json_parses_json_literals() {
        let v = query_to_json("limit=200&name=foo");
        assert_eq!(v, json!({"limit": 200, "name": "foo"}));
    }

    #[test]
    fn query_to_json_handles_empty() {
        assert_eq!(query_to_json(""), Value::Null);
    }

    #[test]
    fn query_to_json_keeps_string_for_non_json_values() {
        // `bar` is a bare identifier — not valid JSON. Treated as a string.
        let v = query_to_json("foo=bar");
        assert_eq!(v, json!({"foo": "bar"}));
    }

    #[test]
    fn query_to_json_handles_booleans_and_null() {
        let v = query_to_json("enabled=true&limit=null");
        assert_eq!(v, json!({"enabled": true, "limit": null}));
    }

    #[test]
    fn query_to_json_url_decodes_values() {
        let v = query_to_json("name=hello%20world");
        assert_eq!(v, json!({"name": "hello world"}));
    }

    #[test]
    fn error_mapping_permission_denied() {
        assert_eq!(
            map_error_to_status(-32603, "Permission denied"),
            StatusCode::FORBIDDEN
        );
        // Case-insensitive.
        assert_eq!(
            map_error_to_status(-32603, "ACCESS DENIED"),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn error_mapping_not_found() {
        assert_eq!(
            map_error_to_status(-32603, "filesystem not found: mypool"),
            StatusCode::NOT_FOUND
        );
        // JSON-RPC code -32601 = MethodNotFound.
        assert_eq!(
            map_error_to_status(-32601, "whatever"),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn error_mapping_bad_request_for_parse_errors() {
        assert_eq!(map_error_to_status(-32700, "x"), StatusCode::BAD_REQUEST);
        assert_eq!(map_error_to_status(-32600, "x"), StatusCode::BAD_REQUEST);
        assert_eq!(map_error_to_status(-32602, "x"), StatusCode::BAD_REQUEST);
        assert_eq!(
            map_error_to_status(-32603, "missing params"),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn error_mapping_falls_through_to_500() {
        assert_eq!(
            map_error_to_status(-32603, "something exploded"),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn canonical_verb_lookup_covers_known_methods() {
        // Cold lookup — also exercises the OnceLock init path.
        assert_eq!(canonical_verb_for("system.info"), Some(HttpVerb::Get));
        assert_eq!(
            canonical_verb_for("auth.delete_user"),
            Some(HttpVerb::Delete)
        );
        assert_eq!(
            canonical_verb_for("system.update.build_dir.set"),
            Some(HttpVerb::Put)
        );
        assert_eq!(canonical_verb_for("system.reboot"), Some(HttpVerb::Post));
        assert_eq!(canonical_verb_for("nonexistent.method"), None);
    }
}
