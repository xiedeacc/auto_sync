use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use auto_sync::core::config::{
    AppConfig, load_config, load_or_create_config, resolve_config_path, save_config,
};
use auto_sync::core::state::State;
use auto_sync::core::sync::{sync_all_now, sync_all_pending};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "auto_syncctl")]
#[command(about = "Control utility for auto_sync")]
struct Args {
    /// Path to the config file. If omitted, the same lookup as auto_sync is used
    /// (conf/auto_sync.toml relative to the current dir, then to the executable).
    #[arg(long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    InitConfig,
    Status,
    SyncNow {
        #[arg(long)]
        close_current: bool,
    },
    /// Cancel running activity on the local daemon (and, unless --local-only,
    /// on every known peer machine): sync passes, compares, and tree walks.
    Cancel {
        /// Limit to one kind: "sync" or "compare". Default cancels both.
        #[arg(long)]
        scope: Option<String>,
        /// With --destination-id, limit the cancel to work scoped to that
        /// destination of this source.
        #[arg(long)]
        source_id: Option<String>,
        #[arg(long)]
        destination_id: Option<String>,
        /// Do not forward the cancel to peer machines.
        #[arg(long)]
        local_only: bool,
    },
    PrintSystemd {
        #[arg(long, default_value = "/opt/auto_sync")]
        install_dir: PathBuf,
    },
    DeployNas {
        #[arg(long, default_value = "192.168.2.247")]
        host: String,
        #[arg(long, default_value_t = 10022)]
        port: u16,
        #[arg(long, default_value = "root")]
        user: String,
        #[arg(long, default_value = "/opt/auto_sync")]
        install_dir: PathBuf,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config_path = resolve_config_path(args.config.as_deref());
    match args.command {
        CommandKind::InitConfig => {
            let cfg = load_or_create_config(&config_path)?;
            save_config(&config_path, &cfg)?;
            println!("initialized {}", config_path.display());
        }
        CommandKind::Status => {
            let cfg = load_config(&config_path)?;
            let state = State::open(&cfg.app.data_db)?;
            state.ensure_config(&cfg)?;
            print_status(&state, &cfg)?;
        }
        CommandKind::SyncNow { close_current } => {
            let cfg = load_config(&config_path)?;
            let mut state = State::open(&cfg.app.data_db)?;
            state.ensure_config(&cfg)?;
            state.ensure_open_cycles(&cfg)?;
            if close_current {
                sync_all_now(&cfg, &mut state)?;
                println!("closed current cycle(s) and synced all destinations");
            } else {
                sync_all_pending(&cfg, &mut state)?;
            }
            print_status(&state, &cfg)?;
        }
        CommandKind::Cancel {
            scope,
            source_id,
            destination_id,
            local_only,
        } => {
            let cfg = load_config(&config_path)?;
            // The daemon binds its preferred LAN address (not loopback); use
            // the same resolution to reach it.
            let local = auto_sync::core::config::MachineConfig {
                id: "local".to_string(),
                host: auto_sync::core::config::preferred_local_host(),
                port: cfg.app.port,
                ..Default::default()
            };
            let req = serde_json::json!({
                "scope": scope,
                "source_id": source_id,
                "destination_id": destination_id,
                "propagate": !local_only,
            });
            let outcome: auto_sync::core::backend::CancelOutcome =
                auto_sync::core::machines::remote_post_json(
                    &local,
                    "/api/cancel-activity",
                    &req,
                    std::time::Duration::from_secs(10),
                )
                .context("failed to reach the local auto_sync daemon")?;
            println!("cancelled {} local operation(s)", outcome.cancelled_local);
            for machine in outcome.machines {
                match machine.error {
                    None => println!(
                        "machine {}: cancelled {} operation(s)",
                        machine.machine_id, machine.cancelled
                    ),
                    Some(err) => {
                        println!("machine {}: cancel failed: {err}", machine.machine_id)
                    }
                }
            }
        }
        CommandKind::PrintSystemd { install_dir } => {
            print!("{}", systemd_unit(&install_dir));
        }
        CommandKind::DeployNas {
            host,
            port,
            user,
            install_dir,
        } => {
            let cfg = load_config(&config_path)?;
            deploy_nas(&cfg, &config_path, &host, port, &user, &install_dir)?;
        }
    }
    Ok(())
}

fn print_status(state: &State, cfg: &AppConfig) -> Result<()> {
    let views = state.destination_views(cfg)?;
    if views.is_empty() {
        println!("no destinations configured");
        return Ok(());
    }
    println!(
        "{:<18} {:<18} {:<7} {:<12} {:<12} {}",
        "SOURCE", "DESTINATION", "STATUS", "TARGET", "VERIFIED", "REASON"
    );
    for view in views {
        println!(
            "{:<18} {:<18} {:<7} {:<12} {:<12} {}",
            view.source_id,
            view.destination_id,
            view.status,
            view.target_cycle_id
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
            view.last_verified_cycle_id
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
            view.status_reason
        );
    }
    Ok(())
}

fn deploy_nas(
    _cfg: &AppConfig,
    config_path: &Path,
    host: &str,
    port: u16,
    user: &str,
    install_dir: &Path,
) -> Result<()> {
    let target = format!("{user}@{host}");
    let ssh_port = port.to_string();
    run(Command::new("ssh").args([
        "-p",
        &ssh_port,
        &target,
        &format!(
            "mkdir -p {0}/bin {0}/conf {0}/data {0}/logs",
            install_dir.display()
        ),
    ]))?;

    // Stop running services before overwriting binaries so scp doesn't fail on
    // a busy (in-use) file. Ignore errors so the first-ever deploy, when the
    // units don't exist yet, still succeeds.
    // Stop and retire the old split services (daemon + separate web) before
    // overwriting binaries; the unified auto_sync.service replaces both.
    run(Command::new("ssh").args([
        "-p",
        &ssh_port,
        &target,
        "systemctl disable --now auto_sync_web.service 2>/dev/null; rm -f /etc/systemd/system/auto_sync_web.service; systemctl stop auto_sync.service 2>/dev/null || true",
    ]))?;

    for binary in ["auto_sync", "auto_syncctl"] {
        let local = PathBuf::from("bin").join(binary);
        if !local.exists() {
            bail!(
                "{} does not exist; build first and place binaries in bin/",
                local.display()
            );
        }
        run(Command::new("scp").args([
            "-P",
            &ssh_port,
            local.to_string_lossy().as_ref(),
            &format!("{target}:{}/bin/{binary}", install_dir.display()),
        ]))?;
    }
    // Remove stale binaries from the previous multi-binary layout.
    run(Command::new("ssh").args([
        "-p",
        &ssh_port,
        &target,
        &format!(
            "rm -f {0}/bin/auto_syncd {0}/bin/auto_sync_web {0}/bin/auto_sync_gui",
            install_dir.display()
        ),
    ]))?;

    // Only seed the config on first deploy; never overwrite an existing one so
    // edits made on the NAS survive redeploys.
    let remote_cfg = format!("{}/conf/auto_sync.toml", install_dir.display());
    let cfg_exists = Command::new("ssh")
        .args(["-p", &ssh_port, &target, &format!("test -f {remote_cfg}")])
        .status()
        .context("failed to check for existing remote config")?
        .success();
    if cfg_exists {
        println!("remote config {remote_cfg} already exists; leaving it untouched");
    } else {
        run(Command::new("scp").args([
            "-P",
            &ssh_port,
            config_path.to_string_lossy().as_ref(),
            &format!("{target}:{remote_cfg}"),
        ]))?;
    }

    let daemon_unit = systemd_unit(install_dir);
    let tmp_unit = PathBuf::from("conf/auto_sync.service");
    fs::write(&tmp_unit, daemon_unit)?;
    run(Command::new("scp").args([
        "-P",
        &ssh_port,
        tmp_unit.to_string_lossy().as_ref(),
        &format!("{target}:/etc/systemd/system/auto_sync.service"),
    ]))?;
    run(Command::new("ssh").args([
        "-p",
        &ssh_port,
        &target,
        "systemctl daemon-reload && systemctl enable --now auto_sync.service && systemctl status --no-pager auto_sync.service",
    ]))?;
    Ok(())
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().context("failed to execute external command")?;
    if !status.success() {
        bail!("external command failed with status {status}");
    }
    Ok(())
}

fn systemd_unit(install_dir: &Path) -> String {
    format!(
        r#"[Unit]
Description=auto_sync daemon
After=local-fs.target network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory={dir}
ExecStart={dir}/bin/auto_sync
Restart=always
RestartSec=5
User=root
Group=root
CapabilityBoundingSet=CAP_SYS_ADMIN CAP_SYS_RAWIO CAP_DAC_READ_SEARCH CAP_DAC_OVERRIDE CAP_FOWNER CAP_CHOWN
AmbientCapabilities=CAP_SYS_ADMIN CAP_SYS_RAWIO CAP_DAC_READ_SEARCH CAP_DAC_OVERRIDE CAP_FOWNER CAP_CHOWN
NoNewPrivileges=false

[Install]
WantedBy=multi-user.target
"#,
        dir = install_dir.display()
    )
}
