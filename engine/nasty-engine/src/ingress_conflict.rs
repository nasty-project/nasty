//! Subdomain conflict detection for the apps.ingress.set RPC and the
//! Apps page's live preview.
//!
//! Caddy silently lets the most recent matching route win when two
//! routes claim the same host — install Jellyfin under
//! `lab.example.com`, then later set the same subdomain on Grafana, and
//! requests to `lab.example.com` start going to Grafana with no warning.
//! The operator might not notice until they hit the dead route by
//! accident. This module catches the common cases upfront:
//!
//!   - The chosen subdomain matches another engine-app's subdomain.
//!   - The chosen subdomain matches NASty's own WebUI hostname
//!     (would intercept the management interface).
//!
//! Path-prefix conflicts are not modelled here: `/apps/<name>/*` paths
//! are derived from app names, names are DNS-safe and unique, and the
//! static Caddyfile routes (`/api/*`, `/ws*`, etc.) live in disjoint
//! prefixes. There's no realistic path collision an operator can
//! produce through the install form.

use crate::AppState;

/// Returns a human-readable reason when `subdomain` would conflict with
/// an existing engine-app ingress or the NASty WebUI hostname. Returns
/// `None` when the choice is clear (or when `subdomain` is empty — that
/// means path-prefix mode, no conflict possible).
///
/// `name` is the app doing the set — we skip its own existing ingress
/// in the "already used by" check so re-saving the same subdomain on
/// the same app doesn't false-positive.
pub async fn find_subdomain_conflict(
    state: &AppState,
    name: &str,
    subdomain: &str,
) -> Option<String> {
    if subdomain.is_empty() {
        return None;
    }

    // WebUI hostname clash. The TLS settings always carry the FQDN the
    // WebUI is served at; an app subdomain matching it would shadow the
    // management interface (Caddy serves the most recent matching
    // route — in this case the app's, not the WebUI's).
    let settings = state.settings.get().await;
    if let Some(tls_domain) = settings.tls_domain.as_deref()
        && tls_domain.eq_ignore_ascii_case(subdomain)
    {
        return Some(format!(
            "'{subdomain}' is the NASty WebUI hostname — using it for an app would shadow the management interface"
        ));
    }

    // Another app's subdomain. We pull the current ingress list rather
    // than the manifest field because Caddy is the actual source of
    // truth at the moment of set — a manifest entry someone forgot to
    // push to Caddy doesn't actually claim the host yet.
    if let Ok(existing) = state.apps.ingress_list().await {
        for ing in existing {
            if ing.name == name {
                continue;
            }
            if let Some(other) = ing.subdomain.as_deref()
                && other.eq_ignore_ascii_case(subdomain)
            {
                return Some(format!(
                    "'{subdomain}' is already used by app '{}'",
                    ing.name
                ));
            }
        }
    }
    None
}
