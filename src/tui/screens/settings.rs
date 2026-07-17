//! Settings screen — mostly read-only view of the active `WorktreeConfig`
//! plus a writable `Delete Branch with Worktree` toggle and a
//! "Check for Updates" entry. Mirrors upstream `SettingsMenu` (steps:
//! `Menu`, five read-only field detail views, the toggle view, and
//! `CheckUpdates`).
//!
//! Async work is owned by `App`: when the user picks "Check for updates",
//! the screen emits `SettingsAction::CheckUpdates`; `App` runs the async
//! call and feeds the outcome back via `set_update_result`. Likewise the
//! reset-to-defaults flow and delete-branch toggle persistence are signalled
//! back to `App`.

use std::cell::RefCell;
use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::config::schema::{
    AiBugkillConfig, AiConfig, AiFixConfig, AiModelConfig, DashboardConfig, LinkStrategy,
    NotificationsConfig, WorktreeConfig,
};
use crate::messages::{colors, UPDATE_CHECKING, UPDATE_CHECK_MENU};
use crate::services::{MultiSourceUpdateResult, UpdateSource};
use crate::tui::screens::ai_model_picker::REASONING_VARIANTS;
use crate::tui::widgets::{
    branded_line, ConfirmationChoice, ConfirmationModal, ConfirmationOutcome, InputOutcome,
    InputPrompt, SelectOption, SelectOutcome, SelectPrompt, Status, StatusIndicator,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsStep {
    Menu,
    SetupProject,
    CopyPatterns,
    LinkPatterns,
    LinkStrategy,
    LinkCacheDir,
    IgnorePatterns,
    PathTemplate,
    Dashboard,
    /// Per-command AI model picker grid, reached from the Dashboard `ai`
    /// rectangle. Esc returns to the Dashboard editor.
    AiSettings,
    Notifications,
    PostCmd,
    TerminalCmd,
    DeleteBranch,
    CopySettings,
    CheckUpdates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyDirection {
    /// Overwrite (or create) the project-local `.wisetree.json` with the
    /// contents of `~/.wisetree/settings.json`.
    GlobalToLocal,
    /// Overwrite (or create) `~/.wisetree/settings.json` with the contents
    /// of the project-local `.wisetree.json`.
    LocalToGlobal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsAction {
    Continue,
    Back,
    CopySettingsFilePath,
    CheckUpdates,
    /// User confirmed an upgrade in the "Check for Updates" screen.
    /// The `App` runs the matching shell command for the source.
    UpgradeSource(UpdateSource),
    SetDeleteBranchWithWorktree(bool),
    Reset,
    SaveCopyPatterns(Vec<String>),
    SaveIgnorePatterns(Vec<String>),
    SaveLinkPatterns(Vec<String>),
    /// Persist the supplied post-create commands to the project-local
    /// `.wisetree.json`. Empty entries and deletion-marked rectangles are
    /// filtered out by the caller before they reach disk.
    SavePostCreateCommands(Vec<String>),
    /// Persist the supplied terminal command to the active config file.
    /// An empty string clears the configured command.
    SaveTerminalCommand(String),
    /// Persist the supplied worktree path template to the active config file.
    /// An empty string falls back to the default template on next load via
    /// the schema default.
    SavePathTemplate(String),
    /// Persist the selected shared-link strategy to the active config file.
    SaveLinkStrategy(LinkStrategy),
    /// Persist the shared cache root override to the active config file.
    /// An empty string clears the override and falls back to the default root.
    SaveLinkCacheDir(String),
    /// Persist the dashboard settings (refresh interval, show pull requests,
    /// columns, per-command AI models) to the active config file. Invalid
    /// numbers or unknown columns are normalized by the caller. Boxed because
    /// `DashboardConfig` is by far the largest `SettingsAction` payload.
    SaveDashboard(Box<DashboardConfig>),
    /// Persist the opt-in notification toggles to the active config file.
    SaveNotifications(NotificationsConfig),
    /// Open the fullscreen AI model picker prefilled with the current `ai`
    /// model and its thinking strength. The caller owns the picker screen and
    /// writes the user's choice back via `apply_ai_selection`.
    OpenAiModelPicker {
        model: String,
        variant: String,
    },
    /// Kick off the background `opencode models opencode` shell-out that
    /// populates the Dashboard editor's inline free-model quick-pick row.
    /// Emitted once when the user enters the Dashboard editor.
    FetchFreeModels,
    /// Copy the active config from one location to the other.
    CopySettings(CopyDirection),
    /// Navigate to the SetupProject screen to bootstrap a project config.
    OpenSetupProject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostCmdRectStatus {
    /// New unsaved rectangle — white border.
    Unchanged,
    /// Currently being typed into — yellow border.
    Editing,
    /// Edited and exited but not yet saved — orange border.
    Modified,
    /// Marked for deletion on the next save — pink/red border.
    MarkedForDeletion,
    /// Just persisted to disk — green border.
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostCmdSelection {
    Rect(usize),
    Create,
    Save,
}

const POST_CMD_SELECTION_MARKER: &str = " ✎﹏ ";

/// State for the inline post-create commands editor surfaced when the user
/// drills into the `Post-Create Commands` setting from the menu.
pub struct PostCmdEditor {
    pub commands: Vec<String>,
    pub statuses: Vec<PostCmdRectStatus>,
    pub selection: PostCmdSelection,
    last_rect_selection: Option<usize>,
    /// Snapshot taken when the user enters edit mode, used to restore on Esc.
    edit_backup: Option<(String, PostCmdRectStatus)>,
    /// For each rectangle, the tick at which a swap-flash animation started.
    /// `None` means no active animation. Parallel to `commands` / `statuses`.
    swap_highlights: Vec<Option<usize>>,
}

/// Duration of the post-swap flash animation, in ticks (≈100ms each ⇒ 2s).
const SWAP_ANIM_TICKS: usize = 20;

fn lerp_rgb(
    from: ratatui::style::Color,
    to: ratatui::style::Color,
    t: f32,
) -> ratatui::style::Color {
    use ratatui::style::Color;
    let t = t.clamp(0.0, 1.0);
    match (from, to) {
        (Color::Rgb(fr, fg, fb), Color::Rgb(tr, tg, tb)) => {
            let r = (fr as f32 + (tr as f32 - fr as f32) * t).round() as u8;
            let g = (fg as f32 + (tg as f32 - fg as f32) * t).round() as u8;
            let b = (fb as f32 + (tb as f32 - fb as f32) * t).round() as u8;
            Color::Rgb(r, g, b)
        }
        _ => to,
    }
}

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

impl PostCmdEditor {
    pub fn new(commands: Vec<String>) -> Self {
        let has_commands = !commands.is_empty();
        let statuses = vec![PostCmdRectStatus::Saved; commands.len()];
        let swap_highlights = vec![None; commands.len()];
        Self {
            commands,
            statuses,
            selection: PostCmdSelection::Create,
            last_rect_selection: if has_commands { Some(0) } else { None },
            edit_backup: None,
            swap_highlights,
        }
    }

    pub fn swap_highlight_start(&self, idx: usize) -> Option<usize> {
        self.swap_highlights.get(idx).copied().flatten()
    }

    pub fn editing_index(&self) -> Option<usize> {
        self.statuses
            .iter()
            .position(|&s| s == PostCmdRectStatus::Editing)
    }

    fn set_selection(&mut self, selection: PostCmdSelection) {
        if let PostCmdSelection::Rect(i) = selection {
            self.last_rect_selection = Some(i);
        }
        self.selection = selection;
    }

    fn visible_range(&self, max_visible: usize) -> Range<usize> {
        if max_visible == 0 || self.commands.is_empty() {
            return 0..0;
        }

        if self.commands.len() <= max_visible {
            return 0..self.commands.len();
        }

        let active = self.editing_index().or(match self.selection {
            PostCmdSelection::Rect(i) => Some(i),
            PostCmdSelection::Create | PostCmdSelection::Save => self.last_rect_selection,
        });
        let start = active
            .unwrap_or(0)
            .saturating_add(1)
            .saturating_sub(max_visible);
        let end = (start + max_visible).min(self.commands.len());
        end.saturating_sub(max_visible)..end
    }

    fn move_up(&mut self) {
        let next = match self.selection {
            PostCmdSelection::Rect(0) => self.selection,
            PostCmdSelection::Rect(i) => PostCmdSelection::Rect(i - 1),
            PostCmdSelection::Create | PostCmdSelection::Save if self.commands.is_empty() => {
                self.selection
            }
            PostCmdSelection::Create | PostCmdSelection::Save => {
                PostCmdSelection::Rect(self.commands.len() - 1)
            }
        };
        self.set_selection(next);
    }

    fn move_down(&mut self) {
        let next = match self.selection {
            PostCmdSelection::Rect(i) if i + 1 < self.commands.len() => {
                PostCmdSelection::Rect(i + 1)
            }
            PostCmdSelection::Rect(_) => PostCmdSelection::Create,
            PostCmdSelection::Create | PostCmdSelection::Save => self.selection,
        };
        self.set_selection(next);
    }

    pub fn move_selected_up(&mut self, current_tick: usize) {
        if self.editing_index().is_some() {
            return;
        }
        let PostCmdSelection::Rect(i) = self.selection else {
            return;
        };
        if i == 0 {
            return;
        }
        self.commands.swap(i, i - 1);
        self.statuses.swap(i, i - 1);
        self.swap_highlights.swap(i, i - 1);
        self.statuses[i] = Self::mark_modified(self.statuses[i]);
        self.statuses[i - 1] = Self::mark_modified(self.statuses[i - 1]);
        self.swap_highlights[i] = Some(current_tick);
        self.swap_highlights[i - 1] = Some(current_tick);
        self.set_selection(PostCmdSelection::Rect(i - 1));
    }

    pub fn move_selected_down(&mut self, current_tick: usize) {
        if self.editing_index().is_some() {
            return;
        }
        let PostCmdSelection::Rect(i) = self.selection else {
            return;
        };
        if i + 1 >= self.commands.len() {
            return;
        }
        self.commands.swap(i, i + 1);
        self.statuses.swap(i, i + 1);
        self.swap_highlights.swap(i, i + 1);
        self.statuses[i] = Self::mark_modified(self.statuses[i]);
        self.statuses[i + 1] = Self::mark_modified(self.statuses[i + 1]);
        self.swap_highlights[i] = Some(current_tick);
        self.swap_highlights[i + 1] = Some(current_tick);
        self.set_selection(PostCmdSelection::Rect(i + 1));
    }

    fn mark_modified(status: PostCmdRectStatus) -> PostCmdRectStatus {
        if status == PostCmdRectStatus::MarkedForDeletion {
            PostCmdRectStatus::MarkedForDeletion
        } else {
            PostCmdRectStatus::Modified
        }
    }

    fn toggle_buttons(&mut self) {
        let next = match self.selection {
            PostCmdSelection::Create => PostCmdSelection::Save,
            PostCmdSelection::Save => PostCmdSelection::Create,
            other => other,
        };
        self.set_selection(next);
    }

    fn toggle_delete_mark(&mut self) {
        let PostCmdSelection::Rect(i) = self.selection else {
            return;
        };

        self.statuses[i] = match self.statuses[i] {
            PostCmdRectStatus::MarkedForDeletion => PostCmdRectStatus::Modified,
            PostCmdRectStatus::Editing => PostCmdRectStatus::Editing,
            _ => PostCmdRectStatus::MarkedForDeletion,
        };
    }

    fn commands_to_save(&self) -> Vec<String> {
        self.commands
            .iter()
            .zip(self.statuses.iter())
            .filter_map(|(command, status)| {
                if matches!(status, PostCmdRectStatus::MarkedForDeletion) {
                    return None;
                }

                let trimmed = command.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalCmdRectStatus {
    Unchanged,
    Editing,
    Modified,
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalCmdSelection {
    Rect,
    Save,
}

/// State for the inline terminal command editor surfaced when the user
/// drills into the `Terminal Command` setting from the menu. Mirrors
/// `PostCmdEditor` but only ever holds a single fixed rectangle and a
/// Save button — the user cannot create or delete entries.
pub struct TerminalCmdEditor {
    pub command: String,
    pub status: TerminalCmdRectStatus,
    pub selection: TerminalCmdSelection,
    edit_backup: Option<(String, TerminalCmdRectStatus)>,
}

impl TerminalCmdEditor {
    pub fn new(command: String) -> Self {
        let status = if command.is_empty() {
            TerminalCmdRectStatus::Unchanged
        } else {
            TerminalCmdRectStatus::Saved
        };
        Self {
            command,
            status,
            selection: TerminalCmdSelection::Save,
            edit_backup: None,
        }
    }

    pub fn editing(&self) -> bool {
        self.status == TerminalCmdRectStatus::Editing
    }

    fn move_up(&mut self) {
        if matches!(self.selection, TerminalCmdSelection::Save) {
            self.selection = TerminalCmdSelection::Rect;
        }
    }

    fn move_down(&mut self) {
        if matches!(self.selection, TerminalCmdSelection::Rect) {
            self.selection = TerminalCmdSelection::Save;
        }
    }

    fn command_to_save(&self) -> String {
        self.command.trim().to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathTemplateRectStatus {
    Unchanged,
    Editing,
    Modified,
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathTemplateSelection {
    Rect,
    Save,
}

/// State for the inline worktree path template editor surfaced when the user
/// drills into the `Path Template` setting from the menu. Mirrors
/// `TerminalCmdEditor`: a single fixed rectangle plus a Save button.
pub struct PathTemplateEditor {
    pub template: String,
    pub status: PathTemplateRectStatus,
    pub selection: PathTemplateSelection,
    edit_backup: Option<(String, PathTemplateRectStatus)>,
}

impl PathTemplateEditor {
    pub fn new(template: String) -> Self {
        let status = if template.is_empty() {
            PathTemplateRectStatus::Unchanged
        } else {
            PathTemplateRectStatus::Saved
        };
        Self {
            template,
            status,
            selection: PathTemplateSelection::Save,
            edit_backup: None,
        }
    }

    pub fn editing(&self) -> bool {
        self.status == PathTemplateRectStatus::Editing
    }

    fn move_up(&mut self) {
        if matches!(self.selection, PathTemplateSelection::Save) {
            self.selection = PathTemplateSelection::Rect;
        }
    }

    fn move_down(&mut self) {
        if matches!(self.selection, PathTemplateSelection::Rect) {
            self.selection = PathTemplateSelection::Save;
        }
    }

    fn template_to_save(&self) -> String {
        self.template.trim().to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStrategyRectStatus {
    Unchanged,
    Modified,
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStrategySelection {
    Rect,
    Save,
}

pub struct LinkStrategyEditor {
    pub value: String,
    pub status: LinkStrategyRectStatus,
    pub selection: LinkStrategySelection,
}

impl LinkStrategyEditor {
    pub fn new(strategy: LinkStrategy) -> Self {
        Self {
            value: link_strategy_label(strategy).to_string(),
            status: LinkStrategyRectStatus::Saved,
            selection: LinkStrategySelection::Save,
        }
    }

    fn move_up(&mut self) {
        if matches!(self.selection, LinkStrategySelection::Save) {
            self.selection = LinkStrategySelection::Rect;
        }
    }

    fn move_down(&mut self) {
        if matches!(self.selection, LinkStrategySelection::Rect) {
            self.selection = LinkStrategySelection::Save;
        }
    }

    fn strategy_to_save(&self) -> LinkStrategy {
        parse_link_strategy(&self.value).unwrap_or(LinkStrategy::CreateEmpty)
    }

    fn toggle_value(&mut self) {
        let next = match self.strategy_to_save() {
            LinkStrategy::CreateEmpty => LinkStrategy::SeedFromSource,
            LinkStrategy::SeedFromSource => LinkStrategy::SeedIfPresent,
            LinkStrategy::SeedIfPresent => LinkStrategy::CreateEmpty,
        };
        self.value = link_strategy_label(next).to_string();
        self.status = LinkStrategyRectStatus::Modified;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkCacheDirRectStatus {
    Unchanged,
    Editing,
    Modified,
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkCacheDirSelection {
    Rect,
    Save,
}

pub struct LinkCacheDirEditor {
    pub value: String,
    pub status: LinkCacheDirRectStatus,
    pub selection: LinkCacheDirSelection,
    edit_backup: Option<(String, LinkCacheDirRectStatus)>,
}

impl LinkCacheDirEditor {
    pub fn new(value: Option<String>) -> Self {
        let value = value.unwrap_or_default();
        let status = if value.is_empty() {
            LinkCacheDirRectStatus::Unchanged
        } else {
            LinkCacheDirRectStatus::Saved
        };
        Self {
            value,
            status,
            selection: LinkCacheDirSelection::Save,
            edit_backup: None,
        }
    }

    pub fn editing(&self) -> bool {
        self.status == LinkCacheDirRectStatus::Editing
    }

    fn move_up(&mut self) {
        if matches!(self.selection, LinkCacheDirSelection::Save) {
            self.selection = LinkCacheDirSelection::Rect;
        }
    }

    fn move_down(&mut self) {
        if matches!(self.selection, LinkCacheDirSelection::Rect) {
            self.selection = LinkCacheDirSelection::Save;
        }
    }

    fn cache_dir_to_save(&self) -> String {
        self.value.trim().to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternListRectStatus {
    Unchanged,
    Editing,
    Modified,
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternListSelection {
    Rect,
    Save,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatternListCursor {
    row: usize,
    col: usize,
}

pub struct PatternListEditor {
    pub lines: Vec<String>,
    saved_lines: Vec<String>,
    pub status: PatternListRectStatus,
    pub selection: PatternListSelection,
    editing: Option<PatternListCursor>,
    scroll: u16,
}

impl PatternListEditor {
    pub fn new(lines: Vec<String>) -> Self {
        let status = if lines.is_empty() {
            PatternListRectStatus::Unchanged
        } else {
            PatternListRectStatus::Saved
        };
        Self {
            saved_lines: lines.clone(),
            lines,
            status,
            selection: PatternListSelection::Rect,
            editing: None,
            scroll: 0,
        }
    }

    pub fn editing(&self) -> bool {
        self.editing.is_some()
    }

    pub fn editing_cursor(&self) -> Option<PatternListCursor> {
        self.editing
    }

    pub fn visible_range(&self, area_height: u16) -> Range<usize> {
        if self.lines.is_empty() {
            return 0..0;
        }

        let visible = area_height.saturating_sub(2).max(1) as usize;
        let max_scroll = self.lines.len().saturating_sub(visible);
        let start = (self.scroll as usize).min(max_scroll);
        let end = (start + visible).min(self.lines.len());
        start..end
    }

    pub fn hidden_counts(&self, area_height: u16) -> (usize, usize) {
        let visible = self.visible_range(area_height);
        (visible.start, self.lines.len().saturating_sub(visible.end))
    }

    pub fn values_to_save(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect()
    }

    fn byte_offset(s: &str, char_idx: usize) -> usize {
        s.char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(s.len())
    }

    fn set_status_from_saved_snapshot(&mut self) {
        self.status = if self.lines == self.saved_lines {
            if self.lines.is_empty() {
                PatternListRectStatus::Unchanged
            } else {
                PatternListRectStatus::Saved
            }
        } else {
            PatternListRectStatus::Modified
        };
    }

    fn move_up(&mut self) {
        if matches!(self.selection, PatternListSelection::Save) {
            self.selection = PatternListSelection::Rect;
        }
    }

    fn move_down(&mut self) {
        if matches!(self.selection, PatternListSelection::Rect) {
            self.selection = PatternListSelection::Save;
        }
    }

    fn start_editing(&mut self) {
        self.selection = PatternListSelection::Rect;
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        let row = self.lines.len() - 1;
        let col = self.lines[row].chars().count();
        self.editing = Some(PatternListCursor { row, col });
        self.status = PatternListRectStatus::Editing;
    }

    fn stop_editing(&mut self) {
        self.editing = None;
        self.set_status_from_saved_snapshot();
    }

    fn with_cursor<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut Vec<String>, &mut PatternListCursor),
    {
        let Some(mut cursor) = self.editing else {
            return;
        };
        f(&mut self.lines, &mut cursor);
        let max_col = self
            .lines
            .get(cursor.row)
            .map(|line| line.chars().count())
            .unwrap_or(0);
        cursor.col = cursor.col.min(max_col);
        self.editing = Some(cursor);
    }

    fn insert_newline(&mut self) {
        self.with_cursor(|lines, cursor| {
            let suffix = {
                let line = &mut lines[cursor.row];
                let byte = Self::byte_offset(line, cursor.col);
                line.split_off(byte)
            };
            lines.insert(cursor.row + 1, suffix);
            cursor.row += 1;
            cursor.col = 0;
        });
    }

    fn delete_left(&mut self) {
        self.with_cursor(|lines, cursor| {
            if cursor.col > 0 {
                let line = &mut lines[cursor.row];
                let end = Self::byte_offset(line, cursor.col);
                let start = Self::byte_offset(line, cursor.col - 1);
                line.drain(start..end);
                cursor.col -= 1;
            } else if cursor.row > 0 {
                let removed = lines.remove(cursor.row);
                let prev_len = lines[cursor.row - 1].chars().count();
                lines[cursor.row - 1].push_str(&removed);
                cursor.row -= 1;
                cursor.col = prev_len;
            }
        });
    }

    fn delete_right(&mut self) {
        self.with_cursor(|lines, cursor| {
            let line_len = lines[cursor.row].chars().count();
            if cursor.col < line_len {
                let line = &mut lines[cursor.row];
                let start = Self::byte_offset(line, cursor.col);
                let end = Self::byte_offset(line, cursor.col + 1);
                line.drain(start..end);
            } else if cursor.row + 1 < lines.len() {
                let next = lines.remove(cursor.row + 1);
                lines[cursor.row].push_str(&next);
            }
        });
    }

    fn move_cursor_left(&mut self) {
        self.with_cursor(|lines, cursor| {
            if cursor.col > 0 {
                cursor.col -= 1;
            } else if cursor.row > 0 {
                cursor.row -= 1;
                cursor.col = lines[cursor.row].chars().count();
            }
        });
    }

    fn move_cursor_right(&mut self) {
        self.with_cursor(|lines, cursor| {
            let line_len = lines[cursor.row].chars().count();
            if cursor.col < line_len {
                cursor.col += 1;
            } else if cursor.row + 1 < lines.len() {
                cursor.row += 1;
                cursor.col = 0;
            }
        });
    }

    fn move_cursor_up(&mut self) {
        self.with_cursor(|_, cursor| {
            if cursor.row > 0 {
                cursor.row -= 1;
            }
        });
    }

    fn move_cursor_down(&mut self) {
        self.with_cursor(|lines, cursor| {
            if cursor.row + 1 < lines.len() {
                cursor.row += 1;
            }
        });
    }

    fn move_cursor_home(&mut self) {
        self.with_cursor(|_, cursor| cursor.col = 0);
    }

    fn move_cursor_end(&mut self) {
        self.with_cursor(|lines, cursor| cursor.col = lines[cursor.row].chars().count());
    }

    fn move_word_left(&mut self) {
        self.with_cursor(|lines, cursor| {
            if cursor.col == 0 {
                if cursor.row > 0 {
                    cursor.row -= 1;
                    cursor.col = lines[cursor.row].chars().count();
                }
                return;
            }
            let chars: Vec<char> = lines[cursor.row].chars().collect();
            let mut i = cursor.col.min(chars.len());
            while i > 0 && !is_word_char(chars[i - 1]) {
                i -= 1;
            }
            while i > 0 && is_word_char(chars[i - 1]) {
                i -= 1;
            }
            cursor.col = i;
        });
    }

    fn move_word_right(&mut self) {
        self.with_cursor(|lines, cursor| {
            let chars: Vec<char> = lines[cursor.row].chars().collect();
            let len = chars.len();
            if cursor.col == len {
                if cursor.row + 1 < lines.len() {
                    cursor.row += 1;
                    cursor.col = 0;
                }
                return;
            }
            let mut i = cursor.col.min(len);
            while i < len && !is_word_char(chars[i]) {
                i += 1;
            }
            while i < len && is_word_char(chars[i]) {
                i += 1;
            }
            cursor.col = i;
        });
    }

    fn delete_word_left(&mut self) {
        self.with_cursor(|lines, cursor| {
            if cursor.col == 0 {
                if cursor.row > 0 {
                    let removed = lines.remove(cursor.row);
                    let prev_len = lines[cursor.row - 1].chars().count();
                    lines[cursor.row - 1].push_str(&removed);
                    cursor.row -= 1;
                    cursor.col = prev_len;
                }
                return;
            }
            let chars: Vec<char> = lines[cursor.row].chars().collect();
            let mut i = cursor.col;
            while i > 0 && !is_word_char(chars[i - 1]) {
                i -= 1;
            }
            while i > 0 && is_word_char(chars[i - 1]) {
                i -= 1;
            }
            let line = &mut lines[cursor.row];
            let start = Self::byte_offset(line, i);
            let end = Self::byte_offset(line, cursor.col);
            line.drain(start..end);
            cursor.col = i;
        });
    }

    fn delete_word_right(&mut self) {
        self.with_cursor(|lines, cursor| {
            let chars: Vec<char> = lines[cursor.row].chars().collect();
            let len = chars.len();
            if cursor.col == len {
                if cursor.row + 1 < lines.len() {
                    let next = lines.remove(cursor.row + 1);
                    lines[cursor.row].push_str(&next);
                }
                return;
            }
            let mut i = cursor.col;
            while i < len && !is_word_char(chars[i]) {
                i += 1;
            }
            while i < len && is_word_char(chars[i]) {
                i += 1;
            }
            let line = &mut lines[cursor.row];
            let start = Self::byte_offset(line, cursor.col);
            let end = Self::byte_offset(line, i);
            line.drain(start..end);
        });
    }

    fn kill_to_start(&mut self) {
        self.with_cursor(|lines, cursor| {
            if cursor.col == 0 {
                return;
            }
            let line = &mut lines[cursor.row];
            let end = Self::byte_offset(line, cursor.col);
            line.drain(..end);
            cursor.col = 0;
        });
    }

    fn kill_to_end(&mut self) {
        self.with_cursor(|lines, cursor| {
            let line = &mut lines[cursor.row];
            let start = Self::byte_offset(line, cursor.col);
            line.drain(start..);
        });
    }

    fn clamp_scroll_for_cursor(&mut self, area_height: u16) {
        let Some(cursor) = self.editing else {
            return;
        };
        let visible = area_height.saturating_sub(2) as usize;
        if visible == 0 {
            return;
        }
        let current = self.scroll as usize;
        let row = cursor.row;
        let new_scroll = if row < current {
            row
        } else if row >= current + visible {
            row + 1 - visible
        } else {
            current
        };
        let max_scroll = self.lines.len().saturating_sub(visible);
        self.scroll = new_scroll.min(max_scroll) as u16;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardRectStatus {
    Unchanged,
    Editing,
    Modified,
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardField {
    RefreshIntervalMs,
    ShowPullRequests,
    WiseMerge,
    Columns,
    Ai,
}

impl DashboardField {
    pub const ALL: [DashboardField; 5] = [
        DashboardField::RefreshIntervalMs,
        DashboardField::ShowPullRequests,
        DashboardField::WiseMerge,
        DashboardField::Columns,
        DashboardField::Ai,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DashboardField::RefreshIntervalMs => "refreshIntervalMs",
            DashboardField::ShowPullRequests => "showPullRequests",
            DashboardField::WiseMerge => "wiseMerge",
            DashboardField::Columns => "columns",
            DashboardField::Ai => "ai",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            DashboardField::RefreshIntervalMs => "5000..60000 (ms)",
            DashboardField::ShowPullRequests => "Press Enter to toggle",
            DashboardField::WiseMerge => "Press Enter to toggle automatic merge of ready PRs",
            DashboardField::Columns => {
                "Comma-separated: branch, status, ai_status, ahead_behind, diff, last_commit, pull_request"
            }
            DashboardField::Ai => {
                "Press Enter to choose a model + thinking strength per AI command"
            }
        }
    }

    pub fn is_toggle(self) -> bool {
        matches!(
            self,
            DashboardField::ShowPullRequests | DashboardField::WiseMerge
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardSelection {
    Rect(usize),
    Save,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsMouseTarget {
    PatternListSave,
    PostCmdCreate,
    PostCmdSave,
    TerminalCmdSave,
    LinkStrategySave,
    LinkCacheDirSave,
    DashboardSave,
    AiSettingsSave,
    NotificationsSave,
    NotificationsToggle(usize),
    PathTemplateSave,
}

/// State for the inline dashboard settings editor surfaced when the user
/// drills into the `Dashboard` setting from the menu. Mirrors `PostCmdEditor`
/// but has a fixed list of rectangles (one per dashboard field) and no
/// Create/Delete affordances — the schema is closed.
pub struct DashboardEditor {
    base_config: DashboardConfig,
    pub values: Vec<String>,
    pub statuses: Vec<DashboardRectStatus>,
    /// Staged per-command AI models. The `ai` rectangle is a navigation entry
    /// into the AI Settings sub-screen, which edits this in place; it is
    /// persisted alongside the other dashboard fields by the Save button.
    pub ai: AiConfig,
    pub selection: DashboardSelection,
    edit_backup: Option<(String, DashboardRectStatus)>,
}

impl DashboardEditor {
    pub fn new(config: &DashboardConfig) -> Self {
        let values = vec![
            config.refresh_interval_ms.to_string(),
            config.show_pull_requests.to_string(),
            config.wise_merge.to_string(),
            config.columns.join(", "),
            // The `ai` rectangle no longer holds a single model — it opens the
            // AI Settings sub-screen — so its value slot stays empty and the
            // renderer shows a per-command summary derived from `ai` instead.
            String::new(),
        ];
        let statuses = vec![DashboardRectStatus::Saved; values.len()];
        Self {
            base_config: config.clone(),
            values,
            statuses,
            ai: config.ai.clone(),
            selection: DashboardSelection::Rect(0),
            edit_backup: None,
        }
    }

    pub fn field(&self, idx: usize) -> DashboardField {
        DashboardField::ALL[idx]
    }

    pub fn editing_index(&self) -> Option<usize> {
        self.statuses
            .iter()
            .position(|&s| s == DashboardRectStatus::Editing)
    }

    /// Build the `DashboardConfig` from current editor values. Numeric and
    /// column normalization happens here so invalid input falls back to the
    /// schema defaults rather than rejecting the save.
    pub fn build_config(&self) -> DashboardConfig {
        use crate::config::schema::{
            clamp_dashboard_refresh_interval, default_refresh_ms, normalize_dashboard_columns,
        };

        let refresh_interval_ms = self.values[0]
            .trim()
            .parse::<u64>()
            .map(clamp_dashboard_refresh_interval)
            .unwrap_or_else(|_| default_refresh_ms());

        let show_pull_requests = matches!(
            self.values[1].trim().to_ascii_lowercase().as_str(),
            "true" | "yes" | "1" | "on"
        );

        let wise_merge = matches!(
            self.values[2].trim().to_ascii_lowercase().as_str(),
            "true" | "yes" | "1" | "on"
        );

        let raw_columns: Vec<String> = self.values[3]
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let (columns, _warnings) = normalize_dashboard_columns(&raw_columns);

        let mut config = self.base_config.clone();
        config.refresh_interval_ms = refresh_interval_ms;
        config.show_pull_requests = show_pull_requests;
        config.wise_merge = wise_merge;
        config.columns = columns;
        // Per-command AI models are staged by the AI Settings sub-screen into
        // `self.ai`; thinking strengths are cleared for any blank model by
        // `normalize_ai` so we never persist an orphan reasoning level.
        config.ai = normalize_ai(&self.ai);
        config
    }
}

/// Drop the thinking strength of any per-command model whose `model` is blank
/// — a reasoning level only means something paired with a model. Mirrors the
/// old single-model behaviour, now applied per command.
fn normalize_ai(ai: &AiConfig) -> AiConfig {
    let clean = |m: &AiModelConfig| {
        let model = m.model.trim().to_string();
        let thinking = if model.is_empty() {
            String::new()
        } else {
            m.thinking.clone()
        };
        AiModelConfig { model, thinking }
    };
    AiConfig {
        enrich: clean(&ai.enrich),
        fix: AiFixConfig {
            plan: clean(&ai.fix.plan),
            apply: clean(&ai.fix.apply),
        },
        review: clean(&ai.review),
        update: clean(&ai.update),
        bugkill: AiBugkillConfig {
            investigate: clean(&ai.bugkill.investigate),
            fix: clean(&ai.bugkill.fix),
            judge: clean(&ai.bugkill.judge),
        },
    }
}

/// One AI-assisted command (or sub-step) configurable on the AI Settings
/// sub-screen. Order matches the rectangles top-to-bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiSlot {
    Enrich,
    FixPlan,
    FixApply,
    Review,
    Update,
    BugkillInvestigate,
    BugkillFix,
    BugkillJudge,
}

impl AiSlot {
    pub const ALL: [AiSlot; 8] = [
        AiSlot::Enrich,
        AiSlot::FixPlan,
        AiSlot::FixApply,
        AiSlot::Review,
        AiSlot::Update,
        AiSlot::BugkillInvestigate,
        AiSlot::BugkillFix,
        AiSlot::BugkillJudge,
    ];

    fn label(self) -> &'static str {
        match self {
            AiSlot::Enrich => "enrich",
            AiSlot::FixPlan => "fix_plan",
            AiSlot::FixApply => "fix_apply",
            AiSlot::Review => "review",
            AiSlot::Update => "update",
            AiSlot::BugkillInvestigate => "bugkill_investigate",
            AiSlot::BugkillFix => "bugkill_fix",
            AiSlot::BugkillJudge => "bugkill_judge",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            AiSlot::Enrich => "Drafts the PR title + description (Enrich)",
            AiSlot::FixPlan => "Plans review-comment fixes — pick a stronger model (Fix · plan)",
            AiSlot::FixApply => "Applies the approved fix live (Fix · apply)",
            AiSlot::Review => {
                "Scans each changed file and drafts review comments — pick a stronger model \
                 (Review)"
            }
            AiSlot::Update => "Resolves merge conflicts (Update Pull Request / branch)",
            AiSlot::BugkillInvestigate => {
                "Investigates the bug and ranks root causes — pick a stronger model \
                 (Bugkill · investigate)"
            }
            AiSlot::BugkillFix => "Applies the selected bug fix live (Bugkill · fix)",
            AiSlot::BugkillJudge => {
                "Judges a freeform \"Other\" answer — fixed or not (Bugkill · judge)"
            }
        }
    }

    fn get(self, ai: &AiConfig) -> &AiModelConfig {
        match self {
            AiSlot::Enrich => &ai.enrich,
            AiSlot::FixPlan => &ai.fix.plan,
            AiSlot::FixApply => &ai.fix.apply,
            AiSlot::Review => &ai.review,
            AiSlot::Update => &ai.update,
            AiSlot::BugkillInvestigate => &ai.bugkill.investigate,
            AiSlot::BugkillFix => &ai.bugkill.fix,
            AiSlot::BugkillJudge => &ai.bugkill.judge,
        }
    }

    fn get_mut(self, ai: &mut AiConfig) -> &mut AiModelConfig {
        match self {
            AiSlot::Enrich => &mut ai.enrich,
            AiSlot::FixPlan => &mut ai.fix.plan,
            AiSlot::FixApply => &mut ai.fix.apply,
            AiSlot::Review => &mut ai.review,
            AiSlot::Update => &mut ai.update,
            AiSlot::BugkillInvestigate => &mut ai.bugkill.investigate,
            AiSlot::BugkillFix => &mut ai.bugkill.fix,
            AiSlot::BugkillJudge => &mut ai.bugkill.judge,
        }
    }
}

/// The eight leaf models in slot order — used by the dashboard `ai` summary and
/// the AI Settings editor.
fn ai_slot_models(ai: &AiConfig) -> [&AiModelConfig; 8] {
    [
        &ai.enrich,
        &ai.fix.plan,
        &ai.fix.apply,
        &ai.review,
        &ai.update,
        &ai.bugkill.investigate,
        &ai.bugkill.fix,
        &ai.bugkill.judge,
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiSettingsSelection {
    Rect(usize),
    /// Cursor on the shared "Current free models:" chip row below the slots.
    /// Inner `usize` is the focused chip; Enter stages it into the
    /// most-recently-focused slot. Only reachable with a non-empty
    /// `free_models` list cached.
    FreeModels(usize),
    Save,
}

/// Editor backing the AI Settings sub-screen: one rectangle per AI command,
/// each opening the model picker (Enter) and cycling its thinking strength
/// (←/→), plus a shared free-model quick-pick row and a Save button. Mirrors
/// [`DashboardEditor`] but is scoped to the nested per-command `ai` config.
pub struct AiSettingsEditor {
    /// Snapshot taken on open, so Save can mark a clean baseline and Esc can
    /// tell whether anything was staged.
    base_ai: AiConfig,
    pub ai: AiConfig,
    pub statuses: Vec<DashboardRectStatus>,
    pub selection: AiSettingsSelection,
    /// Slot the chip row applies to — the rectangle the cursor was last on.
    last_rect: usize,
}

impl AiSettingsEditor {
    pub fn new(ai: &AiConfig) -> Self {
        Self {
            base_ai: ai.clone(),
            ai: ai.clone(),
            statuses: vec![DashboardRectStatus::Saved; AiSlot::ALL.len()],
            selection: AiSettingsSelection::Rect(0),
            last_rect: 0,
        }
    }

    fn slot(idx: usize) -> AiSlot {
        AiSlot::ALL[idx]
    }

    /// Index of the slot the picker/chip row should target.
    fn target_idx(&self) -> usize {
        match self.selection {
            AiSettingsSelection::Rect(i) => i,
            _ => self.last_rect,
        }
    }

    /// Model + thinking for the targeted slot, used to pre-fill the picker.
    pub fn focused_model(&self) -> AiModelConfig {
        Self::slot(self.target_idx()).get(&self.ai).clone()
    }

    /// Stamp a picked model + thinking into the targeted slot (Modified, not
    /// Saved — persisted via the Save button).
    pub fn apply_selection(&mut self, model: String, thinking: String) {
        let idx = self.target_idx();
        let target = Self::slot(idx).get_mut(&mut self.ai);
        target.model = model;
        target.thinking = thinking;
        self.statuses[idx] = DashboardRectStatus::Modified;
        self.selection = AiSettingsSelection::Rect(idx);
        self.last_rect = idx;
    }

    /// Stage a chip-row free model into the most-recently-focused slot,
    /// clearing its thinking (free models reset to Default).
    fn apply_free_model(&mut self, pair: String) {
        let idx = self.last_rect.min(AiSlot::ALL.len() - 1);
        let target = Self::slot(idx).get_mut(&mut self.ai);
        target.model = pair;
        target.thinking.clear();
        self.statuses[idx] = DashboardRectStatus::Modified;
    }

    fn mark_saved(&mut self) {
        for status in &mut self.statuses {
            *status = DashboardRectStatus::Saved;
        }
        self.base_ai = normalize_ai(&self.ai);
        self.ai = self.base_ai.clone();
    }

    /// The normalized config to persist / stage back into the dashboard editor.
    fn build_ai(&self) -> AiConfig {
        normalize_ai(&self.ai)
    }
}

/// Opt-in terminal-bell toggles shown on the standalone Notifications screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationsField {
    AiStatusOk,
    PrChecksOk,
}

impl NotificationsField {
    pub const ALL: [NotificationsField; 2] = [
        NotificationsField::AiStatusOk,
        NotificationsField::PrChecksOk,
    ];

    pub fn label(self) -> &'static str {
        match self {
            NotificationsField::AiStatusOk => "aiStatusOk",
            NotificationsField::PrChecksOk => "prChecksOk",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            NotificationsField::AiStatusOk => {
                "Press Enter to toggle terminal bell when AI work finishes"
            }
            NotificationsField::PrChecksOk => {
                "Press Enter to toggle terminal bell when PR checks pass"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationsSelection {
    Rect(usize),
    Save,
}

/// Editor backing the Notifications settings screen. Each field is a plain
/// boolean toggle plus a Save button — no free-text editing, so the state is
/// just the current values and their per-rectangle save status.
pub struct NotificationsEditor {
    pub values: Vec<bool>,
    pub statuses: Vec<DashboardRectStatus>,
    pub selection: NotificationsSelection,
}

impl NotificationsEditor {
    pub fn new(config: &NotificationsConfig) -> Self {
        let values = vec![config.ai_status_ok, config.pr_checks_ok];
        let statuses = vec![DashboardRectStatus::Saved; values.len()];
        Self {
            values,
            statuses,
            selection: NotificationsSelection::Rect(0),
        }
    }

    pub fn field(&self, idx: usize) -> NotificationsField {
        NotificationsField::ALL[idx]
    }

    pub fn toggle(&mut self, idx: usize) {
        self.values[idx] = !self.values[idx];
        self.statuses[idx] = DashboardRectStatus::Modified;
    }

    pub fn build_config(&self) -> NotificationsConfig {
        NotificationsConfig {
            ai_status_ok: self.values[0],
            pr_checks_ok: self.values[1],
        }
    }

    fn step_up(&mut self) {
        self.selection = match self.selection {
            NotificationsSelection::Rect(0) => NotificationsSelection::Rect(0),
            NotificationsSelection::Rect(i) => NotificationsSelection::Rect(i - 1),
            NotificationsSelection::Save => NotificationsSelection::Rect(self.values.len() - 1),
        };
    }

    fn step_down(&mut self) {
        self.selection = match self.selection {
            NotificationsSelection::Rect(i) if i + 1 < self.values.len() => {
                NotificationsSelection::Rect(i + 1)
            }
            NotificationsSelection::Rect(_) => NotificationsSelection::Save,
            NotificationsSelection::Save => NotificationsSelection::Save,
        };
    }
}

pub struct SettingsScreen {
    step: SettingsStep,
    config: WorktreeConfig,
    config_path: String,
    global_config_path: Option<String>,
    /// Optional path of the project-local config the post-create commands
    /// editor will write to. Shown to the user while they edit.
    local_config_path: Option<String>,
    /// When true, the settings menu shows a "Setup Project Config" entry at
    /// the top — visible only when no project-local config exists yet.
    has_setup_project: bool,
    error: Option<String>,
    select: Option<SelectPrompt<SettingsStep>>,
    delete_branch_dialog: Option<ConfirmationModal>,
    pattern_list_editor: Option<PatternListEditor>,
    post_cmd_editor: Option<PostCmdEditor>,
    post_cmd_input: Option<InputPrompt>,
    terminal_cmd_editor: Option<TerminalCmdEditor>,
    terminal_cmd_input: Option<InputPrompt>,
    path_template_editor: Option<PathTemplateEditor>,
    path_template_input: Option<InputPrompt>,
    link_strategy_editor: Option<LinkStrategyEditor>,
    link_cache_dir_editor: Option<LinkCacheDirEditor>,
    link_cache_dir_input: Option<InputPrompt>,
    dashboard_editor: Option<DashboardEditor>,
    dashboard_input: Option<InputPrompt>,
    /// Per-command AI model editor, alive only while on the AI Settings
    /// sub-screen (reached from the Dashboard `ai` rectangle).
    ai_settings_editor: Option<AiSettingsEditor>,
    notifications_editor: Option<NotificationsEditor>,
    /// Result of the background `opencode models opencode` fetch surfaced
    /// inline under the AI Settings slots. `None` while the request is in
    /// flight; `Some(Ok(_))` after a successful list, `Some(Err(_))` to
    /// render the failure message instead of chips. The cursor lives in
    /// `AiSettingsSelection::FreeModels(i)` — there is no separate focus field.
    free_models: Option<Result<Vec<String>, String>>,
    /// `provider/model` → authoritative reasoning variants (weakest→strongest),
    /// harvested from `opencode models --verbose`. Drives each AI slot's ←/→
    /// reasoning cycle so it only steps through the levels the chosen model
    /// actually accepts. `None` until the background fetch lands; a model
    /// missing from the map falls back to the generic ladder.
    ai_model_variants: Option<std::collections::HashMap<String, Vec<String>>>,
    copy_settings_select: Option<SelectPrompt<CopyDirection>>,
    update_result: Option<MultiSourceUpdateResult>,
    checking_updates: bool,
    /// Which source rectangle is currently highlighted in the
    /// "Check for Updates" screen.
    update_selection: UpdateSource,
    /// Source currently being upgraded — `Some` shows a spinner instead of
    /// the rectangles. Cleared once `set_upgrade_result` is called.
    upgrading_source: Option<UpdateSource>,
    /// Result of the last upgrade attempt. Rendered as a success / error
    /// line above the rectangles when present.
    upgrade_outcome: Option<UpgradeOutcome>,
    mouse_targets: RefCell<Vec<(SettingsMouseTarget, Rect)>>,
    pub tick: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeOutcome {
    pub source: UpdateSource,
    pub success: bool,
    pub message: String,
}

impl SettingsScreen {
    pub fn new(config: WorktreeConfig, config_path: String) -> Self {
        let mut s = Self {
            step: SettingsStep::Menu,
            config,
            config_path,
            global_config_path: None,
            local_config_path: None,
            has_setup_project: false,
            error: None,
            select: None,
            delete_branch_dialog: None,
            pattern_list_editor: None,
            post_cmd_editor: None,
            post_cmd_input: None,
            terminal_cmd_editor: None,
            terminal_cmd_input: None,
            path_template_editor: None,
            path_template_input: None,
            link_strategy_editor: None,
            link_cache_dir_editor: None,
            link_cache_dir_input: None,
            dashboard_editor: None,
            dashboard_input: None,
            ai_settings_editor: None,
            notifications_editor: None,
            free_models: None,
            ai_model_variants: None,
            copy_settings_select: None,
            update_result: None,
            checking_updates: false,
            update_selection: UpdateSource::Npm,
            upgrading_source: None,
            upgrade_outcome: None,
            mouse_targets: RefCell::new(Vec::new()),
            tick: 0,
        };
        s.select = Some(s.build_menu());
        s
    }

    /// Configure the path the post-create commands editor will save to.
    /// Stored verbatim and surfaced in the editor footer.
    pub fn with_local_config_path(mut self, path: Option<String>) -> Self {
        self.local_config_path = path;
        self
    }

    /// Show or hide the "Setup Project Config" entry at the top of the menu.
    /// Pass `true` when no project-local config exists yet and a git root is
    /// in scope.
    pub fn with_has_setup_project(mut self, value: bool) -> Self {
        self.has_setup_project = value;
        self.select = Some(self.build_menu());
        self
    }

    pub fn with_global_config_path(mut self, path: String) -> Self {
        self.global_config_path = Some(path);
        self
    }

    pub fn local_config_path(&self) -> Option<&str> {
        self.local_config_path.as_deref()
    }

    pub fn post_cmd_editor(&self) -> Option<&PostCmdEditor> {
        self.post_cmd_editor.as_ref()
    }

    pub fn pattern_list_editor(&self) -> Option<&PatternListEditor> {
        self.pattern_list_editor.as_ref()
    }

    pub fn terminal_cmd_editor(&self) -> Option<&TerminalCmdEditor> {
        self.terminal_cmd_editor.as_ref()
    }

    pub fn path_template_editor(&self) -> Option<&PathTemplateEditor> {
        self.path_template_editor.as_ref()
    }

    pub fn link_strategy_editor(&self) -> Option<&LinkStrategyEditor> {
        self.link_strategy_editor.as_ref()
    }

    pub fn link_cache_dir_editor(&self) -> Option<&LinkCacheDirEditor> {
        self.link_cache_dir_editor.as_ref()
    }

    pub fn dashboard_editor(&self) -> Option<&DashboardEditor> {
        self.dashboard_editor.as_ref()
    }

    pub fn step(&self) -> SettingsStep {
        self.step
    }

    pub fn config(&self) -> &WorktreeConfig {
        &self.config
    }

    pub fn config_path(&self) -> &str {
        &self.config_path
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn checking_updates(&self) -> bool {
        self.checking_updates
    }

    pub fn update_result(&self) -> Option<&MultiSourceUpdateResult> {
        self.update_result.as_ref()
    }

    pub fn update_selection(&self) -> UpdateSource {
        self.update_selection
    }

    pub fn upgrading_source(&self) -> Option<UpdateSource> {
        self.upgrading_source
    }

    pub fn upgrade_outcome(&self) -> Option<&UpgradeOutcome> {
        self.upgrade_outcome.as_ref()
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
    }

    pub fn set_config(&mut self, config: WorktreeConfig, config_path: String) {
        self.config = config;
        self.config_path = config_path;
        self.step = SettingsStep::Menu;
        self.select = Some(self.build_menu());
        self.delete_branch_dialog = None;
        self.pattern_list_editor = None;
        self.post_cmd_editor = None;
        self.post_cmd_input = None;
        self.terminal_cmd_editor = None;
        self.terminal_cmd_input = None;
        self.path_template_editor = None;
        self.path_template_input = None;
        self.link_strategy_editor = None;
        self.link_cache_dir_editor = None;
        self.link_cache_dir_input = None;
        self.dashboard_editor = None;
        self.dashboard_input = None;
        self.notifications_editor = None;
        self.free_models = None;
        self.copy_settings_select = None;
        self.error = None;
    }

    /// Mirror a successful save back into the settings menu.
    pub fn mark_post_create_commands_saved(&mut self, commands: Vec<String>) {
        self.config.post_create_cmd = commands;
        self.select = Some(self.build_menu());
        self.post_cmd_editor = None;
        self.post_cmd_input = None;
        self.step = SettingsStep::Menu;
    }

    pub fn mark_copy_patterns_saved(&mut self, patterns: Vec<String>) {
        self.config.worktree_copy_patterns = patterns;
        self.select = Some(self.build_menu());
        self.pattern_list_editor = None;
        self.step = SettingsStep::Menu;
    }

    pub fn mark_ignore_patterns_saved(&mut self, patterns: Vec<String>) {
        self.config.worktree_copy_ignores = patterns;
        self.select = Some(self.build_menu());
        self.pattern_list_editor = None;
        self.step = SettingsStep::Menu;
    }

    pub fn mark_link_patterns_saved(&mut self, patterns: Vec<String>) {
        self.config.worktree_link_patterns = patterns;
        self.select = Some(self.build_menu());
        self.pattern_list_editor = None;
        self.step = SettingsStep::Menu;
    }

    /// Mirror a successful terminal command save back into the settings menu.
    pub fn mark_terminal_command_saved(&mut self, command: String) {
        self.config.terminal_command = command;
        self.select = Some(self.build_menu());
        self.terminal_cmd_editor = None;
        self.terminal_cmd_input = None;
        self.step = SettingsStep::Menu;
    }

    /// Mirror a successful path template save back into the settings menu.
    pub fn mark_path_template_saved(&mut self, template: String) {
        self.config.worktree_path_template = template;
        self.select = Some(self.build_menu());
        self.path_template_editor = None;
        self.path_template_input = None;
        self.step = SettingsStep::Menu;
    }

    pub fn mark_link_strategy_saved(&mut self, strategy: LinkStrategy) {
        self.config.worktree_link_strategy = strategy;
        self.select = Some(self.build_menu());
        self.link_strategy_editor = None;
        self.step = SettingsStep::Menu;
    }

    pub fn mark_link_cache_dir_saved(&mut self, cache_dir: Option<String>) {
        self.config.worktree_link_cache_dir = cache_dir;
        self.select = Some(self.build_menu());
        self.link_cache_dir_editor = None;
        self.link_cache_dir_input = None;
        self.step = SettingsStep::Menu;
    }

    /// Mirror a successful dashboard save back into the settings menu.
    pub fn mark_dashboard_saved(&mut self, dashboard: DashboardConfig) {
        self.config.dashboard = dashboard;
        self.select = Some(self.build_menu());
        self.dashboard_editor = None;
        self.dashboard_input = None;
        self.free_models = None;
        self.step = SettingsStep::Menu;
    }

    /// Mirror a successful notifications save back into the settings menu.
    pub fn mark_notifications_saved(&mut self, notifications: NotificationsConfig) {
        self.config.notifications = notifications;
        self.select = Some(self.build_menu());
        self.notifications_editor = None;
        self.step = SettingsStep::Menu;
    }

    /// Surface the successful free-model list fetched by the background
    /// `opencode models opencode` shell-out. The chip row becomes navigable
    /// next time the user steps Down from the ai rectangle.
    pub fn set_free_models(&mut self, models: Vec<String>) {
        self.free_models = Some(Ok(models));
    }

    /// Surface a failure from the free-model fetch — rendered inline under
    /// the `ai` rectangle where the chips would otherwise appear. If the
    /// cursor happened to be on the chip row when the fetch failed (rare,
    /// since the chips never showed up to land on), step the cursor back
    /// onto the ai rectangle so the user isn't stranded.
    pub fn set_free_models_error(&mut self, message: String) {
        self.free_models = Some(Err(message));
        if let Some(editor) = self.ai_settings_editor.as_mut() {
            if let AiSettingsSelection::FreeModels(_) = editor.selection {
                editor.selection = AiSettingsSelection::Rect(editor.last_rect);
            }
        }
    }

    /// Test-only accessor for the cached free-model list.
    #[cfg(test)]
    pub fn free_models(&self) -> Option<&Result<Vec<String>, String>> {
        self.free_models.as_ref()
    }

    /// Cache the `provider/model` → reasoning-variant map fetched in the
    /// background, so the `ai` ←/→ cycle becomes model-aware.
    pub fn set_ai_model_variants(
        &mut self,
        variants: std::collections::HashMap<String, Vec<String>>,
    ) {
        self.ai_model_variants = Some(variants);
    }

    /// The reasoning ladder (weakest→strongest, excluding "Default") to cycle
    /// for `pair`. Prefers the authoritative set from the local CLI — which may
    /// legitimately be empty, meaning the model accepts no reasoning override —
    /// and falls back to the generic ladder for models the CLI doesn't know (or
    /// before the fetch lands).
    fn ai_thinking_ladder(&self, pair: &str) -> Vec<String> {
        if let Some(map) = &self.ai_model_variants {
            if let Some(variants) = map.get(pair) {
                return variants.clone();
            }
        }
        REASONING_VARIANTS.iter().map(|s| s.to_string()).collect()
    }

    /// Stamp the picked model + thinking strength into the focused slot of the
    /// AI Settings sub-screen. The slot is marked `Modified` (not `Saved`) — the
    /// user persists it via the Save button, matching the edit-then-Save model
    /// of every other settings page. Leaves the editor on screen.
    pub fn apply_ai_selection(&mut self, model: String, thinking: String) {
        if let Some(editor) = self.ai_settings_editor.as_mut() {
            editor.apply_selection(model, thinking);
        }
    }

    pub fn start_checking_updates(&mut self) {
        self.checking_updates = true;
        self.update_result = None;
        self.upgrade_outcome = None;
        self.upgrading_source = None;
        self.update_selection = UpdateSource::Npm;
    }

    pub fn set_update_result(&mut self, result: MultiSourceUpdateResult) {
        self.update_result = Some(result);
        self.checking_updates = false;
    }

    /// Mark the start of an in-flight `<source>` upgrade so the screen can
    /// swap to a spinner while the subprocess is running.
    pub fn start_upgrade(&mut self, source: UpdateSource) {
        self.upgrading_source = Some(source);
        self.upgrade_outcome = None;
    }

    /// Surface the result of an upgrade attempt. Caller decides whether to
    /// also pop the user back to the settings menu.
    pub fn set_upgrade_outcome(&mut self, outcome: UpgradeOutcome) {
        self.upgrading_source = None;
        self.upgrade_outcome = Some(outcome);
    }

    fn build_menu(&self) -> SelectPrompt<SettingsStep> {
        let mut opts: Vec<SelectOption<SettingsStep>> = Vec::new();
        if self.has_setup_project {
            opts.push(
                SelectOption::new("Setup Project Config", SettingsStep::SetupProject)
                    .with_description("Initialize project-local config"),
            );
        }
        opts.extend([
            SelectOption::new("Dashboard", SettingsStep::Dashboard).with_description(format!(
                "{}ms refresh, {} columns",
                self.config.dashboard.refresh_interval_ms,
                self.config.dashboard.columns.len()
            )),
            SelectOption::new("Notifications", SettingsStep::Notifications)
                .with_description(notifications_menu_description(&self.config.notifications)),
            SelectOption::new("Copy Patterns", SettingsStep::CopyPatterns).with_description(
                format!("{} patterns", self.config.worktree_copy_patterns.len()),
            ),
            SelectOption::new("Ignore Patterns", SettingsStep::IgnorePatterns).with_description(
                format!("{} patterns", self.config.worktree_copy_ignores.len()),
            ),
            SelectOption::new("Link Patterns", SettingsStep::LinkPatterns).with_description(
                format!("{} patterns", self.config.worktree_link_patterns.len()),
            ),
            SelectOption::new("Link Strategy", SettingsStep::LinkStrategy)
                .with_description(link_strategy_label(self.config.worktree_link_strategy)),
            SelectOption::new("Link Cache Dir", SettingsStep::LinkCacheDir).with_description(
                self.config
                    .worktree_link_cache_dir
                    .clone()
                    .unwrap_or_else(|| "(default)".to_string()),
            ),
            SelectOption::new("Post-Create Commands", SettingsStep::PostCmd)
                .with_description(format!("{} commands", self.config.post_create_cmd.len())),
            SelectOption::new("Terminal Command", SettingsStep::TerminalCmd).with_description(
                if self.config.terminal_command.is_empty() {
                    "(none)".to_string()
                } else {
                    self.config.terminal_command.clone()
                },
            ),
            SelectOption::new("Path Template", SettingsStep::PathTemplate)
                .with_description(self.config.worktree_path_template.clone()),
            SelectOption::new("Copy Settings", SettingsStep::CopySettings)
                .with_description("Sync global and local config"),
            SelectOption::new("Delete Branch with Worktree", SettingsStep::DeleteBranch)
                .with_description(if self.config.delete_branch_with_worktree {
                    "enabled"
                } else {
                    "disabled"
                }),
            SelectOption::new(UPDATE_CHECK_MENU, SettingsStep::CheckUpdates)
                .with_description("Check npm for latest version"),
        ]);
        SelectPrompt::new("Select setting to view:", opts)
            .searchable()
            .with_footer_spacer()
    }

    fn build_copy_settings_select(&self) -> SelectPrompt<CopyDirection> {
        SelectPrompt::new(
            "Choose copy direction:",
            vec![
                SelectOption::new("global → local", CopyDirection::GlobalToLocal)
                    .with_description("Overwrite/create the local config from global"),
                SelectOption::new("local → global", CopyDirection::LocalToGlobal)
                    .with_description("Overwrite/create the global config from local"),
            ],
        )
        .with_footer_spacer()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SettingsAction {
        if self.error.is_some() {
            // Press 'r' to reset, anything else clears the error and goes back.
            if let KeyCode::Char(c) = key.code {
                if c.eq_ignore_ascii_case(&'r') {
                    return SettingsAction::Reset;
                }
            }
            self.error = None;
            return SettingsAction::Back;
        }
        match self.step {
            SettingsStep::Menu => self.handle_menu(key),
            SettingsStep::SetupProject => SettingsAction::OpenSetupProject,
            SettingsStep::CopyPatterns
            | SettingsStep::LinkPatterns
            | SettingsStep::IgnorePatterns => self.handle_pattern_list(key),
            SettingsStep::DeleteBranch => self.handle_delete_branch(key),
            SettingsStep::CopySettings => self.handle_copy_settings(key),
            SettingsStep::CheckUpdates => self.handle_check_updates(key),
            SettingsStep::PostCmd => self.handle_post_cmd(key),
            SettingsStep::TerminalCmd => self.handle_terminal_cmd(key),
            SettingsStep::LinkStrategy => self.handle_link_strategy(key),
            SettingsStep::LinkCacheDir => self.handle_link_cache_dir(key),
            SettingsStep::PathTemplate => self.handle_path_template(key),
            SettingsStep::Dashboard => self.handle_dashboard(key),
            SettingsStep::AiSettings => self.handle_ai_settings(key),
            SettingsStep::Notifications => self.handle_notifications(key),
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> SettingsAction {
        if self.error.is_some() {
            return SettingsAction::Continue;
        }

        match self.step {
            SettingsStep::Menu => {
                let select = match self.select.as_mut() {
                    Some(select) => select,
                    None => return SettingsAction::Back,
                };
                match select.handle_mouse_click(position) {
                    SelectOutcome::Selected(_, value) => {
                        self.step = value;
                        if matches!(value, SettingsStep::SetupProject) {
                            return SettingsAction::OpenSetupProject;
                        }
                        if matches!(value, SettingsStep::CheckUpdates) {
                            return SettingsAction::CheckUpdates;
                        }
                        if matches!(value, SettingsStep::DeleteBranch) {
                            self.delete_branch_dialog = Some(self.build_delete_branch_dialog());
                        }
                        if matches!(value, SettingsStep::PostCmd) {
                            self.post_cmd_editor =
                                Some(PostCmdEditor::new(self.config.post_create_cmd.clone()));
                        }
                        if matches!(value, SettingsStep::CopyPatterns) {
                            self.pattern_list_editor = Some(PatternListEditor::new(
                                self.config.worktree_copy_patterns.clone(),
                            ));
                        }
                        if matches!(value, SettingsStep::IgnorePatterns) {
                            self.pattern_list_editor = Some(PatternListEditor::new(
                                self.config.worktree_copy_ignores.clone(),
                            ));
                        }
                        if matches!(value, SettingsStep::LinkPatterns) {
                            self.pattern_list_editor = Some(PatternListEditor::new(
                                self.config.worktree_link_patterns.clone(),
                            ));
                        }
                        if matches!(value, SettingsStep::TerminalCmd) {
                            self.terminal_cmd_editor =
                                Some(TerminalCmdEditor::new(self.config.terminal_command.clone()));
                        }
                        if matches!(value, SettingsStep::LinkStrategy) {
                            self.link_strategy_editor =
                                Some(LinkStrategyEditor::new(self.config.worktree_link_strategy));
                        }
                        if matches!(value, SettingsStep::LinkCacheDir) {
                            self.link_cache_dir_editor = Some(LinkCacheDirEditor::new(
                                self.config.worktree_link_cache_dir.clone(),
                            ));
                        }
                        if matches!(value, SettingsStep::PathTemplate) {
                            self.path_template_editor = Some(PathTemplateEditor::new(
                                self.config.worktree_path_template.clone(),
                            ));
                        }
                        if matches!(value, SettingsStep::Dashboard) {
                            self.dashboard_editor =
                                Some(DashboardEditor::new(&self.config.dashboard));
                        }
                        if matches!(value, SettingsStep::Notifications) {
                            self.notifications_editor =
                                Some(NotificationsEditor::new(&self.config.notifications));
                        }
                        if matches!(value, SettingsStep::CopySettings) {
                            self.copy_settings_select = Some(self.build_copy_settings_select());
                        }
                        SettingsAction::Continue
                    }
                    SelectOutcome::Cancelled | SelectOutcome::Pending => SettingsAction::Continue,
                }
            }
            SettingsStep::CopySettings => {
                let select = match self.copy_settings_select.as_mut() {
                    Some(select) => select,
                    None => {
                        self.step = SettingsStep::Menu;
                        return SettingsAction::Continue;
                    }
                };
                match select.handle_mouse_click(position) {
                    SelectOutcome::Selected(_, direction) => {
                        SettingsAction::CopySettings(direction)
                    }
                    SelectOutcome::Cancelled | SelectOutcome::Pending => SettingsAction::Continue,
                }
            }
            SettingsStep::DeleteBranch => {
                if self.delete_branch_dialog.is_none() {
                    self.delete_branch_dialog = Some(self.build_delete_branch_dialog());
                }
                let Some(dialog) = self.delete_branch_dialog.as_mut() else {
                    return SettingsAction::Continue;
                };
                match dialog.handle_mouse_click(position) {
                    ConfirmationOutcome::Confirmed => {
                        SettingsAction::SetDeleteBranchWithWorktree(true)
                    }
                    ConfirmationOutcome::Declined => {
                        SettingsAction::SetDeleteBranchWithWorktree(false)
                    }
                    ConfirmationOutcome::Cancelled | ConfirmationOutcome::Pending => {
                        SettingsAction::Continue
                    }
                }
            }
            _ => self.handle_custom_button_click(position),
        }
    }

    fn handle_custom_button_click(&mut self, position: Position) -> SettingsAction {
        let Some((target, _)) = self
            .mouse_targets
            .borrow()
            .iter()
            .copied()
            .find(|(_, rect)| contains_position(*rect, position))
        else {
            return SettingsAction::Continue;
        };

        let enter = KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
        match target {
            SettingsMouseTarget::PatternListSave => {
                if let Some(editor) = self.pattern_list_editor.as_mut() {
                    editor.selection = PatternListSelection::Save;
                }
                self.handle_pattern_list(enter)
            }
            SettingsMouseTarget::PostCmdCreate => {
                if let Some(editor) = self.post_cmd_editor.as_mut() {
                    editor.selection = PostCmdSelection::Create;
                }
                self.handle_post_cmd(enter)
            }
            SettingsMouseTarget::PostCmdSave => {
                if let Some(editor) = self.post_cmd_editor.as_mut() {
                    editor.selection = PostCmdSelection::Save;
                }
                self.handle_post_cmd(enter)
            }
            SettingsMouseTarget::TerminalCmdSave => {
                if let Some(editor) = self.terminal_cmd_editor.as_mut() {
                    editor.selection = TerminalCmdSelection::Save;
                }
                self.handle_terminal_cmd(enter)
            }
            SettingsMouseTarget::LinkStrategySave => {
                if let Some(editor) = self.link_strategy_editor.as_mut() {
                    editor.selection = LinkStrategySelection::Save;
                }
                self.handle_link_strategy(enter)
            }
            SettingsMouseTarget::LinkCacheDirSave => {
                if let Some(editor) = self.link_cache_dir_editor.as_mut() {
                    editor.selection = LinkCacheDirSelection::Save;
                }
                self.handle_link_cache_dir(enter)
            }
            SettingsMouseTarget::DashboardSave => {
                if let Some(editor) = self.dashboard_editor.as_mut() {
                    editor.selection = DashboardSelection::Save;
                }
                self.handle_dashboard(enter)
            }
            SettingsMouseTarget::AiSettingsSave => {
                if let Some(editor) = self.ai_settings_editor.as_mut() {
                    editor.selection = AiSettingsSelection::Save;
                }
                self.handle_ai_settings(enter)
            }
            SettingsMouseTarget::NotificationsSave => {
                if let Some(editor) = self.notifications_editor.as_mut() {
                    editor.selection = NotificationsSelection::Save;
                }
                self.handle_notifications(enter)
            }
            SettingsMouseTarget::NotificationsToggle(idx) => {
                if let Some(editor) = self.notifications_editor.as_mut() {
                    editor.selection = NotificationsSelection::Rect(idx);
                }
                self.handle_notifications(enter)
            }
            SettingsMouseTarget::PathTemplateSave => {
                if let Some(editor) = self.path_template_editor.as_mut() {
                    editor.selection = PathTemplateSelection::Save;
                }
                self.handle_path_template(enter)
            }
        }
    }

    fn push_mouse_target(&self, target: SettingsMouseTarget, rect: Rect) {
        self.mouse_targets.borrow_mut().push((target, rect));
    }

    fn handle_menu(&mut self, key: KeyEvent) -> SettingsAction {
        let select = match self.select.as_mut() {
            Some(s) => s,
            None => return SettingsAction::Back,
        };
        match select.handle_key(key) {
            SelectOutcome::Selected(_, value) => {
                self.step = value;
                if matches!(value, SettingsStep::SetupProject) {
                    return SettingsAction::OpenSetupProject;
                }
                if matches!(value, SettingsStep::CheckUpdates) {
                    return SettingsAction::CheckUpdates;
                }
                if matches!(value, SettingsStep::DeleteBranch) {
                    self.delete_branch_dialog = Some(self.build_delete_branch_dialog());
                }
                if matches!(value, SettingsStep::PostCmd) {
                    self.post_cmd_editor =
                        Some(PostCmdEditor::new(self.config.post_create_cmd.clone()));
                }
                if matches!(value, SettingsStep::CopyPatterns) {
                    self.pattern_list_editor = Some(PatternListEditor::new(
                        self.config.worktree_copy_patterns.clone(),
                    ));
                }
                if matches!(value, SettingsStep::IgnorePatterns) {
                    self.pattern_list_editor = Some(PatternListEditor::new(
                        self.config.worktree_copy_ignores.clone(),
                    ));
                }
                if matches!(value, SettingsStep::LinkPatterns) {
                    self.pattern_list_editor = Some(PatternListEditor::new(
                        self.config.worktree_link_patterns.clone(),
                    ));
                }
                if matches!(value, SettingsStep::TerminalCmd) {
                    self.terminal_cmd_editor =
                        Some(TerminalCmdEditor::new(self.config.terminal_command.clone()));
                }
                if matches!(value, SettingsStep::LinkStrategy) {
                    self.link_strategy_editor =
                        Some(LinkStrategyEditor::new(self.config.worktree_link_strategy));
                }
                if matches!(value, SettingsStep::LinkCacheDir) {
                    self.link_cache_dir_editor = Some(LinkCacheDirEditor::new(
                        self.config.worktree_link_cache_dir.clone(),
                    ));
                }
                if matches!(value, SettingsStep::PathTemplate) {
                    self.path_template_editor = Some(PathTemplateEditor::new(
                        self.config.worktree_path_template.clone(),
                    ));
                }
                if matches!(value, SettingsStep::Dashboard) {
                    self.dashboard_editor = Some(DashboardEditor::new(&self.config.dashboard));
                }
                if matches!(value, SettingsStep::Notifications) {
                    self.notifications_editor =
                        Some(NotificationsEditor::new(&self.config.notifications));
                }
                if matches!(value, SettingsStep::CopySettings) {
                    self.copy_settings_select = Some(self.build_copy_settings_select());
                }
                SettingsAction::Continue
            }
            SelectOutcome::Cancelled => SettingsAction::Back,
            SelectOutcome::Pending => SettingsAction::Continue,
        }
    }

    fn handle_copy_settings(&mut self, key: KeyEvent) -> SettingsAction {
        let select = match self.copy_settings_select.as_mut() {
            Some(select) => select,
            None => {
                self.step = SettingsStep::Menu;
                return SettingsAction::Continue;
            }
        };

        match select.handle_key(key) {
            SelectOutcome::Selected(_, direction) => SettingsAction::CopySettings(direction),
            SelectOutcome::Cancelled => {
                self.copy_settings_select = None;
                self.step = SettingsStep::Menu;
                SettingsAction::Continue
            }
            SelectOutcome::Pending => SettingsAction::Continue,
        }
    }

    fn handle_pattern_list(&mut self, key: KeyEvent) -> SettingsAction {
        let editor = match self.pattern_list_editor.as_mut() {
            Some(editor) => editor,
            None => {
                self.step = SettingsStep::Menu;
                return SettingsAction::Continue;
            }
        };

        if editor.editing() {
            return self.handle_pattern_list_editing(key);
        }

        match key.code {
            KeyCode::Esc => {
                self.pattern_list_editor = None;
                self.step = SettingsStep::Menu;
                SettingsAction::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                editor.move_up();
                SettingsAction::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                editor.move_down();
                SettingsAction::Continue
            }
            KeyCode::Enter => match editor.selection {
                PatternListSelection::Rect => {
                    editor.start_editing();
                    SettingsAction::Continue
                }
                PatternListSelection::Save => match self.step {
                    SettingsStep::CopyPatterns => {
                        SettingsAction::SaveCopyPatterns(editor.values_to_save())
                    }
                    SettingsStep::IgnorePatterns => {
                        SettingsAction::SaveIgnorePatterns(editor.values_to_save())
                    }
                    SettingsStep::LinkPatterns => {
                        SettingsAction::SaveLinkPatterns(editor.values_to_save())
                    }
                    _ => SettingsAction::Continue,
                },
            },
            _ => SettingsAction::Continue,
        }
    }

    fn handle_pattern_list_editing(&mut self, key: KeyEvent) -> SettingsAction {
        let editor = match self.pattern_list_editor.as_mut() {
            Some(editor) => editor,
            None => return SettingsAction::Continue,
        };
        let ctrl = key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(crossterm::event::KeyModifiers::ALT);

        let mutate = |editor: &mut PatternListEditor| {
            editor.clamp_scroll_for_cursor(10_000);
        };

        match key.code {
            KeyCode::Esc => {
                editor.stop_editing();
                SettingsAction::Continue
            }
            KeyCode::Enter => {
                editor.insert_newline();
                mutate(editor);
                SettingsAction::Continue
            }
            KeyCode::Backspace if ctrl || alt => {
                editor.delete_word_left();
                mutate(editor);
                SettingsAction::Continue
            }
            KeyCode::Backspace => {
                editor.delete_left();
                mutate(editor);
                SettingsAction::Continue
            }
            KeyCode::Delete => {
                editor.delete_right();
                mutate(editor);
                SettingsAction::Continue
            }
            KeyCode::Left if ctrl || alt => {
                editor.move_word_left();
                mutate(editor);
                SettingsAction::Continue
            }
            KeyCode::Left => {
                editor.move_cursor_left();
                mutate(editor);
                SettingsAction::Continue
            }
            KeyCode::Right if ctrl || alt => {
                editor.move_word_right();
                mutate(editor);
                SettingsAction::Continue
            }
            KeyCode::Right => {
                editor.move_cursor_right();
                mutate(editor);
                SettingsAction::Continue
            }
            KeyCode::Up => {
                editor.move_cursor_up();
                mutate(editor);
                SettingsAction::Continue
            }
            KeyCode::Down => {
                editor.move_cursor_down();
                mutate(editor);
                SettingsAction::Continue
            }
            KeyCode::Home => {
                editor.move_cursor_home();
                SettingsAction::Continue
            }
            KeyCode::End => {
                editor.move_cursor_end();
                SettingsAction::Continue
            }
            KeyCode::Char(c) if ctrl => {
                match c.to_ascii_lowercase() {
                    'a' => editor.move_cursor_home(),
                    'e' => editor.move_cursor_end(),
                    'b' => editor.move_cursor_left(),
                    'f' => editor.move_cursor_right(),
                    'h' => editor.delete_left(),
                    'd' => editor.delete_right(),
                    'w' => editor.delete_word_left(),
                    'u' => editor.kill_to_start(),
                    'k' => editor.kill_to_end(),
                    _ => return SettingsAction::Continue,
                }
                mutate(editor);
                SettingsAction::Continue
            }
            KeyCode::Char(c) if alt => {
                match c.to_ascii_lowercase() {
                    'b' => editor.move_word_left(),
                    'f' => editor.move_word_right(),
                    'd' => editor.delete_word_right(),
                    _ => return SettingsAction::Continue,
                }
                mutate(editor);
                SettingsAction::Continue
            }
            KeyCode::Char(c) => {
                editor.with_cursor(|lines, cursor| {
                    let line = &mut lines[cursor.row];
                    let byte = PatternListEditor::byte_offset(line, cursor.col);
                    line.insert(byte, c);
                    cursor.col += 1;
                });
                mutate(editor);
                SettingsAction::Continue
            }
            _ => SettingsAction::Continue,
        }
    }

    fn handle_post_cmd(&mut self, key: KeyEvent) -> SettingsAction {
        let editing_idx = self
            .post_cmd_editor
            .as_ref()
            .and_then(PostCmdEditor::editing_index);
        if let Some(idx) = editing_idx {
            return self.handle_post_cmd_editing(idx, key);
        }

        let editor = match self.post_cmd_editor.as_mut() {
            Some(e) => e,
            None => {
                self.step = SettingsStep::Menu;
                return SettingsAction::Continue;
            }
        };

        let mut start_editing = None;
        let action = match key.code {
            KeyCode::Esc => {
                self.post_cmd_editor = None;
                self.post_cmd_input = None;
                self.step = SettingsStep::Menu;
                SettingsAction::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                editor.move_up();
                SettingsAction::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                editor.move_down();
                SettingsAction::Continue
            }
            KeyCode::Char('K') => {
                editor.move_selected_up(self.tick);
                SettingsAction::Continue
            }
            KeyCode::Char('J') => {
                editor.move_selected_down(self.tick);
                SettingsAction::Continue
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                editor.toggle_buttons();
                SettingsAction::Continue
            }
            KeyCode::Enter => match editor.selection {
                PostCmdSelection::Rect(i) => {
                    start_editing = Some(i);
                    SettingsAction::Continue
                }
                PostCmdSelection::Create => {
                    editor.commands.push(String::new());
                    editor.statuses.push(PostCmdRectStatus::Unchanged);
                    editor.swap_highlights.push(None);
                    let idx = editor.commands.len() - 1;
                    editor.set_selection(PostCmdSelection::Rect(idx));
                    start_editing = Some(idx);
                    SettingsAction::Continue
                }
                PostCmdSelection::Save => {
                    let to_save = editor.commands_to_save();
                    SettingsAction::SavePostCreateCommands(to_save)
                }
            },
            KeyCode::Backspace | KeyCode::Delete => {
                editor.toggle_delete_mark();
                SettingsAction::Continue
            }
            _ => SettingsAction::Continue,
        };

        if let Some(idx) = start_editing {
            self.start_post_cmd_editing(idx);
        }

        action
    }

    fn start_post_cmd_editing(&mut self, idx: usize) {
        let editor = match self.post_cmd_editor.as_mut() {
            Some(editor) => editor,
            None => return,
        };
        let prior = match editor.statuses[idx] {
            PostCmdRectStatus::MarkedForDeletion => PostCmdRectStatus::Modified,
            other => other,
        };
        editor.set_selection(PostCmdSelection::Rect(idx));
        editor.edit_backup = Some((editor.commands[idx].clone(), prior));
        editor.statuses[idx] = PostCmdRectStatus::Editing;
        self.post_cmd_input = Some(build_post_cmd_input(&editor.commands[idx]));
    }

    fn handle_post_cmd_editing(&mut self, idx: usize, key: KeyEvent) -> SettingsAction {
        let (outcome, current_value) = match self.post_cmd_input.as_mut() {
            Some(prompt) => {
                let outcome = prompt.handle_key(key);
                let current_value = prompt.value.clone();
                (outcome, current_value)
            }
            None => {
                if let Some(editor) = self.post_cmd_editor.as_mut() {
                    editor.statuses[idx] = PostCmdRectStatus::Unchanged;
                    editor.edit_backup = None;
                }
                return SettingsAction::Continue;
            }
        };

        match outcome {
            InputOutcome::Cancelled => {
                let editor = match self.post_cmd_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                if let Some((value, prior)) = editor.edit_backup.take() {
                    editor.commands[idx] = value;
                    editor.statuses[idx] = prior;
                } else {
                    editor.statuses[idx] = PostCmdRectStatus::Unchanged;
                }
                self.post_cmd_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Submitted(value) => {
                let editor = match self.post_cmd_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                let next_status = editor
                    .edit_backup
                    .take()
                    .map(|(original, prior)| {
                        if value == original {
                            prior
                        } else {
                            PostCmdRectStatus::Modified
                        }
                    })
                    .unwrap_or(PostCmdRectStatus::Modified);
                editor.commands[idx] = value;
                editor.statuses[idx] = next_status;
                self.post_cmd_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Pending => {
                let editor = match self.post_cmd_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                editor.commands[idx] = current_value;
                SettingsAction::Continue
            }
        }
    }

    fn handle_terminal_cmd(&mut self, key: KeyEvent) -> SettingsAction {
        let is_editing = self
            .terminal_cmd_editor
            .as_ref()
            .map(|e| e.editing())
            .unwrap_or(false);
        if is_editing {
            return self.handle_terminal_cmd_editing(key);
        }

        let editor = match self.terminal_cmd_editor.as_mut() {
            Some(e) => e,
            None => {
                self.step = SettingsStep::Menu;
                return SettingsAction::Continue;
            }
        };

        let mut start_editing = false;
        let action = match key.code {
            KeyCode::Esc => {
                self.terminal_cmd_editor = None;
                self.terminal_cmd_input = None;
                self.step = SettingsStep::Menu;
                SettingsAction::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                editor.move_up();
                SettingsAction::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                editor.move_down();
                SettingsAction::Continue
            }
            KeyCode::Enter => match editor.selection {
                TerminalCmdSelection::Rect => {
                    start_editing = true;
                    SettingsAction::Continue
                }
                TerminalCmdSelection::Save => {
                    SettingsAction::SaveTerminalCommand(editor.command_to_save())
                }
            },
            _ => SettingsAction::Continue,
        };

        if start_editing {
            self.start_terminal_cmd_editing();
        }

        action
    }

    fn start_terminal_cmd_editing(&mut self) {
        let editor = match self.terminal_cmd_editor.as_mut() {
            Some(editor) => editor,
            None => return,
        };
        editor.selection = TerminalCmdSelection::Rect;
        editor.edit_backup = Some((editor.command.clone(), editor.status));
        editor.status = TerminalCmdRectStatus::Editing;
        self.terminal_cmd_input = Some(build_terminal_cmd_input(&editor.command));
    }

    fn handle_terminal_cmd_editing(&mut self, key: KeyEvent) -> SettingsAction {
        let (outcome, current_value) = match self.terminal_cmd_input.as_mut() {
            Some(prompt) => {
                let outcome = prompt.handle_key(key);
                let current_value = prompt.value.clone();
                (outcome, current_value)
            }
            None => {
                if let Some(editor) = self.terminal_cmd_editor.as_mut() {
                    editor.status = TerminalCmdRectStatus::Unchanged;
                    editor.edit_backup = None;
                }
                return SettingsAction::Continue;
            }
        };

        match outcome {
            InputOutcome::Cancelled => {
                let editor = match self.terminal_cmd_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                if let Some((value, prior)) = editor.edit_backup.take() {
                    editor.command = value;
                    editor.status = prior;
                } else {
                    editor.status = TerminalCmdRectStatus::Unchanged;
                }
                self.terminal_cmd_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Submitted(value) => {
                let editor = match self.terminal_cmd_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                let next_status = editor
                    .edit_backup
                    .take()
                    .map(|(original, prior)| {
                        if value == original {
                            prior
                        } else {
                            TerminalCmdRectStatus::Modified
                        }
                    })
                    .unwrap_or(TerminalCmdRectStatus::Modified);
                editor.command = value;
                editor.status = next_status;
                self.terminal_cmd_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Pending => {
                let editor = match self.terminal_cmd_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                editor.command = current_value;
                SettingsAction::Continue
            }
        }
    }

    fn handle_link_strategy(&mut self, key: KeyEvent) -> SettingsAction {
        let editor = match self.link_strategy_editor.as_mut() {
            Some(e) => e,
            None => {
                self.step = SettingsStep::Menu;
                return SettingsAction::Continue;
            }
        };

        match key.code {
            KeyCode::Esc => {
                self.link_strategy_editor = None;
                self.step = SettingsStep::Menu;
                SettingsAction::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                editor.move_up();
                SettingsAction::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                editor.move_down();
                SettingsAction::Continue
            }
            KeyCode::Enter => match editor.selection {
                LinkStrategySelection::Rect => {
                    editor.toggle_value();
                    SettingsAction::Continue
                }
                LinkStrategySelection::Save => {
                    SettingsAction::SaveLinkStrategy(editor.strategy_to_save())
                }
            },
            _ => SettingsAction::Continue,
        }
    }

    fn handle_link_cache_dir(&mut self, key: KeyEvent) -> SettingsAction {
        let is_editing = self
            .link_cache_dir_editor
            .as_ref()
            .map(|e| e.editing())
            .unwrap_or(false);
        if is_editing {
            return self.handle_link_cache_dir_editing(key);
        }

        let editor = match self.link_cache_dir_editor.as_mut() {
            Some(e) => e,
            None => {
                self.step = SettingsStep::Menu;
                return SettingsAction::Continue;
            }
        };

        let mut start_editing = false;
        let action = match key.code {
            KeyCode::Esc => {
                self.link_cache_dir_editor = None;
                self.link_cache_dir_input = None;
                self.step = SettingsStep::Menu;
                SettingsAction::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                editor.move_up();
                SettingsAction::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                editor.move_down();
                SettingsAction::Continue
            }
            KeyCode::Enter => match editor.selection {
                LinkCacheDirSelection::Rect => {
                    start_editing = true;
                    SettingsAction::Continue
                }
                LinkCacheDirSelection::Save => {
                    SettingsAction::SaveLinkCacheDir(editor.cache_dir_to_save())
                }
            },
            _ => SettingsAction::Continue,
        };

        if start_editing {
            self.start_link_cache_dir_editing();
        }

        action
    }

    fn start_link_cache_dir_editing(&mut self) {
        let editor = match self.link_cache_dir_editor.as_mut() {
            Some(editor) => editor,
            None => return,
        };
        editor.selection = LinkCacheDirSelection::Rect;
        editor.edit_backup = Some((editor.value.clone(), editor.status));
        editor.status = LinkCacheDirRectStatus::Editing;
        self.link_cache_dir_input = Some(build_link_cache_dir_input(&editor.value));
    }

    fn handle_link_cache_dir_editing(&mut self, key: KeyEvent) -> SettingsAction {
        let (outcome, current_value) = match self.link_cache_dir_input.as_mut() {
            Some(prompt) => {
                let outcome = prompt.handle_key(key);
                let current_value = prompt.value.clone();
                (outcome, current_value)
            }
            None => {
                if let Some(editor) = self.link_cache_dir_editor.as_mut() {
                    editor.status = LinkCacheDirRectStatus::Unchanged;
                    editor.edit_backup = None;
                }
                return SettingsAction::Continue;
            }
        };

        match outcome {
            InputOutcome::Cancelled => {
                let editor = match self.link_cache_dir_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                if let Some((value, prior)) = editor.edit_backup.take() {
                    editor.value = value;
                    editor.status = prior;
                } else {
                    editor.status = LinkCacheDirRectStatus::Unchanged;
                }
                self.link_cache_dir_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Submitted(value) => {
                let editor = match self.link_cache_dir_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                let next_status = editor
                    .edit_backup
                    .take()
                    .map(|(original, prior)| {
                        if value == original {
                            prior
                        } else {
                            LinkCacheDirRectStatus::Modified
                        }
                    })
                    .unwrap_or(LinkCacheDirRectStatus::Modified);
                editor.value = value;
                editor.status = next_status;
                self.link_cache_dir_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Pending => {
                let editor = match self.link_cache_dir_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                editor.value = current_value;
                SettingsAction::Continue
            }
        }
    }

    fn handle_path_template(&mut self, key: KeyEvent) -> SettingsAction {
        let is_editing = self
            .path_template_editor
            .as_ref()
            .map(|e| e.editing())
            .unwrap_or(false);
        if is_editing {
            return self.handle_path_template_editing(key);
        }

        let editor = match self.path_template_editor.as_mut() {
            Some(e) => e,
            None => {
                self.step = SettingsStep::Menu;
                return SettingsAction::Continue;
            }
        };

        let mut start_editing = false;
        let action = match key.code {
            KeyCode::Esc => {
                self.path_template_editor = None;
                self.path_template_input = None;
                self.step = SettingsStep::Menu;
                SettingsAction::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                editor.move_up();
                SettingsAction::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                editor.move_down();
                SettingsAction::Continue
            }
            KeyCode::Enter => match editor.selection {
                PathTemplateSelection::Rect => {
                    start_editing = true;
                    SettingsAction::Continue
                }
                PathTemplateSelection::Save => {
                    SettingsAction::SavePathTemplate(editor.template_to_save())
                }
            },
            _ => SettingsAction::Continue,
        };

        if start_editing {
            self.start_path_template_editing();
        }

        action
    }

    fn start_path_template_editing(&mut self) {
        let editor = match self.path_template_editor.as_mut() {
            Some(editor) => editor,
            None => return,
        };
        editor.selection = PathTemplateSelection::Rect;
        editor.edit_backup = Some((editor.template.clone(), editor.status));
        editor.status = PathTemplateRectStatus::Editing;
        self.path_template_input = Some(build_path_template_input(&editor.template));
    }

    fn handle_path_template_editing(&mut self, key: KeyEvent) -> SettingsAction {
        let (outcome, current_value) = match self.path_template_input.as_mut() {
            Some(prompt) => {
                let outcome = prompt.handle_key(key);
                let current_value = prompt.value.clone();
                (outcome, current_value)
            }
            None => {
                if let Some(editor) = self.path_template_editor.as_mut() {
                    editor.status = PathTemplateRectStatus::Unchanged;
                    editor.edit_backup = None;
                }
                return SettingsAction::Continue;
            }
        };

        match outcome {
            InputOutcome::Cancelled => {
                let editor = match self.path_template_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                if let Some((value, prior)) = editor.edit_backup.take() {
                    editor.template = value;
                    editor.status = prior;
                } else {
                    editor.status = PathTemplateRectStatus::Unchanged;
                }
                self.path_template_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Submitted(value) => {
                let editor = match self.path_template_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                let next_status = editor
                    .edit_backup
                    .take()
                    .map(|(original, prior)| {
                        if value == original {
                            prior
                        } else {
                            PathTemplateRectStatus::Modified
                        }
                    })
                    .unwrap_or(PathTemplateRectStatus::Modified);
                editor.template = value;
                editor.status = next_status;
                self.path_template_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Pending => {
                let editor = match self.path_template_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                editor.template = current_value;
                SettingsAction::Continue
            }
        }
    }

    fn handle_dashboard(&mut self, key: KeyEvent) -> SettingsAction {
        let editing_idx = self
            .dashboard_editor
            .as_ref()
            .and_then(DashboardEditor::editing_index);
        if let Some(idx) = editing_idx {
            return self.handle_dashboard_editing(idx, key);
        }

        let editor = match self.dashboard_editor.as_mut() {
            Some(e) => e,
            None => {
                self.step = SettingsStep::Menu;
                return SettingsAction::Continue;
            }
        };

        let mut start_editing = None;
        let mut toggle_idx = None;
        let mut open_ai_settings: Option<AiConfig> = None;
        let action = match key.code {
            KeyCode::Esc => {
                self.dashboard_editor = None;
                self.dashboard_input = None;
                self.step = SettingsStep::Menu;
                SettingsAction::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.dashboard_step_up();
                SettingsAction::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.dashboard_step_down();
                SettingsAction::Continue
            }
            KeyCode::Enter => match editor.selection {
                DashboardSelection::Rect(i) => {
                    let field = editor.field(i);
                    if field.is_toggle() {
                        toggle_idx = Some(i);
                        SettingsAction::Continue
                    } else if matches!(field, DashboardField::Ai) {
                        // Drill into the AI Settings sub-screen to choose a
                        // model + thinking strength for each AI command.
                        open_ai_settings = Some(editor.ai.clone());
                        SettingsAction::Continue
                    } else {
                        start_editing = Some(i);
                        SettingsAction::Continue
                    }
                }
                DashboardSelection::Save => {
                    SettingsAction::SaveDashboard(Box::new(editor.build_config()))
                }
            },
            _ => SettingsAction::Continue,
        };

        if let Some(idx) = toggle_idx {
            self.toggle_dashboard_bool(idx);
        }
        if let Some(idx) = start_editing {
            self.start_dashboard_editing(idx);
        }
        if let Some(ai) = open_ai_settings {
            self.ai_settings_editor = Some(AiSettingsEditor::new(&ai));
            self.free_models = None;
            self.step = SettingsStep::AiSettings;
            // Populate the free-model chip row + per-model variant ladders.
            return SettingsAction::FetchFreeModels;
        }

        action
    }

    fn handle_ai_settings(&mut self, key: KeyEvent) -> SettingsAction {
        let editor = match self.ai_settings_editor.as_mut() {
            Some(e) => e,
            None => {
                self.step = SettingsStep::Dashboard;
                return SettingsAction::Continue;
            }
        };

        let mut open_picker: Option<AiModelConfig> = None;
        let action = match key.code {
            KeyCode::Esc => {
                // Stage the (possibly edited) per-command config back into the
                // still-open Dashboard editor so its Save persists it, then
                // return to the Dashboard sub-screen.
                let staged = editor.build_ai();
                let changed = staged != normalize_ai(&editor.base_ai);
                self.ai_settings_editor = None;
                self.free_models = None;
                if let Some(dash) = self.dashboard_editor.as_mut() {
                    dash.ai = staged;
                    if let Some(idx) = ai_field_index() {
                        if changed {
                            dash.statuses[idx] = DashboardRectStatus::Modified;
                        }
                        dash.selection = DashboardSelection::Rect(idx);
                    }
                }
                self.step = SettingsStep::Dashboard;
                SettingsAction::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.ai_settings_step_up();
                SettingsAction::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.ai_settings_step_down();
                SettingsAction::Continue
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if !self.ai_settings_adjust_thinking(false) {
                    self.ai_settings_cycle_chip(false);
                }
                SettingsAction::Continue
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if !self.ai_settings_adjust_thinking(true) {
                    self.ai_settings_cycle_chip(true);
                }
                SettingsAction::Continue
            }
            KeyCode::Enter => match editor.selection {
                AiSettingsSelection::Rect(_) => {
                    // Open the model picker pre-filled with this slot's model +
                    // thinking. The picker writes back via `apply_ai_selection`.
                    open_picker = Some(editor.focused_model());
                    SettingsAction::Continue
                }
                AiSettingsSelection::FreeModels(_) => {
                    self.apply_focused_free_model_ai();
                    SettingsAction::Continue
                }
                AiSettingsSelection::Save => self.save_ai_settings(),
            },
            _ => SettingsAction::Continue,
        };

        if let Some(model) = open_picker {
            return SettingsAction::OpenAiModelPicker {
                model: model.model,
                variant: model.thinking,
            };
        }

        action
    }

    /// Stage the AI sub-screen config into the Dashboard editor and persist the
    /// whole dashboard config (so the AI page's Save behaves like every other
    /// Save button). Returns the `SaveDashboard` action for the App to write.
    fn save_ai_settings(&mut self) -> SettingsAction {
        let staged = match self.ai_settings_editor.as_mut() {
            Some(editor) => {
                editor.mark_saved();
                editor.build_ai()
            }
            None => return SettingsAction::Continue,
        };
        let Some(dash) = self.dashboard_editor.as_mut() else {
            return SettingsAction::Continue;
        };
        dash.ai = staged;
        if let Some(idx) = ai_field_index() {
            dash.statuses[idx] = DashboardRectStatus::Saved;
        }
        SettingsAction::SaveDashboard(Box::new(dash.build_config()))
    }

    fn handle_notifications(&mut self, key: KeyEvent) -> SettingsAction {
        let editor = match self.notifications_editor.as_mut() {
            Some(e) => e,
            None => {
                self.step = SettingsStep::Menu;
                return SettingsAction::Continue;
            }
        };

        let mut toggle_idx = None;
        let action = match key.code {
            KeyCode::Esc => {
                self.notifications_editor = None;
                self.step = SettingsStep::Menu;
                SettingsAction::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.notifications_step_up();
                SettingsAction::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.notifications_step_down();
                SettingsAction::Continue
            }
            KeyCode::Enter => match editor.selection {
                NotificationsSelection::Rect(i) => {
                    toggle_idx = Some(i);
                    SettingsAction::Continue
                }
                NotificationsSelection::Save => {
                    SettingsAction::SaveNotifications(editor.build_config())
                }
            },
            _ => SettingsAction::Continue,
        };

        if let Some(idx) = toggle_idx {
            self.toggle_notifications_bool(idx);
        }

        action
    }

    fn notifications_step_up(&mut self) {
        if let Some(editor) = self.notifications_editor.as_mut() {
            editor.step_up();
        }
    }

    fn notifications_step_down(&mut self) {
        if let Some(editor) = self.notifications_editor.as_mut() {
            editor.step_down();
        }
    }

    fn toggle_notifications_bool(&mut self, idx: usize) {
        if let Some(editor) = self.notifications_editor.as_mut() {
            editor.toggle(idx);
        }
    }

    /// Snapshot of the free-model list if the fetch succeeded and produced
    /// at least one chip. Used both to gate cursor transitions and to
    /// resolve the active pair on Enter.
    fn free_models_list(&self) -> Option<&[String]> {
        match &self.free_models {
            Some(Ok(list)) if !list.is_empty() => Some(list.as_slice()),
            _ => None,
        }
    }

    /// Up navigation across the dashboard rectangles + Save (no chip row —
    /// per-command AI models live on the AI Settings sub-screen now).
    fn dashboard_step_up(&mut self) {
        let Some(editor) = self.dashboard_editor.as_mut() else {
            return;
        };
        editor.selection = match editor.selection {
            DashboardSelection::Rect(0) => DashboardSelection::Rect(0),
            DashboardSelection::Rect(i) => DashboardSelection::Rect(i - 1),
            DashboardSelection::Save => DashboardSelection::Rect(editor.values.len() - 1),
        };
    }

    fn dashboard_step_down(&mut self) {
        let Some(editor) = self.dashboard_editor.as_mut() else {
            return;
        };
        editor.selection = match editor.selection {
            DashboardSelection::Rect(i) if i + 1 < editor.values.len() => {
                DashboardSelection::Rect(i + 1)
            }
            DashboardSelection::Rect(_) => DashboardSelection::Save,
            DashboardSelection::Save => DashboardSelection::Save,
        };
    }

    // ── AI Settings sub-screen navigation ──────────────────────────────────

    /// Up navigation across slot rectangles / chip row / Save. The chip row
    /// sits between the last slot and Save when free models are cached.
    fn ai_settings_step_up(&mut self) {
        let has_chips = self.free_models_list().is_some();
        let Some(editor) = self.ai_settings_editor.as_mut() else {
            return;
        };
        let last = AiSlot::ALL.len() - 1;
        editor.selection = match editor.selection {
            AiSettingsSelection::Rect(0) => AiSettingsSelection::Rect(0),
            AiSettingsSelection::Rect(i) => {
                editor.last_rect = i - 1;
                AiSettingsSelection::Rect(i - 1)
            }
            AiSettingsSelection::FreeModels(_) => AiSettingsSelection::Rect(last),
            AiSettingsSelection::Save if has_chips => AiSettingsSelection::FreeModels(0),
            AiSettingsSelection::Save => AiSettingsSelection::Rect(last),
        };
    }

    fn ai_settings_step_down(&mut self) {
        let has_chips = self.free_models_list().is_some();
        let Some(editor) = self.ai_settings_editor.as_mut() else {
            return;
        };
        let last = AiSlot::ALL.len() - 1;
        editor.selection = match editor.selection {
            AiSettingsSelection::Rect(i) if i < last => {
                editor.last_rect = i + 1;
                AiSettingsSelection::Rect(i + 1)
            }
            AiSettingsSelection::Rect(_) if has_chips => AiSettingsSelection::FreeModels(0),
            AiSettingsSelection::Rect(_) => AiSettingsSelection::Save,
            AiSettingsSelection::FreeModels(_) => AiSettingsSelection::Save,
            AiSettingsSelection::Save => AiSettingsSelection::Save,
        };
    }

    /// Left/Right cycle within the AI Settings chip row. No-op unless the
    /// cursor is on the chip row with at least one chip cached.
    fn ai_settings_cycle_chip(&mut self, forward: bool) {
        let chip_count = match self.free_models_list().map(|l| l.len()) {
            Some(n) if n > 0 => n,
            _ => return,
        };
        let Some(editor) = self.ai_settings_editor.as_mut() else {
            return;
        };
        if let AiSettingsSelection::FreeModels(i) = editor.selection {
            let next = if forward {
                (i + 1) % chip_count
            } else if i == 0 {
                chip_count - 1
            } else {
                i - 1
            };
            editor.selection = AiSettingsSelection::FreeModels(next);
        }
    }

    /// Left/Right on a slot rectangle adjusts that slot's thinking strength.
    /// The empty string is the persisted "Default"; concrete variants follow
    /// the chosen model's own weakest-to-strongest ladder. Returns `true` when
    /// the cursor was on a rectangle (so the caller skips chip cycling).
    fn ai_settings_adjust_thinking(&mut self, increase: bool) -> bool {
        // Read the targeted slot + its model up front so the immutable borrow
        // is released before `ai_thinking_ladder` (which also borrows `self`).
        let (idx, pair, current) = {
            let Some(editor) = self.ai_settings_editor.as_ref() else {
                return false;
            };
            let AiSettingsSelection::Rect(idx) = editor.selection else {
                return false;
            };
            let model = AiSettingsEditor::slot(idx).get(&editor.ai);
            let pair = model.model.trim().to_string();
            if pair.is_empty() {
                return true;
            }
            (idx, pair, model.thinking.clone())
        };

        let ladder = self.ai_thinking_ladder(&pair);
        let current_idx = reasoning_level_index(&ladder, &current);
        let next_idx = if increase {
            (current_idx + 1).min(reasoning_level_count(&ladder) - 1)
        } else {
            current_idx.saturating_sub(1)
        };
        let next = reasoning_level_at(&ladder, next_idx).unwrap_or_default();
        let Some(editor) = self.ai_settings_editor.as_mut() else {
            return false;
        };
        let target = AiSettingsEditor::slot(idx).get_mut(&mut editor.ai);
        if next != target.thinking {
            target.thinking = next;
            editor.statuses[idx] = DashboardRectStatus::Modified;
        }
        true
    }

    /// Stage the chip-row cursor's pair into the targeted slot without saving —
    /// persisted via the Save button. Cursor stays on the chip row so the user
    /// can keep cycling.
    fn apply_focused_free_model_ai(&mut self) {
        let models = match self.free_models_list() {
            Some(list) => list.to_vec(),
            None => return,
        };
        let Some(editor) = self.ai_settings_editor.as_mut() else {
            return;
        };
        let AiSettingsSelection::FreeModels(i) = editor.selection else {
            return;
        };
        let pair = models[i.min(models.len() - 1)].clone();
        editor.apply_free_model(pair);
    }

    fn toggle_dashboard_bool(&mut self, idx: usize) {
        let editor = match self.dashboard_editor.as_mut() {
            Some(editor) => editor,
            None => return,
        };
        let current = matches!(
            editor.values[idx].trim().to_ascii_lowercase().as_str(),
            "true" | "yes" | "1" | "on"
        );
        let next = (!current).to_string();
        editor.values[idx] = next;
        editor.statuses[idx] = DashboardRectStatus::Modified;
    }

    fn start_dashboard_editing(&mut self, idx: usize) {
        let editor = match self.dashboard_editor.as_mut() {
            Some(editor) => editor,
            None => return,
        };
        editor.selection = DashboardSelection::Rect(idx);
        editor.edit_backup = Some((editor.values[idx].clone(), editor.statuses[idx]));
        editor.statuses[idx] = DashboardRectStatus::Editing;
        let field = editor.field(idx);
        self.dashboard_input = Some(build_dashboard_input(field, &editor.values[idx]));
    }

    fn handle_dashboard_editing(&mut self, idx: usize, key: KeyEvent) -> SettingsAction {
        let (outcome, current_value) = match self.dashboard_input.as_mut() {
            Some(prompt) => {
                let outcome = prompt.handle_key(key);
                let current_value = prompt.value.clone();
                (outcome, current_value)
            }
            None => {
                if let Some(editor) = self.dashboard_editor.as_mut() {
                    editor.statuses[idx] = DashboardRectStatus::Unchanged;
                    editor.edit_backup = None;
                }
                return SettingsAction::Continue;
            }
        };

        match outcome {
            InputOutcome::Cancelled => {
                let editor = match self.dashboard_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                if let Some((value, prior)) = editor.edit_backup.take() {
                    editor.values[idx] = value;
                    editor.statuses[idx] = prior;
                } else {
                    editor.statuses[idx] = DashboardRectStatus::Unchanged;
                }
                self.dashboard_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Submitted(value) => {
                let editor = match self.dashboard_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                let next_status = editor
                    .edit_backup
                    .take()
                    .map(|(original, prior)| {
                        if value == original {
                            prior
                        } else {
                            DashboardRectStatus::Modified
                        }
                    })
                    .unwrap_or(DashboardRectStatus::Modified);
                editor.values[idx] = value;
                editor.statuses[idx] = next_status;
                self.dashboard_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Pending => {
                let editor = match self.dashboard_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                editor.values[idx] = current_value;
                SettingsAction::Continue
            }
        }
    }

    fn handle_delete_branch(&mut self, key: KeyEvent) -> SettingsAction {
        if self.delete_branch_dialog.is_none() {
            self.delete_branch_dialog = Some(self.build_delete_branch_dialog());
        }
        let dialog = self
            .delete_branch_dialog
            .as_mut()
            .expect("dialog initialized above");

        match key.code {
            KeyCode::Esc => {
                self.step = SettingsStep::Menu;
                self.delete_branch_dialog = None;
                SettingsAction::Continue
            }
            KeyCode::Enter => SettingsAction::SetDeleteBranchWithWorktree(matches!(
                dialog.selected(),
                ConfirmationChoice::Confirm
            )),
            _ => {
                let _ = dialog.handle_key(key);
                SettingsAction::Continue
            }
        }
    }

    fn handle_check_updates(&mut self, key: KeyEvent) -> SettingsAction {
        // While the registry check or an upgrade is running, swallow keys
        // except Esc (which still backs out of the screen).
        if self.checking_updates || self.upgrading_source.is_some() {
            if matches!(key.code, KeyCode::Esc) {
                self.leave_check_updates();
            }
            return SettingsAction::Continue;
        }
        match key.code {
            KeyCode::Esc => {
                self.leave_check_updates();
                SettingsAction::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.update_selection = UpdateSource::Npm;
                SettingsAction::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.update_selection = UpdateSource::Homebrew;
                SettingsAction::Continue
            }
            KeyCode::Enter => SettingsAction::UpgradeSource(self.update_selection),
            _ => SettingsAction::Continue,
        }
    }

    fn leave_check_updates(&mut self) {
        self.step = SettingsStep::Menu;
        self.update_result = None;
        self.upgrading_source = None;
        self.upgrade_outcome = None;
        self.update_selection = UpdateSource::Npm;
    }

    /// Inner content height for the panel (excludes the rounded border).
    pub fn preferred_content_height(&self) -> u16 {
        if self.error.is_some() {
            return 6;
        }
        match self.step {
            // Settings menu: config path header + select prompt + hint.
            SettingsStep::Menu => 23,
            SettingsStep::SetupProject => 6,
            // Title + description + 2 rectangles (3 rows each) + 2 hints
            // + spacer + result line + footer hint.
            SettingsStep::CheckUpdates => 14,
            SettingsStep::CopyPatterns => self.pattern_list_preferred_height(),
            SettingsStep::LinkPatterns => self.pattern_list_preferred_height(),
            SettingsStep::LinkStrategy => self.link_strategy_preferred_height(),
            SettingsStep::LinkCacheDir => self.link_cache_dir_preferred_height(),
            SettingsStep::IgnorePatterns => self.pattern_list_preferred_height(),
            SettingsStep::Dashboard => self.dashboard_preferred_height(),
            SettingsStep::AiSettings => self.ai_settings_preferred_height(),
            SettingsStep::Notifications => self.notifications_preferred_height(),
            SettingsStep::PathTemplate => self.path_template_preferred_height(),
            SettingsStep::TerminalCmd => self.terminal_cmd_preferred_height(),
            SettingsStep::PostCmd => self.post_cmd_preferred_height(),
            SettingsStep::DeleteBranch => 16,
            SettingsStep::CopySettings => 13,
        }
    }

    fn post_cmd_preferred_height(&self) -> u16 {
        let n = self
            .post_cmd_editor
            .as_ref()
            .map(|e| e.commands.len() as u16)
            .unwrap_or(0);
        // title + subtitle + commands (3 rows each, min 3) + scroll + buttons + saving + vars_intro + vars + hints
        let command_rows = n.saturating_mul(3).max(3);
        2 + command_rows + 1 + 3 + 1 + 1 + 1 + 1
    }

    fn terminal_cmd_preferred_height(&self) -> u16 {
        // Title + description + 1 rectangle (3 rows) + per-field hint +
        // spacer + Save button (3 rows) + saving-to line + footer hint.
        2 + 3 + 1 + 1 + 3 + 2
    }

    fn dashboard_preferred_height(&self) -> u16 {
        // Title + description + rectangles (3 rows each) + hint rows
        // + spacer + Save button (3 rows) + saving-to line + footer hint.
        let rects = DashboardField::ALL.len() as u16;
        2 + rects * 3 + rects + 1 + 3 + 2
    }

    fn ai_settings_preferred_height(&self) -> u16 {
        // Title + description + slot rectangles (3 rows each) + hint rows
        // + chip row + chip hint + spacer + Save button (3 rows) + footer hint.
        let rects = AiSlot::ALL.len() as u16;
        2 + rects * 3 + rects + 2 + 1 + 3 + 1
    }

    fn path_template_preferred_height(&self) -> u16 {
        // Title + description + 1 rectangle (3 rows) + per-field hint
        // + 3 variable hints + spacer + Save button (3 rows) + saving-to
        // line + footer hint.
        2 + 3 + 1 + 3 + 1 + 3 + 2
    }

    fn pattern_list_preferred_height(&self) -> u16 {
        let count = self
            .pattern_list_editor
            .as_ref()
            .map(|editor| editor.lines.len().max(1) as u16)
            .unwrap_or(1);
        count + 10
    }

    fn link_strategy_preferred_height(&self) -> u16 {
        // Title + description + 1 rectangle (3 rows) + per-field hint
        // + blank line + section title + 3 option lines + spacer (1) +
        // Save button (3 rows) + footer blurb + saving-to line + footer
        // hint.
        18
    }

    fn link_cache_dir_preferred_height(&self) -> u16 {
        // Title + description + 1 rectangle (3 rows) + per-field hint
        // + blank line + section title + 4 didactic lines + spacer (1)
        // + Save button (3 rows) + saving-to line + footer hint.
        18
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        self.mouse_targets.borrow_mut().clear();
        if let Some(err) = &self.error {
            self.render_error(frame, area, err);
            return;
        }
        match self.step {
            SettingsStep::Menu => self.render_menu(frame, area),
            SettingsStep::SetupProject => self.render_menu(frame, area),
            SettingsStep::CopyPatterns => self.render_copy_patterns(frame, area),
            SettingsStep::LinkPatterns => self.render_link_patterns(frame, area),
            SettingsStep::LinkStrategy => self.render_link_strategy(frame, area),
            SettingsStep::LinkCacheDir => self.render_link_cache_dir(frame, area),
            SettingsStep::IgnorePatterns => self.render_ignore_patterns(frame, area),
            SettingsStep::PathTemplate => self.render_path_template(frame, area),
            SettingsStep::Dashboard => self.render_dashboard(frame, area),
            SettingsStep::AiSettings => self.render_ai_settings(frame, area),
            SettingsStep::Notifications => self.render_notifications(frame, area),
            SettingsStep::PostCmd => self.render_post_cmd(frame, area),
            SettingsStep::TerminalCmd => self.render_terminal_cmd(frame, area),
            SettingsStep::DeleteBranch => self.render_delete_branch(frame, area),
            SettingsStep::CopySettings => self.render_copy_settings(frame, area),
            SettingsStep::CheckUpdates => self.render_check_updates(frame, area),
        }
    }

    fn render_error(&self, frame: &mut Frame, area: Rect, err: &str) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new("Configuration Error").style(Style::default().fg(colors::ERROR)),
            chunks[0],
        );
        frame.render_widget(Paragraph::new(err.to_string()), chunks[1]);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("Please edit the configuration file at: "),
                Span::styled(
                    self.config_path.clone(),
                    Style::default()
                        .fg(colors::PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            chunks[2],
        );
        frame.render_widget(
            Paragraph::new("Or press 'r' to reset to default settings, any other key to go back")
                .style(Style::default().fg(colors::MUTED)),
            chunks[3],
        );
    }

    fn render_menu(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(area);
        let header = Line::from(vec![
            Span::raw("Configuration file: "),
            Span::styled(
                self.config_path.clone(),
                Style::default()
                    .fg(colors::PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        frame.render_widget(Paragraph::new(header), chunks[0]);
        if let Some(s) = &self.select {
            s.render(frame, chunks[1]);
        }
    }

    fn render_copy_patterns(&self, frame: &mut Frame, area: Rect) {
        self.render_pattern_list_page(
            frame,
            area,
            "Copy Patterns",
            "Files or globs copied from the source checkout into each new worktree:",
            "worktreeCopyPatterns",
            colors::SUCCESS,
            Some("Use one line per pattern. Great for .env files, editor settings, and local config files you always need."),
        );
    }

    fn render_ignore_patterns(&self, frame: &mut Frame, area: Rect) {
        self.render_pattern_list_page(
            frame,
            area,
            "Ignore Patterns",
            "Files or globs that should never be copied into new worktrees:",
            "worktreeCopyIgnores",
            colors::ERROR,
            Some("Use this to avoid dragging large caches, build artifacts, or machine-specific junk into every worktree."),
        );
    }

    fn render_link_patterns(&self, frame: &mut Frame, area: Rect) {
        self.render_pattern_list_page(
            frame,
            area,
            "Link Patterns",
            "Directories shared through Wisetree's dependency cache instead of duplicated per worktree:",
            "worktreeLinkPatterns",
            colors::BRAND,
            Some("These are usually heavy dependency folders like node_modules, target, vendor, or .venv that you want to install once and reuse."),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn render_pattern_list_page(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        description: &str,
        field_name: &str,
        accent: ratatui::style::Color,
        didactic: Option<&str>,
    ) {
        let editor = match &self.pattern_list_editor {
            Some(editor) => editor,
            None => return,
        };

        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);

        let block_height = editor.lines.len().max(1) as u16 + 2;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(block_height),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(title, title_style))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(description, muted_style))),
            chunks[1],
        );

        render_pattern_list_block(frame, chunks[2], field_name, editor, accent);

        let aux_text = if let Some(didactic) = didactic {
            if editor.editing() {
                "Each line is one pattern. Enter inserts a new line; Esc finishes editing."
            } else {
                didactic
            }
        } else if editor.editing() {
            "Each line is one pattern. Enter inserts a new line; Esc finishes editing."
        } else {
            "Each line is one pattern. Empty lines are ignored when you save."
        };
        frame.render_widget(Paragraph::new(aux_text).style(dim_muted_style), chunks[3]);

        self.render_pattern_list_save_button(frame, chunks[5], editor);

        let target = self.config_path.clone();
        let saving_line = Line::from(vec![
            Span::styled("Saving to: ", Style::default().fg(colors::MUTED)),
            Span::styled(target, Style::default().fg(colors::EMPHASIS)),
        ]);
        frame.render_widget(Paragraph::new(saving_line), chunks[6]);

        let hint = if editor.editing() {
            "Editing: Enter newline • Ctrl/Alt word movement • Ctrl+W / Alt+Backspace delete word • Ctrl+U/K kill line • Esc finish"
        } else {
            "↑↓ move • Enter edit/Save • Esc back"
        };
        frame.render_widget(Paragraph::new(hint).style(dim_muted_style), chunks[7]);
    }

    fn render_link_strategy(&self, frame: &mut Frame, area: Rect) {
        let editor = match &self.link_strategy_editor {
            Some(e) => e,
            None => return,
        };

        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let info_style = Style::default().fg(colors::INFO);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line("Link Strategy", title_style))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "How Wisetree prepares the shared cache before linking it into each worktree:",
                muted_style,
            ))),
            chunks[1],
        );

        let is_selected = matches!(editor.selection, LinkStrategySelection::Rect);
        let is_focused = is_selected;
        let border_color = match editor.status {
            LinkStrategyRectStatus::Unchanged => colors::WHITE,
            LinkStrategyRectStatus::Modified => colors::ACCENT,
            LinkStrategyRectStatus::Saved => colors::SUCCESS,
        };
        let show_selection_marker = is_selected;
        let content_style = if is_focused {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::WHITE)
        };
        let border_style = Style::default().fg(border_color);
        let inner_line = Line::from(Span::styled(editor.value.clone(), content_style));
        let title_line = if show_selection_marker {
            Line::from(vec![
                Span::styled(
                    POST_CMD_SELECTION_MARKER,
                    Style::default().fg(colors::ACCENT),
                ),
                Span::styled("worktreeLinkStrategy ", info_style),
            ])
        } else {
            Line::from(Span::styled(" worktreeLinkStrategy ", info_style))
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(border_style)
            .padding(Padding::horizontal(1))
            .title(title_line);
        frame.render_widget(Paragraph::new(inner_line).block(block), chunks[2]);

        let hint_line = Line::from(vec![
            Span::styled("  ↳ ", muted_style),
            Span::styled(
                "Press Enter to cycle between the three strategies",
                muted_style,
            ),
        ]);
        frame.render_widget(Paragraph::new(hint_line), chunks[3]);

        frame.render_widget(Paragraph::new(""), chunks[4]);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line("Link Options", info_style))),
            chunks[5],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "  • CreateEmpty: start with an empty shared directory; installs fill it later.",
                muted_style,
            ))),
            chunks[6],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "  • SeedFromSource: seed the cache from this checkout first, then reuse it.",
                muted_style,
            ))),
            chunks[7],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "  • SeedIfPresent: only link when the source directory already exists locally.",
                muted_style,
            ))),
            chunks[8],
        );

        self.render_link_strategy_save_button(frame, chunks[10], editor);

        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Shared links let heavy dependency folders be installed once and reused across worktrees.",
                dim_muted_style,
            ))),
            chunks[11],
        );

        let target = self.config_path.clone();
        let saving_line = Line::from(vec![
            Span::styled("Saving to: ", Style::default().fg(colors::MUTED)),
            Span::styled(target, Style::default().fg(colors::EMPHASIS)),
        ]);
        frame.render_widget(Paragraph::new(saving_line), chunks[12]);

        let hint = "↑↓ to move • Enter to toggle/Save • Esc to go back";
        frame.render_widget(Paragraph::new(hint).style(dim_muted_style), chunks[13]);
    }

    fn render_link_cache_dir(&self, frame: &mut Frame, area: Rect) {
        let editor = match &self.link_cache_dir_editor {
            Some(e) => e,
            None => return,
        };

        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let info_style = Style::default().fg(colors::INFO);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line("Link Cache Dir", title_style))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Optional override for the shared dependency cache root:",
                muted_style,
            ))),
            chunks[1],
        );

        let is_editing = editor.editing();
        let is_selected = matches!(editor.selection, LinkCacheDirSelection::Rect);
        let is_focused = is_selected || is_editing;
        let border_color = match editor.status {
            LinkCacheDirRectStatus::Unchanged => colors::WHITE,
            LinkCacheDirRectStatus::Editing => colors::WARNING,
            LinkCacheDirRectStatus::Modified => colors::ACCENT,
            LinkCacheDirRectStatus::Saved => colors::SUCCESS,
        };
        let show_selection_marker = is_selected && !is_editing;
        let content_style = if is_focused {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::WHITE)
        };
        let border_style = Style::default().fg(border_color);
        let inner_line = if is_editing {
            self.link_cache_dir_input
                .as_ref()
                .map(|prompt| prompt.inline_line())
                .unwrap_or_else(|| Line::from(Span::raw(editor.value.clone())))
        } else if editor.value.is_empty() {
            let placeholder = Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM);
            Line::from(Span::styled("(none)", placeholder))
        } else {
            Line::from(Span::styled(editor.value.clone(), content_style))
        };
        let title_line = if show_selection_marker {
            Line::from(vec![
                Span::styled(
                    POST_CMD_SELECTION_MARKER,
                    Style::default().fg(colors::ACCENT),
                ),
                Span::styled("worktreeLinkCacheDir ", info_style),
            ])
        } else {
            Line::from(Span::styled(" worktreeLinkCacheDir ", info_style))
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(border_style)
            .padding(Padding::horizontal(1))
            .title(title_line);
        frame.render_widget(Paragraph::new(inner_line).block(block), chunks[2]);

        let hint_line = Line::from(vec![
            Span::styled("  ↳ ", muted_style),
            Span::styled(
                "Leave blank to use Wisetree's default per-repository cache root",
                muted_style,
            ),
        ]);
        frame.render_widget(Paragraph::new(hint_line), chunks[3]);

        frame.render_widget(Paragraph::new(""), chunks[4]);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line("Why this exists:", info_style))),
            chunks[5],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "  • The shared cache avoids reinstalling heavy dependencies in every worktree.",
                muted_style,
            ))),
            chunks[6],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "  • Override it when you want the cache on a faster disk or larger volume.",
                muted_style,
            ))),
            chunks[7],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "  • Default: ~/.wisetree/cache/<repo-id>",
                muted_style,
            ))),
            chunks[8],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "  • Variables: $BASE_PATH, $WORKTREE_PATH, $BRANCH_NAME, $SOURCE_BRANCH",
                muted_style,
            ))),
            chunks[9],
        );

        self.render_link_cache_dir_save_button(frame, chunks[11], editor);

        let target = self.config_path.clone();
        let saving_line = Line::from(vec![
            Span::styled("Saving to: ", Style::default().fg(colors::MUTED)),
            Span::styled(target, Style::default().fg(colors::EMPHASIS)),
        ]);
        frame.render_widget(Paragraph::new(saving_line), chunks[12]);

        let hint = if is_editing {
            "Editing: same cursor shortcuts as other inputs. Enter confirms, Esc cancels"
        } else {
            "↑↓ to move • Enter to edit/Save • Esc to go back"
        };
        frame.render_widget(Paragraph::new(hint).style(dim_muted_style), chunks[13]);
    }

    fn render_link_strategy_save_button(
        &self,
        frame: &mut Frame,
        area: Rect,
        editor: &LinkStrategyEditor,
    ) {
        let save_label = "Save";
        let save_width = save_label.chars().count() as u16 + 4;
        let side = area.width.saturating_sub(save_width) / 2;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(save_width),
                Constraint::Min(0),
            ])
            .split(area);

        let save_selected = editor.selection == LinkStrategySelection::Save;
        let save_text_style = if save_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        };
        let save_border = Style::default().fg(colors::SUCCESS);

        let save_box = Paragraph::new(Line::from(Span::styled(save_label, save_text_style))).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(save_border)
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(save_box, cols[1]);
        self.push_mouse_target(SettingsMouseTarget::LinkStrategySave, cols[1]);
    }

    fn render_link_cache_dir_save_button(
        &self,
        frame: &mut Frame,
        area: Rect,
        editor: &LinkCacheDirEditor,
    ) {
        let save_label = "Save";
        let save_width = save_label.chars().count() as u16 + 4;
        let side = area.width.saturating_sub(save_width) / 2;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(save_width),
                Constraint::Min(0),
            ])
            .split(area);

        let save_selected = editor.selection == LinkCacheDirSelection::Save;
        let save_text_style = if save_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        };
        let save_border = Style::default().fg(colors::SUCCESS);

        let save_box = Paragraph::new(Line::from(Span::styled(save_label, save_text_style))).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(save_border)
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(save_box, cols[1]);
        self.push_mouse_target(SettingsMouseTarget::LinkCacheDirSave, cols[1]);
    }

    fn render_pattern_list_save_button(
        &self,
        frame: &mut Frame,
        area: Rect,
        editor: &PatternListEditor,
    ) {
        let save_label = "Save";
        let save_width = save_label.chars().count() as u16 + 4;
        let side = area.width.saturating_sub(save_width) / 2;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(save_width),
                Constraint::Min(0),
            ])
            .split(area);

        let save_selected = editor.selection == PatternListSelection::Save;
        let save_text_style = if save_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        };
        let save_border = Style::default().fg(colors::SUCCESS);

        let save_box = Paragraph::new(Line::from(Span::styled(save_label, save_text_style))).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(save_border)
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(save_box, cols[1]);
        self.push_mouse_target(SettingsMouseTarget::PatternListSave, cols[1]);
    }

    fn render_dashboard(&self, frame: &mut Frame, area: Rect) {
        let editor = match &self.dashboard_editor {
            Some(e) => e,
            None => return,
        };

        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);

        let mut constraints: Vec<Constraint> = vec![Constraint::Length(1), Constraint::Length(1)];
        for _ in DashboardField::ALL.iter() {
            constraints.push(Constraint::Length(3));
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Min(0));
        constraints.push(Constraint::Length(3));
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(1));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line("Dashboard", title_style))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Live dashboard settings (edit each field):",
                muted_style,
            ))),
            chunks[1],
        );

        let editing_idx = editor.editing_index();
        let mut cursor = 2usize;
        for (i, _field) in DashboardField::ALL.iter().enumerate() {
            let rect_chunk = chunks[cursor];
            let hint_chunk = chunks[cursor + 1];
            self.render_dashboard_rectangle(frame, rect_chunk, hint_chunk, editor, i, editing_idx);
            cursor += 2;
        }

        // The cursor now points at `Min(0)`. Save button is one past that,
        // then `Saving to:`, then the bottom navigation hint.
        let save_chunk = chunks[cursor + 1];
        self.render_dashboard_save_button(frame, save_chunk, editor);

        let target = self.config_path.clone();
        let saving_line = Line::from(vec![
            Span::styled("Saving to: ", Style::default().fg(colors::MUTED)),
            Span::styled(target, Style::default().fg(colors::EMPHASIS)),
        ]);
        frame.render_widget(Paragraph::new(saving_line), chunks[cursor + 2]);

        let hint = if editing_idx.is_some() {
            "Editing: same cursor shortcuts as other inputs. Enter confirms, Esc cancels"
        } else {
            "↑↓ to move • Enter to edit/toggle/Save • Esc to go back"
        };
        frame.render_widget(
            Paragraph::new(hint).style(dim_muted_style),
            chunks[cursor + 3],
        );
    }

    /// Render the shared "Current free models:" chip row beneath the AI
    /// Settings slots, plus a per-state action hint. The cursor sits in
    /// `AiSettingsSelection::FreeModels(i)`; the focused chip lights up ACCENT
    /// and a chip matching the targeted slot's model lights up SUCCESS. Enter
    /// stages the focused chip into the most-recently-focused command.
    fn render_ai_settings_free_models(&self, frame: &mut Frame, chips_area: Rect, hint_area: Rect) {
        let muted_style = Style::default().fg(colors::MUTED);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);
        let info_style = Style::default().fg(colors::INFO);

        let focused_chip = self
            .ai_settings_editor
            .as_ref()
            .and_then(|e| match e.selection {
                AiSettingsSelection::FreeModels(i) => Some(i),
                _ => None,
            });

        let mut spans: Vec<Span> = vec![
            Span::styled("  ↳ ", muted_style),
            Span::styled("Current free models: ", info_style),
        ];
        match &self.free_models {
            None => {
                spans.push(Span::styled("(loading from opencode)…", dim_muted_style));
            }
            Some(Err(message)) => {
                spans.push(Span::styled(
                    format!("(unavailable: {message})"),
                    Style::default().fg(colors::ERROR),
                ));
            }
            Some(Ok(models)) if models.is_empty() => {
                spans.push(Span::styled("(none)", dim_muted_style));
            }
            Some(Ok(models)) => {
                let active = self
                    .ai_settings_editor
                    .as_ref()
                    .map(|e| {
                        AiSettingsEditor::slot(e.last_rect)
                            .get(&e.ai)
                            .model
                            .trim()
                            .to_string()
                    })
                    .unwrap_or_default();
                for (i, model) in models.iter().enumerate() {
                    if i > 0 {
                        spans.push(Span::styled(" ", muted_style));
                    }
                    let is_focused = focused_chip == Some(i);
                    let is_active = !active.is_empty() && active == *model;
                    let (left, right) = if is_focused { ("[", "]") } else { (" ", " ") };
                    let mut chip_style = if is_focused {
                        Style::default()
                            .fg(colors::ACCENT)
                            .add_modifier(Modifier::BOLD)
                    } else if is_active {
                        Style::default().fg(colors::SUCCESS)
                    } else {
                        Style::default().fg(colors::EMPHASIS)
                    };
                    if !is_focused && !is_active {
                        chip_style = chip_style.add_modifier(Modifier::DIM);
                    }
                    spans.push(Span::styled(left.to_string(), chip_style));
                    spans.push(Span::styled(model.clone(), chip_style));
                    spans.push(Span::styled(right.to_string(), chip_style));
                }
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), chips_area);

        let action_hint = match &self.free_models {
            Some(Ok(models)) if !models.is_empty() && focused_chip.is_some() => {
                "    ← →/h l cycle • Enter stages into the focused command • ↑↓ leave row"
            }
            Some(Ok(models)) if !models.is_empty() => {
                "    Move down into the row to quick-pick a free model"
            }
            Some(Err(_)) => "    Free-model picker disabled until opencode CLI is reachable",
            _ => "",
        };
        if !action_hint.is_empty() {
            frame.render_widget(
                Paragraph::new(action_hint).style(dim_muted_style),
                hint_area,
            );
        }
    }

    /// Render the AI Settings sub-screen: one rectangle per AI command (model +
    /// thinking strength), a shared free-model chip row, and a Save button.
    fn render_ai_settings(&self, frame: &mut Frame, area: Rect) {
        let editor = match &self.ai_settings_editor {
            Some(e) => e,
            None => return,
        };

        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);

        let mut constraints: Vec<Constraint> = vec![Constraint::Length(1), Constraint::Length(1)];
        for _ in AiSlot::ALL.iter() {
            constraints.push(Constraint::Length(3));
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Length(1)); // chips line
        constraints.push(Constraint::Length(1)); // chip-action hint
        constraints.push(Constraint::Min(0));
        constraints.push(Constraint::Length(3)); // Save
        constraints.push(Constraint::Length(1)); // bottom hint

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        frame.render_widget(
            Paragraph::new(Line::from(branded_line("AI Models", title_style))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Pick a model + thinking strength per AI command:",
                muted_style,
            ))),
            chunks[1],
        );

        let mut cursor = 2usize;
        for (i, _slot) in AiSlot::ALL.iter().enumerate() {
            self.render_ai_settings_rectangle(frame, chunks[cursor], chunks[cursor + 1], editor, i);
            cursor += 2;
        }

        self.render_ai_settings_free_models(frame, chunks[cursor], chunks[cursor + 1]);
        cursor += 2;

        // `cursor` now points at `Min(0)`; Save is one past it.
        self.render_ai_settings_save_button(frame, chunks[cursor + 1], editor);

        let on_chips = matches!(editor.selection, AiSettingsSelection::FreeModels(_));
        let hint = if on_chips {
            "← → cycle chips • Enter stages • ↑↓ leave row • Esc back to Dashboard"
        } else {
            "↑↓ move • ← → thinking strength • Enter pick model/Save • Esc back to Dashboard"
        };
        frame.render_widget(
            Paragraph::new(hint).style(dim_muted_style),
            chunks[cursor + 2],
        );
    }

    fn render_ai_settings_rectangle(
        &self,
        frame: &mut Frame,
        rect_area: Rect,
        hint_area: Rect,
        editor: &AiSettingsEditor,
        idx: usize,
    ) {
        let muted_style = Style::default().fg(colors::MUTED);
        let info_style = Style::default().fg(colors::INFO);

        let slot = AiSlot::ALL[idx];
        let model = slot.get(&editor.ai);
        let status = editor.statuses[idx];
        let is_selected = matches!(editor.selection, AiSettingsSelection::Rect(j) if j == idx);
        let border_color = match status {
            DashboardRectStatus::Unchanged => colors::WHITE,
            DashboardRectStatus::Editing => colors::WARNING,
            DashboardRectStatus::Modified => colors::ACCENT,
            DashboardRectStatus::Saved => colors::SUCCESS,
        };
        let content_style = if is_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let border_style = Style::default().fg(border_color);

        let mut inner_line = if model.model.trim().is_empty() {
            Line::from(Span::styled(
                "(none — press Enter to choose a model)",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ))
        } else {
            Line::from(vec![
                Span::raw(model.model.clone()),
                Span::styled(
                    format!(
                        "  ·  {}",
                        reasoning_level_label(
                            &self.ai_thinking_ladder(model.model.trim()),
                            &model.thinking,
                        )
                    ),
                    Style::default()
                        .fg(colors::MUTED)
                        .add_modifier(Modifier::DIM),
                ),
            ])
        };
        inner_line.style = content_style;

        let title_line = if is_selected {
            Line::from(vec![
                Span::styled(
                    POST_CMD_SELECTION_MARKER,
                    Style::default().fg(colors::ACCENT),
                ),
                Span::styled(format!("{} ", slot.label()), info_style),
            ])
        } else {
            Line::from(Span::styled(format!(" {} ", slot.label()), info_style))
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(border_style)
            .padding(Padding::horizontal(1))
            .title(title_line);
        frame.render_widget(Paragraph::new(inner_line).block(block), rect_area);

        let hint_line = Line::from(vec![
            Span::styled("  ↳ ", muted_style),
            Span::styled(slot.hint(), muted_style),
        ]);
        frame.render_widget(Paragraph::new(hint_line), hint_area);
    }

    fn render_ai_settings_save_button(
        &self,
        frame: &mut Frame,
        area: Rect,
        editor: &AiSettingsEditor,
    ) {
        let save_label = "Save";
        let save_width = save_label.chars().count() as u16 + 4;
        let side = area.width.saturating_sub(save_width) / 2;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(save_width),
                Constraint::Min(0),
            ])
            .split(area);

        let save_selected = editor.selection == AiSettingsSelection::Save;
        let save_text_style = if save_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        };

        let save_box = Paragraph::new(Line::from(Span::styled(save_label, save_text_style))).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(Style::default().fg(colors::SUCCESS))
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(save_box, cols[1]);
        self.push_mouse_target(SettingsMouseTarget::AiSettingsSave, cols[1]);
    }

    fn render_dashboard_rectangle(
        &self,
        frame: &mut Frame,
        rect_area: Rect,
        hint_area: Rect,
        editor: &DashboardEditor,
        idx: usize,
        editing_idx: Option<usize>,
    ) {
        let muted_style = Style::default().fg(colors::MUTED);
        let info_style = Style::default().fg(colors::INFO);

        let value = &editor.values[idx];
        let status = editor.statuses[idx];
        let field = editor.field(idx);
        let is_selected = matches!(editor.selection, DashboardSelection::Rect(j) if j == idx);
        let is_editing = editing_idx == Some(idx);
        let is_focused = is_selected || is_editing;
        let border_color = match status {
            DashboardRectStatus::Unchanged => colors::WHITE,
            DashboardRectStatus::Editing => colors::WARNING,
            DashboardRectStatus::Modified => colors::ACCENT,
            DashboardRectStatus::Saved => colors::SUCCESS,
        };
        let show_selection_marker = is_selected && !is_editing;
        let content_style = if is_focused {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let border_style = Style::default().fg(border_color);
        let mut inner_line = if is_editing {
            self.dashboard_input
                .as_ref()
                .map(|prompt| prompt.inline_line())
                .unwrap_or_else(|| Line::from(Span::raw(value.clone())))
        } else if matches!(field, DashboardField::Ai) {
            // The `ai` rectangle is a gateway into the AI Settings sub-screen;
            // show a compact "configured / total" summary rather than a single
            // model value (per-command models live on the sub-screen).
            let leaves = ai_slot_models(&editor.ai);
            let configured = leaves.iter().filter(|m| !m.model.trim().is_empty()).count();
            Line::from(Span::styled(
                format!("{configured}/{} AI commands configured", leaves.len()),
                Style::default().fg(colors::MUTED),
            ))
        } else if value.is_empty() {
            let placeholder = Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM);
            Line::from(Span::styled("(empty — press Enter to edit)", placeholder))
        } else {
            Line::from(Span::raw(value.clone()))
        };
        inner_line.style = content_style;
        let title_line = if show_selection_marker {
            Line::from(vec![
                Span::styled(
                    POST_CMD_SELECTION_MARKER,
                    Style::default().fg(colors::ACCENT),
                ),
                Span::styled(format!("{} ", field.label()), info_style),
            ])
        } else {
            Line::from(Span::styled(format!(" {} ", field.label()), info_style))
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(border_style)
            .padding(Padding::horizontal(1))
            .title(title_line);
        frame.render_widget(Paragraph::new(inner_line).block(block), rect_area);

        let hint_line = Line::from(vec![
            Span::styled("  ↳ ", muted_style),
            Span::styled(field.hint(), muted_style),
        ]);
        frame.render_widget(Paragraph::new(hint_line), hint_area);
    }

    fn render_dashboard_save_button(
        &self,
        frame: &mut Frame,
        area: Rect,
        editor: &DashboardEditor,
    ) {
        let save_label = "Save";
        let save_width = save_label.chars().count() as u16 + 4;
        let side = area.width.saturating_sub(save_width) / 2;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(save_width),
                Constraint::Min(0),
            ])
            .split(area);

        let save_selected = editor.selection == DashboardSelection::Save;
        let save_text_style = if save_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        };
        let save_border = Style::default().fg(colors::SUCCESS);

        let save_box = Paragraph::new(Line::from(Span::styled(save_label, save_text_style))).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(save_border)
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(save_box, cols[1]);
        self.push_mouse_target(SettingsMouseTarget::DashboardSave, cols[1]);
    }

    fn render_notifications(&self, frame: &mut Frame, area: Rect) {
        let editor = match &self.notifications_editor {
            Some(e) => e,
            None => return,
        };

        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);

        let mut constraints: Vec<Constraint> = vec![Constraint::Length(1), Constraint::Length(1)];
        for _ in NotificationsField::ALL.iter() {
            constraints.push(Constraint::Length(3));
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Min(0));
        constraints.push(Constraint::Length(3));
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(1));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line("Notifications", title_style))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Terminal-bell alerts (Enter toggles each):",
                muted_style,
            ))),
            chunks[1],
        );

        let mut cursor = 2usize;
        for idx in 0..NotificationsField::ALL.len() {
            self.render_notifications_rectangle(
                frame,
                chunks[cursor],
                chunks[cursor + 1],
                editor,
                idx,
            );
            cursor += 2;
        }

        // `cursor` now points at the `Min(0)` spacer; Save is one past it.
        self.render_notifications_save_button(frame, chunks[cursor + 1], editor);

        let saving_line = Line::from(vec![
            Span::styled("Saving to: ", Style::default().fg(colors::MUTED)),
            Span::styled(
                self.config_path.clone(),
                Style::default().fg(colors::EMPHASIS),
            ),
        ]);
        frame.render_widget(Paragraph::new(saving_line), chunks[cursor + 2]);

        frame.render_widget(
            Paragraph::new("↑↓ to move • Enter to toggle/Save • Esc to go back")
                .style(dim_muted_style),
            chunks[cursor + 3],
        );
    }

    fn render_notifications_rectangle(
        &self,
        frame: &mut Frame,
        rect_area: Rect,
        hint_area: Rect,
        editor: &NotificationsEditor,
        idx: usize,
    ) {
        let muted_style = Style::default().fg(colors::MUTED);
        let info_style = Style::default().fg(colors::INFO);

        let enabled = editor.values[idx];
        let status = editor.statuses[idx];
        let field = editor.field(idx);
        let is_selected = matches!(editor.selection, NotificationsSelection::Rect(j) if j == idx);
        let border_color = match status {
            DashboardRectStatus::Unchanged => colors::WHITE,
            DashboardRectStatus::Editing => colors::WARNING,
            DashboardRectStatus::Modified => colors::ACCENT,
            DashboardRectStatus::Saved => colors::SUCCESS,
        };
        let content_style = if is_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let value_text = if enabled { "true" } else { "false" };
        let inner_line = Line::from(Span::styled(value_text, content_style));
        let title_line = if is_selected {
            Line::from(vec![
                Span::styled(
                    POST_CMD_SELECTION_MARKER,
                    Style::default().fg(colors::ACCENT),
                ),
                Span::styled(format!("{} ", field.label()), info_style),
            ])
        } else {
            Line::from(Span::styled(format!(" {} ", field.label()), info_style))
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(border_color))
            .padding(Padding::horizontal(1))
            .title(title_line);
        frame.render_widget(Paragraph::new(inner_line).block(block), rect_area);
        self.push_mouse_target(SettingsMouseTarget::NotificationsToggle(idx), rect_area);

        let hint_line = Line::from(vec![
            Span::styled("  ↳ ", muted_style),
            Span::styled(field.hint(), muted_style),
        ]);
        frame.render_widget(Paragraph::new(hint_line), hint_area);
    }

    fn render_notifications_save_button(
        &self,
        frame: &mut Frame,
        area: Rect,
        editor: &NotificationsEditor,
    ) {
        let save_label = "Save";
        let save_width = save_label.chars().count() as u16 + 4;
        let side = area.width.saturating_sub(save_width) / 2;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(save_width),
                Constraint::Min(0),
            ])
            .split(area);

        let save_selected = editor.selection == NotificationsSelection::Save;
        let save_text_style = if save_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        };

        let save_box = Paragraph::new(Line::from(Span::styled(save_label, save_text_style))).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(Style::default().fg(colors::SUCCESS))
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(save_box, cols[1]);
        self.push_mouse_target(SettingsMouseTarget::NotificationsSave, cols[1]);
    }

    fn notifications_preferred_height(&self) -> u16 {
        // Title + description + rectangles (3 rows each) + hint rows
        // + spacer + Save button (3 rows) + saving-to line + footer hint.
        let rects = NotificationsField::ALL.len() as u16;
        2 + rects * 3 + rects + 1 + 3 + 2
    }

    fn render_path_template(&self, frame: &mut Frame, area: Rect) {
        let editor = match &self.path_template_editor {
            Some(e) => e,
            None => return,
        };

        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let info_style = Style::default().fg(colors::INFO);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // title
                Constraint::Length(1), // description
                Constraint::Length(3), // rectangle
                Constraint::Length(1), // per-field hint
                Constraint::Length(1), // variables label
                Constraint::Length(1), // variable line 1
                Constraint::Length(1), // variable lines
                Constraint::Min(0),    // spacer
                Constraint::Length(3), // save button
                Constraint::Length(1), // saving-to
                Constraint::Length(1), // footer hint
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Worktree Path Template",
                title_style,
            ))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Template for worktree directory paths:",
                muted_style,
            ))),
            chunks[1],
        );

        let is_editing = editor.editing();
        let is_selected = matches!(editor.selection, PathTemplateSelection::Rect);
        let is_focused = is_selected || is_editing;
        let border_color = match editor.status {
            PathTemplateRectStatus::Unchanged => colors::WHITE,
            PathTemplateRectStatus::Editing => colors::WARNING,
            PathTemplateRectStatus::Modified => colors::ACCENT,
            PathTemplateRectStatus::Saved => colors::SUCCESS,
        };
        let show_selection_marker = is_selected && !is_editing;
        let content_style = if is_focused {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let border_style = Style::default().fg(border_color);
        let mut inner_line = if is_editing {
            self.path_template_input
                .as_ref()
                .map(|prompt| prompt.inline_line())
                .unwrap_or_else(|| Line::from(Span::raw(editor.template.clone())))
        } else if editor.template.is_empty() {
            let placeholder = Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM);
            Line::from(Span::styled("(empty — press Enter to edit)", placeholder))
        } else {
            Line::from(Span::raw(editor.template.clone()))
        };
        inner_line.style = content_style;
        let title_line = if show_selection_marker {
            Line::from(vec![
                Span::styled(
                    POST_CMD_SELECTION_MARKER,
                    Style::default().fg(colors::ACCENT),
                ),
                Span::styled("worktreePathTemplate ", info_style),
            ])
        } else {
            Line::from(Span::styled(" worktreePathTemplate ", info_style))
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(border_style)
            .padding(Padding::horizontal(1))
            .title(title_line);
        frame.render_widget(Paragraph::new(inner_line).block(block), chunks[2]);

        let hint_line = Line::from(vec![
            Span::styled("  ↳ ", muted_style),
            Span::styled(
                "Directory path for new worktrees (variables below)",
                muted_style,
            ),
        ]);
        frame.render_widget(Paragraph::new(hint_line), chunks[3]);

        frame.render_widget(
            Paragraph::new(Line::from(branded_line("Available variables:", info_style))),
            chunks[4],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "  • $BASE_PATH - Repository name",
                muted_style,
            ))),
            chunks[5],
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(branded_line(
                    "  • $WORKTREE_PATH - Full worktree path",
                    muted_style,
                )),
                Line::from(branded_line(
                    "  • $BRANCH_NAME - New branch name",
                    muted_style,
                )),
            ]),
            chunks[6],
        );

        self.render_path_template_save_button(frame, chunks[8], editor);

        let target = self.config_path.clone();
        let saving_line = Line::from(vec![
            Span::styled("Saving to: ", Style::default().fg(colors::MUTED)),
            Span::styled(target, Style::default().fg(colors::EMPHASIS)),
        ]);
        frame.render_widget(Paragraph::new(saving_line), chunks[9]);

        let hint = if is_editing {
            "Editing: same cursor shortcuts as other inputs. Enter confirms, Esc cancels"
        } else {
            "↑↓ to move • Enter to edit/Save • Esc to go back"
        };
        frame.render_widget(Paragraph::new(hint).style(dim_muted_style), chunks[10]);
    }

    fn render_path_template_save_button(
        &self,
        frame: &mut Frame,
        area: Rect,
        editor: &PathTemplateEditor,
    ) {
        let save_label = "Save";
        let save_width = save_label.chars().count() as u16 + 4;
        let side = area.width.saturating_sub(save_width) / 2;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(save_width),
                Constraint::Min(0),
            ])
            .split(area);

        let save_selected = editor.selection == PathTemplateSelection::Save;
        let save_text_style = if save_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        };
        let save_border = Style::default().fg(colors::SUCCESS);

        let save_box = Paragraph::new(Line::from(Span::styled(save_label, save_text_style))).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(save_border)
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(save_box, cols[1]);
        self.push_mouse_target(SettingsMouseTarget::PathTemplateSave, cols[1]);
    }

    fn render_copy_settings(&self, frame: &mut Frame, area: Rect) {
        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let emphasis_style = Style::default().fg(colors::EMPHASIS);
        let global_path = self
            .global_config_path
            .clone()
            .unwrap_or_else(|| self.config_path.clone());
        let local_path = self
            .local_config_path
            .clone()
            .unwrap_or_else(|| ".wisetree.json (project local)".to_string());

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(area);

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(branded_line("Copy Settings", title_style)),
                Line::from(branded_line(
                    "Copy the full settings file between global and project-local config.",
                    muted_style,
                )),
            ]),
            chunks[0],
        );

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("Global: ", muted_style),
                    Span::styled(global_path, emphasis_style),
                ]),
                Line::from(vec![
                    Span::styled("Local:  ", muted_style),
                    Span::styled(local_path, emphasis_style),
                ]),
            ]),
            chunks[1],
        );

        if let Some(select) = &self.copy_settings_select {
            select.render(frame, chunks[3]);
        }
    }

    fn render_post_cmd(&self, frame: &mut Frame, area: Rect) {
        let editor = match &self.post_cmd_editor {
            Some(e) => e,
            None => return,
        };

        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let info_style = Style::default().fg(colors::INFO);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Post-Create Commands",
                title_style,
            ))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Commands executed after creating a worktree (in order):",
                muted_style,
            ))),
            chunks[1],
        );

        let editing_idx = editor.editing_index();
        let command_area = chunks[2];
        let visible_range = editor.visible_range((command_area.height / 3) as usize);
        let hidden_above = visible_range.start;
        let hidden_below = editor.commands.len().saturating_sub(visible_range.end);
        let is_scrollable = hidden_above > 0 || hidden_below > 0;
        let command_chunks: Vec<Rect> = (0..visible_range.len())
            .map(|i| Rect {
                x: command_area.x,
                y: command_area.y + (i as u16) * 3,
                width: command_area.width,
                height: 3,
            })
            .collect();

        for (chunk, i) in command_chunks.into_iter().zip(visible_range.clone()) {
            let cmd = &editor.commands[i];
            let status = editor.statuses[i];
            let is_selected = matches!(editor.selection, PostCmdSelection::Rect(j) if j == i);
            let is_editing = editing_idx == Some(i);
            let is_focused = is_selected || is_editing;
            let base_border_color = match status {
                PostCmdRectStatus::Unchanged => colors::WHITE,
                PostCmdRectStatus::Editing => colors::WARNING,
                PostCmdRectStatus::Modified => colors::ACCENT,
                PostCmdRectStatus::MarkedForDeletion => colors::ERROR,
                PostCmdRectStatus::Saved => colors::SUCCESS,
            };
            let (border_color, animating) = editor
                .swap_highlight_start(i)
                .map(|start| (start, self.tick.saturating_sub(start)))
                .filter(|(_, elapsed)| *elapsed < SWAP_ANIM_TICKS)
                .map(|(_, elapsed)| {
                    let progress = elapsed as f32 / SWAP_ANIM_TICKS as f32;
                    let eased = ease_out_cubic(progress);
                    (lerp_rgb(colors::TEAL, base_border_color, eased), true)
                })
                .unwrap_or((base_border_color, false));
            let show_selection_marker = is_selected && !is_editing;
            let content_style = if is_focused {
                Style::default()
                    .fg(colors::WHITE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let border_style = if animating {
                Style::default()
                    .fg(border_color)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(border_color)
            };
            let mut inner_line = if is_editing {
                self.post_cmd_input
                    .as_ref()
                    .map(|prompt| prompt.inline_line())
                    .unwrap_or_else(|| Line::from(Span::raw(cmd.clone())))
            } else if cmd.is_empty() {
                let placeholder = Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM);
                Line::from(Span::styled("(empty — press Enter to edit)", placeholder))
            } else {
                Line::from(Span::raw(cmd.clone()))
            };
            inner_line.style = content_style;
            let title_line = if show_selection_marker {
                Line::from(vec![
                    Span::styled(
                        POST_CMD_SELECTION_MARKER,
                        Style::default().fg(colors::ACCENT),
                    ),
                    Span::styled(format!("postCreateCmd[{}] ", i), info_style),
                ])
            } else {
                Line::from(Span::styled(format!(" postCreateCmd[{}] ", i), info_style))
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(border_style)
                .padding(Padding::horizontal(1))
                .title(title_line);
            frame.render_widget(Paragraph::new(inner_line).block(block), chunk);
        }

        if is_scrollable {
            self.render_post_cmd_scroll_indicator(frame, chunks[3], hidden_above, hidden_below);
        }

        self.render_post_cmd_buttons(frame, chunks[4], editor);

        // Saving-to line.
        let target = self
            .local_config_path
            .clone()
            .unwrap_or_else(|| ".wisetree.json (project local)".to_string());
        let saving_line = Line::from(vec![
            Span::styled("Saving to: ", Style::default().fg(colors::MUTED)),
            Span::styled(target, Style::default().fg(colors::EMPHASIS)),
        ]);
        frame.render_widget(Paragraph::new(saving_line), chunks[5]);

        let var_style = Style::default().fg(colors::INFO);
        let accent_style = Style::default().fg(colors::ACCENT);
        let legend_intro = Line::from(vec![
            Span::styled("Variables ", muted_style),
            Span::styled("(usable in any post-create command)", dim_muted_style),
            Span::styled(":", muted_style),
        ]);
        frame.render_widget(Paragraph::new(legend_intro), chunks[6]);

        let legend_vars = Line::from(vec![
            Span::styled("  ", muted_style),
            Span::styled("$BASE_PATH", var_style),
            Span::styled(" repo dir", muted_style),
            Span::styled("  •  ", accent_style),
            Span::styled("$WORKTREE_PATH", var_style),
            Span::styled(" new worktree path", muted_style),
            Span::styled("  •  ", accent_style),
            Span::styled("$BRANCH_NAME", var_style),
            Span::styled(" new branch", muted_style),
            Span::styled("  •  ", accent_style),
            Span::styled("$SOURCE_BRANCH", var_style),
            Span::styled(" source branch", muted_style),
        ]);
        frame.render_widget(Paragraph::new(legend_vars), chunks[7]);

        let hint = if editing_idx.is_some() {
            "Editing: same cursor shortcuts as other inputs. Enter confirms, Esc cancels"
        } else if is_scrollable {
            "▲/▼ scroll • Shift+K reorder up • Shift+J reorder down • Enter edit/Create/Save • Backspace toggles delete • ←→ between buttons • Esc back"
        } else {
            "↑↓ move • Shift+K reorder up • Shift+J reorder down • Enter edit/Create/Save • Backspace toggles delete • ←→ between buttons • Esc back"
        };
        frame.render_widget(Paragraph::new(hint).style(dim_muted_style), chunks[8]);
    }

    fn render_post_cmd_scroll_indicator(
        &self,
        frame: &mut Frame,
        area: Rect,
        hidden_above: usize,
        hidden_below: usize,
    ) {
        let muted = Style::default().fg(colors::MUTED);
        let inactive = muted.add_modifier(Modifier::DIM);
        let emphasis = Style::default().fg(colors::EMPHASIS);
        let accent = Style::default()
            .fg(colors::ACCENT)
            .add_modifier(Modifier::BOLD);
        let info = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);

        let mut spans = Vec::new();
        if hidden_above > 0 {
            spans.push(Span::styled("▲", accent));
            spans.push(Span::styled(format!(" {hidden_above} above"), emphasis));
        } else {
            spans.push(Span::styled("▲ top", inactive));
        }

        spans.push(Span::styled(" • ", muted));
        spans.push(Span::styled("▲/▼", info));
        spans.push(Span::styled(" to scroll", muted));
        spans.push(Span::styled(" • ", muted));

        if hidden_below > 0 {
            spans.push(Span::styled("▼", accent));
            spans.push(Span::styled(format!(" {hidden_below} below"), emphasis));
        } else {
            spans.push(Span::styled("▼ bottom", inactive));
        }

        frame.render_widget(
            Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
            area,
        );
    }

    fn render_post_cmd_buttons(&self, frame: &mut Frame, area: Rect, editor: &PostCmdEditor) {
        let create_label = "Create";
        let save_label = "Save";
        let create_width = create_label.chars().count() as u16 + 4;
        let save_width = save_label.chars().count() as u16 + 4;
        let gap: u16 = 2;
        let total_width = create_width + save_width + gap;
        let side = area.width.saturating_sub(total_width) / 2;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(create_width),
                Constraint::Length(gap),
                Constraint::Length(save_width),
                Constraint::Min(0),
            ])
            .split(area);

        let create_selected = editor.selection == PostCmdSelection::Create;
        let save_selected = editor.selection == PostCmdSelection::Save;

        let create_text_style = if create_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        };
        let save_text_style = if save_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        };

        let create_border = Style::default().fg(colors::WHITE);
        let save_border = Style::default().fg(colors::SUCCESS);

        let create_box = Paragraph::new(Line::from(Span::styled(create_label, create_text_style)))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Plain)
                    .border_style(create_border)
                    .padding(Padding::horizontal(1)),
            );
        let save_box = Paragraph::new(Line::from(Span::styled(save_label, save_text_style))).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(save_border)
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(create_box, cols[1]);
        frame.render_widget(save_box, cols[3]);
        self.push_mouse_target(SettingsMouseTarget::PostCmdCreate, cols[1]);
        self.push_mouse_target(SettingsMouseTarget::PostCmdSave, cols[3]);
    }

    fn render_terminal_cmd(&self, frame: &mut Frame, area: Rect) {
        let editor = match &self.terminal_cmd_editor {
            Some(e) => e,
            None => return,
        };

        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let info_style = Style::default().fg(colors::INFO);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // title
                Constraint::Length(1), // description
                Constraint::Length(3), // rectangle
                Constraint::Length(1), // per-field hint
                Constraint::Min(0),    // spacer
                Constraint::Length(3), // save button
                Constraint::Length(1), // saving-to
                Constraint::Length(1), // footer hint
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line("Terminal Command", title_style))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Command to open terminal in new worktree ($WORKTREE_PATH available):",
                muted_style,
            ))),
            chunks[1],
        );

        let is_editing = editor.editing();
        let is_selected = matches!(editor.selection, TerminalCmdSelection::Rect);
        let is_focused = is_selected || is_editing;
        let border_color = match editor.status {
            TerminalCmdRectStatus::Unchanged => colors::WHITE,
            TerminalCmdRectStatus::Editing => colors::WARNING,
            TerminalCmdRectStatus::Modified => colors::ACCENT,
            TerminalCmdRectStatus::Saved => colors::SUCCESS,
        };
        let show_selection_marker = is_selected && !is_editing;
        let content_style = if is_focused {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let border_style = Style::default().fg(border_color);
        let mut inner_line = if is_editing {
            self.terminal_cmd_input
                .as_ref()
                .map(|prompt| prompt.inline_line())
                .unwrap_or_else(|| Line::from(Span::raw(editor.command.clone())))
        } else if editor.command.is_empty() {
            let placeholder = Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM);
            Line::from(Span::styled("(none)", placeholder))
        } else {
            Line::from(Span::raw(editor.command.clone()))
        };
        inner_line.style = content_style;
        let title_line = if show_selection_marker {
            Line::from(vec![
                Span::styled(
                    POST_CMD_SELECTION_MARKER,
                    Style::default().fg(colors::ACCENT),
                ),
                Span::styled("terminalCommand ", info_style),
            ])
        } else {
            Line::from(Span::styled(" terminalCommand ", info_style))
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(border_style)
            .padding(Padding::horizontal(1))
            .title(title_line);
        frame.render_widget(Paragraph::new(inner_line).block(block), chunks[2]);

        let hint_line = Line::from(vec![
            Span::styled("  ↳ ", muted_style),
            Span::styled(
                "Shell command (e.g., 'code $WORKTREE_PATH') — leave empty to disable",
                muted_style,
            ),
        ]);
        frame.render_widget(Paragraph::new(hint_line), chunks[3]);

        self.render_terminal_cmd_save_button(frame, chunks[5], editor);

        let target = self.config_path.clone();
        let saving_line = Line::from(vec![
            Span::styled("Saving to: ", Style::default().fg(colors::MUTED)),
            Span::styled(target, Style::default().fg(colors::EMPHASIS)),
        ]);
        frame.render_widget(Paragraph::new(saving_line), chunks[6]);

        let hint = if is_editing {
            "Editing: same cursor shortcuts as other inputs. Enter confirms, Esc cancels"
        } else {
            "↑↓ to move • Enter to edit/Save • Esc to go back"
        };
        frame.render_widget(Paragraph::new(hint).style(dim_muted_style), chunks[7]);
    }

    fn render_terminal_cmd_save_button(
        &self,
        frame: &mut Frame,
        area: Rect,
        editor: &TerminalCmdEditor,
    ) {
        let save_label = "Save";
        let save_width = save_label.chars().count() as u16 + 4;
        let side = area.width.saturating_sub(save_width) / 2;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(save_width),
                Constraint::Min(0),
            ])
            .split(area);

        let save_selected = editor.selection == TerminalCmdSelection::Save;
        let save_text_style = if save_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        };
        let save_border = Style::default().fg(colors::SUCCESS);

        let save_box = Paragraph::new(Line::from(Span::styled(save_label, save_text_style))).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(save_border)
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(save_box, cols[1]);
        self.push_mouse_target(SettingsMouseTarget::TerminalCmdSave, cols[1]);
    }

    fn build_delete_branch_dialog(&self) -> ConfirmationModal {
        let default_choice = if self.config.delete_branch_with_worktree {
            ConfirmationChoice::Confirm
        } else {
            ConfirmationChoice::Cancel
        };

        ConfirmationModal::new()
            .with_title("Delete Branch with Worktree")
            .with_subtitle(
                "Also delete the associated git branch when deleting a worktree?\n\n\
Safety features:\n\
  • Never deletes current or default branches\n\
  • Shows branch status (commits ahead/behind)\n\
  • Requires explicit confirmation",
            )
            .with_confirm_text("Yes")
            .with_cancel_text("No")
            .with_color_value(colors::WARNING)
            .with_selected(default_choice)
    }

    fn render_delete_branch(&self, frame: &mut Frame, area: Rect) {
        // Render the settings menu underneath so the modal floats over the
        // list the user just navigated from.
        self.render_menu(frame, area);

        if let Some(dialog) = &self.delete_branch_dialog {
            dialog.render(frame, area);
        } else {
            self.build_delete_branch_dialog().render(frame, area);
        }
    }

    fn render_check_updates(&self, frame: &mut Frame, area: Rect) {
        if self.checking_updates {
            StatusIndicator::new(Status::Loading, UPDATE_CHECKING)
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }
        if let Some(source) = self.upgrading_source {
            let msg = format!("Upgrading via {}...", source.label());
            StatusIndicator::new(Status::Loading, &msg)
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }
        let result = match &self.update_result {
            Some(r) => r,
            None => {
                StatusIndicator::new(Status::Loading, UPDATE_CHECKING)
                    .with_tick(self.tick)
                    .render(frame, area);
                return;
            }
        };

        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);

        let sources = [UpdateSource::Npm, UpdateSource::Homebrew];
        let mut constraints: Vec<Constraint> = vec![
            Constraint::Length(1), // title
            Constraint::Length(1), // description
        ];
        for _ in 0..sources.len() {
            constraints.push(Constraint::Length(3)); // rectangle
            constraints.push(Constraint::Length(1)); // per-source hint
        }
        constraints.push(Constraint::Length(1)); // outcome / blank
        constraints.push(Constraint::Min(0));
        constraints.push(Constraint::Length(1)); // footer hint

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        frame.render_widget(
            Paragraph::new(Line::from(branded_line(UPDATE_CHECK_MENU, title_style))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                &format!(
                    "Current version: v{}. Select a source and press Enter to upgrade:",
                    result.current_version
                ),
                muted_style,
            ))),
            chunks[1],
        );

        for (i, source) in sources.iter().enumerate() {
            let rect_chunk = chunks[2 + i * 2];
            let hint_chunk = chunks[2 + i * 2 + 1];
            self.render_update_rectangle(frame, rect_chunk, hint_chunk, *source, result);
        }

        let outcome_chunk = chunks[2 + sources.len() * 2];
        if let Some(outcome) = &self.upgrade_outcome {
            let style = if outcome.success {
                Style::default().fg(colors::SUCCESS)
            } else {
                Style::default().fg(colors::ERROR)
            };
            let prefix = if outcome.success { "✓ " } else { "✗ " };
            let label = outcome.source.label();
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("{prefix}{label}: {}", outcome.message),
                    style,
                ))),
                outcome_chunk,
            );
        }

        let footer_chunk = chunks[2 + sources.len() * 2 + 2];
        frame.render_widget(
            Paragraph::new("↑↓ to move • Enter to upgrade • Esc to go back").style(dim_muted_style),
            footer_chunk,
        );
    }

    fn render_update_rectangle(
        &self,
        frame: &mut Frame,
        rect_area: Rect,
        hint_area: Rect,
        source: UpdateSource,
        result: &MultiSourceUpdateResult,
    ) {
        let muted_style = Style::default().fg(colors::MUTED);
        let info_style = Style::default().fg(colors::INFO);

        let is_selected = self.update_selection == source;
        let per_source = result.source(source);

        // Body inside the rectangle: latest version, or error string.
        let body_text = if let Some(err) = &per_source.error {
            format!("error: {err}")
        } else if let Some(latest) = &per_source.latest_version {
            format!("v{latest}")
        } else {
            "(unavailable)".to_string()
        };

        let content_style = if is_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let mut inner_line = Line::from(Span::raw(body_text));
        if is_selected {
            inner_line.spans.insert(
                0,
                Span::styled(
                    POST_CMD_SELECTION_MARKER,
                    Style::default().fg(colors::ACCENT),
                ),
            );
        }
        inner_line.style = content_style;

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(colors::SUCCESS))
            .padding(Padding::horizontal(1))
            .title(Span::styled(format!(" {} ", source.label()), info_style));
        frame.render_widget(Paragraph::new(inner_line).block(block), rect_area);

        let hint_text = if per_source.error.is_some() {
            format!(
                "  ↳ Press Enter to attempt: {}",
                source.upgrade_command_display()
            )
        } else if per_source.has_update {
            format!(
                "  ↳ Press Enter to upgrade with: {}",
                source.upgrade_command_display()
            )
        } else {
            format!(
                "  ↳ Already up to date. Press Enter to re-run: {}",
                source.upgrade_command_display()
            )
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint_text, muted_style))),
            hint_area,
        );
    }
}

