# 部署路径说明

本文档记录 dev 和 NAS 的预期真实路径。collector 路径、部署脚本、systemd 单元以及 shell 环境配置都需要和这里保持一致。

## 主机路径矩阵

NAS 的根分区很小，所以大型软件目录和大部分用户 home 状态都放在 `/opt` 下。dev 的根分区是大 SSD，故意不复制 NAS 的 symlink 布局。

| 组件 | NAS 真实路径 | Dev 真实路径 | 说明 |
| --- | --- | --- | --- |
| auto_sync | `/opt/usr/local/auto_sync` | `/usr/local/auto_sync` | 只在 Windows、NAS、dev 部署和运行；NAS 使用 `scripts/deploy_nas.sh`，dev 使用 `scripts/deploy_local.sh --install-dir /usr/local/auto_sync`；AWS、OpenWrt、test 不作为常规 auto_sync 部署目标。 |
| rblog | `/opt/usr/local/blog` | `/usr/local/blog` | NAS 和 dev 启动 `rblog`、`rblog-backup.timer`；AWS 也启动 `rblog`、`rblog-backup.timer` 并使用 `/usr/local/blog`；OpenWrt 不运行。service unit 必须使用对应主机路径。NAS 的 `.backup-worktree` 位于 `/opt/usr/local/blog` 下。 |
| Go SDK | `/opt/usr/local/go/go1.25.1` | `/usr/local/go/go1.25.1` | NAS 通过 `/etc/profile.d/opt-usr-local-path.sh` 把 Go 加入 `PATH`；dev 使用正常的 `/usr/local/go`。 |
| GOPATH | `/root/src/go`，通过 `/opt/user/root/src/go` symlink 指向 | `/root/src/go` | 不要重新创建 `/root/go`。 |
| bin 工具 | `/opt/usr/local/bin` | `/usr/local/bin` | 包含 `buildifier` 等工具。不要在 NAS 上把 `/usr/local/bin` 重新做成 bind mount。 |
| buildifier | `/opt/usr/local/bin/buildifier` | `/usr/local/bin/buildifier` | 由 collector 部署脚本安装。 |
| Halo 安装目录 | `/opt/usr/local/halo` | `/usr/local/halo` | NAS 和 dev 启动 `halo2.service` 并以 root 运行；test 按 NAS 逻辑测试；AWS、OpenWrt 不运行。 |
| Halo 运行时 home | `/root/.halo2` symlink 到 `/opt/user/root/.halo2` | `/root/.halo2` 真实本地路径 | Halo 2 使用 `.halo2`；旧的 `.halo` 是 Halo 1.x 遗留路径，不应再创建。NAS 将 root home 状态放到根分区之外；dev 保持真实本地路径。 |
| shadowsocks | `/opt/usr/local/shadowsocks` | `/usr/local/shadowsocks` | NAS/dev/test 保留文件但默认 disable/stop `shadowsocks` 和 `shadowsocks-rust`；AWS 默认 enable/start `shadowsocks-rust`；OpenWrt 默认 enable/start 非 rust 版 `shadowsocks`，并 disable/stop `shadowsocks-rust`。 |
| TBox | `/opt/usr/local/tbox` | `/usr/local/tbox` | 只在 NAS 上 enable/start `tbox_client.service` 和 `tbox-logrotate.timer`；dev/test/AWS/OpenWrt 不运行，脚本如果遇到相关 unit 必须 disable/stop。 |
| Waiwei | `/opt/usr/local/waiwei` | `/usr/local/waiwei` | 所有 host 都 disable/stop：`waiwei`、`waiwei-web`、`waiwei-puller` 不允许运行；文件仅用于采集、备份或保留历史配置。 |
| Xray | `/opt/usr/local/xray` | `/usr/local/xray` | 所有 host 都 disable/stop 独立的 `xray` 服务或进程；OpenWrt 上 `xray-plugin` 是 shadowsocks 链路插件，不属于这里的 `xray` 服务，不能被部署脚本杀掉。文件仅用于采集、备份或保留历史配置。 |
| Immich 运行目录 | `/opt/immich` | `/usr/local/immich` | NAS 和 dev 启动 `immich`、`immich-ml`；test 按 NAS 逻辑测试；AWS、OpenWrt 不运行。原生部署，不使用 Docker。 |
| Immich 源码 checkout | `/opt/src/software/immich` | `/root/src/software/immich` | 使用 `deploy` 分支。 |
| GitLab 仓库数据 | 导入的 NAS ZFS 池上的 `/zfs/gitlab_data` | 普通目录 `/zfs/gitlab_data` | NAS 和 dev 启动 GitLab；test 按普通目录测试；AWS、OpenWrt 不运行。collector 部署脚本将 GitLab 仓库和 LFS 存储配置到这里。 |
| 数据库备份 | `/zfs/backup/pg_backup` 和 `/zfs/backup/mysql_backup` | `/zfs/backup/pg_backup` 和 `/zfs/backup/mysql_backup` | 备份 cron 脚本将每周 dump 放在这里，并清理超过 7 天的文件。NAS 在恢复前会唤醒 `/zfs`，用完后可让磁盘 standby。 |
| 静态站点根目录 | `/opt/www/gitlab_cleaner`、`/opt/www/unlock-music` | `/opt/www/gitlab_cleaner`、`/opt/www/unlock-music` | collector 部署脚本只补充这些平台默认路径；coverage 不再由部署脚本自动创建或采集。 |
| Flutter SDK | `/opt/src/software/flutter` | `/root/src/software/flutter` | NAS wrapper 设置 `FLUTTER_ROOT`；通用/dev 默认是 `/root/src/software/flutter`。 |
| NVM root | `/opt/src/software/tools/nvm` | `/root/src/software/tools/nvm` | 部署时作为 `NVM_DIR` 使用；NAS 的 `/root/.nvm` 本身和其他 root home 内容一样放在 `/opt/user/root` 下。不要重建 tiger 用户的 nvm/npm 状态。 |
| Node.js | `/opt/src/software/tools/nvm/versions/node/v24.18.0` | `/root/src/software/tools/nvm/versions/node/v24.18.0` | 由 nvm 安装；部署脚本可回退到最新 Node 24。 |
| npm | nvm root 下的 Node.js `bin/npm` | nvm root 下的 Node.js `bin/npm` | npm registry 由部署脚本配置。 |
| pnpm | nvm root 下由 Node.js/Corepack 管理的二进制 | nvm root 下由 Node.js/Corepack 管理的二进制 | Node 设置完成后安装或激活。 |
| Python | apt 的系统 Python，通常是 `/usr/bin/python3` | apt 的系统 Python，通常是 `/usr/bin/python3` | 额外的项目 virtualenv 留在各自项目目录内。 |
| pip 配置 | `/etc/pip.conf` | `/etc/pip.conf` | 由 collector 部署脚本管理。 |
| uv 配置 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` 中的环境变量 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` 中的环境变量 | 不需要单独的 uv 配置文件。 |
| Rust 工具链 | `/root/.cargo`、`/root/.rustup` symlink 到 `/opt/user/root` | `/root/.cargo`、`/root/.rustup` 真实本地路径 | 由 rustup 安装；Cargo registry 镜像配置位于 `/root/.cargo/config.toml`。 |
| Java / JDK | apt OpenJDK，路径 `/usr/lib/jvm/java-21-openjdk-amd64` | apt OpenJDK，路径 `/usr/lib/jvm/java-21-openjdk-amd64` | 不要重新创建 `/usr/local/java`；`JAVA_HOME` 指向 apt JDK。 |
| pgvector 和源码工具 | `/opt/src/software` | `/root/src/software` | dev 的源码工具包含 `aarch64-linux-musl-cross`。 |
| root home 外置目录 | `/opt/user/root` | 无；`/root` 是真实路径 | NAS 将 `/root` 下除 `/root/.ssh` 外的所有子项 symlink 到这里；`/root/.ssh` 保持真实目录以保证 SSH 登录安全。 |
| tiger home 外置目录 | `/opt/user/tiger` | 无；`/home/tiger` 是真实路径 | NAS 将选定的 `/home/tiger` dotfile symlink 到这里。 |

