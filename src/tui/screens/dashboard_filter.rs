//! Global Dashboard filter evaluation. The editor and dashboard share this model.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::config::schema::{
    DashboardFilter, DashboardFilterField, DashboardFilterKind, DashboardFilterOperator,
    DashboardFilterRule,
};
use crate::messages::{self, colors};
use crate::services::DashboardRow;
use crate::tui::widgets::branded_line;

use super::dashboard::{dashboard_worktree_name, remote_only, status_label_and_style};

impl DashboardFilter {
    pub fn rule_count(&self) -> usize {
        self.groups.iter().map(|group| group.rules.len()).sum()
    }

    /// Compact rule descriptions shared with the dashboard's hover preview.
    pub fn tooltip_lines(&self, width: usize) -> Vec<Line<'static>> {
        if self.rule_count() == 0 {
            return vec![Line::styled(messages::GLOBAL_FILTER_NONE, muted())];
        }
        let mut lines = Vec::new();
        for (group_index, group) in self.groups.iter().enumerate() {
            if group_index > 0 {
                lines.push(or_line());
            }
            for (rule_index, rule) in group.rules.iter().enumerate() {
                if rule_index > 0 {
                    lines.push(Line::styled("AND", keyword()));
                }
                let prefix = rule_prefix(rule);
                let prefix_width = Line::from(prefix.clone()).width() + 1;
                let fits = |shown: usize| {
                    prefix_width
                        + Line::from(value_spans(
                            &rule.values[..shown],
                            rule.values.len() - shown,
                        ))
                        .width()
                        <= width
                };
                let shown = (1..=rule.values.len())
                    .take_while(|shown| fits(*shown))
                    .last()
                    .unwrap_or(0);
                let values = value_spans(&rule.values[..shown], rule.values.len() - shown);
                if !fits(shown) && shown < rule.values.len() {
                    lines.push(Line::from(prefix));
                    lines.push(Line::from(values));
                } else {
                    lines.push(Line::from([prefix, vec![Span::raw(" ")], values].concat()));
                }
            }
        }
        lines
    }

    pub fn matches(&self, row: &DashboardRow) -> bool {
        self.groups.is_empty()
            || self
                .groups
                .iter()
                .any(|group| group.rules.iter().all(|rule| rule.matches(row)))
    }
}

impl DashboardFilterField {
    /// Dynamic enum choices retain their displayed spelling and deterministic order.
    pub fn choices(self, rows: &[DashboardRow]) -> Vec<String> {
        let mut choices: Vec<String> = self
            .enum_values()
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        if matches!(self, Self::Profile | Self::Repository) {
            choices.extend(rows.iter().flat_map(|row| field_values(self, row)));
            choices.sort();
            choices.dedup();
        }
        choices
    }
}

impl DashboardFilterRule {
    pub fn matches(&self, row: &DashboardRow) -> bool {
        use DashboardFilterOperator as Op;
        let actual = field_values(self.field, row);
        // Negation applies to the entire any-of comparison, including absent data.
        let matched = actual.iter().any(|actual| {
            self.values.iter().any(|expected| {
                if self.field.kind() == DashboardFilterKind::Number {
                    let (Ok(actual), Ok(expected)) =
                        (actual.parse::<i128>(), expected.parse::<i128>())
                    else {
                        return false;
                    };
                    match self.operator {
                        Op::Equal | Op::NotEqual => actual == expected,
                        Op::GreaterThan => actual > expected,
                        Op::GreaterThanOrEqual => actual >= expected,
                        Op::LessThan => actual < expected,
                        Op::LessThanOrEqual => actual <= expected,
                        _ => false,
                    }
                } else {
                    let actual = actual.to_lowercase();
                    let expected = expected.to_lowercase();
                    match self.operator {
                        Op::Contains | Op::DoesNotContain => actual.contains(&expected),
                        Op::Is | Op::IsNot | Op::IsAnyOf | Op::IsNoneOf => actual == expected,
                        _ => false,
                    }
                }
            })
        });
        if self.operator.is_negated() {
            !matched
        } else {
            matched
        }
    }
}

fn field_values(field: DashboardFilterField, row: &DashboardRow) -> Vec<String> {
    use DashboardFilterField as Field;
    let local = !remote_only(row);
    let status = row.worktree.branch_status.as_ref().filter(|_| local);
    let pr = row.pull_request.as_ref();
    let value = match field {
        Field::Profile => row.repository.as_ref().map(|repo| repo.owner.clone()),
        Field::Repository => row.repository.as_ref().map(|repo| repo.name.clone()),
        Field::Location => Some(if local { "Local" } else { "Not cloned" }.into()),
        Field::Status => Some(status_label_and_style(row).0.into()),
        Field::Checks => pr.and_then(|pr| pr.checks_status).map(|s| format!("{s:?}")),
        Field::Review => pr.and_then(|pr| pr.review_status).map(|s| format!("{s:?}")),
        Field::Mergeability => pr.and_then(|pr| pr.merge_status).map(|s| format!("{s:?}")),
        Field::Worktree => Some(dashboard_worktree_name(row)),
        Field::Branch => Some(row.worktree.branch.clone()),
        Field::From => status.and_then(|s| s.source_branch.clone()),
        Field::LastCommit => {
            return row
                .last_commit
                .as_ref()
                .filter(|_| local)
                .map(|commit| {
                    vec![
                        commit.summary.clone(),
                        commit.author.clone(),
                        commit.sha.clone(),
                    ]
                })
                .unwrap_or_default()
        }
        Field::PrTitle => pr.map(|pr| pr.title.clone()),
        Field::Ahead => status.map(|s| s.ahead.to_string()),
        Field::Behind => status.map(|s| s.behind.to_string()),
        Field::Insertions => status.and_then(|s| s.insertions).map(|n| n.to_string()),
        Field::Deletions => status.and_then(|s| s.deletions).map(|n| n.to_string()),
        Field::PrNumber => pr.map(|pr| pr.number.to_string()),
    };
    match value {
        Some(value) => vec![value],
        None if field.kind() == DashboardFilterKind::Enum => vec!["None".into()],
        None => Vec::new(),
    }
}

