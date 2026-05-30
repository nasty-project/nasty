//! Render the method registry as an OpenAPI 3.1 document.
//!
//! Consumed by Swagger UI at `/api/docs` (PR3) and emitted to disk by
//! `nasty-engine --dump-docs`.
//!
//! OpenAPI 3.1 uses JSON Schema 2020-12 natively, which matches what schemars
//! emits — so the params/result schemas drop in with only minor rewrites:
//!   - `$ref: "#/$defs/X"` is rewritten to `$ref: "#/components/schemas/X"`
//!   - all per-type `$defs` collections are merged into `components.schemas`
//!
//! Paths and verbs come from [`super::paths::translate`] so the REST gateway
//! and the docs site agree on every URL.

use super::paths::{HttpVerb, translate};
use super::{Method, MethodParams, MethodRole};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Build the full OpenAPI 3.1 document as a `serde_json::Value`.
pub fn render_openapi(version: &'static str, groups: &[(&str, Vec<Method>)]) -> Value {
    let mut paths: Map<String, Value> = Map::new();
    let mut schemas: BTreeMap<String, Value> = BTreeMap::new();

    for (group, methods) in groups {
        for m in methods {
            let (verb, path) = translate(m.name);
            let op = build_operation(group, m, &mut schemas);

            let entry = paths
                .entry(path)
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(map) = entry {
                map.insert(verb.as_str().to_string(), op);
            }
        }
    }

    let components_schemas: Map<String, Value> = schemas.into_iter().collect();

    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "NASty JSON-RPC API (REST gateway)",
            "version": version,
            "description": "NASty's JSON-RPC API surface, exposed over a thin REST gateway.\n\nEach JSON-RPC method `foo.bar.baz` is mounted at `/api/v1/foo/bar/baz` with an HTTP verb inferred from the method's last segment (`.get`/`.list`/`.status` → GET, `.delete`/`.remove` → DELETE, `.set`/`.update` → PUT, everything else → POST).\n\nGET methods take their params as query string; everything else takes a JSON body. Authentication mirrors the WebSocket transport: send the `nasty_session` cookie or an `Authorization: Bearer <token>` header.\n\nStreaming endpoints (log_stream, terminal, vm_console, telemetry pushes, event broadcasts) stay on the WebSocket at `/ws` and are intentionally not modeled here.",
        },
        "servers": [{"url": "/", "description": "This engine"}],
        "paths": Value::Object(paths),
        "components": {
            "schemas": Value::Object(components_schemas),
            "securitySchemes": {
                "cookieAuth": {
                    "type": "apiKey",
                    "in": "cookie",
                    "name": "nasty_session",
                    "description": "Session cookie set by `POST /api/login`."
                },
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "Long-lived API token created via `auth.token.create`."
                }
            }
        },
        "security": [{"cookieAuth": []}, {"bearerAuth": []}]
    })
}

fn build_operation(group: &str, m: &Method, schemas: &mut BTreeMap<String, Value>) -> Value {
    let (verb, _) = translate(m.name);
    let mut op = Map::new();
    op.insert("operationId".into(), Value::String(m.name.to_string()));
    op.insert(
        "tags".into(),
        Value::Array(vec![Value::String(group.to_string())]),
    );
    op.insert("summary".into(), Value::String(m.desc.to_string()));
    op.insert(
        "x-nasty-role".into(),
        Value::String(m.role.as_str().to_string()),
    );
    // Cheap, human-readable role tag in the description so Swagger UI renders it
    // visibly under the summary even when the viewer ignores `x-*` extensions.
    op.insert(
        "description".into(),
        Value::String(format!(
            "{}\n\n**Required role:** `{}`",
            m.desc,
            m.role.as_str()
        )),
    );

    // Params → query (GET) or requestBody (everything else).
    match &m.params {
        MethodParams::None => {}
        MethodParams::Schema(v) | MethodParams::AdHoc(v) => {
            let schema = process_schema(v, schemas);
            if verb == HttpVerb::Get {
                op.insert("parameters".into(), schema_to_query_params(&schema));
            } else {
                op.insert(
                    "requestBody".into(),
                    serde_json::json!({
                        "required": true,
                        "content": {
                            "application/json": {"schema": schema}
                        }
                    }),
                );
            }
        }
    }

    // Response.
    let response = match &m.result {
        None => serde_json::json!({"description": "Success — no content."}),
        Some(v) => {
            let schema = process_schema(v, schemas);
            serde_json::json!({
                "description": "OK",
                "content": {
                    "application/json": {"schema": schema}
                }
            })
        }
    };
    let status_code = if m.result.is_none() { "204" } else { "200" };
    op.insert(
        "responses".into(),
        serde_json::json!({
            status_code: response,
            "401": {"description": "Authentication required."},
            "403": {"description": "Caller's role lacks permission for this method."},
            "500": {"description": "Engine-side error (see `error.message` for detail).", "content": {"application/json": {"schema": json_rpc_error_schema()}}}
        }),
    );

    // Role-aware security: methods marked `any` are still authenticated (no
    // unauthenticated callers), but we keep them at the default security.
    // Stricter security objects could narrow per-role in future.
    let _ = MethodRole::Any; // marker only

    Value::Object(op)
}

