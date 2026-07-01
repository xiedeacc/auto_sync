# auto_sync 代码与方案设计 Review

> 范围：全量源码 + `system_design.md`。本文只做评审，不改代码。
> 行号基于评审时的工作树（`src/core/...`）。
> 严重度：**Critical / High / Medium / Low**。

---

## 0'. 状态更新（2026-07-02，四轮 review 后）

本文最初的行号和部分结论已过时。当天完成了 Full / Compare / Changed-Since / Incremental 四轮 review 并落地修复（commits `ffdc323` → `7a23723`），各编号问题现状：

| # | 原结论 | 现状 |
|---|--------|------|
| F1 | `reconcile_interval` 未接线，realtime 无兜底 | **部分解决 + 用户决策**：疑似丢事件（overflow/USN gap/启动 gap）已自动触发完整 reconcile（红色 `event_loss_reconcile`，修复含删除在内的一切差异）；周期性对账实现后按用户明确要求移除（7a23723）——漂移由手动 `Scan` 检查、`Full` 修复。realtime 绿点语义 = "事件已应用"，为用户接受的取舍。另修复计划顺序 bug：被标记 rescan 但无可执行事件的 cycle 不再空计划盖绿章。 |
| F2 | overflow 卡黄无自愈 | **已修复**：rescan 事件自动升级完整 reconcile。 |
| F3 | USN 启动 gap 不置 rescan | **已修复**（2026-06-30 起 needs_full_rescan 接线）。 |
| F4 | fsync 默认关闭 | **未变**（用户曾搁置该决策，改动前需再确认）。 |
| F5 | put-file/chunk 只校验 size | **已修复**：所有跨机传输路径均带 blake3 全文件端到端校验（put-file、chunked finish、delta）。 |
| F6 | peer API 无鉴权 + body 无上限 | **未变**（内网部署，用户接受；改动前需确认）。 |
| F7 | 整文件入内存 × 并发 | **部分修复**：delta 发送端接入全局 1 GiB 内存预算（`acquire_transfer_memory`）；chunked 路径本就流式。接收端 body 缓冲未变。 |
| F8/F9 | watcher 单错误死亡 / handle_paths 泄漏 | **未变**。 |
| §1.4 | fast-missing-dirs 标绿不校验 | **已修复**：改为对本轮写入条目逐路径校验（含批量复制的子树）。 |
| §2.1 | 单文件失败废弃整轮、无重试分类 | **已修复**：source_changing 容忍（黄）、单文件失败容忍至 20 个、连接级错误即断、终态错误不重试；错误正文跨 HTTP 边界传递。 |
| §4 | 长事务锁竞争 | **已缓解**：path_snapshot 批量写/删分块（20K 行/事务）；event_log、path_snapshot 均有修剪；空闲时零 DB 写入（配置指纹门控）；watcher 事件即时唤醒调度器。 |

其余新增能力：跨机 cycle 的 ZFS snapshot 只读视图 + `zfs diff` 快路径、Full/Compare 两端并行扫描、Compare 失败可见（错误报告落库）、全树 snapshot 请求超时下限（1h/6h）、Changed-Since 基线回退与 hash 存在性容忍、SnapshotEntry 空 hash 不上线。

以下原文保留作历史评审记录。

---

## 0. 结论速览

整体架构是合理的：snapshot 提供稳定读视图、临时文件 + 原子 rename、per-dst offset、WAL + `synchronous=FULL` 的状态库、watcher 分 source 隔离——这些都符合“最终一致 + 不轻易标绿”的设计意图。

但是**当前实现与设计文档在几个关键的“正确性兜底”上存在偏离**，导致设计文档承诺的“即使丢事件也能最终一致”在 realtime 场景下基本没有兑现。最需要优先处理的是下面这组：

