# 部署路径说明

本文档记录 dev 和 nas 的预期真实路径。collector 路径、部署脚本、systemd 单元以及 shell 环境配置都需要和这里保持一致。

## 主机路径矩阵

nas 的根分区很小，所以大型软件目录和大部分用户 home 状态都放在 `/opt` 下。dev 的根分区是大 SSD，故意不复制 nas 的 symlink 布局。

### 服务与应用

| 组件 | NAS | dev | AWS | OpenWrt |
| --- | --- | --- | --- | --- |
| shadowsocks | • **部署目录**:<br>&emsp;• `/opt/usr/local/shadowsocks`<br>• **状态**:<br>&emsp;• shadowsocks disable/stop<br>&emsp;• shadowsocks-rust disable/stop | • **部署目录**:<br>&emsp;• `/usr/local/shadowsocks`<br>• **状态**:<br>&emsp;• shadowsocks disable/stop<br>&emsp;• shadowsocks-rust disable/stop | • **部署目录**:<br>&emsp;• `/usr/local/shadowsocks`<br>• **状态**:<br>&emsp;• shadowsocks disable/stop<br>&emsp;• shadowsocks-rust enable/start | • **部署目录**:<br>&emsp;• `/usr/local/shadowsocks`<br>• **状态**:<br>&emsp;• shadowsocks enable/start<br>&emsp;• shadowsocks-rust disable/stop |
| xray | • **部署目录**:<br>&emsp;• `/opt/usr/local/xray`<br>• **状态**:<br>&emsp;• disable/stop | • **部署目录**:<br>&emsp;• `/usr/local/xray`<br>• **状态**:<br>&emsp;• disable/stop | • **部署目录**:<br>&emsp;• `/usr/local/xray`<br>• **状态**:<br>&emsp;• disable/stop | × |
| tbox | • **部署目录**:<br>&emsp;• `/opt/usr/local/tbox`<br>• **状态**:<br>&emsp;• tbox-client enable/start | × | • **部署目录**:<br>&emsp;• `/usr/local/tbox`<br>• **状态**:<br>&emsp;• tbox-server enable/start | × |
| rblog | × | × | • **部署目录**:<br>&emsp;• `/usr/local/blog`<br>• **状态**:<br>&emsp;• enable/start<br>• **备份**:<br>&emsp;• `rblog-backup.timer` -> `rblog-backup.service` | × |
| waiwei | • **部署目录**:<br>&emsp;• `/opt/usr/local/waiwei`<br>• **状态**:<br>&emsp;• waiwei-puller disable/stop<br>&emsp;• waiwei-web disable/stop | × | • **部署目录**:<br>&emsp;• `/usr/local/waiwei`<br>• **状态**:<br>&emsp;• waiwei-puller disable/stop<br>&emsp;• waiwei-web disable/stop | × |
| auto_sync | • **部署目录**:<br>&emsp;• `/opt/usr/local/auto_sync`<br>• **状态**:<br>&emsp;• enable/start | • **部署目录**:<br>&emsp;• `/usr/local/auto_sync`<br>• **状态**:<br>&emsp;• enable/start | × | × |
| domus | • **部署目录**:<br>&emsp;• `/opt/usr/local/domus`<br>• **状态**:<br>&emsp;• enable/start<br>• **备份**:<br>&emsp;• `domus-backup.timer` -> `domus-backup.service` | × | × | × |
| rgit | • **部署目录**:<br>&emsp;• `/opt/usr/local/rgit`<br>• **状态**:<br>&emsp;• enable/start<br>• **备份**:<br>&emsp;• `rgit-backup.timer` -> `rgit-backup.service` | × | × | × |
| mysql | • **部署目录**:<br>&emsp;• `/etc/mysql`<br>• **状态**:<br>&emsp;• disable/stop | • **部署目录**:<br>&emsp;• `/etc/mysql`<br>• **状态**:<br>&emsp;• enable/start | × | × |
| postgres | • **部署目录**:<br>&emsp;• `/etc/postgresql`<br>• **状态**:<br>&emsp;• disable/stop | • **部署目录**:<br>&emsp;• `/etc/postgresql`<br>• **状态**:<br>&emsp;• enable/start | × | × |
| redis | • **部署目录**:<br>&emsp;• 系统默认<br>• **状态**:<br>&emsp;• disable/stop | • **部署目录**:<br>&emsp;• 系统默认<br>• **状态**:<br>&emsp;• enable/start | × | × |

