//! Shared border-color state. Replaces React's `BorderContext` from upstream:
//! widgets that render inside another bordered region can briefly tint the
//! outer border (e.g., confirm dialogs colorize their parent panel by variant).

use ratatui::style::Color;

use crate::messages::colors;

#[derive(Debug, Clone, Copy)]
pub struct BorderState {
    pub color: Color,
}

impl BorderState {
    pub fn new() -> Self {
        Self {
            color: colors::MUTED,
        }
    }

    pub fn set(&mut self, color: Color) {
        self.color = color;
    }

    pub fn reset(&mut self) {
        self.color = colors::MUTED;
    }
}

impl Default for BorderState {
    fn default() -> Self {
        Self::new()
    }
}
