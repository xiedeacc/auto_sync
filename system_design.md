# auto_sync 系统设计

## 1. 目标与边界

auto_sync 是一个使用 Rust 实现的目录周期同步工具，支持图形界面配置多组源目录和目标目录，并在 Linux 上通过 fanotify 监听源目录变化。系统需要把变化事件持久化，按可配置周期集中同步，且在进程重启、目标盘离线、队列溢出等情况下不丢失文件不一致问题。

已确认实现决策：

- 同步语义采用 mirror 模式：`src` 删除后，`dst` 对应文件也删除。
- 当前版本先不支持 Windows 平台，编译目标限定为 Linux。
- NAS 确认为支持 systemd 和 fanotify，后台部署按 systemd service 实现。
- GUI 已实现为 Linux Tauri 应用，前端资源位于 `src/ui`，后端命令直接复用 Rust core。
- Headless 场景支持独立 Web UI：`auto_sync_web`，默认监听 `0.0.0.0:18765`。

核心目标：

- 支持多组 `src`，每组 `src` 可以配置多个 `dst`。
- 支持本机部署，也支持部署到 NAS：`ssh -p 10022 root@192.168.3.178`。
- 当前版本只支持 Linux；Ubuntu 支持图形化运行，也支持 systemd 后台运行。
- Linux 使用 fanotify 监听源目录事件，事件必须落盘持久化。
- fanotify 事件按周期累积，支持每天或每周同步一次，周期可配置。
- UI 中每个 `dst` 用绿点/红点展示是否已经同步到最新周期；后台运行时定期把状态输出到日志。
- 通过周期 offset、事件 offset、快照校验和落盘状态保证重启后可恢复。
- 不能只依赖文件系统事件来保证正确性；周期同步时必须通过源目录快照和目标目录校验兜底，确保不会遗漏任何最终文件不一致。

非目标或边界：

- fanotify 是 Linux 能力；当前版本不支持 Windows，也不生成 Windows 产物。
- 本设计同步语义为“镜像备份”：每个 `dst` 在完成某个周期后，应与该周期结束时的 `src` 快照一致，包括新增、修改、删除、重命名和元数据变化。
- NAS 已确认支持 systemd 和 fanotify；CPU 架构仍需部署前检测以选择正确二进制。

## 2. 目录约束

项目目录遵循用户指定约束：

```text
auto_sync/
  src/                    # 所有业务代码、Rust crate、Tauri 集成和 UI 代码
  bin/                    # 构建后产物复制到这里
  conf/                   # 配置文件、状态数据库、systemd 模板
  logs/                   # 运行日志
  system_design.md        # 本设计文档
```

建议细化目录：

```text
src/
  bin/
    auto_sync_gui.rs      # Linux Tauri GUI 入口
    auto_sync_web.rs      # Headless/Web UI 入口
    auto_syncd.rs         # Ubuntu/NAS 后台 daemon 入口
    auto_syncctl.rs       # 可选 CLI，部署、状态、手动触发同步
  core/
    config.rs             # 配置加载、校验、热更新
    state.rs              # SQLite 状态库访问
    model.rs              # src group、dst、cycle、event、status 数据模型
    scheduler.rs          # daily/weekly 周期计算
    watcher/
      mod.rs              # Watcher trait
      fanotify.rs         # Linux fanotify 实现
      null.rs             # 无 watcher 或测试实现
    sync/
      planner.rs          # 根据事件和快照生成同步计划
      executor.rs         # 拷贝、删除、rename、校验、重试
      verifier.rs         # dst 完整性校验
    status.rs             # dst 状态聚合
    logging.rs            # tracing 日志初始化
  daemon/
    service.rs            # 后台服务生命周期
    ipc.rs                # GUI/ctl 与 daemon 通信
  ui/
    ...                   # Tauri 前端代码
```

构建后通过 `xtask` 或构建脚本把产物复制到：

```text
bin/auto_sync_gui
bin/auto_sync_web
bin/auto_syncd
bin/auto_syncctl
```

## 3. 总体架构

系统分为五层：

1. 配置层：读取 `conf/auto_sync.toml`，管理 src/dst 配置、同步周期、日志参数、部署目标。
2. 状态层：使用 SQLite + WAL 持久化事件、周期、offset、同步任务和校验结果。
3. 事件层：Linux daemon 通过 fanotify 收集源目录事件，写入状态库。
4. 同步层：周期结束后为每个 `dst` 生成同步计划，执行拷贝/删除/校验，并更新 offset。
5. 展示与控制层：Tauri GUI 和 Web UI 展示配置、状态和进度；daemon 后台周期输出状态日志；可选 `auto_syncctl` 用于部署、状态查看和手动触发。

