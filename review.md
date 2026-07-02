# auto_sync 深度 Review(第五轮,2026-07-02)

---

## 0''. 修复状态(2026-07-03,第五轮修复后)

本轮全部结论已按用户要求集中修复(commits `39deeed` → 修复完成),各编号现状:

| 编号 | 现状 |
|---|---|
| R1/L1、L2 | **已修复**:三处目录挡路判断改 lstat(dst symlink 挡路进回收站,不再写穿);replace_path 类型翻转走回收站。 |
| R2/S1 | **已修复**(即用户报告的 79640 假差异 bug):parse_zfs_diff / 按路径快照 / mirror 删除集三层过滤 `.auto_sync_*` 内部目录,带回归测试;实测同一比较 79640 → 15 个真实差异。 |
| R3/C1 | **已修复**:事件/zfs diff 证据路径改精确 mtime 比较(容忍 100ns FILETIME 截断),verify 同步收紧;modify-window 仅保留给全树 quick-check。 |
| R10/C2/盲区1/盲区2 | **已修复**:entries_match 双侧带 hash 时 hash 优先;dst 侧独有 diff 路径强制补哈希(Compare 与 zfs Full);权限漂移成为 metadata 差异类(Compare 报告 + chmod 原地修复,本地与远端 set-modes 端点);fanotify 订阅 FAN_ATTRIB。 |
| R4/W1 | **已修复**:fanotify 分级初始化(去掉 TARGET_FID 硬前提,5.9+/5.1+ 两级 FID 后才 fd-path);fd-path 降级 error 级日志 + 每次启动记 rescan 事件强制对账。 |
| R5/P1、W3 | **已修复**:config 变更与 supervisor 重启窗口都记 watcher_restart_gap(rescan);config_signature 只哈希 watcher 相关字段,无关保存不再重启 watcher。 |
| R6/E1 | **已修复**:引擎 pass per-source 错误隔离,坏源不再饿死后续 source。 |
| R7/S2、repair-3 | **已修复**:基线 Refresh 用 `zfs diff 旧基线→新基线` 对照本轮触碰集(walk Full 传全触碰集,zfs Full 传并集),发现外部写入降级 Retain。传输路径的本地 dst Refresh 暂无触碰集(不做核对)。 |
| R8/S3 | **已修复**:混合 cycle 本地 ZFS dst 增量 Retain、全量 Refresh,仅真远程 Clear。 |
| R9/S4 | **已修复**:清理匹配校验 `_<12位cycle>` 后缀,前缀关系 id 不再互删。 |
| R11/W4、W5 | **已修复**:watcher 事件按 read() 批次单事务写入(每批一次 fsync)+ 同路径去重;持久化失败置 sticky,下次成功先补写 rescan 标记。 |
| R12/E5 | **已修复**:trash 按 `sync.trash_keep_days`(默认 30 天)回收;tmp 非当前 cycle 7 天清扫;move_to_trash 失败不再静默硬删(EXDEV copy+delete,其余报错)。 |
| R13/P2-P12 | **已修复**:配置 RMW 进程锁 + 唯一 tmp 名(P2);GET 轮询零写库(P3);Sync All 尽力而为(P4);UDP 发现改 TCP 回读确认后才落盘(P5);save_config 推送失败降级警告 + 后台重推(P6);peer 连接支持主机名(P7);Tauri 命令全部 spawn_blocking(P8);清 rescan 不再顺带清 manual 标志(P9);ensure_open_cycle 原子插入(P10);远端状态聚合并行 + 按机器去重(P11);首选网段可配置(P12)。 |
| F6 | **已修复(机制)**:`app.peer_token` 鉴权 /api/transfer/* 与 delegated 推送(各机器配同一值;为空=旧行为);body 上限 1 GiB。需在两端 conf 设置 token 以启用。 |
| E2 | **已修复**:`\`→`/` 重写仅限 Windows;非 UTF-8 文件名两侧跳过并告警(不再 lossy 错传)。 |
| E3 | **已修复**:同名快照创建时间早于 cycle 开始即销毁重建;基线快照强制新鲜。 |
| E4 | **已修复**(见 R12)。E6 批发拷贝失败容忍对齐 20 个契约;E7 tmp lstat 预清理;E8 介质检测只计数据 vdev;E9 zfs list 按 tab 解析。 |
| S5 | **已修复**:删除的 (src,dst) 对的 offset 行修剪 + dstbase 快照回收(不再永久钉住 source 快照)。 |
| S6/S7/S8 | **已修复**:文件 source 不打基线;Compare 对同 dst 活跃 sync 快速失败;destroy 退出码正确处理(单个 hold 不再阻塞清扫)。 |
| L3-L9 | **已修复**:dst 根 symlink 拒绝(src 根警告);跨 OS target 分隔符归一 + hash 同步归一;Windows 目录链删除 RemoveDirectory 回退;Unix 间 target 原始字节传输;发送端 symlink 变化转黄;浏览器显示 symlink。L9 由失败容忍缓解(无专门降级)。 |
| W6-W9 | W8 eager 溢出重建(10 分钟节流)、W9 mark 失败记 rescan **已修复**;W6(嵌套挂载点检测)**未做**——当前部署无嵌套 dataset,列为后续。 |
| perf 一~四 | 部分完成:prune 空转门控(只在有进展的 pass 后跑)、sorted_read_dir 零分配排序已做;串行双树扫描、3×lstat、M 目录递归展开、map_entries 深拷贝、快照流式、delta 哈希、进度节流、统一介质并行度为**遗留优化项**(见 §8)。 |
| C3 | **已修复(文档)**:system_design.md §9 如实记录事件路径读活树 + 守卫链语义、两档判等、回收站保留、基线交叉核对、peer_token。 |

> 范围:全量源码 + `system_design.md`,基于 commit `d2f3688`。只评审,不改代码。
> 方法:按 8 个维度(snapshot 机制 / fanotify / symlink / 引擎 / 状态与 API 层 / 写入中一致性 / 修复能力矩阵 / 性能)各由一个独立评审深读代码,产出的每条结论再由独立的"怀疑者"逐条对抗验证(读代码给行级证据),被证伪的结论已剔除或降级为其残余问题。共 68 条结论进入验证,3 条被证伪。
> 行号基于评审时工作树。严重度:**Critical / High / Medium / Low**。
> 上一轮(四轮 review 及其状态表)的全文见 git 历史(`git log -- review.md`)。

---

## 0. 上一轮 F1–F9 现状

| # | 原结论 | 本轮核实现状 |
|---|--------|------|
| F1/F2/F3 | reconcile 兜底未接线 / 溢出卡黄 / USN 启动 gap 不置 rescan | **已按用户决策收敛**:疑似丢事件(overflow / USN gap / 启动 gap)自动触发红色 `event_loss_reconcile` 完整对账;周期性对账按用户要求不做;重启窗口由 restart notice 提示。本轮发现该不变量仍有若干漏网窗口,见 §3(W3/W4)与 §5(P1)。 |
| F4 | fsync 默认关闭 | **已修复**:`NativeSyncConfig::default` 现在 `fsync: true`(config.rs:258,注释 config.rs:82-89),每个 sync pass 重新应用配置,接收端启动时也应用;`finish_received_file` 的 fsync 失败不再被吞。旧记录已过时。 |
| F5 | put-file/chunk 只校验 size | **已修复**(所有跨机路径 blake3 端到端)。 |
| F6 | peer API 无鉴权 + body 无上限 | **未变**(用户接受的内网取舍)。但本轮发现一个**新派生面**:未认证 UDP 发现应答可污染持久配置(P5,见 §5)。 |
| F7 | 整文件入内存 × 并发 | **部分修复**(1 GiB 内存预算);跨机整树快照仍是单块 JSON 双端整体缓冲(见 §8)。 |
| F8 | watcher 单错误死亡(Linux) | **已修复**:fanotify per-source 监督循环,出错 5s 退避重启(fanotify.rs:146-170);单事件持久化失败 warn+skip 不再杀线程。残余:重启窗口不置 rescan(W3)。 |
| F9 | handle_paths 只增不删 | **已修复**:删除/移出事件按路径前缀清理缓存(fanotify.rs:512-536、859-863)。残余:清理是 O(整个 map) retain 且缓存无上限(W7)。 |

---

## 1. 结论速览(本轮新发现,按严重度)

整体判断:经过前四轮修复,**核心数据通路(临时文件+原子 rename+fsync、端到端 blake3、source-changing 容忍、事件丢失自动对账、dstbase 三态不变量)是健康的**;本轮没有发现"会把错误数据当正确结果写进目的地并且无法察觉"的 Critical。新发现集中在:**快路径(zfs diff)与回退路径(walk)的行为不一致**、**判等容差留下的静默漂移窗口**、以及**"疑似丢事件必须可见"不变量的几个漏网边角**。

| # | 严重度 | 一句话 | 详见 |
|---|--------|--------|------|
| R1 | **High** | walk 版全量 reconcile 不替换 dst 侧 symlink:src 目录路径上挡着一个指向真实目录的 dst symlink 时,整棵子树**写穿链接目标**(可写到镜像树外),verify 通过、状态绿、永不收敛 | §4 L1 |
| R2 | **High** | zfs diff 快路径不过滤 `.auto_sync_trash`:Compare 报假差异;zfs 版 Full 会把回收站嵌套搬运,且对 trash 目录本身的搬运失败会回退 `remove_dir_all`——**回收站可能被永久清空** | §2 S1 |
| R3 | **High** | 默认 1 秒 modify-window:同尺寸文件在上次已同步版本 mtime ±1s 内再次改写,事件虽到但被判"未变"跳过,**永久绿色漂移**;zfs-diff 基线随后推进,连 diff 都不再列出该路径 | §6 C1 |
| R4 | **High** | `FAN_REPORT_TARGET_FID` 被当成 FID 模式硬前提(需内核 5.17+):5.9–5.16 内核本可完整工作,却被一步踢进只有 MODIFY\|CLOSE_WRITE 的 fd-path 降级模式——delete/rename/mkdir 全部无事件,且**降级只有一条启动 warn,下游完全不可见** | §3 W1 |
| R5 | **High** | 配置变更触发的 watcher 重启(Linux)是静默丢事件窗口:既不升 restart notice 也不置 rescan;而**任何**配置字段变化(含发现线程写回的机器元数据)都会触发 stop→start | §5 P1 |
| R6 | **High** | 单个 source 的持久错误(拔盘/毒事件行)让整个引擎 pass 中止:配置序里它之后的所有 source 每 tick 饿死(同步、修剪全部停摆),且被饿死的下游无任何根因提示 | §5a E1 |
| R7 | Medium | dst 基线快照在 pass 结束才打:扫描/diff 之后落入 dst 的外部漂移被烘进新基线,之后所有 zfs-diff Compare/Full 都看不见(直到某次 walk 全量) | §2 S2 |
| R8 | Medium | 混合 cycle(同 source 有远程 dst)中,本地 ZFS dst 的基线被按"远程"处理无条件 Clear:双侧 zfs diff 快路径在混合配置下失效,61 万条目退化全树 walk | §2 S3 |
| R9 | Medium | 快照清理前缀匹配缺结尾分隔符:id 存在前缀关系的 source/destination 互删对方快照(含被引用的 diff base 与当前 dstbase) | §2 S4 |
| R10 | Medium | fanotify 缺 `FAN_ATTRIB` + 判等不比 mode:**文件权限/属主漂移是所有同步、校验、Compare 手段的共同盲区**(与 checksum 开关无关) | §3 W2、§7 |
| R11 | Medium | 事件持久化失败被 warn+skip:`queue_overflow` 这个"必须全量对账"的信号本身可静默丢失;每事件 2-3 个独立 fsync 自提交事务,HDD 上事件风暴易压出 overflow(而 overflow 的代价是 61 万条目全量对账) | §3 W4/W5 |
| R12 | Medium | `.auto_sync_trash` 只进不出 + `move_to_trash` 的 rename 任意失败静默降级为**永久递归删除**:回收站承诺两头漏 | §5a E4、§7 |
| R13 | Medium | 配置文件读-改-写全程无进程内互斥(6 个并发写入者),后写者覆盖前者;UDP 发现应答无认证即可写回持久配置(host/port 可被 LAN 伪造劫持) | §5 P2/P5 |

---

## 2. Q1:snapshot 机制设计是否合理

**总体:分层设计合理,dstbase 不变量在所有代码路径上被正确维护。** 三类快照各司其职:源侧 cycle 快照(稳定读视图,`SourceReadView::prepare`,幂等复用失败 cycle 的同名快照,"cycle=版本点"语义正确);dst 基线快照(`DstBaselineAction::{Refresh,Retain,Clear}`);按路径快照(zfs diff 并集展开)。本轮对**所有**标 verified 的成功路径逐一核对了三态选择:本地 zfs diff 增量→Retain ✔、双侧 zfs Full→Refresh ✔、walk 全量→Refresh/Clear ✔、事件路径→不动基线 ✔、transfer 路径→Clear(对远程正确,对混合 cycle 中的本地 dst 过度保守,见 S3)。

**Retain 语义做了完整数学推演,是对的**:Retain 后 src 基线推进、dst 基线停留,任何窗口期源变化要么已被写入 dst(必然出现在 dst 侧 `diff dstbase→live` 里被复查),要么内容本就相等——归纳到多次 Retain 链也成立,"diff 集合只增大,正确性不受影响"的注释准确。**崩溃原子性恰好安全**:基线两条 UPDATE 先 src 后 dst,中间崩溃留下的"src 新+dst 旧"正是 Retain 语义;Refresh 且 src 快照缺失时强制降级 None,杜绝无配对基线。所有失败点的降级方向全部核实为保守正确(diff 失败→回退全量、基线不匹配→拒绝快路径、清理失败→只 warn)。

**问题集中在快路径的三处工程缝隙:**

- **S1(High)快路径不过滤内部目录**(mod.rs:5985-6046、4615-4638)。全树 walk 在 Destination 模式跳过 `.auto_sync_trash` 等内部名(should_visit_path,mod.rs:6155-6163),但 zfs diff 并集与按路径快照完全不过滤。mirror 删除写入 trash 的 rename 必然出现在 dst 侧 diff 里(增量 Retain 不推进 dst 基线,漂移必报):(a) zfs diff Compare 把回收站内容报成假 to_delete 差异,与回退 walk 对同一棵树给出不同报告;(b) zfs 版 Full 会把 trash 条目再 move_to_trash 造成嵌套搬运;**更糟**:diff 也会报出 `.auto_sync_trash` 目录本身,对它的 move_to_trash 是把目录 rename 进自己的子树(POSIX 必败 EINVAL),随后回退 `remove_any` = `remove_dir_all` 把整个回收站永久删除。修复:并集进入前按 rel 首段过滤 INTERNAL_TMP/TRASH/PROBE。
- **S2(Medium)基线刷新的时间窗**(mod.rs:4948、2338-2377)。walk 全量对未触碰路径的一致性确认发生在 pass 开头的扫描时刻,而 Refresh 的 dstbase 快照在整个 pass 成功后才打;双侧 zfs Full 的 dst 变更集也是同步前一次算好。数小时的 Full 期间外部写入 dst 的路径 P 会被烘进新基线,此后所有 zfs-diff Compare/Full 都看不见 P——直到某次 event-loss reconcile 或 diff 失败回退 walk 才能发现。三个 Refresh 调用点(2422/2369/2492)窗口时长不同,需一并处理(先打快照对快照校验,或刷新后对窗口做一次 `zfs diff` 断言为空)。
- **S3(Medium)混合 cycle 的本地 dst 基线被误 Clear**(mod.rs:3068-3075、3179-3186)。任一远程目标使整个 cycle 走 transfer 路径,per-dst 循环不区分本地/远程,成功一律 Clear(注释自己假设"destination is remote here")。NAS 源加一个远程 dst 后,本地 /zfs_pool 的双侧快路径基线活不过任何一个混合 cycle。修法:本地 ZFS dst 增量→Retain、全量→Refresh,只对真远程 Clear。
- **S4(Medium)清理前缀匹配缺边界校验**(mod.rs:4787-4807、4891-4898、5006-5018)。前缀 `{prefix}_{id}` 裸 `starts_with`,无结尾分隔符、不校验 12 位 cycle 后缀:id "docs" 的清理会按自己的 keep 窗口删 "docs_old"/"docs2" 的快照(referenced 集合只查本 source);dstbase 同病,destination id "a" 会销毁 (src,"a_b") 的当前基线;极端者 source id "d" 的前缀 `auto_sync_d` 匹配所有 `auto_sync_dstbase_*`。均为退化非损坏,但 61 万条目下代价高且难排查。注意 `sanitize_snapshot_component` 保留 `_`,仅补分隔符不够,须校验后缀恰为 `_`+12 位数字。
- **S5(Low)配置变更/删除产生盘上快照孤儿**(state.rs:1427-1444)。reset 只清 DB 名称;删除 destination/source、改路径换 dataset 后,旧 dstbase/source 快照无任何回收路径。且全库没有 `DELETE FROM destination_offset`,被删 dst 的残留行会让 `source_referenced_snapshots` 永久钉住对应 source 快照。与 system_design.md §7"删除 dst 后释放引用"相悖。建议提供按前缀扫描比对当前配置的孤儿 reaper。
- **S6(Low)文件 source + 本地 ZFS 目录 dst**:每次 verified 都对整个 dst 数据集打 dstbase 快照,但文件 source 的基线没有任何消费者(Compare/Full 快路径都要求 dir source),白钉整个数据集的 churn 空间;应改 Clear(mod.rs:2500-2505)。
- **S7(Low)compare 与 sync 互斥是单向的**(mod.rs:395-397):sync 给 compare 让路(waiting_for_compare),但 Scan 可在同 destination 的 sync 进行中启动——读到半更新树报瞬态假差异;sync 收尾的基线清扫还会使进行中的 compare 从快路径降级全树 walk。自愈、只影响报告;补齐方向是 compare 入口对同 dst 活跃 sync fail-fast。
- **S8(Low)两个清理器错误处理各偏一端**(mod.rs:5016-5018、4823-4829):dstbase 清扫不查 `zfs destroy` 退出码(hold/busy 静默当成功);source 快照清理则任一非零直接 bail——排序靠前的一个被 hold 的旧快照会让其后所有可删快照永远清不掉。统一为"失败记 warn 并 continue"。

**被证伪后修正的两条**(原结论不成立,残余问题记录在案):
- "scheduled 同步走事件积压+live 读是缺陷" → **是文档化(system_design.md §8)且用户接受的设计**;watcher 死亡盖绿的场景因 F8 已修不再成立。残余(Low):Linux fanotify 持续 setup 失败时不像 Windows 那样写 `rescan_required` 事件,仅每 ~5s error 日志(→W3)。
- "任一 dst 持续失败期间快照无限累积" → **不成立**:失败 dst 被钉在同一 target cycle 重试,快照按 cycle_id 幂等复用,上界=每失败 dst 1 个+keep 窗口。残余(Low):健康 dst 触发的 cleanup 可能提前销毁失败 dst 钉住的快照(referenced 只含 last_verified),重试时同名重建但内容已更新——读视图内容漂移的边角。

---

## 3. Q2:fanotify 使用是否合理

**总体:主路径(FID/name 模式)是这个代码库里质量较高的部分。** 标志组合与 info record 解析正确(FID/DFID_NAME 布局、fsid 跳过、MAX_HANDLE_SZ 上界、FAN_EVENT_NEXT 迭代);每 source 独立 group/fd/线程,overflow 隔离且正确置 `rescan_required=true`;armed 握手时序正确(mark 全部就位后才 on_armed,调度器等 armed 后才升 restart notice);F8(监督重启)、F9(handle 缓存清理)均已修复。lazy `open_by_handle_at` 模式下无法解析的事件当作同文件系统噪声丢弃是安全的。

**真问题(按严重度):**

- **W1(High)降级链条一步到底 + TARGET_FID 硬前提**(fanotify.rs:273-277、228-241)。`fanotify_init` 一次性要求 4 个 FID 标志,其中 `FAN_REPORT_TARGET_FID` 需内核 5.17+;5.9–5.16(Ubuntu 22.04、Debian 11、RHEL 9 都在此区间)EINVAL 后不尝试去掉它的组合(解析层根本不依赖它),直接落到只有 `MODIFY|CLOSE_WRITE` 的 fd-path 模式——delete、rename(含编辑器"写 tmp 再 rename"原子写,最终路径永无事件)、mkdir 全部无事件,mirror 增量静默漏删漏改名而绿点照常。整个降级只有一条启动 warn:无持久状态、UI 无标记、不置 rescan。修复:分级回退(先全组合→去 TARGET_FID→FID-only→fd-path),且进入 fd-path 时对每个 open cycle 持续置 rescan_required(或等价持久告警)。
- **W2(Medium)缺 `FAN_ATTRIB`**(fanotify.rs:219-227)。touch/chmod/chown 无事件;Windows USN 侧对应 reason(BASIC_INFO_CHANGE/SECURITY_CHANGE)都在监听,设计文档 §6 也列了 attrib——两端不对等。mtime-only 变化在事件增量下永不传播。注意即使加了 ATTRIB,判等器不比 mode 仍会拦截修复(见 §7 R10),两处要一起修。
- **W3(Medium)watcher 进程内重启窗口无标记**(fanotify.rs:149-170;auto_sync.rs:687-693)。supervisor 出错重启(≈5s 退避,旧 fd 关闭时内核队列未读事件全丢)与配置变更重启之间的丢事件既不记 rescan_required 也不重升 restart notice(raise_restart_notices 只在进程启动调用一次)。fd-path 回退的逐目录 mark 模式下重启窗口可达分钟级。同族:持续 setup 失败(如权限变化)期间 marks 整段不在位,同样无标记。修复:重启/重试路径记一条合成 `rescan_required=true` 事件,复用现成的 event_loss_reconcile。
- **W4(Medium)持久化失败=静默丢事件**(fanotify.rs:448-469)。F8 修复把 record_event 失败降级为 warn+skip,但 `FAN_Q_OVERFLOW` 分支走同一容错——"内核已丢事件必须全量对账"的唯一信号在 DB 磁盘满/IO 错/外部进程持锁超 30s 时被吞,绿点照常;溢出分支内 per-source 循环还用 `?` 短路,第一个失败连带跳过其余 source。修复:内存 sticky 标志,失败后首次成功写入前补写 rescan 事件。
- **W5(Medium)每事件双写 + 逐语句 fsync**(fanotify.rs:517-538;state.rs:917-964)。TARGET_FID 使每个事件带两条 info record,各 insert 一行 event_log(同路径写两行);record_event 内部 2-3 条独立 autocommit,`synchronous=FULL` 下每条一次 WAL fsync。HDD 上吞吐几十事件/s,批量拷入/解压时事件产生速率高两个数量级,16384 的内核队列几十秒溢出→触发 61 万条目红色全量对账。修复三管齐下:去掉 TARGET_FID(顺带修 W1 的内核前提)、同事件同路径去重、按 read() 批次包事务(见 §8)。
- **W6(Low)FAN_MARK_FILESYSTEM 不跨挂载点**(fanotify.rs:322)。source 下嵌套 ZFS dataset/异 fs bind mount 是另一个 superblock,事件全部不可见且无检测;Full 的 walk 会跨挂载点修一次,形成"Full 修好、增量永远漂移"的循环;zfs snapshot/diff 快路径同样 dataset 级盲(连 Compare 快路径都看不到子 dataset)。当前部署无嵌套 dataset,但产品隐含"source 树内无异 fs 挂载"假设且从未校验。至少启动时读 mountinfo 检测并告警。
- **W7(Low)removal 缓存清理 O(map) retain、缓存无上限**(fanotify.rs:859-863、677-683):文件 handle 也全量缓存(与字段注释"只存目录"不符),大子树 rm -rf 是 O(N×M) 前缀比较;改分层反向索引或只缓存目录。
- **W8(Low)eager 回退模式(open_by_handle_at 不可用)下 overflow 期间丢 create 的新目录成为 watcher 永久盲区**:event_loss_reconcile 修内容但不重建 handle map。lazy 模式(当前 NAS 部署)不受影响。
- **W9(Low,证伪后残余)**:per-dir 回退模式的新目录 mark 竞态**有下游防护**(父目录的 create 事件带路径,同步时对该目录整棵递归快照,gap 内文件照常同步)——原报告不成立;真正的窄洞是 `mark_directory_tree` 运行期撞 `max_user_marks`(ENOSPC)仅 warn 不置 rescan,该新子树自此永久失 watch(fanotify.rs:695-697),建议该失败分支记 rescan 事件。

---

## 4. Q3:symlink 处理是否合理(含跨机、跨系统)

**总体:同构系统的 symlink 生命周期正确且自洽**;上一轮两个 High 中"Linux 目录链→Windows 建错类型"**已修复**(线格式带 `is_dir`,源侧判定,mod.rs:935-947、6693-6710),"跨 OS 回读反复重传"转化为可见硬失败。全程 lstat 语义:symlink 指向目录不跟随、链接环不递归、树外指向不卷入;判等只比 `symlink:<target>` 字符串(不比 size/mtime,避免抖动,取舍合理);mirror 删除 rename 链接本体不进目标;悬空链接正常镜像。

**本轮新发现一个 High:**

- **L1(High)walk 版全量 reconcile 不替换 dst 侧 symlink**(mod.rs:5544,同型 5351)。`copy_missing_directory_tree` 用 **follow 语义** `target.exists() && !target.is_dir()` 判断是否替换:src 是目录、dst 同路径是**指向真实目录的 symlink**(典型演化:源曾是 symlink 已同步,后改为真实目录)时两者皆真被跳过——整棵子树写穿 symlink 落到链接目标处(可在镜像树外、甚至写回源树),verify 逐路径 lstat 沿中间链接解析而通过,状态绿;且 dst 扫描不遍历 symlink 内部,子项每轮全视为缺失,**每轮 Full 整棵子树穿链重拷,永不收敛**。dst symlink 悬空时则 create_dir_all 报错整轮红。该路径不仅是 zfs diff 的 Full 回退,也是所有非 ZFS 源(Windows 源)的标准 Full 和 event_loss_reconcile 路径。另两条修复路径(zfs-diff 增量的 type_mismatch 预 trash、跨机版先 remove 再 mkdir)都是对的,唯独这条有洞。修复:改 lstat 判断,或进入该分支时(dst_map 已知 wrong-type)无条件先 trash;补集成测试。
- **L2(Medium)fast_missing_dirs 类型翻转绕过回收站**(mod.rs:6447)。src 变 file/symlink、dst 同路径是真实目录时,该路径无预 trash pass,`replace_path` 判 incompatible 后直接 `remove_any` = `remove_dir_all`,整棵旧目录树**不进回收站永久硬删**——违背 system_design.md "删除默认进 trash" 的承诺(zfs-diff/事件路径有预 trash,正确)。
- **L3(Medium)目的根本身是 symlink→目录**(mod.rs:6105-6112):resolve/online 检查 follow 放行,但扫描端 lstat 判根非 dir 直接返回**空清单**——每轮全树重拷、mirror 删除永不执行、状态恒绿。触发需 `add_directory=false` 且 dst.path 为 symlink(Linux 上 `/data/xxx→/pool/xxx` 常见布局)。UI 浏览器藏 symlink 降低了触发概率(见 L8)。应在 resolve 阶段 lstat 检查:拒绝或 canonicalize。
- **L4(Medium)跨系统 target 不做分隔符/绝对路径转换**(mod.rs:1859-1862)。Windows 源的 `..\data\file`、`C:\...`、junction 在 NAS 上被创建成含字面反斜杠的断链;因 target 是不透明字节且两侧回读一致,hash 判"in sync"——**字节保真、语义破坏,Compare/Full 永远检不出**。反方向 Linux→Windows 多为响亮失败。真实部署(Windows Documents→NAS)中源里任何 mklink/junction 都会以此形态落地。至少对相对 target 做分隔符归一,绝对/不可转换 target 记黄色 issue;若定位为"备份保真"取舍应在文档明示。
- **L5(Medium→实测修正)Windows 目的端目录 symlink 的删除路径**(mod.rs:6625-6633):`remove_any` 对一切 symlink 走 `remove_file`,对目录 reparse point 实测 ACCESS_DENIED——触发面收窄为"dst 已有目录链、src 变普通文件"等 remove_any 路径(每轮红、无自愈);`replace_path` 直接 rename 覆盖目录链与 move_to_trash 实测**成功**(更新 target、镜像删除正常)。修法:Windows 下对 symlink 先试 remove_file 再 remove_dir。
- **L6(Low)跨机 target 经 `to_string_lossy`**(mod.rs:1861):非 UTF-8 target 被 U+FFFD 损坏且双侧 hash 一致无法检出(本地路径走原始字节,保真)。线格式改 bytes/base64。
- **L7(Low)远端 symlink 在扫描与推送间隔中变化被判硬失败(红)而非 source_changing(黄)**(mod.rs:1563-1565):发送端无防护、接收端 hash mismatch 文案不在 source_changed_paths 识别集,白费 3 次重试后计入 20 个失败额度;本地路径同场景正确转黄。发送端推送前重算 hash 即可。
- **L8(Low)目录浏览器静默隐藏所有 symlink**(backend.rs:1330-1336):lstat 分类后 symlink 既不可见也不可进入,用户想选 symlink 目录只能手改 toml(然后踩到 L3/源根按单文件处理的边界)。
- **L9(Low)Windows 无 symlink 创建权限时无降级**:提权失败 + 未开开发者模式时每条链接反复硬失败,攒满 20 条中止整批;应捕获 ERROR_PRIVILEGE_NOT_HELD 记黄色跳过。因默认自提权,实际暴露面小。

**场景正误清单**:正确——同 OS 本地/跨机的新增/target 变更/删除/悬空镜像、类型翻转(zfs-diff 版与跨机版)、链接环/树外指向、拷贝中变化转黄(本地)、Linux 目录链→Windows 类型(已修);出错——上表 L1–L9,另记录:源根是 symlink→目录时按**单文件 source** 处理(不跟随是安全默认但 surprising,未列 finding)。

---

## 5. Q4:代码不合理之处

### 5a. 引擎层(sync/mod.rs、delta.rs、storage.rs)

**总体:引擎核心数据通路健康**——tmp+fsync+原子 rename、端到端 blake3、cancel token 贯穿 worker 池与树遍历、`TransferOutcome` 三分类失败语义都核实无误;delta.rs 实现正确且有全文件哈希兜底。不合理之处集中在五类:错误隔离、文件名鲁棒性、快照身份、清理时机、批发拷贝容错。

- **E1(High)单 source 持久错误饿死其后所有 source**(mod.rs:279)。`sync_all_pending_inner` 对每个 source 的 cycle 用 `?` 直接上抛,一个 source 出 Err 即整个 pass 返回——其后 source 的同步、path_snapshot 修剪、事件修剪全部跳过,调度器只记日志 5s 后原样重试。持续复现的 Err 出口有两个:source 路径不可用(拔盘/未挂载,mod.rs:2018-2033,failed cycle 每 tick 重驱重败)和事件 rel_path normalize 失败(mod.rs:2656,毒行只在全部 dst verified 后才修剪,永不消费;正常运行产生不了,需 DB 损坏或 scan_repair 注入)。失败 source 本身有红色状态可见,**真正无根因提示的是被饿死的下游 source**(只显示"落后")。修复:cycle 级错误就地捕获后 continue 下一 source,仅 cancel 与 DB 级错误上抛。
- **E2(Medium)含反斜杠/非 UTF-8 的合法 Linux 文件名**(mod.rs:1913、6769、4662)。`normalize_rel_path` 在 Linux 上也把 `\` 替换成 `/`;非 UTF-8 名被 lossy。事件/zfs-diff 增量路径:改写后的路径被按路径快照静默判"不存在"(NotFound 容忍)→ 真实变更永不同步、verify 通过、标绿;**mirror 开启时更糟**——该 rel 被当 extra,`move_to_trash` 裸 join 命中真实的 `a\b`,会真正删除目的端副本;基线推进后 diff 也不再列出,永久静默漂移。Full 路径:非 UTF-8 名每轮打不开源文件永久红;反斜杠名本地 full 单轮红后自愈,跨机 full 则 finish 端经 safe_join_rel 落成嵌套 `a/b` 目录,布局静默改变、每轮重传永久红。修复:`\`→`/` 仅限 Windows 来源;本地 copy/verify 统一同一 join 函数;非 UTF-8 走原始字节或至少记黄色 issue。
- **E3(Medium)快照身份只由名字决定**(mod.rs:4761-4771、4986)。`ensure_zfs_snapshot` 见同名即复用,无创建时间/世代校验;快照名=prefix+source_id+cycle_id,而 cycle id 是 SQLite 自增——**DB 丢失/重置后从 1 重计**,与盘上未回收的旧世代孤儿快照(S5 已证不回收)同名碰撞时,陈年快照被当作本轮读视图:dst 被覆盖回旧内容、mirror 把新文件搬进 trash、verify 对着陈旧快照通过并标绿;dstbase 同病,直接破坏基线不变量。一般为单轮窗口(下轮号不碰撞则 diff 自愈),连续碰撞则持续假绿。修复:快照名嵌入建库时生成的世代 token,或 ensure 时校验快照 creation ≥ cycle starts_at。
- **E4(Medium)`move_to_trash` 的 rename 任意失败静默降级为永久递归删除**(mod.rs:6475-6480)。`Err(_)` 捕获全部错误后直接 `remove_any`(目录=remove_dir_all),丢弃原错误:dst 内嵌套挂载点(EXDEV)的条目**完全静默地被永久删除且上报成功**;Windows 锁定目录则先删掉可删子项再报错(部分子树不可恢复)。9 处调用点全经此函数,打破"删除默认进回收站"的设计承诺——§2 S1 的"回收站被清空"正是此回退作用于 trash 目录自身的特例。修复:只对可证明安全的错误(目标已不存在)回退;EXDEV 改 copy+delete 进 trash;其余上抛并 warn 记录原错误。
- **E5(Medium)`.auto_sync_tmp` 跨 cycle 泄漏**(mod.rs:6891-6898、3346)。跨机传输把半成品写入 dst 的 `.auto_sync_tmp/<cycle>/`;失败/取消时 cleanup 在 `?` 之后被跳过——这对同 cycle 断点续传是必要的,但 target 一旦移到新 cycle(取消清 target 后新事件建新 cycle、手动 sync 强制新 cycle),旧 cycle 目录里可达 GB 级的半截大文件**永无人清**:cleanup_tmp_cycle 只删当前 cycle 子目录,全代码无遍历 `.auto_sync_tmp` 的清扫;该目录又被 dst 扫描排除,Compare 不可见。设计文档 §13 的"重启/下次同步时清理"只实现了同 cycle 一半。本机路径不受影响(四处调用均无条件清理)。与 R12 的 trash 只进不出同构,修复时一并:pass 收尾清扫非当前 target cycle 的 tmp 子目录(两侧同做)。
- **E6(Medium)批发子树拷贝零失败容错**(mod.rs:5595-5601、5646-5651)。顺序路径首个硬错误即 Err;并行路径池内虽容忍 20 个,但 flush 只要有失败就整体 Err——错误经 `?` 中止整轮,后续子树、to_copy、mirror、校验全部跳过,违反设计文档"单文件硬失败容忍 20 个"的契约。**首次全量到空 dst(全部走批发)完全零容错**,61 万条目里一个 EACCES 就整轮红从头再来。缓解:已拷文件持久,下一轮失败文件改走容忍 20 个的 to_copy 路径,k 个坏文件约 k 轮后收敛——非"永不收敛",但每轮多付一次全树扫描。修复:批发两条路径把 ≤20 个失败聚入 outcome 延后裁决,与主批次统一。
- **E7(Low)copy_file/copy_symlink 的 tmp 预清理用 follow 语义 `exists()`**(mod.rs:6255、6426):残留悬空链接不被删,copy_file 会写穿链接在目标处新建杂散文件(触发需进程硬杀+同 cycle 重试+PID 复用,窗口极窄,不覆盖既有数据);同文件 receive_symlink_target(1558)已是正确写法,统一即可。
- **E8(Low)storage.rs 的 zpool 介质判定把 cache/log/spare vdev 一并计入**(storage.rs:115-128):全 SSD 数据池挂一块 HDD 热备/L2ARC 即被判 rotational,批发并行静默退化为串行(方向安全,只丢性能)。解析时按 zpool -v 段落排除非数据 vdev。
- **E9(Low)`zfs_filesystems` 用 split_whitespace 解析 mountpoint**(mod.rs:4736-4742):`-H` 输出是制表符分隔,含空格的挂载点被截断——resolve 不到 dataset(auto 回退活树读、zfs 强制则失败、快路径失效),截断路径还可能误匹配无关目录。改按 `\t` 切分(parse_zfs_diff 已是这么做的)。
- **E10(观察,未单独验证)**:sync/scan 的 kind 标签有全局字符串共享点(mod.rs:66 附近),并发时 task_log 标签理论上可能错乱;以及 L1/L2、S1-S4、C1、并行度门控只用一半(§8)等引擎问题已在各自小节覆盖,不重复。

### 5b. 状态 / API / 配置层

**骨架健康**:WAL+synchronous=FULL、task_log 惰性修剪+重启 abort、cancel 严格目标匹配、path_snapshot/event_log 分块修剪、task-wait 长轮询实现都核实无误。结构性弱点是**"配置文件"和"每请求新开的 State"都没有进程级并发控制**:

- **P1(High)配置变更触发的 watcher 重启静默丢事件**(auto_sync.rs:686-693)。`raise_restart_notices` 只在进程启动调用一次;config 变更 stop→start 之间(lazy 模式亚秒~秒级,eager 预建模式**分钟级**)丢的事件无 notice 无 rescan。且 `config_signature` 覆盖整份 TOML——发现线程写回机器元数据、controller 推送、UI 偏好都会触发重启,窗口出现频率不低。修复:重启路径也升 notice(或置 rescan);signature 只哈希 watcher 相关字段。
- **P2(Medium)配置读-改-写无互斥**(backend.rs:69;config.rs:417-436)。六个写入入口(save_config 两路、add/remove_machine、peer 推送 apply_delegated、发现线程 30s 落盘)全是 load→mutate→save 无锁,交错时后写者用旧快照覆盖前者(最宽窗口:发现线程 load 后跑 ~700ms 才 save);tmp 文件名固定共享。加进程级 CONFIG_MUTEX + tmp 加 pid 后缀。
- **P3(Medium)GET 端点每请求写库**(backend.rs:156-163;state.rs:166)。每请求 `DbState::open`(重跑全套 CREATE TABLE+PRAGMA 探测)+`ensure_config`,指纹门控在 per-instance Cell 里永远 miss——每次 /api/status 轮询(5s)都对每个 source/dst 行做带新 updated_at 的 upsert,每条一次 WAL fsync。"空闲零写入"目标只对调度器成立,UI 一开就破。指纹改进程级 static,或只读路径不调 ensure_config。
- **P4(Medium)sync_now 串行 `?` 传播**(backend.rs:216-222):任一远端 source 机器离线,Sync All 整体报错且**本地 source 也不同步**;部分成功时呈"远端已触发+本地没动"。与 status 聚合的降级哲学自相矛盾。改尽力而为+本地无条件执行。
- **P5(Medium)UDP 发现应答直接写回持久配置**(backend.rs:1240-1261)。一个伪造的明文 UDP 应答(alias/id 命中已配置机器)即可把该机器的 host/port/os/install_dir 无校验持久化——此后同步/控制 HTTP 全部打到攻击者端点。ssh_user/ssh_port 也被写入但运行时无消费(deploy 脚本用命令行参数),仅 UI 误导级。F6 的新派生面:无需 TCP 可达即可完成持久污染。host 应以 UDP 源地址为准或经 TCP 回读确认。
- **P6(Medium)save_config 半应用**(backend.rs:69-87):本地写盘、offset 重置都生效后,delegation 推送失败才返回 Err——UI 显示"失败"但本地已改,远端滞留旧配置且**无后台重推/对账**,控制机与执行机配置可无限期分叉。推送失败应降级为携带 per-machine 警告的成功+后台补推。
- **P7(Medium)peer 连接只接受 IP 字面量**(machines.rs:994-996):`SocketAddr::parse`,host 填主机名的机器所有 peer HTTP 立即失败,而同文件 ssh 探测用 `to_socket_addrs` 支持 DNS——不一致且入口无校验。
- **P8(Medium)Tauri 非 async command 在主线程跑阻塞 I/O**(auto_sync.rs:832-896):get_status/get_sync_activity 对远端串行 3s 超时/台,NAS 不可达时桌面窗口每个轮询周期冻结 3-6s;web 层有 blocking() 卸载,Tauri 未对齐。全部改 async + spawn_blocking。
- **P9(Low)`advance_due_destination_targets` 用关闭前读到的旧 needs_full_rescan 清标志**(state.rs:548-556):毫秒级窗口内并发到达的手动 Full 被静默抹掉(clear 还顺带清 manual_* 标志)。条件写或只清 needs_full_rescan。
- **P10(Low)`ensure_open_cycle` 无锁 check-then-insert**(state.rs:437-451):多连接可造双 open cycle,旧行永久滞留(数据不丢,幽灵行累积)。IMMEDIATE 事务或 partial unique index。
- **P11(Low)/api/status 远端聚合串行 3s/台**(backend.rs:718-770),且按 source group 而非机器去重;all_tasks 已并行,status 未对齐。
- **P12(Low)`preferred_local_host` 硬编码偏好 192.168.2.0/24**(config.rs:968-969):环境耦合进核心库;无默认路由的纯内网机退 127.0.0.1(web 只绑 loopback),默认路由走 VPN 的多网卡机自报错误网卡。应做成配置。

另核实为正确/可接受:SQL 全参数化无注入面;task_log 修剪正确(running 免疫);cancel 目标匹配严格不误杀;启动顺序(armed→notice→调度)正确;`reconcile_interval` 仍强制非零校验但无消费者(死配置小坑)。

---

## 6. Q5:同步时文件正在写入,最终状态会不一致吗

**结论:绝大多数路径不会。** 系统有完整的多层防护链:ZFS 快照只读视图 → 读后 lstat 复核(`ensure_source_stable`,size+mtime_ns 精确比较,mod.rs:6558-6574)→ 跨机 blake3 端到端 → source_changing 黄色容忍+failed cycle 每 tick 重驱 → 事件驱动下一轮收敛。逐路径核实:

- **(a) NAS ZFS 快照路径**(walk 全量、zfs diff 增量、双侧 Full、跨机本机源):读的是不可变快照,"正在写入"完全不影响本 cycle。
- **(b) 活树读路径**(Windows 全部;**所有事件增量路径**——包括 NAS weekly 的常规积压 pass,读的是活 /zfs;auto 回退):写入方并发修改由读后 lstat 抓住(内容写入必然推进 mtime),污染的 tmp 立即删除、转黄不标绿;下一轮收敛。
- **(c) delta 路径**:长度复核+lstat 复核+全文件 blake3,与 (b) 等价。
- **(d) put-file/chunked 中途变化**:读到 EOF 提前即 source_changing;断点续传的"前旧后新缝合怪"被全文件哈希封死(mismatch→删 tmp→重传)。
- **(e) verify 校验的是"cycle N 的快照条目 vs dst 实际"**,复制后源再变不会使校验失败——这是正确设计,新变化由新事件落入下一 cycle 追平。
- **(f) 收敛闭环**:同步期间的新写入事件即时唤醒调度器开新一轮;source_changing/failed cycle 每 tick 重驱直到成功。

**两个真实的漏检窗口(都源于默认 checksum=false 下判定完全依赖 size+mtime):**

- **C1(High)1 秒 modify-window 吞掉"事件已知变化"的同尺寸改写**(mod.rs:6522、6545-6548;config.rs:16 默认 1s)。精确触发条件:realtime 把版本 A(源 mtime t0)写入 dst 并把 dst mtime 强制设为 t0;**A 复制完成之后**,写入方产生同尺寸版本 B 且 mtime 与 t0 差 ≤1s,此后不再改动。B 的事件确实触发下一 cycle,但源 (S, t0+0.8s) 与 dst (S, t0) 被 entries_match 判一致——复制跳过、verify 通过、标绿。**永久漂移**:Compare/Full walk 用同一判等,默认配置全看不见;zfs-diff 路径更糟——跳过发生的那个 cycle 成功后源基线推进到含 B 的快照,此后 diff 连该路径都不再列出。唯一修复途径是开 checksum 跑 Full。编辑器双保存、固定长度状态文件、数据库页写等亚秒同尺寸改写真实可达。**系统拿到了变化证据(事件、首轮 diff 输出)却被容差层丢弃**,这是本轮最值得修的正确性问题:对事件/diff 集合内的路径不用宽容比较(无条件复制或强制 hash),modify-window 只留给"无证据的全树 quick-check"(其本意是吸收 Windows↔Linux 时间戳粒度差,收紧到 100ns 级也可达目的)。
- **C2(Medium,文档已声明的已知局限)元数据不可见的写入穿透守卫**:mmap 写(解除映射前不更新 mtime)、时间戳冻结、写后回设 mtime——读到撕裂内容而 lstat 复核通过,发布为 verified。若写入方随后有可见写入则下轮覆盖(瞬态);否则与 C1 叠加为持久漂移。checksum 模式可拦截(两次独立读一致才发布);system_design.md §7 明确声明了"没有稳定快照时只能保证不发布检测到不一致的结果"——实现与文档一致,属 rsync 同级取舍。缓解:活树源建议开 checksum,或 ensure_source_stable 加 ctime 复核(可抓 mtime 回设,抓不了 mmap)。
- **C3(Low,文档偏离)**:ZFS 源最常运行的事件积压路径读活树,与 system_design.md §9"ZFS 后端所有 copy/verify 必须来自只读 snapshot"的强承诺不符(行为可收敛、与用户接受的绿点语义一致,但 §9"增长中文件不会读到半截"的字面保证在周常路径上不成立)。修文档或让事件 pass 在 ZFS 源上也套 SourceReadView(每轮一次瞬时快照)。

---

## 7. Q6:文件不一致了,手动手段能修复吗

先回答字面问题:**"手动 Changed Since"已不存在**(模式已移除,wire 值被拒)。现存手段 5 种:incremental(事件)、自动 event_loss_reconcile、手动 Full(walk 版)、手动 Full(zfs diff 版,仅本机双 ZFS+双基线)、Compare→Repair(截断报告自动升级 Full)。判等核心 `entries_match`(mod.rs:6515-6526):file→checksum 开时 size+hash,关(默认)时 size+mtime±1s;symlink→target 字符串;dir→仅 mtime;**mode/uid/gid 完全不参与**。

**场景 × 手段矩阵(默认 checksum=false):**

| 场景 | incremental | Full(walk) | Full(zfs diff) | Compare→Repair |
|---|---|---|---|---|
| ① dst 内容损坏,size+mtime 未变 | 否 | **否**(checksum=true 才行) | POSIX 改写会进 dst diff 并集但仍被判等放行→**否**(checksum=true 则**是**且只 hash 并集,代价小);纯 bitrot 无写入 diff 不可见(ZFS 侧靠 scrub 兜底) | 同左列 |
| ② dst 文件被改(mtime 变) | 否(无 src 事件,绿点保持——已接受) | **是** | **是**(Retain 下基线虽旧,dst diff=旧基线→活树,窗口只大不漏,并集完备——不变量推演见 §2) | **是** |
| ③ dst 文件被删 | 否 | 是 | 是(dst diff `-`) | 是 |
| ④ dst 多出文件 | 否 | 是→进 trash | 是(dst diff `+`) | 是 |
| ⑤ src 与 dst 都变 | 是(src 赢) | 是 | 是 | 是;delta 路径即使 dst basis 损坏也安全(重建只由 src 内容+blake3 决定) |
| ⑥ symlink 不一致 | 需 src 事件 | 是(target hash 恒比较) | 是 | 是(跨 OS 缺陷见 §4 L4) |
| ⑦ 目录结构不一致 | 需 src 事件 | 是(缺补/多删/类型替换;目录 mode+mtime 每次全量重置) | 同左,只重置并集内目录 | add/delete/type_mismatch 会报;**目录 mtime 漂移 Compare 有意不报**(Full 顺带修但报告看不见) |

**当前任何手段都发现不了的盲区:**
1. **checksum=false 时 size+mtime 复原的内容改写**(①行全线失守)。尤其可惜:zfs-diff 快路径明明拿到了"dst 自基线后被写过"的确凿证据(diff M 行),仍被判等层丢弃(mod.rs:5817-5824)——且 manual Full 成功后 Refresh 把被篡改状态**烘进新基线**,此后连开了 checksum 的快路径都看不见(只剩 checksum 全树 walk),与 mod.rs:2305-2308"dst 基线留给未来发现 dst 漂移"的注释意图矛盾。修复代价小:对"仅出现在 dst 侧 diff"的路径强制 hash 对比。
2. **文件权限/属主漂移**(R10):判等不比 mode、fanotify 无 ATTRIB、chmod 不改 mtime——src 端权限修改永不传播、dst 端漂移永不修复、Compare 永不报告,与 checksum 开关无关;目录是唯一例外(每次 Full 被 set_dir_mtimes 顺带重置)。权限敏感数据目前无解。
3. **verify→dst 基线快照之间落入 dst 的写**被烘进基线(§2 S2 的同一窗口,修复时一并处理)。
4. **`.auto_sync_trash` 只进不出**(R12,mod.rs:6460-6482):全代码库无任何 trash 清理逻辑,设计文档承诺的"按保留策略清理"未落地;trash 又被排除在 dst 扫描外(Compare 不报),空间单调增长直到 ENOSPC 连锁拖垮同步。需要落地保留窗口清理 + UI 暴露 trash 大小。同族:跨机路径的 `.auto_sync_tmp/<cycle>` 在 target 移到新 cycle 后同样永不清理(§5a E5,GB 级半截文件),清理时一并处理。

**用户兜底组合建议**:NAS 本机对(/zfs→/zfs_pool)——例行 Compare(双基线在时秒级)+差异 Repair;每月手动 Full 刷新双基线;把该 dst `sync.checksum=true`(zfs-diff 快路径下增量/Compare/Full 都只 hash 变化并集,代价很小,直接覆盖盲区 1);bitrot 交给 `zpool scrub`。Windows→NAS 对——Compare(walk)当例行体检+Repair;checksum=true 意味着全树双侧 hash,建议仅怀疑损坏时临时开启跑一次 Full。

---

## 8. Q7:性能与代码质量优化空间

传输层(并行 push、bulk dirs/removes、put-file 单往返、delta、连接池)几轮 rework 后已比较健康。残留热点按对真实部署的影响排序:

**一、61 万条目场景的扫描/DB 热点(最值钱的四条)**
1. **(Medium)本地 Full walk 回退时 dst/src 两棵树串行扫描**(mod.rs:5320→5332),Compare 和跨机路径都已双侧并行——NAS 上 /zfs 与 /zfs_pool 是独立池,并行可把扫描阶段近似减半,一次就是几十分钟级。注意并非顺手改动:src 走树与批发拷贝融合,需先纯扫描或后置复制。
2. **(Medium)全树扫描每条目 3 次 lstat**(mod.rs:6120、5965、6063):`read_dir` 免费的 file_type/metadata 被丢弃,61 万条目一棵树 ≈183 万次 lstat,实际 60 万够;Windows 上每次 lstat 是完整句柄往返,代价最高。管道传 metadata 即可,scan 阶段 syscall 减 60-70%。
3. **(Medium)`prune_path_snapshots` 每个调度 tick 全表扫**(mod.rs:289;state.rs:1697-1699):DELETE probe 的子查询无可 seek 索引(PK 首列是 cycle_id),61 万行表无行可删也 ~60ms/次且持进程写锁,空闲机器每 ≤5s 白烧。短期加脏标记/索引;长期随 path_snapshot purge 计划一并消失(Full 每次还写 61 万行无读者数据 ≈31 个 chunk 事务)。
4. **(Medium)watcher 每事件 2-3 个独立 fsync 自提交事务**(state.rs:917;fanotify 一次 read() 一批却逐条落库):事件风暴时持久化吞吐被 fsync 限死→推高 overflow→代价是全树对账。按 read 批次包事务,吞吐提升 1-2 个数量级,at-least-once 语义不变(W5 同源)。

**二、zfs diff 快路径的"按路径快照"放大**(Medium,mod.rs:4615、6024):diff 对"目录下增删子项"输出 `M <dir>`,子项本身已有 +/- 行,对 M 目录做整子树递归展开是重复劳动(10 万子孙目录加 1 个文件,两侧各多走 10 万条目,verify 再走第三遍);union 内嵌套路径无祖先去重。只有 R(rename)和部分 + 目录真需递归。修好后快路径成本才真正正比于变化量。

**三、内存峰值**
- (Medium)`fast_missing_dirs` 用 `map_entries` 对两棵全树快照做 owned 深拷贝(mod.rs:5322、5344),原 Vec 不释放——双树四份数百 MB 峰值;`diff_manifests` 早已为此改借用版 `map_entry_refs`,这两处漏改,修复无阻碍。
- (Medium)跨机整树快照是单块明文 JSON(web_api.rs:436;machines.rs:661):~90 万条目 100-180MB 双端 raw+parsed 各两份瞬时驻留,无压缩无上限;`file_type` 用 String 再放大。改 NDJSON 流式 + 枚举(需版本协商);zfs-diff 快路径普及后触发频率已降,机制未修。

**四、散点(Low)**:`sorted_read_dir` 排序每次比较分配 2 个 String(改 OsStr 直接比较,一行);`build_delta` 每字节一次 SipHash 查找(换 FxHash/tag 预筛,提速数倍);`verify_copied_entries` 单线程逐条 2 次 lstat(并行化+去冗余 stat);扫描进度每条目一次全局锁+String 分配(节流前移);**并行度策略不一致**——wholesale 路径按介质门控(HDD 串行)但同函数主 to_copy 批次无条件 16 并发(mod.rs:5376),跨机 push 到 HDD dst 同样;HDD-thrash 论据只应用了一半,建议把 `path_is_rotational` 上提为统一的 effective 并行度输入,先实测再定默认。

**五、代码质量**
- `sync/mod.rs` 已 8660 行(含 ~1800 行测试),天然接缝清晰:transfer_wire / walk / zfs / engine(两个 cycle 驱动)/ local_copy / pool 六块可拆。
- 重复模式:两个 cycle 引擎(~540 行与 ~490 行)在状态转移、zfs diff 分支、baseline 记录上镜像重复;`push_entries_parallel` 与 `copy_entries_parallel` 90% 相同;本地/跨机 event_paths 平行。
- 测试盲区:`scheduler.rs` 0 测试(daily/weekly 边界、DST 跳变,设计文档 §15 明确列为单测项);`machines.rs` 手写 HTTP 解析(`read_http_response`/`read_chunked_body`)无测试——它是所有跨机功能的地基;worker pool 并发语义(fatal 竞态、20 次上限)无专门测试。

---

## 9. 修复优先级建议

**P0(正确性,建议尽快)**
1. R1:walk 版批发拷贝的 symlink 判断改 lstat(两处,mod.rs:5544/5351)+ 集成测试。
2. R2+E4:zfs diff 并集过滤内部目录(trash/tmp/probe);`move_to_trash` 的 rename 失败只对安全错误回退,EXDEV 改 copy+delete,不再静默降级永久删除。
3. R3:事件/diff 集合内的路径不用 modify-window 宽容比较(或收紧窗口到 100ns 级)。
4. R10+盲区1:zfs-diff 的 dst-only M 路径强制 hash;entries_match(至少 Full/Compare)比较 mode,fanotify 加 FAN_ATTRIB。

**P1(不变量可见性与可用性)**
5. R6/E1:引擎 pass 的 per-source 错误隔离(捕获后 continue,不再饿死下游 source)。
6. R4/W1:fanotify 分级回退 + 降级持久可见;R5/P1:watcher 重启路径升 notice/置 rescan + config_signature 只哈希 watcher 相关字段。
7. W4:overflow 记录失败的 sticky 补写;W5/perf-4:事件批量入事务。
8. S2:基线刷新窗口(先快照后校验,或刷新后 diff 断言为空)。
9. R12:trash 保留窗口清理 + tmp 非当前 cycle 清扫(E5)+ UI 暴露大小。

**P2(退化与体验)**
10. E2(反斜杠/非 UTF-8 文件名)、E3(快照世代校验)、E6(批发拷贝失败容忍对齐 20 个契约)。
11. S3(混合 cycle 本地 dst 基线)、S4(前缀边界)、P2-P8(配置互斥、GET 写库、sync_now 容错、UDP 发现确认、save_config 半应用、主机名支持、Tauri async)。
12. perf 一~三(串行双扫、3×lstat、prune 空转、事件 fsync、M 目录展开、深拷贝、快照流式)。

**P3(边角)**:L4-L9、S5-S8、W6-W9、P9-P12、E7-E9、perf 散点、mod.rs 拆分与测试补盲。
