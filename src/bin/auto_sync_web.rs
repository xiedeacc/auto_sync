use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use auto_sync::core::backend::Backend;
use auto_sync::core::config::load_or_create_config;
use auto_sync::core::logging::init_logging;
use auto_sync::core::web_api;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "auto_sync_web")]
#[command(about = "Headless Web UI for auto_sync")]
struct Args {
    #[arg(long, default_value = "conf/auto_sync.toml")]
    config: PathBuf,

    #[arg(long)]
    bind: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = load_or_create_config(&args.config)?;
    let _log_guard = init_logging(&cfg.app.log_dir, "auto_sync_web")?;
    let bind = args.bind.unwrap_or_else(|| cfg.app.web_bind.clone());
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid bind address {bind}"))?;
    let backend = Backend::new(args.config, addr.port());
    web_api::serve(backend, addr).await
}
