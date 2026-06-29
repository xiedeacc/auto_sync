# auto_sync 系统设计

## 1. 目标与边界

auto_sync 是一个使用 Rust 实现的路径同步工具，支持图形界面配置多组 source path 和 destination path。对 ZFS source，系统以 ZFS snapshot + `zfs diff` 作为强一致基础；fanotify 仅作为 realtime 策略下的近实时触发优化。系统需要把 cycle、snapshot、事件提示和 dst offset 持久化，且在进程重启、目标盘离线、fanotify 队列溢出等情况下仍能通过 snapshot/diff 保证最终一致。

已确认实现决策：

- 同步语义采用 mirror 模式：`src` 删除后，`dst` 对应文件也删除。
- 当前版本先不支持 Windows 平台，编译目标限定为 Linux。
- NAS 确认为支持 systemd 和 fanotify，后台部署按 systemd service 实现。
- GUI 已实现为 Linux Tauri 应用，前端资源位于 `src/ui`，后端命令直接复用 Rust core。
- Headless 场景支持独立 Web UI：`auto_sync_web`，默认监听 `0.0.0.0:18765`。
- 对 ZFS source，snapshot/diff 是推荐后端；fanotify 不再作为强一致的唯一依据。
- 对 realtime schedule，fanotify 提供“尽快同步”的体验，定期 ZFS snapshot reconcile 提供“最终完全一致”的保证。
- fanotify watcher 采用“每个 source 一个 fanotify group/fd”的策略，避免一个 source 的 overflow 污染其他 source。

核心目标：

- 支持多组 `src`，每组 `src` 可以配置多个 `dst`。
- 支持本机部署，也支持部署到 NAS：`ssh -p 10022 root@192.168.3.178`。
- 当前版本只支持 Linux；Ubuntu 支持图形化运行，也支持 systemd 后台运行。
- Linux 对 realtime source 使用 fanotify 监听源路径事件，事件作为触发提示落盘持久化。
- 每个 dst 独立配置 Schedule：`realtime`、`daily` 或 `weekly`。UI 中的 `Schedule` 表示触发规则；后端 `cycle` 表示一次 source 状态版本点。
- UI 中每个 `dst` 用绿点/红点展示是否已经同步到该 dst 的目标 cycle；后台运行时定期把状态输出到日志。
- 通过 cycle offset、ZFS snapshot 名称、dst verified offset、事件提示和落盘状态保证重启后可恢复。
- 不能依赖 fanotify 保证正确性；ZFS source 必须通过 snapshot/diff 和目标目录校验兜底，确保不会遗漏任何最终文件不一致。

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
  conf/                   # 配置文件、状态数据库、systemd/procd 模板
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

- 当前 GUI/Web 直接读写本机配置和 SQLite 状态库。
- 后续如需要常驻 daemon 控制面，可增加 Unix domain socket。
- 远程 NAS 使用 SSH 执行 `auto_syncctl status`、上传配置、启动/停止服务。
- Web/API 控制面配置只保存 `port`，运行时用自动探测到的本机地址加该端口监听，适合 NAS/headless 内网部署。
- Web UI 是配置写入口。生产环境至少需要一种保护：防火墙限制到可信 LAN/IP、反向代理认证、HTTP token/basic auth，或仅在本机地址监听。

当前实现与目标设计：

- Web/Tauri/daemon/CLI 均启用真实同步执行；配置保存后会更新 SQLite 状态，daemon 根据 per-dst schedule 自动推进 target cycle。
- 手动同步语义是“为选定 source 创建一个新的 cycle/snapshot，并让目标 dst 追赶该 cycle”，不是直接复制未落盘状态。
- `snapshot.backend = "auto"` 时，source 可用 ZFS snapshot 视图则使用 ZFS，否则回退 manifest；`snapshot.backend = "zfs"` 时 ZFS 不可用会使该 cycle 失败。

## 4. 配置设计

主配置文件：`conf/auto_sync.toml`。

示例：

```toml
[app]
data_db = "conf/state/auto_sync.sqlite"
log_dir = "logs"
status_log_interval_secs = 300

[[source_groups]]
id = "photos"
src = "/data/photos"
enabled = true
mode = "mirror"             # mirror 为默认；archive 预留

  [source_groups.snapshot]
  backend = "zfs"            # zfs | manifest
  dataset = "tank/photos"
  path_in_dataset = "."
  prefix = "auto_sync"
  reconcile_interval = "15m" # realtime schedule 的兜底 snapshot 周期
  keep_extra_cycles = 2

  [[source_groups.destinations]]
  id = "usb_backup"
  path = "/mnt/backup/photos"
  enabled = true
    [source_groups.destinations.schedule]
    mode = "realtime"        # realtime | daily | weekly
    time = "02:00"           # HH:MM，分钟粒度
    timezone = "local"
    weekday = "monday"

  [[source_groups.destinations]]
  id = "nas_backup"
  path = "/mnt/nas/photos"
  enabled = true
    [source_groups.destinations.schedule]
    mode = "daily"
    time = "02:00"
    timezone = "local"
    weekday = "monday"

[[source_groups]]
id = "docs"
src = "/home/user/docs"
enabled = true

  [[source_groups.destinations]]
  id = "docs_backup"
  path = "/mnt/backup/docs"
  enabled = true
    [source_groups.destinations.schedule]
    mode = "weekly"
    time = "03:00"
    timezone = "local"
    weekday = "sunday"

[[sync_order]]
before = { source_id = "photos", destination_id = "usb_backup" }
after = { source_id = "docs", destination_id = "docs_backup" }

[[machines]]
id = "nas"
name = "nas"
host = "192.168.3.178"
port = 18765
ssh_user = "root"
ssh_port = 10022
os = "linux"
install_dir = "/opt/auto_sync"
```

