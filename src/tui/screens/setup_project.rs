//! Setup Project Config screen.
//!
//! Three-step flow that bootstraps a new project's `.wisetree.json` from a
//! preset:
//!
//! 1. `PresetList`  — `SelectPrompt` over `Wise Preset` plus the static preset
//!    catalog. A root-level match is still tagged `detected`; otherwise Wise is
//!    the default choice.
//! 2. `Discovering` — spinner while `App` performs the deep Wise scan.
//! 3. `Confirm`     — three rectangle blocks (Copy Patterns / Ignore Patterns /
//!    Post-Create Commands) showing the values that will be written, plus a
//!    Yes/No row with Yes pre-selected.
//!
//! `Esc` walks back one step (Confirm → PresetList; PresetList → menu).
//! `App` owns Wise discovery + persistence and consumes
//! [`SetupProjectAction::Apply`] to write the chosen values to disk.

use std::cell::Cell;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
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
const CONFIRM_BLOCK_COUNT: usize = 3;

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
    pub post_create_cmd: Vec<String>,
}

impl SetupProjectPresetValues {
    fn from_preset(preset: &Preset) -> Self {
        Self {
            label: preset.label.to_string(),
            copy_patterns: preset.copy_patterns_owned(),
            copy_ignores: preset.copy_ignores_owned(),
            post_create_cmd: preset.post_create_cmd_owned(),
        }
    }

