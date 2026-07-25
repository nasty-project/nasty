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
//!   - The chosen subdomain matches the dedicated files portal hostname.
//!
//! Path-prefix conflicts are not modelled here: `/apps/<name>/*` paths
//! are derived from app names, names are DNS-safe and unique, and the
//! static Caddyfile routes (`/api/*`, `/ws*`, etc.) live in disjoint
//! prefixes. There's no realistic path collision an operator can
//! produce through the install form.

use std::sync::LazyLock;

use crate::AppState;

static HOSTNAME_RESERVATIONS: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Serialize the check-and-commit sections for app host routes and the files
/// portal hostname. Caddy otherwise accepts both and lets route order decide.
pub async fn lock_hostname_reservations() -> tokio::sync::MutexGuard<'static, ()> {
    HOSTNAME_RESERVATIONS.lock().await
}

fn reserved_hostname_conflict(
    settings: &nasty_system::settings::Settings,
    hostname: &str,
) -> Option<String> {
    if let Some(tls_domain) = settings.tls_domain.as_deref()
        && tls_domain.eq_ignore_ascii_case(hostname)
    {
        return Some(format!(
            "'{hostname}' is the NASty WebUI hostname — using it for an app would shadow the management interface"
        ));
    }
    if let Some(files_domain) = settings.files_domain.as_deref()
        && files_domain.eq_ignore_ascii_case(hostname)
    {
        return Some(format!(
            "'{hostname}' is the files portal hostname — using it for an app would shadow the user portal"
        ));
    }
    None
}

fn files_domain_app_conflict(
    files_domain: &str,
    ingresses: Result<&[nasty_apps::AppIngress], String>,
) -> Result<Option<String>, String> {
    let ingresses = ingresses?;
    Ok(ingresses.iter().find_map(|ingress| {
        ingress
            .subdomain
            .as_deref()
            .filter(|host| host.eq_ignore_ascii_case(files_domain))
            .map(|_| {
                format!(
                    "files portal domain '{files_domain}' is already used by app '{}'",
                    ingress.name
                )
            })
    }))
}

/// Reject a non-empty files portal hostname already claimed by an app route.
/// An unavailable Caddy route snapshot is an error: settings updates fail
/// closed rather than risking two host matchers for the same hostname.
pub async fn ensure_files_domain_available(
    state: &AppState,
    files_domain: &str,
) -> Result<(), String> {
    let files_domain = files_domain.trim();
    if files_domain.is_empty() {
        return Ok(());
    }
    nasty_system::settings::validate_files_domain(files_domain)?;
    let ingresses = state
        .apps
        .ingress_list()
        .await
        .map_err(|e| format!("cannot verify app hostname reservations: {e}"))?;
    if let Some(reason) = files_domain_app_conflict(files_domain, Ok(&ingresses))? {
        return Err(reason);
    }
    Ok(())
}

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

    // Reserved NASty hostname clash. Caddy serves the most recent matching
    // route, so an app could otherwise shadow either the management UI or
    // the files portal. The portal hostname only selects presentation and
    // routing; Role User authorization in the server remains the boundary.
    let settings = state.settings.get().await;
    if let Some(reason) = reserved_hostname_conflict(&settings, subdomain) {
        return Some(reason);
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

#[cfg(test)]
mod tests {
    use super::{files_domain_app_conflict, reserved_hostname_conflict};
    use nasty_apps::AppIngress;
    use nasty_system::settings::Settings;

    fn ingress(name: &str, subdomain: Option<&str>) -> AppIngress {
        AppIngress {
            name: name.into(),
            host_port: 8080,
            path: format!("/apps/{name}/"),
            subdomain: subdomain.map(str::to_string),
        }
    }

    #[test]
    fn app_hostname_rejects_configured_files_domain_case_insensitively() {
        let settings = Settings {
            files_domain: Some("Files.Example.com".into()),
            ..Settings::default()
        };
        let reason = reserved_hostname_conflict(&settings, "files.example.com").unwrap();
        assert!(reason.contains("files portal hostname"));
    }

    #[test]
    fn files_domain_rejects_existing_app_hostname_case_insensitively() {
        let ingresses = [
            ingress("path-only", None),
            ingress("jellyfin", Some("MEDIA.EXAMPLE.COM")),
        ];
        let reason = files_domain_app_conflict("media.example.com", Ok(&ingresses))
            .unwrap()
            .unwrap();
        assert!(reason.contains("jellyfin"));
    }

    #[test]
    fn files_domain_check_fails_closed_without_route_metadata() {
        let result =
            files_domain_app_conflict("files.example.com", Err("Caddy unavailable".into()));
        assert_eq!(result.unwrap_err(), "Caddy unavailable");
    }
}