配置原则：

- GUI 修改配置时先写临时文件，再原子替换 `conf/auto_sync.toml`。
- daemon 监听配置文件变更，重新加载时只增量更新 source group/dst，不清空状态库。
- 每个 `source_groups.id` 和 `destinations.id` 必须稳定，作为 offset 和状态表的外键。
- `dst` 可以离线；离线不删除状态，只标记红点并在下一次在线时从落后周期补齐。
- 每个 `dst` 独立配置 `schedule`。`schedule.mode = "realtime"` 表示 fanotify 触发近实时同步，同时由 source snapshot backend 定期 reconcile。
- `schedule` 是用户可见的触发策略；`cycle` 是后端生成的 source 版本点，两者不能混用命名。
- ZFS snapshot 是 dataset 级别，不是任意目录级别。若 `src` 是 dataset 的子目录，配置需要记录 `dataset` 和 `path_in_dataset`。
- 路径类型规则：`src` 可以是文件或目录；`dst` 可以是目录，且当 `src` 是文件并且 `dst` 路径已经存在为文件时，允许 `src file -> dst file`。如果 `src` 是文件而 `dst` 路径不存在，则按 `src file -> dst dir` 处理；不支持 `src dir -> dst file`。
- auto_sync 只管理自己 `prefix` 命名的 snapshot，不能删除用户手工创建或其他工具创建的 snapshot。

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
  id, src_path, enabled, mode
  snapshot_backend, zfs_dataset, path_in_dataset, snapshot_prefix
  reconcile_interval_secs, keep_extra_cycles, created_at, updated_at

destination
  id, source_group_id, dst_path, enabled
  schedule_mode, schedule_time, schedule_weekday, timezone
  created_at, updated_at

sync_cycle
  id, source_group_id, starts_at, ends_at, status
  snapshot_name, previous_snapshot_name, snapshot_backend
  trigger = realtime | daily | weekly | reconcile | manual
  status = creating_snapshot | snapshot_ready | diffing | planned | syncing | verified | failed

event_log
  event_id, source_group_id, cycle_id, observed_at
  raw_mask, event_kind, rel_path, file_key, is_dir
  coalesce_key, overflow_marker, persisted_at

path_snapshot
  snapshot_id, source_group_id, cycle_id, rel_path
  file_type, size, mtime_ns, mode, uid, gid, content_hash, hash_status

zfs_snapshot
  snapshot_name, source_group_id, cycle_id, dataset
  path_in_dataset, created_at, status
  status = creating | ready | diffing | retained | deleting | deleted | failed

destination_offset
  destination_id, source_group_id
  target_cycle_id
  target_snapshot_name
  last_completed_cycle_id
  last_verified_cycle_id
  last_verified_snapshot_name
  current_cycle_id
  current_event_high_watermark
  status, status_reason, updated_at

destination_snapshot_cursor
  destination_id, source_group_id
  base_snapshot_name
  target_snapshot_name
  target_cycle_id
  status = idle | planning | syncing | verifying | verified | failed

destination_issue
  destination_id, source_group_id
  cycle_id, rel_path, issue_kind, message, updated_at
  issue_kind = source_changing | verify_failed | permission_denied | other

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
- `sync_cycle.snapshot_name` 是 ZFS 后端下的强一致版本点。
- 每个 `dst` 独立维护 `target_cycle_id`、`last_completed_cycle_id` 和 `last_verified_cycle_id`。
- `target_cycle_id` 表示按该 dst schedule 当前必须追赶到的后端版本点。per-dst schedule 下，不能用 source 全局 latest cycle 作为所有 dst 的目标。
- UI 绿点条件：`dst` 在线，且 `last_verified_cycle_id >= target_cycle_id`。
- UI 红点条件：`dst` 离线，或 `last_verified_cycle_id < target_cycle_id`，或最近一次校验失败。
- 后台日志定期输出每个 `dst` 的 `target_cycle_id`、`last_verified_cycle_id`、在线状态、当前任务进度和错误原因。
- `destination_snapshot_cursor` 是 snapshot 清理的显式引用来源。清理器只能删除不被任何 cursor、未完成 task、保留窗口引用的 snapshot。

## 6. fanotify 事件设计

Linux 后台进程对 `schedule.mode = "realtime"` 的 source 使用 fanotify。fanotify 的职责是尽快发现运行期间的变化并触发近实时同步，不承担最终一致性的唯一责任。ZFS source 的最终一致性由周期性 ZFS snapshot + `zfs diff` reconcile 保证。

当前实现说明：

