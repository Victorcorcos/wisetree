//! Setup Project Config screen.
//!
//! Three-step flow that bootstraps a new project's `.wisetree.json` from a
//! preset:
//!
//! 1. `PresetList`  — `SelectPrompt` over `Wise Preset` plus the static preset
//!    catalog. A root-level match is still tagged `detected`; otherwise Wise is
//!    the default choice.
//! 2. `Discovering` — spinner while `App` performs the deep Wise scan.
//! 3. `Confirm`     — four editable rectangle blocks (Copy Patterns / Ignore
//!    Patterns / Shared Cache Links / Post-Create Commands) seeded from the
//!    chosen preset, plus a Yes/No row with Yes pre-selected. Each line inside
//!    a block is one entry in the corresponding `.wisetree.json` list;
//!    pressing Enter inside a block adds a new entry.
//!
//! `Esc` walks back one step (Confirm → PresetList; PresetList → menu).
//! `App` owns Wise discovery + persistence and consumes
//! [`SetupProjectAction::Apply`] to write the chosen values to disk.

use std::cell::Cell;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::messages::colors;
use crate::services::presets::{catalog, detect, Preset, PresetId, WisePresetDiscovery};
use crate::tui::widgets::{
    branded_line, ConfirmChoice, SelectOption, SelectOutcome, SelectPrompt, Status, StatusIndicator,
};

