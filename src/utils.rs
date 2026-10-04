use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Serialize, de::DeserializeOwned};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{collections::HashMap, fs, path::PathBuf};
use uuid::Uuid;

/// Generate random uuid as string
pub fn random_uuid() -> String {
    Uuid::new_v4().to_string()
}

/// Check if hashmap is empty
pub fn is_empty_map<K, V>(map: &HashMap<K, V>) -> bool {
    map.is_empty()
}

/// Loading configurations from toml with shellexpand
pub fn load_toml<T: DeserializeOwned>(path: &PathBuf) -> Result<T> {
    let config_str = fs::read_to_string(path)?;
    let config: T = toml::from_str(&config_str)?;
    Ok(config)
}

/// Saving configurations to toml with shellexpand
pub fn save_toml<T: Serialize>(path: &PathBuf, config: &T) -> Result<()> {
    let parent_path = path.parent().context("Invalid path")?;
    fs::create_dir_all(parent_path)?;
    let default_str = toml::to_string_pretty(config)?;
    fs::write(&path, &default_str)?;
    Ok(())
}

/// Creates new config using template
pub fn create_from_template(
    template: &str,
    data: HashMap<&str, &str>,
    target: &PathBuf,
) -> Result<()> {
    let mut content = template.to_string();
    for (k, v) in data.iter() {
        content = content.replace(&format!("{{{}}}", k), v);
    }
    let parent_path = target.parent().context("Invalid path")?;
    fs::create_dir_all(parent_path)?;
    fs::write(&target, &content)?;
    Ok(())
}

/// Get current timestamp
pub fn current_time() -> u64 {
    let now = SystemTime::now();
    let unix_timestamp = now.duration_since(UNIX_EPOCH).expect("Time went backwards");
    let timestamp_secs = unix_timestamp.as_secs();
    timestamp_secs
}

/// Get time in human-readable format
pub fn format_timestamp(timestamp: u64) -> String {
    let system_time = UNIX_EPOCH + Duration::from_secs(timestamp);
    let datetime: DateTime<Utc> = system_time.into();
    datetime.format("%Y-%m-%d %H:%M:%S").to_string()
}
