//! Caddy admin-API client for app-ingress routes.
//!
//! NASty's reverse-proxy backend is Caddy.  Static routes (WebUI,
//! `/api/*`, `/ws`, `/health`, `/api/files/content`) live in the
//! NixOS-baked Caddyfile.  Per-app `/apps/<name>/` routes change at
//! runtime, so the engine pushes them through Caddy's admin API on
//! `localhost:2019` — PUT to install (inserts at index 0 so app
//! routes outrank the Caddyfile's catch-all subroute), DELETE by
//! `@id` to remove.
//! No file rewrite, no `systemctl reload`.
//!
//! Each app's route is tagged with `@id = nasty-app-<name>` so we
//! can manipulate it without traversing the JSON config tree.

use std::time::Duration;

use serde_json::{Value, json};
use tracing::{info, warn};

const ADMIN_URL: &str = "http://127.0.0.1:2019";

/// `@id` prefix for routes the engine owns.  Anything else in the
/// route list is left alone — that's the static Caddyfile content.
const ROUTE_ID_PREFIX: &str = "nasty-app-";

/// One ingress rule, ready to be turned into a Caddy route object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRoute {
    /// App name — used both as `/apps/<name>/` path and `@id` suffix.
    pub name: String,
    /// Host port the app's container listens on (Docker port mapping
    /// to 127.0.0.1).
    pub host_port: u16,
}

/// Caddy admin-API HTTP client.  Cheap to construct — internally
/// wraps a `reqwest::Client`, which manages its own connection pool.
pub struct CaddyApi {
    client: reqwest::Client,
    base_url: String,
}

impl Default for CaddyApi {
    fn default() -> Self {
        Self::new()
    }
}

