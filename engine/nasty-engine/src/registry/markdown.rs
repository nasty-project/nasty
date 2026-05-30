//! Render the method registry as the `docs/api.md` markdown file.
//!
//! Output shape matches what `nasty-apidoc` produced verbatim, so the
//! drift-check CI step keeps passing across the move.

use super::{Method, MethodParams};
use serde_json::Value;
use std::collections::BTreeMap;

pub fn render_markdown(groups: &[(&str, Vec<Method>)]) -> String {
    let defs_json = {
        let mut merged = serde_json::Map::new();
        let all = collect_defs(groups);
        for (k, v) in all {
            merged.insert(k, v);
        }
        Value::Object(merged)
    };

    let mut out = String::new();

    out.push_str("# NASty JSON-RPC API\n\n");
    out.push_str("NASty exposes a **JSON-RPC 2.0** API over **WebSocket** at `/ws`.\n\n");
    out.push_str("## Transport\n\n");
    out.push_str("Connect to `ws://<host>/ws` with a valid session cookie or `Authorization: Bearer <token>` header.\n\n");
    out.push_str("**Request:**\n```json\n{\"jsonrpc\": \"2.0\", \"id\": 1, \"method\": \"pool.list\", \"params\": {}}\n```\n\n");
    out.push_str(
        "**Response:**\n```json\n{\"jsonrpc\": \"2.0\", \"id\": 1, \"result\": [...]}\n```\n\n",
    );
    out.push_str("**Error:**\n```json\n{\"jsonrpc\": \"2.0\", \"id\": 1, \"error\": {\"code\": -32603, \"message\": \"filesystem not found: mypool\"}}\n```\n\n");
    out.push_str("## Authentication\n\n");
    out.push_str("Send `POST /api/login` with `{\"username\": \"...\", \"password\": \"...\"}` to receive a session token. ");
    out.push_str("Pass it as a cookie (`session=<token>`) or `Authorization: Bearer <token>` header on the WebSocket upgrade.\n\n");
    out.push_str("## Roles\n\n");
    out.push_str("| Role | Description |\n|------|-------------|\n");
    out.push_str("| `admin` | Full access to all methods |\n");
    out.push_str("| `operator` | Create/delete subvolumes and snapshots; read pools. Cannot manage users, destroy pools, or change system settings. |\n");
    out.push_str("| `readonly` | Read-only access to all list/get methods |\n\n");
    out.push_str(
        "API tokens can additionally be scoped to a single **filesystem** (restricts visibility) ",
    );
    out.push_str("and for operator tokens to a single **owner** (restricts to subvolumes owned by that token).\n\n");
    out.push_str("## Real-time Events\n\n");
    out.push_str(
        "After any successful mutation the server broadcasts an event on the same WebSocket:\n",
    );
    out.push_str("```json\n{\"event\": \"pool\"}\n```\n");
    out.push_str("Clients should re-fetch the relevant resource when they receive an event. ");
    out.push_str("Event types: `filesystem`, `subvolume`, `snapshot`, `share.nfs`, `share.smb`, `share.iscsi`, `share.nvmeof`, `protocol`, `settings`, `alert`.\n\n");
    out.push_str("---\n\n");

    out.push_str("## Contents\n\n");
    for (group, _) in groups {
        let anchor = group
            .to_lowercase()
            .replace([' ', '/'], "-")
            .replace(['&', '(', ')', '.'], "");
        out.push_str(&format!("- [{group}](#{anchor})\n"));
    }
    out.push('\n');

    for (group, methods) in groups {
        out.push_str(&format!("## {group}\n\n"));
        for m in methods {
            out.push_str(&format!("### `{}`\n\n", m.name));
            out.push_str(&format!("{}\n\n", m.desc));
            out.push_str(&format!("**Role:** `{}`\n\n", m.role.as_str()));

            match &m.params {
                MethodParams::None => {}
                MethodParams::Schema(v) | MethodParams::AdHoc(v) => {
                    out.push_str("**Params:**\n\n");
                    if let Some(table) = render_properties(v, &defs_json) {
                        out.push_str(&table);
                        out.push('\n');
                    } else {
                        out.push_str(&format!("{}\n\n", render_result_summary(v)));
                    }
                }
            }

            if let Some(result) = &m.result {
                out.push_str("**Returns:**\n\n");
                if let Some(table) = render_properties(result, &defs_json) {
                    out.push_str(&table);
                    out.push('\n');
                } else {
                    out.push_str(&format!("{}\n\n", render_result_summary(result)));
                }
            }

            out.push('\n');
        }
    }

    out.push_str("---\n\n## Object Definitions\n\n");
    let mut def_names: Vec<&String> = defs_json
        .as_object()
        .map(|m| m.keys().collect())
        .unwrap_or_default();
    def_names.sort();

    for name in def_names {
        let schema = &defs_json[name];
        out.push_str(&format!("### `{name}`\n\n"));

        if let Some(vals) = schema.get("enum").and_then(|v| v.as_array()) {
            let parts: Vec<String> = vals
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| format!("`{s}`"))
                .collect();
            out.push_str(&format!("Enum: {}\n\n", parts.join(", ")));
            continue;
        }
        if let Some(variants) = schema.get("oneOf").and_then(|v| v.as_array()) {
            let parts: Vec<String> = variants
                .iter()
                .filter_map(|v| {
                    v.get("enum")?
                        .as_array()?
                        .first()?
                        .as_str()
                        .map(|s| format!("`{s}`"))
                })
                .collect();
            if !parts.is_empty() {
                out.push_str(&format!("Enum: {}\n\n", parts.join(", ")));
                continue;
            }
        }

        if let Some(table) = render_properties(schema, &defs_json) {
            out.push_str(&table);
            out.push('\n');
        } else {
            out.push_str("*(see schema)*\n\n");
        }
    }

    out
}

