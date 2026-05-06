//! Configuration loading and persistence.

pub mod schema;
pub mod service;

pub use schema::{AppState, WorktreeConfig};
pub use service::{ConfigService, ResolvedConfig};
