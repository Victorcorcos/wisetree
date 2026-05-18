//! Argument parsing for the `wisetree` binary.
//!
//! Mirrors `branchlet/src/index.tsx` argument handling so the CLI surface is
//! identical (flag names, aliases, modes, non-interactive detection,
//! `--from-wrapper` semantics).

use std::ffi::OsString;

use crate::messages::WELCOME;

/// Initial TUI screen the user wants to land on. `Setup` is intentionally
/// **not** here — it can only be reached from the main menu, matching upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Menu,
    Create,
    Dashboard,
    Cache,
    Settings,
}

impl AppMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Menu => "menu",
            Self::Create => "create",
            Self::Dashboard => "dashboard",
            Self::Cache => "cache",
            Self::Settings => "settings",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "menu" => Some(Self::Menu),
            "create" => Some(Self::Create),
            "dashboard" => Some(Self::Dashboard),
            "cache" => Some(Self::Cache),
            "settings" => Some(Self::Settings),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheAction {
    List,
    Prune,
    Clear,
    Path,
}

impl CacheAction {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "list" => Some(Self::List),
            "prune" => Some(Self::Prune),
            "clear" => Some(Self::Clear),
            "path" => Some(Self::Path),
            _ => None,
        }
    }
}

/// Subcommand selected for non-interactive CLI execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    Create,
    Dashboard,
    Cache { action: CacheAction },
}

/// Flags supplied alongside a non-interactive subcommand.
#[derive(Debug, Clone)]
pub struct CliArgs {
    pub command: CliCommand,
    pub name: Option<String>,
    pub source: Option<String>,
    pub branch: Option<String>,
    pub json: bool,
    pub watch: bool,
    pub force: bool,
}

/// Result of parsing the process command line. The dispatcher acts on this.
#[derive(Debug, Clone)]
pub struct ParsedArgs {
    pub mode: AppMode,
    pub help: bool,
    pub version: bool,
    pub is_from_wrapper: bool,
    pub cli_args: Option<CliArgs>,
}

#[derive(Debug, thiserror::Error)]
pub enum ArgError {
    #[error("Unknown option: {0}")]
    Unknown(String),
    #[error("Option {0} requires a value")]
    MissingValue(String),
}

/// Parse `argv[1..]`, mirroring branchlet's minimist semantics: unknown
/// flags are tolerated as positional arguments where possible, but here we
/// surface them so the user gets a clear diagnostic.
pub fn parse_args<I>(iter: I) -> Result<ParsedArgs, ArgError>
where
    I: IntoIterator,
    I::Item: Into<OsString>,
{
    let raw: Vec<String> = iter
        .into_iter()
        .map(|s| s.into().to_string_lossy().into_owned())
        .collect();

    let mut help = false;
    let mut version = false;
    let mut from_wrapper = false;
    let mut mode_arg: Option<String> = None;
    let mut name: Option<String> = None;
    let mut source: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut json = false;
    let mut watch = false;
    let mut force = false;
    let mut positional: Vec<String> = Vec::new();

    let mut i = 0;
    while i < raw.len() {
        let token = raw[i].as_str();
        match token {
            "-h" | "--help" => help = true,
            "-v" | "--version" => version = true,
            "--from-wrapper" => from_wrapper = true,
            "--json" => json = true,
            "-w" | "--watch" => watch = true,
            "-f" | "--force" => force = true,
            "-m" | "--mode" => {
                mode_arg = Some(take_value(&raw, &mut i, token)?);
            }
            "-n" | "--name" => {
                name = Some(take_value(&raw, &mut i, token)?);
            }
            "-s" | "--source" => {
                source = Some(take_value(&raw, &mut i, token)?);
            }
            "-b" | "--branch" => {
                branch = Some(take_value(&raw, &mut i, token)?);
            }
            t if t.starts_with("--mode=") => {
                mode_arg = Some(t["--mode=".len()..].to_string());
            }
            t if t.starts_with("--name=") => {
                name = Some(t["--name=".len()..].to_string());
            }
            t if t.starts_with("--source=") => {
                source = Some(t["--source=".len()..].to_string());
            }
            t if t.starts_with("--branch=") => {
                branch = Some(t["--branch=".len()..].to_string());
            }
            t if t.starts_with("--") || (t.starts_with('-') && t.len() > 1) => {
                return Err(ArgError::Unknown(t.to_string()));
            }
            t => positional.push(t.to_string()),
        }
        i += 1;
    }

    if help {
        return Ok(ParsedArgs {
            mode: AppMode::Menu,
            help: true,
            version: false,
            is_from_wrapper: false,
            cli_args: None,
        });
    }
    if version {
        return Ok(ParsedArgs {
            mode: AppMode::Menu,
            help: false,
            version: true,
            is_from_wrapper: false,
            cli_args: None,
        });
    }

    // Mode resolution: explicit `--mode <x>` first, then a positional command,
    // both clamped to the known set; unknown values fall back to Menu.
    let mut mode = AppMode::Menu;
    if let Some(m) = mode_arg.as_deref().and_then(AppMode::parse) {
        mode = m;
    }
    if let Some(first) = positional.first() {
        if let Some(m) = AppMode::parse(first) {
            mode = m;
        }
    }

    let cache_action = positional
        .get(1)
        .and_then(|value| CacheAction::parse(value));

    // Detect non-interactive CLI subcommand.
    let cli_command = match mode {
        AppMode::Create => Some(CliCommand::Create),
        AppMode::Dashboard => Some(CliCommand::Dashboard),
        AppMode::Cache => cache_action.map(|action| CliCommand::Cache { action }),
        _ => None,
    };
    let has_cli_flags = name.is_some()
        || source.is_some()
        || branch.is_some()
        || json
        || watch
        || force
        || cache_action.is_some();

    let cli_args = match mode {
        AppMode::Create | AppMode::Dashboard => match (cli_command, has_cli_flags) {
            (Some(cmd), true) => Some(CliArgs {
                command: cmd,
                name,
                source,
                branch,
                json,
                watch,
                force,
            }),
            _ => None,
        },
        AppMode::Cache => cli_command.map(|cmd| CliArgs {
            command: cmd,
            name,
            source,
            branch,
            json,
            watch,
            force,
        }),
        _ => None,
    };

    Ok(ParsedArgs {
        mode,
        help: false,
        version: false,
        is_from_wrapper: from_wrapper,
        cli_args,
    })
}