/// A draft stays isolated from the dashboard until Apply. Rows are borrowed on
/// every event/render so polling can update choices and counts without resetting it.
pub struct DashboardFilterEditor {
    draft: DashboardFilter,
    focus: usize,
    rule_focus: usize,
    step: EditorStep,
    target: (usize, Option<usize>),
    field: DashboardFilterField,
    operator: DashboardFilterOperator,
    selected_values: Vec<String>,
    input: crate::tui::widgets::InputPrompt,
    choice: usize,
    error: Option<&'static str>,
    hits: Vec<(ratatui::layout::Rect, EditorHit)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorStep {
    Overview,
    Field,
    Operator,
    Value,
}

#[derive(Clone, Copy)]
enum EditorHit {
    Focus(usize),
    Choice(usize),
    Confirm,
    Back,
}

pub enum FilterEditorOutcome {
    Pending,
    Apply(DashboardFilter),
    Cancel,
}

impl DashboardFilterEditor {
    pub fn new(filter: DashboardFilter) -> Self {
        Self {
            draft: filter,
            focus: 0,
            rule_focus: 0,
            step: EditorStep::Overview,
            target: (0, None),
            field: DashboardFilterField::Status,
            operator: DashboardFilterOperator::IsAnyOf,
            selected_values: Vec::new(),
            input: crate::tui::widgets::InputPrompt::new(""),
            choice: 0,
            error: None,
            hits: Vec::new(),
        }
    }

    fn rules(&self) -> Vec<(usize, usize)> {
        self.draft
            .groups
            .iter()
            .enumerate()
            .flat_map(|(g, group)| (0..group.rules.len()).map(move |r| (g, r)))
            .collect()
    }

    fn focused_group(&self) -> usize {
        self.rules()
            .get(self.focus)
            .map(|(g, _)| *g)
            .unwrap_or_else(|| self.target.0.min(self.draft.groups.len().saturating_sub(1)))
    }

    fn set_input(&mut self, value: &str) {
        self.input.value = value.to_string();
        self.input.cursor = value.chars().count();
    }

    fn start_rule(&mut self, group: usize, rule: Option<usize>) {
        self.target = (group, rule);
        self.selected_values.clear();
        self.field = DashboardFilterField::Status;
        self.operator = DashboardFilterOperator::IsAnyOf;
        if let Some(rule) = rule.and_then(|r| self.draft.groups.get(group)?.rules.get(r)) {
            self.field = rule.field;
            self.operator = rule.operator;
            self.selected_values = rule.values.clone();
        }
        self.step = EditorStep::Field;
        self.input.placeholder = crate::messages::GLOBAL_FILTER_FIELD_INPUT.to_string();
        self.set_input("");
        self.choice = DashboardFilterField::ALL
            .iter()
            .position(|f| *f == self.field)
            .unwrap_or(0);
        self.error = None;
    }

    fn fields(&self) -> Vec<DashboardFilterField> {
        DashboardFilterField::ALL
            .into_iter()
            .filter(|f| {
                f.label()
                    .to_lowercase()
                    .contains(&self.input.value.to_lowercase())
            })
            .collect()
    }

    fn values(&self, rows: &[DashboardRow]) -> Vec<String> {
        if self.field.kind() == DashboardFilterKind::Number {
            return Vec::new();
        }
        let mut values = if self.field.kind() == DashboardFilterKind::Enum {
            self.field.choices(rows)
        } else {
            rows.iter()
                .flat_map(|row| field_values(self.field, row))
                .collect()
        };
        // Preserve custom dynamic enum values, even if their rows disappear.
        if matches!(
            self.field,
            DashboardFilterField::Profile | DashboardFilterField::Repository
        ) {
            values.extend(self.selected_values.clone());
        }
        if self.field.enum_values().is_empty() {
            values.sort();
            values.dedup();
        }
        values
            .into_iter()
            .filter(|v| {
                self.input.value.is_empty()
                    || v.to_lowercase().contains(&self.input.value.to_lowercase())
            })
            .collect()
    }

    fn pending_rule(&self) -> Option<DashboardFilterRule> {
        let values = if self.field.kind() == DashboardFilterKind::Enum {
            let mut values = self.selected_values.clone();
            if matches!(
                self.field,
                DashboardFilterField::Profile | DashboardFilterField::Repository
            ) && !self.input.value.trim().is_empty()
                && !values
                    .iter()
                    .any(|v| v.eq_ignore_ascii_case(self.input.value.trim()))
            {
                values.push(self.input.value.trim().to_string());
            }
            values
        } else {
            let value = self.input.value.trim();
            if value.is_empty()
                || (self.field.kind() == DashboardFilterKind::Number
                    && value.parse::<i128>().is_err())
            {
                return None;
            }
            vec![value.to_string()]
        };
        (!values.is_empty()).then_some(DashboardFilterRule {
            field: self.field,
            operator: self.operator,
            values,
        })
    }

    fn put_rule(
        filter: &mut DashboardFilter,
        target: (usize, Option<usize>),
        rule: DashboardFilterRule,
    ) {
        if target.0 >= filter.groups.len() {
            filter
                .groups
                .push(crate::config::schema::DashboardFilterGroup { rules: vec![rule] });
        } else if let Some(index) = target.1 {
            filter.groups[target.0].rules[index] = rule;
        } else {
            filter.groups[target.0].rules.push(rule);
        }
    }

