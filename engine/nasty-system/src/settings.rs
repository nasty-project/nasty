use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

const STATE_PATH: &str = "/var/lib/nasty/settings.json";
const STATE_DIR: &str = "/var/lib/nasty";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub smart_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            smart_enabled: false,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SettingsUpdate {
    #[serde(default)]
    pub smart_enabled: Option<bool>,
}

pub struct SettingsService {
    state: Arc<RwLock<Settings>>,
}

impl SettingsService {
    pub async fn new() -> Self {
        let settings = load().await;
        Self {
            state: Arc::new(RwLock::new(settings)),
        }
    }

    pub async fn get(&self) -> Settings {
        self.state.read().await.clone()
    }

    pub async fn update(&self, update: SettingsUpdate) -> Result<Settings, String> {
        let mut settings = self.state.write().await;
        if let Some(v) = update.smart_enabled {
            settings.smart_enabled = v;
        }
        save(&settings).await.map_err(|e| e.to_string())?;
        Ok(settings.clone())
    }
}

async fn load() -> Settings {
    match tokio::fs::read_to_string(STATE_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

async fn save(settings: &Settings) -> Result<(), std::io::Error> {
    tokio::fs::create_dir_all(STATE_DIR).await?;
    let json = serde_json::to_string_pretty(settings).unwrap();
    tokio::fs::write(STATE_PATH, json).await?;
    Ok(())
}
