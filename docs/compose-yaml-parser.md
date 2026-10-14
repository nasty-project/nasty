# Compose YAML parser maintenance

NASty uses `serde-saphyr` 2.x with only its deserialization feature enabled,
replacing `serde_yaml_ng` and its unmaintained `unsafe-libyaml` dependency
(RUSTSEC-2024-0449). No advisory exception is added for those crates.

Editor syntax checks, bind/UID inspection, image/pull-policy inventory and engine
deployment policy use the same `nasty_apps::parse_compose_yaml` helper. Docker
Compose remains the authoritative schema validator; deployment/runtime commands
are unchanged.

Compatibility/security details:

- YAML anchors and `<<` merge keys are expanded before inspection, including
  explicit service overrides. Privileges, namespaces and binds hidden in merged
  defaults are subject to the same policy as direct fields.
- Duplicate keys, recursive/undefined aliases and multiple documents are
  rejected rather than ambiguously interpreted. Default parser resource/alias
  budgets remain enabled.
- Compose `${VARIABLE}` strings are not expanded by the policy parser. Include,
  filesystem-resolver and property-interpolation features are disabled.
- Quoted strings, booleans, numbers, nulls and block/folded commands retain their
  ordinary YAML types/content. Non-string mapping shapes invalid for Compose may
  fail in the syntax phase rather than the later Docker schema phase.
- Errors retain 1-based line information but omit raw source snippets, which
  could otherwise echo adjacent environment values/secrets into diagnostics.

This change does not rewrite stored Compose files or restart deployed apps.