/// Transform a schemars-emitted JSON Schema for use inside an OpenAPI document:
///   - lift any inline `$defs` into the shared `schemas` map
///   - rewrite local `$ref` paths from `#/$defs/X` to `#/components/schemas/X`
fn process_schema(schema: &Value, schemas: &mut BTreeMap<String, Value>) -> Value {
    let mut s = schema.clone();
    if let Some(obj) = s.as_object_mut()
        && let Some(defs) = obj.remove("$defs")
        && let Value::Object(defs_map) = defs
    {
        for (name, mut def_schema) in defs_map {
            rewrite_refs(&mut def_schema);
            schemas.entry(name).or_insert(def_schema);
        }
    }
    rewrite_refs(&mut s);
    s
}

/// Recursively rewrite `#/$defs/X` references to `#/components/schemas/X`.
fn rewrite_refs(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if let Some(r) = map.get_mut("$ref")
                && let Some(s) = r.as_str()
                && let Some(rest) = s.strip_prefix("#/$defs/")
            {
                *r = Value::String(format!("#/components/schemas/{rest}"));
            }
            for (_, child) in map.iter_mut() {
                rewrite_refs(child);
            }
        }
        Value::Array(arr) => {
            for child in arr.iter_mut() {
                rewrite_refs(child);
            }
        }
        _ => {}
    }
}

/// Convert an object schema into a list of OpenAPI `parameters` entries
/// suitable for a GET request's query string.
///
/// Schemars-emitted object schemas use `properties` + `required`; each
/// property becomes one `in: query` parameter. Schemas that aren't object-
/// shaped (or don't have properties) produce an empty parameter list, which
/// Swagger UI renders as "no parameters" — acceptable for the edge case.
fn schema_to_query_params(schema: &Value) -> Value {
    let Some(props) = schema.get("properties").and_then(|v| v.as_object()) else {
        return Value::Array(vec![]);
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let params: Vec<Value> = props
        .iter()
        .map(|(name, prop)| {
            let desc = prop
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            serde_json::json!({
                "name": name,
                "in": "query",
                "required": required.contains(&name.as_str()),
                "description": desc,
                "schema": prop,
            })
        })
        .collect();
    Value::Array(params)
}

fn json_rpc_error_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "error": {
                "type": "object",
                "properties": {
                    "code": {"type": "integer"},
                    "message": {"type": "string"}
                },
                "required": ["code", "message"]
            }
        },
        "required": ["error"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rewrites_refs_to_components_schemas() {
        let mut v = json!({"$ref": "#/$defs/Foo"});
        rewrite_refs(&mut v);
        assert_eq!(v, json!({"$ref": "#/components/schemas/Foo"}));
    }

    #[test]
    fn rewrites_refs_in_nested_arrays_and_maps() {
        let mut v = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {"$ref": "#/$defs/Item"}
                },
                "single": {"$ref": "#/$defs/Other"}
            }
        });
        rewrite_refs(&mut v);
        assert_eq!(
            v.pointer("/properties/items/items/$ref").unwrap(),
            "#/components/schemas/Item"
        );
        assert_eq!(
            v.pointer("/properties/single/$ref").unwrap(),
            "#/components/schemas/Other"
        );
    }

    #[test]
    fn process_schema_lifts_defs() {
        let mut shared = BTreeMap::new();
        let s = json!({
            "$ref": "#/$defs/Outer",
            "$defs": {
                "Outer": {"type": "object", "properties": {"inner": {"$ref": "#/$defs/Inner"}}},
                "Inner": {"type": "string"}
            }
        });
        let cleaned = process_schema(&s, &mut shared);
        assert!(shared.contains_key("Outer"));
        assert!(shared.contains_key("Inner"));
        assert_eq!(cleaned.get("$ref").unwrap(), "#/components/schemas/Outer");
        // Outer's inner ref must also have been rewritten when it landed in shared.
        let outer_inner_ref = shared
            .get("Outer")
            .unwrap()
            .pointer("/properties/inner/$ref")
            .unwrap();
        assert_eq!(outer_inner_ref, "#/components/schemas/Inner");
        // `$defs` is gone from the returned schema.
        assert!(cleaned.get("$defs").is_none());
    }

    #[test]
    fn schema_to_query_params_marks_required_fields() {
        let s = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Name."},
                "limit": {"type": "integer"}
            },
            "required": ["name"]
        });
        let params = schema_to_query_params(&s);
        let arr = params.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let by_name: BTreeMap<&str, &Value> = arr
            .iter()
            .map(|p| (p.get("name").unwrap().as_str().unwrap(), p))
            .collect();
        assert_eq!(by_name["name"].get("required").unwrap(), true);
        assert_eq!(by_name["limit"].get("required").unwrap(), false);
        assert_eq!(by_name["name"].get("in").unwrap(), "query");
    }

    #[test]
    fn full_registry_renders_valid_openapi() {
        // Smoke test — produce the full doc against the live registry and
        // assert top-level invariants hold. Catches any future method whose
        // schemas crash the lift step.
        let (_g, groups) = super::super::build_full_registry();
        let doc = render_openapi("test", &groups);
        assert_eq!(doc.get("openapi").unwrap(), "3.1.0");
        assert!(doc.get("paths").unwrap().as_object().unwrap().len() > 250);
        // Every registered method maps to an existing path; sample one.
        assert!(doc.pointer("/paths/~1api~1v1~1system~1info/get").is_some());
    }
}
