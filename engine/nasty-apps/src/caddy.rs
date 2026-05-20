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
//!
//! TLS automation is driven through the same admin API: `set_tls_automation`
//! PUTs the full `apps.tls.automation` block, replacing whatever was there.
//! The caller (engine startup + settings/ingress mutations) builds the
//! desired policy set from settings + per-app subdomains and pushes it in
//! one shot. No file rewrite, no reload — Caddy issues the certs.

use std::time::Duration;

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};
use tracing::{info, warn};

const ADMIN_URL: &str = "http://127.0.0.1:2019";

/// `@id` prefix for routes the engine owns.  Anything else in the
/// route list is left alone — that's the static Caddyfile content.
const ROUTE_ID_PREFIX: &str = "nasty-app-";

/// One ingress rule, ready to be turned into a Caddy route object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRoute {
    /// App name — used as `@id` suffix and as path-prefix segment when
    /// `subdomain` is None.
    pub name: String,
    /// Host port the app's container listens on (Docker port mapping
    /// to 127.0.0.1).
    pub host_port: u16,
    /// Fully-qualified hostname to serve the app under (e.g.
    /// `jellyfin.example.com`). When set, the emitted Caddy route
    /// matches by host instead of by `/apps/<name>/` path prefix and
    /// drops the strip-prefix handler — the app sees itself rooted
    /// at `/`, which is what most upstream apps assume and what the
    /// post-install probe in #219 had to disable ingress for when
    /// the path-prefix mode broke their absolute-path assets.
    ///
    /// Caddy's automatic HTTPS picks up the new hostname from the
    /// match block; the operator's existing ACME config (wildcard
    /// via DNS-01, or per-name via HTTP-01/TLS-ALPN-01) handles cert
    /// acquisition. V1 doesn't add per-app ACME knobs.
    pub subdomain: Option<String>,
}

/// One hostname that Caddy should auto-provision a certificate for.
/// The caller assembles a `Vec<TlsPolicy>` from settings (main domain) +
/// per-app subdomains and passes it to [`CaddyApi::set_tls_automation`].
///
/// All policies share the same issuer config — they differ only in
/// `subjects` — so we don't carry per-host email/provider knobs. If a
/// future use case wants that (e.g. one app on Cloudflare DNS, another
/// on Route 53), grow the struct then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsPolicy {
    /// Fully-qualified hostname Caddy should issue a cert for.
    pub host: String,
}

/// Issuer configuration shared by every policy emitted in one
/// `set_tls_automation` call. Mirrors the user's settings (email + DNS
/// provider + staging flag) and gets folded into the ACME issuer block
/// of each policy.
///
/// When `dns_provider` is `Some`, the policy uses DNS-01 with the named
/// caddy-dns plugin. When `None`, Caddy falls back to its default
/// challenge order (HTTP-01 then TLS-ALPN-01) — useful when port 80 is
/// reachable from the internet but DNS automation isn't set up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsIssuer {
    pub email: Option<String>,
    pub dns_provider: Option<String>,
    pub staging: bool,
}

/// One row in the Ingress overview page — every route Caddy is serving,
/// engine-owned or static. Walked out of the admin API by
/// `CaddyApi::list_all_route_summaries`; consumed via the
/// `apps.caddy.routes` RPC. Read-only; the WebUI uses this to answer
/// "did my subdomain actually wire up?" / "what's exposed at /api/*?"
/// without making the operator shell in and read the live Caddy config.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CaddyRouteSummary {
    /// "host", "path", or "catch_all". The WebUI groups by this so the
    /// operator sees host-match (subdomain) routes separately from
    /// path-prefix routes.
    pub match_kind: String,
    /// Human-readable match value:
    /// - host_match: the hostname (`jellyfin.example.com`)
    /// - path_match: the first path glob (`/apps/haze/*`)
    /// - catch_all: `(any)` so the WebUI doesn't have to special-case
    ///   the empty string
    pub match_value: String,
    /// Reverse-proxy upstream (e.g. `127.0.0.1:4420`) when the route
    /// ends in a `reverse_proxy` handler. `None` for `file_server`,
    /// `static_response`, etc. — `handler_kind` carries that detail.
    pub upstream: Option<String>,
    /// Dominant handler kind, in display order: `reverse_proxy`,
    /// `file_server`, `static_response`, `rewrite`, or `other`. The
    /// WebUI uses this to render a meaningful "handler" column for
    /// rows whose upstream is None.
    pub handler_kind: String,
    /// `engine-app` when the route's `@id` carries the `nasty-app-`
    /// prefix (owned by AppsService::ingress_set); `static` for
    /// anything else (the Caddyfile-baked WebUI / API / WS routes).
    pub source: String,
    /// App name when `source` is `engine-app`; `None` otherwise.
    /// Lets the WebUI link the row back to the Apps page.
    pub app_name: Option<String>,
    /// Caddy server name (`srv0`, `srv1`, ...) so the WebUI can group
    /// by listener — the HTTP-to-HTTPS redirect lives on a different
    /// server and shouldn't be lumped in with the HTTPS routes.
    pub server: String,
    /// On-disk certificate Caddy currently serves for this route's host.
    /// Populated by the engine binary after `list_all_route_summaries`
    /// returns — nasty-apps doesn't have access to the cert directory
    /// or PEM parser. `None` for non-host routes (`path` / `catch_all`)
    /// and for host routes Caddy hasn't issued a cert for yet (the
    /// "pending" state — auto-HTTPS issues asynchronously on first
    /// request).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert: Option<HostCert>,
}

