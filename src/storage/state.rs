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

    /// The project id this state belongs to.
    pub fn id(&self) -> &String {
        &self.id
    }

    /// Read-modify-write under an exclusive file lock, starting from what's on disk *now*.
    /// The web page, its job queue and the terminal can all change the state at once; a plain
    /// load … save would let the last writer silently drop the others' tasks and sessions.
    pub fn update<T>(project_id: &String, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<(Self, T)> {
        let path = Self::build_path(project_id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_extension("lock"))
            .context("Could not open the state lock file")?;
        lock.lock().context("Could not lock the project state")?;
        let mut state = Self::load(project_id)?;
        let out = f(&mut state)?;
        state.save()?;
        Ok((state, out))
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
        let (fresh, _) = Self::update(&self.id, |st| {
            st.current_session = Some(session.id.clone());
            st.sessions.insert(session.id.clone(), session);
            Ok(())
        })?;
        *self = fresh;
        Ok(())
    }

    /// Switch to session
    pub fn switch_session(&mut self, session_id: &String) -> Result<()> {
        // A session only we know about (created in this process, not on disk yet) is kept.
        let mine = self.sessions.get(session_id).cloned();
        let (fresh, _) = Self::update(&self.id, |st| {
            if let Some(s) = mine {
                st.sessions.entry(session_id.clone()).or_insert(s);
            }
            ensure!(st.sessions.contains_key(session_id), "Session with id \"{}\" not found", session_id);
            st.current_session = Some(session_id.clone());
            Ok(())
        })?;
        *self = fresh;
        Ok(())
    }

    /// The session with this id, inserting `session` if the file doesn't have it (yet).
    pub fn session_or_insert(&mut self, session: Session) -> &mut Session {
        self.sessions.entry(session.id.clone()).or_insert(session)
    }

    /// Sets the current session without saving (for use inside [`Self::update`]).
    pub fn switch_current(&mut self, session_id: String) {
        if self.sessions.contains_key(&session_id) {
            self.current_session = Some(session_id);
        }
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

    /// Get session by id as mutable
    pub fn session_mut(&mut self, session_id: &str) -> Option<&mut Session> {
        self.sessions.get_mut(session_id)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_updates_keep_every_change() {
        let id = format!("test-{}", crate::utils::random_uuid());
        let threads: Vec<_> = (0..20)
            .map(|i| {
                let id = id.clone();
                std::thread::spawn(move || {
                    ProjectState::update(&id, |st| {
                        let s = Session::new(&format!("server{i}"), None);
                        st.session_or_insert(s);
                        Ok(())
                    })
                    .unwrap();
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        let n = ProjectState::load(&id).unwrap().list_sessions().count();
        let path = ProjectState::build_path(&id);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("lock"));
        assert_eq!(n, 20);
    }
}