    fn activate(&mut self) -> FilterEditorOutcome {
        let rules = self.rules();
        if let Some(&(group, rule)) = rules.get(self.focus) {
            self.start_rule(group, Some(rule));
        } else {
            match self.focus.saturating_sub(rules.len()) {
                0 => self.start_rule(self.focused_group(), None),
                1 => self.start_rule(self.draft.groups.len(), None),
                2 => {
                    self.draft = DashboardFilter::default();
                    self.focus = 0;
                    self.target.0 = 0;
                }
                3 => {
                    self.draft.groups.clear();
                    self.focus = 0;
                    self.target.0 = 0;
                }
                4 => {
                    let mut filter = self.draft.clone();
                    if filter.normalize().is_ok() {
                        return FilterEditorOutcome::Apply(filter);
                    }
                }
                5 => return FilterEditorOutcome::Cancel,
                _ => {}
            }
        }
        FilterEditorOutcome::Pending
    }

    fn confirm(&mut self) {
        match self.step {
            EditorStep::Field => {
                let Some(field) = self.fields().get(self.choice).copied() else {
                    return;
                };
                if self.field != field {
                    self.selected_values.clear();
                }
                self.field = field;
                let operators = DashboardFilterOperator::for_kind(field.kind());
                self.choice = operators
                    .iter()
                    .position(|op| *op == self.operator)
                    .unwrap_or(0);
                self.operator = operators[self.choice];
                self.step = EditorStep::Operator;
                self.set_input("");
            }
            EditorStep::Operator => {
                self.operator = DashboardFilterOperator::for_kind(self.field.kind())[self.choice];
                self.step = EditorStep::Value;
                self.input.placeholder = match self.field.kind() {
                    DashboardFilterKind::Enum => crate::messages::GLOBAL_FILTER_ENUM_INPUT,
                    DashboardFilterKind::Text => crate::messages::GLOBAL_FILTER_TEXT_INPUT,
                    DashboardFilterKind::Number => crate::messages::GLOBAL_FILTER_NUMBER_INPUT,
                }
                .to_string();
                self.choice = 0;
                let value = if self.field.kind() == DashboardFilterKind::Enum {
                    String::new()
                } else {
                    self.selected_values.first().cloned().unwrap_or_default()
                };
                self.set_input(&value);
            }
            EditorStep::Value => {
                if let Some(rule) = self.pending_rule() {
                    Self::put_rule(&mut self.draft, self.target, rule);
                    self.focus =
                        self.rules()
                            .iter()
                            .position(|(g, r)| {
                                *g == self.target.0
                                    && self.target.1.map_or(
                                        *r + 1 == self.draft.groups[*g].rules.len(),
                                        |index| index == *r,
                                    )
                            })
                            .unwrap_or(0);
                    self.step = EditorStep::Overview;
                    self.error = None;
                } else {
                    self.error = Some(if self.field.kind() == DashboardFilterKind::Number {
                        crate::messages::GLOBAL_FILTER_INTEGER_ERROR
                    } else {
                        crate::messages::GLOBAL_FILTER_VALUE_ERROR
                    });
                }
            }
            EditorStep::Overview => {}
        }
    }

    fn back(&mut self) -> FilterEditorOutcome {
        self.error = None;
        match self.step {
            EditorStep::Overview => return FilterEditorOutcome::Cancel,
            EditorStep::Field => self.step = EditorStep::Overview,
            EditorStep::Operator => {
                self.step = EditorStep::Field;
                self.input.placeholder = crate::messages::GLOBAL_FILTER_FIELD_INPUT.to_string();
                self.set_input("");
                self.choice = DashboardFilterField::ALL
                    .iter()
                    .position(|f| *f == self.field)
                    .unwrap_or(0);
            }
            EditorStep::Value => {
                // Retain valid input when revisiting the operator step.
                if let Some(rule) = self.pending_rule() {
                    self.selected_values = rule.values;
                }
                self.step = EditorStep::Operator;
                self.choice = DashboardFilterOperator::for_kind(self.field.kind())
                    .iter()
                    .position(|op| *op == self.operator)
                    .unwrap_or(0);
            }
        }
        FilterEditorOutcome::Pending
    }

    fn toggle(&mut self, rows: &[DashboardRow]) {
        if let Some(value) = self.values(rows).get(self.choice) {
            if let Some(index) = self.selected_values.iter().position(|v| v == value) {
                self.selected_values.remove(index);
            } else {
                self.selected_values.push(value.clone());
            }
            self.error = None;
        }
    }

