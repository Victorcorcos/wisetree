//! `ConfigService` — discovery, load, save, reset.
//!
//! Resolution order is the project-local `.wisetree/config.json` first,
//! then `~/.wisetree/config.json`, falling back to
//! `WorktreeConfig::default()`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::schema::{GlobalDashboardConfig, WorktreeConfig};
use crate::constants::{global_config_file, local_config_file};
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
    warnings: Vec<String>,
    global_path: PathBuf,
}

impl Default for ConfigService {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigService {
    pub fn new() -> Self {
        Self::with_global_config_path(global_config_file())
    }

    /// Use an explicit global path, including for isolated tests.
    pub fn with_global_config_path(global_path: PathBuf) -> Self {
        Self {
            config: WorktreeConfig::default(),
            config_path: None,
            warnings: Vec::new(),
            global_path,
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

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Apply a partial update in-memory.
    pub fn update(&mut self, mut f: impl FnMut(&mut WorktreeConfig)) -> WorktreeConfig {
        f(&mut self.config);
        self.config.clone()
    }

    /// Reset the in-memory config to defaults (does not touch disk).
    pub fn reset(&mut self) -> WorktreeConfig {
        self.config = WorktreeConfig::default();
        self.warnings.clear();
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

        self.warnings.clear();
        Ok(self.config.clone())
    }

    /// Load configuration for a worktree, preferring the mother worktree's
    /// local file over the current worktree's local file and the global file.
    pub fn load_for_worktree(
        &mut self,
        mother_path: &Path,
        child_path: &Path,
    ) -> Result<WorktreeConfig> {
        self.ensure_global_config()?;

        let candidates = [
            local_config_file(mother_path),
            local_config_file(child_path),
            self.global_path.clone(),
        ];
        if let Some(path) = candidates.into_iter().find(|path| path.exists()) {
            return self.load_from_path(path);
        }

        self.warnings.clear();
        Ok(self.config.clone())
    }

    /// Load the global config file directly, ignoring any project-local file.
    pub fn load_global(&mut self) -> Result<WorktreeConfig> {
        self.ensure_global_config()?;
        self.load_from_path(self.global_path.clone())
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

        // The selection is saved by the account picker, so the Settings editor
        // may hold an older one. Keep the latest; project configs never store it.
        let mut config = config.clone();
        config.global_dashboard = if target == self.global_path {
            read_global_dashboard_preferences(&target)?.or(config.global_dashboard)
        } else {
            None
        };
        let json = serialize_pretty(&config)?;
        fs::write(&target, json)?;

        self.config = config;
        self.config_path = Some(target);
        self.warnings.clear();
        Ok(())
    }

    /// Create or overwrite the global config file with current defaults.
    pub fn create_global_config(&mut self) -> Result<PathBuf> {
        let dir = self.global_path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir)?;
        let path = self.global_path.clone();
        let defaults = WorktreeConfig::default();
        let json = serialize_pretty(&defaults)?;
        fs::write(&path, json)?;
        self.config = defaults;
        self.config_path = Some(path.clone());
        self.warnings.clear();
        Ok(path)
    }

    /// True when `~/.wisetree/config.json` exists on disk.
    pub fn has_global_config(&self) -> bool {
        self.global_path.exists()
    }

    pub fn load_global_dashboard_preferences() -> Result<GlobalDashboardConfig> {
        Self::load_global_dashboard_preferences_at(&global_config_file())
    }

    pub fn load_global_dashboard_preferences_at(path: &Path) -> Result<GlobalDashboardConfig> {
        Ok(read_global_dashboard_preferences(path)?.unwrap_or_default())
    }

    pub fn update_global_dashboard_selected_accounts(
        accounts: Vec<String>,
    ) -> Result<GlobalDashboardConfig> {
        Self::update_global_dashboard_selected_accounts_at(&global_config_file(), accounts)
    }

    /// Atomically patch only the selection, keeping all other JSON keys.
    pub fn update_global_dashboard_selected_accounts_at(
        path: &Path,
        accounts: Vec<String>,
    ) -> Result<GlobalDashboardConfig> {
        let mut document: serde_json::Value = match fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                serde_json::to_value(WorktreeConfig::default())?
            }
            Err(err) => return Err(err.into()),
        };
        let mut preferences = GlobalDashboardConfig {
            selected_accounts: accounts,
        };
        preferences.deduplicate();
        let invalid = || {
            WisetreeError::config(
                "Global configuration and globalDashboard must be objects",
                Some(path.to_path_buf()),
            )
        };
        let root = document.as_object_mut().ok_or_else(invalid)?;
        let dashboard = root
            .entry("globalDashboard")
            .or_insert_with(|| serde_json::json!({}));
        dashboard.as_object_mut().ok_or_else(invalid)?.insert(
            "selectedAccounts".to_string(),
            serde_json::to_value(&preferences.selected_accounts)?,
        );
        let parent = path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        std::io::Write::write_all(&mut file, serialize_pretty(&document)?.as_bytes())?;
        file.persist(path).map_err(|err| err.error)?;
        Ok(preferences)
    }

    /// Ensure the global config file exists; create it with defaults if not.
    pub fn ensure_global_config(&self) -> Result<()> {
        let path = self.global_path.clone();
        if path.exists() {
            return Ok(());
        }
        let dir = self.global_path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir)?;
        let json = serialize_pretty(&WorktreeConfig::default())?;
        fs::write(&path, json)?;
        Ok(())
    }

    fn find_config_file(&self, project_path: Option<&Path>) -> Result<Option<PathBuf>> {
        let cwd = project_path
            .map(|p| p.to_path_buf())
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));

        let candidates = [local_config_file(&cwd), self.global_path.clone()];
        Ok(candidates.into_iter().find(|path| path.exists()))
    }

    fn load_from_path(&mut self, path: PathBuf) -> Result<WorktreeConfig> {
        let raw = fs::read_to_string(&path).map_err(|e| {
            WisetreeError::config(
                format!("Failed to load config from {}: {e}", path.display()),
                Some(path.clone()),
            )
        })?;

        let mut parsed: WorktreeConfig = serde_json::from_str(&raw).map_err(|e| {
            WisetreeError::config(
                format!("Invalid configuration in {}: {e}", path.display()),
                Some(path.clone()),
            )
        })?;

        if let Some(preferences) = &mut parsed.global_dashboard {
            preferences.deduplicate();
        }
        parsed.dashboard.clamp();
        self.warnings = parsed.dashboard.normalize_columns();

        self.config = parsed;
        self.config_path = Some(path);
        Ok(self.config.clone())
    }
}

fn read_global_dashboard_preferences(path: &Path) -> Result<Option<GlobalDashboardConfig>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let document: serde_json::Value = serde_json::from_str(&raw)?;
    let mut preferences: Option<GlobalDashboardConfig> = document
        .get("globalDashboard")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()?;
    if let Some(preferences) = &mut preferences {
        preferences.deduplicate();
    }
    Ok(preferences)
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
