use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

const DEFAULT_CHATS_DIR: &str = "~/.may/chats";

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub config_path: PathBuf,
    pub chats_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppConfigFile {
    chats_dir: String,
}

impl AppConfig {
    pub fn load_or_create() -> Result<Self> {
        let config_path = may_home()?.join("config.toml");
        if !config_path.exists() {
            let default = AppConfigFile {
                chats_dir: DEFAULT_CHATS_DIR.to_string(),
            };
            write_config_file(&config_path, &default)?;
        }

        let raw = fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let parsed: AppConfigFile = toml::from_str(&raw)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;
        let chats_dir = expand_tilde(&parsed.chats_dir)?;
        fs::create_dir_all(&chats_dir)
            .with_context(|| format!("failed to create {}", chats_dir.display()))?;

        Ok(Self {
            config_path,
            chats_dir,
        })
    }
}

fn write_config_file(path: &PathBuf, config: &AppConfigFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let data = toml::to_string_pretty(config).context("failed to serialize config")?;
    fs::write(path, data).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn may_home() -> Result<PathBuf> {
    if let Ok(value) = std::env::var("MAY_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine the home directory"))?;
    Ok(home.join(".may"))
}

fn expand_tilde(path: &str) -> Result<PathBuf> {
    if path == "~" {
        return dirs::home_dir().ok_or_else(|| anyhow!("could not determine the home directory"));
    }

    if let Some(remainder) = path.strip_prefix("~/") {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine the home directory"))?;
        return Ok(home.join(remainder));
    }

    Ok(PathBuf::from(path))
}