| # | 严重度 | 一句话 | 位置 |
|---|--------|--------|------|
| F1 | **Critical** | `reconcile_interval` 被解析校验但从未使用——realtime 的周期性 snapshot reconcile 兜底根本没接线；溢出/gap 标黄后无自愈，只能人工 Full | `state.rs:341` / `config.rs:115,279` |
| F2 | **High** | fanotify 溢出虽正确置 `rescan=true`→cycle 转 Unusable/yellow，但因 F1 永不自动 reconcile，会长期卡黄需人工 Full | `fanotify.rs:386` |
| F3 | **Critical** | Windows USN **启动时**确实丢事件的场景反而 `rescan_required=false`，cycle 带空计划直接转 green（运行中同样的 gap 却置 true，自相矛盾） | `windows_usn.rs:269,280` |
| F4 | **High** | fsync 默认关闭——崩溃/掉电后刚同步完的文件可能变成 0 长度/垃圾，但仍显示已完成 | `config.rs`、`mod.rs:4806,4930` |
| F5 | **High** | 跨机快速路径（put-file / chunk）只校验 size、不校验 hash，静默损坏可通过 | `mod.rs:739,707,917` |
| F6 | **High** | peer 传输 API 无鉴权 + `DefaultBodyLimit::disable()` + 整体缓冲，可被任意 LAN 端写/删任意路径并 OOM | `web_api.rs:93` |
| F7 | **High** | 整文件 `fs::read` 入内存（最大 1GB）× 并发 worker，双端 RAM 可被打爆 | `mod.rs:1027,1080,765` |
| F8 | **High** | watcher 线程遇到单个 DB 错误/坏事件即 `?` 退出，Linux 侧无监督重启，realtime 静默死亡 | `fanotify.rs:214,120` |
| F9 | **High** | `handle_paths` 只增不删 + rename/delete 后陈旧——内存泄漏 + inode 复用导致路径解析错 | `fanotify.rs` |

下面按你提的 7 个问题展开。

---

## 1. 文件不丢失 / 不写错 / 事件不丢（重启、crash、溢出）

### 1.1 本地写入路径——基本健壮，但 fsync 默认关闭是真实掉数据风险

写文件流程是对的（`copy_file` / `finish_received_file`，`mod.rs:4476`、`mod.rs:4910`）：
写临时文件 → 设 mode/mtime →（可选）hash 校验 → `fsync_file` → 原子 `rename` → `fsync_parent`。崩溃只会留下 `.auto_sync_tmp` 里的半成品，不会污染最终路径。删除走 `.auto_sync_trash`（`mod.rs:4586`），符合设计。

**但 `FSYNC_ENABLED` 默认 `false`**（`mod.rs:4804`，`configure_fsync` 从 config 设置，config 默认关）。于是默认配置下：

- `fsync_file` / `fsync_parent` 直接早返回（`mod.rs:4814,4821`）；
- `copy_file_with_progress` 只做 `writer.flush()`（用户态 flush，不落盘，`mod.rs:4535`）。

**后果（直接回答“磁盘有用异步刷新吗？会丢数据吗？”）**：是的，依赖 OS 异步回写。在 **ext4/NTFS 目标盘**上，rename 的元数据可能先于数据块落盘（经典的 rename-without-fsync 数据丢失），掉电后最终文件可能是 0 长度或旧/垃圾内容，**而 cycle 已经标绿**。ZFS 目标盘因为 TXG 内 data+rename 同序，风险低很多。

> 注意：状态库本身是安全的（`state.rs:81-84` WAL + `synchronous=FULL` + `busy_timeout=10s`），掉电不会丢状态；丢的是**数据文件内容**。
> 设计文档第 9 节明确把“临时文件 + fsync + 原子 rename”列为防半文件的关键机制，**实现默认关掉了其中的 fsync**——属于实现与设计的偏离。建议：要么默认开 fsync，要么至少对“close→rename”的文件做 `fdatasync`，并把“弱持久化模式”在 UI/日志显式标注（与设计第 9 节弱一致模式的精神一致）。
> 次要：即使开了 fsync，`fsync_file(..).ok()` / `fsync_parent(..).ok()` 把 `ENOSPC/EIO` 吞掉了（`mod.rs:4930,4935`），失败的持久化屏障不可见。

### 1.2 事件持久化——顺序对，但兜底没接线（最严重）

