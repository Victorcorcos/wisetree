//! `ConfigService` — discovery, load, save, reset.
//!
//! Resolution order matches the upstream behaviour: project-local
//! `.wisetree.json` first, then `~/.wisetree/settings.json`, falling back to
//! `WorktreeConfig::default()`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::schema::WorktreeConfig;
use crate::constants::{global_config_dir, global_config_file, LOCAL_CONFIG_FILE_NAME};
use crate::errors::{Result, WisetreeError};

/// Where the active config came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfig {
    pub config: WorktreeConfig,
    pub path: Option<PathBuf>,
    pub is_global: bool,
}

#[derive(Debug, Clone)]
pub struct ConfigService {
    config: WorktreeConfig,
    config_path: Option<PathBuf>,
}

impl Default for ConfigService {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigService {
    pub fn new() -> Self {
        Self {
            config: WorktreeConfig::default(),
            config_path: None,
        }
    }

    /// Snapshot of the current in-memory config.
    pub fn config(&self) -> &WorktreeConfig {
        &self.config
    }

    /// Path the config was loaded from, if any.
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// Apply a partial update in-memory.
    pub fn update(&mut self, mut f: impl FnMut(&mut WorktreeConfig)) -> WorktreeConfig {
        f(&mut self.config);
        self.config.clone()
    }

    /// Reset the in-memory config to defaults (does not touch disk).
    pub fn reset(&mut self) -> WorktreeConfig {
        self.config = WorktreeConfig::default();
        self.config.clone()
    }

    /// Load the config, populating the global config file if absent.
    ///
    /// `project_path` overrides the `cwd` used to look up the local config.
    pub fn load(&mut self, project_path: Option<&Path>) -> Result<WorktreeConfig> {
        self.ensure_global_config()?;

        if let Some(path) = self.find_config_file(project_path)? {
            return self.load_from_path(path);
        }

        Ok(self.config.clone())
    }

    /// Load the global config file directly, ignoring any project-local file.
    pub fn load_global(&mut self) -> Result<WorktreeConfig> {
        self.ensure_global_config()?;
        self.load_from_path(global_config_file())
    }

    /// Persist the given config to disk, defaulting to the previously-loaded
    /// path. Pretty-prints with 2-space indent.
    pub fn save(&mut self, config: &WorktreeConfig, path: Option<&Path>) -> Result<()> {
        let target: PathBuf = path
            .map(|p| p.to_path_buf())
            .or_else(|| self.config_path.clone())
            .ok_or_else(|| WisetreeError::config("No config path available for saving", None))?;

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }

        let json = serialize_pretty(config)?;
        fs::write(&target, json)?;

        self.config = config.clone();
        self.config_path = Some(target);
        Ok(())
    }

    /// Create or overwrite the global config file with current defaults.
    pub fn create_global_config(&mut self) -> Result<PathBuf> {
        let dir = global_config_dir();
        fs::create_dir_all(&dir)?;
        let path = global_config_file();
        let defaults = WorktreeConfig::default();
        let json = serialize_pretty(&defaults)?;
        fs::write(&path, json)?;
        self.config = defaults;
        self.config_path = Some(path.clone());
        Ok(path)
    }

    /// True when `~/.wisetree/settings.json` exists on disk.
    pub fn has_global_config(&self) -> bool {
        global_config_file().exists()
    }

    /// Ensure the global config file exists; create it with defaults if not.
    pub fn ensure_global_config(&self) -> Result<()> {
        let path = global_config_file();
        if path.exists() {
            return Ok(());
        }
        let dir = global_config_dir();
        fs::create_dir_all(&dir)?;
        let json = serialize_pretty(&WorktreeConfig::default())?;
        fs::write(&path, json)?;
        Ok(())
    }

    fn find_config_file(&self, project_path: Option<&Path>) -> Result<Option<PathBuf>> {
        let cwd = project_path
            .map(|p| p.to_path_buf())
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));

        let candidates = [cwd.join(LOCAL_CONFIG_FILE_NAME), global_config_file()];
        for c in candidates {
            if c.exists() {
                return Ok(Some(c));
            }
        }
        Ok(None)
    }

    fn load_from_path(&mut self, path: PathBuf) -> Result<WorktreeConfig> {
        let raw = fs::read_to_string(&path).map_err(|e| {
            WisetreeError::config(
                format!("Failed to load config from {}: {e}", path.display()),
                Some(path.clone()),
            )
        })?;

        let parsed: WorktreeConfig = serde_json::from_str(&raw).map_err(|e| {
            WisetreeError::config(
                format!("Invalid configuration in {}: {e}", path.display()),
                Some(path.clone()),
            )
        })?;

        self.config = parsed;
        self.config_path = Some(path);
        Ok(self.config.clone())
    }
}

/// Pretty-print a config in upstream-compatible JSON (2-space indent).
fn serialize_pretty<T: serde::Serialize>(value: &T) -> Result<String> {
    let mut out = Vec::with_capacity(256);
    let mut ser = serde_json::Serializer::with_formatter(
        &mut out,
        serde_json::ser::PrettyFormatter::with_indent(b"  "),
    );
    value.serialize(&mut ser)?;
    String::from_utf8(out).map_err(|e| WisetreeError::other(e.to_string()))
}