    pub fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        rows: &[DashboardRow],
    ) -> FilterEditorOutcome {
        use crossterm::event::KeyCode;
        if key.code == KeyCode::Esc {
            return self.back();
        }
        if self.step == EditorStep::Overview {
            if let Some(&(group, _)) = self.rules().get(self.focus) {
                self.target.0 = group;
            }
            let count = self.draft.rule_count();
            // Arrows move inside the focused section (rules or buttons);
            // Tab switches sections, returning to the rule it left.
            let in_rules = self.focus < count;
            let (start, len) = if in_rules {
                (0, count)
            } else {
                (count, messages::GLOBAL_FILTER_BUTTONS.len())
            };
            let previous = start + (self.focus - start + len - 1) % len;
            let next = start + (self.focus - start + 1) % len;
            match key.code {
                KeyCode::Up => self.focus = previous,
                KeyCode::Down => self.focus = next,
                KeyCode::Left if !in_rules => self.focus = previous,
                KeyCode::Right if !in_rules => self.focus = next,
                KeyCode::Tab | KeyCode::BackTab if in_rules => {
                    self.rule_focus = self.focus;
                    self.focus = count;
                }
                KeyCode::Tab | KeyCode::BackTab if count > 0 => {
                    self.focus = self.rule_focus.min(count - 1)
                }
                KeyCode::Enter => return self.activate(),
                KeyCode::Char('a') => self.start_rule(self.focused_group(), None),
                KeyCode::Char('o') => self.start_rule(self.draft.groups.len(), None),
                KeyCode::Backspace | KeyCode::Delete => {
                    if let Some(&(g, r)) = self.rules().get(self.focus) {
                        self.draft.groups[g].rules.remove(r);
                        self.draft.groups.retain(|g| !g.rules.is_empty());
                        self.focus = self.focus.min(self.draft.rule_count().saturating_sub(1));
                        self.target.0 = self.focused_group();
                    }
                }
                _ => {}
            }
        } else {
            let count = match self.step {
                EditorStep::Field => self.fields().len(),
                EditorStep::Operator => DashboardFilterOperator::for_kind(self.field.kind()).len(),
                _ => self.values(rows).len(),
            };
            match key.code {
                KeyCode::Up => self.choice = self.choice.saturating_sub(1),
                KeyCode::Down => self.choice = (self.choice + 1).min(count.saturating_sub(1)),
                KeyCode::Enter => self.confirm(),
                KeyCode::Char(' ')
                    if self.step == EditorStep::Value
                        && self.field.kind() == DashboardFilterKind::Enum =>
                {
                    self.toggle(rows)
                }
                KeyCode::Tab
                    if self.step == EditorStep::Value
                        && self.field.kind() != DashboardFilterKind::Enum =>
                {
                    if let Some(value) = self.values(rows).get(self.choice).cloned() {
                        self.set_input(&value);
                    }
                }
                _ if self.step != EditorStep::Operator => {
                    self.input.handle_key(key);
                    self.choice = 0;
                    self.error = None;
                }
                _ => {}
            }
        }
        FilterEditorOutcome::Pending
    }

    pub fn handle_mouse_click(
        &mut self,
        position: ratatui::layout::Position,
        rows: &[DashboardRow],
    ) -> FilterEditorOutcome {
        if let Some(hit) = self
            .hits
            .iter()
            .find(|(rect, _)| rect.contains(position))
            .map(|(_, hit)| *hit)
        {
            match hit {
                EditorHit::Focus(index) => {
                    self.focus = index;
                    return self.activate();
                }
                EditorHit::Choice(index) => {
                    self.choice = index;
                    if self.step == EditorStep::Value {
                        if self.field.kind() == DashboardFilterKind::Enum {
                            self.toggle(rows);
                        } else if let Some(value) = self.values(rows).get(index).cloned() {
                            self.set_input(&value);
                        }
                    } else {
                        self.confirm();
                    }
                }
                EditorHit::Confirm => self.confirm(),
                EditorHit::Back => return self.back(),
            }
        }
        FilterEditorOutcome::Pending
    }