事件是“先写 SQLite 再推进游标”，方向正确（at-least-once）：USN `record_event` 先于 `set_windows_usn_cursor`（`windows_usn.rs:222→255`）。fanotify 同理在 `parse_events` 内先 `record_event`。崩溃会重放、不会丢——**这一层没问题**。

问题在“丢事件之后靠什么补”。设计文档第 6/7/13 节的核心承诺是：**fanotify/USN 只是近实时提示，最终一致由“周期性 snapshot + reconcile”兜底**。但实现里这个兜底没有接上：

**F1（Critical）—— `reconcile_interval` 从未使用。**
`reconcile_interval_secs` 在 `config.rs:115` 定义、`config.rs:279` 默认 900、`config.rs:682` 校验非零，但全代码库再无引用（已全量 grep 确认，只剩一个测试名提到它）。realtime 目标是否“到期”完全由事件驱动：`advance_due_destination_targets` 里 realtime 分支的 due 条件是 `open_has_events`（`state.rs:366-367`）。**没有任何按 `reconcile_interval` 触发的周期性全量 reconcile cycle。** 这意味着：

- 如果 realtime source 在某段时间**没有产生可解析事件**（停机窗口、溢出、删除/移动），就**永远不会**生成一个去做全量比对的 cycle；
- realtime 同步走的是 event-path 增量（`sync_endpoint_event_paths`，只同步事件命中的相对路径），**不做整树比对**；
- 而且 event-path 增量成功后**直接把 `last_verified_cycle_id` 推进并标绿**（`mod.rs:1388-1394`）。这本身就偏离设计第 9 节“realtime 快速同步完成不能推进 `last_verified_cycle_id`，只有 reconcile 校验通过才推进绿点”。

合起来看：**溢出窗口**会卡 yellow（F2，需人工 Full）；**停机窗口的删除/移动**（fanotify 全程下线、无事件，startup_mtime_scan 又抓不到删除）和 **Windows 启动 gap**（F3）则会**长期显示绿点却与 source 不一致**；两类都**没有自动机制**纠正，只能靠人工 `Sync... → Full`。这是对问题 1（“尽量保证文件不丢失/不写错/事件不丢”）影响最大的一组缺陷。

**F2（High）—— fanotify 溢出标黄后无自愈。**
`fanotify.rs:386` 溢出时 `record_event(..., "queue_overflow", None, true)`，第 5 参 `true` 会置 `mark_open_cycle_needs_rescan`。于是 `realtime_incremental_plan` 里 `actionable.iter().any(rescan_required)` 命中 → 返回 `Unusable("realtime_rescan_required")` → cycle 标 failed、dst 转 **yellow**（`mod.rs:1726-1730`）。**这一步是对的，溢出不会静默转绿**（fd-path 无法解析路径的分支 `fanotify.rs:410` 同样置 true）。
真正的问题是**之后没有自愈**：因为 F1 没有任何自动 reconcile 周期，cycle 会一直卡在 yellow，必须用户手动 `Sync... → Full` 才能恢复。设计第 13 节要求“溢出后下一次 reconcile interval 用 diff 补齐”——这个补齐动作不存在。

**F3（Critical）—— Windows USN 启动重连的 gap 不置 rescan。**
运行中检测到 gap（`next_usn < LowestValidUsn`）会正确地 `record_event("usn_gap", None, true)`（`windows_usn.rs:205`，rescan=true）。**但同样的 gap 在启动时检测到，却 `rescan_required=false`**：
- 首启无 cursor：`usn_initial_reconcile`，false（`windows_usn.rs:269`）；
- 停机后 cursor 落在 `LowestValidUsn` 之前（确认丢了事件）或 journal 被重建：`usn_cursor_reconcile`，false（`windows_usn.rs:280`）。

`false` + 无 rel_path 的事件在 `realtime_incremental_plan` 里不算 actionable，cycle 走空计划转绿。**最该全量 reconcile 的“确认丢事件”场景，反而不触发 rescan。** 与运行中路径自相矛盾，应统一为 `true`。