impl CaddyApi {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            // Admin API is localhost — short timeouts catch a
            // wedged Caddy instead of letting us hang forever.
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client builder");
        Self {
            client,
            base_url: ADMIN_URL.to_string(),
        }
    }

    /// Block until the admin endpoint responds with a parseable
    /// config tree, or return Err after `max_secs`.  Called at
    /// engine startup before the ingress reconcile so a fresh boot
    /// where caddy.service hasn't fully started yet doesn't lose
    /// every ingress on first attempt.
    pub async fn wait_ready(&self, max_secs: u64) -> Result<(), String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(max_secs);
        let mut backoff = Duration::from_millis(250);
        loop {
            match self
                .client
                .get(format!("{}/config/", self.base_url))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => return Ok(()),
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "Caddy admin API at {} not ready within {}s",
                    self.base_url, max_secs
                ));
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
    }

    /// Locate the HTTP server that owns the TLS listener — we add
    /// app routes there so they share the same TLS + security
    /// headers + ordering as the static routes.  Caches across
    /// calls would be nice but in practice we only look up once
    /// per operation; admin API is cheap on localhost.
    async fn find_https_server_name(&self) -> Result<String, String> {
        let url = format!("{}/config/apps/http/servers/", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("GET {url}: status {}", resp.status()));
        }
        let servers: serde_json::Map<String, Value> =
            resp.json().await.map_err(|e| format!("parse {url}: {e}"))?;
        // Pick the server whose listen address ends in :<port> for
        // the WebUI port.  In practice with NASty's Caddyfile this
        // is the only TLS-bearing server, but we still filter so a
        // future :80 redirect server (or other) doesn't get picked.
        for (name, body) in &servers {
            let listens = body.get("listen").and_then(Value::as_array);
            let has_tls = body.get("tls_connection_policies").is_some();
            // The :80 redirect server has no TLS policies.  Anything
            // with TLS policies is the one we want.
            if has_tls && listens.is_some() {
                return Ok(name.clone());
            }
        }
        Err(format!(
            "no TLS-bearing server found among {:?}",
            servers.keys().collect::<Vec<_>>()
        ))
    }

    /// All routes in the TLS server, returned as raw JSON values so
    /// the caller can inspect `@id` tags and other metadata.
    async fn list_server_routes(&self, server: &str) -> Result<Vec<Value>, String> {
        let url = format!(
            "{}/config/apps/http/servers/{server}/routes/",
            self.base_url
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("GET {url}: status {}", resp.status()));
        }
        resp.json::<Vec<Value>>()
            .await
            .map_err(|e| format!("parse {url}: {e}"))
    }

    /// Return the engine-owned app routes currently in Caddy.  Used
    /// both by the `apps.ingress.list` RPC and by the startup
    /// reconcile that recovers state after a Caddy restart.
    pub async fn list_app_routes(&self) -> Result<Vec<AppRoute>, String> {
        let server = self.find_https_server_name().await?;
        let routes = self.list_server_routes(&server).await?;
        let mut out = Vec::new();
        for route in routes {
            let Some(id) = route.get("@id").and_then(Value::as_str) else {
                continue;
            };
            let Some(name) = id.strip_prefix(ROUTE_ID_PREFIX) else {
                continue;
            };
            // Walk the route's handlers to find the reverse_proxy
            // upstream port.  Shape mirrors what `set_app_route`
            // emits; if the JSON has drifted (operator edit?) we
            // just skip — caller does whole-set reconcile anyway.
            if let Some(port) = extract_upstream_port(&route) {
                out.push(AppRoute {
                    name: name.to_string(),
                    host_port: port,
                });
            }
        }
        Ok(out)
    }

    /// Install or replace an app route.  Idempotent: a second call
    /// with the same name updates the port; with a different name
    /// adds another entry.
    pub async fn set_app_route(&self, route: &AppRoute) -> Result<(), String> {
        // Remove any prior route with this name so we don't end up
        // with duplicates after a port change.
        let _ = self.remove_app_route(&route.name).await;

        let server = self.find_https_server_name().await?;
        let payload = build_route_json(route);

        // PUT to `routes/0` inserts the new route at index 0,
        // shifting existing routes down. App routes need to land
        // *before* the wrapping subroute that holds the Caddyfile's
        // catch-all `handle { file_server }` — otherwise that
        // subroute's host matcher swallows the request and our
        // `/apps/<name>/*` matcher is never even evaluated.
        //
        // POST has subtly different semantics here: POST to a
        // numeric array index appends to the array instead of
        // inserting before that index (verified empirically against
        // Caddy 2.11.2 and caught by the appliance-smoke test —
        // POST sent the route to the end of the list, behind the
        // terminal catch-all subroute, so /apps/<name>/ silently
        // 404'd).
        let url = format!(
            "{}/config/apps/http/servers/{server}/routes/0",
            self.base_url
        );
        let resp = self
            .client
            .put(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("PUT {url}: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("PUT {url}: status {status} body={body}"));
        }
        info!(
            "caddy: set app route {} → 127.0.0.1:{}",
            route.name, route.host_port
        );
        Ok(())
    }

    /// Remove the app route identified by name.  No-op (Ok) when
    /// the route doesn't exist, so callers can issue it
    /// unconditionally on app uninstall.
    pub async fn remove_app_route(&self, name: &str) -> Result<(), String> {
        let id = format!("{ROUTE_ID_PREFIX}{name}");
        let url = format!("{}/id/{id}", self.base_url);
        let resp = self
            .client
            .delete(&url)
            .send()
            .await
            .map_err(|e| format!("DELETE {url}: {e}"))?;
        match resp.status().as_u16() {
            200 | 204 => {
                info!("caddy: removed app route {name}");
                Ok(())
            }
            // Caddy returns 404 when the @id isn't found — treat
            // as success so a duplicate-remove is harmless.
            404 => Ok(()),
            s => {
                let body = resp.text().await.unwrap_or_default();
                Err(format!("DELETE {url}: status {s} body={body}"))
            }
        }
    }

    /// Replace the engine-owned route set with `desired`, leaving
    /// non-nasty routes (the static ones from the Caddyfile)
    /// alone.  Used by the startup reconcile to recover ingress
    /// state from a fresh Caddy that came up without our routes,
    /// e.g. after a host reboot or `systemctl restart caddy`.
    pub async fn replace_app_routes(&self, desired: &[AppRoute]) -> Result<(), String> {
        let existing = self.list_app_routes().await.unwrap_or_default();
        let desired_names: std::collections::HashSet<&str> =
            desired.iter().map(|r| r.name.as_str()).collect();
        for old in &existing {
            if desired_names.contains(old.name.as_str()) {
                continue;
            }
            if let Err(e) = self.remove_app_route(&old.name).await {
                warn!("caddy reconcile: remove {} failed: {e}", old.name);
            }
        }
        for new in desired {
            if let Err(e) = self.set_app_route(new).await {
                warn!("caddy reconcile: set {} failed: {e}", new.name);
            }
        }
        Ok(())
    }
}

