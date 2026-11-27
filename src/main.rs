//! Vibefleet — a ratatui control room for orchestrating a fleet of AI coding
//! agents in parallel git worktrees, with Flagsmith-driven feature flags.

mod agent;
mod domain;
mod flags;
mod orchestrator;
mod seed;
mod tui;
mod workspace;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // Logging goes to a file so it never corrupts the TUI.
    let log_dir = directories::ProjectDirs::from("", "", "vibefleet")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let _ = std::fs::create_dir_all(&log_dir);
    let file = std::fs::File::create(log_dir.join("vibefleet.log")).ok();
    if let Some(file) = file {
        let _ = tracing_subscriber::fmt()
            .with_writer(file)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .try_init();
    }

    tui::run().await
}