NAS 不允许把 `/opt/usr/local` bind-mount 回 `/usr/local`。旧路径如 `/usr/local/blog`、`/usr/local/go`、`/usr/local/halo`、`/usr/local/tbox`、`/usr/local/waiwei`、`/usr/local/xray`、`/usr/local/bin` 应通过移除 bind mount 或旧 symlink 消失，不能通过 `/usr/local` 删除真实数据。collector 的 NAS 部署脚本保留了对历史 `/opt/auto_sync` 和 `/usr/local/*` 布局的迁移逻辑，但最终 systemd 和运行时路径都应是 `/opt/usr/local/*`。

dev 的 collector 部署脚本应将意外出现的 `/opt/user` symlink 恢复为真实本地文件/目录，将旧的 `/opt/src/software` 或 `/opt/software/src` 内容迁移到 `/root/src/software`，并把陈旧的 service/profile 引用改回 dev 路径。

## auto_sync 部署默认值

| 入口 | 默认安装目录 | NAS 覆盖值 |
| --- | --- | --- |
| `scripts/deploy_local.sh` | `/usr/local/auto_sync` | 只用于 dev；`scripts/deploy_nas.sh` 设置为 `/opt/usr/local/auto_sync`。 |
| `scripts/deploy_nas.sh` | `/opt/usr/local/auto_sync` | 脚本会拒绝任何其他 NAS 安装目录。 |
| UI 新增 Linux 机器默认值 | `/usr/local/auto_sync` | NAS 必须显式设置为 `/opt/usr/local/auto_sync`。 |