/// Build the Caddy JSON route object for one app ingress.  Mirror
/// of `handle_path /apps/<name>/* { reverse_proxy 127.0.0.1:<port>
/// { header_up X-Real-IP {http.request.remote.host} } }` as produced
/// by `caddy adapt`, with `@id` so we can delete it by ID later.
fn build_route_json(route: &AppRoute) -> Value {
    json!({
        "@id": format!("{ROUTE_ID_PREFIX}{}", route.name),
        "match": [{"path": [format!("/apps/{}/*", route.name)]}],
        "handle": [{
            "handler": "subroute",
            "routes": [{
                "handle": [
                    {
                        "handler": "rewrite",
                        "strip_path_prefix": format!("/apps/{}", route.name),
                    },
                    {
                        "handler": "reverse_proxy",
                        "upstreams": [{"dial": format!("127.0.0.1:{}", route.host_port)}],
                        "headers": {
                            "request": {
                                "set": {
                                    "X-Real-Ip": ["{http.request.remote.host}"]
                                }
                            }
                        },
                        // Mirrors the Caddyfile's `stream_close_delay 30m`
                        // on the static WS handlers — every other app
                        // install/remove triggers a Caddy config reload
                        // (this very admin API call), and without the
                        // delay any WS the app itself exposes would die
                        // on every neighbouring app's lifecycle event.
                        "stream_close_delay": "30m",
                    }
                ]
            }]
        }],
        "terminal": true
    })
}

/// Walk a route JSON value to find the reverse_proxy upstream port.
/// Returns None if the structure isn't shaped like one of ours.
fn extract_upstream_port(route: &Value) -> Option<u16> {
    let outer_handles = route.get("handle")?.as_array()?;
    for h in outer_handles {
        let inner_routes = h.get("routes")?.as_array()?;
        for r in inner_routes {
            let inner_handles = r.get("handle")?.as_array()?;
            for ih in inner_handles {
                if ih.get("handler").and_then(Value::as_str) != Some("reverse_proxy") {
                    continue;
                }
                let upstreams = ih.get("upstreams")?.as_array()?;
                let dial = upstreams.first()?.get("dial")?.as_str()?;
                if let Some((_, port)) = dial.rsplit_once(':')
                    && let Ok(p) = port.parse()
                {
                    return Some(p);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_route_json_has_expected_shape() {
        let r = AppRoute {
            name: "foo".into(),
            host_port: 18080,
        };
        let v = build_route_json(&r);
        assert_eq!(v["@id"], "nasty-app-foo");
        assert_eq!(v["match"][0]["path"][0], "/apps/foo/*");
        assert_eq!(v["terminal"], true);
        // Drill down to the strip_path_prefix + reverse_proxy
        // upstream — the round-trip extractor below depends on
        // the exact nesting, so explicitly assert it.
        let inner_handle = &v["handle"][0]["routes"][0]["handle"];
        assert_eq!(inner_handle[0]["strip_path_prefix"], "/apps/foo");
        assert_eq!(inner_handle[1]["upstreams"][0]["dial"], "127.0.0.1:18080");
    }

    #[test]
    fn extract_upstream_port_roundtrips_through_build() {
        let r = AppRoute {
            name: "bar".into(),
            host_port: 9999,
        };
        let v = build_route_json(&r);
        assert_eq!(extract_upstream_port(&v), Some(9999));
    }

    #[test]
    fn extract_upstream_port_returns_none_for_foreign_route() {
        // A route shaped unlike ours — extractor must say "not
        // mine" rather than panic, since the routes list also
        // contains static Caddyfile entries we don't own.
        let v = json!({
            "match": [{"path": ["/health"]}],
            "handle": [{"handler": "reverse_proxy", "upstreams": [{"dial": "127.0.0.1:2137"}]}]
        });
        assert_eq!(extract_upstream_port(&v), None);
    }
}
