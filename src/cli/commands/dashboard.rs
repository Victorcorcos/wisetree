//! `wisetree dashboard` non-interactive handler.

use crate::config::schema::DashboardConfig;
use crate::errors::Result;
use crate::services::DashboardService;
use crate::worktree::WorktreeService;
use std::io::Write;

pub async fn run(service: &WorktreeService, watch: bool) -> Result<()> {
    let git_root = service.git_service().git_root().to_path_buf();
    let config: DashboardConfig = service.config_service().config().dashboard.clone();
    let dashboard = DashboardService::new(git_root, config);

    if watch {
        let mut watch = dashboard.watch();
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                maybe_rows = watch.rx.recv() => {
                    let Some(update) = maybe_rows else {
                        break;
                    };
                    let line = serde_json::to_string(update.rows())?;
                    println!("{line}");
                    let _ = std::io::stdout().flush();
                }
            }
        }
        Ok(())
    } else {
        let rows = dashboard.snapshot().await?;
        let json = serde_json::to_string_pretty(&rows)?;
        println!("{json}");
        Ok(())
    }
}
