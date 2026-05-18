//! Configuration loading and persistence.

pub mod schema;
pub mod service;

pub use schema::{AppState, LinkStrategy, WorktreeConfig};
pub use service::{ConfigService, ResolvedConfig};
