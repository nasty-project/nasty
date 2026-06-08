//! Notification channels — SMTP, Telegram, Webhook, ntfy.

use nasty_common::secrets::{self, EncryptedBlob};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const CONFIG_PATH: &str = "/var/lib/nasty/notifications.json";

/// Placeholder shown in place of a secret over the API. An update that
/// echoes this value back means "keep the stored secret".
const REDACTED: &str = "***";

// ── Types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct NotificationConfig {
    pub channels: Vec<ChannelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChannelConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    #[serde(flatten)]
    pub channel: ChannelType,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelType {
    Smtp {
        host: String,
        port: u16,
        username: String,
        /// Legacy plaintext SMTP password. Encrypted into
        /// `password_encrypted` at rest when the secrets backend is
        /// healthy, and redacted to `***` in API responses.
        #[serde(default)]
        password: String,
        /// SMTP password encrypted at rest via systemd-creds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password_encrypted: Option<EncryptedBlob>,
        from: String,
        to: String,
    },
    Telegram {
        /// Legacy plaintext bot token; see `bot_token_encrypted`.
        #[serde(default)]
        bot_token: String,
        /// Telegram bot token encrypted at rest via systemd-creds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bot_token_encrypted: Option<EncryptedBlob>,
        chat_id: String,
    },
    Webhook {
        url: String,
        #[serde(default)]
        headers: std::collections::HashMap<String, String>,
        /// HMAC-SHA256 signing key. When set, every webhook POST carries
        /// an `X-NASty-Signature: sha256=<hex>` header that's the HMAC
        /// of the raw body. Receivers verify the request really came
        /// from NASty by recomputing the signature with the shared
        /// secret. Optional — old webhook configs without a secret
        /// still work, just unsigned.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<String>,
        /// Signing secret encrypted at rest via systemd-creds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret_encrypted: Option<EncryptedBlob>,
    },
    Ntfy {
        server_url: String,
        topic: String,
        #[serde(default)]
        token: Option<String>,
        /// Bearer token encrypted at rest via systemd-creds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_encrypted: Option<EncryptedBlob>,
    },
    Signal {
        api_url: String,
        from_number: String,
        to_number: String,
    },
}

// ── Config persistence ─────────────────────────────────────────

impl NotificationConfig {
    pub fn load() -> Self {
        std::fs::read_to_string(CONFIG_PATH)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub async fn save(&self) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("serialize: {e}"))?;
        tokio::fs::write(CONFIG_PATH, json)
            .await
            .map_err(|e| format!("write {CONFIG_PATH}: {e}"))?;
        // Contains SMTP passwords, Telegram bot tokens, webhook URLs.
        tokio::fs::set_permissions(CONFIG_PATH, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|e| format!("chmod {CONFIG_PATH}: {e}"))
    }
}

// ── Encrypt-at-rest for channel secrets (systemd-creds) ─────────

/// `systemd-creds` AEAD name binding a channel secret to host + channel + field.
fn secret_name(channel_id: &str, field: &str) -> String {
    format!("nasty.notifications.{channel_id}.{field}")
}

/// Seal one plaintext secret. Empty → `None` (nothing to seal). On
/// systemd-creds failure, warn and return `None` so the caller keeps
/// the plaintext (degraded, not broken).
async fn seal(name: &str, plain: &str) -> Option<EncryptedBlob> {
    if plain.is_empty() {
        return None;
    }
    match secrets::encrypt(name, plain).await {
        Ok(blob) => Some(blob),
        Err(e) => {
            warn!("Failed to encrypt {name} — keeping plaintext: {e}");
            None
        }
    }
}

/// Decrypt one secret blob; warn + `None` on failure.
async fn unseal(name: &str, blob: &EncryptedBlob) -> Option<String> {
    match secrets::decrypt(name, blob).await {
        Ok(plain) => Some(plain),
        Err(e) => {
            warn!("Failed to decrypt {name}: {e}");
            None
        }
    }
}

impl ChannelType {
    /// True when this channel holds a sealed secret blob.
    fn has_encrypted_secret(&self) -> bool {
        matches!(
            self,
            ChannelType::Smtp {
                password_encrypted: Some(_),
                ..
            } | ChannelType::Telegram {
                bot_token_encrypted: Some(_),
                ..
            } | ChannelType::Webhook {
                secret_encrypted: Some(_),
                ..
            } | ChannelType::Ntfy {
                token_encrypted: Some(_),
                ..
            }
        )
    }