/// Subset of `nasty_system::settings::HostCertInfo` re-shaped for the
/// Ingress overview wire format. Defined here (rather than re-using
/// the nasty-system type directly) so `nasty-apps` doesn't grow a dep
/// on `nasty-system` just for the field — the engine binary copies the
/// fields across when enriching.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HostCert {
    pub issuer: Option<String>,
    pub issued: Option<String>,
    pub expires: Option<String>,
    /// Days until expiry from now; negative = expired. Lets the WebUI
    /// colour the badge (red ≤ 7, amber ≤ 30, green otherwise) without
    /// parsing the RFC-2822 expires string client-side.
    pub expires_in_days: Option<i64>,
    /// On-disk path; surfaced as a tooltip in the WebUI for debugging
    /// "which cert is this actually serving" questions.
    pub path: String,
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
                    subdomain: extract_route_host(&route),
                });
            }
        }
        Ok(out)
    }

    /// Walk every Caddy server's route list and produce one
    /// `CaddyRouteSummary` per route. Used by the Ingress overview
    /// page so the operator can see everything Caddy is serving —
    /// engine-owned app ingresses, the Caddyfile-baked WebUI / API /
    /// WS routes, and the HTTP-to-HTTPS redirect on the alt server.
    ///
    /// Best-effort per row: a route whose JSON shape doesn't match
    /// our expectations contributes a `(unknown)` summary rather than
    /// being dropped silently — the operator should be able to tell
    /// "Caddy has a thing here we don't understand" apart from "Caddy
    /// has nothing here".
    pub async fn list_all_route_summaries(&self) -> Result<Vec<CaddyRouteSummary>, String> {
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

        let mut out = Vec::new();
        let mut server_names: Vec<&String> = servers.keys().collect();
        server_names.sort();
        for name in server_names {
            let Some(routes) = servers.get(name).and_then(|s| s.get("routes")) else {
                continue;
            };
            let Some(routes) = routes.as_array() else {
                continue;
            };
            for route in routes {
                out.push(summarise_route(name, route));
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

    /// Replace Caddy's `apps.tls.automation.policies` array with the
    /// caller's managed-host set + the always-present `nasty.local`
    /// internal-CA fallback. Empty `policies` ⇒ only the fallback is
    /// pushed; ACME-off boxes still serve a working cert for IP and
    /// unknown-SNI traffic via the `tls internal` site in the
    /// Caddyfile.
    ///
    /// One policy per managed host, all sharing the same ACME issuer.
    /// Per-host policies keep failures isolated and make the
    /// `cert_info_for_host` view map one-to-one onto Caddy's storage.
    /// The `nasty.local` fallback is appended last so SNI matching
    /// hits managed hosts first.
    ///
    /// HTTP verb is PATCH: Caddy admin API uses PUT and POST with
    /// "create new" semantics that error 409 when the path already
    /// has a value. The Caddyfile-derived config always creates
    /// `apps.tls.automation` (because of `tls internal` on the
    /// fallback site), so the path is always pre-existing by the time
    /// we get here.
    ///
    /// Idempotent: identical input ⇒ identical PATCH body ⇒ Caddy
    /// no-ops internally. Failures (Caddy not running, JSON shape
    /// rejected) are returned for the caller to log and surface in
    /// `acme_status`.
    pub async fn set_tls_automation(
        &self,
        policies: &[TlsPolicy],
        issuer: &TlsIssuer,
    ) -> Result<(), String> {
        // 1) PATCH the policies array. This says HOW to issue each
        //    hostname's cert (ACME issuer + DNS provider config).
        let body = build_tls_automation_json(policies, issuer);
        let url = format!("{}/config/apps/tls/automation", self.base_url);
        let resp = self
            .client
            .patch(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("PATCH {url}: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("PATCH {url}: status {status} body={text}"));
        }

        // 2) PATCH `certificates.automate` with the same hostname set.
        //    This is the list Caddy walks to *trigger* issuance — without
        //    it the policies sit there describing how to issue certs
        //    Caddy never tries to obtain. The Caddyfile populates this
        //    from `tls internal` on the fallback site (nasty.local
        //    only); ACME-managed hosts are admin-API-only so they have
        //    to be added here explicitly.
        let automate: Vec<&str> = policies
            .iter()
            .map(|p| p.host.as_str())
            .chain(std::iter::once("nasty.local"))
            .collect();
        let automate_url = format!("{}/config/apps/tls/certificates/automate", self.base_url);
        let resp = self
            .client
            .patch(&automate_url)
            .json(&automate)
            .send()
            .await
            .map_err(|e| format!("PATCH {automate_url}: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("PATCH {automate_url}: status {status} body={text}"));
        }

        if policies.is_empty() {
            info!("caddy: TLS automation set to fallback-only (nasty.local internal CA)");
        } else {
            info!(
                "caddy: set TLS automation for {} managed host(s): {} (+ nasty.local fallback)",
                policies.len(),
                policies
                    .iter()
                    .map(|p| p.host.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(())
    }
}

/// Build the Caddy JSON route object for one app ingress.  Mirror
/// of `handle_path /apps/<name>/* { reverse_proxy 127.0.0.1:<port>
/// { header_up X-Real-IP {http.request.remote.host} } }` as produced
/// by `caddy adapt`, with `@id` so we can delete it by ID later.
fn build_route_json(route: &AppRoute) -> Value {
    // Subdomain mode matches by host and serves the app at its own root
    // — no path prefix to strip, no risk of the absolute-asset-path
    // failure that the post-install probe in #219 catches for path-prefix
    // mode. Path-prefix mode keeps the strip-prefix handler so the
    // upstream sees clean URLs (`/api/...` rather than `/apps/foo/api/...`).
    let reverse_proxy = json!({
        "handler": "reverse_proxy",
        "upstreams": [{"dial": format!("127.0.0.1:{}", route.host_port)}],
        "headers": {
            "request": {
                "set": {
                    "X-Real-Ip": ["{http.request.remote.host}"]
                }
            }
        },
        // Mirrors the Caddyfile's `stream_close_delay 30m` on the static
        // WS handlers — every other app install/remove triggers a Caddy
        // config reload (this very admin API call), and without the
        // delay any WS the app itself exposes would die on every
        // neighbouring app's lifecycle event.
        "stream_close_delay": "30m",
    });

    let (match_block, inner_handles) = match &route.subdomain {
        Some(host) => (json!([{"host": [host]}]), json!([reverse_proxy])),
        None => (
            json!([{"path": [format!("/apps/{}/*", route.name)]}]),
            json!([
                {
                    "handler": "rewrite",
                    "strip_path_prefix": format!("/apps/{}", route.name),
                },
                reverse_proxy,
            ]),
        ),
    };

    json!({
        "@id": format!("{ROUTE_ID_PREFIX}{}", route.name),
        "match": match_block,
        "handle": [{
            "handler": "subroute",
            "routes": [{
                "handle": inner_handles,
            }]
        }],
        "terminal": true
    })
}

/// Build the JSON body for `PATCH /config/apps/tls/automation`. One
/// policy per managed host (using the shared ACME issuer), plus a
/// trailing `nasty.local` policy that uses Caddy's internal CA — that
/// last entry mirrors what `tls internal` in the Caddyfile creates,
/// and we always include it so a PATCH that replaces the whole policy
/// list doesn't accidentally orphan the IP / unknown-SNI fallback
/// cert.
///
/// Order matters: managed hosts first, so Caddy's SNI matching hits
/// them before falling through to nasty.local.
fn build_tls_automation_json(policies: &[TlsPolicy], issuer: &TlsIssuer) -> Value {
    let mut policy_values: Vec<Value> = policies
        .iter()
        .map(|p| {
            json!({
                "subjects": [p.host.clone()],
                "issuers": [build_acme_issuer_json(issuer)],
            })
        })
        .collect();
    policy_values.push(json!({
        "subjects": ["nasty.local"],
        "issuers": [{ "module": "internal" }],
    }));
    json!({ "policies": policy_values })
}

/// Build one ACME issuer block for an automation policy. Shape mirrors
/// `caddy adapt`'s output for a `tls EMAIL { dns PROVIDER ... }` block —
/// emitted by hand here because the admin API takes JSON, not Caddyfile.
///
/// DNS-01 vs HTTP-01/TLS-ALPN-01 split: when `dns_provider` is None we
/// omit the `challenges` field entirely and let Caddy use its default
/// challenge order (HTTP-01 then TLS-ALPN-01). When DNS is set, we emit
/// only the `dns` challenge — pinning matters because some users pick
/// DNS specifically because port 80 isn't reachable from the public
/// internet, and falling back to HTTP-01 in that case would just rack
/// up failed attempts at the ACME server.
fn build_acme_issuer_json(issuer: &TlsIssuer) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("module".into(), json!("acme"));
    if let Some(email) = issuer.email.as_deref()
        && !email.is_empty()
    {
        obj.insert("email".into(), json!(email));
    }
    if issuer.staging {
        // Staging directory URL — accepts any caller, doesn't count
        // against Let's Encrypt's production rate limits. Used for
        // testing changes to the cert pipeline without burning quota.
        obj.insert(
            "ca".into(),
            json!("https://acme-staging-v02.api.letsencrypt.org/directory"),
        );
    }
    if let Some(provider) = issuer.dns_provider.as_deref() {
        // `resolvers` tells Caddy/certmagic which DNS servers to use
        // when verifying that the TXT challenge record has propagated.
        // Without this, propagation checks go through the local stub
        // resolver (systemd-resolved on most boxes), which in our setup
        // (split-horizon DNS, internal-only resolver, or just a slow
        // ISP cache) may not see the record for minutes — long enough
        // that Caddy gives up. Pin to Cloudflare + Google so the box's
        // own resolver setup doesn't block issuance.
        //
        // `propagation_delay` makes Caddy sleep N seconds after the
        // provider's API call before issuing the verification query.
        // Without this Caddy queries instantly, hits the resolver's
        // negative cache (the prior NXDOMAIN for `_acme-challenge.X`
        // is cached for the SOA's MINIMUM TTL — often 1 hour for
        // Cloudflare), sees no record, retries on a backoff that
        // never converges within the propagation timeout. 30s
        // matches what the lego flow used (`--dns.propagation-wait`).
        //
        // Both settings mirror the lego defaults; hardcoded for now
        // because the settings struct doesn't expose knobs. Easy to
        // make configurable when a user has a real reason.
        obj.insert(
            "challenges".into(),
            json!({
                "dns": {
                    "provider": dns_provider_json(provider),
                    "resolvers": ["1.1.1.1", "8.8.8.8"],
                    "propagation_delay": "30s"
                }
            }),
        );
    }
    Value::Object(obj)
}

/// Map a DNS provider code to its caddy-dns plugin JSON shape. Each
/// plugin exposes the credentials it needs as named JSON fields; the
/// values are `{env.NAME}` placeholders that Caddy expands at request
/// time from its EnvironmentFile (sourced from `tls_dns_credentials`).
///
/// Env var names mirror what users typically write in the credentials
/// textarea: `CLOUDFLARE_DNS_API_TOKEN=…`, `PORKBUN_API_KEY=…`, etc.
/// They don't necessarily match what each plugin's README suggests as
/// a convention — but matching what's already saved in 0.0.7 boxes'
/// `settings.json` matters more than matching upstream docs.
///
/// Unknown providers get a stub object with just the name; Caddy will
/// reject the PUT with "unknown module" if the plugin isn't compiled
/// in, and the engine surfaces that through `acme_status`.
fn dns_provider_json(provider: &str) -> Value {
    match provider {
        "cloudflare" => json!({
            "name": "cloudflare",
            "api_token": "{env.CLOUDFLARE_DNS_API_TOKEN}"
        }),
        "duckdns" => json!({
            "name": "duckdns",
            "api_token": "{env.DUCKDNS_TOKEN}"
        }),
        "linode" => json!({
            "name": "linode",
            "api_token": "{env.LINODE_TOKEN}"
        }),
        "desec" => json!({
            "name": "desec",
            "token": "{env.DESEC_TOKEN}"
        }),
        "hetzner" => json!({
            "name": "hetzner",
            "api_token": "{env.HETZNER_API_TOKEN}"
        }),
        // route53 plugin reads AWS_REGION / AWS_ACCESS_KEY_ID /
        // AWS_SECRET_ACCESS_KEY (+ AWS_SESSION_TOKEN) from env on its
        // own. Empty config object lets it self-configure.
        "route53" => json!({ "name": "route53" }),
        "porkbun" => json!({
            "name": "porkbun",
            "api_key": "{env.PORKBUN_API_KEY}",
            "api_secret_key": "{env.PORKBUN_SECRET_API_KEY}"
        }),
        "namecheap" => json!({
            "name": "namecheap",
            "user": "{env.NAMECHEAP_USER}",
            "api_key": "{env.NAMECHEAP_API_KEY}",
            "client_ip": "{env.NAMECHEAP_CLIENT_IP}"
        }),
        "rfc2136" => json!({
            "name": "rfc2136",
            "key_name": "{env.RFC2136_KEY_NAME}",
            "key": "{env.RFC2136_KEY}",
            "key_alg": "{env.RFC2136_KEY_ALG}",
            "server": "{env.RFC2136_SERVER}"
        }),
        other => json!({ "name": other }),
    }
}

/// Pull the first hostname out of the route's `match[].host[]` array,
/// if any. Returns None for path-prefix routes (their match block has
/// `path` instead of `host`). Used by `list_app_routes` to round-trip
/// subdomain mode back into `AppRoute.subdomain` so the reconcile pass
/// can rebuild Caddy routes in the same mode after a restart.
fn extract_route_host(route: &Value) -> Option<String> {
    let matches = route.get("match")?.as_array()?;
    for m in matches {
        if let Some(hosts) = m.get("host").and_then(Value::as_array)
            && let Some(first) = hosts.first().and_then(Value::as_str)
        {
            return Some(first.to_string());
        }
    }
    None
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

/// Convert one raw Caddy route into the summary the Ingress overview page
/// renders. Best-effort: unfamiliar shapes contribute `(unknown)` rather
/// than being dropped so the operator can tell "Caddy has a thing here"
/// from "Caddy has nothing here". Free function (not a method) so the
/// match-by-row unit tests don't have to spin up a CaddyApi.
fn summarise_route(server: &str, route: &Value) -> CaddyRouteSummary {
    // ── Source attribution from @id ───────────────────────────
    let id = route.get("@id").and_then(Value::as_str);
    let app_name = id
        .and_then(|s| s.strip_prefix(ROUTE_ID_PREFIX))
        .map(String::from);
    let source = if app_name.is_some() {
        "engine-app"
    } else {
        "static"
    };

    // ── Match block: pick the first matcher we recognise. ─────
    // Real-world Caddy routes can have multiple alternatives in the
    // match array (an OR). We surface the first one; that matches the
    // route-id-keyed UX (one row per route) and avoids the table
    // exploding for every alternative spelling.
    let (match_kind, match_value) = match route.get("match").and_then(Value::as_array) {
        None => ("catch_all".to_string(), "(any)".to_string()),
        Some(arr) if arr.is_empty() => ("catch_all".to_string(), "(any)".to_string()),
        Some(arr) => {
            let first = &arr[0];
            // An empty matcher object (`{}`) is Caddy's catch-all.
            if first.as_object().is_some_and(|o| o.is_empty()) {
                ("catch_all".to_string(), "(any)".to_string())
            } else if let Some(hosts) = first.get("host").and_then(Value::as_array)
                && let Some(h) = hosts.first().and_then(Value::as_str)
            {
                ("host".to_string(), h.to_string())
            } else if let Some(paths) = first.get("path").and_then(Value::as_array)
                && let Some(p) = paths.first().and_then(Value::as_str)
            {
                ("path".to_string(), p.to_string())
            } else {
                // A matcher shape we don't model (e.g. header, method) —
                // surface the JSON so the operator at least sees there's
                // *something* there.
                ("other".to_string(), first.to_string())
            }
        }
    };

    // ── Handler chain: walk to find the dominant kind. ────────
    // Routes are usually subroute → inner routes → handlers. We walk
    // until we hit a recognised leaf handler. Order matters only for
    // displaying which kind is "dominant"; the actual ordering in the
    // route is handled by Caddy and not our concern.
    let (handler_kind, upstream) = extract_handler_summary(route);

    CaddyRouteSummary {
        match_kind,
        match_value,
        upstream,
        handler_kind,
        source: source.to_string(),
        app_name,
        server: server.to_string(),
        // `cert` is left None here — the cert directory lives outside
        // this crate's blast radius. The engine binary (which depends on
        // nasty-system) enriches host-match rows after the walker
        // returns. See `list_caddy_routes` in nasty-engine/src/router/apps.rs.
        cert: None,
    }
}

/// Walk a route JSON to identify its leaf handler. Returns the handler
/// kind (`reverse_proxy`, `file_server`, `static_response`, `rewrite`,
/// `other`, or `unknown` when the chain has no handler we recognise)
/// and the reverse_proxy upstream (`127.0.0.1:port`) when present.
///
/// Recursive in the static sense: subroute → inner routes → handlers
/// is the only nesting Caddy emits in practice for the patterns NASty
/// uses, so we just do two levels by hand rather than reaching for a
/// general tree walker.
fn extract_handler_summary(route: &Value) -> (String, Option<String>) {
    fn classify(handler: &Value) -> Option<(String, Option<String>)> {
        let kind = handler.get("handler").and_then(Value::as_str)?;
        match kind {
            "reverse_proxy" => {
                let upstream = handler
                    .get("upstreams")
                    .and_then(Value::as_array)
                    .and_then(|us| us.first())
                    .and_then(|u| u.get("dial"))
                    .and_then(Value::as_str)
                    .map(String::from);
                Some(("reverse_proxy".to_string(), upstream))
            }
            "file_server" => Some(("file_server".to_string(), None)),
            "static_response" => Some(("static_response".to_string(), None)),
            "rewrite" => Some(("rewrite".to_string(), None)),
            other => Some((other.to_string(), None)),
        }
    }

    let Some(handlers) = route.get("handle").and_then(Value::as_array) else {
        return ("unknown".to_string(), None);
    };
    for handler in handlers {
        // Subroute: dive into the nested route list.
        if handler.get("handler").and_then(Value::as_str) == Some("subroute") {
            let Some(inner_routes) = handler.get("routes").and_then(Value::as_array) else {
                continue;
            };
            for inner in inner_routes {
                let Some(inner_handles) = inner.get("handle").and_then(Value::as_array) else {
                    continue;
                };
                // Prefer reverse_proxy over rewrite/headers (the leaf of
                // an app route is reverse_proxy; rewrite/headers are
                // preprocessing the operator doesn't care about for the
                // overview).
                let mut best: Option<(String, Option<String>)> = None;
                for h in inner_handles {
                    if let Some(classified) = classify(h) {
                        let is_proxy = classified.0 == "reverse_proxy";
                        let is_terminal = matches!(
                            classified.0.as_str(),
                            "reverse_proxy" | "file_server" | "static_response"
                        );
                        if is_proxy {
                            return classified;
                        }
                        if is_terminal && best.is_none() {
                            best = Some(classified);
                        }
                    }
                }
                if let Some(b) = best {
                    return b;
                }
            }
            continue;
        }
        if let Some(classified) = classify(handler) {
            return classified;
        }
    }
    ("unknown".to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_route_json_has_expected_shape() {
        let r = AppRoute {
            name: "foo".into(),
            host_port: 18080,
            subdomain: None,
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
    fn build_route_json_subdomain_mode_matches_by_host() {
        // Subdomain mode: match block carries `host`, not `path`; no
        // strip_path_prefix handler (app is at root). Caddy's automatic
        // HTTPS picks up the hostname from the match block.
        let r = AppRoute {
            name: "jellyfin".into(),
            host_port: 8096,
            subdomain: Some("jellyfin.example.com".into()),
        };
        let v = build_route_json(&r);
        assert_eq!(v["@id"], "nasty-app-jellyfin");
        assert_eq!(v["match"][0]["host"][0], "jellyfin.example.com");
        assert!(v["match"][0].get("path").is_none());
        let inner_handle = &v["handle"][0]["routes"][0]["handle"];
        // First (and only) handler is the reverse_proxy — no rewrite
        // in front of it.
        assert_eq!(inner_handle[0]["handler"], "reverse_proxy");
        assert!(inner_handle.as_array().unwrap().len() == 1);
        assert_eq!(inner_handle[0]["upstreams"][0]["dial"], "127.0.0.1:8096");
    }

    #[test]
    fn extract_upstream_port_roundtrips_through_build() {
        let r = AppRoute {
            name: "bar".into(),
            host_port: 9999,
            subdomain: None,
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

    #[test]
    fn extract_route_host_roundtrips_through_build_subdomain() {
        // List → set round trip in subdomain mode must preserve the
        // hostname so reconcile_app_routes can rebuild the same Caddy
        // shape after an engine restart.
        let r = AppRoute {
            name: "vault".into(),
            host_port: 11000,
            subdomain: Some("vault.example.com".into()),
        };
        let v = build_route_json(&r);
        assert_eq!(extract_route_host(&v), Some("vault.example.com".into()));
        assert_eq!(extract_upstream_port(&v), Some(11000));
    }

    #[test]
    fn extract_route_host_returns_none_for_path_mode() {
        // Path-prefix mode has no host match — the helper must say
        // None so `list_app_routes` round-trips `subdomain: None`.
        let r = AppRoute {
            name: "foo".into(),
            host_port: 1234,
            subdomain: None,
        };
        let v = build_route_json(&r);
        assert_eq!(extract_route_host(&v), None);
    }

    // ── summarise_route ──

    #[test]
    fn summarise_path_prefix_app_route_attributes_to_engine() {
        let r = AppRoute {
            name: "haze".into(),
            host_port: 4420,
            subdomain: None,
        };
        let v = build_route_json(&r);
        let s = summarise_route("srv0", &v);
        assert_eq!(s.match_kind, "path");
        assert_eq!(s.match_value, "/apps/haze/*");
        assert_eq!(s.handler_kind, "reverse_proxy");
        assert_eq!(s.upstream.as_deref(), Some("127.0.0.1:4420"));
        assert_eq!(s.source, "engine-app");
        assert_eq!(s.app_name.as_deref(), Some("haze"));
        assert_eq!(s.server, "srv0");
    }

    #[test]
    fn summarise_subdomain_app_route_picks_up_host() {
        let r = AppRoute {
            name: "jellyfin".into(),
            host_port: 8096,
            subdomain: Some("jellyfin.example.com".into()),
        };
        let v = build_route_json(&r);
        let s = summarise_route("srv0", &v);
        assert_eq!(s.match_kind, "host");
        assert_eq!(s.match_value, "jellyfin.example.com");
        assert_eq!(s.handler_kind, "reverse_proxy");
        assert_eq!(s.upstream.as_deref(), Some("127.0.0.1:8096"));
        assert_eq!(s.source, "engine-app");
        assert_eq!(s.app_name.as_deref(), Some("jellyfin"));
    }

    #[test]
    fn summarise_caddyfile_route_marks_source_static() {
        // Routes baked into the Caddyfile have no @id (or one that
        // doesn't carry our prefix). The summary must mark them
        // `static` and leave app_name None so the WebUI doesn't
        // pretend the operator can edit them via apps.ingress.set.
        let v = json!({
            "match": [{"path": ["/api/*"]}],
            "handle": [{"handler": "subroute", "routes": [{"handle": [
                {"handler": "reverse_proxy", "upstreams": [{"dial": "127.0.0.1:2137"}]}
            ]}]}],
            "terminal": true
        });
        let s = summarise_route("srv0", &v);
        assert_eq!(s.match_kind, "path");
        assert_eq!(s.match_value, "/api/*");
        assert_eq!(s.source, "static");
        assert_eq!(s.app_name, None);
        assert_eq!(s.upstream.as_deref(), Some("127.0.0.1:2137"));
    }

    #[test]
    fn summarise_catch_all_route_has_match_value_any() {
        // The WebUI SPA fallback is shaped as a route with the empty
        // matcher `{}`. The summary surfaces a non-empty match_value
        // ("(any)") so the operator sees a meaningful row instead of
        // a blank cell, and tags handler_kind as file_server.
        let v = json!({
            "match": [{}],
            "handle": [{"handler": "subroute", "routes": [{"handle": [
                {"handler": "file_server", "hide": ["/etc/caddy/caddy_config"]}
            ]}]}]
        });
        let s = summarise_route("srv0", &v);
        assert_eq!(s.match_kind, "catch_all");
        assert_eq!(s.match_value, "(any)");
        assert_eq!(s.handler_kind, "file_server");
        assert_eq!(s.upstream, None);
        assert_eq!(s.source, "static");
    }

    // ── build_tls_automation_json ──

    #[test]
    fn tls_automation_empty_policies_yields_fallback_only() {
        // The "ACME off" PATCH body. We still emit the trailing
        // nasty.local internal-CA policy so the IP / unknown-SNI
        // fallback keeps working — without it, PATCHing an empty
        // policies array would orphan the cert that `tls internal` in
        // the Caddyfile depends on.
        let body = build_tls_automation_json(
            &[],
            &TlsIssuer {
                email: None,
                dns_provider: None,
                staging: false,
            },
        );
        let policies = body["policies"].as_array().unwrap();
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0]["subjects"][0], "nasty.local");
        assert_eq!(policies[0]["issuers"][0]["module"], "internal");
    }

    #[test]
    fn tls_automation_managed_hosts_precede_fallback() {
        // SNI matching order: managed hosts first so a request for
        // nas.example.com hits its ACME-issued cert, not the local-CA
        // fallback. Verifying the order explicitly because Caddy walks
        // the policies array top-to-bottom and a future "sort
        // alphabetically" refactor would silently break SNI routing.
        let body = build_tls_automation_json(
            &[TlsPolicy {
                host: "nas.example.com".into(),
            }],
            &TlsIssuer {
                email: None,
                dns_provider: None,
                staging: false,
            },
        );
        let policies = body["policies"].as_array().unwrap();
        assert_eq!(policies.len(), 2);
        assert_eq!(policies[0]["subjects"][0], "nas.example.com");
        assert_eq!(policies[1]["subjects"][0], "nasty.local");
    }

    #[test]
    fn tls_automation_cloudflare_dns_shape() {
        let body = build_tls_automation_json(
            &[TlsPolicy {
                host: "nas.example.com".into(),
            }],
            &TlsIssuer {
                email: Some("ops@example.com".into()),
                dns_provider: Some("cloudflare".into()),
                staging: false,
            },
        );
        let p = &body["policies"][0];
        assert_eq!(p["subjects"][0], "nas.example.com");
        let issuer = &p["issuers"][0];
        assert_eq!(issuer["module"], "acme");
        assert_eq!(issuer["email"], "ops@example.com");
        // No staging CA → no `ca` override field at all (Caddy uses prod).
        assert!(issuer.get("ca").is_none());
        let prov = &issuer["challenges"]["dns"]["provider"];
        assert_eq!(prov["name"], "cloudflare");
        assert_eq!(prov["api_token"], "{env.CLOUDFLARE_DNS_API_TOKEN}");
    }

    #[test]
    fn tls_automation_staging_sets_ca_url() {
        let body = build_tls_automation_json(
            &[TlsPolicy {
                host: "h.example".into(),
            }],
            &TlsIssuer {
                email: None,
                dns_provider: None,
                staging: true,
            },
        );
        let issuer = &body["policies"][0]["issuers"][0];
        assert_eq!(
            issuer["ca"],
            "https://acme-staging-v02.api.letsencrypt.org/directory"
        );
    }

    #[test]
    fn tls_automation_dns_challenge_pins_external_resolvers() {
        // DNS propagation check has to bypass the box's stub resolver
        // (systemd-resolved on most boxes) — without an explicit
        // resolver list, certmagic queries 127.0.0.53, may not see the
        // freshly-set TXT record for minutes, and gives up. Pin to
        // Cloudflare + Google so issuance doesn't depend on whatever
        // DNS the operator has wired up locally. Mirrors what the
        // lego flow did with `--dns.resolvers` before the switch.
        let body = build_tls_automation_json(
            &[TlsPolicy {
                host: "h.example".into(),
            }],
            &TlsIssuer {
                email: None,
                dns_provider: Some("cloudflare".into()),
                staging: false,
            },
        );
        let resolvers = &body["policies"][0]["issuers"][0]["challenges"]["dns"]["resolvers"];
        let resolvers = resolvers.as_array().expect("resolvers array present");
        assert_eq!(resolvers.len(), 2);
        assert_eq!(resolvers[0], "1.1.1.1");
        assert_eq!(resolvers[1], "8.8.8.8");
    }

    #[test]
    fn tls_automation_no_dns_provider_omits_challenges() {
        // Without a DNS provider Caddy uses its default challenge order
        // (HTTP-01, TLS-ALPN-01). Asserting the `challenges` field is
        // absent — not present-but-empty — because Caddy treats
        // `challenges: {}` as "no challenge modules enabled" which
        // would block all issuance.
        let body = build_tls_automation_json(
            &[TlsPolicy {
                host: "h.example".into(),
            }],
            &TlsIssuer {
                email: Some("a@b".into()),
                dns_provider: None,
                staging: false,
            },
        );
        let issuer = &body["policies"][0]["issuers"][0];
        assert!(issuer.get("challenges").is_none());
    }

    #[test]
    fn tls_automation_one_policy_per_host() {
        // Per-host policies isolate failures and match `cert_info_for_host`'s
        // one-cert-per-hostname assumption. Two managed subjects in →
        // two managed policies out + the nasty.local fallback, not one
        // combined-SAN policy.
        let body = build_tls_automation_json(
            &[
                TlsPolicy {
                    host: "a.example".into(),
                },
                TlsPolicy {
                    host: "b.example".into(),
                },
            ],
            &TlsIssuer {
                email: None,
                dns_provider: None,
                staging: false,
            },
        );
        let policies = body["policies"].as_array().unwrap();
        assert_eq!(policies.len(), 3);
        assert_eq!(policies[0]["subjects"][0], "a.example");
        assert_eq!(policies[1]["subjects"][0], "b.example");
        assert_eq!(policies[2]["subjects"][0], "nasty.local");
    }

    #[test]
    fn dns_provider_json_unknown_provider_is_stub() {
        // A provider name we don't have a hand-rolled shape for (e.g. a
        // newly-added plugin) gets a bare `{name: <code>}` object.
        // Caddy will reject if the plugin isn't compiled in; we don't
        // try to be smart at the engine layer.
        let v = dns_provider_json("freenom");
        assert_eq!(v, json!({"name": "freenom"}));
    }

    #[test]
    fn summarise_http_redirect_server_static_response() {
        // The HTTP→HTTPS redirect on srv1 is a static_response with a
        // Location header. The overview should show it as a row with
        // handler_kind = static_response and server = srv1 so the
        // WebUI groups it separately from the HTTPS routes.
        let v = json!({
            "match": [{}],
            "handle": [{
                "handler": "static_response",
                "status_code": 301,
                "headers": {"Location": ["https://{http.request.host}{http.request.uri}"]}
            }]
        });
        let s = summarise_route("srv1", &v);
        assert_eq!(s.match_kind, "catch_all");
        assert_eq!(s.handler_kind, "static_response");
        assert_eq!(s.server, "srv1");
        assert_eq!(s.source, "static");
    }
}