pub(super) fn type_str_from_schema(schema: &Value) -> String {
    if let Some(r) = schema.get("$ref").and_then(|v| v.as_str()) {
        return format!("`{}`", r.split('/').next_back().unwrap_or(r));
    }
    if let Some(items) = schema.get("items") {
        let inner = type_str_from_schema(items);
        return format!("{inner}[]");
    }
    if let Some(t) = schema.get("type").and_then(|v| v.as_str()) {
        return t.to_string();
    }
    if let Some(arr) = schema.get("type").and_then(|v| v.as_array()) {
        let parts: Vec<&str> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|&s| s != "null")
            .collect();
        return parts.join(" | ");
    }
    if let Some(variants) = schema.get("oneOf").or_else(|| schema.get("anyOf"))
        && let Some(arr) = variants.as_array()
    {
        let parts: Vec<String> = arr.iter().map(type_str_from_schema).collect();
        return parts.join(" \\| ");
    }
    if let Some(vals) = schema.get("enum").and_then(|v| v.as_array()) {
        let parts: Vec<String> = vals
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| format!("`{s}`"))
            .collect();
        return parts.join(" \\| ");
    }
    "object".to_string()
}

pub(super) fn render_properties(schema: &Value, defs: &Value) -> Option<String> {
    let resolved;
    let schema = if let Some(r) = schema.get("$ref").and_then(|v| v.as_str()) {
        let def_name = r.strip_prefix("#/$defs/").unwrap_or(r);
        resolved = defs.get(def_name).cloned().unwrap_or(Value::Null);
        &resolved
    } else {
        schema
    };

    let props = schema.get("properties")?.as_object()?;
    if props.is_empty() {
        return None;
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut rows = String::new();
    for (name, prop) in props {
        let is_req = required.contains(&name.as_str());
        let type_s = type_str_from_schema(prop);
        let desc = prop
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        rows.push_str(&format!(
            "| `{name}` | {type_s} | {} | {desc} |\n",
            if is_req { "yes" } else { "no" }
        ));
    }

    Some(format!(
        "| Field | Type | Required | Description |\n\
         |-------|------|:--------:|-------------|\n\
         {rows}"
    ))
}

pub(super) fn render_result_summary(schema: &Value) -> String {
    if schema.get("type").and_then(|v| v.as_str()) == Some("array") {
        if let Some(items) = schema.get("items") {
            let t = type_str_from_schema(items);
            return format!("`{t}[]`");
        }
        return "`array`".into();
    }
    if let Some(r) = schema.get("$ref").and_then(|v| v.as_str()) {
        return format!("`{}`", r.split('/').next_back().unwrap_or(r));
    }
    "`object`".into()
}

fn collect_defs(groups: &[(&str, Vec<Method>)]) -> BTreeMap<String, Value> {
    let mut all = BTreeMap::new();
    for (_, methods) in groups {
        for m in methods {
            // Walk every schema (params + result), collect $defs.
            let schemas: Vec<&Value> = std::iter::once(match &m.params {
                MethodParams::Schema(v) | MethodParams::AdHoc(v) => Some(v),
                MethodParams::None => None,
            })
            .flatten()
            .chain(m.result.as_ref())
            .collect();
            for v in schemas {
                if let Some(defs) = v.get("$defs").and_then(|d| d.as_object()) {
                    for (k, s) in defs {
                        all.entry(k.clone()).or_insert_with(|| s.clone());
                    }
                }
            }
        }
    }
    all
}