### 其它路径与工具链

| 组件 | NAS | dev |
| --- | --- | --- |
| go sdk | • **路径**: `/opt/usr/local/go/go1.25.1`<br>• **状态**:<br>&emsp;• 使用<br>• **PATH**: `/etc/profile.d/opt-usr-local-path.sh` | • **路径**: `/usr/local/go/go1.25.1`<br>• **状态**:<br>&emsp;• 使用 |
| gopath | • **路径**: `/root/src/go` -> `/opt/user/root/src/go`<br>• **状态**:<br>&emsp;• 使用 | • **路径**: `/root/src/go`<br>• **状态**:<br>&emsp;• 使用 |
| bin | • **路径**: `/opt/usr/local/bin`<br>• **状态**:<br>&emsp;• 使用 | • **路径**: `/usr/local/bin`<br>• **状态**:<br>&emsp;• 使用 |
| buildifier | • **路径**: `/opt/usr/local/bin/buildifier`<br>• **状态**:<br>&emsp;• 安装 | • **路径**: `/usr/local/bin/buildifier`<br>• **状态**:<br>&emsp;• 安装 |
| flutter sdk | • **路径**: `/root/src/software/flutter`<br>• **状态**:<br>&emsp;• 使用<br>• **环境**: `FLUTTER_ROOT` | • **路径**: `/root/src/software/flutter`<br>• **状态**:<br>&emsp;• 使用 |
| nvm root | • **路径**: `/root/src/software/tools/nvm`<br>• **状态**:<br>&emsp;• `NVM_DIR` | • **路径**: `/root/src/software/tools/nvm`<br>• **状态**:<br>&emsp;• `NVM_DIR` |
| node.js | • **路径**: `/root/src/software/tools/nvm/versions/node/v24.18.0`<br>• **状态**:<br>&emsp;• 使用 | • **路径**: `/root/src/software/tools/nvm/versions/node/v24.18.0`<br>• **状态**:<br>&emsp;• 使用 |
| npm | • **路径**: nvm node `bin/npm`<br>• **状态**:<br>&emsp;• 使用 | • **路径**: nvm node `bin/npm`<br>• **状态**:<br>&emsp;• 使用 |
| pnpm | • **路径**: nvm/Corepack 管理<br>• **状态**:<br>&emsp;• 使用 | • **路径**: nvm/Corepack 管理<br>• **状态**:<br>&emsp;• 使用 |
| python | • **路径**: `/usr/bin/python3`<br>• **状态**:<br>&emsp;• 使用 | • **路径**: `/usr/bin/python3`<br>• **状态**:<br>&emsp;• 使用 |
| pip | • **路径**: `/etc/pip.conf`<br>• **状态**:<br>&emsp;• 管理 | • **路径**: `/etc/pip.conf`<br>• **状态**:<br>&emsp;• 管理 |
| uv | • **路径**: `/etc/profile.d/auto-sync-domestic-mirrors.sh`<br>• **状态**:<br>&emsp;• 管理 | • **路径**: `/etc/profile.d/auto-sync-domestic-mirrors.sh`<br>• **状态**:<br>&emsp;• 管理 |
| rust | • **路径**: `/root/.cargo`、`/root/.rustup` -> `/opt/user/root`<br>• **状态**:<br>&emsp;• symlink | • **路径**: `/root/.cargo`、`/root/.rustup`<br>• **状态**:<br>&emsp;• 真实路径 |
| java | • **路径**: `/usr/lib/jvm/java-21-openjdk-amd64`<br>• **状态**:<br>&emsp;• 使用 | • **路径**: `/usr/lib/jvm/java-21-openjdk-amd64`<br>• **状态**:<br>&emsp;• 使用 |
| pgvector | • **路径**: `/root/src/software`<br>• **状态**:<br>&emsp;• 使用 | • **路径**: `/root/src/software`<br>• **状态**:<br>&emsp;• 使用 |
| openwrt toolchain | × | • **路径**: `/root/src/software/openwrt`<br>• **状态**:<br>&emsp;• 构建使用 |
| root home | • **路径**: `/opt/user/root`<br>• **状态**:<br>&emsp;• `/root` 子项 symlink，`/root/.ssh` 除外 | • **路径**: `/root`<br>• **状态**:<br>&emsp;• 真实路径 |
| tiger home | • **路径**: `/opt/user/tiger`<br>• **状态**:<br>&emsp;• `/home/tiger` 选定 dotfile symlink | • **路径**: `/home/tiger`<br>• **状态**:<br>&emsp;• 真实路径 |