    fn wise(discovery: WisePresetDiscovery) -> Self {
        Self {
            label: WISE_PRESET_CONFIRM_LABEL.to_string(),
            copy_patterns: discovery.copy_patterns,
            copy_ignores: discovery.copy_ignores,
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

pub struct SetupProjectScreen {
    step: SetupProjectStep,
    presets: Vec<Preset>,
    detected: Option<PresetId>,
    selected_choice: PresetChoice,
    select: SelectPrompt<PresetChoice>,
    confirm_choice: ConfirmChoice,
    pending_preset: Option<SetupProjectPresetValues>,
    confirm_block_scrolls: [u16; CONFIRM_BLOCK_COUNT],
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
            confirm_choice: ConfirmChoice::Confirm,
            pending_preset: None,
            confirm_block_scrolls: [0; CONFIRM_BLOCK_COUNT],
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

    pub fn confirm_choice(&self) -> ConfirmChoice {
        self.confirm_choice
    }

    pub fn complete_wise_discovery(&mut self, discovery: WisePresetDiscovery) {
        self.pending_preset = Some(SetupProjectPresetValues::wise(discovery));
        self.confirm_block_scrolls = [0; CONFIRM_BLOCK_COUNT];
        self.confirm_choice = ConfirmChoice::Confirm;
        self.step = SetupProjectStep::Confirm;
    }

    pub fn reset_after_wise_discovery_failure(&mut self) {
        self.pending_preset = None;
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

        match mouse.kind {
            MouseEventKind::ScrollDown => {
                self.scroll_confirm_block(block_idx, 1);
                true
            }
            MouseEventKind::ScrollUp => {
                self.scroll_confirm_block(block_idx, -1);
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

    fn confirm_preset(&self) -> Option<&SetupProjectPresetValues> {
        self.pending_preset.as_ref()
    }

    fn handle_preset_list(&mut self, key: KeyEvent) -> SetupProjectAction {
        match self.select.handle_key(key) {
            SelectOutcome::Selected(_, choice) => {
                self.selected_choice = choice;
                self.confirm_choice = ConfirmChoice::Confirm;
                match choice {
                    PresetChoice::Wise => {
                        self.pending_preset = None;
                        self.confirm_block_scrolls = [0; CONFIRM_BLOCK_COUNT];
                        self.step = SetupProjectStep::Discovering;
                        SetupProjectAction::DiscoverWise
                    }
                    PresetChoice::Catalog(id) => {
                        self.pending_preset =
                            Some(SetupProjectPresetValues::from_preset(self.preset(id)));
                        self.confirm_block_scrolls = [0; CONFIRM_BLOCK_COUNT];
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
        match key.code {
            KeyCode::Esc => {
                self.step = SetupProjectStep::PresetList;
                SetupProjectAction::Continue
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                self.confirm_choice = match self.confirm_choice {
                    ConfirmChoice::Confirm => ConfirmChoice::Cancel,
                    ConfirmChoice::Cancel => ConfirmChoice::Confirm,
                };
                SetupProjectAction::Continue
            }
            KeyCode::Char(c) => {
                match c.to_ascii_lowercase() {
                    'y' => self.confirm_choice = ConfirmChoice::Confirm,
                    'n' => self.confirm_choice = ConfirmChoice::Cancel,
                    _ => {}
                }
                SetupProjectAction::Continue
            }
            KeyCode::Enter => match self.confirm_choice {
                ConfirmChoice::Confirm => self
                    .confirm_preset()
                    .cloned()
                    .map(SetupProjectAction::Apply)
                    .unwrap_or(SetupProjectAction::Continue),
                ConfirmChoice::Cancel => {
                    self.step = SetupProjectStep::PresetList;
                    SetupProjectAction::Continue
                }
            },
            _ => SetupProjectAction::Continue,
        }
    }

    pub fn preferred_content_height(&self) -> u16 {
        match self.step {
            // Intro line + spacer + select prompt (label + spacer + N rows
            // + footer) + footer description (2 lines).
            SetupProjectStep::PresetList => {
                let rows = self.select.options.len().min(12) as u16;
                3 + rows + 4
            }
            SetupProjectStep::Discovering => 3,
            SetupProjectStep::Confirm => 18,
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
                Constraint::Length(2),
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
                "Confirming will replace Copy Patterns, Ignore Patterns, and Post-Create Commands in .wisetree.json with the chosen preset.",
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
        let Some(preset) = self.confirm_preset() else {
            self.render_discovering(frame, area);
            return;
        };

        let detail_total_height = area.height.saturating_sub(7);
        let [copy_h, ignore_h, post_h] = confirm_block_heights(preset, detail_total_height);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(copy_h),
                Constraint::Length(ignore_h),
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
                preset.label.clone(),
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

        self.confirm_block_rects
            .set([chunks[2], chunks[3], chunks[4]]);

        render_block(
            frame,
            chunks[2],
            "worktreeCopyPatterns",
            &preset.copy_patterns,
            colors::SUCCESS,
            self.confirm_block_scrolls[0],
        );
        render_block(
            frame,
            chunks[3],
            "worktreeCopyIgnores",
            &preset.copy_ignores,
            colors::ERROR,
            self.confirm_block_scrolls[1],
        );
        render_block(
            frame,
            chunks[4],
            "postCreateCmd",
            &preset.post_create_cmd,
            colors::ACCENT,
            self.confirm_block_scrolls[2],
        );

        render_yes_no(frame, chunks[6], self.confirm_choice);

        let hint = Paragraph::new(
            "Mouse wheel scrolls inside preset boxes • ←→/Tab toggle • Enter confirm • Esc back",
        )
        .style(
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        )
        .alignment(Alignment::Center);
        frame.render_widget(hint, chunks[7]);
    }

    fn confirm_block_index_at(&self, position: Position) -> Option<usize> {
        self.confirm_block_rects
            .get()
            .iter()
            .position(|rect| contains_position(*rect, position))
    }

    fn scroll_confirm_block(&mut self, block_idx: usize, delta: i16) {
        let Some(preset) = self.confirm_preset() else {
            return;
        };

        let rect = self.confirm_block_rects.get()[block_idx];
        let visible_lines = rect.height.saturating_sub(2) as usize;
        let total_lines = confirm_block_line_count(preset, block_idx);
        let max_scroll = total_lines.saturating_sub(visible_lines) as u16;
        let current = self.confirm_block_scrolls[block_idx];
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as u16).min(max_scroll)
        };
        self.confirm_block_scrolls[block_idx] = next.min(max_scroll);
    }
}

fn render_block(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    lines: &[String],
    accent: ratatui::style::Color,
    scroll: u16,
) {
    let body: Vec<Line<'static>> = if lines.is_empty() {
        vec![Line::from(Span::styled(
            "(none)",
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        ))]
    } else {
        lines
            .iter()
            .map(|line| {
                Line::from(Span::styled(
                    line.clone(),
                    Style::default().fg(colors::WHITE),
                ))
            })
            .collect()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(accent))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ));
    frame.render_widget(Paragraph::new(body).scroll((scroll, 0)).block(block), area);
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

fn confirm_block_heights(preset: &SetupProjectPresetValues, total: u16) -> [u16; CONFIRM_BLOCK_COUNT] {
    let caps = split_detail_heights(total);
    let desired = [
        block_height_for_line_count(preset.copy_patterns.len()),
        block_height_for_line_count(preset.copy_ignores.len()),
        block_height_for_line_count(preset.post_create_cmd.len()),
    ];

    [
        desired[0].min(caps[0]),
        desired[1].min(caps[1]),
        desired[2].min(caps[2]),
    ]
}

fn block_height_for_line_count(line_count: usize) -> u16 {
    line_count.max(1) as u16 + 2
}

fn confirm_block_line_count(preset: &SetupProjectPresetValues, block_idx: usize) -> usize {
    match block_idx {
        0 => preset.copy_patterns.len().max(1),
        1 => preset.copy_ignores.len().max(1),
        2 => preset.post_create_cmd.len().max(1),
        _ => 0,
    }
}

fn contains_position(area: Rect, position: Position) -> bool {
    position.x >= area.left()
        && position.x < area.right()
        && position.y >= area.top()
        && position.y < area.bottom()
}

fn render_yes_no(frame: &mut Frame, area: Rect, choice: ConfirmChoice) {
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

    let confirm_selected = matches!(choice, ConfirmChoice::Confirm);
    let cancel_selected = matches!(choice, ConfirmChoice::Cancel);

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

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(copy: usize, ignore: usize, post: usize) -> SetupProjectPresetValues {
        SetupProjectPresetValues {
            label: "Wise Preset".into(),
            copy_patterns: (0..copy).map(|idx| format!("copy-{idx}")).collect(),
            copy_ignores: (0..ignore).map(|idx| format!("ignore-{idx}")).collect(),
            post_create_cmd: (0..post).map(|idx| format!("cmd-{idx}")).collect(),
        }
    }

    #[test]
    fn confirm_block_heights_shrink_for_short_sections() {
        let heights = confirm_block_heights(&preset(1, 1, 1), 15);
        assert_eq!(heights, [3, 3, 3]);
    }

    #[test]
    fn confirm_block_heights_cap_tall_sections() {
        let heights = confirm_block_heights(&preset(20, 2, 2), 15);
        assert_eq!(heights, [5, 4, 4]);
    }
}
