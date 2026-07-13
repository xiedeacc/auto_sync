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
| NVM root | `/opt/src/software/tools/nvm` | `/root/src/software/tools/nvm` | NAS `/root/.nvm` and `/home/tiger/.nvm` should point to the `/opt` location. |
| Node.js | `/opt/src/software/tools/nvm/versions/node/v24.18.0` | `/root/src/software/tools/nvm/versions/node/v24.18.0` | Installed by nvm; deployment scripts may fall back to latest Node 24. |
| npm | Node.js `bin/npm` under the nvm root | Node.js `bin/npm` under the nvm root | npm registry is configured by deployment scripts. |
| pnpm | Node.js/Corepack-managed binary under the nvm root | Node.js/Corepack-managed binary under the nvm root | Installed or activated after Node setup. |
| Python | system Python from apt, normally `/usr/bin/python3` | system Python from apt, normally `/usr/bin/python3` | Additional project virtualenvs stay inside their project directories. |
| pip config | `/etc/pip.conf` | `/etc/pip.conf` | Managed by collector deployment scripts. |
| uv config | environment variables in `/etc/profile.d/auto-sync-domestic-mirrors.sh` | environment variables in `/etc/profile.d/auto-sync-domestic-mirrors.sh` | No separate uv config file is required. |
| Rust toolchain | `/root/.cargo`, `/root/.rustup` symlinked to `/opt/user/root` | `/root/.cargo`, `/root/.rustup` real local paths | Installed by rustup; cargo registry mirror lives in `/root/.cargo/config.toml`. |
| Java / JDK | apt OpenJDK at `/usr/lib/jvm/java-21-openjdk-amd64` | apt OpenJDK at `/usr/lib/jvm/java-21-openjdk-amd64` | Do not recreate `/usr/local/java`; `JAVA_HOME` points to the apt JDK. |
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
| UI default for new Linux machines | `/usr/local/auto_sync` | Set NAS explicitly to `/opt/usr/local/auto_sync`. |

## Environment Files

| Host | File | Purpose |
| --- | --- | --- |
| NAS/dev/test | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | Domestic mirrors for Go, Node, npm, Python, Rust, and `JAVA_HOME` from apt OpenJDK. |
| NAS/test | `/etc/profile.d/opt-usr-local-path.sh` | Adds `/opt/usr/local/bin` and `/opt/usr/local/go/go1.25.1/bin` to `PATH`. |

## Source Matrix

| Tool | Setting | Value | Where it is managed |
| --- | --- | --- | --- |
| apt | Ubuntu archive mirror | `https://mirrors.cloud.tencent.com` | `conf/collector_deploy_dev.ps1`, `conf/collector_deploy_nas.ps1`, and `conf/collector_deploy_test.ps1` rewrite Ubuntu sources before package install; `scripts/deploy_local.sh` also rewrites official Ubuntu sources before installing auto_sync build deps. |
| Go modules | `GOPROXY` | `https://goproxy.cn,direct` | `/etc/profile.d/auto-sync-domestic-mirrors.sh`; also exported by `scripts/deploy_local.sh` during normal auto_sync deploys. |
| Go SDK tarball | download URL | first `https://mirrors.aliyun.com/golang/go1.25.1.linux-amd64.tar.gz`, fallback `https://go.dev/dl/go1.25.1.linux-amd64.tar.gz` | Collector deployment scripts. |
| nvm installer | source URL | `https://gitee.com/mirrors/nvm/raw/v0.40.3/install.sh` with `NVM_SOURCE=https://gitee.com/mirrors/nvm.git` | Collector deployment scripts. |
| Node.js downloads | `NVM_NODEJS_ORG_MIRROR` | `https://npmmirror.com/mirrors/node` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` |
| npm | `npm_config_registry` | `https://registry.npmmirror.com` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` and `npm config set registry`. |
| Corepack | `COREPACK_NPM_REGISTRY` | `https://registry.npmmirror.com` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` |
| pnpm | registry | `https://registry.npmmirror.com` | Collector deployment scripts run `pnpm config set registry`. |
| pip | `index-url` | `https://pypi.tuna.tsinghua.edu.cn/simple` | `/etc/pip.conf` |
| pip | `PIP_INDEX_URL` | `https://pypi.tuna.tsinghua.edu.cn/simple` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` |
| uv | `UV_DEFAULT_INDEX`, `UV_INDEX_URL` | `https://pypi.tuna.tsinghua.edu.cn/simple` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` |
| rustup | `RUSTUP_DIST_SERVER` | `https://rsproxy.cn` | `/etc/profile.d/auto-sync-domestic-mirrors.sh`; also exported by `scripts/deploy_local.sh` during normal auto_sync deploys. |
| rustup | `RUSTUP_UPDATE_ROOT` | `https://rsproxy.cn/rustup` | `/etc/profile.d/auto-sync-domestic-mirrors.sh`; also exported by `scripts/deploy_local.sh` during normal auto_sync deploys. |
| rustup installer | source URL | `https://rsproxy.cn/rustup-init.sh` | Collector deployment scripts and `scripts/deploy_local.sh` when Rust is missing. |
| Cargo | crates.io replacement | `sparse+https://rsproxy.cn/index/` | `/root/.cargo/config.toml`; `scripts/deploy_local.sh` creates this when no cargo config exists. |
| Flutter pub | `PUB_HOSTED_URL` | `https://pub.flutter-io.cn` | `scripts/deploy_local.sh` and `scripts/deploy_windows.ps1` during Flutter build. |
| Flutter storage | `FLUTTER_STORAGE_BASE_URL` | `https://storage.flutter-io.cn` | `scripts/deploy_local.sh` and `scripts/deploy_windows.ps1` during Flutter build. |
| Java | `JAVA_HOME` | `/usr/lib/jvm/java-21-openjdk-amd64` | `/etc/profile.d/auto-sync-domestic-mirrors.sh`; JDK is installed from apt. |

Systemd unit files should use absolute real paths from the tables above.
`scripts/deploy_local.sh` renders `auto_sync.service` directly from the selected
install directory; `scripts/deploy_nas.sh` supplies the NAS path.