    pub fn render(&mut self, frame: &mut ratatui::Frame, area: Rect, rows: &[DashboardRow]) {
        use ratatui::layout::{Constraint, Layout};
        self.hits.clear();
        let mut preview = self.draft.clone();
        if self.step == EditorStep::Value {
            if let Some(rule) = self.pending_rule() {
                Self::put_rule(&mut preview, self.target, rule);
            }
        }
        let count = self.draft.rule_count();
        let overview = self.step == EditorStep::Overview;
        let confirm = if self.step == EditorStep::Value {
            messages::GLOBAL_FILTER_SAVE
        } else {
            messages::GLOBAL_FILTER_NEXT
        };
        let buttons: Vec<(&str, Color, bool, EditorHit)> = if overview {
            let roles = [
                colors::INFO,
                colors::FILTER_KEYWORD,
                colors::WARNING,
                colors::ERROR,
                colors::SUCCESS,
                colors::GRAY_LIGHT,
            ];
            messages::GLOBAL_FILTER_BUTTONS
                .iter()
                .zip(roles)
                .enumerate()
                .map(|(index, (label, color))| {
                    let focus = count + index;
                    (*label, color, self.focus == focus, EditorHit::Focus(focus))
                })
                .collect()
        } else {
            vec![
                (confirm, colors::SUCCESS, false, EditorHit::Confirm),
                (
                    messages::GLOBAL_FILTER_BACK,
                    colors::GRAY_LIGHT,
                    false,
                    EditorHit::Back,
                ),
            ]
        };
        // Commit actions (Apply/Cancel, Next/Back) hug the right edge.
        let labels: Vec<&str> = buttons.iter().map(|(label, ..)| *label).collect();
        let grid = button_grid(area.width, &labels, 2);
        let button_rows = grid.iter().map(|(_, row, _)| row + 1).max().unwrap_or(0);
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(button_rows * 3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
        frame.render_widget(
            Paragraph::new(messages::GLOBAL_FILTER_TITLE).style(
                Style::default()
                    .fg(colors::BRAND)
                    .add_modifier(Modifier::BOLD),
            ),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(summary_line(
                rows.iter().filter(|r| preview.matches(r)).count(),
                rows.len(),
            )),
            chunks[2],
        );
        if overview {
            self.render_rules(frame, chunks[4]);
        } else {
            self.render_step(frame, chunks[4], rows);
        }
        for ((x, row, width), (label, color, focused, hit)) in grid.into_iter().zip(buttons) {
            let rect =
                Rect::new(chunks[6].x + x, chunks[6].y + row * 3, width, 3).intersection(chunks[6]);
            render_button(frame, rect, label, color, focused);
            self.hits.push((rect, hit));
        }
        let hint: Vec<(&'static str, &'static str)> = if !overview {
            // Enter does what the confirm button says: Next, or Save on the last step.
            messages::GLOBAL_FILTER_VALUE_HINT
                .into_iter()
                .map(|(key, action)| (key, if key == "Enter" { confirm } else { action }))
                .collect()
        } else if self.focus < count {
            messages::GLOBAL_FILTER_RULES_HINT.to_vec()
        } else {
            // Without rules there is no other section for Tab to reach.
            messages::GLOBAL_FILTER_BUTTONS_HINT
                .into_iter()
                .filter(|(key, _)| count > 0 || *key != "Tab")
                .collect()
        };
        frame.render_widget(Paragraph::new(hint_line(&hint)), chunks[8]);
    }

    /// One bordered card per OR group; the focused rule gets the selection bar.
    fn render_rules(&mut self, frame: &mut ratatui::Frame, body: Rect) {
        let width = body.width as usize;
        let border = muted();
        // Scroll whole card lines together; focus remains visible for arbitrarily many rules.
        let mut lines = Vec::new();
        let mut rule_lines = Vec::new();
        let mut index = 0;
        for (g, group) in self.draft.groups.iter().enumerate() {
            if g > 0 {
                lines.push(or_line().alignment(Alignment::Center));
            }
            let [prefix, all, suffix] = messages::GLOBAL_FILTER_ALL;
            let title = Line::from(format!("{prefix}{all}{suffix}")).width();
            lines.push(Line::from(vec![
                Span::styled("┌─ ", border),
                Span::styled(prefix, muted()),
                Span::styled(all, keyword()),
                Span::styled(suffix, muted()),
                Span::styled(
                    format!(" {}┐", "─".repeat(width.saturating_sub(title + 5))),
                    border,
                ),
            ]));
            for rule in &group.rules {
                rule_lines.push((index, lines.len()));
                let focused = self.focus == index;
                // Room left between "│ " + marker and " │".
                let available = width.saturating_sub(6);
                let mut content = fit_spans(
                    [
                        rule_prefix(rule),
                        vec![Span::raw(" ")],
                        value_spans(&rule.values, 0),
                    ]
                    .concat(),
                    available,
                );
                let padding = available.saturating_sub(Line::from(content.clone()).width());
                content.push(Span::raw(" ".repeat(padding)));
                let mut spans = vec![Span::styled("│", border)];
                spans.extend(selectable_spans(content, focused));
                spans.push(Span::styled("│", border));
                lines.push(Line::from(spans));
                index += 1;
            }
            lines.push(Line::styled(
                format!("└{}┘", "─".repeat(width.saturating_sub(2))),
                border,
            ));
        }
        if lines.is_empty() {
            lines.push(Line::styled(
                messages::GLOBAL_FILTER_NONE,
                muted().add_modifier(Modifier::ITALIC),
            ));
        }
        let selected_line = rule_lines
            .iter()
            .find(|(i, _)| *i == self.focus)
            .map(|(_, l)| *l)
            .unwrap_or(0);
        let offset = selected_line.saturating_sub(body.height.saturating_sub(2) as usize);
        frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), body);
        for (index, line) in rule_lines {
            if line >= offset && line - offset < body.height as usize {
                self.hits.push((
                    Rect::new(body.x, body.y + (line - offset) as u16, body.width, 1),
                    EditorHit::Focus(index),
                ));
            }
        }
    }

    /// The field / operator / value picker of the guided rule flow.
    fn render_step(&mut self, frame: &mut ratatui::Frame, body: Rect, rows: &[DashboardRow]) {
        use ratatui::layout::{Constraint, Layout};
        let [card, status] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(body);
        let heading = Style::default()
            .fg(colors::WHITE)
            .add_modifier(Modifier::BOLD);
        let field = Span::styled(
            self.field.label(),
            Style::default().fg(colors::FILTER_FIELD),
        );
        let operator = Span::styled(
            self.operator.label(),
            Style::default().fg(colors::FILTER_OPERATOR),
        );
        let mut title = vec![Span::raw(" ")];
        match self.step {
            EditorStep::Field => {
                title.push(Span::styled(messages::GLOBAL_FILTER_FIELD_STEP, heading))
            }
            EditorStep::Operator => title.extend([
                Span::styled(messages::GLOBAL_FILTER_OPERATOR_STEP, heading),
                Span::styled(" · ", muted()),
                field,
            ]),
            _ => title.extend([
                Span::styled(messages::GLOBAL_FILTER_VALUE_STEP, heading),
                Span::styled(" · ", muted()),
                field,
                Span::raw(" "),
                operator,
            ]),
        }
        title.push(Span::raw(" "));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(muted())
            .title(Line::from(title));
        let content = block.inner(card);
        frame.render_widget(block, card);
        if self.step != EditorStep::Operator {
            frame.render_widget(
                Paragraph::new(self.input.inline_line()),
                Rect::new(content.x, content.y, content.width, content.height.min(1)),
            );
        }
        let top = u16::from(self.step != EditorStep::Operator).min(content.height);
        let list = Rect::new(
            content.x,
            content.y + top,
            content.width,
            content.height.saturating_sub(top),
        );
        let items: Vec<Vec<Span<'static>>> = match self.step {
            EditorStep::Field => self
                .fields()
                .iter()
                .map(|f| {
                    vec![
                        Span::styled(f.label(), Style::default().fg(colors::FILTER_FIELD)),
                        Span::styled(format!(" — {}", field_description(*f)), muted()),
                    ]
                })
                .collect(),
            EditorStep::Operator => DashboardFilterOperator::for_kind(self.field.kind())
                .iter()
                .map(|o| {
                    vec![Span::styled(
                        o.label(),
                        Style::default().fg(colors::FILTER_OPERATOR),
                    )]
                })
                .collect(),
            _ => self
                .values(rows)
                .into_iter()
                .map(|v| {
                    if self.field.kind() != DashboardFilterKind::Enum {
                        return value_spans(&[v], 0);
                    }
                    let count = rows
                        .iter()
                        .filter(|r| {
                            field_values(self.field, r)
                                .iter()
                                .any(|a| a.eq_ignore_ascii_case(&v))
                        })
                        .count();
                    let checkbox = if self.selected_values.contains(&v) {
                        Span::styled("☒", Style::default().fg(colors::SUCCESS))
                    } else {
                        Span::styled("☐", muted())
                    };
                    [
                        vec![checkbox, Span::raw(" ")],
                        value_spans(&[v], 0),
                        vec![Span::styled(format!(" ({count})"), muted())],
                    ]
                    .concat()
                })
                .collect(),
        };
        self.choice = self.choice.min(items.len().saturating_sub(1));
        let offset = self
            .choice
            .saturating_sub(list.height.saturating_sub(1) as usize);
        for (index, item) in items
            .into_iter()
            .enumerate()
            .skip(offset)
            .take(list.height as usize)
        {
            let rect = Rect::new(list.x, list.y + (index - offset) as u16, list.width, 1);
            let selected = index == self.choice;
            let mut paragraph = Paragraph::new(Line::from(selectable_spans(item, selected)));
            if selected {
                paragraph = paragraph.style(Style::default().bg(colors::BG_SELECTED));
            }
            frame.render_widget(paragraph, rect);
            self.hits.push((rect, EditorHit::Choice(index)));
        }
        let validation = self.error.or_else(|| {
            (self.step == EditorStep::Value
                && self.field.kind() == DashboardFilterKind::Number
                && !self.input.value.is_empty()
                && self.pending_rule().is_none())
            .then_some(messages::GLOBAL_FILTER_INTEGER_ERROR)
        });
        let line = if let Some(error) = validation {
            Line::styled(error, Style::default().fg(colors::ERROR))
        } else if self.step == EditorStep::Value && self.field.kind() == DashboardFilterKind::Enum {
            Line::from(
                [
                    vec![Span::styled(messages::GLOBAL_FILTER_SELECTED, muted())],
                    value_spans(&self.selected_values, 0),
                ]
                .concat(),
            )
        } else {
            Line::default()
        };
        frame.render_widget(Paragraph::new(line), status);
    }
}

