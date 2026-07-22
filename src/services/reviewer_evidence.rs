//! Bounded, language-light evidence extraction for large changed files.
//!
//! This is intentionally not a parser framework. It recognizes common
//! declaration boundaries, keeps complete enclosing symbols when syntax is
//! clear, and explicitly asks the reviewer to read the real file otherwise.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

const SYMBOL_EVIDENCE_MAX_BYTES: usize = 24 * 1024;
const REFERENCE_CONTEXT_LINES: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtractedReviewEvidence {
    pub rendered: String,
    pub complete: bool,
    pub symbols: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Symbol {
    name: String,
    start: usize,
    end: usize,
}

pub(crate) fn extract_symbol_evidence(
    path: &str,
    source: &str,
    annotated_diff: &str,
) -> ExtractedReviewEvidence {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !supported_extension(&extension) {
        return fallback("language has no proven lightweight extractor");
    }
    let lines = source.lines().collect::<Vec<_>>();
    let changed = changed_lines(annotated_diff);
    if changed.is_empty() || lines.is_empty() {
        return fallback("changed lines could not be mapped to current source");
    }

    let (declarations, syntax_complete) = declarations(&lines, &extension);
    let mut selected = BTreeSet::new();
    for line in &changed {
        let zero_based = line.saturating_sub(1) as usize;
        let enclosing = declarations
            .iter()
            .filter(|symbol| symbol.start <= zero_based && zero_based <= symbol.end)
            .min_by_key(|symbol| symbol.end.saturating_sub(symbol.start));
        if let Some(symbol) = enclosing {
            selected.insert((symbol.start, symbol.end, symbol.name.clone()));
        }
    }
    if selected.is_empty() {
        return module_window(&lines, &changed, syntax_complete);
    }

    let selected_symbols = selected
        .iter()
        .map(|(_, _, name)| name.clone())
        .collect::<Vec<_>>();
    let mut rendered = String::from("### ENCLOSING SYMBOL EVIDENCE\n");
    let mut referenced = HashSet::new();
    for (start, end, name) in &selected {
        rendered.push_str(&format!(
            "\n#### SYMBOL: {name} (lines {}-{})\n",
            start + 1,
            end + 1
        ));
        for (index, line) in lines[*start..=*end].iter().enumerate() {
            let number = start + index + 1;
            let marker = if changed.contains(&(number as u64)) {
                '+'
            } else {
                ' '
            };
            push_bounded(&mut rendered, &format!("{number:>6} {marker}{line}\n"));
            referenced.extend(reference_tokens(line));
        }
    }

    let selected_names = selected_symbols.iter().cloned().collect::<HashSet<_>>();
    let mut related = declarations
        .iter()
        .filter(|symbol| {
            (referenced.contains(&symbol.name)
                || selected_names.iter().any(|selected| {
                    lines[symbol.start..=symbol.end]
                        .iter()
                        .any(|line| line.contains(selected))
                }))
                && !selected_names.contains(&symbol.name)
        })
        .collect::<Vec<_>>();
    related.sort_by_key(|symbol| symbol.start);
    related.truncate(12);
    if !related.is_empty() {
        rendered.push_str("\n### DIRECT LOCAL REFERENCES\n");
        for symbol in related {
            let start = symbol.start.saturating_sub(REFERENCE_CONTEXT_LINES);
            let end = (symbol.start + REFERENCE_CONTEXT_LINES).min(lines.len().saturating_sub(1));
            rendered.push_str(&format!(
                "\n#### {} near line {}\n",
                symbol.name,
                symbol.start + 1
            ));
            for (index, line) in lines[start..=end].iter().enumerate() {
                push_bounded(
                    &mut rendered,
                    &format!("{:>6}  {line}\n", start + index + 1),
                );
            }
        }
    }

    let complete = syntax_complete
        && changed.iter().all(|line| {
            selected
                .iter()
                .any(|(start, end, _)| *start < *line as usize && *line as usize <= end + 1)
        });
    if !complete {
        push_bounded(
            &mut rendered,
            "\nEVIDENCE-FALLBACK: extraction was partial; read the real full file before completing this file's discovery judgment.\n",
        );
    }
    ExtractedReviewEvidence {
        rendered: rendered.trim_end().to_string(),
        complete,
        symbols: selected_symbols,
    }
}

fn supported_extension(extension: &str) -> bool {
    matches!(
        extension,
        "rs" | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "py"
            | "rb"
            | "go"
            | "java"
            | "kt"
            | "swift"
            | "cs"
            | "c"
            | "h"
            | "cpp"
            | "cc"
            | "php"
    )
}

fn changed_lines(annotated_diff: &str) -> BTreeSet<u64> {
    annotated_diff
        .lines()
        .filter(|line| line.as_bytes().get(7) == Some(&b'+'))
        .filter_map(|line| line.trim_start().split_once(' ')?.0.parse().ok())
        .collect()
}

fn declarations(lines: &[&str], extension: &str) -> (Vec<Symbol>, bool) {
    let mut symbols = Vec::new();
    let mut complete = true;
    for (index, line) in lines.iter().enumerate() {
        let Some(name) = declaration_name(line) else {
            continue;
        };
        let start = attribute_start(lines, index);
        let (end, closed) = if line.trim_end().ends_with(';') && !line.contains('{') {
            (index, true)
        } else if matches!(extension, "py") {
            indentation_end(lines, index)
        } else if matches!(extension, "rb") {
            ruby_end(lines, index)
        } else {
            brace_end(lines, index)
        };
        complete &= closed;
        symbols.push(Symbol { name, start, end });
    }
    (symbols, complete)
}

fn declaration_name(line: &str) -> Option<String> {
    let mut line = line.trim_start();
    loop {
        let next = line
            .strip_prefix("pub(crate) ")
            .or_else(|| line.strip_prefix("pub(super) "))
            .or_else(|| line.strip_prefix("pub "))
            .or_else(|| line.strip_prefix("private "))
            .or_else(|| line.strip_prefix("protected "))
            .or_else(|| line.strip_prefix("public "))
            .or_else(|| line.strip_prefix("unsafe "));
        let Some(next) = next else { break };
        line = next;
    }
    let prefixes = [
        "async fn ",
        "pub fn ",
        "fn ",
        "async def ",
        "def ",
        "class ",
        "struct ",
        "enum ",
        "trait ",
        "interface ",
        "impl ",
        "type ",
        "const ",
        "static ",
        "function ",
        "func ",
        "module ",
        "mod ",
        "record ",
    ];
    for prefix in prefixes {
        if let Some(rest) = line.strip_prefix(prefix) {
            let name = rest
                .trim_start_matches('<')
                .split(|character: char| {
                    !(character.is_alphanumeric() || character == '_' || character == ':')
                })
                .find(|part| !part.is_empty())?;
            return Some(name.to_string());
        }
    }
    if let Some((left, _)) = line.split_once("=>") {
        let name = left
            .split(|character: char| !character.is_alphanumeric() && character != '_')
            .rfind(|part| !part.is_empty())?;
        return Some(name.to_string());
    }
    None
}

fn attribute_start(lines: &[&str], declaration: usize) -> usize {
    let mut start = declaration;
    while start > 0 {
        let previous = lines[start - 1].trim_start();
        if previous.starts_with("#[") || previous.starts_with('@') || previous.starts_with("///") {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

fn brace_end(lines: &[&str], start: usize) -> (usize, bool) {
    let mut balance = 0i64;
    let mut opened = false;
    for (index, line) in lines.iter().enumerate().skip(start) {
        for character in line.chars() {
            if character == '{' {
                balance += 1;
                opened = true;
            } else if character == '}' && opened {
                balance -= 1;
            }
        }
        if opened && balance <= 0 {
            return (index, true);
        }
        if !opened && index > start + 3 {
            return (start, false);
        }
    }
    (lines.len().saturating_sub(1), false)
}

fn indentation_end(lines: &[&str], start: usize) -> (usize, bool) {
    let indent = lines[start].len() - lines[start].trim_start().len();
    for (index, line) in lines.iter().enumerate().skip(start + 1) {
        if line.trim().is_empty() {
            continue;
        }
        let next_indent = line.len() - line.trim_start().len();
        if next_indent <= indent && !line.trim_start().starts_with('@') {
            return (index - 1, true);
        }
    }
    (lines.len().saturating_sub(1), true)
}

fn ruby_end(lines: &[&str], start: usize) -> (usize, bool) {
    let mut depth = 0i64;
    for (index, line) in lines.iter().enumerate().skip(start) {
        let trimmed = line.trim_start();
        if declaration_name(trimmed).is_some()
            || trimmed.starts_with("do ")
            || trimmed.ends_with(" do")
        {
            depth += 1;
        }
        if trimmed == "end" {
            depth -= 1;
            if depth <= 0 {
                return (index, true);
            }
        }
    }
    (lines.len().saturating_sub(1), false)
}

fn module_window(
    lines: &[&str],
    changed: &BTreeSet<u64>,
    _syntax_complete: bool,
) -> ExtractedReviewEvidence {
    let first = changed.iter().next().copied().unwrap_or(1) as usize;
    let last = changed.iter().next_back().copied().unwrap_or(first as u64) as usize;
    let start = first.saturating_sub(21);
    let end = (last + 20).min(lines.len());
    let mut rendered =
        String::from("### BOUNDED MODULE EVIDENCE (change is between recognized symbols)\n");
    for (index, line) in lines[start..end].iter().enumerate() {
        let number = start + index + 1;
        let marker = if changed.contains(&(number as u64)) {
            '+'
        } else {
            ' '
        };
        push_bounded(&mut rendered, &format!("{number:>6} {marker}{line}\n"));
    }
    push_bounded(
        &mut rendered,
        "\nEVIDENCE-FALLBACK: no unambiguous enclosing symbol; read the real full file before completing this file's discovery judgment.\n",
    );
    ExtractedReviewEvidence {
        rendered: rendered.trim_end().to_string(),
        complete: false,
        symbols: Vec::new(),
    }
}

fn reference_tokens(line: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    let words = line
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|word| word.len() > 2);
    for word in words {
        if word.chars().next().is_some_and(char::is_uppercase)
            || word
                .chars()
                .all(|character| character.is_uppercase() || character == '_')
            || line.contains(&format!("{word}("))
        {
            tokens.insert(word.to_string());
        }
    }
    tokens
}

fn fallback(reason: &str) -> ExtractedReviewEvidence {
    ExtractedReviewEvidence {
        rendered: format!(
            "EVIDENCE-FALLBACK: {reason}; read the real full file before completing this file's discovery judgment."
        ),
        complete: false,
        symbols: Vec::new(),
    }
}

fn push_bounded(rendered: &mut String, value: &str) {
    if rendered.len() >= SYMBOL_EVIDENCE_MAX_BYTES {
        return;
    }
    let remaining = SYMBOL_EVIDENCE_MAX_BYTES - rendered.len();
    if value.len() <= remaining {
        rendered.push_str(value);
    } else {
        let mut end = remaining;
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        rendered.push_str(&value[..end]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_nested_symbol_once_for_multiple_hunks_and_keeps_attributes() {
        let source = "#[test]\nasync fn outer() {\n    fn inner() {\n        risky();\n        risky_again();\n    }\n    inner();\n}\n";
        let diff = "@@ -1,8 +1,8 @@\n     1  #[test]\n     2  async fn outer() {\n     3      fn inner() {\n     4 +        risky();\n@@ -5,4 +5,4 @@\n     5 +        risky_again();";
        let evidence = extract_symbol_evidence("tests/a.rs", source, diff);
        assert!(evidence.complete);
        assert_eq!(evidence.symbols, vec!["inner"]);
        assert_eq!(evidence.rendered.matches("#### SYMBOL: inner").count(), 1);
    }

    #[test]
    fn supports_python_decorators_and_overloads_without_merging_symbols() {
        let source = "@route('/a')\ndef load(value):\n    return value\n\n@route('/b')\ndef load_many(values):\n    return list(values)\n";
        let diff = "@@ -5,3 +5,3 @@\n     5  @route('/b')\n     6  def load_many(values):\n     7 +    return list(values)";
        let evidence = extract_symbol_evidence("api.py", source, diff);
        assert_eq!(evidence.symbols, vec!["load_many"]);
        assert!(evidence.rendered.contains("@route('/b')"));
        assert!(!evidence.rendered.contains("@route('/a')"));
    }

    #[test]
    fn partial_syntax_and_unsupported_languages_require_full_file_read() {
        let partial = extract_symbol_evidence(
            "broken.rs",
            "fn broken() {\n    call();\n",
            "@@ -1,2 +1,2 @@\n     1  fn broken() {\n     2 +    call();",
        );
        assert!(!partial.complete);
        assert!(partial.rendered.contains("read the real full file"));
        let template = extract_symbol_evidence("view.hbs", "{{thing}}", "     1 +{{thing}}");
        assert!(!template.complete);
        assert!(template
            .rendered
            .contains("no proven lightweight extractor"));
    }

    #[test]
    fn change_between_symbols_gets_module_window_not_wrong_symbol() {
        let source = "fn first() {}\n\nregister_routes();\n\nfn second() {}\n";
        let evidence = extract_symbol_evidence(
            "lib.rs",
            source,
            "@@ -1,5 +1,5 @@\n     3 +register_routes();",
        );
        assert!(evidence.symbols.is_empty());
        assert!(evidence.rendered.contains("BOUNDED MODULE EVIDENCE"));
    }
}
