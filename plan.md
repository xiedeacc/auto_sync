# auto_sync 修复与优化计划

目标：① 修复 review.md 全部问题；② 把 sync 速度提到“系统 copy 级别”；③ 修复 Destination Log speed 显示经常为 0；④ 校正 sync 逻辑。

分阶段执行，每阶段独立可编译、可提交、可部署。

---

## 阶段 1：传输速度 + 速度显示（对应 #2 #3，本轮先做）

**根因**
- 本地拷贝单线程顺序执行（`sync_destination` / fast 路径的 `for entry … copy_entry`），且 `copy_file_with_progress` 用用户态 16MB read/write 循环，未用 `copy_file_range`/reflink/`CopyFileEx` → 远低于系统 `cp`。
- 进度用 per-file `start_transfer`（单一全局槽，文件间清空；<250ms 的小文件从不采样）→ speed 经常 0。

**改动**
1. 新增跨平台快速拷贝原语 `copy_file_data(src, tmp, rel, size)`：
   - Linux：先试 reflink（`FICLONE` ioctl，ZFS 2.2+/btrfs 瞬时）；否则 `copy_file_range`（内核态拷贝，ZFS 高效）；再不行回退流式。
   - Windows：`std::fs::copy`（`CopyFileExW`，系统级速度）。
   - 进度通过 `record_transfer` 上报。
2. 本地文件拷贝循环并行化：新增 `copy_entries_parallel`（worker pool，复用 `resolve_parallelism`/`thread::scope`，收集 `changing_paths`，硬错误走 first-error），用于 `sync_destination` 与 fast 路径的 file/symlink 阶段。
3. 聚合进度：在 `sync_destination` / `sync_destination_fast_missing_dirs` / `sync_file_to_path` 开始处 `begin_transfer`，拷贝过程 `record_transfer`，去掉 per-file `start_transfer`。
4. `progress.rs` 速度稳健性：聚合后速度跨文件连续；保留 `transferred/elapsed` 兜底，避免活动期显示 0。

**验证**：本地大目录/大文件 sync 计时对比；UI Destination Log speed 持续非 0。

---

## 阶段 2：realtime 正确性兜底（对应 #1 #4，review F1/F3 + realtime 标绿）

1. **接线 `reconcile_interval`**：realtime source 到期 `reconcile_interval_secs` 时关闭一个“全量 reconcile cycle”（整树 source↔dst 比对，含删除/移动），校验通过才推进绿点。
2. **USN 启动 gap 置 `rescan_required=true`**：`windows_usn.rs:269,280` 与运行中 `:205` 对齐，确认丢事件场景触发 reconcile 而非空计划转绿。
3. **realtime event-path 增量**完成后不直接推进 `last_verified`（或区分 “near-realtime ok” 与 “verified”），绿点只给 reconcile/全量校验。
4. fanotify 溢出/USN gap 标黄后由阶段 2.1 的 reconcile 自动清除，不再永久卡黄。

---

## 阶段 3：持久化与可靠性（review P1）

1. fsync 默认开启或对 close→rename 做 `fdatasync`；弱持久化模式显式标注；不再 `.ok()` 吞 fsync 错误。
2. 跨机所有路径端到端 hash 校验（put-file/chunk 接收端校验内容 hash）。
3. watcher 单事件错误 skip 而非杀线程 + 外层监督重启退避（Linux fanotify 对齐 Windows `run_resilient`）。
4. SQLite 写串行化（单写连接/队列）；关键状态转移（offset+cycle 状态+snapshot+事件清理）入单事务。

---

## 阶段 4：传输性能/安全 + 跨系统 symlink（review P2/P3）

1. 流式传输替代整文件 `fs::read`，限制并发内存。
2. peer 传输 API 加共享密钥鉴权 + 恢复 body 上限 + 仅 LAN 监听。
3. per-file 重试退避；put-file/delta 支持续传。
4. symlink 线格式加 `is_dir` + 规范化 target 编码；Windows 缺权限降级策略。
5. `handle_paths` 在 delete/rename 时清理，防泄漏/陈旧。

---

## #4 sync 逻辑结论（详见 review.md）

是的，有问题，核心是 realtime 的“最终一致兜底”没接线：`reconcile_interval` 从未使用、USN 启动 gap 不触发 rescan、realtime 增量即标绿。后果是 realtime 目标可能长期显绿却漏掉停机/溢出窗口的删除与移动。阶段 2 修复。全量 reconcile（手动 Full / scheduled）路径本身是带整树校验的，逻辑正确，只是慢（阶段 1 提速）。
