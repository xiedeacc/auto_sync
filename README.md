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

Windows deployment can use the system OpenSSH Server optional feature, with a
bundled OpenSSH fallback for machines that do not have it. Download the bundled
fallback archive with:

```bash
scripts/download_windows_runtime.sh
```

The script places OpenSSH under `bin/windows/`, including extracted folders and
`SHA256SUMS`. The Windows runtime directory is tracked in git. Local Windows
deployment prefers the Windows OpenSSH Server optional feature when it is
available, and falls back to the bundled OpenSSH runtime when it is not.

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
`auto_sync_web.service` on Linux. On Linux hosts without a GUI environment it
installs headless mode only; with a GUI it also installs `auto_sync_gui`.

Machine deploy helpers:

```bash
scripts/deploy_tiger.sh
scripts/deploy_nas.sh --host 192.168.3.178 --port 10022 --user root
pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1
scripts/deploy_openwrt.sh --host 192.168.2.1 --port 10022 --user root
```

`deploy_tiger.sh` deploys to localhost. `deploy_nas.sh` deploys to the NAS with
systemd. `deploy_windows.ps1` is run locally on Windows, builds release
binaries, installs them under `C:\auto_sync`, ensures `sshd` is available,
installs the `auto_syncd` service with Automatic startup, and requests
administrator privileges when service or machine PATH changes are needed. The
Windows daemon uses the NTFS USN Journal for realtime local source change
detection and keeps periodic full reconciliation as a fallback for journal
gaps, journal resets, and first-run verification. The GUI and Web UI share this
daemon-backed state instead of running separate watcher logic.
`deploy_openwrt.sh` cross-compiles aarch64 OpenWrt binaries, installs procd
init scripts, and deploys to `/usr/local/auto_sync`.