const WISE_PRESET_LIST_LABEL: &str = "Wise Preset";
const WISE_PRESET_CONFIRM_LABEL: &str = "Wise Preset";
const CONFIRM_BLOCK_COUNT: usize = 4;
/// Selection marker shown on the focused-but-not-editing block. Mirrors the
/// `POST_CMD_SELECTION_MARKER` used by the Post-Create Commands editor.
const SELECTION_MARKER: &str = " ✎﹏ ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupProjectStep {
    PresetList,
    Discovering,
    Confirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetChoice {
    Wise,
    Catalog(PresetId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupProjectPresetValues {
    pub label: String,
    pub copy_patterns: Vec<String>,
    pub copy_ignores: Vec<String>,
    pub link_patterns: Vec<String>,
    pub post_create_cmd: Vec<String>,
}

impl SetupProjectPresetValues {
    fn from_preset(preset: &Preset) -> Self {
        Self {
            label: preset.label.to_string(),
            copy_patterns: preset.copy_patterns_owned(),
            copy_ignores: preset.copy_ignores_owned(),
            link_patterns: preset.link_patterns_owned(),
            post_create_cmd: preset.post_create_cmd_owned(),
        }
    }

    fn wise(discovery: WisePresetDiscovery) -> Self {
        Self {
            label: WISE_PRESET_CONFIRM_LABEL.to_string(),
            copy_patterns: discovery.copy_patterns,
            copy_ignores: discovery.copy_ignores,
            link_patterns: discovery.link_patterns,
            post_create_cmd: discovery.post_create_cmd,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupProjectAction {
    /// No-op; the screen handled the key internally.
    Continue,
    /// User hit Esc on the preset list. Caller should `back_to_menu()`.
    Cancelled,
    /// User selected Wise Preset; caller should start async discovery.
    DiscoverWise,
    /// User confirmed Yes; caller should persist the preset to disk.
    Apply(SetupProjectPresetValues),
}

/// Which item on the Confirm step is currently focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmSelection {
    /// One of the four editable rectangles (0=Copy Patterns,
    /// 1=Copy Ignores, 2=Shared Cache Links, 3=Post-Create Cmd).
    Block(usize),
    Yes,
    No,
}

/// Cursor position inside a block while editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EditCursor {
    row: usize,
    col: usize,
}

/// Editable state backing the Confirm step. Holds four independent
/// `Vec<String>` buffers (one per `.wisetree.json` list field) plus the
/// current selection and an optional edit cursor.
pub struct ConfirmEditor {
    label: String,
    blocks: [Vec<String>; CONFIRM_BLOCK_COUNT],
    selection: ConfirmSelection,
    editing: Option<EditCursor>,
    scrolls: [u16; CONFIRM_BLOCK_COUNT],
}

impl ConfirmEditor {
    fn from_values(values: SetupProjectPresetValues) -> Self {
        Self {
            label: values.label,
            blocks: [
                values.copy_patterns,
                values.copy_ignores,
                values.link_patterns,
                values.post_create_cmd,
            ],
            selection: ConfirmSelection::Yes,
            editing: None,
            scrolls: [0; CONFIRM_BLOCK_COUNT],
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn selection(&self) -> ConfirmSelection {
        self.selection
    }

    pub fn editing_block(&self) -> Option<usize> {
        self.editing.and(match self.selection {
            ConfirmSelection::Block(i) => Some(i),
            _ => None,
        })
    }

    fn block_len(&self, idx: usize) -> usize {
        self.blocks[idx].len()
    }

    fn to_values(&self) -> SetupProjectPresetValues {
        let normalize = |block: &Vec<String>| -> Vec<String> {
            block
                .iter()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect()
        };
        SetupProjectPresetValues {
            label: self.label.clone(),
            copy_patterns: normalize(&self.blocks[0]),
            copy_ignores: normalize(&self.blocks[1]),
            link_patterns: normalize(&self.blocks[2]),
            post_create_cmd: normalize(&self.blocks[3]),
        }
    }

    fn move_up(&mut self) {
        self.selection = match self.selection {
            ConfirmSelection::Block(0) => ConfirmSelection::Block(0),
            ConfirmSelection::Block(i) => ConfirmSelection::Block(i - 1),
            ConfirmSelection::Yes | ConfirmSelection::No => {
                ConfirmSelection::Block(CONFIRM_BLOCK_COUNT - 1)
            }
        };
    }

    fn move_down(&mut self) {
        self.selection = match self.selection {
            ConfirmSelection::Block(i) if i + 1 < CONFIRM_BLOCK_COUNT => {
                ConfirmSelection::Block(i + 1)
            }
            ConfirmSelection::Block(_) => ConfirmSelection::Yes,
            ConfirmSelection::Yes | ConfirmSelection::No => self.selection,
        };
    }

    fn toggle_yes_no(&mut self) {
        self.selection = match self.selection {
            ConfirmSelection::Yes => ConfirmSelection::No,
            ConfirmSelection::No => ConfirmSelection::Yes,
            other => other,
        };
    }

    fn start_editing(&mut self) {
        let ConfirmSelection::Block(i) = self.selection else {
            return;
        };
        if self.blocks[i].is_empty() {
            self.blocks[i].push(String::new());
        }
        let row = self.blocks[i].len() - 1;
        let col = self.blocks[i][row].chars().count();
        self.editing = Some(EditCursor { row, col });
    }

    fn stop_editing(&mut self) {
        self.editing = None;
    }

    fn editing_cursor(&self) -> Option<(usize, EditCursor)> {
        self.editing.and_then(|cursor| match self.selection {
            ConfirmSelection::Block(i) => Some((i, cursor)),
            _ => None,
        })
    }

    fn byte_offset(s: &str, char_idx: usize) -> usize {
        s.char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(s.len())
    }

    fn with_cursor<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut [Vec<String>; CONFIRM_BLOCK_COUNT], usize, &mut EditCursor),
    {
        let Some((block, mut cursor)) = self.editing_cursor() else {
            return;
        };
        f(&mut self.blocks, block, &mut cursor);
        // Re-clamp cursor in case `f` left it past the end of the line.
        let max_col = self.blocks[block]
            .get(cursor.row)
            .map(|line| line.chars().count())
            .unwrap_or(0);
        cursor.col = cursor.col.min(max_col);
        self.editing = Some(cursor);
    }

    fn insert_char(&mut self, c: char) {
        self.with_cursor(|blocks, block, cursor| {
            let line = &mut blocks[block][cursor.row];
            let byte = Self::byte_offset(line, cursor.col);
            line.insert(byte, c);
            cursor.col += 1;
        });
    }

    fn insert_newline(&mut self) {
        self.with_cursor(|blocks, block, cursor| {
            let suffix = {
                let line = &mut blocks[block][cursor.row];
                let byte = Self::byte_offset(line, cursor.col);
                line.split_off(byte)
            };
            blocks[block].insert(cursor.row + 1, suffix);
            cursor.row += 1;
            cursor.col = 0;
        });
    }

    fn delete_left(&mut self) {
        self.with_cursor(|blocks, block, cursor| {
            if cursor.col > 0 {
                let line = &mut blocks[block][cursor.row];
                let end = Self::byte_offset(line, cursor.col);
                let start = Self::byte_offset(line, cursor.col - 1);
                line.drain(start..end);
                cursor.col -= 1;
            } else if cursor.row > 0 {
                let removed = blocks[block].remove(cursor.row);
                let prev_len = blocks[block][cursor.row - 1].chars().count();
                blocks[block][cursor.row - 1].push_str(&removed);
                cursor.row -= 1;
                cursor.col = prev_len;
            }
        });
    }

    fn delete_right(&mut self) {
        self.with_cursor(|blocks, block, cursor| {
            let line_len = blocks[block][cursor.row].chars().count();
            if cursor.col < line_len {
                let line = &mut blocks[block][cursor.row];
                let start = Self::byte_offset(line, cursor.col);
                let end = Self::byte_offset(line, cursor.col + 1);
                line.drain(start..end);
            } else if cursor.row + 1 < blocks[block].len() {
                let next = blocks[block].remove(cursor.row + 1);
                blocks[block][cursor.row].push_str(&next);
            }
        });
    }

    fn move_cursor_left(&mut self) {
        self.with_cursor(|blocks, block, cursor| {
            if cursor.col > 0 {
                cursor.col -= 1;
            } else if cursor.row > 0 {
                cursor.row -= 1;
                cursor.col = blocks[block][cursor.row].chars().count();
            }
        });
    }

    fn move_cursor_right(&mut self) {
        self.with_cursor(|blocks, block, cursor| {
            let line_len = blocks[block][cursor.row].chars().count();
            if cursor.col < line_len {
                cursor.col += 1;
            } else if cursor.row + 1 < blocks[block].len() {
                cursor.row += 1;
                cursor.col = 0;
            }
        });
    }

    fn move_cursor_up(&mut self) {
        self.with_cursor(|_, _, cursor| {
            if cursor.row > 0 {
                cursor.row -= 1;
            }
        });
    }

    fn move_cursor_down(&mut self) {
        self.with_cursor(|blocks, block, cursor| {
            if cursor.row + 1 < blocks[block].len() {
                cursor.row += 1;
            }
        });
    }

    fn move_cursor_home(&mut self) {
        self.with_cursor(|_, _, cursor| {
            cursor.col = 0;
        });
    }

    fn move_cursor_end(&mut self) {
        self.with_cursor(|blocks, block, cursor| {
            cursor.col = blocks[block][cursor.row].chars().count();
        });
    }

    fn move_word_left(&mut self) {
        self.with_cursor(|blocks, block, cursor| {
            if cursor.col == 0 {
                if cursor.row > 0 {
                    cursor.row -= 1;
                    cursor.col = blocks[block][cursor.row].chars().count();
                }
                return;
            }
            let chars: Vec<char> = blocks[block][cursor.row].chars().collect();
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
        self.with_cursor(|blocks, block, cursor| {
            let chars: Vec<char> = blocks[block][cursor.row].chars().collect();
            let len = chars.len();
            if cursor.col == len {
                if cursor.row + 1 < blocks[block].len() {
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
        self.with_cursor(|blocks, block, cursor| {
            if cursor.col == 0 {
                if cursor.row > 0 {
                    let removed = blocks[block].remove(cursor.row);
                    let prev_len = blocks[block][cursor.row - 1].chars().count();
                    blocks[block][cursor.row - 1].push_str(&removed);
                    cursor.row -= 1;
                    cursor.col = prev_len;
                }
                return;
            }
            let chars: Vec<char> = blocks[block][cursor.row].chars().collect();
            let mut i = cursor.col;
            while i > 0 && !is_word_char(chars[i - 1]) {
                i -= 1;
            }
            while i > 0 && is_word_char(chars[i - 1]) {
                i -= 1;
            }
            let line = &mut blocks[block][cursor.row];
            let start = Self::byte_offset(line, i);
            let end = Self::byte_offset(line, cursor.col);
            line.drain(start..end);
            cursor.col = i;
        });
    }

    fn delete_word_right(&mut self) {
        self.with_cursor(|blocks, block, cursor| {
            let chars: Vec<char> = blocks[block][cursor.row].chars().collect();
            let len = chars.len();
            if cursor.col == len {
                if cursor.row + 1 < blocks[block].len() {
                    let next = blocks[block].remove(cursor.row + 1);
                    blocks[block][cursor.row].push_str(&next);
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
            let line = &mut blocks[block][cursor.row];
            let start = Self::byte_offset(line, cursor.col);
            let end = Self::byte_offset(line, i);
            line.drain(start..end);
        });
    }

    fn kill_to_start(&mut self) {
        self.with_cursor(|blocks, block, cursor| {
            if cursor.col == 0 {
                return;
            }
            let line = &mut blocks[block][cursor.row];
            let end = Self::byte_offset(line, cursor.col);
            line.drain(..end);
            cursor.col = 0;
        });
    }

    fn kill_to_end(&mut self) {
        self.with_cursor(|blocks, block, cursor| {
            let line = &mut blocks[block][cursor.row];
            let start = Self::byte_offset(line, cursor.col);
            line.drain(start..);
        });
    }

    /// Clamp `scrolls[block]` so the cursor row is visible within `area_height`
    /// (which includes the block border — usable line count is height - 2).
    fn clamp_scroll_for_cursor(&mut self, block: usize, area_height: u16) {
        let Some((b, cursor)) = self.editing_cursor() else {
            return;
        };
        if b != block {
            return;
        }
        let visible = area_height.saturating_sub(2) as usize;
        if visible == 0 {
            return;
        }
        let current = self.scrolls[block] as usize;
        let row = cursor.row;
        let new_scroll = if row < current {
            row
        } else if row >= current + visible {
            row + 1 - visible
        } else {
            current
        };
        let total_lines = self.blocks[block].len();
        let max_scroll = total_lines.saturating_sub(visible);
        self.scrolls[block] = new_scroll.min(max_scroll) as u16;
    }

    fn scroll(&mut self, block: usize, delta: i16, area_height: u16) {
        let visible = area_height.saturating_sub(2) as usize;
        let total_lines = self.blocks[block].len().max(1);
        let max_scroll = total_lines.saturating_sub(visible) as u16;
        let current = self.scrolls[block];
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as u16).min(max_scroll)
        };
        self.scrolls[block] = next.min(max_scroll);
    }
}

pub struct SetupProjectScreen {
    step: SetupProjectStep,
    presets: Vec<Preset>,
    detected: Option<PresetId>,
    selected_choice: PresetChoice,
    select: SelectPrompt<PresetChoice>,
    confirm: Option<ConfirmEditor>,
    confirm_block_rects: Cell<[Rect; CONFIRM_BLOCK_COUNT]>,
    pub tick: usize,
}

impl SetupProjectScreen {
    pub fn new(project_root: Option<&Path>) -> Self {
        let presets = catalog();
        let detected = project_root.and_then(detect);
        let selected_choice = detected
            .map(PresetChoice::Catalog)
            .unwrap_or(PresetChoice::Wise);

        let mut wise_option =
            SelectOption::new(WISE_PRESET_LIST_LABEL, PresetChoice::Wise).with_color(colors::BRAND);
        wise_option = if detected.is_none() {
            wise_option
                .with_description("recommended")
                .with_description_color(colors::SUCCESS)
        } else {
            wise_option.with_description("deep scan nested apps")
        };

        let mut options: Vec<SelectOption<PresetChoice>> = vec![wise_option];
        options.extend(presets.iter().map(|preset| {
            let mut option = SelectOption::new(preset.label, PresetChoice::Catalog(preset.id));
            if Some(preset.id) == detected {
                option = option
                    .with_description("detected")
                    .with_description_color(colors::SUCCESS);
            } else {
                option = option.with_description(preset.description);
            }
            option
        }));

        let default_idx = options
            .iter()
            .position(|option| option.value == selected_choice)
            .unwrap_or(0);

        let select = SelectPrompt::new("Choose a preset", options)
            .with_default_index(default_idx)
            .searchable()
            .without_hint();

        Self {
            step: SetupProjectStep::PresetList,
            presets,
            detected,
            selected_choice,
            select,
            confirm: None,
            confirm_block_rects: Cell::new([Rect::default(); CONFIRM_BLOCK_COUNT]),
            tick: 0,
        }
    }

    pub fn step(&self) -> SetupProjectStep {
        self.step
    }

    pub fn detected(&self) -> Option<PresetId> {
        self.detected
    }

    pub fn selected_choice(&self) -> PresetChoice {
        self.selected_choice
    }

    pub fn selected_preset(&self) -> Option<PresetId> {
        match self.selected_choice {
            PresetChoice::Wise => None,
            PresetChoice::Catalog(id) => Some(id),
        }
    }

    pub fn confirm_editor(&self) -> Option<&ConfirmEditor> {
        self.confirm.as_ref()
    }

    /// Yes/No state of the Confirm step, collapsing the block-selection
    /// rectangles into `ConfirmChoice::Confirm`. Returns `Confirm` when no
    /// editor is active so callers that probe before discovery completes get
    /// the same default the screen uses.
    pub fn confirm_choice(&self) -> ConfirmChoice {
        self.confirm
            .as_ref()
            .map(|editor| confirm_choice_for(editor.selection))
            .unwrap_or(ConfirmChoice::Confirm)
    }

    pub fn complete_wise_discovery(&mut self, discovery: WisePresetDiscovery) {
        self.confirm = Some(ConfirmEditor::from_values(SetupProjectPresetValues::wise(
            discovery,
        )));
        self.step = SetupProjectStep::Confirm;
    }

    pub fn reset_after_wise_discovery_failure(&mut self) {
        self.confirm = None;
        self.step = SetupProjectStep::PresetList;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SetupProjectAction {
        match self.step {
            SetupProjectStep::PresetList => self.handle_preset_list(key),
            SetupProjectStep::Discovering => SetupProjectAction::Continue,
            SetupProjectStep::Confirm => self.handle_confirm(key),
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        if !matches!(self.step, SetupProjectStep::Confirm) {
            return false;
        }

        let block_idx = self.confirm_block_index_at(Position {
            x: mouse.column,
            y: mouse.row,
        });
        let Some(block_idx) = block_idx else {
            return false;
        };

        let area_height = self.confirm_block_rects.get()[block_idx].height;
        let Some(editor) = self.confirm.as_mut() else {
            return false;
        };
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                editor.scroll(block_idx, 1, area_height);
                true
            }
            MouseEventKind::ScrollUp => {
                editor.scroll(block_idx, -1, area_height);
                true
            }
            _ => false,
        }
    }

    fn preset(&self, id: PresetId) -> &Preset {
        self.presets
            .iter()
            .find(|preset| preset.id == id)
            .expect("preset id from catalog")
    }

    fn handle_preset_list(&mut self, key: KeyEvent) -> SetupProjectAction {
        match self.select.handle_key(key) {
            SelectOutcome::Selected(_, choice) => {
                self.selected_choice = choice;
                match choice {
                    PresetChoice::Wise => {
                        self.confirm = None;
                        self.step = SetupProjectStep::Discovering;
                        SetupProjectAction::DiscoverWise
                    }
                    PresetChoice::Catalog(id) => {
                        let values = SetupProjectPresetValues::from_preset(self.preset(id));
                        self.confirm = Some(ConfirmEditor::from_values(values));
                        self.step = SetupProjectStep::Confirm;
                        SetupProjectAction::Continue
                    }
                }
            }
            SelectOutcome::Cancelled => SetupProjectAction::Cancelled,
            SelectOutcome::Pending => SetupProjectAction::Continue,
        }
    }

    fn handle_confirm(&mut self, key: KeyEvent) -> SetupProjectAction {
        let editor = match self.confirm.as_mut() {
            Some(editor) => editor,
            None => {
                self.step = SetupProjectStep::PresetList;
                return SetupProjectAction::Continue;
            }
        };

        if editor.editing.is_some() {
            return Self::handle_confirm_editing(editor, &self.confirm_block_rects, key);
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Esc => {
                self.step = SetupProjectStep::PresetList;
                SetupProjectAction::Continue
            }
            KeyCode::Up | KeyCode::Char('k') if !ctrl => {
                editor.move_up();
                SetupProjectAction::Continue
            }
            KeyCode::Down | KeyCode::Char('j') if !ctrl => {
                editor.move_down();
                SetupProjectAction::Continue
            }
            KeyCode::Left | KeyCode::Right => {
                if matches!(
                    editor.selection,
                    ConfirmSelection::Yes | ConfirmSelection::No
                ) {
                    editor.toggle_yes_no();
                }
                SetupProjectAction::Continue
            }
            KeyCode::Tab => {
                editor.selection = match editor.selection {
                    ConfirmSelection::Block(i) if i + 1 < CONFIRM_BLOCK_COUNT => {
                        ConfirmSelection::Block(i + 1)
                    }
                    ConfirmSelection::Block(_) => ConfirmSelection::Yes,
                    ConfirmSelection::Yes => ConfirmSelection::No,
                    ConfirmSelection::No => ConfirmSelection::Block(0),
                };
                SetupProjectAction::Continue
            }
            KeyCode::BackTab => {
                editor.selection = match editor.selection {
                    ConfirmSelection::Block(0) => ConfirmSelection::No,
                    ConfirmSelection::Block(i) => ConfirmSelection::Block(i - 1),
                    ConfirmSelection::Yes => ConfirmSelection::Block(CONFIRM_BLOCK_COUNT - 1),
                    ConfirmSelection::No => ConfirmSelection::Yes,
                };
                SetupProjectAction::Continue
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                editor.selection = ConfirmSelection::Yes;
                SetupProjectAction::Continue
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                editor.selection = ConfirmSelection::No;
                SetupProjectAction::Continue
            }
            KeyCode::Enter => match editor.selection {
                ConfirmSelection::Block(_) => {
                    editor.start_editing();
                    SetupProjectAction::Continue
                }
                ConfirmSelection::Yes => SetupProjectAction::Apply(editor.to_values()),
                ConfirmSelection::No => {
                    self.step = SetupProjectStep::PresetList;
                    SetupProjectAction::Continue
                }
            },
            _ => SetupProjectAction::Continue,
        }
    }

    fn handle_confirm_editing(
        editor: &mut ConfirmEditor,
        rects: &Cell<[Rect; CONFIRM_BLOCK_COUNT]>,
        key: KeyEvent,
    ) -> SetupProjectAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let block = match editor.selection {
            ConfirmSelection::Block(i) => i,
            _ => {
                editor.stop_editing();
                return SetupProjectAction::Continue;
            }
        };

        let mutate = |editor: &mut ConfirmEditor| {
            let height = rects.get()[block].height;
            editor.clamp_scroll_for_cursor(block, height);
        };

        match key.code {
            KeyCode::Esc => {
                editor.stop_editing();
                SetupProjectAction::Continue
            }
            KeyCode::Enter => {
                editor.insert_newline();
                mutate(editor);
                SetupProjectAction::Continue
            }
            KeyCode::Backspace if ctrl || alt => {
                editor.delete_word_left();
                mutate(editor);
                SetupProjectAction::Continue
            }
            KeyCode::Backspace => {
                editor.delete_left();
                mutate(editor);
                SetupProjectAction::Continue
            }
            KeyCode::Delete => {
                editor.delete_right();
                mutate(editor);
                SetupProjectAction::Continue
            }
            KeyCode::Left if ctrl || alt => {
                editor.move_word_left();
                mutate(editor);
                SetupProjectAction::Continue
            }
            KeyCode::Left => {
                editor.move_cursor_left();
                mutate(editor);
                SetupProjectAction::Continue
            }
            KeyCode::Right if ctrl || alt => {
                editor.move_word_right();
                mutate(editor);
                SetupProjectAction::Continue
            }
            KeyCode::Right => {
                editor.move_cursor_right();
                mutate(editor);
                SetupProjectAction::Continue
            }
            KeyCode::Up => {
                editor.move_cursor_up();
                mutate(editor);
                SetupProjectAction::Continue
            }
            KeyCode::Down => {
                editor.move_cursor_down();
                mutate(editor);
                SetupProjectAction::Continue
            }
            KeyCode::Home => {
                editor.move_cursor_home();
                SetupProjectAction::Continue
            }
            KeyCode::End => {
                editor.move_cursor_end();
                SetupProjectAction::Continue
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
                    _ => return SetupProjectAction::Continue,
                }
                mutate(editor);
                SetupProjectAction::Continue
            }
            KeyCode::Char(c) if alt => {
                match c.to_ascii_lowercase() {
                    'b' => editor.move_word_left(),
                    'f' => editor.move_word_right(),
                    'd' => editor.delete_word_right(),
                    _ => return SetupProjectAction::Continue,
                }
                mutate(editor);
                SetupProjectAction::Continue
            }
            KeyCode::Char(c) => {
                editor.insert_char(c);
                mutate(editor);
                SetupProjectAction::Continue
            }
            _ => SetupProjectAction::Continue,
        }
    }

    pub fn preferred_content_height(&self) -> u16 {
        match self.step {
            // Intro line + spacer + select prompt (label + spacer + N rows
            // + footer) + footer description (3 lines).
            SetupProjectStep::PresetList => {
                let rows = self.select.options.len().min(12) as u16;
                3 + rows + 5
            }
            SetupProjectStep::Discovering => 3,
            SetupProjectStep::Confirm => 20,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        match self.step {
            SetupProjectStep::PresetList => self.render_preset_list(frame, area),
            SetupProjectStep::Discovering => self.render_discovering(frame, area),
            SetupProjectStep::Confirm => self.render_confirm(frame, area),
        }
    }

    fn render_preset_list(&self, frame: &mut Frame, area: Rect) {
        self.confirm_block_rects
            .set([Rect::default(); CONFIRM_BLOCK_COUNT]);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(3),
            ])
            .split(area);

        let info = Style::default().fg(colors::INFO);
        let intro = Line::from(vec![
            Span::styled("Pick a project preset to bootstrap ", info),
            Span::styled(
                ".wisetree.json",
                Style::default()
                    .fg(colors::EMPHASIS)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" with ", info),
            Span::styled(
                "Copy Patterns",
                Style::default()
                    .fg(colors::SUCCESS)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(", ", info),
            Span::styled(
                "Ignore Patterns",
                Style::default()
                    .fg(colors::ERROR)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(", ", info),
            Span::styled(
                "Shared Cache Links",
                Style::default()
                    .fg(colors::BRAND)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(", and ", info),
            Span::styled(
                "Post-Create Commands",
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(".", info),
        ]);
        frame.render_widget(Paragraph::new(intro), chunks[0]);

        self.select.render(frame, chunks[1]);

        let footer_lines = vec![
            Line::from(Span::styled(
                "Confirming will replace Copy Patterns, Ignore Patterns, Shared Cache Links, and Post-Create Commands in .wisetree.json with the chosen preset.",
                Style::default().fg(colors::MUTED),
            )),
            Line::from(Span::styled(
                "Shared cache links default to SeedFromSource so an installed source checkout can seed later worktrees.",
                Style::default().fg(colors::MUTED),
            )),
            Line::from(Span::styled(
                "Type to filter • ↑↓ to move • Enter to continue • Esc to clear search / go back",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            )),
        ];
        frame.render_widget(Paragraph::new(footer_lines), chunks[2]);
    }

    fn render_discovering(&self, frame: &mut Frame, area: Rect) {
        self.confirm_block_rects
            .set([Rect::default(); CONFIRM_BLOCK_COUNT]);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(area);

        StatusIndicator::new(
            Status::Loading,
            "Wise Preset is researching the repository...",
        )
        .with_tick(self.tick)
        .render(frame, chunks[0]);
        frame.render_widget(
            Paragraph::new("Scanning nested apps and framework-specific folders.").style(
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
            chunks[1],
        );
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        let Some(editor) = self.confirm.as_ref() else {
            self.render_discovering(frame, area);
            return;
        };

        let detail_total_height = area.height.saturating_sub(8);
        let lengths = [
            editor.block_len(0),
            editor.block_len(1),
            editor.block_len(2),
            editor.block_len(3),
        ];
        let [copy_h, ignore_h, link_h, post_h] =
            confirm_block_heights(lengths, detail_total_height);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(copy_h),
                Constraint::Length(ignore_h),
                Constraint::Length(link_h),
                Constraint::Length(post_h),
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);

        let title = Line::from(vec![
            Span::styled(
                "Apply ",
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                editor.label.clone(),
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " to ",
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                ".wisetree.json",
                Style::default()
                    .fg(colors::EMPHASIS)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "?",
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        frame.render_widget(Paragraph::new(title), chunks[0]);

        frame.render_widget(
            Paragraph::new(
                "Shared cache links will set worktreeLinkStrategy to SeedFromSource, reusing installed dependency directories across worktrees.",
            )
            .style(Style::default().fg(colors::MUTED).add_modifier(Modifier::DIM)),
            chunks[1],
        );

        self.confirm_block_rects
            .set([chunks[2], chunks[3], chunks[4], chunks[5]]);

        let editing = editor.editing_cursor();
        let specs = [
            ("worktreeCopyPatterns", colors::SUCCESS, chunks[2], 0),
            ("worktreeCopyIgnores", colors::ERROR, chunks[3], 1),
            ("worktreeLinkPatterns", colors::BRAND, chunks[4], 2),
            ("postCreateCmd", colors::ACCENT, chunks[5], 3),
        ];
        for (title, accent, rect, idx) in specs {
            let is_selected = matches!(editor.selection, ConfirmSelection::Block(i) if i == idx);
            let cursor = editing.and_then(|(b, c)| (b == idx).then_some(c));
            render_editable_block(
                frame,
                rect,
                title,
                &editor.blocks[idx],
                accent,
                editor.scrolls[idx],
                is_selected,
                cursor,
            );
        }

        render_yes_no(frame, chunks[7], editor.selection);

        let hint_text = if editor.editing.is_some() {
            "Enter newline • Ctrl+←→ word • Ctrl+W/Alt+D del word • Ctrl+U/K kill line • Ctrl+A/E start/end • Esc finish"
        } else {
            "↑↓ to move • ←→ Yes/No • Enter edit block / confirm • y/n shortcut • Esc back"
        };
        let hint = Paragraph::new(hint_text)
            .style(
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            )
            .alignment(Alignment::Center);
        frame.render_widget(hint, chunks[8]);
    }

    fn confirm_block_index_at(&self, position: Position) -> Option<usize> {
        self.confirm_block_rects
            .get()
            .iter()
            .position(|rect| contains_position(*rect, position))
    }
}

#[allow(clippy::too_many_arguments)]
fn render_editable_block(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    lines: &[String],
    accent: Color,
    scroll: u16,
    selected: bool,
    cursor: Option<EditCursor>,
) {
    let editing = cursor.is_some();
    let border_color = if editing { colors::WARNING } else { accent };
    let border_modifier = if selected || editing {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };
    let border_style = Style::default()
        .fg(border_color)
        .add_modifier(border_modifier);
    let title_style = Style::default()
        .fg(border_color)
        .add_modifier(Modifier::BOLD);

    let mut body: Vec<Line<'static>> = if lines.is_empty() && !editing {
        vec![Line::from(Span::styled(
            "(none — Enter to edit)",
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        ))]
    } else {
        lines
            .iter()
            .enumerate()
            .map(|(idx, line)| {
                let on_cursor_row = cursor.map(|c| c.row == idx).unwrap_or(false);
                if on_cursor_row {
                    cursor_line(line, cursor.unwrap().col)
                } else if line.is_empty() && editing {
                    Line::from(Span::styled(
                        " ",
                        Style::default()
                            .fg(colors::MUTED)
                            .add_modifier(Modifier::DIM),
                    ))
                } else {
                    Line::from(Span::styled(
                        line.clone(),
                        Style::default().fg(colors::WHITE),
                    ))
                }
            })
            .collect()
    };

    if selected && !editing {
        if let Some(first) = body.first_mut() {
            first.spans.insert(
                0,
                Span::styled(SELECTION_MARKER, Style::default().fg(colors::ACCENT)),
            );
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(border_style)
        .padding(Padding::horizontal(1))
        .title(Span::styled(format!(" {title} "), title_style));
    frame.render_widget(Paragraph::new(body).scroll((scroll, 0)).block(block), area);
}

fn cursor_line(text: &str, col: usize) -> Line<'static> {
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

fn split_detail_heights(total: u16) -> [u16; CONFIRM_BLOCK_COUNT] {
    let base = total / CONFIRM_BLOCK_COUNT as u16;
    let remainder = total % CONFIRM_BLOCK_COUNT as u16;
    let mut heights = [base; CONFIRM_BLOCK_COUNT];
    for height in heights.iter_mut().take(remainder as usize) {
        *height = height.saturating_add(1);
    }
    heights
}

fn confirm_block_heights(
    line_counts: [usize; CONFIRM_BLOCK_COUNT],
    total: u16,
) -> [u16; CONFIRM_BLOCK_COUNT] {
    let caps = split_detail_heights(total);
    std::array::from_fn(|idx| block_height_for_line_count(line_counts[idx]).min(caps[idx]))
}

fn block_height_for_line_count(line_count: usize) -> u16 {
    line_count.max(1) as u16 + 2
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn contains_position(area: Rect, position: Position) -> bool {
    position.x >= area.left()
        && position.x < area.right()
        && position.y >= area.top()
        && position.y < area.bottom()
}

fn render_yes_no(frame: &mut Frame, area: Rect, selection: ConfirmSelection) {
    let confirm_label = "Yes";
    let cancel_label = "No";
    let confirm_width = confirm_label.chars().count() as u16 + 4;
    let cancel_width = cancel_label.chars().count() as u16 + 4;
    let gap: u16 = 2;
    let total = confirm_width + cancel_width + gap;
    let side = area.width.saturating_sub(total) / 2;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(side),
            Constraint::Length(confirm_width),
            Constraint::Length(gap),
            Constraint::Length(cancel_width),
            Constraint::Min(0),
        ])
        .split(area);

    let confirm_selected = matches!(selection, ConfirmSelection::Yes);
    let cancel_selected = matches!(selection, ConfirmSelection::No);

    let confirm_text = Line::from(branded_line(
        confirm_label,
        if confirm_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        },
    ));
    let cancel_text = Line::from(branded_line(
        cancel_label,
        if cancel_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        },
    ));

    let confirm_border = if confirm_selected {
        colors::INFO
    } else {
        colors::MUTED
    };
    let cancel_border = if cancel_selected {
        colors::EMPHASIS
    } else {
        colors::MUTED
    };

    let confirm_box = Paragraph::new(confirm_text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(confirm_border))
            .padding(Padding::horizontal(1)),
    );
    let cancel_box = Paragraph::new(cancel_text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(cancel_border))
            .padding(Padding::horizontal(1)),
    );
    frame.render_widget(confirm_box, cols[1]);
    frame.render_widget(cancel_box, cols[3]);
}

/// Map the active selection back to the legacy `ConfirmChoice` enum so callers
/// that only care about Yes/No (e.g. some tests) can keep using it.
pub fn confirm_choice_for(selection: ConfirmSelection) -> ConfirmChoice {
    match selection {
        ConfirmSelection::No => ConfirmChoice::Cancel,
        _ => ConfirmChoice::Confirm,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(copy: usize, ignore: usize, links: usize, post: usize) -> SetupProjectPresetValues {
        SetupProjectPresetValues {
            label: "Wise Preset".into(),
            copy_patterns: (0..copy).map(|idx| format!("copy-{idx}")).collect(),
            copy_ignores: (0..ignore).map(|idx| format!("ignore-{idx}")).collect(),
            link_patterns: (0..links).map(|idx| format!("link-{idx}")).collect(),
            post_create_cmd: (0..post).map(|idx| format!("cmd-{idx}")).collect(),
        }
    }

    fn editor_from(copy: usize, ignore: usize, links: usize, post: usize) -> ConfirmEditor {
        ConfirmEditor::from_values(values(copy, ignore, links, post))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn confirm_block_heights_shrink_for_short_sections() {
        let heights = confirm_block_heights([1, 1, 1, 1], 20);
        assert_eq!(heights, [3, 3, 3, 3]);
    }

    #[test]
    fn confirm_block_heights_cap_tall_sections() {
        let heights = confirm_block_heights([20, 2, 2, 2], 20);
        assert_eq!(heights, [5, 4, 4, 4]);
    }

    #[test]
    fn editor_default_selection_is_yes() {
        let editor = editor_from(2, 1, 1, 1);
        assert_eq!(editor.selection(), ConfirmSelection::Yes);
    }

    #[test]
    fn editor_to_values_filters_empty_lines() {
        let mut editor = editor_from(0, 0, 0, 0);
        editor.blocks[0] = vec!["a".into(), "".into(), "  ".into(), "b".into()];
        let values = editor.to_values();
        assert_eq!(values.copy_patterns, vec!["a", "b"]);
    }

    #[test]
    fn editor_move_navigation_cycles_blocks_and_buttons() {
        let mut editor = editor_from(1, 1, 1, 1);
        editor.selection = ConfirmSelection::Block(0);
        editor.move_down();
        assert_eq!(editor.selection(), ConfirmSelection::Block(1));
        editor.move_down();
        assert_eq!(editor.selection(), ConfirmSelection::Block(2));
        editor.move_down();
        assert_eq!(editor.selection(), ConfirmSelection::Block(3));
        editor.move_down();
        assert_eq!(editor.selection(), ConfirmSelection::Yes);
        editor.toggle_yes_no();
        assert_eq!(editor.selection(), ConfirmSelection::No);
        editor.move_up();
        assert_eq!(editor.selection(), ConfirmSelection::Block(3));
    }

    #[test]
    fn enter_splits_line_into_new_entry() {
        let mut editor = editor_from(0, 0, 0, 0);
        editor.selection = ConfirmSelection::Block(0);
        editor.blocks[0] = vec!["hello world".into()];
        editor.editing = Some(EditCursor { row: 0, col: 5 });
        editor.insert_newline();
        assert_eq!(
            editor.blocks[0],
            vec!["hello".to_string(), " world".to_string()]
        );
        assert_eq!(editor.editing, Some(EditCursor { row: 1, col: 0 }));
    }

    #[test]
    fn backspace_at_col_zero_merges_with_previous_line() {
        let mut editor = editor_from(0, 0, 0, 0);
        editor.selection = ConfirmSelection::Block(0);
        editor.blocks[0] = vec!["foo".into(), "bar".into()];
        editor.editing = Some(EditCursor { row: 1, col: 0 });
        editor.delete_left();
        assert_eq!(editor.blocks[0], vec!["foobar".to_string()]);
        assert_eq!(editor.editing, Some(EditCursor { row: 0, col: 3 }));
    }

    #[test]
    fn screen_enter_apply_yields_filtered_values() {
        let mut screen = SetupProjectScreen::new(None);
        screen.confirm = Some(editor_from(2, 1, 1, 1));
        screen.step = SetupProjectStep::Confirm;
        let action = screen.handle_key(key(KeyCode::Enter));
        match action {
            SetupProjectAction::Apply(values) => {
                assert_eq!(values.copy_patterns.len(), 2);
                assert_eq!(values.copy_ignores.len(), 1);
                assert_eq!(values.link_patterns.len(), 1);
                assert_eq!(values.post_create_cmd.len(), 1);
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn screen_enter_on_block_starts_editing() {
        let mut screen = SetupProjectScreen::new(None);
        screen.confirm = Some(editor_from(2, 1, 1, 1));
        screen.step = SetupProjectStep::Confirm;
        screen.confirm.as_mut().unwrap().selection = ConfirmSelection::Block(0);
        screen.handle_key(key(KeyCode::Enter));
        assert!(screen.confirm.as_ref().unwrap().editing.is_some());
    }

    #[test]
    fn ctrl_w_deletes_previous_word() {
        let mut editor = editor_from(0, 0, 0, 0);
        editor.selection = ConfirmSelection::Block(0);
        editor.blocks[0] = vec!["foo bar baz".into()];
        editor.editing = Some(EditCursor { row: 0, col: 11 });
        editor.delete_word_left();
        assert_eq!(editor.blocks[0], vec!["foo bar ".to_string()]);
        assert_eq!(editor.editing, Some(EditCursor { row: 0, col: 8 }));
    }

    #[test]
    fn ctrl_u_kills_to_line_start() {
        let mut editor = editor_from(0, 0, 0, 0);
        editor.selection = ConfirmSelection::Block(0);
        editor.blocks[0] = vec!["foo bar".into()];
        editor.editing = Some(EditCursor { row: 0, col: 4 });
        editor.kill_to_start();
        assert_eq!(editor.blocks[0], vec!["bar".to_string()]);
        assert_eq!(editor.editing, Some(EditCursor { row: 0, col: 0 }));
    }

    #[test]
    fn ctrl_k_kills_to_line_end() {
        let mut editor = editor_from(0, 0, 0, 0);
        editor.selection = ConfirmSelection::Block(0);
        editor.blocks[0] = vec!["foo bar".into()];
        editor.editing = Some(EditCursor { row: 0, col: 3 });
        editor.kill_to_end();
        assert_eq!(editor.blocks[0], vec!["foo".to_string()]);
        assert_eq!(editor.editing, Some(EditCursor { row: 0, col: 3 }));
    }

    #[test]
    fn move_word_right_wraps_to_next_line_at_end() {
        let mut editor = editor_from(0, 0, 0, 0);
        editor.selection = ConfirmSelection::Block(0);
        editor.blocks[0] = vec!["foo".into(), "bar".into()];
        editor.editing = Some(EditCursor { row: 0, col: 3 });
        editor.move_word_right();
        assert_eq!(editor.editing, Some(EditCursor { row: 1, col: 0 }));
    }

    #[test]
    fn alt_d_deletes_next_word() {
        let mut editor = editor_from(0, 0, 0, 0);
        editor.selection = ConfirmSelection::Block(0);
        editor.blocks[0] = vec!["foo bar baz".into()];
        editor.editing = Some(EditCursor { row: 0, col: 4 });
        editor.delete_word_right();
        assert_eq!(editor.blocks[0], vec!["foo  baz".to_string()]);
        assert_eq!(editor.editing, Some(EditCursor { row: 0, col: 4 }));
    }

    #[test]
    fn ctrl_w_via_handle_key_routes_to_delete_word_left() {
        let mut screen = SetupProjectScreen::new(None);
        screen.confirm = Some(editor_from(0, 0, 0, 0));
        screen.step = SetupProjectStep::Confirm;
        let editor = screen.confirm.as_mut().unwrap();
        editor.selection = ConfirmSelection::Block(0);
        editor.blocks[0] = vec!["foo bar".into()];
        editor.editing = Some(EditCursor { row: 0, col: 7 });
        screen.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        let editor = screen.confirm.as_ref().unwrap();
        assert_eq!(editor.blocks[0], vec!["foo ".to_string()]);
    }

    #[test]
    fn screen_esc_during_edit_returns_to_navigation() {
        let mut screen = SetupProjectScreen::new(None);
        screen.confirm = Some(editor_from(1, 1, 1, 1));
        screen.step = SetupProjectStep::Confirm;
        screen.confirm.as_mut().unwrap().selection = ConfirmSelection::Block(0);
        screen.handle_key(key(KeyCode::Enter));
        screen.handle_key(key(KeyCode::Char('x')));
        screen.handle_key(key(KeyCode::Esc));
        let editor = screen.confirm.as_ref().unwrap();
        assert!(editor.editing.is_none());
        assert!(editor.blocks[0].iter().any(|l| l.contains('x')));
        assert_eq!(screen.step, SetupProjectStep::Confirm);
    }
}