**startup_mtime_scan 不是完整兜底。**
`watcher/mod.rs:45` 的启动补漏扫描只能发现 **mtime 变新的文件**，且 `latest_event_observed_at` 为 `None`（全新库/首启）时直接跳过（`mod.rs:46-48`）。它**无法发现停机期间的删除与移动**——这正是 mirror 模式最危险的不一致（dst 残留已删文件却显示绿点）。设计第 9 节也说它“只补 event-path 增量，不触发 Full”，所以它本就不该承担兜底，真正兜底是 F1 那个缺失的 reconcile。

### 1.3 重启恢复——隐式、靠状态过滤而非显式复位

没有显式的“启动时把 `syncing/planning` cycle、`running` task 复位”的代码。恢复是“顺带”发生的：`closed_cycles_for_source` 选 `status IN ('closed','planning','syncing','failed')`（`state.rs:503`），于是崩在 syncing 的 cycle 下一轮被重新驱动。常见情况能自愈（offset 幂等），但：

- offset 推进 + cycle 状态 + snapshot 写入 + startup 事件清理是**分散的多条 autocommit**，不在一个事务里（`mod.rs:1557` 推进 offset 与 `mod.rs:1613` 标 verified 之间还有整个 dst 循环）。崩溃可留下“offset 已推进但 cycle 仍 syncing”的半应用态——靠幂等重验自愈，但“offset 推进 ⟺ cycle verified”不是崩溃原子的。**（High）**
- 唯一用显式事务的是 `replace_snapshot`（`state.rs:956`）。
- `mark_cycle_status("verified")` 还顺带 `DELETE` 掉 startup 事件（`state.rs:640-657`），同样非事务。

### 1.4 “绿点 = 已校验”的真实性

- **本地全量 reconcile / 远程全量**：`sync_endpoint → sync_destination` 结尾会 `verify_destination` 整树比对（`mod.rs:4610`）后才推进 offset——这条路径的绿点是可信的。
- **realtime event-path 增量**：只校验命中的子集，却推进 offset 标绿（见 F1）。
- **fast missing-dirs 快路径**（`mod.rs:1440`）：`Ok` 后立即标绿（`mod.rs:1453`），**没有内容/hash 校验**，仅补目录结构。绿点偏乐观。**（Medium）**
- `targeted_count == 0` 也无条件标 verified（`mod.rs:1612`）。

---

## 2. 跨机可靠性与传输速度

### 2.1 可靠性

- **接收端持久化**：和本地一样用 temp→rename，结构正确；但同样受 F4（fsync 默认关）影响——崩溃可留下半成品却报成功。
- **F5（High）完整性校验在最常见路径上缺失**：`finish_received_file` 只在 `entry.hash.is_some()` 时校验 hash（`mod.rs:917`）。而小/中文件快速路径 `transfer_put_file` 硬编码 `hash:None`（`mod.rs:739`），分块路径 `transfer_finish_file` 只校验 size（`mod.rs:707`）。默认（非 checksum 模式）传输的文件，接收端**只校验字节数**，保长度的位翻转/中间件错误可静默通过。只有 delta 路径与 checksum 模式带全量 hash 端到端校验。
- **重试**：`push_entries_parallel` 第一个 worker 出错即 `first_error` 置位、其余 worker 停止取活（`mod.rs:2971-3015`），**单个瞬时 HTTP 失败让整个 dst cycle 失败**，无 per-file 退避重试，靠下一轮调度。HTTP 层仅对“复用的 keep-alive 连接”失败重试一次（`machines.rs:752`），无无限重试。**（Medium）**
- **超时有界**（好）：每个远程调用都带 `Duration`，默认 120s（`config.rs`），hung peer 不会永久阻塞 worker。
- **断点续传**：只有 16MB 分块路径支持（先问 `file-offset` 再 seek，`mod.rs:972-985`）。`put-file`（≤16MB）和 delta（256KB–1GB）**不可续传**，中断即重头，可续传窗口其实很窄。**（Medium）**

### 2.2 速度

