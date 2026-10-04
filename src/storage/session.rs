use crate::utils::{current_time, format_timestamp, random_uuid};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};

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
        format_timestamp(self.created_at)
    }
}
