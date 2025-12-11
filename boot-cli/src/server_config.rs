use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerConfig {
    pub servers: Vec<Server>,
}

impl ServerConfig {
    fn config_path() -> Result<PathBuf> {
        let proj_dirs = ProjectDirs::from("", "", "boot")
            .context("Failed to determine project directories")?;
        
        let config_dir = proj_dirs.data_dir();
        fs::create_dir_all(config_dir)
            .context("Failed to create config directory")?;
        
        Ok(config_dir.join("servers.json"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        
        if !path.exists() {
            return Ok(ServerConfig {
                servers: Vec::new(),
            });
        }

        let content = fs::read_to_string(&path)
            .context("Failed to read servers config file")?;
        
        let config: ServerConfig = serde_json::from_str(&content)
            .context("Failed to parse servers config file")?;
        
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        let content = serde_json::to_string_pretty(self)
            .context("Failed to serialize servers config")?;
        
        fs::write(&path, content)
            .context("Failed to write servers config file")?;
        
        Ok(())
    }

    pub fn add_server(&mut self, name: String, url: String) -> Result<()> {
        if self.servers.iter().any(|s| s.name == name) {
            anyhow::bail!("Server with name '{}' already exists", name);
        }

        self.servers.push(Server { name, url });
        Ok(())
    }

    pub fn remove_server(&mut self, name: &str) -> Result<()> {
        let initial_len = self.servers.len();
        self.servers.retain(|s| s.name != name);
        
        if self.servers.len() == initial_len {
            anyhow::bail!("Server '{}' not found", name);
        }
        
        Ok(())
    }

    pub fn list_servers(&self) -> &[Server] {
        &self.servers
    }
}