- 当前 Linux watcher 优先使用 FID/name 模式：`FAN_REPORT_FID | FAN_REPORT_DIR_FID | FAN_REPORT_NAME | FAN_REPORT_TARGET_FID`。
- FID/name 模式注册 `FAN_MODIFY | FAN_CLOSE_WRITE | FAN_CREATE | FAN_DELETE | FAN_MOVED_FROM | FAN_MOVED_TO | FAN_DELETE_SELF | FAN_MOVE_SELF | FAN_ONDIR`，用于覆盖 modify/create/delete/move 以及目录自身变化。
- watcher 启动时为 source 下所有文件和目录建立 `file handle -> path` 表；带 name 的事件用 parent directory handle + name 拼路径，不带 name 的 target FID 事件用对象 handle 查路径。新建文件/目录被解析后会补充进 handle 表，新建目录还会递归注册 mark。
- 如果 FID/name 模式或 filesystem mark 不可用，会回退到传统 fd-path 模式；fd-path 模式只注册 `FAN_MODIFY | FAN_CLOSE_WRITE`。
- 若 filesystem/mount mark 不可用，会回退到对 src 目录树逐目录注册 inode mark。
- 无法解析路径的 FID/name 附属记录不会把 realtime cycle 标记为 Full/rescan；realtime 自动同步仍只按已解析的 event path 做增量同步。queue overflow 仍会写入需要 reconcile 的事件。
- 删除、rename 和部分元数据变化不依赖事件流保证最终正确性，而是由周期关闭时的源目录快照和 dst 校验/reconcile 兜底。

建议 fanotify 策略：

- 每个 source group 单独创建一个 fanotify group/fd/读取循环。
- 一个 source 的 `FAN_Q_OVERFLOW` 只影响该 source，不把其他 source 标记为不可信。
- 使用 `FAN_REPORT_FID`、`FAN_REPORT_DIR_FID`、`FAN_REPORT_NAME` 获取稳定文件标识和目录项信息。
- 关注事件：create、modify、close_write、delete、move_from、move_to、attrib、delete_self、move_self、queue_overflow。
- 对同一文件在同一周期内的多次修改只在同步计划阶段合并，但原始事件仍写入 `event_log`。
- 每个 fanotify 事件必须先持久化到 SQLite，再更新内存状态。
- 如果收到 queue overflow，写入 `overflow_marker = true` 的事件，并把对应 source group 标记为 `needs_reconcile`。
- 如果进程重启，daemon 先从状态库恢复未完成 cycle、snapshot cursor、未完成任务和 destination offset，再重新注册 fanotify mark。

fanotify 重要边界：

- fanotify 不应作为唯一正确性来源。rename、临时文件写入、权限变化、事件队列溢出、挂载点变化都可能导致事件流不足以直接推导最终状态。
- fanotify group 是进程通过 `fanotify_init()` 创建的内核对象，生命周期绑定 fd；进程退出后 group 和未读事件都会消失。
- 开机、关机、服务重启、mark 尚未注册、队列 overflow 期间都可能丢事件。
- 对 ZFS source，丢事件不触发百万文件全量扫描，而是触发下一次 snapshot/diff reconcile。
- 对非 ZFS source，丢事件后只能回退到 manifest/reconcile 策略。

## 7. ZFS snapshot 后端设计

ZFS 后端是百万文件规模下的首选一致性方案。创建 snapshot 通常是近似瞬时操作，不随文件数量线性复制；真正需要关注的是 snapshot 数量、`zfs diff` 耗时和 snapshot 清理策略。

source 与 dataset 关系：

- ZFS snapshot 作用于 dataset，不作用于任意目录。
- 如果 source 是 dataset 挂载点，例如 `/tank/photos`，则 `dataset = "tank/photos"`，`path_in_dataset = "."`。
- 如果 source 是 dataset 子目录，例如 `/tank/photos/upload`，则仍对 dataset `tank/photos` 创建 snapshot，并在解析 `zfs diff` 时只保留 `upload/` 下的路径。
- 自动识别时可用 `zfs list -H -t filesystem -o name,mountpoint`，对 source realpath 做最长 mountpoint 前缀匹配。

snapshot 命名：

```text
<dataset>@<prefix>_<source_id>_<cycle_id>
```

示例：

```text
tank/photos@auto_sync_photos_000001
tank/photos@auto_sync_photos_000002
```

流程：

1. 到达某个 dst schedule 触发点，或 realtime reconcile interval 到达。
2. 对 source 所在 dataset 创建新的 ZFS snapshot。
3. 在 `sync_cycle` 和 `zfs_snapshot` 表中记录 snapshot 名称、dataset、cycle id。
4. 如果存在上一个 retained snapshot，执行：

   ```bash
   zfs diff <old_snapshot> <new_snapshot>
   ```

5. 解析 diff 输出，过滤到 `path_in_dataset` 下的相对路径，生成 copy/delete/rename/metadata 任务。
6. 每个 dst 独立追赶到对应 cycle/snapshot。
7. dst 校验通过后记录 `last_verified_cycle_id` 和 `last_verified_snapshot_name`。

snapshot 清理：

- auto_sync 只删除自己 `prefix` 命名的 snapshot。
- `destination_snapshot_cursor` 显式记录每个 dst 的 base snapshot、target snapshot 和 target cycle。
- 某个 snapshot 只有在所有 enabled dst 都已经 verified 到它之后，并且它不再被任何 cursor、未完成 task、diff plan 或保留窗口引用时，才允许删除。
- 至少保留最新 snapshot、每个落后 dst 所需的 base snapshot，以及 `keep_extra_cycles` 指定的额外历史窗口。
- 清理逻辑按 snapshot refcount 执行：`refcount = cursor 引用 + running task 引用 + keep_extra_cycles 引用 + 最新 snapshot 引用`。只有 refcount 为 0 的 auto_sync snapshot 可进入 deleting。
- 删除或禁用某个 dst 后，必须释放该 dst 的 `destination_snapshot_cursor` 引用，并取消/归档它未完成的 task；随后重新计算 snapshot refcount。被该 dst 独占引用的旧 snapshot 可以在下一轮清理中删除。
- 删除 dst 不应立即同步删除 dst 目录中的备份数据；它只表示 auto_sync 不再维护该 dst，也不再为它保留 snapshot base/target。
- 删除 snapshot 可能释放大量 block，清理应在后台限速执行，失败只记录状态，不影响当前同步。