fn muted() -> Style {
    Style::default().fg(colors::MUTED)
}

fn keyword() -> Style {
    Style::default()
        .fg(colors::FILTER_KEYWORD)
        .add_modifier(Modifier::BOLD)
}

/// `Field operator`, without the values.
fn rule_prefix(rule: &DashboardFilterRule) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            rule.field.label(),
            Style::default().fg(colors::FILTER_FIELD),
        ),
        Span::raw(" "),
        Span::styled(
            rule.operator.label(),
            Style::default().fg(colors::FILTER_OPERATOR),
        ),
    ]
}

/// `a, b, +N more` with the values highlighted and the separators muted.
fn value_spans(values: &[String], hidden: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for value in values {
        if !spans.is_empty() {
            spans.push(Span::styled(", ", muted()));
        }
        spans.push(Span::styled(
            value.clone(),
            Style::default().fg(colors::FILTER_VALUE),
        ));
    }
    if hidden > 0 {
        if !spans.is_empty() {
            spans.push(Span::styled(", ", muted()));
        }
        spans.push(Span::styled(format!("+{hidden} more"), muted()));
    }
    spans
}

fn or_line() -> Line<'static> {
    Line::from(vec![
        Span::styled("── ", muted()),
        Span::styled("OR", keyword()),
        Span::styled(" ──", muted()),
    ])
}

/// `Showing 17 of 37 worktrees · 20 hidden`, each count colored by meaning.
pub(super) fn summary_line(visible: usize, total: usize) -> Line<'static> {
    let [showing, of, worktrees, hidden_label] = messages::GLOBAL_FILTER_SUMMARY;
    let hidden = total.saturating_sub(visible);
    let count = |value: usize, color: Color| {
        Span::styled(
            value.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    };
    let mut spans = vec![
        Span::styled(showing, muted()),
        count(visible, colors::SUCCESS),
        Span::styled(of, muted()),
        count(total, colors::WHITE),
    ];
    spans.extend(branded_line(worktrees, muted()));
    spans.push(count(
        hidden,
        if hidden == 0 {
            colors::MUTED
        } else {
            colors::ACCENT
        },
    ));
    spans.push(Span::styled(hidden_label, muted()));
    Line::from(spans)
}

/// Keyboard hints styled like the other command footers: key in the brand
/// accent (error pink for Esc), action muted.
fn hint_line(hints: &[(&'static str, &'static str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, action)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ·  ", muted()));
        }
        let color = if *key == "Esc" {
            colors::ERROR
        } else {
            colors::BRAND
        };
        spans.push(Span::styled(format!("{key} "), Style::default().fg(color)));
        spans.push(Span::styled(*action, muted()));
    }
    Line::from(spans)
}

