//! Shell-integration service. Mirrors upstream `shell-integration-service.ts`
//! field-for-field — same signature/end-marker pattern, same fallback logic
//! (50-line lookahead for a closing `}` when the end marker is absent), same
//! completion bodies (with the `_branchlet`/`_wisetree` function-name swap).

use std::fs;
use std::path::{Path, PathBuf};

fn home_dir() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return Some(PathBuf::from(home));
        }
    }
    dirs::home_dir()
}

const WRAPPER_SIGNATURE: &str = "# Wisetree setup: added on";
const SETUP_END_MARKER: &str = "# End Wisetree setup";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    Bash,
    Unknown,
}

impl Shell {
    pub fn as_str(self) -> &'static str {
        match self {
            Shell::Zsh => "zsh",
            Shell::Bash => "bash",
            Shell::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellIntegrationStatus {
    pub is_installed: bool,
    pub shell: Shell,
    pub config_path: Option<PathBuf>,
    pub reason: Option<String>,
}

/// Reads `$SHELL` and returns the matching shell variant.
pub fn detect_shell() -> Shell {
    let shell = std::env::var("SHELL").unwrap_or_default().to_lowercase();
    if shell.contains("zsh") {
        Shell::Zsh
    } else if shell.contains("bash") {
        Shell::Bash
    } else {
        Shell::Unknown
    }
}

pub fn get_config_path(shell: Shell) -> Option<PathBuf> {
    get_config_paths(shell)?.into_iter().next()
}

fn get_config_paths(shell: Shell) -> Option<Vec<PathBuf>> {
    let home = home_dir()?;
    match shell {
        Shell::Zsh => Some(vec![home.join(".zshrc")]),
        Shell::Bash => {
            let mut paths = vec![home.join(bash_config_name())];
            if cfg!(target_os = "macos") {
                paths.push(home.join(".bashrc"));
            }
            Some(paths)
        }
        Shell::Unknown => None,
    }
}

fn bash_config_name() -> &'static str {
    if cfg!(target_os = "macos") {
        ".bash_profile"
    } else {
        ".bashrc"
    }
}

pub fn detect_shell_integration() -> ShellIntegrationStatus {
    detect_shell_integration_with(detect_shell())
}

pub fn detect_shell_integration_with(shell: Shell) -> ShellIntegrationStatus {
    let config_paths = match get_config_paths(shell) {
        Some(paths) => paths,
        None => {
            return ShellIntegrationStatus {
                is_installed: false,
                shell,
                config_path: None,
                reason: Some("Could not determine shell config file".into()),
            };
        }
    };
    let config_path = config_paths[0].clone();
    let mut first_read_error: Option<(PathBuf, String)> = None;
    let mut legacy_path_with_setup: Option<PathBuf> = None;

    for (idx, path) in config_paths.iter().enumerate() {
        if !path.exists() {
            continue;
        }
        match fs::read_to_string(path) {
            Ok(content) => {
                if !content.contains(WRAPPER_SIGNATURE) {
                    continue;
                }
                if idx == 0 {
                    return ShellIntegrationStatus {
                        is_installed: true,
                        shell,
                        config_path: Some(path.clone()),
                        reason: None,
                    };
                }
                legacy_path_with_setup = Some(path.clone());
            }
            Err(e) => {
                if first_read_error.is_none() {
                    first_read_error = Some((path.clone(), e.to_string()));
                }
            }
        }
    }

    if let Some(legacy_path) = legacy_path_with_setup {
        return ShellIntegrationStatus {
            is_installed: false,
            shell,
            config_path: Some(config_path.clone()),
            reason: Some(format!(
                "Legacy shell integration found in {}. Reinstall to move it to {}.",
                legacy_path.display(),
                config_path.display()
            )),
        };
    }

    if let Some((path, err)) = first_read_error {
        return ShellIntegrationStatus {
            is_installed: false,
            shell,
            config_path: Some(path),
            reason: Some(format!("Failed to read config: {err}")),
        };
    }

    if !config_path.exists() {
        return ShellIntegrationStatus {
            is_installed: false,
            shell,
            config_path: Some(config_path),
            reason: Some("Config file does not exist".into()),
        };
    }
    ShellIntegrationStatus {
        is_installed: false,
        shell,
        config_path: Some(config_path),
        reason: Some("Shell integration not found in config".into()),
    }
}

/// Install the integration. Replaces any existing block (matched by signature).
/// `command_name` is the binary name to wrap; in production this is `wisetree`.
pub fn install_shell_integration(shell: Shell, command_name: &str) -> std::io::Result<()> {
    let path = get_config_path(shell).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not determine shell config path",
        )
    })?;
    let config_paths = get_config_paths(shell).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not determine shell config paths",
        )
    })?;

    let block = generate_setup_block(shell, command_name, today_iso());

    for config_path in &config_paths {
        remove_shell_integration_from_path(config_path)?;
    }

    let to_append = format!("\n{block}\n");
    if path.exists() {
        let mut existing = fs::read_to_string(&path)?;
        existing.push_str(&to_append);
        fs::write(&path, existing)?;
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, to_append)?;
    }
    Ok(())
}

pub fn remove_shell_integration(shell: Shell) -> std::io::Result<()> {
    let config_paths = match get_config_paths(shell) {
        Some(paths) => paths,
        None => return Ok(()),
    };
    for path in config_paths {
        remove_shell_integration_from_path(&path)?;
    }
    Ok(())
}