Realtime + ZFS：

- realtime schedule 使用 fanotify 触发近实时同步。
- 同时按 `reconcile_interval` 定期创建 ZFS snapshot，并用 `zfs diff` 补齐 fanotify 漏掉的变化。
- UI 绿色状态以该 dst 的 `target_cycle_id` 是否 verified 为准；fanotify 实时任务完成只能表示“近实时队列已处理”，不能单独证明完全一致。

非 ZFS fallback：

- 如果 source 不在 ZFS dataset 上，snapshot backend 使用 manifest。
- manifest 模式可以使用 fanotify 事件缩小扫描范围，但进程停机或 overflow 后仍需要分片 reconcile。
- manifest 模式不能像 ZFS diff 一样低成本覆盖停机窗口，百万文件场景应优先建议用户把 source 放到 ZFS dataset 上。
- 对 ext4 等无原生快照的 source，manifest 模式只能从 live source 读取。正在增长或被持续写入的文件在复制期间可能变化，必须通过复制前后 fingerprint 校验发现不稳定输入；一旦发现 size、mtime 或 hash 变化，该文件任务和对应 cycle/dst 标记 failed 或 retry，不推进 `last_verified_cycle_id`。
- ext4 source 的 realtime 同步建议等待 `FAN_CLOSE_WRITE`、文件静默窗口或应用日志轮转后再复制；对持续 append 的日志文件，可以配置 exclude/defer 策略，或要求应用先 rotate/close 后再进入 verified cycle。
- 如果 ext4 source 需要和 ZFS 同级的一致性，应把 source 放到 ZFS dataset，或使用 LVM/dm-thin snapshot、fsfreeze + snapshot、应用级 quiesce hook 等外部快照机制。没有稳定快照时，auto_sync 只能保证“不发布检测到不一致的结果”，不能保证一次 cycle 内复制到一个正在写入文件的完整最终版本。

## 8. Schedule 调度与 cycle 生成

概念区分：

- `Schedule` 是每个 dst 的用户配置，表示触发规则：`realtime`、`daily`、`weekly`。
- `cycle` 是后端生成的 source 版本点。对 ZFS source，cycle 对应一个 ZFS snapshot。
- cycle 创建时间不是备份完成时间；某个 dst 完成备份后只是把自己的 `last_verified_cycle_id` 推进到该 cycle。

调度模型：

- 每个 dst 独立判断自己的 schedule 是否到期。
- 对同一 source，如果多个 dst 在同一时间窗口需要同一个版本点，应复用同一个 cycle/snapshot。
- `daily` schedule 以配置的本地时间每天触发。
- `weekly` schedule 以配置的星期几和时间触发。
- `realtime` schedule 使用 fanotify 尽快触发小批量同步，并按 source 的 `reconcile_interval` 定期创建 ZFS snapshot 兜底。
- source group 不再依赖一个全局 daily/weekly schedule。

流程：

1. daemon 启动后加载配置和状态库。
2. 对每个 source group 恢复已存在的 cycle/snapshot 状态。
3. 对 realtime source 启动 fanotify group，事件持续写入 `event_log`，作为触发提示。
4. 当某个 dst schedule 到期，或 realtime reconcile interval 到期时，为 source 创建新的 cycle。
5. ZFS 后端对 cycle 创建 snapshot，并用上一个 retained snapshot 到当前 snapshot 的 `zfs diff` 生成同步计划。
6. 将触发该 schedule 的 dst 的 `target_cycle_id` / `target_snapshot_name` 更新到该 cycle；同一 source 的其他 dst 只有在自己的 schedule 到期或需要 reconcile 时才更新 target。
7. 为需要追赶该 cycle 的 enabled dst 生成 sync_task。
8. 在线 dst 执行同步；离线 dst 保持红点，等待后续恢复。
9. sync_task 全部完成并通过校验后，更新该 dst 的 `last_verified_cycle_id` 和 `last_verified_snapshot_name`。
10. snapshot 清理器根据 `destination_snapshot_cursor` 和所有 dst 的 verified offset 决定哪些旧 snapshot 可以删除。

手动同步：

- 手动同步会为全部 source 或选定 source 关闭当前 open cycle，创建新的 target cycle，并让 enabled dst 追赶该 cycle。
- 若目标离线或校验失败，不推进 `last_verified_cycle_id`，下一轮会继续重试该 target cycle。

当前实现的 Changed Since sync：

