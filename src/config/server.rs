use crate::utils::{is_empty_map, random_uuid};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Server init arguments
pub enum HostArgs<'a> {
    FromString {
        string: &'a str,
    },
    FromData {
        user: &'a str,
        host: &'a str,
        port: &'a str,
    },
}

/// Remote server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub identity: Option<String>,
    #[serde(default, skip_serializing_if = "is_empty_map")]
    pub env: HashMap<String, String>,
    #[serde(skip)]
    pub id: String,
}

impl Server {
    /// Create new server
    pub fn new(host: HostArgs, identity: Option<String>) -> Result<Self> {
        let (user, host, port) = match host {
            HostArgs::FromData { user, host, port } => (user, host, port),
            HostArgs::FromString { string } => parse_host_string(string)?,
        };
        let data = Self {
            id: random_uuid(),
            user: user.to_string(),
            host: host.to_string(),
            port: port.parse::<u16>().unwrap_or(22),
            identity: identity,
            env: HashMap::default(),
            aliases: Vec::from([format!("{}@{}:{}", user, host, port)]),
        };
        Ok(data)
    }
}

/// Parse host string (ssh command, etc.), return user, host port
fn parse_host_string(raw: &str) -> Result<(&str, &str, &str)> {
    let err = || anyhow::anyhow!("Invalid input");
    // Parse complex input
    let mut user: &str = "root";
    let mut port: &str = "22";
    // Extract user
    let mut host = if raw.contains("@") {
        let parts: Vec<&str> = raw.split("@").collect();
        let user_part = *parts.get(0).ok_or_else(err)?;
        let host_part = *parts.get(1).ok_or_else(err)?;
        user = if user_part.contains(" ") {
            user_part.split_whitespace().next_back().unwrap().trim()
        } else {
            user_part.trim()
        };
        host_part.trim()
    } else {
        raw.trim()
    };
    // Extract port
    if raw.contains("-p") {
        let parts: Vec<&str> = host.split("-p").collect();
        let port_part = *parts.get(1).ok_or_else(err)?;
        let host_part = *parts.get(0).ok_or_else(err)?;
        port = port_part.trim();
        host = host_part.trim();
    }
    if raw.contains(":") {
        let parts: Vec<&str> = host.split(":").collect();
        let port_part = *parts.get(1).ok_or_else(err)?;
        let host_part = *parts.get(0).ok_or_else(err)?;
        port = port_part.trim();
        host = host_part.trim();
    }
    Ok((user, host, port))
}
