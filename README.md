# auto_sync

Rust directory sync tool for Linux headless/Web UI deployments, with LAN peer
discovery and native TCP transfer for Linux and Windows peers.

The GUI is implemented with Tauri and uses WebKitGTK on Linux.
Headless deployments can use the Web UI.

## LAN machines

The Web UI exposes a machine selector in path picking. Peers are discovered on
UDP `18766`; the Web API listens on the configured `web_bind` port, commonly
`18765`. Manual machines can also be added with host, web port, SSH user, SSH
port, and OS.

Cross-machine sync uses the auto_sync Web API and a keep-alive TCP connection
pool. Full sync and reconcile build source and destination snapshots, transfer
only mismatched files and symlinks, optionally mirror-delete destination extras,
and verify the destination after transfer.

Windows deployment can use the system OpenSSH Server optional feature when
requested, but the daemon and GUI are launched through a current-user Startup
launcher rather than a Windows service.

## Build

```bash
cargo build --release
install -m 0755 target/release/auto_syncd bin/auto_syncd
install -m 0755 target/release/auto_syncctl bin/auto_syncctl
install -m 0755 target/release/auto_sync_gui bin/auto_sync_gui
install -m 0755 target/release/auto_sync_web bin/auto_sync_web
```

## Run

```bash
bin/auto_sync_gui --config conf/auto_sync.toml
bin/auto_sync_web --config conf/auto_sync.toml --bind 0.0.0.0:18765
bin/auto_syncd --config conf/auto_sync.toml
bin/auto_syncctl --config conf/auto_sync.toml status
bin/auto_syncctl --config conf/auto_sync.toml sync-now --close-current
```

Local Linux systemd deploy:

```bash
scripts/deploy_local.sh
```

The local deploy script builds release binaries, installs them to
`/usr/local/auto_sync`, seeds the config only if it does not already exist,
installs systemd units, and starts `auto_sync.service` plus
`auto_sync_web.service` on Linux. It first checks the Ubuntu/Debian build
environment and installs missing build dependencies plus Rust stable only when
needed. On Linux hosts without a GUI environment it installs headless mode only;
with a GUI it also installs `auto_sync_gui`.

Machine deploy helpers:

```bash
pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1
ssh -p 10022 root@192.168.2.247 'cd /root/src/rust/auto_sync && git pull && scripts/deploy_local.sh'
scripts/deploy_openwrt.sh --host 192.168.2.1 --port 10022 --user root
```

`deploy_windows.ps1` is run locally on Windows, builds release binaries into
the repository `bin\` directory, keeps config under `conf\`, and creates a
current-user Startup launcher for both `auto_syncd` and `auto_sync_gui` instead
of installing `auto_syncd` as a Windows service. OpenSSH setup is opt-in via
`-InstallSshd`. NAS is Ubuntu x64 and builds on NAS itself under
`/root/src/rust/auto_sync`; use `git pull` plus `scripts/deploy_local.sh` on
NAS for deployments. `deploy_local.sh` installs missing Ubuntu/Debian build
dependencies and Rust stable on first run, then skips setup on later runs. The
Windows daemon uses the NTFS USN Journal for realtime local source change
detection and keeps periodic full reconciliation as a fallback for journal
gaps, journal resets, and first-run verification. The GUI and Web UI share this
daemon-backed state instead of running separate watcher logic.
`deploy_openwrt.sh` cross-compiles aarch64 OpenWrt binaries when needed,
installs procd init scripts, and deploys to `/usr/local/auto_sync`.