- **F7（High）整文件入内存**：`send_put_file_tcp` `fs::read(src)`（`mod.rs:1027`）、`send_file_delta` `fs::read`（`mod.rs:1080`，最大 1GB）、接收端 `transfer_block_sums` `fs::read` 整个 basis（`mod.rs:765`）、`transfer_put_file` 全缓冲 `Bytes`（`web_api.rs:296`）。`max_parallel_transfers` 个 worker 各持最大 1GB，双端峰值 RAM ≈ worker 数 × 文件大小，**几个大文件并行就能 OOM**。
- **delta 算法本身是对的**（`delta.rs`：rolling checksum + blake3 强校验，按文件大小调块，delta 超过 ~90% 退回全量），确实减少线上字节。
- **push 阶段已并行**（`push_entries_parallel`）+ 连接池复用（`TcpConnectionPool`，`machines.rs:933`），MEMORY.md 记的“每文件顺序往返”瓶颈在 push 阶段已缓解；单个大文件内分块仍是顺序的。
- **改进方向**：用流式 body（bounded buffer）替代 `fs::read` 整文件；put-file/delta 也接入续传；per-file 重试退避。

### 2.3 安全（顺带，影响可靠性）

**F6（High）peer API 无鉴权 + body 无上限。** `web_api.rs:93` `.layer(DefaultBodyLimit::disable())` 对所有路由去掉 2MB 上限；`/api/transfer/*`（含 `remove-path`、`remove-paths`、`put-file`）无任何 token/auth；默认监听 `0.0.0.0:18765`。任何能到端口的人都能：① 往任意 dst 根下写/删文件（虽有 `reject_dangerous_destination` + `safe_join_rel` 挡穿越/系统目录），② POST 超大 body 让接收端整体缓冲 → OOM（handler 内的 size 校验发生在缓冲之后，太晚）。设计第 3/14 节只提到 Web UI 需保护，但传输 API 直接执行文件写删，危险得多。建议：共享密钥/HMAC + 恢复 body 上限 + 仅 LAN 监听。

> 附带：`ApiError` 一律 `500` 且把 `anyhow` 原文回给对端（`web_api.rs:371`），泄漏内部路径，且客户端无法用状态码区分可重试/致命。响应体读取无上限（`machines.rs:856`）。

---

## 3. snapshot 机制与其它措施是否搭配合理

**搭配的设计意图是合理的**，但实现层有两处关键偏离 + 一处与设计不同的取舍：

1. **reconcile 兜底缺失（F1）**——已在 §1.2 详述。这是 snapshot 机制“搭配”里最大的漏洞：snapshot 只被用作“每个 cycle 的稳定只读读视图”（`ZfsSnapshot::create` → `.zfs/snapshot/...` 作为 source root，`mod.rs:3176`），**没有**实现设计第 7/9 节的 `zfs diff(old,new)` 增量计划；reconcile 周期也不存在。

2. **实现没用 `zfs diff`，改为每 cycle 全量 manifest 比对。** 这与 MEMORY.md 里“full-sync rework / 900K 顺序往返”一致：百万文件下 scheduled/full reconcile 仍是 source 整树扫描 + dst 整树扫描比对，慢。好处是不依赖“上一个 base snapshot”，因此——

3. **snapshot 清理偏离设计但当前基本无害。** `cleanup_zfs_snapshots`（`mod.rs:3347`）只按 `keep_extra_cycles+1` 保留最新 N 个，**完全不查 `destination_snapshot_cursor` / running task / refcount**（设计第 7 节要求的引用计数清理）。因为实现不做 snapshot-to-snapshot diff，落后 dst 也不需要旧 base，所以**目前**删掉旧 snapshot不致命；但一旦将来接入 `zfs diff` 增量，这个清理会删掉落后/离线 dst 仍需要的 base，必须先补 refcount。设计里描述的 `destination_snapshot_cursor`、`zfs_snapshot` refcount 等表/逻辑实际未落地。

4. **ZFS 只读视图取数正确**：copy/verify 都从 snapshot 路径读，保证“增长中的文件本 cycle 读到的是快照时刻版本”，符合设计第 9 节。`snapdir` 不可见时 `bail` 提示（`mod.rs:3192`）也合理。

> 小结：snapshot 作为“稳定读视图”这一层是好的；但“snapshot + 周期 reconcile + diff + refcount 清理”这套**最终一致兜底**只兑现了第一层。对 ZFS 百万文件，要么补 `zfs diff` 增量 + reconcile 周期，要么至少把 `reconcile_interval` 接成“周期性全量 reconcile cycle”。

