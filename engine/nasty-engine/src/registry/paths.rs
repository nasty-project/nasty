//! JSON-RPC method name → REST path + HTTP verb translation.
//!
//! Used by the OpenAPI emitter and the REST gateway in `rest_gateway.rs` so
//! the two never disagree about what URL a method lives at.
//!
//! Translation is purely mechanical:
//!   - dots → slashes, segments otherwise unchanged
//!   - verb inferred from the underscore-separated words of the last segment:
//!       * **first word** matches an action-verb table (`delete`, `set`, …)
//!       * **last word** matches a noun-read table (`info`, `notice`, …)
//!       * action verbs take priority; default is POST
//!
//! The split into first-word vs last-word matching is what handles compound
//! names: `delete_user` matches `delete` as the first word (→ DELETE), and
//! `tagged_release_notice` matches `notice` as the last word (→ GET).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpVerb {
    Get,
    Post,
    Put,
    Delete,
}

impl HttpVerb {
    pub fn as_str(self) -> &'static str {
        match self {
            HttpVerb::Get => "get",
            HttpVerb::Post => "post",
            HttpVerb::Put => "put",
            HttpVerb::Delete => "delete",
        }
    }
}

/// First-word table: action verbs at the start of the last segment.
const ACTION_VERBS: &[(&str, HttpVerb)] = &[
    ("get", HttpVerb::Get),
    ("list", HttpVerb::Get),
    ("status", HttpVerb::Get),
    ("check", HttpVerb::Get),
    ("find", HttpVerb::Get),
    ("delete", HttpVerb::Delete),
    ("destroy", HttpVerb::Delete),
    ("remove", HttpVerb::Delete),
    ("set", HttpVerb::Put),
    ("update", HttpVerb::Put),
];

/// Last-word table: noun reads at the end of the last segment.
///
/// These cover methods that don't have an action-verb prefix but are still
/// reads (`system.info`, `system.alerts`, `system.version.tagged_release_notice`).
/// Without them, those methods would default to POST and Swagger UI would
/// render a misleading empty-body editor.
const READ_NOUNS: &[&str] = &[
    "info",
    "health",
    "stats",
    "version",
    "alerts",
    "disks",
    "usage",
    "timezones",
    "notice",
    "me",
    "config",
    "available",
    "routes",
    "logs",
    "summary",
    "iommu",
    "level",
    "history",
    "prometheus",
    "pending",
    "devices",
    "capabilities",
    "constraints",
    "readiness",
    "snapshots",
    "dependents",
    "children",
    "top",
    "timestats",
    "required",
];

/// Translate a JSON-RPC method name to (verb, REST path).
pub fn translate(method: &str) -> (HttpVerb, String) {
    let path = format!("/api/v1/{}", method.replace('.', "/"));
    let verb = infer_verb(method);
    (verb, path)
}

/// Reverse direction: REST path segments → JSON-RPC method name.
/// Used by the gateway to recover the method name from the captured URL tail.
pub fn method_from_segments(segments: &str) -> String {
    segments.replace('/', ".")
}

