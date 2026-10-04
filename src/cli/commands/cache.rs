//! `wisetree cache` non-interactive handler.

use crate::cli::args::{CacheAction, CliArgs, CliCommand};
use crate::errors::{Result, WisetreeError};
use crate::files::{CacheOverview, CachePruneReport};
use crate::worktree::WorktreeService;

pub async fn run(args: CliArgs, service: &WorktreeService) -> Result<()> {
    let CliCommand::Cache { action } = args.command else {
        return Err(WisetreeError::other("Invalid cache command invocation"));
    };

    match action {
        CacheAction::List => {
            let overview = service.cache_overview().await?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&overview)?);
            } else {
                print_overview(&overview);
            }
        }
        CacheAction::Prune => {
            let report = service.prune_repo_cache().await?;
            print_prune_report(&report);
        }
        CacheAction::Clear => {
            if !args.force {
                return Err(WisetreeError::other(
                    "Refusing to clear cache without --force",
                ));
            }
            service.clear_repo_cache().await?;
            println!("Cleared cache for this repository");
        }
        CacheAction::Path => {
            println!("{}", service.cache_dir()?.display());
        }
    }

    Ok(())
}

fn print_overview(overview: &CacheOverview) {
    println!("Cache root: {}", overview.cache_dir.display());
    println!("Entries: {}", overview.entries.len());
    println!("Total size: {}", human_size(overview.total_size_bytes));
    if overview.users.is_empty() {
        println!("Active worktrees: none");
    } else {
        println!("Active worktrees:");
        for user in &overview.users {
            println!("  - {}", user.worktree_path);
        }
    }

    for entry in &overview.entries {
        println!(
            "{}\n  size: {}\n  age: {}d\n  users: {}",
            entry.relative_path,
            human_size(entry.size_bytes),
            entry.age_days,
            entry.user_count,
        );
    }
}

fn print_prune_report(report: &CachePruneReport) {
    println!("Cache root: {}", report.cache_dir.display());
    if report.removed.is_empty() {
        println!("Removed: none");
    } else {
        println!("Removed:");
        for item in &report.removed {
            println!("  - {item}");
        }
    }

    if !report.skipped.is_empty() {
        println!("Skipped:");
        for item in &report.skipped {
            println!("  - {item}");
        }
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
