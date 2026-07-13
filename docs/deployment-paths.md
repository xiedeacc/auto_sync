# 部署路径说明

本文档记录 dev 和 nas 的预期真实路径。collector 路径、部署脚本、systemd 单元以及 shell 环境配置都需要和这里保持一致。

## 主机路径矩阵

nas 的根分区很小，所以大型软件目录和大部分用户 home 状态都放在 `/opt` 下。dev 的根分区是大 SSD，故意不复制 nas 的 symlink 布局。

| 组件 | nas 真实路径 | dev 真实路径 | 说明 |
| --- | --- | --- | --- |
| auto_sync | `/opt/usr/local/auto_sync` | `/usr/local/auto_sync` | • aws: 不作为常规 auto_sync 部署目标。<br>• dev: 部署并运行，使用 `scripts/deploy_local.sh --install-dir /usr/local/auto_sync`。<br>• nas: 部署并运行，使用 `scripts/deploy_nas.sh`。<br>• windows: 部署并运行，使用 `scripts/deploy_windows.ps1`。<br>• openwrt: 不作为常规 auto_sync 部署目标。 |
| rblog | `/opt/usr/local/blog` | `/usr/local/blog` | • aws: 启动 `rblog`、`rblog-backup.timer`，路径为 `/usr/local/blog`。<br>• dev: 启动 `rblog`、`rblog-backup.timer`，路径为 `/usr/local/blog`。<br>• nas: 启动 `rblog`、`rblog-backup.timer`，路径为 `/opt/usr/local/blog`；`.backup-worktree` 位于该目录下。<br>• windows: 不运行。<br>• openwrt: 不运行。 |
| go sdk | `/opt/usr/local/go/go1.25.1` | `/usr/local/go/go1.25.1` | • aws: 不由 collector 部署脚本管理。<br>• dev: 使用正常的 `/usr/local/go`。<br>• nas: 通过 `/etc/profile.d/opt-usr-local-path.sh` 把 go 加入 `PATH`。<br>• windows: 不使用该 Linux 路径。<br>• openwrt: 不使用该 Linux 路径。 |
| gopath | `/root/src/go`，通过 `/opt/user/root/src/go` symlink 指向 | `/root/src/go` | • aws: 不由 collector 部署脚本管理。<br>• dev: 使用 `/root/src/go`，不要重新创建 `/root/go`。<br>• nas: 使用 `/root/src/go`，实际通过 symlink 指到 `/opt/user/root/src/go`；不要重新创建 `/root/go`。<br>• windows: 不使用该 Linux 路径。<br>• openwrt: 不使用该 Linux 路径。 |
| bin | `/opt/usr/local/bin` | `/usr/local/bin` | • aws: 不由 collector 部署脚本管理。<br>• dev: 使用 `/usr/local/bin`。<br>• nas: 使用 `/opt/usr/local/bin`；不要把 `/usr/local/bin` 重新做成 bind mount。<br>• windows: 不使用该 Linux 路径。<br>• openwrt: 不使用该 Linux 路径。 |
| buildifier | `/opt/usr/local/bin/buildifier` | `/usr/local/bin/buildifier` | • aws: 不由 collector 部署脚本管理。<br>• dev: 由 collector 部署脚本安装到 `/usr/local/bin/buildifier`。<br>• nas: 由 collector 部署脚本安装到 `/opt/usr/local/bin/buildifier`。<br>• windows: 不使用该 Linux 路径。<br>• openwrt: 不使用该 Linux 路径。 |
| halo bin | `/opt/usr/local/halo` | `/usr/local/halo` | • aws: 不运行。<br>• dev: 启动 `halo2.service`，以 root 运行。<br>• nas: 启动 `halo2.service`，以 root 运行。<br>• windows: 不运行。<br>• openwrt: 不运行。 |
| halo home | `/root/.halo2` symlink 到 `/opt/user/root/.halo2` | `/root/.halo2` 真实本地路径 | • aws: 不使用。<br>• dev: `/root/.halo2` 是真实本地路径。<br>• nas: `/root/.halo2` symlink 到 `/opt/user/root/.halo2`，避免占用根分区。<br>• windows: 不使用该 Linux 路径。<br>• openwrt: 不使用。<br>• 备注: 旧 `.halo` 是 halo 1.x 遗留路径，不应再创建。 |
| shadowsocks | `/opt/usr/local/shadowsocks` | `/usr/local/shadowsocks` | • aws: enable/start `shadowsocks-rust`。<br>• dev: 保留文件但 disable/stop `shadowsocks` 和 `shadowsocks-rust`。<br>• nas: 保留文件但 disable/stop `shadowsocks` 和 `shadowsocks-rust`。<br>• windows: 不运行。<br>• openwrt: enable/start 非 rust 版 `shadowsocks`，并 disable/stop `shadowsocks-rust`。 |
| tbox | `/opt/usr/local/tbox` | `/usr/local/tbox` | • aws: disable/stop。<br>• dev: disable/stop。<br>• nas: enable/start `tbox_client.service` 和 `tbox-logrotate.timer`。<br>• windows: 不运行。<br>• openwrt: 不运行。 |
| waiwei | `/opt/usr/local/waiwei` | `/usr/local/waiwei` | • aws: disable/stop `waiwei-web`、`waiwei-puller`。<br>• dev: disable/stop `waiwei-web`、`waiwei-puller`。<br>• nas: disable/stop `waiwei-web`、`waiwei-puller`。<br>• windows: 不运行。<br>• openwrt: 不运行。<br>• 备注: 只有 `waiwei-web` 和 `waiwei-puller`，不存在单独的 `waiwei` systemd 服务；文件仅用于采集、备份或保留历史配置。 |
| xray | `/opt/usr/local/xray` | `/usr/local/xray` | • aws: disable/stop 独立 `xray` 服务或进程。<br>• dev: disable/stop 独立 `xray` 服务或进程。<br>• nas: disable/stop 独立 `xray` 服务或进程。<br>• windows: 不运行。<br>• openwrt: 独立 `xray` 不运行；`xray-plugin` 是 shadowsocks 链路插件，不属于独立 `xray`，不能被部署脚本杀掉。 |
| immich | `/opt/immich` | `/usr/local/immich` | • aws: 不运行。<br>• dev: 启动 `immich`、`immich-ml`，路径为 `/usr/local/immich`。<br>• nas: 启动 `immich`、`immich-ml`，路径为 `/opt/immich`。<br>• windows: 不运行。<br>• openwrt: 不运行。 |
| immich 源码 | `/opt/src/software/immich` | `/root/src/software/immich` | • aws: 不使用。<br>• dev: `/root/src/software/immich`，使用 `deploy` 分支。<br>• nas: `/opt/src/software/immich`，使用 `deploy` 分支。<br>• windows: 不使用该 Linux 路径。<br>• openwrt: 不使用。 |
| gitlab | 导入的 nas ZFS 池上的 `/zfs/gitlab_data` | 普通目录 `/zfs/gitlab_data` | • aws: 不运行 gitlab。<br>• dev: 启动 gitlab，`/zfs/gitlab_data` 是普通目录。<br>• nas: 启动 gitlab，`/zfs/gitlab_data` 位于导入的 nas ZFS 池。<br>• windows: 不运行 gitlab。<br>• openwrt: 不运行 gitlab。 |
| 数据库备份 | `/zfs/backup/pg_backup` 和 `/zfs/backup/mysql_backup` | `/zfs/backup/pg_backup` 和 `/zfs/backup/mysql_backup` | • aws: 不由这套 nas/dev 备份 cron 管理。<br>• dev: 每周 dump 到 `/zfs/backup/pg_backup` 和 `/zfs/backup/mysql_backup`，并清理超过 7 天的文件。<br>• nas: 每周 dump 到 `/zfs/backup/pg_backup` 和 `/zfs/backup/mysql_backup`，恢复前唤醒 `/zfs`，用完后可让磁盘 standby。<br>• windows: 不运行该 Linux cron。<br>• openwrt: 不运行该 Linux cron。 |
| 静态站点根目录 | `/opt/www/gitlab_cleaner`、`/opt/www/unlock-music` | `/opt/www/gitlab_cleaner`、`/opt/www/unlock-music` | • aws: 不由 nas/dev collector 默认路径管理。<br>• dev: collector 部署脚本只补充这些平台默认路径；coverage 不再由脚本自动创建或采集。<br>• nas: collector 部署脚本只补充这些平台默认路径。<br>• windows: 不使用该 Linux 路径。<br>• openwrt: 不使用该 Linux 路径。 |
| flutter sdk | `/opt/src/software/flutter` | `/root/src/software/flutter` | • aws: 不安装。<br>• dev: `/root/src/software/flutter`。<br>• nas: `/opt/src/software/flutter`，wrapper 设置 `FLUTTER_ROOT`。<br>• windows: 使用 Windows 本机 flutter/构建环境，不使用该 Linux 路径。<br>• openwrt: 不安装。 |
| nvm root | `/opt/src/software/tools/nvm` | `/root/src/software/tools/nvm` | • aws: 不由 collector 部署脚本管理。<br>• dev: `/root/src/software/tools/nvm`，作为 `NVM_DIR`。<br>• nas: `/opt/src/software/tools/nvm`，作为 `NVM_DIR`；不要重建 tiger 用户 nvm/npm 状态。<br>• windows: 不使用该 Linux 路径。<br>• openwrt: 不使用该 Linux 路径。 |
| node.js | `/opt/src/software/tools/nvm/versions/node/v24.18.0` | `/root/src/software/tools/nvm/versions/node/v24.18.0` | • aws: 不由 collector 部署脚本管理。<br>• dev: 由 nvm 安装，路径在 `/root/src/software/tools/nvm` 下；可回退到最新 Node 24。<br>• nas: 由 nvm 安装，路径在 `/opt/src/software/tools/nvm` 下；可回退到最新 Node 24。<br>• windows: 使用 Windows 本机 Node，不使用该 Linux 路径。<br>• openwrt: 不使用该 Linux 路径。 |
| npm | nvm root 下的 node.js `bin/npm` | nvm root 下的 node.js `bin/npm` | • aws: 不由 collector 部署脚本管理。<br>• dev: 使用 nvm root 下的 `npm`，registry 由部署脚本配置。<br>• nas: 使用 nvm root 下的 `npm`，registry 由部署脚本配置。<br>• windows: 使用 Windows 本机 npm。<br>• openwrt: 不使用该 Linux 路径。 |
| pnpm | nvm root 下由 node.js/Corepack 管理的二进制 | nvm root 下由 node.js/Corepack 管理的二进制 | • aws: 不由 collector 部署脚本管理。<br>• dev: Node 设置完成后安装或激活。<br>• nas: Node 设置完成后安装或激活。<br>• windows: 使用 Windows 本机 pnpm/Corepack。<br>• openwrt: 不使用该 Linux 路径。 |
| python | apt 的系统 python，通常是 `/usr/bin/python3` | apt 的系统 python，通常是 `/usr/bin/python3` | • aws: 不由这套 nas/dev collector 脚本管理。<br>• dev: 使用 apt 系统 python；项目 virtualenv 留在各自项目目录内。<br>• nas: 使用 apt 系统 python；项目 virtualenv 留在各自项目目录内。<br>• windows: 使用 Windows 本机 python。<br>• openwrt: 不使用该 Linux 路径。 |
| pip | `/etc/pip.conf` | `/etc/pip.conf` | • aws: 不由这套 nas/dev collector 脚本管理。<br>• dev: collector 部署脚本管理 `/etc/pip.conf`。<br>• nas: collector 部署脚本管理 `/etc/pip.conf`。<br>• windows: 不使用该 Linux 配置文件。<br>• openwrt: 不使用该 Linux 配置文件。 |
| uv | `/etc/profile.d/auto-sync-domestic-mirrors.sh` 中的环境变量 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` 中的环境变量 | • aws: 不由这套 nas/dev collector 脚本管理。<br>• dev: 通过 `/etc/profile.d/auto-sync-domestic-mirrors.sh` 中的环境变量配置。<br>• nas: 通过 `/etc/profile.d/auto-sync-domestic-mirrors.sh` 中的环境变量配置。<br>• windows: 不使用该 Linux 配置文件。<br>• openwrt: 不使用该 Linux 配置文件。 |
| rust | `/root/.cargo`、`/root/.rustup` symlink 到 `/opt/user/root` | `/root/.cargo`、`/root/.rustup` 真实本地路径 | • aws: 不由这套 nas/dev collector 脚本管理。<br>• dev: `/root/.cargo`、`/root/.rustup` 是真实本地路径；Cargo registry 镜像配置位于 `/root/.cargo/config.toml`。<br>• nas: `/root/.cargo`、`/root/.rustup` symlink 到 `/opt/user/root`，避免占用根分区。<br>• windows: 使用 Windows 本机 rust 工具链。<br>• openwrt: 不使用该 Linux 路径。 |
| java | apt openjdk，路径 `/usr/lib/jvm/java-21-openjdk-amd64` | apt openjdk，路径 `/usr/lib/jvm/java-21-openjdk-amd64` | • aws: 不由这套 nas/dev collector 脚本管理。<br>• dev: 使用 apt openjdk，`JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64`。<br>• nas: 使用 apt openjdk，`JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64`。<br>• windows: 使用 Windows 本机 JDK；不要从该表推导 Windows 路径。<br>• openwrt: 不使用该 Linux 路径。 |
| pgvector | `/opt/src/software` | `/root/src/software` | • aws: 不由这套 nas/dev collector 脚本管理。<br>• dev: 使用 `/root/src/software`，包含 `aarch64-linux-musl-cross` 等源码工具。<br>• nas: 使用 `/opt/src/software`，避免占用根分区。<br>• windows: 不使用该 Linux 路径。<br>• openwrt: 不使用该 Linux 路径。 |
| root home | `/opt/user/root` | 无；`/root` 是真实路径 | • aws: 不使用 nas 的 `/opt/user/root` 布局。<br>• dev: `/root` 是真实路径，不创建 `/opt/user/root`。<br>• nas: `/root` 下除 `/root/.ssh` 外的所有子项 symlink 到 `/opt/user/root`；`/root/.ssh` 保持真实目录以保证 SSH 登录安全。<br>• windows: 不使用该 Linux 路径。<br>• openwrt: 不使用该 Linux 路径。 |
| tiger home | `/opt/user/tiger` | 无；`/home/tiger` 是真实路径 | • aws: 不使用 nas 的 `/opt/user/tiger` 布局。<br>• dev: `/home/tiger` 是真实路径，不创建 `/opt/user/tiger`。<br>• nas: 选定的 `/home/tiger` dotfile symlink 到 `/opt/user/tiger`。<br>• windows: 不使用该 Linux 路径。<br>• openwrt: 不使用该 Linux 路径。 |

nas 不允许把 `/opt/usr/local` bind-mount 回 `/usr/local`。旧路径如 `/usr/local/blog`、`/usr/local/go`、`/usr/local/halo`、`/usr/local/tbox`、`/usr/local/waiwei`、`/usr/local/xray`、`/usr/local/bin` 应通过移除 bind mount 或旧 symlink 消失，不能通过 `/usr/local` 删除真实数据。collector 的 nas 部署脚本保留了对历史 `/opt/auto_sync` 和 `/usr/local/*` 布局的迁移逻辑，但最终 systemd 和运行时路径都应是 `/opt/usr/local/*`。

dev 的 collector 部署脚本应将意外出现的 `/opt/user` symlink 恢复为真实本地文件/目录，将旧的 `/opt/src/software` 或 `/opt/software/src` 内容迁移到 `/root/src/software`，并把陈旧的 service/profile 引用改回 dev 路径。

## auto_sync 部署默认值

| 入口 | 默认安装目录 | nas 覆盖值 |
| --- | --- | --- |
| `scripts/deploy_local.sh` | `/usr/local/auto_sync` | 只用于 dev；`scripts/deploy_nas.sh` 设置为 `/opt/usr/local/auto_sync`。 |
| `scripts/deploy_nas.sh` | `/opt/usr/local/auto_sync` | 脚本会拒绝任何其他 nas 安装目录。 |
| UI 新增 Linux 机器默认值 | `/usr/local/auto_sync` | nas 必须显式设置为 `/opt/usr/local/auto_sync`。 |

## 环境文件

| 主机 | 文件 | 作用 |
| --- | --- | --- |
| nas/dev | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | go、Node、npm、python、rust 国内源，以及 apt openjdk 的 `JAVA_HOME`。 |
| nas | `/etc/profile.d/opt-usr-local-path.sh` | 将 `/opt/usr/local/bin` 和 `/opt/usr/local/go/go1.25.1/bin` 加入 `PATH`。 |

## 托管配置文件

这些文件由部署脚本创建或修改。新增主机级配置时，需要让本列表和 `conf/collector_deploy_*.ps1`、`scripts/deploy_local.sh`、`scripts/deploy_nas.sh` 保持同步。

| 文件 | 主机 | 管理方 | 作用 |
| --- | --- | --- | --- |
| `/etc/apt/sources.list.d/ubuntu.sources` | nas/dev | collector 部署脚本和 `scripts/deploy_local.sh` | 存在时将官方 Ubuntu archive/security URL 改写为 `https://mirrors.cloud.tencent.com`。 |
| `/etc/apt/sources.list` | nas/dev | `scripts/deploy_local.sh` | 存在旧格式源时，将官方 Ubuntu archive/security URL 改写为 `https://mirrors.cloud.tencent.com`。 |
| `/etc/profile.d/auto-sync-domestic-mirrors.sh` | nas/dev | collector 部署脚本 | 持久化国内源环境变量，以及 apt openjdk 的 `JAVA_HOME`。 |
| `/etc/profile.d/opt-usr-local-path.sh` | nas | collector 部署脚本 | 不 bind-mount `/usr/local`，只把 nas 的 `/opt/usr/local/bin` 和 `/opt/usr/local/go/go1.25.1/bin` 加入 `PATH`。 |
| `/etc/pip.conf` | nas/dev | collector 部署脚本 | 将全局 pip index 设置为清华 PyPI 镜像。 |
| `/root/.cargo/config.toml` | nas/dev | collector 部署脚本和 `scripts/deploy_local.sh` | 将 crates.io 替换为 rsproxy sparse registry，并启用 git CLI fetch。 |
| `/etc/systemd/system/auto_sync.service` | nas/dev | nas 上由 `scripts/deploy_nas.sh` 调用 `scripts/deploy_local.sh` 生成 | 从对应主机的安装目录启动统一的 `auto_sync` 进程。 |
| `/etc/systemd/coredump.conf` | nas/dev | collector 部署脚本 | 启用外部无限大小 coredump 存储。 |
| `/etc/security/limits.conf` | nas/dev | collector 部署脚本 | 追加无限 core size 限制。 |
| `/etc/sysctl.conf` | nas/dev | collector 部署脚本 | 追加 coredump pattern 并重新加载 sysctl。 |
| `/etc/hosts` | nas/dev | collector 部署脚本 | 确保脚本管理的所有站点域名都指向本机 `127.0.0.1`，并移除重复的陈旧项。 |
| `/etc/fstab` | nas/dev | collector 部署脚本 | 禁用 swap 时注释 `/swap.img` 条目。 |
| `/etc/ssh/sshd_config` | nas/dev | collector 部署脚本 | bootstrap 主机时强制项目 SSH 策略。 |

nginx vhost、gitlab、MySQL、PostgreSQL、immich、halo、tbox、waiwei、xray、rblog、logrotate 等服务专用文件，会从 collector share 复制到它们正常的系统位置。它们应和对应的 collect 路径保持一致，不要在脚本里临时散落生成。

## 托管环境变量

java 只使用 `JAVA_HOME`；不要设置全局 `CLASSPATH`/`CLASS_PATH`。collector 部署脚本会显式清理 root 和 tiger profile 中陈旧的 `CLASSPATH` 以及旧 `/usr/local/java` 启动项，所以每个 java 项目自行负责自己的 classpath。

| 变量 | 值 | 作用域 | 管理方 | 说明 |
| --- | --- | --- | --- | --- |
| `GOPROXY` | `https://goproxy.cn,direct` | nas/dev 持久化；普通 Linux auto_sync 部署时进程内生效 | `/etc/profile.d/auto-sync-domestic-mirrors.sh`、`scripts/deploy_local.sh` | go module 镜像。 |
| `NVM_NODEJS_ORG_MIRROR` | `https://npmmirror.com/mirrors/node` | nas/dev 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | nvm 的 Node 二进制下载镜像。 |
| `npm_config_registry` | `https://registry.npmmirror.com` | nas/dev 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh`；也由 `npm config set registry` 写入 | npm registry 镜像。 |
| `COREPACK_NPM_REGISTRY` | `https://registry.npmmirror.com` | nas/dev 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | Corepack package registry。 |
| `PIP_INDEX_URL` | `https://pypi.tuna.tsinghua.edu.cn/simple` | nas/dev 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | python package 镜像；`/etc/pip.conf` 也携带同样 index。 |
| `UV_DEFAULT_INDEX` | `https://pypi.tuna.tsinghua.edu.cn/simple` | nas/dev 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | uv 默认 package index。 |
| `UV_INDEX_URL` | `https://pypi.tuna.tsinghua.edu.cn/simple` | nas/dev 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | uv package index URL。 |
| `RUSTUP_DIST_SERVER` | `https://rsproxy.cn` | nas/dev 持久化；普通 Linux auto_sync 部署时进程内生效 | `/etc/profile.d/auto-sync-domestic-mirrors.sh`、`scripts/deploy_local.sh` | rust distribution 镜像。 |
| `RUSTUP_UPDATE_ROOT` | `https://rsproxy.cn/rustup` | nas/dev 持久化；普通 Linux auto_sync 部署时进程内生效 | `/etc/profile.d/auto-sync-domestic-mirrors.sh`、`scripts/deploy_local.sh` | rustup metadata 镜像。 |
| `JAVA_HOME` | `/usr/lib/jvm/java-21-openjdk-amd64` | nas/dev 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | apt openjdk 21。没有 `/usr/local/java` symlink，也没有全局 `CLASSPATH`。 |
| `PATH` | 前置 `$JAVA_HOME/bin` | nas/dev 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | 让 apt openjdk 工具可见。 |
| `PATH` | 存在时前置 `/opt/usr/local/bin` 和 `/opt/usr/local/go/go1.25.1/bin` | nas 持久化 | `/etc/profile.d/opt-usr-local-path.sh` | 仅 nas 需要，因为 `/usr/local` 软件放在 `/opt/usr/local` 下。 |
| `GOPATH` | `/root/src/go` | collector 部署过程中进程内生效 | collector 部署脚本 | 安装 go 工具时使用；不要重新创建 `/root/go`。 |
| `GOBIN` | `$GOPATH/bin` | collector 部署过程中进程内生效 | collector 部署脚本 | go 工具安装输出目录。 |
| `NVM_DIR` | nas: `/opt/src/software/tools/nvm`；dev: `/root/src/software/tools/nvm` | collector 部署过程中进程内生效 | collector 部署脚本 | nvm 安装根目录。 |
| `PUB_HOSTED_URL` | `https://pub.flutter-io.cn` | flutter 构建过程中进程内生效 | `scripts/deploy_local.sh`、`scripts/deploy_windows.ps1` | flutter/Dart package 镜像。 |
| `FLUTTER_STORAGE_BASE_URL` | `https://storage.flutter-io.cn` | flutter 构建过程中进程内生效 | `scripts/deploy_local.sh`、`scripts/deploy_windows.ps1` | flutter artifact 镜像。 |
| `CARGO_REGISTRIES_CRATES_IO_PROTOCOL` | `sparse` | 普通 Linux auto_sync 部署过程中进程内生效 | `scripts/deploy_local.sh` | 强制 Cargo sparse 协议；实际 registry replacement 在 `/root/.cargo/config.toml`。 |
| `RUSTUP_INIT_URL` | 未设置时默认为 `https://rsproxy.cn/rustup-init.sh` | 普通 Linux auto_sync 部署的进程内覆盖项 | `scripts/deploy_local.sh` | 可选 rustup installer URL 覆盖项。 |

## 源矩阵

| 工具 | 设置项 | 值 | 管理位置 |
| --- | --- | --- | --- |
| apt | Ubuntu archive 镜像 | `https://mirrors.cloud.tencent.com` | `conf/collector_deploy_dev.ps1`、`conf/collector_deploy_nas.ps1` 在安装包前改写 Ubuntu sources；`scripts/deploy_local.sh` 安装 auto_sync 构建依赖前也会改写官方 Ubuntu sources。 |
| go modules | `GOPROXY` | `https://goproxy.cn,direct` | `/etc/profile.d/auto-sync-domestic-mirrors.sh`；普通 auto_sync Linux 部署时也由 `scripts/deploy_local.sh` export。 |
| go SDK tarball | 下载 URL | 优先 `https://mirrors.aliyun.com/golang/go1.25.1.linux-amd64.tar.gz`，回退 `https://go.dev/dl/go1.25.1.linux-amd64.tar.gz` | collector 部署脚本。 |
| nvm installer | source URL | `https://gitee.com/mirrors/nvm/raw/v0.40.3/install.sh`，并设置 `NVM_SOURCE=https://gitee.com/mirrors/nvm.git` | collector 部署脚本。 |
| node.js downloads | `NVM_NODEJS_ORG_MIRROR` | `https://npmmirror.com/mirrors/node` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` |
| npm | `npm_config_registry` | `https://registry.npmmirror.com` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` 和 `npm config set registry`。 |
| Corepack | `COREPACK_NPM_REGISTRY` | `https://registry.npmmirror.com` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` |
| pnpm | registry | `https://registry.npmmirror.com` | collector 部署脚本执行 `pnpm config set registry`。 |
| pip | `index-url` | `https://pypi.tuna.tsinghua.edu.cn/simple` | `/etc/pip.conf` |
| pip | `PIP_INDEX_URL` | `https://pypi.tuna.tsinghua.edu.cn/simple` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` |
| uv | `UV_DEFAULT_INDEX`、`UV_INDEX_URL` | `https://pypi.tuna.tsinghua.edu.cn/simple` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` |
| rustup | `RUSTUP_DIST_SERVER` | `https://rsproxy.cn` | `/etc/profile.d/auto-sync-domestic-mirrors.sh`；普通 auto_sync Linux 部署时也由 `scripts/deploy_local.sh` export。 |
| rustup | `RUSTUP_UPDATE_ROOT` | `https://rsproxy.cn/rustup` | `/etc/profile.d/auto-sync-domestic-mirrors.sh`；普通 auto_sync Linux 部署时也由 `scripts/deploy_local.sh` export。 |
| rustup installer | source URL | `https://rsproxy.cn/rustup-init.sh` | collector 部署脚本，以及 rust 缺失时的 `scripts/deploy_local.sh`。 |
| Cargo | crates.io replacement | `sparse+https://rsproxy.cn/index/` | `/root/.cargo/config.toml`；没有 cargo config 时，`scripts/deploy_local.sh` 会创建。 |
| flutter pub | `PUB_HOSTED_URL` | `https://pub.flutter-io.cn` | `scripts/deploy_local.sh` 和 `scripts/deploy_windows.ps1` 在 flutter 构建过程中设置。 |
| flutter storage | `FLUTTER_STORAGE_BASE_URL` | `https://storage.flutter-io.cn` | `scripts/deploy_local.sh` 和 `scripts/deploy_windows.ps1` 在 flutter 构建过程中设置。 |
| java | `JAVA_HOME` | `/usr/lib/jvm/java-21-openjdk-amd64` | `/etc/profile.d/auto-sync-domestic-mirrors.sh`；JDK 从 apt 安装。 |

systemd unit 文件应使用上表中的绝对真实路径。`scripts/deploy_local.sh` 会根据选定安装目录直接渲染 `auto_sync.service`；`scripts/deploy_nas.sh` 提供 nas 路径。