fn take_value(raw: &[String], i: &mut usize, flag: &str) -> Result<String, ArgError> {
    let next = i
        .checked_add(1)
        .and_then(|n| raw.get(n))
        .ok_or_else(|| ArgError::MissingValue(flag.to_string()))?;
    *i += 1;
    Ok(next.clone())
}

/// Hand-written help text that mirrors `showHelp()` from upstream verbatim,
/// substituting `wisetree` for `branchlet`.
pub fn help_text() -> String {
    format!(
        "\n{WELCOME}\n\n\
Usage:\n  wisetree [command] [options]\n\n\
Commands:\n  \
create     Create a new worktree\n  \
dashboard  Live worktree dashboard\n  \
cache      Inspect or clean the shared dependency cache\n  \
settings   Manage configuration\n  \
(no command) Start interactive menu\n\n\
Interactive Options:\n  \
-h, --help     Show this help message\n  \
-v, --version  Show version number\n  \
-m, --mode     Set initial mode\n  \
--from-wrapper Called from shell wrapper (outputs path to stdout)\n\n\
Non-Interactive Options:\n  \
-n, --name <name>      Worktree directory name (create)\n  \
-s, --source <branch>  Source branch (create)\n  \
-b, --branch <branch>  New branch name; defaults to source (create)\n  \
-f, --force            Skip confirmation for cache clear\n  \
--json                 Output as JSON (dashboard)\n  \
-w, --watch            Stream JSON Lines (dashboard)\n\n\
Cache Subcommands:\n  \
wisetree cache list    Show cache entries and users\n  \
wisetree cache prune   Remove orphaned cache entries older than 14 days\n  \
wisetree cache clear   Delete the entire cache for this repo (requires --force)\n  \
wisetree cache path    Print the cache root path\n\n\
Interactive Examples:\n  \
wisetree                # Start interactive menu\n  \
wisetree create         # Go directly to create worktree flow\n  \
wisetree dashboard      # Open the live dashboard\n  \
wisetree --from-wrapper # Used by shell wrapper to enable directory switching\n  \
wisetree cache          # Open the shared cache screen\n  \
wisetree settings       # Open settings menu\n\n\
Non-Interactive Examples:\n  \
wisetree create -n my-feature -s main              # Create worktree from main\n  \
wisetree create -n my-feature -s main -b feat/foo  # Create with new branch\n  \
wisetree cache list --json                         # Show cache details as JSON\n  \
wisetree cache clear --force                       # Remove this repo's shared cache\n\n\
wisetree dashboard --json                          # Snapshot dashboard as JSON\n  \
wisetree dashboard --watch                         # Stream dashboard snapshots\n\n\
wisetree cache list --json                         # Show cache details as JSON\n  \
wisetree cache clear --force                       # Remove this repo's shared cache\n\n\
Shell Integration:\n  \
Run 'wisetree' and select \"Setup Shell Integration\" to enable quick directory switching.\n  \
After setup, just run 'wisetree' to quickly change to any worktree directory.\n\n\
Configuration:\n  \
The tool looks for configuration files in the following order:\n  \
1. .wisetree.json in current directory\n  \
2. ~/.wisetree/settings.json (global config)\n\n\
For more information, visit: https://github.com/victorcorcos/wisetree\n"
    )
}