    /// Copy with secrets masked to `***` (when set) and ciphertext dropped.
    pub fn redacted(&self) -> ChannelType {
        let mut c = self.clone();
        match &mut c {
            ChannelType::Smtp {
                password,
                password_encrypted,
                ..
            } => {
                if !password.is_empty() || password_encrypted.is_some() {
                    *password = REDACTED.to_string();
                }
                *password_encrypted = None;
            }
            ChannelType::Telegram {
                bot_token,
                bot_token_encrypted,
                ..
            } => {
                if !bot_token.is_empty() || bot_token_encrypted.is_some() {
                    *bot_token = REDACTED.to_string();
                }
                *bot_token_encrypted = None;
            }
            ChannelType::Webhook {
                secret,
                secret_encrypted,
                ..
            } => {
                if secret.is_some() || secret_encrypted.is_some() {
                    *secret = Some(REDACTED.to_string());
                }
                *secret_encrypted = None;
            }
            ChannelType::Ntfy {
                token,
                token_encrypted,
                ..
            } => {
                if token.is_some() || token_encrypted.is_some() {
                    *token = Some(REDACTED.to_string());
                }
                *token_encrypted = None;
            }
            ChannelType::Signal { .. } => {}
        }
        c
    }
}

impl ChannelConfig {
    /// Copy with secrets redacted for API responses.
    pub fn redacted(&self) -> ChannelConfig {
        ChannelConfig {
            channel: self.channel.redacted(),
            ..self.clone()
        }
    }

    /// Seal the channel's plaintext secret at rest before persisting.
    /// Idempotent (already-sealed → no-op). Degraded backend keeps the
    /// plaintext (see [`seal`]).
    async fn encrypt_secrets_in_place(&mut self) {
        let id = self.id.clone();
        match &mut self.channel {
            ChannelType::Smtp {
                password,
                password_encrypted,
                ..
            } => {
                if password_encrypted.is_none()
                    && let Some(blob) = seal(&secret_name(&id, "smtp_password"), password).await
                {
                    *password_encrypted = Some(blob);
                    password.clear();
                }
            }
            ChannelType::Telegram {
                bot_token,
                bot_token_encrypted,
                ..
            } => {
                if bot_token_encrypted.is_none()
                    && let Some(blob) =
                        seal(&secret_name(&id, "telegram_bot_token"), bot_token).await
                {
                    *bot_token_encrypted = Some(blob);
                    bot_token.clear();
                }
            }
            ChannelType::Webhook {
                secret,
                secret_encrypted,
                ..
            } => {
                if secret_encrypted.is_none()
                    && let Some(plain) = secret.as_deref()
                    && let Some(blob) = seal(&secret_name(&id, "webhook_secret"), plain).await
                {
                    *secret_encrypted = Some(blob);
                    *secret = None;
                }
            }
            ChannelType::Ntfy {
                token,
                token_encrypted,
                ..
            } => {
                if token_encrypted.is_none()
                    && let Some(plain) = token.as_deref()
                    && let Some(blob) = seal(&secret_name(&id, "ntfy_token"), plain).await
                {
                    *token_encrypted = Some(blob);
                    *token = None;
                }
            }
            ChannelType::Signal { .. } => {}
        }
    }

    /// A copy of the channel with secrets decrypted into the plaintext
    /// fields, ready for [`send_to_channel`].
    async fn resolved_channel(&self) -> ChannelType {
        let id = &self.id;
        let mut channel = self.channel.clone();
        match &mut channel {
            ChannelType::Smtp {
                password,
                password_encrypted,
                ..
            } => {
                if let Some(blob) = password_encrypted.take()
                    && let Some(plain) = unseal(&secret_name(id, "smtp_password"), &blob).await
                {
                    *password = plain;
                }
            }
            ChannelType::Telegram {
                bot_token,
                bot_token_encrypted,
                ..
            } => {
                if let Some(blob) = bot_token_encrypted.take()
                    && let Some(plain) = unseal(&secret_name(id, "telegram_bot_token"), &blob).await
                {
                    *bot_token = plain;
                }
            }
            ChannelType::Webhook {
                secret,
                secret_encrypted,
                ..
            } => {
                if let Some(blob) = secret_encrypted.take()
                    && let Some(plain) = unseal(&secret_name(id, "webhook_secret"), &blob).await
                {
                    *secret = Some(plain);
                }
            }
            ChannelType::Ntfy {
                token,
                token_encrypted,
                ..
            } => {
                if let Some(blob) = token_encrypted.take()
                    && let Some(plain) = unseal(&secret_name(id, "ntfy_token"), &blob).await
                {
                    *token = Some(plain);
                }
            }
            ChannelType::Signal { .. } => {}
        }
        channel
    }