---

## 4. 当前代码不合理 / 可疑之处

按严重度：

- **High**
  - **F8 watcher 无监督重启（Linux）**：`run_fanotify_loop` 任一 `?` 失败（`parse_events` 里版本不符/长度非法/`record_event` DB 错都会 `bail!`/`?`，`fanotify.rs:374-399,214`）→ 线程 `error!` 后退出（`fanotify.rs:120-123`），**不重启**。一个瞬时 `database is locked` 就能永久停掉该 source 的 realtime（Windows 侧有 `run_resilient_source_watcher` 包了一层，Linux 侧没有）。应：单事件错误 skip 而非 kill 整循环 + 外层重启退避。
  - **F9 `handle_paths` 只增不删**：`SourceRoot.handle_paths`（`fanotify.rs:84`）只 insert，从不在 `FAN_DELETE/MOVED_FROM/DELETE_SELF` 时删除。① 长跑 churn 树内存泄漏；② inode 被复用后，陈旧 `handle→旧路径` 把新对象的事件解析到错误路径；③ 父目录 rename 后，子项缓存的路径字符串仍是旧路径，子事件解析错。
  - **多连接 + 长事务的锁竞争**：`State` 持单 `Connection` 非池（`state.rs:67`），scheduler / watcher / 每个 web 请求各开各的连到同一文件（`auto_sync.rs:318,413`，`backend.rs:79-274`）。WAL 只允许 1 writer；`replace_snapshot` 写 ~900K 行的长事务可能超 `busy_timeout=10s`，让 watcher 的 `record_event` 撞 `database is locked` → 配合 F8 杀死 watcher。建议应用层串行化写或用单写连接/队列。

- **Medium**
  - **realtime 推进 offset 偏离设计**（§1.4 / F1）。
  - **fast missing-dirs 路径标绿不校验内容**（`mod.rs:1453`）。
  - **USN `ReadDirectoryChangesW` fallback 同步阻塞**：`lpOverlapped=None`（`windows_usn.rs:408`），无变更时线程卡在调用里，shutdown 只在两次调用之间检查 → `join()` 可能挂住整个进程关闭。应改 overlapped + `CancelIoEx`/超时。
  - **USN 同批次新建目录的子项丢失**：目录索引在批末才 `rebuild`（`windows_usn.rs:246-249`），同一批里“建目录 X”后“在 X 下建文件”因 X 还不在 `directories` 而被 `return Ok(false)` 静默丢弃（`windows_usn.rs:562-564`）；父 FRN 不在索引内的删除/移动也静默丢、无 rescan 兜底。
  - **`sync_gate` 忽略 mutex 中毒**：`lock().unwrap_or_else(|e| e.into_inner())`（`mod.rs:104`），一次 sync panic 后下一次照常在可能不一致的内存态上继续。
  - **schema 迁移无版本号**：`ensure_column` 探测 `PRAGMA table_info` 后 forward-only `ALTER ADD COLUMN`（`state.rs:202-215`），无 `user_version`、无回退、无事务包裹、需手工记得加调用；需要数据回填/改类型的变更无机制。

- **Low**
  - `foreign_keys=ON`（`state.rs:84`）但 schema 没声明任何 FK，配置空转。
  - fanotify 用 200ms 轮询 + `FAN_NONBLOCK`（`fanotify.rs:204,242`）而非 `poll()` 阻塞，平添最多 200ms 延迟、空闲 5 次/秒唤醒。
  - `mask_to_kind`（`fanotify.rs:721`）丢失 `FAN_ONDIR`，下游无法从 kind 区分目录 create 与文件 create。
  - fd-path fallback 模式下无法解析路径时，对**所有 source**都记一条无路径事件（`fanotify.rs:408-413`），可能给无关 source 触发不必要处理。
  - `cycle_from_row` 时间解析失败把单条坏行变成整个 source 调度迭代的 `Err`（`state.rs:1160`）。
  - `reject_dangerous_destination` 用 `canonicalize().unwrap_or(raw)`（`mod.rs:3509`），对不存在路径降级为字面比较；关键路径清单是 Linux-only，Windows 系统目录未覆盖。