- Changed Since sync 只由用户在单个 destination 的 `Sync... -> Changed Since` 手动触发。它适合 source 已推进到较新的 cycle，而该 destination 还停在较旧 verified cycle，只想追赶旧 cycle 之后变化路径的场景。
- 手动 Changed Since 会关闭该 source 当前 open cycle，只把当前 destination 的 `target_cycle_id` 指向这个新 cycle，并把 cycle 标记为 `manual_changed_since_rescan = 1`。它不会设置 `needs_full_rescan`，因此语义上不是 Full。
- 执行时先对 source 做完整 snapshot/manifest 扫描，并将当前完整 `source_snapshot` 写入 `path_snapshot`。随后读取该 destination 的 `last_verified_cycle_id`，找到对应 source cycle 的时间点；时间取该 source cycle 的 `ends_at`，如果没有则回退到 `starts_at`，并转换成 Unix epoch nanoseconds。这个时间来自 source cycle，而不是当前机器执行同步时的 wall-clock。
- 路径计划由当前完整 `source_snapshot` 和 last verified cycle 的历史 `path_snapshot` 生成：当前条目的 mtime 晚于上述 cycle 时间、当前条目相对历史 snapshot 有 metadata/content 变化、新增路径、以及历史 snapshot 中存在但当前 source 已不存在的路径，都会加入待同步相对路径集合。
- 对待同步路径集合执行局部 source/destination snapshot、复制、类型替换、mirror 删除和局部校验。没有出现在该集合中的 destination-only 文件不会被扫描或删除；需要验证整棵目标树时使用 Full。
- 如果 destination 从未 verified、找不到它的基线 source cycle，或基线 `path_snapshot` 没有可用条目，Changed Since 没有可靠的时间锚点，会降级为完整 reconcile。

当前实现的 Full sync：

- Full sync 只由用户在单个 destination 的 `Sync... -> Full` 手动触发。`Sync All`、source 级 `Sync`、destination 级 `Incremental`、以及 realtime watcher 自动触发都按 incremental 处理。
- 手动 Full 会先保存配置，然后调用 `sync_destination_now_with_mode(..., Full)`：关闭该 source 当前 open cycle，只把当前 destination 的 `target_cycle_id` 指向这个新 cycle，并把 cycle 标记为 `needs_full_rescan = 1`、`manual_full_rescan = 1`。
- realtime destination 也允许手动 Full。`manual_full_rescan` 是 realtime 的显式例外：同步执行时会跳过 event-path realtime incremental 分支，进入完整 reconcile。fanotify overflow、USN gap 等自动产生的 `needs_full_rescan` 事件不会偷偷执行 Full；它们会让该 realtime cycle 标记为需要人工/完整处理，而不是自动全量扫描。
- Full sync 不是“删除目标后全部重传”。它会对 source 做完整 snapshot/manifest 扫描，并对目标 destination root 做完整 snapshot/manifest 扫描，然后按相对路径比较两边状态。
- 对目录 source，完整 reconcile 的执行顺序是：
  1. 读取 source path 信息，确认 source 是目录或文件。
  2. 对目录 source，生成完整 `source_snapshot`，并写入 `path_snapshot`。
  3. 对当前 destination 生成完整 `dst_snapshot`。
  4. 如果同一路径 source/dst 类型不同，先删除或替换目标端旧路径。
  5. 批量创建 source 中存在的目录。
  6. 对 source 中的文件和 symlink，只复制目标端缺失或 metadata/content 不匹配的条目；已匹配文件不会重传。
  7. mirror 模式下，删除目标端存在但 source_snapshot 中不存在、且不在 exclude 规则内的额外路径。
  8. 清理本 cycle 的临时传输目录。
  9. 再次对目标端做 snapshot，并用 `verify_snapshot_entries` 校验目标端与 source_snapshot 一致。
- 对文件 source，Full sync 只围绕该单个文件生成 source snapshot，并同步到目标文件或目标目录下的同名文件；不支持 `src dir -> dst file`。
- 跨机器 Full sync 使用同一语义，但 source/destination snapshot、批量 mkdir、批量 remove 和文件推送通过 peer HTTP transfer API 执行。大文件传输仍会按差异策略决定是 delta 还是流式全量传输；这不改变 Full sync 的“完整扫描 + 只修复差异”语义。
- Full sync 成功后，目标 destination 的 `last_verified_cycle_id` 推进到该 cycle，并显示绿色。任何复制、删除或最终校验失败都不会推进 verified offset，目标保持红/黄状态并在下一轮继续处理。

## 9. 同步一致性设计

目标语义：每个 `dst` 完成 cycle N 后，应与 source group 在 cycle N 对应 snapshot 中的源状态一致。

同步计划生成：

- ZFS 后端输入：当前 cycle snapshot、上一 retained/base snapshot、`zfs diff` 输出、dst 当前状态。
- fanotify event_log 输入只作为 realtime 快速同步提示，不作为最终计划的唯一依据。
- ZFS 后端的所有 copy/verify source view 必须来自只读 snapshot 路径，而不是 live source 路径。
- 输出：copy、mkdir、metadata、delete、verify 任务。
- 对于同一 rel_path 的多次事件，任务层合并为最终动作。
- 对于 rename，如果事件信息完整，可优化为 dst rename；否则退化为 copy 新路径 + delete 旧路径。

文件复制策略：

- ZFS 后端 source 路径必须解析到 snapshot 只读视图，例如 `<dataset_mountpoint>/.zfs/snapshot/<snapshot>/<path_in_dataset>/<rel_path>`；如果 `.zfs/snapshot` 不可见，应使用 ZFS clone、临时只读挂载或等价方式提供稳定 snapshot view。
- 拷贝到目标目录同文件系统下的临时路径：`.auto_sync_tmp/<cycle>/<rel_path>.tmp`。
- 拷贝完成后 fsync 文件和父目录。
- 校验 size、mtime 和可选 hash；大文件可先 size+mtime，异常时再 hash。
- 校验通过后原子 rename 到最终路径。
- 目录 mtime 不在目录创建时设置；所有子文件/子目录复制、删除和 rename 完成后，按 deepest-first 顺序批量设置目录 mode/mtime，避免子项操作再次改变父目录 mtime。
- 因为读取来自 snapshot，同一个 cycle 内 source fingerprint 必须稳定；若 snapshot 读取失败或 fingerprint 与 plan 不一致，该 cycle/task 标记 failed，不能改读 live source。

