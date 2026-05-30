//! In-engine RPC method registry.
//!
//! Each RPC method declares its metadata (name, role, params schema, result
//! schema, description) in [`methods::registry`]. The dispatcher in
//! `router::*` is the only authority on *how* a method runs; this module is
//! the single authority on *what* a method is — used by:
//!   - role enforcement (`is_universally_allowed`, `is_operator_allowed`)
//!   - the markdown docs generator (`--dump-docs`)
//!   - the OpenAPI emitter and `/api/docs` site (PR2)
//!
//! Building the registry walks every JsonSchema-derived type in the workspace,
//! so it's not free. Callers that need only role data should use
//! [`role_lookup`] (lazy, cached) instead of [`build_full_registry`].

use schemars::{JsonSchema, SchemaGenerator};
use serde_json::Value;

mod markdown;
mod methods;

#[cfg(test)]
mod tests;

pub use markdown::render_markdown;

/// Role required to invoke a method.
///
/// Distinct from [`crate::auth::Role`] (the user's role): this is the
/// *minimum* role needed to call the method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodRole {
    /// Any authenticated user, including ReadOnly. Pure reads + self-mutations
    /// (logout, change own password, manage own webauthn credentials).
    Any,
    /// Operator or Admin. Subvolume/snapshot/share/vm/app management.
    Operator,
    /// Admin only. User management, system-level changes, destructive ops.
    Admin,
}

impl MethodRole {
    pub fn as_str(self) -> &'static str {
        match self {
            MethodRole::Any => "any",
            MethodRole::Operator => "operator",
            MethodRole::Admin => "admin",
        }
    }
}

/// Shape of a method's params.
pub enum MethodParams {
    /// No params accepted.
    None,
    /// Schema derived by schemars from a Rust type. May reference `$defs`.
    Schema(Value),
    /// Self-contained JSON Schema written inline. Used for the handful of
    /// one-off request shapes (`{"id": string}`, `{"name": string}`, …) that
    /// the engine parses ad-hoc via `require_str` rather than into a struct.
    AdHoc(Value),
}

pub struct Method {
    pub name: &'static str,
    pub desc: &'static str,
    pub role: MethodRole,
    pub params: MethodParams,
    /// JSON Schema for the result, or None if the method returns no value.
    pub result: Option<Value>,
}

/// Build the full registry, including JSON schemas. Expensive — walks every
/// JsonSchema type in the workspace. Cache the result if calling repeatedly.
pub fn build_full_registry() -> (SchemaGenerator, Vec<(&'static str, Vec<Method>)>) {
    let mut g = SchemaGenerator::default();
    let groups = methods::registry(&mut g);
    (g, groups)
}

// ── Helpers used by methods::registry ─────────────────────────────────

pub(crate) fn gen_schema<T: JsonSchema>(generator: &mut SchemaGenerator) -> Value {
    let schema = generator.root_schema_for::<T>();
    serde_json::to_value(&schema).unwrap()
}

/// Build a one-required-string-field JSON Schema for an AdHoc params shape.
pub(crate) fn ad_hoc_one(field: &'static str, desc: &'static str) -> Value {
    let mut props = serde_json::Map::new();
    props.insert(
        field.to_string(),
        serde_json::json!({"type": "string", "description": desc}),
    );
    serde_json::json!({
        "type": "object",
        "properties": props,
        "required": [field],
    })
}

/// Two-required-string-fields AdHoc schema.
pub(crate) fn ad_hoc_two(
    f1: &'static str,
    d1: &'static str,
    f2: &'static str,
    d2: &'static str,
) -> Value {
    let mut props = serde_json::Map::new();
    props.insert(
        f1.to_string(),
        serde_json::json!({"type": "string", "description": d1}),
    );
    props.insert(
        f2.to_string(),
        serde_json::json!({"type": "string", "description": d2}),
    );
    serde_json::json!({
        "type": "object",
        "properties": props,
        "required": [f1, f2],
    })
}