---

## 5. fanotify 使用是否有问题 / 合理

**合理的部分**：每 source 独立 group/fd/线程（`fanotify.rs:114-124`），符合设计“一个 source 的 overflow 不污染其他”；优先 `FAN_REPORT_FID|DIR_FID|NAME|TARGET_FID`（`fanotify.rs:221`），失败回退 fd-path、再回退逐目录 inode mark；事件 mask 覆盖 modify/close_write/create/delete/move/self/ondir（`fanotify.rs:168-176`），方向对。

**问题**（除 F2 溢出、F8 无重启、F9 句柄表外）：

- **mark 注册竞态**：先 `build_handle_path_map` 走树再 `fanotify_mark`（`fanotify.rs:155→177`），扫描与 mark 之间的变更不被观察；常规做法是“先 mark 再扫描再去重”。`FAN_MARK_FILESYSTEM` 模式下因为整个 fs 已 mark，影响小；但回退到逐目录 mark 模式时是真实丢窗口。
- **新目录递归 mark 竞态**（仅回退模式）：`track_new_path_and_mark_directory`（`fanotify.rs:560`）在 create 事件**已处理后**才 mark 新目录并递归，新目录里在 mark 完成前创建的子项事件会丢；且 `mark_directory_tree` walk 出来的新条目**没有补进 `handle_paths`**，后续 FID 事件解析不到。
- **读循环脆弱**：任一坏事件 `bail!` 杀整线程（同 F8）。
- 64KB 读缓冲对单事件足够（fanotify 不返回半个事件，`FAN_EVENT_NEXT` 迭代正确）；FID 模式 `fd==FAN_NOFD` 无需 close——这两点没问题。

> 结论：fanotify 的**分组/标志/回退结构是对的**，但“溢出不 reconcile + 句柄表只增不减/陈旧 + 单错杀线程 + 无周期 reconcile”叠加，使它在丢事件后**没有自愈路径**——而设计的全部前提就是“fanotify 会丢，靠 reconcile 兜底”。

---

## 6. symlink 处理是否合理（含跨机、跨系统）

**本地/同构系统：基本正确。** 全程用 `symlink_metadata`(lstat) 分类与扫描，递归只在 `metadata.is_dir()` 为真时下钻——symlink 指向目录不会被跟随，**符号链接环不会无限递归**（`mod.rs:4300,4357`，`3777`，`3878`）。dangling symlink 在 Unix 上也能正常快照/复制。symlink 指纹用 `"symlink:<target>"` 字符串 hash 比较、**不比 size/mtime**（`entries_match` `mod.rs:4652`），避免“跟随链接的 stat 差异导致反复重传”——这个取舍是对的。

**跨系统（Linux ⇄ Windows）：有真实问题。**