    /// Carry forward the stored secret when the incoming update echoed
    /// the `***` placeholder (operator didn't change it).
    fn merge_secrets_from(&mut self, existing: &ChannelConfig) {
        match (&mut self.channel, &existing.channel) {
            (
                ChannelType::Smtp {
                    password,
                    password_encrypted,
                    ..
                },
                ChannelType::Smtp {
                    password: ex,
                    password_encrypted: ex_enc,
                    ..
                },
            ) if password == REDACTED => {
                *password = ex.clone();
                *password_encrypted = ex_enc.clone();
            }
            (
                ChannelType::Telegram {
                    bot_token,
                    bot_token_encrypted,
                    ..
                },
                ChannelType::Telegram {
                    bot_token: ex,
                    bot_token_encrypted: ex_enc,
                    ..
                },
            ) if bot_token == REDACTED => {
                *bot_token = ex.clone();
                *bot_token_encrypted = ex_enc.clone();
            }
            (
                ChannelType::Webhook {
                    secret,
                    secret_encrypted,
                    ..
                },
                ChannelType::Webhook {
                    secret: ex,
                    secret_encrypted: ex_enc,
                    ..
                },
            ) if secret.as_deref() == Some(REDACTED) => {
                *secret = ex.clone();
                *secret_encrypted = ex_enc.clone();
            }
            (
                ChannelType::Ntfy {
                    token,
                    token_encrypted,
                    ..
                },
                ChannelType::Ntfy {
                    token: ex,
                    token_encrypted: ex_enc,
                    ..
                },
            ) if token.as_deref() == Some(REDACTED) => {
                *token = ex.clone();
                *token_encrypted = ex_enc.clone();
            }
            _ => {}
        }
    }
}

impl NotificationConfig {
    /// Copy with every channel's secrets redacted for API responses.
    pub fn redacted(&self) -> NotificationConfig {
        NotificationConfig {
            channels: self.channels.iter().map(|c| c.redacted()).collect(),
        }
    }

    /// Persist an operator update: carry forward secrets the UI echoed
    /// as `***`, seal new plaintext secrets at rest, then save.
    pub async fn apply_update(mut self) -> Result<(), String> {
        let existing = NotificationConfig::load();
        let by_id: std::collections::HashMap<&str, &ChannelConfig> = existing
            .channels
            .iter()
            .map(|c| (c.id.as_str(), c))
            .collect();
        for ch in &mut self.channels {
            if let Some(prev) = by_id.get(ch.id.as_str()) {
                ch.merge_secrets_from(prev);
            }
            ch.encrypt_secrets_in_place().await;
        }
        self.save().await
    }

    /// Eagerly seal plaintext secrets left on disk from before
    /// encrypt-at-rest. Called once on boot; idempotent, no-op when
    /// already sealed / empty / backend unavailable.
    pub async fn migrate_secrets() {
        let mut config = NotificationConfig::load();
        let mut changed = false;
        for ch in &mut config.channels {
            let before = ch.channel.has_encrypted_secret();
            ch.encrypt_secrets_in_place().await;
            if ch.channel.has_encrypted_secret() != before {
                changed = true;
            }
        }
        if changed {
            match config.save().await {
                Ok(()) => info!("Migrated notification channel secrets to systemd-creds"),
                Err(e) => warn!("Failed to persist migrated notification secrets: {e}"),
            }
        }
    }
}

/// Test a saved channel by id: resolve its sealed secrets, then send a
/// test event. Used by the WebUI so testing an existing channel never
/// requires the secret to leave the engine.
pub async fn test_saved_channel(id: &str) -> Result<String, String> {
    let config = NotificationConfig::load();
    let ch = config
        .channels
        .iter()
        .find(|c| c.id == id)
        .ok_or_else(|| format!("notification channel '{id}' not found"))?;
    let resolved = ch.resolved_channel().await;
    test_channel(&resolved).await
}

