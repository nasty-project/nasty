//! Confirm-or-rollback management listener changes, separate from TLS settings.
use crate::firewall::{FirewallService, PortSpec, webui_ports};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

const STATE: &str = "/var/lib/nasty/webui-listeners.json";
const ADMIN: &str = "http://127.0.0.1:2019/config/apps/http/";
const TIMEOUT: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ListenerPorts {
    pub https_port: u16,
    /// None disables the plaintext redirect listener, not HTTPS.
    pub http_port: Option<u16>,
}

impl ListenerPorts {
    pub fn validate(self) -> Result<(), String> {
        if self.https_port == 0 || self.http_port == Some(0) {
            return Err("Ports must be between 1 and 65535".into());
        }
        if self.http_port == Some(self.https_port) {
            return Err("HTTP and HTTPS ports must differ".into());
        }
        if [Some(self.https_port), self.http_port]
            .into_iter()
            .flatten()
            .any(|p| [2019, 2137].contains(&p))
        {
            return Err("Caddy admin and engine API ports are reserved".into());
        }
        Ok(())
    }
    pub fn ports(self) -> Vec<PortSpec> {
        webui_ports(self.https_port, self.http_port)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PendingListeners {
    pub txn_id: String,
    pub ports: ListenerPorts,
    pub deadline: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListenerState {
    pub confirmed: ListenerPorts,
    pub pending: Option<PendingListeners>,
    pub last_error: Option<String>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// NixOS options remain the defaults until a UI transaction is journaled.
pub fn load() -> Result<ListenerState, String> {
    match std::fs::read(STATE) {
        Ok(bytes) => {
            let state: ListenerState = serde_json::from_slice(&bytes)
                .map_err(|e| format!("Invalid WebUI listener state: {e}"))?;
            state.confirmed.validate()?;
            if let Some(pending) = &state.pending {
                pending.ports.validate()?;
            }
            Ok(state)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let https_port = std::env::var("NASTY_WEBUI_HTTPS_PORT")
                .unwrap_or_else(|_| "443".into())
                .parse()
                .map_err(|_| "Invalid default HTTPS port")?;
            let http = std::env::var("NASTY_WEBUI_HTTP_PORT").unwrap_or_else(|_| "80".into());
            let http_port = if http == "disabled" {
                None
            } else {
                Some(http.parse().map_err(|_| "Invalid default HTTP port")?)
            };
            let confirmed = ListenerPorts {
                https_port,
                http_port,
            };
            confirmed.validate()?;
            Ok(ListenerState {
                confirmed,
                pending: None,
                last_error: None,
            })
        }
        Err(e) => Err(format!("Read WebUI listener state: {e}")),
    }
}

async fn save(state: &ListenerState) -> Result<(), String> {
    tokio::fs::create_dir_all("/var/lib/nasty")
        .await
        .map_err(|e| e.to_string())?;
    let tmp = format!("{STATE}.tmp");
    tokio::fs::write(
        &tmp,
        serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?,
    )
    .await
    .map_err(|e| e.to_string())?;
    tokio::fs::rename(tmp, STATE)
        .await
        .map_err(|e| e.to_string())
}

/// Preserve app routes, TLS policies, certificates and other Caddy apps.
fn configure_http(http: &mut Value, ports: ListenerPorts) -> Result<(), String> {
    ports.validate()?;
    let servers = http
        .get_mut("servers")
        .and_then(Value::as_object_mut)
        .ok_or("Caddy has no HTTP servers")?;
    let tls_names: Vec<_> = servers
        .iter()
        .filter(|(_, s)| {
            s.get("tls_connection_policies")
                .and_then(Value::as_array)
                .is_some_and(|p| !p.is_empty())
        })
        .map(|(n, _)| n.clone())
        .collect();
    if tls_names.len() != 1 {
        return Err(
            "Expected exactly one WebUI TLS server; refusing ambiguous Caddy configuration".into(),
        );
    }
    let tls = servers.get_mut(&tls_names[0]).unwrap();
    let addresses = tls["listen"]
        .as_array()
        .ok_or("WebUI TLS server has no listener addresses")?;
    let mut listen = Vec::new();
    for address in addresses {
        let (host, old_port) = address
            .as_str()
            .and_then(|a| a.rsplit_once(':'))
            .ok_or("Unsupported Caddy listener address")?;
        old_port
            .parse::<u16>()
            .map_err(|_| "Unsupported Caddy listener port")?;
        listen.push(format!("{host}:{}", ports.https_port));
    }
    tls["listen"] = json!(listen);
    tls["automatic_https"]["disable_redirects"] = json!(true);
    // Identify our generated ID or the narrow baked redirect shape.
    servers.retain(|_, s| !is_redirect_server(s));
    if let Some(port) = ports.http_port {
        if servers.contains_key("nasty_webui_redirect") {
            return Err("Caddy redirect server name is already in use".into());
        }
        let suffix = if ports.https_port == 443 {
            String::new()
        } else {
            format!(":{}", ports.https_port)
        };
        servers.insert("nasty_webui_redirect".into(), json!({"listen":[format!(":{port}")], "routes":[{"@id":"nasty-webui-redirect", "handle":[
            {"handler":"map", "source":"{http.request.host}", "destinations":["{nasty.redirect_host}"], "mappings":[{"input_regexp":"^(.+:.*)$", "outputs":["[${1}]"]}], "defaults":["{http.request.host}"]},
            {"handler":"static_response", "status_code":308, "headers":{"Location":[format!("https://{{nasty.redirect_host}}{suffix}{{http.request.uri}}")]} }]}]}));
    }
    http["https_port"] = json!(ports.https_port);
    http["http_port"] = json!(ports.http_port.unwrap_or(80));
    Ok(())
}

fn is_redirect_server(server: &Value) -> bool {
    server
        .get("routes")
        .and_then(Value::as_array)
        .is_some_and(|routes| {
            routes.len() == 1
                && (routes[0].get("@id").and_then(Value::as_str) == Some("nasty-webui-redirect")
                    || routes[0]
                        .get("handle")
                        .and_then(Value::as_array)
                        .is_some_and(|handlers| {
                            handlers.len() == 1
                                && handlers[0]["handler"] == "static_response"
                                && handlers[0]["headers"]["Location"].as_array().is_some_and(
                                    |locations| {
                                        locations.len() == 1
                                            && locations[0].as_str().is_some_and(|url| {
                                                url.starts_with("https://{http.request.host}")
                                                    && url.ends_with("{http.request.uri}")
                                            })
                                    },
                                )
                        }))
        })
}

pub async fn apply_caddy(ports: ListenerPorts) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    for _ in 0..4 {
        let response = client
            .get(ADMIN)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        let etag = response.headers().get(reqwest::header::ETAG).cloned().ok_or("Caddy did not return an ETag; refusing unsafe concurrent configuration replacement")?;
        let mut http: Value = response.json().await.map_err(|e| e.to_string())?;
        let previous = http.clone();
        configure_http(&mut http, ports)?;
        if http == previous {
            return Ok(());
        }
        let response = client
            .patch(ADMIN)
            .header(reqwest::header::IF_MATCH, etag)
            .json(&http)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status() == reqwest::StatusCode::PRECONDITION_FAILED {
            continue;
        }
        if !response.status().is_success() {
            return Err(format!(
                "Caddy rejected listener configuration: {}",
                response.text().await.unwrap_or_default()
            ));
        }
        return Ok(());
    }
    Err("Caddy configuration changed concurrently; retry the port change".into())
}

pub struct WebuiService {
    state: Mutex<ListenerState>,
    recovering: std::sync::atomic::AtomicBool,
}

impl WebuiService {
    pub fn new() -> Result<Self, String> {
        Ok(Self::from_state(load()?))
    }
    fn from_state(mut state: ListenerState) -> Self {
        // Restart/reboot never implicitly confirms an unverified change.
        state.pending = None;
        Self {
            state: Mutex::new(state),
            recovering: std::sync::atomic::AtomicBool::new(true),
        }
    }
    pub async fn get(&self) -> ListenerState {
        self.state.lock().await.clone()
    }
    pub async fn update(
        &self,
        ports: ListenerPorts,
        firewall: &FirewallService,
    ) -> Result<ListenerState, String> {
        ports.validate()?;
        let mut state = self.state.lock().await;
        if self.recovering.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("WebUI listener startup recovery has not completed".into());
        }
        if state.pending.is_some() {
            return Err("Confirm or roll back the pending WebUI port change first".into());
        }
        if ports == state.confirmed {
            return Ok(state.clone());
        }
        state.last_error = None;
        state.pending = Some(PendingListeners {
            txn_id: uuid::Uuid::new_v4().to_string(),
            ports,
            deadline: now() + TIMEOUT,
        });
        if let Err(e) = save(&state).await {
            state.pending = None;
            return Err(e);
        }
        let result = async {
            firewall
                .set_webui_ports(ports.https_port, ports.http_port)
                .await?;
            apply_caddy(ports).await
        }
        .await;
        if let Err(e) = result {
            state.last_error = Some(e.clone());
            if let Err(rollback) = self.rollback_locked(&mut state, firewall).await {
                return Err(format!("{e}; rollback failed: {rollback}"));
            }
            return Err(e);
        }
        Ok(state.clone())
    }
    pub async fn confirm(&self, txn_id: &str) -> Result<ListenerState, String> {
        let mut state = self.state.lock().await;
        if self.recovering.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("WebUI listeners are being recovered; cannot confirm".into());
        }
        let pending = state
            .pending
            .as_ref()
            .ok_or("No pending WebUI port change")?;
        if pending.txn_id != txn_id || now() >= pending.deadline {
            return Err("Unknown or expired WebUI port transaction".into());
        }
        let health: Value = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| e.to_string())?
            .get(format!(
                "https://127.0.0.1:{}/health",
                pending.ports.https_port
            ))
            .send()
            .await
            .map_err(|e| format!("New HTTPS listener is not reachable: {e}"))?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        if health["status"] != "ok" {
            return Err("New HTTPS listener failed its health check".into());
        }
        if now() >= pending.deadline {
            return Err("WebUI port transaction expired during health check".into());
        }
        let confirmed = ListenerState {
            confirmed: pending.ports,
            pending: None,
            last_error: None,
        };
        save(&confirmed).await?;
        *state = confirmed;
        Ok(state.clone())
    }
    async fn rollback_locked(
        &self,
        state: &mut ListenerState,
        firewall: &FirewallService,
    ) -> Result<(), String> {
        self.recovering
            .store(true, std::sync::atomic::Ordering::SeqCst);
        apply_caddy(state.confirmed).await?;
        firewall
            .set_webui_ports(state.confirmed.https_port, state.confirmed.http_port)
            .await?;
        let restored = ListenerState {
            pending: None,
            ..state.clone()
        };
        // Do not freeze baked defaults merely because the engine started.
        if std::path::Path::new(STATE).exists() {
            save(&restored).await?;
        }
        *state = restored;
        self.recovering
            .store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    pub async fn rollback(&self, firewall: &FirewallService) -> Result<ListenerState, String> {
        let mut state = self.state.lock().await;
        self.rollback_locked(&mut state, firewall).await?;
        Ok(state.clone())
    }
    pub async fn tick(&self, firewall: &FirewallService) -> Result<(), String> {
        let mut state = self.state.lock().await;
        if (self.recovering.load(std::sync::atomic::Ordering::SeqCst)
            || state.pending.as_ref().is_some_and(|p| now() >= p.deadline))
            && let Err(e) = self.rollback_locked(&mut state, firewall).await
        {
            state.last_error = Some(format!("Rollback failed; retrying: {e}"));
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pending_state(deadline: u64) -> ListenerState {
        ListenerState {
            confirmed: ListenerPorts {
                https_port: 443,
                http_port: Some(80),
            },
            pending: Some(PendingListeners {
                txn_id: "test".into(),
                ports: ListenerPorts {
                    https_port: 8443,
                    http_port: None,
                },
                deadline,
            }),
            last_error: None,
        }
    }
    #[tokio::test]
    async fn restart_drops_unconfirmed_candidate_and_requires_recovery() {
        let service = WebuiService::from_state(pending_state(now() + TIMEOUT));
        let state = service.get().await;
        assert_eq!(state.confirmed.https_port, 443);
        assert!(state.pending.is_none());
        assert!(
            service
                .update(
                    ListenerPorts {
                        https_port: 8443,
                        http_port: None
                    },
                    &FirewallService::new()
                )
                .await
                .unwrap_err()
                .contains("recovery")
        );
    }
    #[tokio::test]
    async fn expired_unknown_and_overlapping_transactions_are_rejected() {
        let service = WebuiService {
            state: Mutex::new(pending_state(now() - 1)),
            recovering: std::sync::atomic::AtomicBool::new(false),
        };
        assert!(
            service
                .confirm("test")
                .await
                .unwrap_err()
                .contains("expired")
        );
        service
            .state
            .lock()
            .await
            .pending
            .as_mut()
            .unwrap()
            .deadline = now() + TIMEOUT;
        assert!(
            service
                .confirm("other")
                .await
                .unwrap_err()
                .contains("Unknown")
        );
        assert!(
            service
                .update(
                    ListenerPorts {
                        https_port: 9443,
                        http_port: None
                    },
                    &FirewallService::new()
                )
                .await
                .unwrap_err()
                .contains("pending")
        );
    }
    fn config() -> Value {
        json!({"servers": {"srv0":{"listen":[":443"], "tls_connection_policies":[{}], "routes":[{"@id":"nasty-app-demo"}]}, "srv1":{"listen":[":80"], "routes":[{"@id":"nasty-webui-redirect"}]} }})
    }
    #[test]
    fn custom_ports_preserve_routes_and_remove_default_listeners() {
        let mut http = config();
        configure_http(
            &mut http,
            ListenerPorts {
                https_port: 8443,
                http_port: Some(8080),
            },
        )
        .unwrap();
        assert_eq!(http["servers"]["srv0"]["listen"], json!([":8443"]));
        assert_eq!(
            http["servers"]["srv0"]["routes"][0]["@id"],
            "nasty-app-demo"
        );
        assert!(http["servers"].get("srv1").is_none());
        assert_eq!(
            http["servers"]["nasty_webui_redirect"]["routes"][0]["handle"][1]["headers"]["Location"],
            json!(["https://{nasty.redirect_host}:8443{http.request.uri}"])
        );
        let snapshot = http.clone();
        configure_http(
            &mut http,
            ListenerPorts {
                https_port: 8443,
                http_port: Some(8080),
            },
        )
        .unwrap();
        assert_eq!(snapshot, http);
    }
    #[test]
    fn disabling_http_and_restoring_default_ports() {
        let mut http = config();
        configure_http(
            &mut http,
            ListenerPorts {
                https_port: 8443,
                http_port: None,
            },
        )
        .unwrap();
        assert_eq!(http["servers"].as_object().unwrap().len(), 1);
        configure_http(
            &mut http,
            ListenerPorts {
                https_port: 443,
                http_port: Some(80),
            },
        )
        .unwrap();
        assert_eq!(
            http["servers"]["nasty_webui_redirect"]["routes"][0]["handle"][1]["headers"]["Location"],
            json!(["https://{nasty.redirect_host}{http.request.uri}"])
        );
    }
    #[test]
    fn invalid_ports_are_rejected() {
        for ports in [
            ListenerPorts {
                https_port: 0,
                http_port: None,
            },
            ListenerPorts {
                https_port: 443,
                http_port: Some(443),
            },
            ListenerPorts {
                https_port: 2019,
                http_port: None,
            },
            ListenerPorts {
                https_port: 8443,
                http_port: Some(2137),
            },
        ] {
            assert!(ports.validate().is_err());
        }
    }
    #[test]
    fn ambiguous_caddy_config_is_not_modified() {
        let mut http = config();
        http["servers"]["other_tls"] = http["servers"]["srv0"].clone();
        let original = http.clone();
        assert!(
            configure_http(
                &mut http,
                ListenerPorts {
                    https_port: 8443,
                    http_port: None
                }
            )
            .is_err()
        );
        assert_eq!(http, original);
    }

    #[test]
    fn tls_bind_addresses_and_unrelated_plaintext_servers_are_preserved() {
        let mut http = config();
        http["servers"]["srv0"]["listen"] = json!(["127.0.0.1:443", "[::1]:443"]);
        let extra = json!({"listen":[":9000"], "routes":[{"handle":[{"handler":"static_response", "body":"unrelated"}]}]});
        http["servers"]["extra"] = extra.clone();
        configure_http(
            &mut http,
            ListenerPorts {
                https_port: 8443,
                http_port: None,
            },
        )
        .unwrap();
        assert_eq!(
            http["servers"]["srv0"]["listen"],
            json!(["127.0.0.1:8443", "[::1]:8443"])
        );
        assert_eq!(http["servers"]["extra"], extra);
    }
}