- **High：Linux 目录 symlink 到 Windows 总被建成 file symlink。** 接收端 `receive_symlink_target` 调 `create_symlink(&final_path, target, &tmp)`（`mod.rs:899`），把**尚不存在的** `final_path` 当 `src`；Windows 版 `create_symlink` 用 `fs::metadata(src).is_dir()` 判定 dir/file（`mod.rs:4794`），不存在 → 永远 `false` → 永远 `symlink_file`。目录符号链接在 Windows 上会建错、无法当目录遍历。根因：`TransferReceiveSymlinkRequest` 线格式只带一个不透明 target 字符串，**没有 is_dir 标志**。
- **High：跨 OS target 字符串差异导致每个 cycle 反复重传 + 校验失败。** hash 是 `symlink:<target>` 纯字符串；Windows `read_link` 回读可能是 `\` 分隔或 `\\?\` 前缀，回读 hash ≠ 源 `symlink:/path` hash → 每轮判定“changed”重传，且 `verify_destination` `bail!("destination mismatch")`。
- **Medium：target 用 `to_string_lossy()`**（`send_symlink_tcp` `mod.rs:1153`、`hash_symlink` `mod.rs:4702`），非 UTF-8 target（Linux 合法）被 U+FFFD 破坏。绝对/相对 target 不做改写（对相对 target 正确），但 Linux 绝对 target `/etc/hosts` 原样搬到 Windows 即 dangling；target 本身不过 `normalize_rel_path`，恶意/相对 target 可指向镜像树外。
- **Medium：Windows 创建 symlink 需 `SeCreateSymbolicLinkPrivilege`（管理员或开发者模式）**，失败时 `copy_symlink`/`receive_symlink_target` 直接 `bail!` 整条目反复失败，无“退化为普通文件/跳过并告警”的兜底。
- **Low：dangling symlink 在 Windows `copy_symlink` 里 `fs::metadata(src)` 跟随失败 → `unwrap_or(false)` → file symlink**；目录浏览 `browse_paths_inner`（`backend.rs:806`）用 `entry.metadata()`（跟随）分类，与全局 lstat 约定不一致，dangling 被静默丢。

> 建议：线格式增加 `is_dir` + 规范化 target 编码（统一 `/`、原始字节而非 lossy）；Windows 缺权限时定义明确降级策略。

---

## 7. 优先级修复建议

**P0（正确性兜底，先做）**
1. 接线 `reconcile_interval`：realtime source 周期性关闭一个“全量 reconcile cycle”，做整树 source↔dst 比对（含删除），校验通过才推进绿点（F1）。
2. 溢出/gap 一律置 `rescan_required=true` 并让对应 cycle 进入 Unusable/yellow 或强制 reconcile：`fanotify.rs:383`、`windows_usn.rs:269,280` 与运行中路径对齐（F2/F3）。
3. realtime event-path 增量成功不要直接推进 `last_verified`（或单独标“near-realtime ok”而非 verified），把“绿点”留给 reconcile（与设计第 9 节一致）。

**P1（持久化/可靠性）**
4. fsync 默认开启（或对 close→rename 做 `fdatasync`），弱持久化模式显式标注；不要 `.ok()` 吞 fsync 错误（F4）。
5. 跨机所有路径带端到端 hash 校验，至少接收端对 put-file/chunk 校验内容 hash（F5）。
6. watcher 单事件错误 skip 而非杀线程 + 外层监督重启退避；Linux 复用类似 `run_resilient` 的包装（F8）。
7. 应用层串行化 SQLite 写（单写连接/队列），避免长事务把 watcher 写挤爆（§4 多连接）。
8. 关键状态转移（offset 推进 + cycle 状态 + snapshot + 事件清理）放进单事务。

**P2（传输性能/安全）**
9. 流式传输替代整文件 `fs::read`，限制并发内存（F7）。
10. peer API 加共享密钥鉴权 + 恢复 body 上限 + 仅 LAN 监听（F6）。
11. per-file 重试退避；put-file/delta 支持续传。

**P3（跨系统 symlink / 清理）**
12. symlink 线格式加 `is_dir` + 规范化 target 编码；Windows 缺权限降级策略（§6）。
13. `handle_paths` 在 delete/rename 时清理，防泄漏/陈旧（F9）。
14. 若将来接 `zfs diff` 增量，先补 snapshot refcount 清理（设计第 7 节）。

---

## 附：实现 vs 设计文档的主要偏离清单

| 设计文档承诺 | 实现现状 |
|---|---|
| 第 6/7/13 节：realtime + 周期 snapshot reconcile 兜底，溢出标 `needs_reconcile` | `reconcile_interval` 未使用；溢出/gap 不 reconcile（F1/F2/F3） |
| 第 7/9 节：`zfs diff(old,new)` 生成增量计划 | 未实现，改为每 cycle 全量 manifest 比对 |
| 第 7 节：`destination_snapshot_cursor` refcount 清理 snapshot | 清理只按 `keep_extra_cycles+1` 保留，不查引用 |
| 第 9 节：临时文件 + **fsync** + 原子 rename | fsync 默认关闭 |
| 第 9 节：realtime 快速同步**不推进** `last_verified`，绿点只给 reconcile | event-path 增量成功即推进并标绿 |
| 第 5 节：`PRAGMA foreign_keys=ON` 且有外键约束 | 开了 pragma 但 schema 无 FK 声明 |