fn infer_verb(method: &str) -> HttpVerb {
    let last_segment = method.rsplit('.').next().unwrap_or(method);
    let words: Vec<&str> = last_segment.split('_').collect();
    let first = words[0];
    let last_word = *words.last().unwrap();

    if let Some((_, verb)) = ACTION_VERBS.iter().find(|(kw, _)| first == *kw) {
        return *verb;
    }
    if READ_NOUNS.contains(&last_word) {
        return HttpVerb::Get;
    }
    HttpVerb::Post
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dots_become_slashes() {
        let (_, path) = translate("system.update.build_dir.set");
        assert_eq!(path, "/api/v1/system/update/build_dir/set");
    }

    #[test]
    fn method_from_segments_inverts_translate() {
        let (_, path) = translate("share.iscsi.remove_acl");
        let tail = path.strip_prefix("/api/v1/").unwrap();
        assert_eq!(method_from_segments(tail), "share.iscsi.remove_acl");
    }

    #[test]
    fn first_word_action_verbs_dominate() {
        assert_eq!(translate("fs.get").0, HttpVerb::Get);
        assert_eq!(translate("subvolume.list").0, HttpVerb::Get);
        assert_eq!(translate("subvolume.list_all").0, HttpVerb::Get);
        assert_eq!(translate("fs.scrub.status").0, HttpVerb::Get);
        assert_eq!(translate("subvolume.find_by_property").0, HttpVerb::Get);
        assert_eq!(translate("auth.delete_user").0, HttpVerb::Delete);
        assert_eq!(translate("share.iscsi.remove_acl").0, HttpVerb::Delete);
        assert_eq!(translate("fs.options.update").0, HttpVerb::Put);
        assert_eq!(translate("fs.device.set_label").0, HttpVerb::Put);
    }

    #[test]
    fn last_word_noun_reads_map_to_get() {
        assert_eq!(translate("auth.me").0, HttpVerb::Get);
        assert_eq!(translate("system.info").0, HttpVerb::Get);
        assert_eq!(translate("system.health").0, HttpVerb::Get);
        assert_eq!(translate("system.alerts").0, HttpVerb::Get);
        assert_eq!(translate("bcachefs.usage").0, HttpVerb::Get);
        assert_eq!(translate("system.update.version").0, HttpVerb::Get);
        assert_eq!(
            translate("system.version.tagged_release_notice").0,
            HttpVerb::Get
        );
        // Methods covered by the expanded noun list:
        assert_eq!(translate("apps.config").0, HttpVerb::Get);
        assert_eq!(translate("apps.caddy.routes").0, HttpVerb::Get);
        assert_eq!(translate("apps.logs").0, HttpVerb::Get);
        assert_eq!(translate("system.hardware.summary").0, HttpVerb::Get);
        assert_eq!(translate("system.hardware.iommu").0, HttpVerb::Get);
        assert_eq!(translate("system.log.level").0, HttpVerb::Get);
        assert_eq!(translate("system.metrics.history").0, HttpVerb::Get);
        assert_eq!(translate("system.metrics.prometheus").0, HttpVerb::Get);
        assert_eq!(translate("backup.snapshots").0, HttpVerb::Get);
        assert_eq!(translate("vm.capabilities").0, HttpVerb::Get);
        assert_eq!(translate("firmware.constraints").0, HttpVerb::Get);
        assert_eq!(translate("firmware.devices").0, HttpVerb::Get);
        assert_eq!(translate("firmware.available").0, HttpVerb::Get);
        assert_eq!(translate("system.secure_boot.readiness").0, HttpVerb::Get);
        assert_eq!(translate("subvolume.children").0, HttpVerb::Get);
        assert_eq!(translate("fs.dependents").0, HttpVerb::Get);
        assert_eq!(translate("fs.locked_dependents").0, HttpVerb::Get);
        assert_eq!(translate("bcachefs.top").0, HttpVerb::Get);
        assert_eq!(translate("bcachefs.timestats").0, HttpVerb::Get);
        assert_eq!(translate("system.reboot_required").0, HttpVerb::Get);
    }

    #[test]
    fn side_effect_methods_without_verb_or_noun_stay_post() {
        assert_eq!(translate("system.reboot").0, HttpVerb::Post);
        assert_eq!(translate("system.shutdown").0, HttpVerb::Post);
        assert_eq!(translate("auth.logout").0, HttpVerb::Post);
        assert_eq!(translate("system.update.apply").0, HttpVerb::Post);
        assert_eq!(translate("system.update.rollback").0, HttpVerb::Post);
        assert_eq!(
            translate("system.version.upgrade_tagged_release").0,
            HttpVerb::Post
        );
        // Service / app lifecycle actions
        assert_eq!(translate("apps.enable").0, HttpVerb::Post);
        assert_eq!(translate("apps.disable").0, HttpVerb::Post);
        assert_eq!(translate("apps.start").0, HttpVerb::Post);
        assert_eq!(translate("apps.stop").0, HttpVerb::Post);
        assert_eq!(translate("apps.restart").0, HttpVerb::Post);
        assert_eq!(translate("apps.install").0, HttpVerb::Post);
        assert_eq!(translate("apps.pull").0, HttpVerb::Post);
        assert_eq!(translate("apps.prune").0, HttpVerb::Post);
        // VM lifecycle
        assert_eq!(translate("vm.start").0, HttpVerb::Post);
        assert_eq!(translate("vm.stop").0, HttpVerb::Post);
        assert_eq!(translate("vm.kill").0, HttpVerb::Post);
        assert_eq!(translate("vm.snapshot").0, HttpVerb::Post);
        // Tailscale / SSH side-effects
        assert_eq!(translate("system.tailscale.connect").0, HttpVerb::Post);
        assert_eq!(translate("system.tailscale.disconnect").0, HttpVerb::Post);
        // Backup operations
        assert_eq!(translate("backup.run").0, HttpVerb::Post);
        assert_eq!(translate("backup.restore").0, HttpVerb::Post);
    }
}