正在增长文件策略：

- ZFS source：cycle 创建时先创建 ZFS snapshot，后续 copy/verify 都从 snapshot 只读视图读取。即使 live 文件继续增长，本 cycle 同步的是 snapshot 时刻的文件版本；下一次 cycle/snapshot 再同步新增内容。因此增长中的日志文件不会导致本 cycle 读到半截变化数据。
- ZFS snapshot 提供的是文件系统 crash-consistent 视图，不等同于应用事务一致性。若应用需要记录边界一致，应配合应用日志 rotate、flush/fsync、pre/post hook 或数据库自身备份接口。
- ext4/manifest source：没有稳定 snapshot 时，不允许假装 live source 是稳定版本。执行器应记录复制前 fingerprint，复制到临时文件后重新读取 source fingerprint 并校验目标；若 source 在复制期间变化，删除该文件临时副本，记录 `destination_issue(issue_kind = source_changing)`，继续同步其他稳定文件。该 dst 最终保持黄色并等待下一次 retry/reconcile，不推进 verified offset。
- 对持续 append 文件，默认策略是失败并重试，防止把无法证明一致的版本标绿。可选优化包括 debounce 静默窗口、只在 `close_write` 后复制、按 glob exclude 某些临时/日志文件、或配置“允许复制增长中文件的当前前缀但不推进 verified offset”的弱一致模式；弱一致模式必须在 UI 中明确标识。
- 对大文件重试要有退避和最大重试次数，避免热日志导致无限占用 IO。超过阈值后输出明确错误原因，并建议用户将 source 迁移到 ZFS 或配置应用级 quiesce。

Realtime 快速同步语义：

- fanotify 触发的 realtime 快速同步可以直接更新正式 dst，以提供接近实时的体验。
- Windows realtime 优先使用 USN Journal；如果当前进程权限或环境无法查询 USN Journal，则降级到 `ReadDirectoryChangesW` 递归目录 watcher，仍然按相对路径写入 `event_log` 触发增量同步。
- Windows 启动时默认要求 elevated 进程：如果当前进程不是管理员且还没有尝试过提权，`auto_sync` 会用 `runas` 重新拉起同一配置的 elevated 进程，并退出当前非 elevated 进程；用户需要在 UAC 中确认。若 elevated 启动失败，则继续以普通权限运行，并在 USN 不可用时使用 `ReadDirectoryChangesW` fallback。
- 进程启动或 watcher 因配置变化重启时，会对本机 realtime source 做一次 mtime 补漏扫描：读取该 source 最近一次 `event_log.observed_at` 作为 cutoff，递归扫描 source 下文件和目录，mtime 晚于 cutoff 且 cutoff 之后没有同路径事件的条目会补写 `startup_mtime_scan` 增量事件。该机制只补 event-path 增量事件，不自动触发 Full sync。
- `startup_mtime_scan` 事件只在其所在 cycle 被标记为 `verified` 后清理；多 dst 场景下，如果某个 dst 尚未完成该 cycle，这些补漏事件仍会保留。
- realtime 快速同步完成只能说明“事件提示队列已处理”，不能证明 dst 与最新 snapshot 完全一致。
- 只有 snapshot/diff reconcile 完成并校验通过后，才能推进 `last_verified_cycle_id` 并显示绿点。
- 如果 realtime 写入与后续 snapshot diff 结果冲突，以 snapshot diff/reconcile 为准修正 dst。

删除策略：

- mirror 模式下，源快照不存在但 dst 存在的文件需要删除。
- 为避免误删，先移动到 `.auto_sync_trash/<cycle>/...`，完成整个 cycle 校验后再按保留策略清理。
- 如果用户选择 archive 模式，则不执行删除，只记录额外文件状态；该模式需要单独确认。

校验策略：

- 每个任务完成后做局部校验。
- 每个 dst 完成 cycle 后做周期级校验：抽样或全量对比可配置；发生 zfs diff 失败、fanotify overflow、异常重启、上次失败、dst 离线恢复时强制更严格校验。
- 校验失败不会推进 `last_verified_cycle_id`。
- 未推进 offset 的 dst 永远显示红点，并在后台日志中输出原因。

保证“不丢失任何文件不一致”的关键机制：

- 事件先落盘，再进入内存队列。
- fanotify overflow 或 watcher 中断会标记 needs_reconcile。
- ZFS source 基于 snapshot/diff 生成最终同步计划，而不是只相信事件。
- 每个 dst 有独立 cycle offset，离线后不会跳过周期；恢复后从最旧未验证周期开始追赶。
- offset 只在同步任务完成且校验通过后推进。
- 临时文件 + fsync + 原子 rename 降低崩溃导致半文件的风险。
- 重启时恢复 running/retry 任务，未验证的 cycle 会重新校验或重做。

## 10. dst 在线状态与 UI 状态

dst 在线检测：

- 路径存在且可读写。
- `dst` 所在设备已挂载；Linux 可通过 `statfs`、`/proc/self/mountinfo` 或设备 ID 判断。
- 对目录 dst，可在 dst 下创建和删除 `.auto_sync_probe` 临时文件。
- 对文件 dst，检测其父目录是否在线且可写；文件 dst 只有在该路径已经存在且不是目录时才按文件目标处理。
- 目标目录剩余空间满足当前待同步任务估算。

