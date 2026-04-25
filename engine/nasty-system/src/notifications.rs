//! Notification channels — SMTP, Telegram, Webhook, ntfy.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const CONFIG_PATH: &str = "/var/lib/nasty/notifications.json";

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
        password: String,
        from: String,
        to: String,
        tls: bool,
    },
    Telegram {
        bot_token: String,
        chat_id: String,
    },
    Webhook {
        url: String,
        #[serde(default)]
        headers: std::collections::HashMap<String, String>,
    },
    Ntfy {
        server_url: String,
        topic: String,
        #[serde(default)]
        token: Option<String>,
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
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("serialize: {e}"))?;
        tokio::fs::write(CONFIG_PATH, json)
            .await
            .map_err(|e| format!("write {CONFIG_PATH}: {e}"))
    }
}

// ── Dispatcher ─────────────────────────────────────────────────

/// Send a notification to all enabled channels.
pub async fn send(config: &NotificationConfig, subject: &str, body: &str) {
    for ch in &config.channels {
        if !ch.enabled {
            continue;
        }
        if let Err(e) = send_to_channel(&ch.channel, subject, body).await {
            warn!("Notification to '{}' ({}) failed: {e}", ch.name, ch.id);
        } else {
            info!("Notification sent to '{}' ({})", ch.name, ch.id);
        }
    }
}

/// Test a specific channel by sending a test message.
pub async fn test_channel(channel: &ChannelType) -> Result<String, String> {
    send_to_channel(channel, "NASty Test", "This is a test notification from NASty.").await?;
    Ok("Test notification sent successfully".to_string())
}

async fn send_to_channel(channel: &ChannelType, subject: &str, body: &str) -> Result<(), String> {
    match channel {
        ChannelType::Smtp { host, port, username, password, from, to, tls } => {
            send_smtp(host, *port, username, password, from, to, *tls, subject, body).await
        }
        ChannelType::Telegram { bot_token, chat_id } => {
            send_telegram(bot_token, chat_id, subject, body).await
        }
        ChannelType::Webhook { url, headers } => {
            send_webhook(url, headers, subject, body).await
        }
        ChannelType::Ntfy { server_url, topic, token } => {
            send_ntfy(server_url, topic, token.as_deref(), subject, body).await
        }
    }
}

// ── SMTP ───────────────────────────────────────────────────────

async fn send_smtp(
    host: &str, port: u16, username: &str, password: &str,
    from: &str, to: &str, tls: bool,
    subject: &str, body: &str,
) -> Result<(), String> {
    use lettre::{
        AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
        message::{header::ContentType, Mailbox},
        transport::smtp::authentication::Credentials,
    };

    let from_mbox: Mailbox = from.parse()
        .map_err(|e| format!("invalid from address: {e}"))?;
    let to_mbox: Mailbox = to.parse()
        .map_err(|e| format!("invalid to address: {e}"))?;

    let email = lettre::Message::builder()
        .from(from_mbox)
        .to(to_mbox)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .map_err(|e| format!("build email: {e}"))?;

    let creds = Credentials::new(username.to_string(), password.to_string());

    let transport = if tls {
        AsyncSmtpTransport::<Tokio1Executor>::relay(host)
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
    }
    .map_err(|e| format!("smtp transport: {e}"))?
    .port(port)
    .credentials(creds)
    .build();

    transport.send(email).await
        .map_err(|e| format!("smtp send: {e}"))?;

    Ok(())
}

// ── Telegram ───────────────────────────────────────────────────

async fn send_telegram(bot_token: &str, chat_id: &str, subject: &str, body: &str) -> Result<(), String> {
    let text = format!("*{subject}*\n\n{body}");
    let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");

    let client = reqwest::Client::new();
    let resp = client.post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "Markdown",
        }))
        .send()
        .await
        .map_err(|e| format!("telegram request: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("telegram API error: {body}"));
    }

    Ok(())
}

// ── Webhook ────────────────────────────────────────────────────

async fn send_webhook(
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    subject: &str,
    body: &str,
) -> Result<(), String> {
    let client = reqwest::Client::new();
    let mut req = client.post(url)
        .json(&serde_json::json!({
            "subject": subject,
            "body": body,
            "source": "nasty",
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }));

    for (k, v) in headers {
        req = req.header(k, v);
    }

    let resp = req.send().await
        .map_err(|e| format!("webhook request: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("webhook error {status}: {body}"));
    }

    Ok(())
}

// ── ntfy ───────────────────────────────────────────────────────

async fn send_ntfy(
    server_url: &str, topic: &str,
    token: Option<&str>,
    subject: &str, body: &str,
) -> Result<(), String> {
    let url = format!("{}/{}", server_url.trim_end_matches('/'), topic);
    let client = reqwest::Client::new();
    let mut req = client.post(&url)
        .header("Title", subject)
        .header("Priority", "high")
        .header("Tags", "warning")
        .body(body.to_string());

    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }

    let resp = req.send().await
        .map_err(|e| format!("ntfy request: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("ntfy error {status}: {body}"));
    }

    Ok(())
}