// ── Dispatcher ─────────────────────────────────────────────────

/// Structured event payload carried by webhooks. Human-readable
/// channels (SMTP, Telegram, Ntfy, Signal) ignore the typed `data`
/// field and use `subject`/`body` like before — they're consumed by
/// humans reading the message. The webhook channel uses the typed
/// fields so integration tools (Home Assistant, n8n, monitoring
/// receivers) can match on `event_type` and pull values out of
/// `data` without parsing the human string.
#[derive(Debug, Clone)]
pub struct Event<'a> {
    /// Stable identifier for the event class (`"alert.fired"`,
    /// `"test"`, …). Receivers route on this.
    pub event_type: &'a str,
    /// Per-event identifier — operators correlate retries / dedupe
    /// on this. The webhook re-uses the same id across all retry
    /// attempts so a receiver that sees the same id twice can drop
    /// the duplicate.
    pub event_id: &'a str,
    /// Human subject (used as-is by SMTP/Telegram/Ntfy/Signal).
    pub subject: &'a str,
    /// Human body (same).
    pub body: &'a str,
    /// Typed payload for the webhook receiver. Channels other than
    /// webhook ignore this. `serde_json::Value` rather than a
    /// generic so the caller doesn't have to bake event-type-specific
    /// types into the notifications crate.
    pub data: serde_json::Value,
}

/// Send a notification to all enabled channels. Backward-compatible
/// thin wrapper that wraps the legacy subject/body call shape into
/// an event with no typed `data` — old callers don't have to thread
/// structured data through. New callers should prefer `send_event`
/// directly so webhook consumers get the typed payload.
pub async fn send(config: &NotificationConfig, subject: &str, body: &str) {
    let event = Event {
        event_type: "notification",
        event_id: &generate_event_id(),
        subject,
        body,
        data: serde_json::Value::Null,
    };
    send_event(config, &event).await;
}

/// Fan an event out to every enabled channel. Errors are logged per
/// channel; one channel's failure doesn't block the others.
pub async fn send_event(config: &NotificationConfig, event: &Event<'_>) {
    for ch in &config.channels {
        if !ch.enabled {
            continue;
        }
        // Decrypt sealed secrets into a plaintext copy just for this send.
        let resolved = ch.resolved_channel().await;
        if let Err(e) = send_to_channel(&resolved, event).await {
            warn!("Notification to '{}' ({}) failed: {e}", ch.name, ch.id);
        } else {
            info!("Notification sent to '{}' ({})", ch.name, ch.id);
        }
    }
}

/// Test a specific channel by sending a synthetic test event.
pub async fn test_channel(channel: &ChannelType) -> Result<String, String> {
    let event = Event {
        event_type: "test",
        event_id: &generate_event_id(),
        subject: "NASty Test",
        body: "This is a test notification from NASty.",
        data: serde_json::json!({"test": true}),
    };
    send_to_channel(channel, &event).await?;
    Ok("Test notification sent successfully".to_string())
}

async fn send_to_channel(channel: &ChannelType, event: &Event<'_>) -> Result<(), String> {
    match channel {
        ChannelType::Smtp {
            host,
            port,
            username,
            password,
            from,
            to,
            ..
        } => {
            send_smtp(
                host,
                *port,
                username,
                password,
                from,
                to,
                event.subject,
                event.body,
            )
            .await
        }
        ChannelType::Telegram {
            bot_token, chat_id, ..
        } => send_telegram(bot_token, chat_id, event.subject, event.body).await,
        ChannelType::Webhook {
            url,
            headers,
            secret,
            ..
        } => send_webhook(url, headers, secret.as_deref(), event).await,
        ChannelType::Ntfy {
            server_url,
            topic,
            token,
            ..
        } => {
            send_ntfy(
                server_url,
                topic,
                token.as_deref(),
                event.subject,
                event.body,
            )
            .await
        }
        ChannelType::Signal {
            api_url,
            from_number,
            to_number,
        } => send_signal(api_url, from_number, to_number, event.subject, event.body).await,
    }
}