推荐进程形态：

- Ubuntu GUI 模式：`auto_sync_gui` 启动 Tauri 界面，当前实现直接调用嵌入式 Rust core 读写配置、查询状态和触发手动同步；周期监听和后台调度由 `auto_syncd` 负责。
- Headless Web 模式：`auto_sync_web` 启动 HTTP 管理页面，复用 `src/ui` 前端和 Rust core API。
- Ubuntu/NAS 后台模式：`auto_syncd` 由 systemd 启动，负责 fanotify、调度、同步和日志。

GUI 与 daemon 通信：

- 当前 GUI/Web 直接读写本机配置和 SQLite 状态库，手动同步直接调用 core。
- 后续如需要常驻 daemon 控制面，可增加 Unix domain socket。
- 远程 NAS 使用 SSH 执行 `auto_syncctl status`、上传配置、启动/停止服务。
- `auto_sync_web` 默认开放 `0.0.0.0:18765`，适合 NAS/headless；生产环境可通过防火墙或反向代理限制访问。

## 4. 配置设计

主配置文件：`conf/auto_sync.toml`。

示例：

```toml
[app]
data_db = "conf/state/auto_sync.sqlite"
log_dir = "logs"
status_log_interval_secs = 300

[schedule]
mode = "daily"              # daily | weekly
time = "02:00:00"
timezone = "local"
sync_current_cycle_manually = false

[[source_groups]]
id = "photos"
src = "/data/photos"
enabled = true
mode = "mirror"             # mirror 为默认；archive 预留

  [[source_groups.destinations]]
  id = "usb_backup"
  path = "/mnt/backup/photos"
  enabled = true

  [[source_groups.destinations]]
  id = "nas_backup"
  path = "/mnt/nas/photos"
  enabled = true

[[source_groups]]
id = "docs"
src = "/home/user/docs"
enabled = true

  [[source_groups.destinations]]
  id = "docs_backup"
  path = "/mnt/backup/docs"
  enabled = true

[[deploy.targets]]
id = "nas"
host = "192.168.3.178"
port = 10022
user = "root"
install_dir = "/opt/auto_sync"
```

配置原则：

- GUI 修改配置时先写临时文件，再原子替换 `conf/auto_sync.toml`。
- daemon 监听配置文件变更，重新加载时只增量更新 source group/dst，不清空状态库。
- 每个 `source_groups.id` 和 `destinations.id` 必须稳定，作为 offset 和状态表的外键。
- `dst` 可以离线；离线不删除状态，只标记红点并在下一次在线时从落后周期补齐。

## 5. 状态持久化设计

状态库建议使用 SQLite，路径为 `conf/state/auto_sync.sqlite`，启用 WAL 和同步写入策略：

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
PRAGMA foreign_keys = ON;
```

核心表：

```text
source_group
  id, src_path, enabled, mode, created_at, updated_at

destination
  id, source_group_id, dst_path, enabled, created_at, updated_at

sync_cycle
  id, source_group_id, starts_at, ends_at, status
  status = open | closed | planning | syncing | verified | failed

event_log
  event_id, source_group_id, cycle_id, observed_at
  raw_mask, event_kind, rel_path, file_key, is_dir
  coalesce_key, overflow_marker, persisted_at

path_snapshot
  snapshot_id, source_group_id, cycle_id, rel_path
  file_type, size, mtime_ns, mode, uid, gid, content_hash, hash_status

destination_offset
  destination_id, source_group_id
  last_completed_cycle_id
  last_verified_cycle_id
  current_cycle_id
  current_event_high_watermark
  status, status_reason, updated_at

sync_task
  task_id, destination_id, cycle_id, rel_path
  action = copy | delete | mkdir | metadata | verify
  status = pending | running | done | retry | failed
  attempt_count, last_error, updated_at

sync_file_result
  result_id, task_id, source_fingerprint, dst_fingerprint
  verified_at, verification_status
