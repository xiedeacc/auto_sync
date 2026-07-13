# Deployment Paths

This file records the intended real paths for dev and NAS. Keep collector
paths, deployment scripts, systemd units, and shell environment setup aligned
with this table.

## Host Path Matrix

NAS has a small root disk, so large software trees and most user-home state live
under `/opt`. Dev has a large root SSD and intentionally does not mirror NAS
symlinks.

| Component | NAS real path | Dev real path | Notes |
| --- | --- | --- | --- |
| auto_sync | `/opt/usr/local/auto_sync` | `/usr/local/auto_sync` | NAS uses `scripts/deploy_nas.sh`; dev uses `scripts/deploy_local.sh --install-dir /usr/local/auto_sync`. |
| rblog | `/opt/usr/local/blog` | `/usr/local/blog` | Service units must use the host-specific path. NAS `.backup-worktree` is under `/opt/usr/local/blog`. |
| Go SDK | `/opt/usr/local/go/go1.25.1` | `/usr/local/go/go1.25.1` | NAS adds Go to `PATH` via `/etc/profile.d/opt-usr-local-path.sh`; dev uses normal `/usr/local/go`. |
| GOPATH | `/root/src/go` via `/opt/user/root/src/go` symlink | `/root/src/go` | Do not recreate `/root/go`. |
| bin tools | `/opt/usr/local/bin` | `/usr/local/bin` | Includes tools such as `buildifier`. Do not recreate `/usr/local/bin` as a NAS bind mount. |
| buildifier | `/opt/usr/local/bin/buildifier` | `/usr/local/bin/buildifier` | Installed by collector deployment scripts. |
| Halo install | `/opt/usr/local/halo` | `/usr/local/halo` | Runs as root on both hosts. |
| Halo runtime home | `/root/.halo`, `/root/.halo2` symlinked to `/opt/user/root` | `/root/.halo`, `/root/.halo2` real local paths | NAS keeps root home state off the root disk. |
| shadowsocks | `/opt/usr/local/shadowsocks` | `/usr/local/shadowsocks` | Directory is supported for collected config/data/logs; service startup may be disabled when xray owns the ports. |
| TBox | `/opt/usr/local/tbox` | `/usr/local/tbox` | `tbox_client.service` and logrotate paths must point to the host-specific path. |
| Waiwei | `/opt/usr/local/waiwei` | `/usr/local/waiwei` | `waiwei-web` and `waiwei-puller` units run from here. |
| Xray | `/opt/usr/local/xray` | `/usr/local/xray` | `xray.service` uses binaries and data from here. |
| Immich runtime | `/opt/immich` | `/usr/local/immich` | Native deployment, not Docker. |
| Immich source checkout | `/opt/src/software/immich` | `/root/src/software/immich` | Uses the `deploy` branch. |
| Flutter SDK | `/opt/src/software/flutter` | `/root/src/software/flutter` | NAS wrapper sets `FLUTTER_ROOT`; generic/dev default is `/root/src/software/flutter`. |
| NVM / Node | `/opt/src/software/tools/nvm` | `/root/src/software/tools/nvm` | NAS `/root/.nvm` and `/home/tiger/.nvm` should point to the `/opt` location. |
| pgvector and source tools | `/opt/src/software` | `/root/src/software` | Dev source tools include `aarch64-linux-musl-cross`. |
| root home spillover | `/opt/user/root` | none; `/root` is real | NAS symlinks most `/root` dotfiles and `/root/src` here. |
| tiger home spillover | `/opt/user/tiger` | none; `/home/tiger` is real | NAS symlinks selected `/home/tiger` dotfiles here. |

NAS must not bind-mount `/opt/usr/local` back to `/usr/local`. Old paths such
as `/usr/local/blog`, `/usr/local/go`, `/usr/local/halo`, `/usr/local/tbox`,
`/usr/local/waiwei`, `/usr/local/xray`, and `/usr/local/bin` should disappear by
removing the bind mount or old symlink, not by deleting through `/usr/local`.
The collector NAS deployment script keeps migration logic for the historical
`/opt/auto_sync` and `/usr/local/*` layouts, but the final systemd and runtime
paths should be `/opt/usr/local/*`.

The dev collector deployment script should restore any accidental `/opt/user`
symlinks back to real local files/directories, move old `/opt/src/software` or
`/opt/software/src` content into `/root/src/software`, and rewrite stale service
or profile references back to dev paths.

## auto_sync Deployment Defaults

| Entry point | Default install dir | Override for NAS |
| --- | --- | --- |
| `scripts/deploy_local.sh` | `/usr/local/auto_sync` | `scripts/deploy_nas.sh` sets `/opt/usr/local/auto_sync`. |
| `scripts/deploy_nas.sh` | `/opt/usr/local/auto_sync` | The script rejects any other NAS install dir. |
| `auto_syncctl print-systemd` | `/usr/local/auto_sync` | Pass `--install-dir /opt/usr/local/auto_sync` for NAS. |
| `auto_syncctl deploy-nas` | `/opt/usr/local/auto_sync` | NAS-specific command default. |
| UI default for new Linux machines | `/usr/local/auto_sync` | Set NAS explicitly to `/opt/usr/local/auto_sync`. |

## Environment Files

| Host | File | Purpose |
| --- | --- | --- |
| NAS/dev/test | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | Domestic mirrors for Go, Node, npm, Python, Rust, and `JAVA_HOME` from apt OpenJDK. |
| NAS/test | `/etc/profile.d/opt-usr-local-path.sh` | Adds `/opt/usr/local/bin` and `/opt/usr/local/go/go1.25.1/bin` to `PATH`. |

Systemd unit files should use absolute real paths from the tables above.
Generated `auto_sync.service` comes from `auto_syncctl print-systemd`.