UI 状态：

- source group 页面展示每个 `src` 和它的多个 `dst`。
- 每个 dst 独立展示 `Schedule` 和 `Cycle`：
  - `Schedule`：用户配置的触发规则。`realtime` 显示 `Realtime`，`daily` 只显示时间如 `02:00`，`weekly` 显示三字母星期缩写加时间如 `Mon 02:00`。
  - `Cycle`：后端版本进度，例如 `verified_cycle / target_cycle`。
- 每个 dst 显示红点、黄点或绿点：
  - 绿点：在线，且 `last_verified_cycle_id` 已达到该 dst 的 `target_cycle_id`。
  - 黄点：dst 在线，但 ext4/manifest live-copy 检测到 source 文件正在变化，当前 cycle 不能证明一致；点击黄点弹窗展示 `destination_issue` 中记录的文件列表、cycle 和原因。
  - 红点：离线、落后、同步失败或校验失败。
- 红点 tooltip 或详情面板展示原因：离线、落后几个周期、最近错误、剩余任务数。
- 支持添加/删除/编辑 source group 和 destination。
- 删除 destination 时，UI 只移除配置并释放该 dst 对 snapshot 的保留引用；不会自动删除该 dst 路径下已有备份数据。
- 支持选择 src 文件或目录，以及 dst 目录或已经存在的 dst 文件。
- 支持查看后端 cycle 进度、上次同步时间、下次 schedule 时间、同步进度。
- 支持 `Sync All`、单个 source 的 `Sync Now` 和单个 destination 的 `Sync Now`，触发前先保存当前配置。source 级同步会让该 source 下所有 enabled dst 追赶同一 cycle；destination 级同步只推进当前 `src -> dst`。

后台日志状态：

daemon 每 `status_log_interval_secs` 输出一次：

```text
source=photos dst=usb_backup online=true target_cycle=42 verified_cycle=42 status=green pending=0
source=photos dst=nas_backup online=false target_cycle=40 verified_cycle=39 status=red reason=dst_offline
```

## 11. 部署设计

本机部署：

- 构建 Rust/Tauri 产物。
- 复制二进制到 `bin/`。
- 初始化 `conf/auto_sync.toml` 和 `conf/state/`。
- 初始化 `logs/`。
- Ubuntu/NAS 后台模式安装 systemd unit；OpenWrt 安装 procd init。
- Linux 机器（tiger Linux、NAS、OpenWrt）共用 `conf/auto_sync.linux.toml` 作为部署配置模板。

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

1. 通过 SSH 执行 `uname -m`、`uname -s`，检测架构和 init 系统（systemd 或 OpenWrt procd）。
2. 根据架构选择、交叉编译，或在目标 Linux 机器本机编译二进制。
3. 上传 `bin/auto_sync`、`bin/auto_syncctl`、`conf/auto_sync.linux.toml`，并按 init 系统安装 systemd unit 或 `conf/auto_sync.procd`。
4. 执行权限设置：`chmod +x /opt/auto_sync/bin/*`。
5. 如果支持 systemd，安装并启动 `auto_sync.service`；如果是 OpenWrt，安装并启动 `/etc/init.d/auto_sync`。
6. 通过 `auto_syncctl status` 验证状态。

配置部署原则：

- Linux 部署把 `conf/auto_sync.linux.toml` 安装为 `/opt/auto_sync/conf/auto_sync.toml`。
- OpenWrt 部署把 `conf/auto_sync.procd` 渲染为 `/etc/init.d/auto_sync`。
- 覆盖配置必须使用显式参数，例如 `--overwrite-config`，并先保存时间戳备份。

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

## 12. 日志与可观测性

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

## 13. 错误恢复

进程重启：

- 读取状态库。
- 将 running 状态任务恢复为 retry。
- 未 verified 的 cycle 重新进入 planning/syncing。
- 对 realtime source 重新创建每个 source 独立的 fanotify group 并注册 mark。
- 对 ZFS source 列出 auto_sync 前缀的 snapshot，与 SQLite 中的 `zfs_snapshot` 和 `sync_cycle` 对账。
- 若发现服务停止期间存在未 reconcile 窗口，则创建新的 snapshot，并用最近 retained snapshot 到新 snapshot 的 `zfs diff` 补齐。

dst 离线：

- 在线检测失败时不执行同步。
- `destination_offset.status = offline`，UI 红点。
- 不推进 cycle offset。
- 定期探测恢复；恢复后从最旧未 verified cycle 开始同步。

fanotify overflow：

- 写入 overflow 事件。
- 仅对应 source group 标记 `needs_reconcile`。
- ZFS source 在下一次 reconcile interval 或立即创建新 snapshot，通过 `zfs diff` 补齐事件丢失窗口。
- 非 ZFS source 回退到 manifest 分片 reconcile。

磁盘空间不足：

- task 标记 failed，dst 红点。
- 不推进 verified cycle。
- 后台日志输出所需空间估算和剩余空间。

拷贝中断：

- 临时文件保留在 `.auto_sync_tmp`。
- 重启或下一次同步时清理同 cycle 旧临时文件并重做任务。

## 14. 安全设计