/// Prefixes the selection marker and, when selected, paints the row bar
/// behind the spans while keeping their syntax colors.
fn selectable_spans(spans: Vec<Span<'static>>, selected: bool) -> Vec<Span<'static>> {
    if !selected {
        return [vec![Span::raw("   ")], spans, vec![Span::raw(" ")]].concat();
    }
    let bar = Style::default()
        .bg(colors::BG_SELECTED)
        .add_modifier(Modifier::BOLD);
    [
        vec![Span::styled(" ➤ ", bar.fg(colors::WHITE))],
        spans,
        vec![Span::raw(" ")],
    ]
    .concat()
    .into_iter()
    .map(|span| {
        let style = bar.patch(span.style);
        span.style(style)
    })
    .collect()
}

/// Truncates styled spans to `max` columns, ending with `…` when cut.
fn fit_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    if Line::from(spans.clone()).width() <= max {
        return spans;
    }
    let mut fitted = Vec::new();
    let mut used = 0;
    for span in spans {
        let mut text = String::new();
        for c in span.content.chars() {
            let width = Span::raw(c.to_string()).width();
            if used + width + 1 > max {
                text.push('…');
                fitted.push(Span::styled(text, span.style));
                return fitted;
            }
            text.push(c);
            used += width;
        }
        fitted.push(Span::styled(text, span.style));
    }
    fitted
}

/// Bordered button matching the dashboard's; focus fills the label row.
pub(super) fn render_button(
    frame: &mut ratatui::Frame,
    area: Rect,
    label: &str,
    color: Color,
    focused: bool,
) {
    let style = if focused {
        Style::default()
            .fg(colors::BG)
            .bg(color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    };
    frame.render_widget(
        Paragraph::new(Line::styled(format!(" {label} "), style))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(color)),
            ),
        area,
    );
}

pub(super) fn button_width(label: &str) -> u16 {
    Line::from(label).width() as u16 + 4
}

/// `(x, row, width)` for each button: left to right, wrapping when the next
/// one does not fit, with the last `trailing` ones aligned to the right edge.
fn button_grid(width: u16, labels: &[&str], trailing: usize) -> Vec<(u16, u16, u16)> {
    let split = labels.len().saturating_sub(trailing);
    let widths: Vec<u16> = labels.iter().map(|label| button_width(label)).collect();
    let mut grid = Vec::new();
    let (mut x, mut row) = (0u16, 0u16);
    for &button in &widths[..split] {
        if x > 0 && x + button > width {
            (x, row) = (0, row + 1);
        }
        grid.push((x, row, button));
        x += button + 1;
    }
    let cluster =
        widths[split..].iter().sum::<u16>() + (widths.len() - split).saturating_sub(1) as u16;
    if x > 0 && x + 1 + cluster > width {
        row += 1;
    }
    let mut x = width.saturating_sub(cluster);
    for &button in &widths[split..] {
        grid.push((x, row, button));
        x += button + 1;
    }
    grid
}