```

offset 语义：

- `event_log.event_id` 是全局递增事件 offset。
- `sync_cycle.id` 是周期 offset。
- 每个 `dst` 独立维护 `last_completed_cycle_id` 和 `last_verified_cycle_id`。
- UI 绿点条件：`dst` 在线，且 `last_verified_cycle_id >= latest_closed_cycle_id`。
- UI 红点条件：`dst` 离线，或 `last_verified_cycle_id < latest_closed_cycle_id`，或最近一次校验失败。
- 后台日志定期输出每个 `dst` 的 `latest_closed_cycle_id`、`last_verified_cycle_id`、在线状态、当前任务进度和错误原因。

## 6. fanotify 事件设计

Linux 后台进程使用 fanotify 监听 source group 所在文件系统，并在用户态过滤到配置的 `src` 路径下。

当前实现说明：

- 第一版实现使用 fanotify fd-path 模式，注册当前环境稳定接受的 `FAN_MODIFY | FAN_CLOSE_WRITE`。
- 若 filesystem/mount mark 不可用，会回退到对 src 目录树逐目录注册 inode mark。
- 删除、rename 和部分元数据变化不依赖事件流保证正确性，而是由周期关闭时的源目录快照和 dst 全量校验兜底。
- 后续如果需要精确持久化 create/delete/move 的目录项名称，可升级到 `FAN_REPORT_FID | FAN_REPORT_DIR_FID | FAN_REPORT_NAME` 模式，并实现 file handle/name 解析。

建议 fanotify 策略：

- 使用 `FAN_REPORT_FID`、`FAN_REPORT_DIR_FID`、`FAN_REPORT_NAME` 获取稳定文件标识和目录项信息。
- 关注事件：create、modify、close_write、delete、move_from、move_to、attrib、delete_self、move_self、queue_overflow。
- 对同一文件在同一周期内的多次修改只在同步计划阶段合并，但原始事件仍写入 `event_log`。
- 每个 fanotify 事件必须先持久化到 SQLite，再更新内存状态。
- 如果收到 queue overflow，写入 `overflow_marker = true` 的事件，并把对应 source group 标记为 `needs_full_rescan`。
- 如果进程重启，daemon 先从状态库恢复 open cycle、未完成任务和 destination offset，再重新注册 fanotify mark。

fanotify 重要边界：

- fanotify 不应作为唯一正确性来源。rename、临时文件写入、权限变化、事件队列溢出、挂载点变化都可能导致事件流不足以直接推导最终状态。
- 每个同步周期关闭时必须对 `src` 做实际快照扫描；有 overflow 或 watcher 中断时必须全量扫描。
- 为降低大目录成本，可以在无 overflow 的情况下优先扫描事件涉及的路径和父目录，但完成周期前仍需要执行轻量 manifest 校验策略，确保没有遗漏。

## 7. 周期调度与事件累积

周期模型：

- 每个 source group 独立维护当前 open cycle。
- `daily` 周期以配置的本地时间每天关闭一次。
- `weekly` 周期以配置的星期几和时间关闭一次，后续编码时可在配置中增加 `weekday`。
- 周期关闭后，新事件进入新的 open cycle；关闭的 cycle 进入 planning/syncing。

流程：

1. daemon 启动后加载配置和状态库。
2. 对每个 source group 创建或恢复 open cycle。
3. fanotify 事件持续写入 `event_log`，绑定到当前 open cycle。
4. 到达周期时间后，将当前 cycle 原子标记为 closed，并开启新的 open cycle。
5. 对 closed cycle 构建源目录快照。
6. 为每个 enabled dst 生成 sync_task。
7. 在线 dst 执行同步；离线 dst 保持红点，等待后续恢复。
8. sync_task 全部完成并通过校验后，更新该 dst 的 `last_verified_cycle_id`。

手动同步：

- GUI 可以提供“立即同步已关闭周期”。
- 是否允许“提前关闭当前周期并同步”由配置 `sync_current_cycle_manually` 控制，避免用户误以为同步到一个仍在变化的窗口。

## 8. 同步一致性设计

目标语义：每个 `dst` 完成 cycle N 后，应与 source group 在 cycle N `ends_at` 附近生成的源快照一致。

同步计划生成：

- 输入：cycle 内 event_log、source snapshot、上一次 verified snapshot、dst 当前状态。
- 输出：copy、mkdir、metadata、delete、verify 任务。
- 对于同一 rel_path 的多次事件，任务层合并为最终动作。
- 对于 rename，如果事件信息完整，可优化为 dst rename；否则退化为 copy 新路径 + delete 旧路径。

文件复制策略：

- 拷贝到目标目录同文件系统下的临时路径：`.auto_sync_tmp/<cycle>/<rel_path>.tmp`。
- 拷贝完成后 fsync 文件和父目录。
- 校验 size、mtime 和可选 hash；大文件可先 size+mtime，异常时再 hash。
- 校验通过后原子 rename 到最终路径。
- 若复制期间源文件发生变化，检测 fingerprint 不一致，重新排队到当前 cycle 或下一 cycle。

删除策略：

- mirror 模式下，源快照不存在但 dst 存在的文件需要删除。
- 为避免误删，先移动到 `.auto_sync_trash/<cycle>/...`，完成整个 cycle 校验后再按保留策略清理。
- 如果用户选择 archive 模式，则不执行删除，只记录额外文件状态；该模式需要单独确认。

校验策略：

- 每个任务完成后做局部校验。
- 每个 dst 完成 cycle 后做周期级校验：抽样或全量对比可配置；发生 overflow、异常重启、上次失败、dst 离线恢复时强制全量校验。
- 校验失败不会推进 `last_verified_cycle_id`。
- 未推进 offset 的 dst 永远显示红点，并在后台日志中输出原因。

保证“不丢失任何文件不一致”的关键机制：

- 事件先落盘，再进入内存队列。
- fanotify overflow 或 watcher 中断会标记 full rescan。
- 周期关闭后基于实际源目录生成 snapshot，而不是只相信事件。
- 每个 dst 有独立 cycle offset，离线后不会跳过周期；恢复后从最旧未验证周期开始追赶。
- offset 只在同步任务完成且校验通过后推进。
- 临时文件 + fsync + 原子 rename 降低崩溃导致半文件的风险。
- 重启时恢复 running/retry 任务，未验证的 cycle 会重新校验或重做。

## 9. dst 在线状态与 UI 状态

dst 在线检测：

- 路径存在且可读写。
- `dst` 所在设备已挂载；Linux 可通过 `statfs`、`/proc/self/mountinfo` 或设备 ID 判断。
- 可在 dst 下创建和删除 `.auto_sync_probe` 临时文件。
- 目标目录剩余空间满足当前待同步任务估算。

UI 状态：

- source group 页面展示每个 `src` 和它的多个 `dst`。
- 每个 dst 显示红点或绿点：
  - 绿点：在线，且 `last_verified_cycle_id` 已达到最新 closed cycle。
  - 红点：离线、落后、同步失败或校验失败。
- 红点 tooltip 或详情面板展示原因：离线、落后几个周期、最近错误、剩余任务数。
- 支持添加/删除/编辑 source group 和 destination。
- 支持选择 src 文件夹和 dst 文件夹。
- 支持查看当前周期、上次同步时间、下次计划同步时间、同步进度。
- 支持手动触发已关闭周期同步。

后台日志状态：

daemon 每 `status_log_interval_secs` 输出一次：

```text
source=photos dst=usb_backup online=true latest_cycle=42 verified_cycle=42 status=green pending=0
source=photos dst=nas_backup online=false latest_cycle=42 verified_cycle=39 status=red reason=dst_offline
```

## 10. 部署设计

本机部署：

- 构建 Rust/Tauri 产物。
- 复制二进制到 `bin/`。
- 初始化 `conf/auto_sync.toml` 和 `conf/state/`。
- 初始化 `logs/`。
- Ubuntu 后台模式安装 systemd unit。

NAS 部署：

目标连接：

```text
ssh -p 10022 root@192.168.3.178
```

建议安装路径：

```text
/opt/auto_sync/
  bin/
  conf/
  logs/
