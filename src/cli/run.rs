//! Top-level CLI dispatcher. Decides between `--help`, `--version`,
//! non-interactive subcommands, and the interactive TUI based on the parsed
//! args.

use std::io::IsTerminal;
use std::process::ExitCode;

use crate::cli::args::{help_text, parse_args, CliArgs, CliCommand, ParsedArgs};
use crate::cli::commands;
use crate::git::exec::get_git_root;
use crate::tui::App;
use crate::worktree::WorktreeService;
use crate::VERSION;

pub fn run() -> Result<ExitCode, anyhow::Error> {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let parsed = match parse_args(argv) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {e}");
            return Ok(ExitCode::from(1));
        }
    };

    if parsed.help {
        print!("{}", help_text());
        return Ok(ExitCode::SUCCESS);
    }
    if parsed.version {
        println!("Wisetree v{VERSION}");
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(cli_args) = parsed.cli_args {
        return run_cli(cli_args);
    }

    run_tui(parsed)
}

fn run_tui(parsed: ParsedArgs) -> Result<ExitCode, anyhow::Error> {
    // In wrapper mode real stdout is a pipe to the parent shell, so the
    // usual `is_terminal()` check on stdout would always fail. We only
    // require a TTY on stdin (so input still works) — rendering is sent
    // to `/dev/tty` directly by `terminal::enter_wrapper`.
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "Error: wisetree requires a TTY for interactive mode. \
             Run a subcommand (create/list/delete) for non-interactive use."
        );
        return Ok(ExitCode::from(1));
    }
    if !parsed.is_from_wrapper && !std::io::stdout().is_terminal() {
        eprintln!(
            "Error: wisetree requires a TTY for interactive mode. \
             Run a subcommand (create/list/delete) for non-interactive use."
        );
        return Ok(ExitCode::from(1));
    }

    if parsed.is_from_wrapper {
        // Belt-and-suspenders: the shell wrapper already exports this, but
        // set it again so a manual `wisetree --from-wrapper` invocation gets
        // the same colored rendering the wrapper relies on.
        std::env::set_var("FORCE_COLOR", "3");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let selected_path = runtime.block_on(async move {
        let app = App::new(parsed.mode, parsed.is_from_wrapper);
        app.run().await
    })?;

    if parsed.is_from_wrapper {
        if let Some(path) = selected_path {
            // Write to *real* stdout (the inherited pipe) so the wrapper's
            // `local dir=$(...)` captures exactly the path. Match branchlet's
            // trailing newline.
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{path}");
            let _ = out.flush();
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn run_cli(args: CliArgs) -> Result<ExitCode, anyhow::Error> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let result = runtime.block_on(async move {
        let git_root = get_git_root(None).await.map(std::path::PathBuf::from);
        let mut service = WorktreeService::new(git_root);
        service.initialize().await?;

        match args.command {
            CliCommand::Create => commands::create::run(args, &service).await,
            CliCommand::List => commands::list::run(&service).await,
            CliCommand::Delete => commands::delete::run(args, &service).await,
        }
    });

    match result {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(err) => {
            eprintln!("Error: {err}");
            Ok(ExitCode::from(1))
        }
    }
}
