# 部署路径说明

本文档记录 dev 和 nas 的预期真实路径。collector 路径、部署脚本、systemd 单元以及 shell 环境配置都需要和这里保持一致。

## 主机路径矩阵

nas 的根分区很小，所以大型软件目录和大部分用户 home 状态都放在 `/opt` 下。dev 的根分区是大 SSD，故意不复制 nas 的 symlink 布局。

### 服务与应用

| 组件 | AWS | dev | NAS | OpenWrt |
| --- | --- | --- | --- | --- |
| auto_sync | × | `/usr/local/auto_sync`<br>部署并运行；运行目录只保留部署产物和数据。 | `/opt/usr/local/auto_sync`<br>部署并运行；运行目录只保留部署产物和数据。 | × |
| rblog | `/usr/local/blog`<br>由 AWS collector 管理并启动 `rblog`、`rblog-backup.timer`。 | × | × | × |
| shadowsocks | enable/start `shadowsocks-rust`。 | `/usr/local/shadowsocks`<br>保留文件但 disable/stop `shadowsocks` 和 `shadowsocks-rust`。 | `/opt/usr/local/shadowsocks`<br>保留文件但 disable/stop `shadowsocks` 和 `shadowsocks-rust`。 | enable/start 非 rust 版 `shadowsocks`，并 disable/stop `shadowsocks-rust`；`xray-plugin` 属于 shadowsocks 链路。 |
| tbox | enable/start `tbox_server.service`。 | × | `/opt/usr/local/tbox`<br>enable/start `tbox_client.service` 和 `tbox-logrotate.timer`。 | × |
| waiwei | disable/stop `waiwei-web`、`waiwei-puller`；不存在单独的 `waiwei` systemd 服务。 | `/usr/local/waiwei`<br>disable/stop `waiwei-web`、`waiwei-puller`；文件仅用于采集、备份或保留历史配置。 | `/opt/usr/local/waiwei`<br>disable/stop `waiwei-web`、`waiwei-puller`；文件仅用于采集、备份或保留历史配置。 | × |
| xray | disable/stop 独立 `xray` 服务或进程。 | `/usr/local/xray`<br>disable/stop 独立 `xray` 服务或进程。 | `/opt/usr/local/xray`<br>disable/stop 独立 `xray` 服务或进程。 | × |
| immich | × | × | ×<br>部署脚本只负责 disable/stop 相关服务且不删除现有文件。 | × |
| domus | × | × | `/opt/usr/local/domus`<br>采集 `/opt/usr/local/domus`、`domus.service`、`domus-backup.service/timer`；部署时 enable/start `domus` 和 `domus-backup.timer`。`/opt/usr/local/domus/.backup-worktree` 指向 `git@github.com:xiedeacc/domus_data.git`，周期性备份 `bin`、`conf` 和 `data` 中的小状态文件；`data/upload`、`data/backups`、logs、`.backup-worktree` 不进 collector/share/git 备份。 | × |
| rgit | × | × | `/opt/usr/local/rgit`<br>采集 `/opt/usr/local/rgit`、`rgit.service`、`rgit-backup.service/timer`、`rgit-ocsp.service/timer` 和 `rgit-backup.service.d`；部署时 enable/start `rgit`、`rgit-backup.timer`、`rgit-ocsp.timer`。 | × |
| gitlab | × | ×<br>如需 GitLab 迁移或维护，用一次性命令或专门脚本处理。 | ×<br>如需 GitLab 迁移或维护，用一次性命令或专门脚本处理。 | × |
| 数据库服务 | × | collector 部署时可运行<br>仍按当前脚本运行这些服务。 | 保留配置/数据但 disable/stop `mysql`、`postgresql`、`redis-server`<br>不再运行 MySQL/PostgreSQL/Redis；部署脚本只保留配置和数据，不启动、不 enable、不作为 required service 校验。 | × |

### 其它路径与工具链