## 环境文件

| 主机 | 文件 | 作用 |
| --- | --- | --- |
| NAS/dev/test | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | Go、Node、npm、Python、Rust 国内源，以及 apt OpenJDK 的 `JAVA_HOME`。 |
| NAS/test | `/etc/profile.d/opt-usr-local-path.sh` | 将 `/opt/usr/local/bin` 和 `/opt/usr/local/go/go1.25.1/bin` 加入 `PATH`。 |

## 托管配置文件

这些文件由部署脚本创建或修改。新增主机级配置时，需要让本列表和 `conf/collector_deploy_*.ps1`、`scripts/deploy_local.sh`、`scripts/deploy_nas.sh` 保持同步。

| 文件 | 主机 | 管理方 | 作用 |
| --- | --- | --- | --- |
| `/etc/apt/sources.list.d/ubuntu.sources` | NAS/dev/test | collector 部署脚本和 `scripts/deploy_local.sh` | 存在时将官方 Ubuntu archive/security URL 改写为 `https://mirrors.cloud.tencent.com`。 |
| `/etc/apt/sources.list` | NAS/dev/test | `scripts/deploy_local.sh` | 存在旧格式源时，将官方 Ubuntu archive/security URL 改写为 `https://mirrors.cloud.tencent.com`。 |
| `/etc/profile.d/auto-sync-domestic-mirrors.sh` | NAS/dev/test | collector 部署脚本 | 持久化国内源环境变量，以及 apt OpenJDK 的 `JAVA_HOME`。 |
| `/etc/profile.d/opt-usr-local-path.sh` | NAS/test | collector 部署脚本 | 不 bind-mount `/usr/local`，只把 NAS 的 `/opt/usr/local/bin` 和 `/opt/usr/local/go/go1.25.1/bin` 加入 `PATH`。 |
| `/etc/pip.conf` | NAS/dev/test | collector 部署脚本 | 将全局 pip index 设置为清华 PyPI 镜像。 |
| `/root/.cargo/config.toml` | NAS/dev/test | collector 部署脚本和 `scripts/deploy_local.sh` | 将 crates.io 替换为 rsproxy sparse registry，并启用 git CLI fetch。 |
| `/etc/systemd/system/auto_sync.service` | NAS/dev | NAS 上由 `scripts/deploy_nas.sh` 调用 `scripts/deploy_local.sh` 生成 | 从对应主机的安装目录启动统一的 `auto_sync` 进程。 |
| `/etc/systemd/coredump.conf` | NAS/dev/test | collector 部署脚本 | 启用外部无限大小 coredump 存储。 |
| `/etc/security/limits.conf` | NAS/dev/test | collector 部署脚本 | 追加无限 core size 限制。 |
| `/etc/sysctl.conf` | NAS/dev/test | collector 部署脚本 | 追加 coredump pattern 并重新加载 sysctl。 |
| `/etc/hosts` | NAS/dev/test | collector 部署脚本 | 确保脚本管理的所有站点域名都指向本机 `127.0.0.1`，并移除重复的陈旧项。 |
| `/etc/fstab` | NAS/dev/test | collector 部署脚本 | 禁用 swap 时注释 `/swap.img` 条目。 |
| `/etc/ssh/sshd_config` | NAS/dev/test | collector 部署脚本 | bootstrap 主机时强制项目 SSH 策略。 |

nginx vhost、GitLab、MySQL、PostgreSQL、Immich、Halo、TBox、Waiwei、Xray、rblog、logrotate 等服务专用文件，会从 collector share 复制到它们正常的系统位置。它们应和对应的 collect 路径保持一致，不要在脚本里临时散落生成。