nas 不允许把 `/opt/usr/local` bind-mount 回 `/usr/local`。旧路径如 `/usr/local/blog`、`/usr/local/go`、`/usr/local/tbox`、`/usr/local/waiwei`、`/usr/local/xray`、`/usr/local/bin` 应通过一次性命令或临时迁移脚本处理；长期 collector 部署脚本只维护当前 `/opt/usr/local/*` 目标布局。

dev 上意外出现的 `/opt/user` symlink、旧 `/opt/software/src` 内容，以及陈旧的 service/profile 路径引用，应通过一次性命令或临时迁移脚本处理；长期 collector 部署脚本只维护当前 `/root/src/software`、`/usr/local/*` 和真实本地 home 布局。

## auto_sync 部署默认值

| 入口 | 默认安装目录 | nas 覆盖值 |
| --- | --- | --- |
| `scripts/deploy_local.sh` | `/usr/local/auto_sync` | 只用于 dev 本机部署。 |
| `scripts/deploy_nas.sh` | `/opt/usr/local/auto_sync` | 必须从 dev 的 `/root/src/rust/auto_sync` 执行；脚本会先部署 dev 本机，再把同一 Linux release 产物推到 NAS。 |
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
| `/etc/systemd/system/auto_sync.service` | nas/dev | dev 上由 `scripts/deploy_local.sh` 生成；NAS 上由 dev 运行的 `scripts/deploy_nas.sh` 生成 | 从对应主机的安装目录启动统一的 `auto_sync` 进程。 |
| `/etc/systemd/coredump.conf` | nas/dev | collector 部署脚本 | 启用外部无限大小 coredump 存储。 |
| `/etc/security/limits.conf` | nas/dev | collector 部署脚本 | 追加无限 core size 限制。 |
| `/etc/sysctl.conf` | nas/dev | collector 部署脚本 | 追加 coredump pattern 并重新加载 sysctl。 |
| `/etc/hosts` | nas/dev | collector 部署脚本 | 确保脚本管理的所有站点域名都指向本机 `127.0.0.1`，并移除重复的陈旧项。 |
| `/etc/fstab` | nas/dev | collector 部署脚本 | 禁用 swap 时注释 `/swap.img` 条目。 |
| `/etc/ssh/sshd_config` | nas/dev | collector 部署脚本 | bootstrap 主机时强制项目 SSH 策略。 |
| `/etc/nginx/ssl` | nas/dev | collector 部署脚本 | 部署前从本地 collector share 的 `aws/etc/nginx/ssl` 复制到目标 share，再随采集文件推到主机；部署脚本会在重启 nginx 前统一修正目录为 `root:root 755`，证书为 `644`，私钥为 `600`。 |

nginx vhost、MySQL、PostgreSQL、domus、tbox、waiwei、xray、rgit、logrotate 等服务专用文件，会从 collector share 复制到它们正常的系统位置。它们应和对应的 collect 路径保持一致，不要在脚本里临时散落生成。rblog 只在 AWS collector 路径中保留；dev/nas 不再管理 rblog。

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
| `NVM_DIR` | nas: `/root/src/software/tools/nvm`；dev: `/root/src/software/tools/nvm` | collector 部署过程中进程内生效 | collector 部署脚本 | nvm 安装根目录。 |
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

systemd unit 文件应使用上表中的绝对真实路径。`scripts/deploy_local.sh` 会根据选定安装目录直接渲染 dev 的 `auto_sync.service`；`scripts/deploy_nas.sh` 在 dev 上构建并渲染 NAS 的 `/opt/usr/local/auto_sync` unit。



