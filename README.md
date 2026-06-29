# auto_sync

Rust directory sync tool for Linux headless/Web UI deployments, with LAN peer
discovery and native TCP transfer for Linux and Windows peers.

The GUI is implemented with Tauri and uses WebKitGTK on Linux.
Headless deployments can use the Web UI.

## LAN machines

The Web UI exposes a machine selector in path picking. Peers are discovered on
UDP `18766`; the Web API listens on the configured `port`, commonly
`18765`. Manual machines can also be added with host, port, SSH user, SSH
port, and OS.

Cross-machine sync uses the auto_sync Web API and a keep-alive TCP connection
pool. Full sync and reconcile build source and destination snapshots, transfer
only mismatched files and symlinks, optionally mirror-delete destination extras,
and verify the destination after transfer.

Windows deployment can use the system OpenSSH Server optional feature when
requested, but `auto_sync` is launched through a current-user Startup launcher
rather than a Windows service.

## Build

A single runtime binary `auto_sync` runs the scheduler, file watcher and web
server in one process, and also opens the desktop window when a display is
available. Build with the desktop (Tauri) feature for GUI hosts, or
`--no-default-features` for a headless web-only build.

```bash
# Desktop-capable build (default features include the GUI):
cargo build --release --bin auto_sync --bin auto_syncctl
# Headless (web only, no Tauri/webkit dependency):
cargo build --release --no-default-features --bin auto_sync --bin auto_syncctl
install -m 0755 target/release/auto_sync bin/auto_sync
install -m 0755 target/release/auto_syncctl bin/auto_syncctl
```

## Run

```bash
bin/auto_sync --config conf/auto_sync.toml            # scheduler + web (+ desktop if available)
bin/auto_sync --config conf/auto_sync.toml --no-gui   # force web-only
bin/auto_syncctl --config conf/auto_sync.toml status
bin/auto_syncctl --config conf/auto_sync.toml sync-now --close-current
```

Local Linux systemd deploy:

```bash
scripts/deploy_local.sh
```

The local deploy script builds the release binary, installs it to
`/opt/auto_sync`, installs `conf/auto_sync.linux.toml` as the local config,
installs the systemd unit, and starts the single `auto_sync.service` on Linux.
It first checks the Ubuntu/Debian build environment and installs missing build
dependencies plus Rust stable only when needed. On Linux hosts without a GUI
environment it builds the headless (web-only) variant; with a GUI it builds with
desktop support. NAS, tiger Linux, and OpenWrt share the same Linux config
template.

Machine deploy helpers:

```bash
pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1
ssh -p 10022 root@192.168.2.247 'cd /opt/auto_sync && git pull && scripts/deploy_local.sh'
scripts/deploy_openwrt.sh --host 192.168.2.1 --port 10022 --user root
```

`deploy_windows.ps1` is run locally on Windows, builds the release binary into
the repository `bin\` directory, keeps config under `conf\`, and creates a
current-user Startup launcher for the single `auto_sync` process instead
of installing it as a Windows service. OpenSSH setup is opt-in via
`-InstallSshd`. NAS is Ubuntu x64 and builds on NAS itself under
`/opt/auto_sync`; use `git pull` plus `scripts/deploy_local.sh` on
NAS for deployments. `deploy_local.sh` installs missing Ubuntu/Debian build
dependencies and Rust stable on first run, then skips setup on later runs. The
Windows daemon uses the NTFS USN Journal for realtime local source change
detection and keeps periodic full reconciliation as a fallback for journal
gaps, journal resets, and first-run verification. The GUI and Web UI share this
daemon-backed state instead of running separate watcher logic.
`deploy_openwrt.sh` cross-compiles aarch64 OpenWrt binaries when needed,
installs the `conf/auto_sync.procd` procd init script, and deploys to
`/opt/auto_sync`.
