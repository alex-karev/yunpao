use crate::config::Server;
use crate::utils::{create_from_template, is_empty_map, load_toml};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Global {
    #[serde(default, skip_serializing_if = "is_empty_map")]
    pub env: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "is_empty_map")]
    pub servers: HashMap<String, Server>,
    #[serde(default)]
    pub default_server: Option<String>,
}

impl Global {
    /// Returns global config path
    pub fn get_path() -> Result<PathBuf> {
        let mut path = dirs::config_dir().context("Invalid path")?;
        path.push("yunpao");
        path.push("config.toml");
        Ok(path)
    }

    /// Checks if config exists
    pub fn exists() -> bool {
        if let Some(path) = Self::get_path().ok() {
            path.exists()
        } else {
            false
        }
    }

    /// Save new config
    pub fn save(&self) -> Result<()> {
        let path = Self::get_path()?;
        let content = toml::to_string_pretty(self)?;
        let template = include_str!("./templates/config.toml");
        create_from_template(
            template,
            HashMap::from([("content", content.as_str())]),
            &path,
        )?;
        Ok(())
    }

    /// Loads config or init new config
    pub fn load() -> Result<Self> {
        let path = Self::get_path()?;
        let mut data = if !path.exists() {
            let data = Self::default();
            data.save()?;
            data
        } else {
            load_toml(&path)?
        };
        for item in &mut data.servers {
            item.1.id = item.0.clone();
        }
        Ok(data)
    }

    /// Find server by alias
    pub fn find_server(&self, query: &String) -> Result<&Server> {
        let result = self
            .servers
            .iter()
            .find(|(k, v)| v.aliases.contains(query) || k == &query || k.ends_with(query));
        let server = result
            .context(format!("Server with alias or id '{}' not found", query))?
            .1;
        Ok(server)
    }

    /// Set server as default
    pub fn set_default_server(&mut self, id: &String) -> Result<()> {
        ensure!(
            self.servers.contains_key(id),
            "Server with id '{}' not found",
            id
        );
        self.default_server = Some(id.clone());
        self.save()?;
        Ok(())
    }

    /// Add new server to global config
    pub fn add_server(&mut self, server: Server) -> Result<()> {
        ensure!(
            !self.servers.contains_key(&server.id),
            "Server with id '{}' already exists",
            &server.id
        );
        self.servers.insert(server.id.clone(), server);
        self.save()?;
        Ok(())
    }

    /// Delete server from global config
    pub fn delete_server(&mut self, id: &String) -> Result<()> {
        ensure!(
            self.servers.contains_key(id),
            "Server with id '{}' not found",
            id
        );
        let _ = self.servers.remove(id);
        self.save()?;
        Ok(())
    }
}
