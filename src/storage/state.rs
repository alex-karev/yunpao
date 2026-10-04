use crate::utils::{load_toml, save_toml};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};
use crate::storage::Session;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectState {
    #[serde(default)]
    current_session: Option<String>,
    #[serde(default)]
    sessions: HashMap<String, Session>,
    #[serde(skip)]
    id: String,
}

impl ProjectState {
    /// Get path helper
    fn build_path(project_id: &String) -> PathBuf {
        let mut path = dirs::cache_dir().unwrap();
        path.push("yunpao");
        path.push("state");
        path.push(format!("{}.toml", project_id));
        path
    }

    /// Returns state file path
    pub fn get_path(&self) -> PathBuf {
        Self::build_path(&self.id)
    }

    /// Load project state
    pub fn load(project_id: &String) -> Result<Self> {
        let path = Self::build_path(project_id);
        let state = if path.exists() {
            let mut state: Self = load_toml(&path)?;
            state.id = project_id.clone();
            for item in &mut state.sessions {
                item.1.id = item.0.clone();
            }
            state
        } else {
            let state = Self {
                current_session: None,
                sessions: HashMap::default(),
                id: project_id.clone(),
            };
            save_toml(&path, &state)?;
            state
        };
        Ok(state)
    }

    /// Save project state
    pub fn save(&self) -> Result<()> {
        let path = self.get_path();
        save_toml(&path, &self)?;
        Ok(())
    }

    /// Start new session
    pub fn new_session(&mut self, server_id: &String, name: Option<&String>) -> Result<()> {
        let session = Session::new(server_id, name);
        self.current_session = Some(session.id.clone());
        self.sessions.insert(session.id.clone(), session);
        self.save()?;
        Ok(())
    }

    /// Switch to session
    pub fn switch_session(&mut self, session_id: &String) -> Result<()> {
        ensure!(self.sessions.contains_key(session_id), "Session with id \"{}\" not found", session_id);
        self.current_session = Some(session_id.clone());
        self.save()?;
        Ok(())
    }

    /// Find session by id or name
    pub fn find_session(&self, query: &String) -> Result<&Session> {
        let session = self.sessions.iter().find(|(k,v)| k.starts_with(query) || &v.name == query);
        let session = session.context(format!("Session with id or name \"{}\" not found", query))?;
        Ok(session.1)
    }

    /// Delete session
    pub fn delete_session(&mut self, session_id: &String) -> Result<()> {
        self.sessions.remove(session_id);
        if let Some(current_session) = self.current_session.as_ref()
            && current_session == session_id
        {
            self.current_session = None;
        }
        self.save()?;
        Ok(())
    }

    /// List sessions
    pub fn list_sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values()
    }

    /// Get current session
    pub fn current_session(&self) -> Option<&Session> {
        if let Some(current_session) = self.current_session.as_ref() {
            self.sessions.get(current_session)
        } else {
            None
        }
    }

    /// Get current session as mutable
    pub fn current_session_mut(&mut self) -> Option<&mut Session> {
        if let Some(current_session) = self.current_session.as_ref() {
            self.sessions.get_mut(current_session)
        } else {
            None
        }
    }
}
