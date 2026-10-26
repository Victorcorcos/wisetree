//! Global Dashboard filter evaluation. The editor and dashboard share this model.

use ratatui::text::Line;

use crate::config::schema::{
    DashboardFilter, DashboardFilterField, DashboardFilterKind, DashboardFilterOperator,
    DashboardFilterRule,
};
use crate::services::DashboardRow;

use super::dashboard::{dashboard_worktree_name, remote_only, status_label_and_style};

impl DashboardFilter {
    pub fn rule_count(&self) -> usize {
        self.groups.iter().map(|group| group.rules.len()).sum()
    }

    /// Compact rule descriptions shared with the dashboard's hover preview.
    pub fn tooltip_lines(&self, width: usize) -> Vec<String> {
        if self.rule_count() == 0 {
            return vec![crate::messages::GLOBAL_FILTER_NONE.to_string()];
        }
        let mut lines = Vec::new();
        for (group_index, group) in self.groups.iter().enumerate() {
            if group_index > 0 {
                lines.push("── OR ──".into());
            }
            for (rule_index, rule) in group.rules.iter().enumerate() {
                if rule_index > 0 {
                    lines.push("AND".into());
                }
                let prefix = format!("{}  {}  ", rule.field.label(), rule.operator.label());
                let mut values = Vec::new();
                for (index, value) in rule.values.iter().enumerate() {
                    let remaining = rule.values.len() - index - 1;
                    let suffix = if remaining == 0 {
                        String::new()
                    } else {
                        format!(", +{remaining} more")
                    };
                    let candidate = format!(
                        "{prefix}{}{value}{suffix}",
                        if values.is_empty() {
                            String::new()
                        } else {
                            format!("{}, ", values.join(", "))
                        }
                    );
                    if Line::from(candidate).width() > width {
                        break;
                    }
                    values.push(value.as_str());
                }
                let remaining = rule.values.len() - values.len();
                let mut text = values.join(", ");
                if remaining > 0 {
                    if !text.is_empty() {
                        text.push_str(", ");
                    }
                    text.push_str(&format!("+{remaining} more"));
                }
                if Line::from(format!("{prefix}{text}")).width() > width && remaining > 0 {
                    lines.push(prefix.trim_end().to_string());
                    lines.push(text);
                } else {
                    lines.push(format!("{prefix}{text}"));
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
            match key.code {
                KeyCode::Up | KeyCode::BackTab => {
                    self.focus = (self.focus + count + 5) % (count + 6)
                }
                KeyCode::Down => self.focus = (self.focus + 1) % (count + 6),
                KeyCode::Tab => {
                    self.focus = if self.focus < count {
                        count
                    } else {
                        (self.focus + 1) % (count + 6)
                    }
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

    pub fn render(
        &mut self,
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        rows: &[DashboardRow],
    ) {
        use crate::messages::{self, colors};
        use ratatui::{
            layout::{Constraint, Layout, Rect},
            style::{Modifier, Style},
            widgets::{Block, Borders, Paragraph},
        };
        self.hits.clear();
        let mut preview = self.draft.clone();
        if self.step == EditorStep::Value {
            if let Some(rule) = self.pending_rule() {
                Self::put_rule(&mut preview, self.target, rule);
            }
        }
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(6),
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
            Paragraph::new(messages::global_filter_summary(
                rows.iter().filter(|r| preview.matches(r)).count(),
                rows.len(),
            )),
            chunks[1],
        );
        let body = chunks[2];
        if self.step == EditorStep::Overview {
            // Scroll whole card lines together; focus remains visible for arbitrarily many rules.
            let mut lines = Vec::new();
            let mut rule_lines = Vec::new();
            let mut index = 0;
            for (g, group) in self.draft.groups.iter().enumerate() {
                if g > 0 {
                    lines.push(Line::from("                OR"));
                }
                let width = body.width.saturating_sub(2) as usize;
                lines.push(Line::from(format!(
                    "┌─ {} {}┐",
                    messages::GLOBAL_FILTER_ALL,
                    "─".repeat(width.saturating_sub(messages::GLOBAL_FILTER_ALL.len() + 3))
                )));
                for rule in &group.rules {
                    rule_lines.push((index, lines.len()));
                    let style = if self.focus == index {
                        Style::default()
                            .fg(colors::BRAND)
                            .add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                    let text = format!(
                        "{}  {}  {}",
                        rule.field.label(),
                        rule.operator.label(),
                        rule.values.join(", ")
                    );
                    let available = width.saturating_sub(2);
                    let text = if Line::from(text.clone()).width() > available {
                        let mut shortened = String::new();
                        for c in text.chars() {
                            if Line::from(format!("{shortened}{c}…")).width() > available {
                                break;
                            }
                            shortened.push(c);
                        }
                        format!("{shortened}…")
                    } else {
                        text
                    };
                    let padding =
                        " ".repeat(available.saturating_sub(Line::from(text.clone()).width()));
                    lines.push(Line::styled(format!("│ {text}{padding} │"), style));
                    index += 1;
                }
                lines.push(Line::from(format!("└{}┘", "─".repeat(width))));
            }
            if lines.is_empty() {
                lines.push(Line::from(messages::GLOBAL_FILTER_NONE));
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
            for (index, label) in messages::GLOBAL_FILTER_BUTTONS.iter().enumerate() {
                let rect = Rect::new(chunks[3].x, chunks[3].y + index as u16, chunks[3].width, 1)
                    .intersection(chunks[3]);
                let style = if self.focus == self.draft.rule_count() + index {
                    Style::default()
                        .fg(colors::BRAND)
                        .add_modifier(Modifier::REVERSED)
                } else {
                    Style::default().fg(colors::INFO)
                };
                frame.render_widget(Paragraph::new(format!("[ {label} ]")).style(style), rect);
                self.hits
                    .push((rect, EditorHit::Focus(self.draft.rule_count() + index)));
            }
            frame.render_widget(
                Paragraph::new(messages::GLOBAL_FILTER_OVERVIEW_HINT),
                chunks[4],
            );
        } else {
            let title = match self.step {
                EditorStep::Field => messages::GLOBAL_FILTER_FIELD_STEP.to_string(),
                EditorStep::Operator => format!(
                    "{} · {}",
                    messages::GLOBAL_FILTER_OPERATOR_STEP,
                    self.field.label()
                ),
                _ => format!(
                    "{} · {} {}",
                    messages::GLOBAL_FILTER_VALUE_STEP,
                    self.field.label(),
                    self.operator.label()
                ),
            };
            let inner = Block::default().borders(Borders::ALL).title(title);
            let content = inner.inner(body);
            frame.render_widget(inner, body);
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
            let labels: Vec<String> = match self.step {
                EditorStep::Field => self
                    .fields()
                    .iter()
                    .map(|f| format!("{} — {}", f.label(), field_description(*f)))
                    .collect(),
                EditorStep::Operator => DashboardFilterOperator::for_kind(self.field.kind())
                    .iter()
                    .map(|o| o.label().to_string())
                    .collect(),
                _ => self
                    .values(rows)
                    .iter()
                    .map(|v| {
                        if self.field.kind() == DashboardFilterKind::Enum {
                            let count = rows
                                .iter()
                                .filter(|r| {
                                    field_values(self.field, r)
                                        .iter()
                                        .any(|a| a.eq_ignore_ascii_case(v))
                                })
                                .count();
                            format!(
                                "{} {v} ({count})",
                                if self.selected_values.contains(v) {
                                    "☒"
                                } else {
                                    "☐"
                                }
                            )
                        } else {
                            v.clone()
                        }
                    })
                    .collect(),
            };
            self.choice = self.choice.min(labels.len().saturating_sub(1));
            let offset = self
                .choice
                .saturating_sub(list.height.saturating_sub(1) as usize);
            for (index, label) in labels
                .into_iter()
                .enumerate()
                .skip(offset)
                .take(list.height as usize)
            {
                let rect = Rect::new(list.x, list.y + (index - offset) as u16, list.width, 1);
                let style = if index == self.choice {
                    Style::default()
                        .fg(colors::BRAND)
                        .add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                frame.render_widget(Paragraph::new(label).style(style), rect);
                self.hits.push((rect, EditorHit::Choice(index)));
            }
            let validation = self.error.or_else(|| {
                (self.step == EditorStep::Value
                    && self.field.kind() == DashboardFilterKind::Number
                    && !self.input.value.is_empty()
                    && self.pending_rule().is_none())
                .then_some(messages::GLOBAL_FILTER_INTEGER_ERROR)
            });
            let info = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(chunks[3]);
            frame.render_widget(
                Paragraph::new(validation.unwrap_or("")).style(Style::default().fg(colors::ERROR)),
                info[0],
            );
            frame.render_widget(Paragraph::new(messages::GLOBAL_FILTER_VALUE_HINT), info[1]);
            frame.render_widget(Paragraph::new(messages::GLOBAL_FILTER_NEXT), info[2]);
            if self.step == EditorStep::Value && self.field.kind() == DashboardFilterKind::Enum {
                frame.render_widget(
                    Paragraph::new(format!(
                        "{}{}",
                        messages::GLOBAL_FILTER_SELECTED,
                        self.selected_values.join(", ")
                    )),
                    info[3],
                );
            }
            self.hits.push((
                Rect::new(info[2].x, info[2].y, 15.min(info[2].width), info[2].height),
                EditorHit::Confirm,
            ));
            self.hits.push((
                Rect::new(
                    info[2].x.saturating_add(16),
                    info[2].y,
                    info[2].width.saturating_sub(16),
                    info[2].height,
                ),
                EditorHit::Back,
            ));
        }
    }
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
