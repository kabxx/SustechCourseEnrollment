use crate::{
    account::AccountStore,
    error::{AppError, Result},
};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

const SETTINGS_FILE: &str = "settings.json";
pub const DEFAULT_REQUEST_INTERVAL_MS: u64 = 1_000;
pub const MAX_REQUEST_INTERVAL_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    System,
    Disabled,
}

impl ProxyMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "跟随系统代理",
            Self::Disabled => "不使用代理",
        }
    }

    pub fn toggle(&mut self) {
        *self = match self {
            Self::System => Self::Disabled,
            Self::Disabled => Self::System,
        };
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub proxy_mode: ProxyMode,
    pub request_interval_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            proxy_mode: ProxyMode::System,
            request_interval_ms: DEFAULT_REQUEST_INTERVAL_MS,
        }
    }
}

impl Settings {
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path).map_err(|error| {
            AppError::Settings(format!("read settings {}: {error}", path.display()))
        })?;
        let settings: Self = serde_json::from_str(&text).map_err(|error| {
            AppError::Settings(format!("parse settings {}: {error}", path.display()))
        })?;
        settings.validate()?;
        Ok(settings)
    }

    pub fn save(&self) -> Result<()> {
        self.validate()?;
        let path = Self::path()?;
        let parent = path
            .parent()
            .ok_or_else(|| AppError::Settings("settings path has no parent directory".into()))?;
        fs::create_dir_all(parent)?;
        let temp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        fs::write(&temp, bytes).map_err(|error| {
            AppError::Settings(format!("write settings {}: {error}", temp.display()))
        })?;
        if path.exists() {
            fs::remove_file(&path)?;
        }
        fs::rename(&temp, &path).map_err(|error| {
            AppError::Settings(format!("replace settings {}: {error}", path.display()))
        })?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.request_interval_ms == 0 || self.request_interval_ms > MAX_REQUEST_INTERVAL_MS {
            return Err(AppError::Settings(format!(
                "请求间隔必须在 1 到 {MAX_REQUEST_INTERVAL_MS} 毫秒之间"
            )));
        }
        Ok(())
    }

    pub fn path() -> Result<PathBuf> {
        Ok(AccountStore::data_dir()?.join(SETTINGS_FILE))
    }
}