```

部署步骤：

1. 通过 SSH 执行 `uname -m`、`uname -s`、`systemctl --version`，检测架构和 systemd。
2. 根据架构选择或交叉编译 Linux 二进制。
3. 上传 `bin/auto_syncd`、`bin/auto_syncctl`、`conf/auto_sync.toml` 和 systemd unit。
4. 执行权限设置：`chmod +x /opt/auto_sync/bin/*`。
5. 如果支持 systemd，安装并启动 `auto_sync.service`。
6. 通过 `auto_syncctl status` 验证状态。

systemd unit 初稿：

```ini
[Unit]
Description=auto_sync daemon
After=local-fs.target network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=/opt/auto_sync
ExecStart=/opt/auto_sync/bin/auto_syncd --config /opt/auto_sync/conf/auto_sync.toml
Restart=always
RestartSec=5
User=root
Group=root
CapabilityBoundingSet=CAP_SYS_ADMIN CAP_DAC_READ_SEARCH
AmbientCapabilities=CAP_SYS_ADMIN CAP_DAC_READ_SEARCH
NoNewPrivileges=false

[Install]
WantedBy=multi-user.target
```

权限说明：

- fanotify 通常需要较高权限或 capability，后台 daemon 建议由 root 运行，尤其在 NAS 和跨用户目录备份场景。
- 若后续要降低权限，需要针对具体 Linux 内核版本和被监听目录权限重新验证。

## 11. 日志与可观测性

日志目录：`logs/`。

建议文件：

```text
logs/auto_syncd.log
logs/auto_sync_gui.log
logs/sync_audit.log
```

日志内容：

- 配置加载、source group/dst 变更。
- fanotify mark 注册、事件数量、overflow。
- 周期关闭、snapshot 生成、任务数量。
- 每个 dst 的在线状态、offset、同步耗时、失败原因。
- 文件级错误写入 audit log，避免主日志过大。

Rust 实现建议使用 `tracing` + `tracing-appender`，支持按天滚动。

## 12. 错误恢复

进程重启：

- 读取状态库。
- 将 running 状态任务恢复为 retry。
- 未 verified 的 cycle 重新进入 planning/syncing。
- 重新注册 fanotify mark。
- 若发现 watcher 停止期间存在 open cycle，则该 cycle 标记 `needs_full_rescan`。

dst 离线：

- 在线检测失败时不执行同步。
- `destination_offset.status = offline`，UI 红点。
- 不推进 cycle offset。
- 定期探测恢复；恢复后从最旧未 verified cycle 开始同步。

fanotify overflow：

- 写入 overflow 事件。
- source group 标记 full rescan。
- 当前 cycle 关闭后执行全量 snapshot 和全量 dst 校验。

磁盘空间不足：

- task 标记 failed，dst 红点。
- 不推进 verified cycle。
- 后台日志输出所需空间估算和剩余空间。

拷贝中断：

- 临时文件保留在 `.auto_sync_tmp`。
- 重启或下一次同步时清理同 cycle 旧临时文件并重做任务。

## 13. 安全设计

- SSH 部署优先使用密钥认证，不在配置文件保存明文密码。
- GUI 与 daemon 的本机 IPC 只允许当前用户或 root 访问。
- 远程控制走 SSH，不暴露网络监听端口。
- 所有路径配置必须规范化，禁止 dst 配置到 src 的子目录，避免递归同步。
- 启动时检测 source groups 之间是否互相嵌套；如果嵌套，需要明确优先级或拒绝启动。
- 删除操作默认进入 `.auto_sync_trash`，并记录 audit log。

## 14. 测试计划

单元测试：

- 配置解析和校验。
- daily/weekly 周期计算。
- event coalesce 逻辑。
- destination offset 推进规则。
- snapshot diff 和 sync plan 生成。

集成测试：

- 使用临时目录模拟 src/dst，验证新增、修改、删除、rename。
- 模拟 dst 离线后恢复，确认从落后 cycle 补齐。
- 模拟进程在 copy 后、rename 前、verify 前崩溃，确认重启可恢复。
- 模拟 SQLite 中 running task，确认恢复为 retry。
- Linux root 环境下测试 fanotify 事件持久化和 overflow 标记。

端到端测试：

- Ubuntu GUI 启动、选择 src/dst、配置保存。
- Ubuntu systemd 启动、状态日志输出。
- Windows GUI 启动和远程 NAS 状态查看。
- NAS SSH 部署、启动、状态检查。

性能测试：

- 大量小文件。
- 单个大文件。
- 高频修改同一文件。
- 多个 dst 同时追赶多个周期。

## 15. 初步里程碑

1. 项目骨架：Cargo、Tauri、目录结构、构建产物复制到 `bin/`。
2. 配置与状态库：`conf/auto_sync.toml`、SQLite schema、配置热加载。
3. 同步核心：snapshot、diff、copy/delete/verify、offset 推进。
4. Linux fanotify watcher：事件持久化、overflow、重启恢复。
5. daemon 与 systemd：后台调度、日志、状态输出。
6. Tauri GUI：src/dst 配置、状态绿点/红点、进度、手动同步。
7. 部署工具：本机安装、NAS SSH 部署、状态检查。
8. 测试与故障恢复：离线、崩溃、overflow、校验失败。

## 16. 待确认问题

编码前建议确认以下点：

1. 同步语义是否接受默认 mirror 模式，即源目录删除后目标目录也删除；或者需要 archive 模式保留历史文件。
2. Windows 是否只作为管理 GUI，还是也必须支持 Windows 本地目录作为 src 自动监听。
3. NAS 的 CPU 架构和系统类型，是否支持 systemd 与 fanotify。
4. 大文件是否需要强 hash 校验，还是 size + mtime + 异常时 hash 即可。
5. `.auto_sync_trash` 删除保留周期，例如 7 天、30 天或手动清理。