## 托管环境变量

Java 只使用 `JAVA_HOME`；不要设置全局 `CLASSPATH`/`CLASS_PATH`。collector 部署脚本会显式清理 root 和 tiger profile 中陈旧的 `CLASSPATH` 以及旧 `/usr/local/java` 启动项，所以每个 Java 项目自行负责自己的 classpath。

| 变量 | 值 | 作用域 | 管理方 | 说明 |
| --- | --- | --- | --- | --- |
| `GOPROXY` | `https://goproxy.cn,direct` | NAS/dev/test 持久化；普通 Linux auto_sync 部署时进程内生效 | `/etc/profile.d/auto-sync-domestic-mirrors.sh`、`scripts/deploy_local.sh` | Go module 镜像。 |
| `NVM_NODEJS_ORG_MIRROR` | `https://npmmirror.com/mirrors/node` | NAS/dev/test 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | nvm 的 Node 二进制下载镜像。 |
| `npm_config_registry` | `https://registry.npmmirror.com` | NAS/dev/test 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh`；也由 `npm config set registry` 写入 | npm registry 镜像。 |
| `COREPACK_NPM_REGISTRY` | `https://registry.npmmirror.com` | NAS/dev/test 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | Corepack package registry。 |
| `PIP_INDEX_URL` | `https://pypi.tuna.tsinghua.edu.cn/simple` | NAS/dev/test 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | Python package 镜像；`/etc/pip.conf` 也携带同样 index。 |
| `UV_DEFAULT_INDEX` | `https://pypi.tuna.tsinghua.edu.cn/simple` | NAS/dev/test 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | uv 默认 package index。 |
| `UV_INDEX_URL` | `https://pypi.tuna.tsinghua.edu.cn/simple` | NAS/dev/test 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | uv package index URL。 |
| `RUSTUP_DIST_SERVER` | `https://rsproxy.cn` | NAS/dev/test 持久化；普通 Linux auto_sync 部署时进程内生效 | `/etc/profile.d/auto-sync-domestic-mirrors.sh`、`scripts/deploy_local.sh` | Rust distribution 镜像。 |
| `RUSTUP_UPDATE_ROOT` | `https://rsproxy.cn/rustup` | NAS/dev/test 持久化；普通 Linux auto_sync 部署时进程内生效 | `/etc/profile.d/auto-sync-domestic-mirrors.sh`、`scripts/deploy_local.sh` | rustup metadata 镜像。 |
| `JAVA_HOME` | `/usr/lib/jvm/java-21-openjdk-amd64` | NAS/dev/test 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | apt OpenJDK 21。没有 `/usr/local/java` symlink，也没有全局 `CLASSPATH`。 |
| `PATH` | 前置 `$JAVA_HOME/bin` | NAS/dev/test 持久化 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` | 让 apt OpenJDK 工具可见。 |
| `PATH` | 存在时前置 `/opt/usr/local/bin` 和 `/opt/usr/local/go/go1.25.1/bin` | NAS/test 持久化 | `/etc/profile.d/opt-usr-local-path.sh` | 仅 NAS/test 需要，因为 `/usr/local` 软件放在 `/opt/usr/local` 下。 |
| `GOPATH` | `/root/src/go` | collector 部署过程中进程内生效 | collector 部署脚本 | 安装 Go 工具时使用；不要重新创建 `/root/go`。 |
| `GOBIN` | `$GOPATH/bin` | collector 部署过程中进程内生效 | collector 部署脚本 | Go 工具安装输出目录。 |
| `NVM_DIR` | NAS/test: `/opt/src/software/tools/nvm`；dev: `/root/src/software/tools/nvm` | collector 部署过程中进程内生效 | collector 部署脚本 | nvm 安装根目录。 |
| `PUB_HOSTED_URL` | `https://pub.flutter-io.cn` | Flutter 构建过程中进程内生效 | `scripts/deploy_local.sh`、`scripts/deploy_windows.ps1` | Flutter/Dart package 镜像。 |
| `FLUTTER_STORAGE_BASE_URL` | `https://storage.flutter-io.cn` | Flutter 构建过程中进程内生效 | `scripts/deploy_local.sh`、`scripts/deploy_windows.ps1` | Flutter artifact 镜像。 |
| `CARGO_REGISTRIES_CRATES_IO_PROTOCOL` | `sparse` | 普通 Linux auto_sync 部署过程中进程内生效 | `scripts/deploy_local.sh` | 强制 Cargo sparse 协议；实际 registry replacement 在 `/root/.cargo/config.toml`。 |
| `RUSTUP_INIT_URL` | 未设置时默认为 `https://rsproxy.cn/rustup-init.sh` | 普通 Linux auto_sync 部署的进程内覆盖项 | `scripts/deploy_local.sh` | 可选 rustup installer URL 覆盖项。 |