| 组件 | AWS | dev | NAS | OpenWrt |
| --- | --- | --- | --- | --- |
| go sdk | × | `/usr/local/go/go1.25.1`<br>使用正常的 `/usr/local/go`。 | `/opt/usr/local/go/go1.25.1`<br>通过 `/etc/profile.d/opt-usr-local-path.sh` 把 go 加入 `PATH`。 | × |
| gopath | × | `/root/src/go`<br>使用 `/root/src/go`，不要重新创建 `/root/go`。 | `/root/src/go`，通过 `/opt/user/root/src/go` symlink 指向<br>使用 `/root/src/go`，实际通过 symlink 指到 `/opt/user/root/src/go`；不要重新创建 `/root/go`。 | × |
| bin | × | `/usr/local/bin`<br>使用 `/usr/local/bin`。 | `/opt/usr/local/bin`<br>使用 `/opt/usr/local/bin`；不要把 `/usr/local/bin` 重新做成 bind mount。 | × |
| buildifier | × | `/usr/local/bin/buildifier`<br>由 collector 部署脚本安装到 `/usr/local/bin/buildifier`。 | `/opt/usr/local/bin/buildifier`<br>由 collector 部署脚本安装到 `/opt/usr/local/bin/buildifier`。 | × |
| 静态站点根目录 | × | × | × | × |
| flutter sdk | × | `/root/src/software/flutter` | `/opt/src/software/flutter`<br>wrapper 设置 `FLUTTER_ROOT`。 | × |
| nvm root | × | `/root/src/software/tools/nvm`<br>作为 `NVM_DIR`。 | `/opt/src/software/tools/nvm`<br>作为 `NVM_DIR`；不要重建 tiger 用户 nvm/npm 状态。 | × |
| node.js | × | `/root/src/software/tools/nvm/versions/node/v24.18.0`<br>由 nvm 安装，路径在 `/root/src/software/tools/nvm` 下；可回退到最新 Node 24。 | `/opt/src/software/tools/nvm/versions/node/v24.18.0`<br>由 nvm 安装，路径在 `/opt/src/software/tools/nvm` 下；可回退到最新 Node 24。 | × |
| npm | × | nvm root 下的 node.js `bin/npm`<br>使用 nvm root 下的 `npm`，registry 由部署脚本配置。 | nvm root 下的 node.js `bin/npm`<br>使用 nvm root 下的 `npm`，registry 由部署脚本配置。 | × |
| pnpm | × | nvm root 下由 node.js/Corepack 管理的二进制<br>Node 设置完成后安装或激活。 | nvm root 下由 node.js/Corepack 管理的二进制<br>Node 设置完成后安装或激活。 | × |
| python | × | apt 的系统 python，通常是 `/usr/bin/python3`<br>使用 apt 系统 python；项目 virtualenv 留在各自项目目录内。 | apt 的系统 python，通常是 `/usr/bin/python3`<br>使用 apt 系统 python；项目 virtualenv 留在各自项目目录内。 | × |
| pip | × | `/etc/pip.conf`<br>collector 部署脚本管理 `/etc/pip.conf`。 | `/etc/pip.conf`<br>collector 部署脚本管理 `/etc/pip.conf`。 | × |
| uv | × | `/etc/profile.d/auto-sync-domestic-mirrors.sh` 中的环境变量<br>通过 `/etc/profile.d/auto-sync-domestic-mirrors.sh` 中的环境变量配置。 | `/etc/profile.d/auto-sync-domestic-mirrors.sh` 中的环境变量<br>通过 `/etc/profile.d/auto-sync-domestic-mirrors.sh` 中的环境变量配置。 | × |
| rust | × | `/root/.cargo`、`/root/.rustup` 真实本地路径<br>`/root/.cargo`、`/root/.rustup` 是真实本地路径；Cargo registry 镜像配置位于 `/root/.cargo/config.toml`。 | `/root/.cargo`、`/root/.rustup` symlink 到 `/opt/user/root`<br>`/root/.cargo`、`/root/.rustup` symlink 到 `/opt/user/root`，避免占用根分区。 | × |
| java | × | apt openjdk，路径 `/usr/lib/jvm/java-21-openjdk-amd64`<br>使用 apt openjdk，`JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64`。 | apt openjdk，路径 `/usr/lib/jvm/java-21-openjdk-amd64`<br>使用 apt openjdk，`JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64`。 | × |
| pgvector | × | `/root/src/software`<br>使用 `/root/src/software`。 | `/opt/src/software`<br>使用 `/opt/src/software`，避免占用根分区。 | × |
| openwrt toolchain | × | `/root/src/software/openwrt`<br>`scripts/deploy_openwrt.sh` 优先从 `/root/src/software/openwrt` 自动发现 `toolchain-aarch64_*_musl`。 | × | 只接收构建产物。 |
| root home | × | `/root`<br>真实路径，不创建 `/opt/user/root`。 | `/opt/user/root`<br>`/root` 下除 `/root/.ssh` 外的所有子项 symlink 到 `/opt/user/root`；`/root/.ssh` 保持真实目录以保证 SSH 登录安全。 | × |
| tiger home | × | `/home/tiger`<br>真实路径，不创建 `/opt/user/tiger`。 | `/opt/user/tiger`<br>选定的 `/home/tiger` dotfile symlink 到 `/opt/user/tiger`。 | × |

nas 不允许把 `/opt/usr/local` bind-mount 回 `/usr/local`。旧路径如 `/usr/local/blog`、`/usr/local/go`、`/usr/local/tbox`、`/usr/local/waiwei`、`/usr/local/xray`、`/usr/local/bin` 应通过一次性命令或临时迁移脚本处理；长期 collector 部署脚本只维护当前 `/opt/usr/local/*` 目标布局。

dev 上意外出现的 `/opt/user` symlink、旧 `/opt/src/software` 或 `/opt/software/src` 内容，以及陈旧的 service/profile 路径引用，应通过一次性命令或临时迁移脚本处理；长期 collector 部署脚本只维护当前 `/root/src/software`、`/usr/local/*` 和真实本地 home 布局。

## auto_sync 部署默认值

| 入口 | 默认安装目录 | nas 覆盖值 |
| --- | --- | --- |
| `scripts/deploy_local.sh` | `/usr/local/auto_sync` | 只用于 dev；`scripts/deploy_nas.sh` 设置为 `/opt/usr/local/auto_sync`。 |
| `scripts/deploy_nas.sh` | `/opt/usr/local/auto_sync` | 必须从 `/opt/src/rust/auto_sync` 执行；脚本会拒绝任何其他 nas 执行目录或安装目录。 |
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
| `/etc/nginx/ssl` | nas/dev | collector 部署脚本 | 部署前从本地 collector share 的 `aws/etc/nginx/ssl` 复制到目标 share，再随采集文件推到主机；部署脚本会在重启 nginx 前统一修正目录为 `root:root 755`，证书为 `644`，私钥为 `600`。 |

nginx vhost、MySQL、PostgreSQL、domus、tbox、waiwei、xray、rgit、logrotate 等服务专用文件，会从 collector share 复制到它们正常的系统位置。它们应和对应的 collect 路径保持一致，不要在脚本里临时散落生成。rblog 只在 AWS collector 路径中保留；dev/nas/test 不再管理 rblog。

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
