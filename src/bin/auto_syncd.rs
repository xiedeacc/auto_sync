use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use auto_sync::core::config::{AppConfig, load_config, load_or_create_config};
use auto_sync::core::logging::init_logging;
use auto_sync::core::state::State;
use auto_sync::core::sync::sync_all_pending;
use auto_sync::core::watcher::fanotify::spawn_fanotify_thread;
use clap::Parser;
use tracing::{error, info, warn};

#[derive(Debug, Parser)]
#[command(name = "auto_syncd")]
#[command(about = "auto_sync periodic directory sync daemon")]
struct Args {
    #[arg(long, default_value = "conf/auto_sync.toml")]
    config: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = load_or_create_config(&args.config)?;
    let _log_guard = init_logging(&cfg.app.log_dir, "auto_syncd")?;
    info!(config = %args.config.display(), "auto_sync daemon starting");

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || {
            shutdown.store(true, Ordering::SeqCst);
        })
        .context("failed to install Ctrl-C handler")?;
    }

    run(args.config, cfg, shutdown)
}

fn run(config_path: PathBuf, initial_cfg: AppConfig, shutdown: Arc<AtomicBool>) -> Result<()> {
    let mut cfg = initial_cfg;
    let mut state = State::open(&cfg.app.data_db)?;
    state.ensure_config(&cfg)?;
    state.ensure_open_cycles(&cfg)?;

    let mut watcher_state = start_watcher(&cfg);
    let mut watcher_signature = config_signature(&cfg);
    let mut last_status_log =
        Instant::now() - Duration::from_secs(cfg.app.status_log_interval_secs);

    while !shutdown.load(Ordering::SeqCst) {
        match load_config(&config_path) {
            Ok(new_cfg) => {
                let signature = config_signature(&new_cfg);
                if signature != watcher_signature {
                    info!("config changed; restarting source watcher");
                    stop_watcher(&mut watcher_state);
                    watcher_state = start_watcher(&new_cfg);
                    watcher_signature = signature;
                }
                cfg = new_cfg;
            }
            Err(err) => warn!(error = %err, "failed to reload config; keeping previous config"),
        }

        if let Err(err) = state.ensure_config(&cfg) {
            error!(error = %err, "failed to persist config into state db");
        }
        match state.advance_due_destination_targets(&cfg) {
            Ok(closed) => {
                for cycle in closed {
                    info!(
                        source = cycle.source_id,
                        cycle_id = cycle.id,
                        "cycle target advanced"
                    );
                }
            }
            Err(err) => error!(error = %err, "failed to advance due destination targets"),
        }

        if let Err(err) = sync_all_pending(&cfg, &mut state) {
            error!(error = %err, "sync pending cycles failed");
        }

        if last_status_log.elapsed() >= Duration::from_secs(cfg.app.status_log_interval_secs) {
            log_destination_status(&state, &cfg);
            last_status_log = Instant::now();
        }

        thread::sleep(Duration::from_secs(5));
    }

    stop_watcher(&mut watcher_state);
    info!("auto_sync daemon stopped");
    Ok(())
}

fn log_destination_status(state: &State, cfg: &AppConfig) {
    match state.destination_views(cfg) {
        Ok(views) => {
            for view in views {
                info!(
                    source = view.source_id,
                    destination = view.destination_id,
                    path = view.path,
                    latest_cycle = view.latest_closed_cycle_id.unwrap_or_default(),
                    verified_cycle = view.last_verified_cycle_id.unwrap_or_default(),
                    status = view.status,
                    reason = view.status_reason,
                    "destination status"
                );
            }
        }
        Err(err) => error!(error = %err, "failed to load destination status"),
    }
}

struct WatcherState {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

fn start_watcher(cfg: &AppConfig) -> WatcherState {
    let stop = Arc::new(AtomicBool::new(false));
    let handle = spawn_fanotify_thread(cfg.clone(), cfg.app.data_db.clone(), stop.clone());
    WatcherState {
        stop,
        handle: Some(handle),
    }
}

fn stop_watcher(state: &mut WatcherState) {
    state.stop.store(true, Ordering::SeqCst);
    if let Some(handle) = state.handle.take() {
        if let Err(err) = handle.join() {
            warn!(?err, "source watcher thread join failed");
        }
    }
}

fn config_signature(cfg: &AppConfig) -> String {
    toml::to_string(cfg).unwrap_or_default()
}