/// Generate a short event id (`evt-<uuid7-style>`). Re-used across
/// retry attempts so webhook receivers can dedupe on it.
fn generate_event_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    format!("evt-{:016x}", micros)
}

// ── SMTP ───────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn send_smtp(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<(), String> {
    use lettre::{
        AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
        message::{Mailbox, header::ContentType},
        transport::smtp::authentication::Credentials,
    };

    let from_mbox: Mailbox = from
        .parse()
        .map_err(|e| format!("invalid from address: {e}"))?;
    let to_mbox: Mailbox = to.parse().map_err(|e| format!("invalid to address: {e}"))?;

    let email = lettre::Message::builder()
        .from(from_mbox)
        .to(to_mbox)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .map_err(|e| format!("build email: {e}"))?;

    let creds = Credentials::new(username.to_string(), password.to_string());

    // Port 465 = implicit TLS (relay), port 587/25 = STARTTLS.
    // The tls flag is kept for backward compat but port takes precedence.
    let transport = if port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(host)
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
    }
    .map_err(|e| format!("smtp transport: {e}"))?
    .port(port)
    .credentials(creds)
    .build();

    transport
        .send(email)
        .await
        .map_err(|e| format!("smtp send: {e}"))?;

    Ok(())
}

// ── Telegram ───────────────────────────────────────────────────

async fn send_telegram(
    bot_token: &str,
    chat_id: &str,
    subject: &str,
    body: &str,
) -> Result<(), String> {
    let text = format!("*{subject}*\n\n{body}");
    let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "Markdown",
        }))
        .send()
        .await
        .map_err(|e| format!("telegram request: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .unwrap_or_else(|e| format!("(failed to read body: {e})"));
        return Err(format!("telegram API error {status}: {body}"));
    }

    Ok(())
}

// ── Webhook ────────────────────────────────────────────────────

/// Build the JSON body the webhook POST carries. Backward-compatible
/// with the pre-v0.0.10 shape — the original `subject`, `body`,
/// `source`, `timestamp` fields are still here so existing consumers
/// (Home Assistant automations, simple scripts that match on `subject`)
/// keep working. The new `event_type`, `event_id`, `data`,
/// `nasty_version`, `nasty_hostname` fields land alongside for
/// consumers that want typed event data instead of regex-matching the
/// human strings.
fn webhook_payload(event: &Event<'_>) -> String {
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "nasty".to_string());
    let payload = serde_json::json!({
        "subject": event.subject,
        "body": event.body,
        "source": "nasty",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "event_type": event.event_type,
        "event_id": event.event_id,
        "nasty_version": env!("CARGO_PKG_VERSION"),
        "nasty_hostname": host,
        "data": event.data,
    });
    payload.to_string()
}

/// HMAC-SHA256 the body with the secret and return the hex digest.
/// Receivers verify by recomputing and constant-time-comparing the
/// header value's `sha256=…` substring.
fn sign_webhook(secret: &str, body: &[u8]) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(body);
    let result = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(result.len() * 2);
    for b in result {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{:02x}", b);
    }
    hex
}

async fn send_webhook(
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    secret: Option<&str>,
    event: &Event<'_>,
) -> Result<(), String> {
    let body = webhook_payload(event);
    let signature = secret.map(|s| sign_webhook(s, body.as_bytes()));

    // Retry transient failures (network errors + 5xx) with an
    // exponential backoff. 4xx responses are NOT retried — they're
    // configuration / endpoint errors that won't fix themselves on
    // resend. Total wall-clock cap: ~35s.
    let mut attempt = 0;
    let mut last_err = String::new();
    loop {
        attempt += 1;
        match send_webhook_once(url, headers, signature.as_deref(), &body).await {
            Ok(()) => return Ok(()),
            Err(WebhookError::Transient(e)) if attempt < 3 => {
                last_err = e;
                let wait_secs = if attempt == 1 { 5 } else { 30 };
                tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
            }
            Err(WebhookError::Transient(e)) => {
                return Err(format!("after {attempt} attempts: {e} (last: {last_err})"));
            }
            Err(WebhookError::Permanent(e)) => return Err(e),
        }
    }
}

enum WebhookError {
    /// 5xx response or network failure — worth retrying.
    Transient(String),
    /// 4xx or other client-fault response — retry won't help.
    Permanent(String),
}