fn link_strategy_label(strategy: LinkStrategy) -> &'static str {
    match strategy {
        LinkStrategy::CreateEmpty => "CreateEmpty",
        LinkStrategy::SeedFromSource => "SeedFromSource",
        LinkStrategy::SeedIfPresent => "SeedIfPresent",
    }
}

fn render_pattern_list_block(
    frame: &mut Frame,
    area: Rect,
    field_name: &str,
    editor: &PatternListEditor,
    accent: ratatui::style::Color,
) {
    let info_style = Style::default().fg(colors::INFO);
    let is_selected = matches!(editor.selection, PatternListSelection::Rect);
    let is_editing = editor.editing();
    let is_focused = is_selected || is_editing;
    let border_color = match editor.status {
        PatternListRectStatus::Unchanged => colors::WHITE,
        PatternListRectStatus::Editing => colors::WARNING,
        PatternListRectStatus::Modified => colors::ACCENT,
        PatternListRectStatus::Saved => accent,
    };
    let show_selection_marker = is_selected && !is_editing;
    let content_style = if is_focused {
        Style::default()
            .fg(colors::WHITE)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(colors::WHITE)
    };
    let border_style = Style::default().fg(border_color);

    let visible_range = editor.visible_range(area.height);
    let body: Vec<Line<'static>> = if editor.lines.is_empty() && !is_editing {
        vec![Line::from(Span::styled(
            "(none — Enter to edit)",
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        ))]
    } else {
        editor
            .lines
            .iter()
            .enumerate()
            .skip(visible_range.start)
            .take(visible_range.end.saturating_sub(visible_range.start))
            .map(|(idx, line)| {
                let on_cursor_row = editor
                    .editing_cursor()
                    .map(|c| c.row == idx)
                    .unwrap_or(false);
                if on_cursor_row {
                    pattern_list_cursor_line(line, editor.editing_cursor().unwrap().col)
                } else if line.is_empty() && is_editing {
                    Line::from(Span::styled(
                        " ",
                        Style::default()
                            .fg(colors::MUTED)
                            .add_modifier(Modifier::DIM),
                    ))
                } else {
                    Line::from(Span::styled(line.clone(), content_style))
                }
            })
            .collect()
    };

    let title_line = if show_selection_marker {
        Line::from(vec![
            Span::styled(
                POST_CMD_SELECTION_MARKER,
                Style::default().fg(colors::ACCENT),
            ),
            Span::styled(format!("{field_name} "), info_style),
        ])
    } else {
        Line::from(Span::styled(format!(" {field_name} "), info_style))
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(border_style)
        .padding(Padding::horizontal(1))
        .title(title_line);
    frame.render_widget(
        Paragraph::new(body).scroll((editor.scroll, 0)).block(block),
        area,
    );
}

