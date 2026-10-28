//! Command-line interface entry point.

pub mod args;
pub mod commands;
pub mod run;

pub use args::{parse_args, AppMode, CliArgs, CliCommand, ParsedArgs};
pub use run::run;