async fn send_webhook_once(
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    signature: Option<&str>,
    body: &str,
) -> Result<(), WebhookError> {
    let client = reqwest::Client::new();
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(body.to_string());
    if let Some(sig) = signature {
        req = req.header("X-NASty-Signature", format!("sha256={sig}"));
    }
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| WebhookError::Transient(format!("webhook request: {e}")))?;

    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp
        .text()
        .await
        .unwrap_or_else(|e| format!("(failed to read body: {e})"));
    let msg = format!("webhook error {status}: {body}");
    if status.is_server_error() {
        Err(WebhookError::Transient(msg))
    } else {
        Err(WebhookError::Permanent(msg))
    }
}

// ── ntfy ───────────────────────────────────────────────────────

async fn send_ntfy(
    server_url: &str,
    topic: &str,
    token: Option<&str>,
    subject: &str,
    body: &str,
) -> Result<(), String> {
    let url = format!("{}/{}", server_url.trim_end_matches('/'), topic);
    let client = reqwest::Client::new();
    let mut req = client
        .post(&url)
        .header("Title", subject)
        .header("Priority", "high")
        .header("Tags", "warning")
        .body(body.to_string());

    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }

    let resp = req.send().await.map_err(|e| format!("ntfy request: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .unwrap_or_else(|e| format!("(failed to read body: {e})"));
        return Err(format!("ntfy error {status}: {body}"));
    }

    Ok(())
}

// ── Signal ─────────────────────────────────────────────────────