fn pattern_list_cursor_line(text: &str, col: usize) -> Line<'static> {
    let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
    let chars: Vec<char> = text.chars().collect();
    let col = col.min(chars.len());
    let before: String = chars[..col].iter().collect();
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(3);
    if !before.is_empty() {
        spans.push(Span::styled(
            before,
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if col < chars.len() {
        let at: String = chars[col..col + 1].iter().collect();
        spans.push(Span::styled(at, cursor_style));
        let after: String = chars[col + 1..].iter().collect();
        if !after.is_empty() {
            spans.push(Span::styled(
                after,
                Style::default()
                    .fg(colors::WHITE)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    } else {
        spans.push(Span::styled(" ", cursor_style));
    }
    Line::from(spans)
}

fn parse_link_strategy(value: &str) -> Option<LinkStrategy> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "createempty" | "create_empty" | "create-empty" => Some(LinkStrategy::CreateEmpty),
        "seedfromsource" | "seed_from_source" | "seed-from-source" => {
            Some(LinkStrategy::SeedFromSource)
        }
        "seedifpresent" | "seed_if_present" | "seed-if-present" => {
            Some(LinkStrategy::SeedIfPresent)
        }
        _ => None,
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn build_post_cmd_input(value: &str) -> InputPrompt {
    InputPrompt::new("")
        .with_placeholder("Type command")
        .with_default(value.to_string())
}

fn build_terminal_cmd_input(value: &str) -> InputPrompt {
    InputPrompt::new("")
        .with_placeholder("Type command (e.g. code $WORKTREE_PATH)")
        .with_default(value.to_string())
}

fn build_link_cache_dir_input(value: &str) -> InputPrompt {
    InputPrompt::new("")
        .with_placeholder("Leave blank for ~/.wisetree/cache/<repo-id>")
        .with_default(value.to_string())
}

fn build_path_template_input(value: &str) -> InputPrompt {
    InputPrompt::new("")
        .with_placeholder("Type template (e.g. $BASE_PATH.worktree)")
        .with_default(value.to_string())
}

/// One-line summary of which notification bells are enabled, shown as the
/// description under the "Notifications" settings-menu entry.
fn notifications_menu_description(config: &NotificationsConfig) -> String {
    match (config.ai_status_ok, config.pr_checks_ok) {
        (false, false) => "all disabled".to_string(),
        (true, false) => "AI finished".to_string(),
        (false, true) => "PR checks".to_string(),
        (true, true) => "AI finished, PR checks".to_string(),
    }
}

/// Position of the `Ai` field inside `DashboardField::ALL`. Re-derived
/// from the static array so reordering the enum doesn't silently break the
/// chip-row navigation.
fn ai_field_index() -> Option<usize> {
    DashboardField::ALL
        .iter()
        .position(|f| matches!(f, DashboardField::Ai))
}

/// Number of cycle stops for `ladder`: the levels plus the leading "Default".
fn reasoning_level_count(ladder: &[String]) -> usize {
    1 + ladder.len()
}

/// The variant stored at cycle position `index` (0 = "Default" / empty string).
fn reasoning_level_at(ladder: &[String], index: usize) -> Option<String> {
    if index == 0 {
        Some(String::new())
    } else {
        ladder.get(index - 1).cloned()
    }
}

/// The cycle position of `variant` within `ladder`. An empty string (or a
/// stale variant no longer in this model's ladder) maps to 0 ("Default").
fn reasoning_level_index(ladder: &[String], variant: &str) -> usize {
    if variant.is_empty() {
        0
    } else {
        ladder
            .iter()
            .position(|v| v == variant)
            .map(|i| i + 1)
            .unwrap_or(0)
    }
}

/// Human label for the staged variant. Shows "default" for the empty value or
/// any variant that isn't valid for the current model's `ladder`.
fn reasoning_level_label<'a>(ladder: &[String], variant: &'a str) -> &'a str {
    if variant.is_empty() || !ladder.iter().any(|v| v == variant) {
        "default"
    } else {
        variant
    }
}

/// When the cursor enters the chip row, prefer the chip whose value matches
/// the `ai` rectangle's current contents. Falls back to chip 0 when no
/// chip matches so the cursor always has somewhere visible to land.
fn contains_position(area: Rect, position: Position) -> bool {
    position.x >= area.left()
        && position.x < area.right()
        && position.y >= area.top()
        && position.y < area.bottom()
}

fn build_dashboard_input(field: DashboardField, value: &str) -> InputPrompt {
    let placeholder = match field {
        DashboardField::RefreshIntervalMs => "Refresh interval in ms (5000..60000)",
        DashboardField::ShowPullRequests => "true or false",
        DashboardField::WiseMerge => "true or false",
        DashboardField::Columns => {
            "branch, status, ai_status, ahead_behind, diff, last_commit, pull_request"
        }
        DashboardField::Ai => "provider/model (e.g. anthropic/claude-sonnet-4-5)",
    };
    InputPrompt::new("")
        .with_placeholder(placeholder)
        .with_default(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{AiStatusConfig, WorktreeConfig};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    fn focus_ai(screen: &mut SettingsScreen) {
        let idx = ai_field_index().unwrap();
        let editor = screen.dashboard_editor.as_mut().unwrap();
        editor.selection = DashboardSelection::Rect(idx);
    }

    /// Open the Dashboard editor, drill into the AI Settings sub-screen via the
    /// `ai` rectangle, and cache the supplied free-model list.
    fn ai_settings_screen(free: Vec<String>) -> SettingsScreen {
        let mut screen = SettingsScreen::new(WorktreeConfig::default(), "test.json".to_string());
        screen.step = SettingsStep::Dashboard;
        screen.dashboard_editor = Some(DashboardEditor::new(&screen.config.dashboard));
        focus_ai(&mut screen);
        let _ = screen.handle_dashboard(key(KeyCode::Enter));
        if !free.is_empty() {
            screen.set_free_models(free);
        }
        screen
    }

    fn focus_ai_slot(screen: &mut SettingsScreen, idx: usize) {
        let editor = screen.ai_settings_editor.as_mut().unwrap();
        editor.selection = AiSettingsSelection::Rect(idx);
        editor.last_rect = idx;
    }

    /// Set a slot's model and reset its thinking to Default. Picking a model in
    /// the real flow restamps both, and tests want a clean baseline now that
    /// slots are seeded with non-blank per-command defaults.
    fn set_slot_model(screen: &mut SettingsScreen, idx: usize, model: &str) {
        let editor = screen.ai_settings_editor.as_mut().unwrap();
        let leaf = AiSettingsEditor::slot(idx).get_mut(&mut editor.ai);
        leaf.model = model.to_string();
        leaf.thinking = String::new();
    }

    fn dashboard_field_index(field: DashboardField) -> usize {
        DashboardField::ALL
            .iter()
            .position(|candidate| *candidate == field)
            .expect("dashboard field exists")
    }

    #[test]
    fn entering_ai_settings_emits_fetch_free_models() {
        let mut screen = SettingsScreen::new(WorktreeConfig::default(), "test.json".to_string());
        screen.step = SettingsStep::Dashboard;
        screen.dashboard_editor = Some(DashboardEditor::new(&screen.config.dashboard));
        focus_ai(&mut screen);
        let action = screen.handle_dashboard(key(KeyCode::Enter));
        assert_eq!(action, SettingsAction::FetchFreeModels);
        assert_eq!(screen.step, SettingsStep::AiSettings);
        assert!(screen.ai_settings_editor.is_some());
        // Each entry resets the cache so we always re-fetch.
        assert!(screen.free_models.is_none());
    }

    #[test]
    fn down_from_last_slot_lands_on_chip_row_when_chips_present() {
        let mut screen = ai_settings_screen(vec![
            "opencode/big-pickle".to_string(),
            "opencode/deepseek-v4-flash-free".to_string(),
        ]);
        focus_ai_slot(&mut screen, AiSlot::ALL.len() - 1);
        let _ = screen.handle_ai_settings(key(KeyCode::Down));
        let editor = screen.ai_settings_editor.as_ref().unwrap();
        assert!(matches!(
            editor.selection,
            AiSettingsSelection::FreeModels(_)
        ));
    }

    #[test]
    fn down_from_last_slot_skips_chips_when_list_unavailable() {
        let mut screen = ai_settings_screen(vec![]);
        focus_ai_slot(&mut screen, AiSlot::ALL.len() - 1);
        let _ = screen.handle_ai_settings(key(KeyCode::Down));
        let editor = screen.ai_settings_editor.as_ref().unwrap();
        assert_eq!(editor.selection, AiSettingsSelection::Save);
    }

    #[test]
    fn arrows_cycle_chip_selection() {
        let mut screen = ai_settings_screen(vec![
            "a/x".to_string(),
            "b/y".to_string(),
            "c/z".to_string(),
        ]);
        screen.ai_settings_editor.as_mut().unwrap().selection = AiSettingsSelection::FreeModels(0);
        let _ = screen.handle_ai_settings(key(KeyCode::Right));
        assert_eq!(
            screen.ai_settings_editor.as_ref().unwrap().selection,
            AiSettingsSelection::FreeModels(1)
        );
        let _ = screen.handle_ai_settings(key(KeyCode::Right));
        let _ = screen.handle_ai_settings(key(KeyCode::Right));
        // Wraps forward.
        assert_eq!(
            screen.ai_settings_editor.as_ref().unwrap().selection,
            AiSettingsSelection::FreeModels(0)
        );
        // Wraps backward.
        let _ = screen.handle_ai_settings(key(KeyCode::Left));
        assert_eq!(
            screen.ai_settings_editor.as_ref().unwrap().selection,
            AiSettingsSelection::FreeModels(2)
        );
    }

    #[test]
    fn enter_on_chip_stages_value_into_focused_slot_without_saving() {
        let mut screen = ai_settings_screen(vec![
            "opencode/big-pickle".to_string(),
            "opencode/deepseek-v4-flash-free".to_string(),
        ]);
        // Focus the fix_plan slot, give it a thinking level, then drop onto the
        // chip row and stage a free model — which must clear the thinking.
        focus_ai_slot(&mut screen, 1);
        AiSettingsEditor::slot(1)
            .get_mut(&mut screen.ai_settings_editor.as_mut().unwrap().ai)
            .thinking = "high".to_string();
        screen.ai_settings_editor.as_mut().unwrap().selection = AiSettingsSelection::FreeModels(1);
        let action = screen.handle_ai_settings(key(KeyCode::Enter));
        assert_eq!(action, SettingsAction::Continue);
        let editor = screen.ai_settings_editor.as_ref().unwrap();
        let slot = AiSlot::ALL[1].get(&editor.ai);
        assert_eq!(slot.model, "opencode/deepseek-v4-flash-free");
        assert_eq!(slot.thinking, "");
        assert_eq!(editor.statuses[1], DashboardRectStatus::Modified);
        // Cursor stays on the chip so the user can keep cycling.
        assert_eq!(editor.selection, AiSettingsSelection::FreeModels(1));
    }

    #[test]
    fn arrows_on_slot_adjust_reasoning_strength() {
        let mut screen = ai_settings_screen(vec![]);
        focus_ai_slot(&mut screen, 0);
        set_slot_model(&mut screen, 0, "opencode/big-pickle");

        let _ = screen.handle_ai_settings(key(KeyCode::Right));
        let editor = screen.ai_settings_editor.as_ref().unwrap();
        assert_eq!(AiSlot::ALL[0].get(&editor.ai).thinking, "minimal");
        assert_eq!(editor.statuses[0], DashboardRectStatus::Modified);

        let _ = screen.handle_ai_settings(key(KeyCode::Right));
        assert_eq!(
            AiSlot::ALL[0]
                .get(&screen.ai_settings_editor.as_ref().unwrap().ai)
                .thinking,
            "low"
        );

        let _ = screen.handle_ai_settings(key(KeyCode::Left));
        let _ = screen.handle_ai_settings(key(KeyCode::Left));
        assert_eq!(
            AiSlot::ALL[0]
                .get(&screen.ai_settings_editor.as_ref().unwrap().ai)
                .thinking,
            ""
        );
    }

    #[test]
    fn arrows_use_authoritative_per_model_variants() {
        // GLM-5.2 only accepts high/max. The first Right jumps straight to
        // "high", and stepping past "max" clamps there.
        let mut screen = ai_settings_screen(vec![]);
        focus_ai_slot(&mut screen, 1);
        set_slot_model(&mut screen, 1, "opencode-go/glm-5.2");
        screen.set_ai_model_variants(std::collections::HashMap::from([(
            "opencode-go/glm-5.2".to_string(),
            vec!["high".to_string(), "max".to_string()],
        )]));

        let slot_thinking = |s: &SettingsScreen| {
            AiSlot::ALL[1]
                .get(&s.ai_settings_editor.as_ref().unwrap().ai)
                .thinking
                .clone()
        };
        let _ = screen.handle_ai_settings(key(KeyCode::Right));
        assert_eq!(slot_thinking(&screen), "high");
        let _ = screen.handle_ai_settings(key(KeyCode::Right));
        assert_eq!(slot_thinking(&screen), "max");
        let _ = screen.handle_ai_settings(key(KeyCode::Right));
        assert_eq!(slot_thinking(&screen), "max");
    }

    #[test]
    fn arrows_are_inert_for_models_with_no_reasoning_variants() {
        // Kimi is reasoning-capable on models.dev but opencode exposes no
        // variants, so the cycle stays on "default".
        let mut screen = ai_settings_screen(vec![]);
        focus_ai_slot(&mut screen, 2);
        set_slot_model(&mut screen, 2, "opencode-go/kimi-k2.7-code");
        screen.set_ai_model_variants(std::collections::HashMap::from([(
            "opencode-go/kimi-k2.7-code".to_string(),
            Vec::new(),
        )]));

        let _ = screen.handle_ai_settings(key(KeyCode::Right));
        assert_eq!(
            AiSlot::ALL[2]
                .get(&screen.ai_settings_editor.as_ref().unwrap().ai)
                .thinking,
            ""
        );
    }

    #[test]
    fn arrows_on_empty_slot_do_not_stage_orphaned_strength() {
        let mut screen = ai_settings_screen(vec![]);
        focus_ai_slot(&mut screen, 0);
        set_slot_model(&mut screen, 0, ""); // clear the seeded default first
        let _ = screen.handle_ai_settings(key(KeyCode::Right));
        let editor = screen.ai_settings_editor.as_ref().unwrap();
        assert_eq!(AiSlot::ALL[0].get(&editor.ai).thinking, "");
        assert_eq!(editor.statuses[0], DashboardRectStatus::Saved);
    }

    #[test]
    fn save_ai_settings_persists_per_command_config() {
        let mut screen = ai_settings_screen(vec![]);
        focus_ai_slot(&mut screen, 1); // fix_plan
        set_slot_model(&mut screen, 1, "openai/gpt-5.5");
        let _ = screen.handle_ai_settings(key(KeyCode::Right)); // minimal thinking
        screen.ai_settings_editor.as_mut().unwrap().selection = AiSettingsSelection::Save;

        match screen.handle_ai_settings(key(KeyCode::Enter)) {
            SettingsAction::SaveDashboard(cfg) => {
                assert_eq!(cfg.ai.fix.plan.model, "openai/gpt-5.5");
                assert_eq!(cfg.ai.fix.plan.thinking, "minimal");
                // Untouched slots keep their per-command default.
                assert_eq!(cfg.ai.enrich.model, "opencode-go/deepseek-v4-flash");
            }
            other => panic!("expected SaveDashboard, got {other:?}"),
        }
    }

    #[test]
    fn apply_ai_selection_targets_focused_slot() {
        let mut screen = ai_settings_screen(vec![]);
        focus_ai_slot(&mut screen, 3); // update
        screen.apply_ai_selection(
            "anthropic/claude-sonnet-4-5".to_string(),
            "high".to_string(),
        );
        let editor = screen.ai_settings_editor.as_ref().unwrap();
        let slot = AiSlot::ALL[3].get(&editor.ai);
        assert_eq!(slot.model, "anthropic/claude-sonnet-4-5");
        assert_eq!(slot.thinking, "high");
        assert_eq!(editor.statuses[3], DashboardRectStatus::Modified);
        assert_eq!(editor.selection, AiSettingsSelection::Rect(3));
    }

    #[test]
    fn esc_stages_ai_back_into_dashboard_and_returns() {
        let mut screen = ai_settings_screen(vec![]);
        focus_ai_slot(&mut screen, 0);
        set_slot_model(&mut screen, 0, "opencode/big-pickle");
        let _ = screen.handle_ai_settings(key(KeyCode::Esc));
        assert_eq!(screen.step, SettingsStep::Dashboard);
        assert!(screen.ai_settings_editor.is_none());
        let dash = screen.dashboard_editor.as_ref().unwrap();
        assert_eq!(dash.ai.enrich.model, "opencode/big-pickle");
        let idx = ai_field_index().unwrap();
        assert_eq!(dash.statuses[idx], DashboardRectStatus::Modified);
    }

    #[test]
    fn save_dashboard_preserves_ai_status() {
        let config = WorktreeConfig {
            dashboard: DashboardConfig {
                ai_status: AiStatusConfig {
                    enabled_harnesses: vec!["opencode".into()],
                    active_window_ms: 7_500,
                },
                ..DashboardConfig::default()
            },
            ..WorktreeConfig::default()
        };
        let mut screen = SettingsScreen::new(config, "test.json".to_string());
        screen.step = SettingsStep::Dashboard;
        screen.dashboard_editor = Some(DashboardEditor::new(&screen.config.dashboard));

        let pr_idx = dashboard_field_index(DashboardField::ShowPullRequests);
        screen.dashboard_editor.as_mut().unwrap().selection = DashboardSelection::Rect(pr_idx);
        let _ = screen.handle_dashboard(key(KeyCode::Enter));
        screen.dashboard_editor.as_mut().unwrap().selection = DashboardSelection::Save;

        match screen.handle_dashboard(key(KeyCode::Enter)) {
            SettingsAction::SaveDashboard(cfg) => {
                // Editing an unrelated dashboard field must not clobber the
                // nested ai_status block carried over from the base config.
                assert_eq!(cfg.ai_status.enabled_harnesses, vec!["opencode"]);
                assert_eq!(cfg.ai_status.active_window_ms, 7_500);
            }
            other => panic!("expected SaveDashboard, got {other:?}"),
        }
    }

    #[test]
    fn notifications_field_metadata() {
        assert_eq!(NotificationsField::AiStatusOk.label(), "aiStatusOk");
        assert_eq!(NotificationsField::PrChecksOk.label(), "prChecksOk");
        assert_eq!(NotificationsField::ALL.len(), 2);
    }

    #[test]
    fn enter_toggles_notifications_before_save() {
        let mut screen = SettingsScreen::new(WorktreeConfig::default(), "test.json".to_string());
        screen.step = SettingsStep::Notifications;
        screen.notifications_editor = Some(NotificationsEditor::new(&screen.config.notifications));

        screen.notifications_editor.as_mut().unwrap().selection = NotificationsSelection::Rect(0);
        assert_eq!(
            screen.handle_notifications(key(KeyCode::Enter)),
            SettingsAction::Continue
        );
        screen.notifications_editor.as_mut().unwrap().selection = NotificationsSelection::Rect(1);
        assert_eq!(
            screen.handle_notifications(key(KeyCode::Enter)),
            SettingsAction::Continue
        );

        let editor = screen.notifications_editor.as_ref().unwrap();
        assert!(editor.values[0]);
        assert!(editor.values[1]);
        assert_eq!(editor.statuses[0], DashboardRectStatus::Modified);
        assert_eq!(editor.statuses[1], DashboardRectStatus::Modified);
    }

    #[test]
    fn enter_on_save_emits_save_notifications() {
        let mut screen = SettingsScreen::new(WorktreeConfig::default(), "test.json".to_string());
        screen.step = SettingsStep::Notifications;
        screen.notifications_editor = Some(NotificationsEditor::new(&screen.config.notifications));

        screen.notifications_editor.as_mut().unwrap().selection = NotificationsSelection::Rect(0);
        let _ = screen.handle_notifications(key(KeyCode::Enter));
        screen.notifications_editor.as_mut().unwrap().selection = NotificationsSelection::Save;

        match screen.handle_notifications(key(KeyCode::Enter)) {
            SettingsAction::SaveNotifications(cfg) => {
                assert!(cfg.ai_status_ok);
                assert!(!cfg.pr_checks_ok);
            }
            other => panic!("expected SaveNotifications, got {other:?}"),
        }
    }

    #[test]
    fn esc_discards_unsaved_notifications() {
        let mut screen = SettingsScreen::new(WorktreeConfig::default(), "test.json".to_string());
        screen.step = SettingsStep::Notifications;
        screen.notifications_editor = Some(NotificationsEditor::new(&screen.config.notifications));

        screen.notifications_editor.as_mut().unwrap().selection = NotificationsSelection::Rect(0);
        let _ = screen.handle_notifications(key(KeyCode::Enter));
        let action = screen.handle_notifications(key(KeyCode::Esc));

        assert_eq!(action, SettingsAction::Continue);
        assert_eq!(screen.step, SettingsStep::Menu);
        // The toggle was never saved, so the backing config is untouched.
        assert!(!screen.config.notifications.ai_status_ok);
        assert!(screen.notifications_editor.is_none());
    }

    #[test]
    fn notifications_menu_entry_initializes_editor() {
        let mut screen = SettingsScreen::new(WorktreeConfig::default(), "test.json".to_string());
        let select = screen.select.as_mut().expect("menu built in new()");
        let idx = select
            .options
            .iter()
            .position(|opt| opt.value == SettingsStep::Notifications)
            .expect("notifications option exists");
        select.selected = idx;
        let action = screen.handle_key(key(KeyCode::Enter));
        assert_eq!(action, SettingsAction::Continue);
        assert_eq!(screen.step, SettingsStep::Notifications);
        assert!(screen.notifications_editor.is_some());
    }

    #[test]
    fn set_free_models_error_pulls_cursor_off_invisible_chip_row() {
        let mut screen = ai_settings_screen(vec!["opencode/big-pickle".to_string()]);
        let editor = screen.ai_settings_editor.as_mut().unwrap();
        editor.last_rect = 2;
        editor.selection = AiSettingsSelection::FreeModels(0);
        screen.set_free_models_error("opencode CLI missing".to_string());
        assert!(matches!(screen.free_models(), Some(Err(_))));
        assert_eq!(
            screen.ai_settings_editor.as_ref().unwrap().selection,
            AiSettingsSelection::Rect(2)
        );
    }
}
