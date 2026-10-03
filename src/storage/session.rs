use crate::utils::{current_time, random_uuid};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::{Duration, UNIX_EPOCH}, collections::HashMap};

#[derive(Debug, Clone)]
pub struct RemotePaths {
    pub workdir: PathBuf,
    pub logdir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Session {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub tasks: HashMap<String, String>,
    #[serde(skip)]
    pub id: String,
}

impl Session {
    /// Create new session struct
    pub fn new(server_id: &String, name: Option<&String>) -> Self {
        let timestamp = current_time();
        let mut session = Self {
            name: name.cloned().unwrap_or("".to_string()),
            server: server_id.clone(),
            created_at: timestamp,
            tasks: HashMap::new(),
            id: random_uuid(),
        };
        if session.name.is_empty() {
            session.name = session.created_at_str()
        }
        session
    }

    /// Get session created_at in human-readable format
    pub fn created_at_str(&self) -> String {
        let system_time = UNIX_EPOCH + Duration::from_secs(self.created_at);
        let datetime: DateTime<Utc> = system_time.into();
        datetime.format("%Y-%m-%d %H:%M:%S").to_string()
    }
}
