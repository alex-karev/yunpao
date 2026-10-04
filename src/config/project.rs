use crate::storage::ProjectState;
use crate::utils::{create_from_template, load_toml, random_uuid};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub actions: HashMap<String, String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub disable_dotenv: bool,
    #[serde(skip)]
    pub state: ProjectState,
}

impl Project {
    /// Returns project config path
    pub fn get_path() -> anyhow::Result<PathBuf> {
        let mut path = std::env::current_dir()?;
        path.push("yunpao.toml");
        Ok(path)
    }

    /// Check if config file is present
    pub fn exists() -> bool {
        let path = Self::get_path().unwrap();
        path.exists()
    }

    /// Initializes new project config in the current directory
    pub fn init() -> Result<()> {
        let path = Self::get_path()?;
        let template = include_str!("./templates/project.toml");
        let id = random_uuid();
        create_from_template(template, HashMap::from([("id", id.as_str())]), &path)?;
        Ok(())
    }

    /// Loads config from toml
    pub fn load() -> Result<Self> {
        let path = Self::get_path()?;
        let mut project: Project = load_toml(&path)?;
        project.state = ProjectState::load(&project.id)?;
        Ok(project)
    }
}