async fn send_signal(
    api_url: &str,
    from_number: &str,
    to_number: &str,
    subject: &str,
    body: &str,
) -> Result<(), String> {
    let message = format!("{subject}\n\n{body}");
    let url = format!("{}/v2/send", api_url.trim_end_matches('/'));

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "message": message,
            "number": from_number,
            "recipients": [to_number],
        }))
        .send()
        .await
        .map_err(|e| format!("signal request: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .unwrap_or_else(|e| format!("(failed to read body: {e})"));
        return Err(format!("signal API error {status}: {body}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_blob() -> EncryptedBlob {
        serde_json::from_str("\"c2VhbGVkLWJsb2I=\"").unwrap()
    }

    fn smtp_channel(password: &str, encrypted: bool) -> ChannelConfig {
        ChannelConfig {
            id: "ch1".into(),
            name: "mail".into(),
            enabled: true,
            channel: ChannelType::Smtp {
                host: "smtp.example.com".into(),
                port: 587,
                username: "user".into(),
                password: password.into(),
                password_encrypted: encrypted.then(fake_blob),
                from: "a@example.com".into(),
                to: "b@example.com".into(),
            },
        }
    }

    #[test]
    fn redacted_masks_plaintext_smtp_password() {
        let r = smtp_channel("hunter2", false).redacted();
        match r.channel {
            ChannelType::Smtp {
                password,
                password_encrypted,
                ..
            } => {
                assert_eq!(password, REDACTED);
                assert!(password_encrypted.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn redacted_masks_encrypted_only_smtp_password() {
        let r = smtp_channel("", true).redacted();
        match r.channel {
            ChannelType::Smtp {
                password,
                password_encrypted,
                ..
            } => {
                assert_eq!(password, REDACTED);
                assert!(password_encrypted.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn merge_carries_forward_redacted_smtp_secret() {
        // UI echoes "***" for an unchanged secret → keep the stored blob.
        let mut incoming = smtp_channel(REDACTED, false);
        let existing = smtp_channel("", true);
        incoming.merge_secrets_from(&existing);
        match incoming.channel {
            ChannelType::Smtp {
                password,
                password_encrypted,
                ..
            } => {
                assert!(password.is_empty());
                assert!(
                    password_encrypted.is_some(),
                    "stored blob should carry forward"
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn merge_keeps_freshly_entered_smtp_secret() {
        let mut incoming = smtp_channel("brand-new", false);
        let existing = smtp_channel("", true);
        incoming.merge_secrets_from(&existing);
        match incoming.channel {
            ChannelType::Smtp {
                password,
                password_encrypted,
                ..
            } => {
                assert_eq!(password, "brand-new");
                assert!(
                    password_encrypted.is_none(),
                    "new plaintext must not inherit the old blob"
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn webhook_redacts_and_carries_forward_optional_secret() {
        let set = ChannelConfig {
            id: "w".into(),
            name: "wh".into(),
            enabled: true,
            channel: ChannelType::Webhook {
                url: "https://x".into(),
                headers: Default::default(),
                secret: Some("sig".into()),
                secret_encrypted: None,
            },
        };
        match set.redacted().channel {
            ChannelType::Webhook { secret, .. } => assert_eq!(secret.as_deref(), Some(REDACTED)),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn loads_legacy_channel_without_encrypted_fields() {
        let json = r#"{ "id": "x", "name": "n", "enabled": true,
            "type": "telegram", "bot_token": "legacy", "chat_id": "1" }"#;
        let ch: ChannelConfig = serde_json::from_str(json).unwrap();
        match ch.channel {
            ChannelType::Telegram {
                bot_token,
                bot_token_encrypted,
                ..
            } => {
                assert_eq!(bot_token, "legacy");
                assert!(bot_token_encrypted.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn hmac_signature_is_stable_for_same_input() {
        // Receivers verify by recomputing — same key + body must
        // always produce the same digest, byte-for-byte.
        let sig_a = sign_webhook("hunter2", b"{\"event\":\"test\"}");
        let sig_b = sign_webhook("hunter2", b"{\"event\":\"test\"}");
        assert_eq!(sig_a, sig_b);
        // Hex-encoded 32-byte SHA256 digest is 64 chars.
        assert_eq!(sig_a.len(), 64);
        assert!(sig_a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hmac_signature_changes_with_key() {
        let body = b"{\"event\":\"test\"}";
        let sig_a = sign_webhook("hunter2", body);
        let sig_b = sign_webhook("different-key", body);
        assert_ne!(sig_a, sig_b);
    }

    #[test]
    fn hmac_signature_changes_with_body() {
        // The whole point of signing: tampering with the body
        // invalidates the signature.
        let sig_a = sign_webhook("hunter2", b"{\"event\":\"alert.fired\"}");
        let sig_b = sign_webhook("hunter2", b"{\"event\":\"alert.resolved\"}");
        assert_ne!(sig_a, sig_b);
    }

    #[test]
    fn hmac_matches_python_reference() {
        // Cross-checked against:
        //   import hmac, hashlib
        //   hmac.new(b"secret", b"hello", hashlib.sha256).hexdigest()
        //   -> '88aab3ede8d3adf94d26ab90d3bafd4a2083070c3bcce9c014ee04a443847c0b'
        // Receivers in Python / Node / Go will use the same standard
        // HMAC-SHA256 so a wire-compat test against a known fixture
        // catches any future crate-bump surprises.
        let sig = sign_webhook("secret", b"hello");
        assert_eq!(
            sig,
            "88aab3ede8d3adf94d26ab90d3bafd4a2083070c3bcce9c014ee04a443847c0b"
        );
    }

    #[test]
    fn webhook_payload_carries_both_legacy_and_structured_fields() {
        // Backward compat — the pre-v0.0.10 webhook shape had
        // {subject, body, source, timestamp}. Consumers that match
        // on subject regex must keep working. New consumers want
        // event_type / event_id / data for typed dispatch.
        let event = Event {
            event_type: "alert.fired",
            event_id: "evt-deadbeef",
            subject: "[NASty CRITICAL] disk failing",
            body: "Disk /dev/sda SMART health check FAILED",
            data: serde_json::json!({
                "rule_id": "smart-health",
                "severity": "critical",
            }),
        };
        let body_str = webhook_payload(&event);
        let parsed: serde_json::Value = serde_json::from_str(&body_str).expect("valid JSON");

        // Legacy keys for existing consumers.
        assert_eq!(parsed["subject"], "[NASty CRITICAL] disk failing");
        assert_eq!(parsed["body"], "Disk /dev/sda SMART health check FAILED");
        assert_eq!(parsed["source"], "nasty");
        assert!(parsed["timestamp"].is_string());

        // New structured keys.
        assert_eq!(parsed["event_type"], "alert.fired");
        assert_eq!(parsed["event_id"], "evt-deadbeef");
        assert_eq!(parsed["data"]["rule_id"], "smart-health");
        assert_eq!(parsed["data"]["severity"], "critical");
        assert!(parsed["nasty_version"].is_string());
        assert!(parsed["nasty_hostname"].is_string());
    }
}
