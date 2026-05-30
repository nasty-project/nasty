//! Markdown emitter unit tests. Pin the small schema-shape converters so a
//! refactor that breaks how oneOf / $ref / nullables render fails fast.

use super::markdown::{render_properties, render_result_summary, type_str_from_schema};
use serde_json::json;

#[test]
fn type_str_simple_type() {
    assert_eq!(type_str_from_schema(&json!({"type": "string"})), "string");
    assert_eq!(type_str_from_schema(&json!({"type": "integer"})), "integer");
    assert_eq!(type_str_from_schema(&json!({"type": "boolean"})), "boolean");
}

#[test]
fn type_str_ref_uses_last_path_segment() {
    assert_eq!(
        type_str_from_schema(&json!({"$ref": "#/$defs/Filesystem"})),
        "`Filesystem`"
    );
}

#[test]
fn type_str_array_with_simple_items() {
    assert_eq!(
        type_str_from_schema(&json!({"type": "array", "items": {"type": "string"}})),
        "string[]"
    );
}

#[test]
fn type_str_array_with_ref_items() {
    // `items` recursion has to handle $ref too — otherwise lists of named
    // structs render as the literal string "object[]".
    assert_eq!(
        type_str_from_schema(&json!({
            "type": "array",
            "items": {"$ref": "#/$defs/Subvolume"}
        })),
        "`Subvolume`[]"
    );
}

#[test]
fn type_str_nullable_strips_null() {
    // `Option<T>` flows in as type = ["T", "null"]. Showing "null" in the
    // docs is noise — the Required column already conveys optionality.
    assert_eq!(
        type_str_from_schema(&json!({"type": ["string", "null"]})),
        "string"
    );
}

#[test]
fn type_str_oneof_pipes_variants() {
    // `\|` is the markdown table-cell escape for a pipe.
    assert_eq!(
        type_str_from_schema(&json!({
            "oneOf": [{"type": "string"}, {"type": "integer"}]
        })),
        "string \\| integer"
    );
}

#[test]
fn type_str_anyof_pipes_variants() {
    assert_eq!(
        type_str_from_schema(&json!({
            "anyOf": [{"type": "string"}, {"type": "integer"}]
        })),
        "string \\| integer"
    );
}

#[test]
fn type_str_enum_quotes_each_variant() {
    assert_eq!(
        type_str_from_schema(&json!({"enum": ["admin", "operator", "readonly"]})),
        "`admin` \\| `operator` \\| `readonly`"
    );
}

#[test]
fn type_str_fallback_is_object() {
    assert_eq!(type_str_from_schema(&json!({})), "object");
}

#[test]
fn result_summary_array_renders_with_items() {
    assert_eq!(
        render_result_summary(&json!({
            "type": "array",
            "items": {"$ref": "#/$defs/Snapshot"}
        })),
        "``Snapshot`[]`"
    );
}

#[test]
fn result_summary_array_without_items_renders_as_array() {
    assert_eq!(render_result_summary(&json!({"type": "array"})), "`array`");
}

#[test]
fn result_summary_ref_uses_short_name() {
    assert_eq!(
        render_result_summary(&json!({"$ref": "#/$defs/SettingsUpdate"})),
        "`SettingsUpdate`"
    );
}

#[test]
fn result_summary_fallback_is_object() {
    assert_eq!(render_result_summary(&json!({})), "`object`");
}

#[test]
fn render_properties_resolves_ref_via_defs() {
    let defs = json!({
        "Foo": {
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Item name"}
            },
            "required": ["name"],
        }
    });
    let rendered = render_properties(&json!({"$ref": "#/$defs/Foo"}), &defs).unwrap();
    assert!(rendered.contains("`name`"), "rendered:\n{rendered}");
    assert!(rendered.contains("string"), "rendered:\n{rendered}");
    assert!(rendered.contains("yes"), "rendered:\n{rendered}");
    assert!(rendered.contains("Item name"), "rendered:\n{rendered}");
}

#[test]
fn render_properties_marks_optional_fields_no() {
    let rendered = render_properties(
        &json!({
            "type": "object",
            "properties": {
                "maybe": {"type": "string"}
            }
        }),
        &json!({}),
    )
    .unwrap();
    assert!(rendered.contains("`maybe`"), "rendered:\n{rendered}");
    assert!(rendered.contains("| no |"), "rendered:\n{rendered}");
}

#[test]
fn render_properties_returns_none_for_empty_object() {
    assert!(render_properties(&json!({"type": "object"}), &json!({})).is_none());
    assert!(render_properties(&json!({"type": "object", "properties": {}}), &json!({})).is_none());
}

#[test]
fn translation_produces_no_collisions_across_registry() {
    // Every registered method must translate to a unique (verb, path) pair —
    // otherwise the REST gateway can't route requests unambiguously.
    use super::paths::translate;
    use std::collections::HashMap;
    let (_g, groups) = super::build_full_registry();
    let mut seen: HashMap<(super::paths::HttpVerb, String), &'static str> = HashMap::new();
    for (_, methods) in &groups {
        for m in methods {
            let key = translate(m.name);
            if let Some(prior) = seen.insert(key.clone(), m.name) {
                panic!(
                    "translation collision: `{prior}` and `{}` both map to {} {}",
                    m.name,
                    key.0.as_str().to_uppercase(),
                    key.1
                );
            }
        }
    }
}

#[test]
fn registry_builds_without_panic() {
    // Smoke test: build the full registry. Catches schema-derivation
    // panics from any registered type — schemars sometimes throws on
    // recursive types or missing JsonSchema impls in dependencies.
    let (_g, groups) = super::build_full_registry();
    assert!(!groups.is_empty(), "registry returned no groups");
    let total: usize = groups.iter().map(|(_, ms)| ms.len()).sum();
    assert!(total > 100, "expected >100 methods, got {total}");
}