- SSH 部署优先使用密钥认证，不在配置文件保存明文密码。
- GUI 与 daemon 的本机 IPC 只允许当前用户或 root 访问。
- 远程控制走 SSH，不暴露网络监听端口。
- 所有路径配置必须规范化，禁止 dst 配置到 src 的子目录，避免递归同步。
- 启动时检测 source groups 之间是否互相嵌套；如果嵌套，需要明确优先级或拒绝启动。
- 删除操作默认进入 `.auto_sync_trash`，并记录 audit log。

## 15. 测试计划

NAS 真实测试约定：

- NAS 上 `/zfs/tmp` 是 auto_sync 的可清理测试目录，允许创建、覆盖和删除 auto_sync 测试用文件、目录、软链、`.auto_sync_tmp`、`.auto_sync_trash` 以及 `auto_sync_*` 前缀的临时测试子目录。
- 测试前后必须清理 `/zfs/tmp/auto_sync_*`、`/zfs/tmp/.auto_sync_tmp`、`/zfs/tmp/.auto_sync_trash`，并清理 `@auto_sync_` 或测试专用 `@auto_sync_real_` 前缀的 ZFS snapshot。
- 不允许把 `/zfs/tmp` 以外的 `/zfs` 顶层目录作为随意清理目标；真实业务目录必须显式确认后才能写入或删除。
- 当前 NAS 控制面 URL 为 `http://192.168.3.178:18765/`；部署测试只重启 Web UI 时，后台 `auto_sync.service` 可以保持 stopped/inactive，避免意外自动同步。

单元测试：

- 配置解析和校验。
- daily/weekly 周期计算。
- event coalesce 逻辑。
- destination offset 推进规则。
- ZFS dataset mountpoint 最长前缀匹配。
- ZFS snapshot 命名、保留和清理判定。
- `zfs diff` 输出解析和 sync plan 生成。
- 正在增长文件在 manifest/ext4 模式下应触发 `source_changed_while_copying` 或等价错误，不推进 verified offset。

集成测试：

- 使用临时目录模拟 src/dst，验证新增、修改、删除、rename。
- 模拟 dst 离线后恢复，确认从落后 cycle 补齐。
- 模拟进程在 copy 后、rename 前、verify 前崩溃，确认重启可恢复。
- 模拟 SQLite 中 running task，确认恢复为 retry。
- Linux root 环境下测试每 source fanotify group 的事件持久化和 overflow 标记。
- ZFS 环境下测试 snapshot 创建、diff、dst 追赶和 snapshot 清理。
- 在 `/zfs/tmp` 下创建 ZFS-backed 测试 source/dst，验证 snapshot 视图下复制、更新、删除、软链、extra 文件清理和 `.auto_sync_tmp` 清理。
- 使用非 ZFS 或 manifest backend 测试持续 append 文件，确认同步失败原因可见且不会把 dst 标记为 green。

端到端测试：

- Linux Tauri GUI 启动、选择 src/dst、配置保存。
- Ubuntu systemd 启动、状态日志输出。
- Web UI 启动和远程 NAS 状态查看。
- NAS SSH 部署、启动、状态检查。
- NAS Web UI 使用 `/usr/local/tbox -> /zfs/tmp` 或 `/zfs/tmp/auto_sync_real_src -> /zfs/tmp/auto_sync_real_dst` 做真实手动 Sync Now 测试；`/usr/local/tbox` 中若存在持续增长日志，预期 manifest 模式会失败并保持红点。

性能测试：

- 大量小文件。
- 单个大文件。
- 高频修改同一文件。
- 多个 dst 同时追赶多个周期。

## 16. 初步里程碑

1. 项目骨架：Cargo、Tauri、目录结构、构建产物复制到 `bin/`。
2. 配置与状态库：`conf/auto_sync.toml`、SQLite schema、配置热加载。
3. ZFS snapshot backend：dataset 识别、snapshot 创建、`zfs diff` 解析、snapshot 清理。
4. 同步核心：diff plan、copy/delete/verify、offset 推进。
5. Linux fanotify watcher：每 source 一个 group、事件持久化、overflow、重启恢复。
6. daemon 与 systemd：后台调度、日志、状态输出。
7. Tauri/Web UI：src/dst 配置、Schedule 配置、Cycle 进度、状态绿点/红点。
8. 部署工具：本机安装、NAS SSH 部署、状态检查；Linux 根据 GUI 环境安装 GUI+headless 或仅 headless。
9. 局域网多机：Web 状态栏显示在线机器数，UDP 自动发现机器，支持手动机器、远程目录浏览、rsync 跨机器同步以及 remote-to-remote runner 模式。
10. Windows peer：Windows 使用 OpenSSH + cwRsync runtime，路径转换为 `/cygdrive/<drive>/...`。
11. 测试与故障恢复：离线、崩溃、overflow、ZFS snapshot 清理、校验失败。

## 17. 待确认问题

编码前建议确认以下点：

1. 同步语义是否接受默认 mirror 模式，即源目录删除后目标目录也删除；或者需要 archive 模式保留历史文件。
2. NAS 的 CPU 架构和系统类型，是否支持 systemd 与 fanotify。
3. NAS 上 source 所在 ZFS dataset 名称、挂载点和是否存在 nested dataset。
4. Realtime schedule 的默认 `reconcile_interval`，例如 5 分钟、15 分钟或 1 小时。
5. 大文件是否需要强 hash 校验，还是 size + mtime + 异常时 hash 即可。
6. `.auto_sync_trash` 删除保留周期，例如 7 天、30 天或手动清理。
7. Windows 是否需要内置安装 OpenSSH/cwRsync 的远程引导流程，而不仅是提供 runtime 包。