fn remove_shell_integration_from_path(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(path)?;
    if !content.contains(WRAPPER_SIGNATURE) {
        return Ok(());
    }
    let mut lines: Vec<String> = content.split('\n').map(|s| s.to_string()).collect();
    let start = match lines.iter().position(|l| l.contains(WRAPPER_SIGNATURE)) {
        Some(i) => i,
        None => return Ok(()),
    };
    let end = {
        let view: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        find_setup_end_index(&view, start)
    };

    let remove_start = if start > 0 && lines[start - 1].trim().is_empty() {
        start - 1
    } else {
        start
    };
    let remove_end = if end + 1 < lines.len() && lines[end + 1].trim().is_empty() {
        end + 1
    } else {
        end
    };
    lines.drain(remove_start..=remove_end);
    fs::write(path, lines.join("\n"))
}

/// Find the end of a setup block. Looks for the explicit end marker first;
/// falls back to the last `}` line within 50 lines of the start (legacy
/// installations from before the marker existed).
pub fn find_setup_end_index(lines: &[&str], start: usize) -> usize {
    for (offset, line) in lines.iter().enumerate().skip(start + 1) {
        if line.contains(SETUP_END_MARKER) {
            return offset;
        }
        if offset - start > 50 {
            break;
        }
    }
    let mut end = start;
    for (offset, line) in lines.iter().enumerate().skip(start + 1) {
        if line.trim() == "}" {
            end = offset;
        }
        if offset - start > 50 {
            break;
        }
    }
    end
}

pub fn generate_setup_block(shell: Shell, command_name: &str, today: String) -> String {
    let completions = match shell {
        Shell::Zsh => generate_zsh_completions(command_name),
        _ => generate_bash_completions(command_name),
    };
    format!(
        "{WRAPPER_SIGNATURE} {today}\n\
         {completions}\n\
         {command_name}() {{\n\
         \x20\x20if [ $# -eq 0 ]; then\n\
         \x20\x20\x20\x20local dir\n\
         \x20\x20\x20\x20if dir=$(FORCE_COLOR=3 command {command_name} --from-wrapper); then\n\
         \x20\x20\x20\x20\x20\x20if [ -n \"$dir\" ]; then\n\
         \x20\x20\x20\x20\x20\x20\x20\x20builtin cd \"$dir\" && echo \"Wisetree: Navigated to $(pwd)\"\n\
         \x20\x20\x20\x20\x20\x20fi\n\
         \x20\x20\x20\x20fi\n\
         \x20\x20else\n\
         \x20\x20\x20\x20command {command_name} \"$@\"\n\
         \x20\x20fi\n\
         }}\n\
         {SETUP_END_MARKER}"
    )
}

fn generate_bash_completions(command_name: &str) -> String {
    format!(
        "_wisetree_completions() {{\n\
         \x20\x20local cur=\"${{COMP_WORDS[COMP_CWORD]}}\"\n\
         \x20\x20local commands=\"create dashboard settings\"\n\
         \x20\x20local flags=\"--help --version --mode --from-wrapper\"\n\
         \x20\x20if [[ ${{COMP_CWORD}} -eq 1 ]]; then\n\
         \x20\x20\x20\x20COMPREPLY=($(compgen -W \"${{commands}} ${{flags}}\" -- \"${{cur}}\"))\n\
         \x20\x20elif [[ \"${{COMP_WORDS[1]}}\" == \"--mode\" || \"${{COMP_WORDS[1]}}\" == \"-m\" ]]; then\n\
         \x20\x20\x20\x20COMPREPLY=($(compgen -W \"menu create dashboard settings\" -- \"${{cur}}\"))\n\
         \x20\x20fi\n\
         }}\n\
         complete -F _wisetree_completions {command_name}"
    )
}

fn generate_zsh_completions(command_name: &str) -> String {
    format!(
        "_wisetree() {{\n\
         \x20\x20local -a commands\n\
         \x20\x20commands=(\n\
         \x20\x20\x20\x20'create:Create a new worktree'\n\
         \x20\x20\x20\x20'dashboard:Live worktree dashboard'\n\
         \x20\x20\x20\x20'settings:Manage configuration'\n\
         \x20\x20)\n\
         \x20\x20_arguments -C \\\n\
         \x20\x20\x20\x20'(-h --help){{-h,--help}}[Show help]' \\\n\
         \x20\x20\x20\x20'(-v --version){{-v,--version}}[Show version]' \\\n\
         \x20\x20\x20\x20'(-m --mode){{-m,--mode}}[Set mode]:mode:(menu create dashboard settings)' \\\n\
         \x20\x20\x20\x20'--from-wrapper[Called from shell wrapper]' \\\n\
         \x20\x20\x20\x20'1:command:->command'\n\
         \x20\x20case \"$state\" in\n\
         \x20\x20\x20\x20command)\n\
         \x20\x20\x20\x20\x20\x20_describe -t commands 'wisetree commands' commands\n\
         \x20\x20\x20\x20\x20\x20;;\n\
         \x20\x20esac\n\
         }}\n\
         compdef _wisetree {command_name}"
    )
}

fn today_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Convert epoch seconds to YYYY-MM-DD (UTC). We don't pull `chrono` in
    // for one date format — this is a small civil-date conversion.
    epoch_to_iso_date(secs)
}

fn epoch_to_iso_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    // Civil date algorithm — Howard Hinnant's date algorithms.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146_096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