fn field_description(field: DashboardFilterField) -> &'static str {
    crate::messages::GLOBAL_FILTER_FIELD_DESCRIPTIONS[DashboardFilterField::ALL
        .iter()
        .position(|candidate| *candidate == field)
        .unwrap_or(0)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::DashboardFilterGroup;
    use crate::git::{BranchStatus, GitWorktree};
    use crate::services::{CommitSummary, DashboardRepository};

    fn row() -> DashboardRow {
        DashboardRow {
            repository: Some(DashboardRepository {
                owner: "Alice".into(),
                name: "Project".into(),
                local_root: Some("/repo".into()),
            }),
            worktree: GitWorktree {
                path: "/repo/feature".into(),
                branch: "Feature/HELLO".into(),
                is_clean: true,
                branch_status: Some(BranchStatus {
                    ahead: 5,
                    behind: 2,
                    insertions: Some(10),
                    deletions: Some(3),
                    source_branch: Some("main".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            last_commit: Some(CommitSummary {
                sha: "ABC123".into(),
                summary: "Fix parser".into(),
                author: "Alice".into(),
                relative_time: "now".into(),
            }),
            pull_request: None,
            error: None,
        }
    }

    fn rule(
        field: DashboardFilterField,
        operator: DashboardFilterOperator,
        values: &[&str],
    ) -> DashboardFilterRule {
        DashboardFilterRule {
            field,
            operator,
            values: values.iter().map(|s| (*s).into()).collect(),
        }
    }

    #[test]
    fn groups_are_or_rules_are_and_and_values_are_any_of() {
        use DashboardFilterField as F;
        use DashboardFilterOperator as O;
        let row = row();
        let mut filter = DashboardFilter {
            groups: vec![DashboardFilterGroup {
                rules: vec![
                    rule(F::Profile, O::IsAnyOf, &["Bob", "Alice"]),
                    rule(F::Branch, O::Contains, &["absent", "hello"]),
                ],
            }],
        };
        assert!(filter.matches(&row));
        filter.groups[0]
            .rules
            .push(rule(F::Status, O::IsAnyOf, &["Dirty"]));
        assert!(!filter.matches(&row));
        filter.groups.push(DashboardFilterGroup {
            rules: vec![rule(F::Repository, O::IsAnyOf, &["Project"])],
        });
        assert!(filter.matches(&row));
    }

    #[test]
    fn text_is_case_insensitive_and_negation_excludes_any_matching_value() {
        use DashboardFilterField as F;
        use DashboardFilterOperator as O;
        let row = row();
        assert!(rule(F::Branch, O::Is, &["feature/hello"]).matches(&row));
        assert!(rule(F::Branch, O::Contains, &["HELLO"]).matches(&row));
        assert!(!rule(F::Branch, O::DoesNotContain, &["absent", "hello"]).matches(&row));
        assert!(!rule(F::Branch, O::IsNot, &["FEATURE/hello"]).matches(&row));
        assert!(rule(F::Branch, O::IsNot, &["other"]).matches(&row));
        assert!(!rule(F::Profile, O::IsNoneOf, &["Alice"]).matches(&row));
        for value in ["abc123", "FIX PARSER", "alice"] {
            assert!(rule(F::LastCommit, O::Is, &[value]).matches(&row));
        }
    }

    #[test]
    fn every_numeric_operator_compares_exact_integers() {
        use DashboardFilterField as F;
        use DashboardFilterOperator as O;
        let row = row();
        for (op, yes, no) in [
            (O::Equal, "5", "4"),
            (O::NotEqual, "4", "5"),
            (O::GreaterThan, "4", "5"),
            (O::GreaterThanOrEqual, "5", "6"),
            (O::LessThan, "6", "5"),
            (O::LessThanOrEqual, "5", "4"),
        ] {
            assert!(rule(F::Ahead, op, &[yes]).matches(&row), "{op:?}");
            assert!(!rule(F::Ahead, op, &[no]).matches(&row), "{op:?}");
        }
        for (field, value) in [(F::Behind, "2"), (F::Insertions, "10"), (F::Deletions, "3")] {
            assert!(rule(field, O::Equal, &[value]).matches(&row));
        }
    }

    #[test]
    fn missing_data_matches_none_and_negation_but_not_positive_comparisons() {
        use DashboardFilterField as F;
        use DashboardFilterOperator as O;
        let mut row = row();
        row.repository.as_mut().unwrap().local_root = None;
        for field in [
            F::Ahead,
            F::Behind,
            F::Insertions,
            F::Deletions,
            F::PrNumber,
        ] {
            for op in [
                O::Equal,
                O::GreaterThan,
                O::GreaterThanOrEqual,
                O::LessThan,
                O::LessThanOrEqual,
            ] {
                assert!(!rule(field, op, &["0"]).matches(&row));
            }
            assert!(rule(field, O::NotEqual, &["0"]).matches(&row));
        }
        for field in [F::From, F::LastCommit, F::PrTitle] {
            assert!(!rule(field, O::Contains, &[""]).matches(&row));
            assert!(!rule(field, O::Is, &[""]).matches(&row));
            assert!(rule(field, O::DoesNotContain, &[""]).matches(&row));
            assert!(rule(field, O::IsNot, &[""]).matches(&row));
        }
        for field in [F::Checks, F::Review, F::Mergeability] {
            assert!(rule(field, O::IsAnyOf, &["None"]).matches(&row));
            assert!(!rule(field, O::IsNoneOf, &["None"]).matches(&row));
        }
        assert!(rule(F::Location, O::IsAnyOf, &["Not cloned"]).matches(&row));
    }

    #[test]
    fn enum_catalogue_values_match_pr_data() {
        use crate::services::{CheckStatus, MergeStatus, ReviewStatus};
        use DashboardFilterField as F;
        use DashboardFilterOperator as O;
        let mut row = row();
        row.pull_request = Some(
            serde_json::from_value(serde_json::json!({
                "number": 1, "state": "Open", "title": "Test", "url": "url"
            }))
            .unwrap(),
        );
        for value in [
            CheckStatus::Passed,
            CheckStatus::Failed,
            CheckStatus::Running,
            CheckStatus::Pending,
            CheckStatus::Errored,
        ] {
            row.pull_request.as_mut().unwrap().checks_status = Some(value);
            let label = format!("{value:?}");
            assert!(F::Checks.enum_values().contains(&label.as_str()));
            assert!(rule(F::Checks, O::IsAnyOf, &[&label]).matches(&row));
        }
        for value in [
            ReviewStatus::Approved,
            ReviewStatus::Rejected,
            ReviewStatus::Pending,
        ] {
            row.pull_request.as_mut().unwrap().review_status = Some(value);
            let label = format!("{value:?}");
            assert!(rule(F::Review, O::IsAnyOf, &[&label]).matches(&row));
        }
        for value in [
            MergeStatus::Draft,
            MergeStatus::Dirty,
            MergeStatus::Blocked,
            MergeStatus::Unknown,
            MergeStatus::Behind,
            MergeStatus::HasHooks,
            MergeStatus::Unstable,
            MergeStatus::Clean,
        ] {
            row.pull_request.as_mut().unwrap().merge_status = Some(value);
            let label = format!("{value:?}");
            assert!(F::Mergeability.enum_values().contains(&label.as_str()));
            assert!(rule(F::Mergeability, O::IsAnyOf, &[&label]).matches(&row));
        }
    }

    #[test]
    fn mother_default_and_empty_filter() {
        let mut row = row();
        assert!(DashboardFilter::default().matches(&row));
        row.worktree.is_main = true;
        assert!(!DashboardFilter::default().matches(&row));
        assert!(DashboardFilter { groups: vec![] }.matches(&row));
    }

    #[test]
    fn catalogue_and_status_share_display_labels() {
        use crate::services::{PrState, PullRequest};
        use DashboardFilterField as F;
        use DashboardFilterOperator as O;
        let mut row = row();
        for (state, label) in [
            (PrState::Open, "Opened"),
            (PrState::Draft, "Drafted"),
            (PrState::Closed, "Closed"),
            (PrState::Merged, "Merged"),
        ] {
            // Reuse the wire defaults for the PR's optional metadata.
            row.pull_request = Some(serde_json::from_value::<PullRequest>(serde_json::json!({
                "number": 42, "state": state, "title": "Title", "url": "url", "headRefName": "feature"
            })).unwrap());
            assert!(rule(F::Status, O::IsAnyOf, &[label]).matches(&row));
            assert!(rule(F::PrNumber, O::Equal, &["42"]).matches(&row));
            assert!(rule(F::PrTitle, O::Contains, &["title"]).matches(&row));
        }
        assert_eq!(F::ALL.len(), 17);
        assert_eq!(F::Profile.choices(&[row]), vec!["Alice"]);
    }
}
