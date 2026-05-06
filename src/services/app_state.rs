//! `AppStateService` — best-effort load/save for `~/.wisetree/state.json`.

use std::fs;
use std::path::PathBuf;

use crate::config::schema::AppState;
use crate::constants::{app_state_file, global_config_dir};

/// Wraps the on-disk app state. Errors during persistence are intentionally
/// swallowed (the upstream service does the same) so a flaky cache file
/// never blocks the user.
#[derive(Debug, Clone, Default)]
pub struct AppStateService {
    state: AppState,
    path: Option<PathBuf>,
}

impl AppStateService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the state, returning defaults on error.
    pub fn load(&mut self) -> AppState {
        let path = app_state_file();
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(parsed) = serde_json::from_str::<AppState>(&content) {
                self.state = parsed;
            }
        }
        self.path = Some(path);
        self.state.clone()
    }

    /// Persist the current state. Failures are silently ignored.
    pub fn save(&self) {
        let _ = self.try_save();
    }

    fn try_save(&self) -> std::io::Result<()> {
        fs::create_dir_all(global_config_dir())?;
        let path = app_state_file();
        let json = serde_json::to_string_pretty(&self.state)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(path, json)
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Apply a partial update. Returns the new state.
    pub fn update(&mut self, mut f: impl FnMut(&mut AppState)) -> AppState {
        f(&mut self.state);
        self.state.clone()
    }
}