## 源矩阵

| 工具 | 设置项 | 值 | 管理位置 |
| --- | --- | --- | --- |
| apt | Ubuntu archive 镜像 | `https://mirrors.cloud.tencent.com` | `conf/collector_deploy_dev.ps1`、`conf/collector_deploy_nas.ps1`、`conf/collector_deploy_test.ps1` 在安装包前改写 Ubuntu sources；`scripts/deploy_local.sh` 安装 auto_sync 构建依赖前也会改写官方 Ubuntu sources。 |
| Go modules | `GOPROXY` | `https://goproxy.cn,direct` | `/etc/profile.d/auto-sync-domestic-mirrors.sh`；普通 auto_sync Linux 部署时也由 `scripts/deploy_local.sh` export。 |
| Go SDK tarball | 下载 URL | 优先 `https://mirrors.aliyun.com/golang/go1.25.1.linux-amd64.tar.gz`，回退 `https://go.dev/dl/go1.25.1.linux-amd64.tar.gz` | collector 部署脚本。 |
| nvm installer | source URL | `https://gitee.com/mirrors/nvm/raw/v0.40.3/install.sh`，并设置 `NVM_SOURCE=https://gitee.com/mirrors/nvm.git` | collector 部署脚本。 |
| Node.js downloads | `NVM_NODEJS_ORG_MIRROR` | `https://npmmirror.com/mirrors/node` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` |
| npm | `npm_config_registry` | `https://registry.npmmirror.com` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` 和 `npm config set registry`。 |
| Corepack | `COREPACK_NPM_REGISTRY` | `https://registry.npmmirror.com` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` |
| pnpm | registry | `https://registry.npmmirror.com` | collector 部署脚本执行 `pnpm config set registry`。 |
| pip | `index-url` | `https://pypi.tuna.tsinghua.edu.cn/simple` | `/etc/pip.conf` |
| pip | `PIP_INDEX_URL` | `https://pypi.tuna.tsinghua.edu.cn/simple` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` |
| uv | `UV_DEFAULT_INDEX`、`UV_INDEX_URL` | `https://pypi.tuna.tsinghua.edu.cn/simple` | `/etc/profile.d/auto-sync-domestic-mirrors.sh` |
| rustup | `RUSTUP_DIST_SERVER` | `https://rsproxy.cn` | `/etc/profile.d/auto-sync-domestic-mirrors.sh`；普通 auto_sync Linux 部署时也由 `scripts/deploy_local.sh` export。 |
| rustup | `RUSTUP_UPDATE_ROOT` | `https://rsproxy.cn/rustup` | `/etc/profile.d/auto-sync-domestic-mirrors.sh`；普通 auto_sync Linux 部署时也由 `scripts/deploy_local.sh` export。 |
| rustup installer | source URL | `https://rsproxy.cn/rustup-init.sh` | collector 部署脚本，以及 Rust 缺失时的 `scripts/deploy_local.sh`。 |
| Cargo | crates.io replacement | `sparse+https://rsproxy.cn/index/` | `/root/.cargo/config.toml`；没有 cargo config 时，`scripts/deploy_local.sh` 会创建。 |
| Flutter pub | `PUB_HOSTED_URL` | `https://pub.flutter-io.cn` | `scripts/deploy_local.sh` 和 `scripts/deploy_windows.ps1` 在 Flutter 构建过程中设置。 |
| Flutter storage | `FLUTTER_STORAGE_BASE_URL` | `https://storage.flutter-io.cn` | `scripts/deploy_local.sh` 和 `scripts/deploy_windows.ps1` 在 Flutter 构建过程中设置。 |
| Java | `JAVA_HOME` | `/usr/lib/jvm/java-21-openjdk-amd64` | `/etc/profile.d/auto-sync-domestic-mirrors.sh`；JDK 从 apt 安装。 |

systemd unit 文件应使用上表中的绝对真实路径。`scripts/deploy_local.sh` 会根据选定安装目录直接渲染 `auto_sync.service`；`scripts/deploy_nas.sh` 提供 NAS 路径。
