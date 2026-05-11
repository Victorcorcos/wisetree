//! ratatui-based terminal UI.

pub mod app;
pub mod event;
pub mod router;
pub mod screens;
pub mod selection;
pub mod terminal;
pub mod widgets;

pub use app::App;
pub use router::Screen;
