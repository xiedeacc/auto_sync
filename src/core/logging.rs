use std::path::Path;

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriterExt;

pub fn init_logging(log_dir: &Path, name: &str) -> Result<WorkerGuard> {
    std::fs::create_dir_all(log_dir)
        .with_context(|| format!("failed to create log dir {}", log_dir.display()))?;
    let file_appender = tracing_appender::rolling::daily(log_dir, format!("{name}.log"));
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let stdout = std::io::stdout.with_max_level(tracing::Level::INFO);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(stdout.and(file_writer))
        .with_ansi(false)
        .init();
    Ok(guard)
}
