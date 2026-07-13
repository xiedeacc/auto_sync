# Deployment Paths

This file records the intended real paths for dev and NAS. Keep collector
paths, deployment scripts, systemd units, and shell environment setup aligned
with this table.

## NAS

NAS has a small root disk. Large software trees and most user-home state live
under `/opt`.

| Component | Real path on NAS | Notes |
| --- | --- | --- |
| auto_sync | `/opt/usr/local/auto_sync` | Normal NAS deployment target. `scripts/deploy_nas.sh` must run from and deploy to this path. |
| rblog | `/opt/usr/local/blog` | Service units must use `/opt/usr/local/blog`, including `.backup-worktree`. |
| Go SDK | `/opt/usr/local/go/go1.25.1` | `/etc/profile.d/opt-usr-local-path.sh` adds this `bin` directory to `PATH`. |
| `/usr/local/bin` tools | `/opt/usr/local/bin` | Includes tools such as `buildifier`; do not recreate `/usr/local/bin` as a bind mount. |
| Halo | `/opt/usr/local/halo` | Runtime home is `/root/.halo` and `/root/.halo2`, both symlinked to `/opt/user/root`. |
| shadowsocks | `/opt/usr/local/shadowsocks` | Directory is supported for collected config/data/logs, but service startup may be disabled when xray owns the ports. |
| TBox | `/opt/usr/local/tbox` | `tbox_client.service` and logrotate paths must point here. |
| Waiwei | `/opt/usr/local/waiwei` | `waiwei-web` and `waiwei-puller` units run from here. |
| Xray | `/opt/usr/local/xray` | `xray.service` uses binaries and data from here. |
| Immich runtime | `/opt/immich` | Immich is deployed under `/opt`, not `/usr/local`. |
| Immich source checkout | `/opt/src/software/immich` | Uses the `deploy` branch and native deployment, not Docker. |
| Flutter SDK | `/opt/src/software/flutter` | Used by NAS web build. |
| NVM / Node | `/opt/src/software/tools/nvm` | `/root/.nvm` and `/home/tiger/.nvm` should point here on NAS. |
| pgvector and source tools | `/opt/src/software` | NAS source/tool checkout root. |
| root home spillover | `/opt/user/root` | Most `/root` dotfiles and `/root/src` are symlinked here. |
| tiger home spillover | `/opt/user/tiger` | Selected `/home/tiger` dotfiles are symlinked here. |

NAS must not bind-mount `/opt/usr/local` back to `/usr/local`. Old paths such
as `/usr/local/blog`, `/usr/local/go`, `/usr/local/halo`, `/usr/local/tbox`,
`/usr/local/waiwei`, `/usr/local/xray`, and `/usr/local/bin` should disappear by
removing the bind mount or old symlink, not by deleting through `/usr/local`.
The collector NAS deployment script keeps migration logic for the historical
`/opt/auto_sync` and `/usr/local/*` layouts, but the final systemd and runtime
paths should be `/opt/usr/local/*`.

## Dev

Dev has a large root SSD. It intentionally does not mirror NAS symlinks.

| Component | Real path on dev | Notes |
| --- | --- | --- |
| auto_sync | `/usr/local/auto_sync` | Dev deployment target and machine `install_dir`. |
| rblog | `/usr/local/blog` | Service units use the normal `/usr/local` layout on dev. |
| Go SDK | `/usr/local/go/go1.25.1` | GOPATH remains `/root/src/go`. |
| GOPATH | `/root/src/go` | Do not recreate `/root/go`. |
| buildifier | `/usr/local/bin/buildifier` | Dev uses the normal `/usr/local/bin`. |
| Halo | `/usr/local/halo` | Runs as root; runtime home is `/root/.halo` and `/root/.halo2` as real local paths. |
| shadowsocks | `/usr/local/shadowsocks` | Dev collector path stays under `/usr/local`. |
| TBox | `/usr/local/tbox` | Dev service units use `/usr/local/tbox`. |
| Waiwei | `/usr/local/waiwei` | Dev service units use `/usr/local/waiwei`. |
| Xray | `/usr/local/xray` | Dev service units use `/usr/local/xray`. |
| Immich runtime | `/usr/local/immich` | Dev does not use `/opt/immich`. |
| Immich source checkout | `/root/src/software/immich` | Native deployment, not Docker. |
| Flutter SDK | `/root/src/software/flutter` when installed for shared dev use | Do not leave `/root/flutter` symlinks. |
| NVM / Node | `/root/src/software/tools/nvm` | Dev should not use `/usr/local/src/software` or `/opt/src/software` for tools. |
| pgvector and source tools | `/root/src/software` | Includes toolchains such as `aarch64-linux-musl-cross`. |
| root home | `/root` | Real local path, not a symlink to `/opt/user/root`. |
| tiger home | `/home/tiger` | Real local path, not a symlink to `/opt/user/tiger`. |

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
