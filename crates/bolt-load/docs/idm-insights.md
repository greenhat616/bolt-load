> **注**：本文是第一轮的简版笔记。完整剖析（含对象模型、fan-out 控制器、冗余对冲、状态机全流程与证据索引）见
> [`idm-strategy-full.md`](./idm-strategy-full.md)，两者结论一致，冲突时以完整版为准。
>
> **b16 复核勘误（2026-10-31）**：
> - 本文的 IDM 结论已经在 v6.43b16 上用 dsh 和 IDA 复核过，被更正的地方在原位用 **「b16 复核」** 标出。
> - bolt-load 一侧的文件和行号引用的是旧结构（`concurrent_task/mod.rs`），当前代码的核验结果以 [`idm-improvement-plan.md`](./idm-improvement-plan.md) §1、§2 为准。
> - §5 的建议已经由该方案的 P0–P3 取代。

# 从 IDM 6.43b15 反推：切分策略与错误处理对 bolt-load 的启示

> 目标：以本机安装的 Internet Download Manager（IDM）为参照物，提炼它在「分片切分」和「出错恢复」上的工程取舍，对照 bolt-load 现状给出可落地的改造清单。
>
> 证据分三类，全文严格区分：
> - **观测**：来自 IDMan.exe 的反编译/字符串清单、注册表配置、磁盘上的下载状态文件；
> - **推断**：由观测到的常量与控制流推出的语义（未逐条动态验证）；
> - **未知**：静态证据无法确认的部分，单列在文末。

---

## 0. 证据与环境

| 项 | 值 |
| --- | --- |
| 被分析产物 | `C:\Program Files (x86)\Internet Download Manager\IDMan.exe` |
| SHA-256 | `279ceed9aaa646650716ee326c0d996be4ce5db4b252cf5b2747f4938a7ce737` |
| 架构 / 格式 | x86 PE（IDA/Ghidra 视角的 32 位 PE，6.2 MB） |
| 版本 | `idmvers = v6.43b15 Full`（`HKCU\Software\DownloadManager`） |
| 关键配置 | `MaxConnectionsNumber = 8`，另存在按服务器维度的连接上限（UI 字符串 `Max. connections number for a server`） |
| 状态文件 | `%APPDATA%\IDM\DwnlData\<user>\<name>_<id>\` 下**每个连接一个临时文件**（如 `QQ9.7.17.29225.exe`、`…exe1` … `…exe18`，共 19 个片段） |
| 每下载元数据 | `HKCU\Software\DownloadManager\<id>`：`FileSize`、`Downloaded`、`Status`、`LastModified`、`FRFileSize`、`lastResult` 等 |
| 反编译证据 | `ChunksList` 插入路径（0x560440，含 `Insert next chunk in list` 与 `Change chunk size to %ld`）、HTTP 连接状态机（0x58fdf0，含 `Connection restarted because of timeout (1/2/3)`） |

对照的 bolt-load 代码（当前工作区）：

- `crates/bolt-load/src/task/instance/concurrent_task/chunk_planner.rs`
- `crates/bolt-load/src/task/instance/concurrent_task/strategy/{dynamic_strategy,concurrency_control_strategy,failure_policy}.rs`
- `crates/bolt-load/src/task/instance/concurrent_task/mod.rs`
- `crates/bolt-load/src/task/options.rs`
- `crates/bolt-load-adapter/src/reqwest.rs`

---

## 1. 结论摘要

| 维度 | IDM（观测 + 推断） | bolt-load 现状 | 建议动作 |
| --- | --- | --- | --- |
| 初始分片 | 连接数上限来自配置（默认 8），再按「服务器维度」二次限制；分片边界按固定粒度对齐 | 并发上限 = `available_parallelism()`（CPU 核数），无配置项 | 引入 `max_connections` 与 `max_per_host_connections`，不要用 CPU 核数当连接数 |
| 切分触发 | 按**实测单连接吞吐**推算目标分片大小（速度 × 时间窗）；~~并做加性/乘性增长~~（b16 复核：增长式只作用在局部切分候选值上，Q 固定为 11680） | 以 1 MiB/s 绝对阈值 + 全局速度差判断，`±1 MiB/s` 的差值在慢链路上几乎不触发；`abs()` 使**降速**也会触发 `SplitAllTask` | 用「每连接增益」判定；改为相对判据；区分升/降速 |
| 分片大小 | 下限用固定常量（观测到 11680 / 23360 / 46720 这一族常量，与 `1460×8/16/32` 完全吻合，疑似按 MSS 倍数取整），并按「包大小」对齐分片边界 | `DEFAULT_MIN_CHUNK_SIZE = 1 MiB`，切分点 = 剩余区间一半，无对齐 | 保留 1 MiB 下限，增加「切分点对齐到 64 KiB / 4 KiB 文件块」避免碎片 |
| 超限保护 | 单次只插入一个新分片，且插入前校验剩余量是否够「三倍包大小」；有 `Need optional connections` / `Optional connections not needed` 的显式闸门 | `SplitAllTask` 会**对每个未完成 runner 同时切分**，不检查 `active < max_concurrency`，一个 tick 可能把连接数翻倍并越过上限 | 切分动作必须与并发上限原子校验（复核：当前闸门下不会越界，风险在于 `split()` 不是原子操作） |
| 尾部分片 | `ChunksListItem::JoinWithNext`：~~剩余量不足以再切时把相邻小块合并~~ 合并的是**已完成**的项 | 只切不合，但尾片仍有连接在下 | ~~增加 merge 动作~~ 不做 |
| 分片存储 | 每**分片**一个临时文件（分片数可以多于连接数），结束时 `JoinFile` 合并 | 单一预分配文件 + 区间写（mmap/pool writer） | 保持现状（更好），但补「分片级提交位点」 |
| 断点恢复 | 恢复前 `Rebuilding old downloaded files before resume`，并把每个分片起始若干字节与文件头比对（`Strart data for this chunk is matched with start of the file!!!`、`Some chunks matched with start of file! Deleting all, except first.`） | `Progress.downloaded_chunks` 仅记录区间，重下时不校验服务器侧是否变化 | 记录 `ETag/Last-Modified`，恢复时用 `If-Range` 并校验分片首字节 |
| 连接重试 | 按连接状态机在**空闲 30/300 个时间单位**（b16 复核：单位是秒，另一个状态机是 120/300 s；重启**没有延时**）时重启（`Connection restarted because of timeout (1|2|3)`），重启前先把在途字节累加进分片位点（`pos += pending; pending = 0;`）再断开 | 每个 runner 有 30 s 空闲超时；但超时即视为一次失败，失败预算按「全局连续失败」计数 | 重试前提交位点（已满足）+ 给失败引入**每分片预算**与**抖动退避** |
| 拥堵处理 | ~~解析 `Retry-After`~~（b16 复核：头名只出现在静态头表里，**没有代码读取它的值**） | 429/503 会被「容忍」但**不解析 Retry-After**，立刻在下一个 tick 重新建连 | 解析 `Retry-After`，按 deadline 冻结该分片的调度 |
| 4xx 语义 | 区分「不支持续传」等，走专门分支（b16 复核：`Try rollback, set chunksize to %I64d` 属于续传前的分片文件重建，与 4xx 无关） | 除 404/401/403/429/503 外，**一切 4xx 都归类为 Retryable Io**，416/410 会被无限重试 | 新增 `RangeNotSatisfiable`/`RangeIgnored`/`ResourceChanged` 等不可重试类别 |
| 服务器忽略 Range | 支持 `multipart/byteranges` 与单连接回退（`IDM started to download the file using one connection because of data corruption…`） | 只检查 `Content-Range` 或 `Accept-Ranges` 探测；`range_stream` 拿到 200 全量响应时**照写不误**，会把整个文件写到 `range.start` | adapter 层校验 206 的 `Content-Range` 起点，200 视为「range 不可用」而不是成功 |
| 磁盘约束 | `Disk is full. Cannot add next chunks.`、`Not enough disk space to store files chunks, canceling download.`（新增分片前检查） | 预分配文件时可能已失败，但切分/新建 runner 前无剩余空间检查 | 在 `spawn_runner`/`split` 前检查可用空间，`ENOSPC` 归类为终止错误 |

---

## 2. IDM 的切分策略（观测与推断）

### 2.1 分片是「链表 + 每片一个文件」

`ChunksList::Insert`（0x560440）里的可见行为：

1. 为每个新分片分配一个序号：`*(in_ECX + 0x94) += 1; item[0x18] = *(in_ECX + 0x94);`；
2. 用 `%s%ld` 拼出分片文件名并 `CreateFile`（`local_164[0x16]` 串成链表，`param_1[0x2a]` 指回当前片）；
3. 每片记录 `start/end`（`item[2]/item[3]`）、归属连接号、以及 `alt/altN`（备用地址与备用计数）。

这与磁盘上的 `DwnlData` 目录完全对应：一个下载目录里是「基名 + 数字后缀」的一串临时文件。**推断**：IDM 用「分片文件」换取了写路径的简单性和崩溃后的可诊断性；代价是完成时要 `JoinFile` 合并一次（多一次全量 I/O）。

bolt-load 走的是相反路线：单文件 + 区间写（`mmap_writer`/`pool_writer`/`compio_writer`），没有合并开销，随机写由 OS 页缓存吸收。**这一点 bolt-load 更优，不建议照搬**。

### 2.2 分片大小是「速度 × 时间」而不是「文件大小 ÷ 连接数」

插入逻辑里能看到两类量：

- 目标分片尺寸参与 `__alldiv(size, 2)`：新片拿目标量的一半；
- 还有一族硬编码常量：`0x2da0 (11680)`、`0x5b40 (23360)`、`0xb680 (46720)`、`0xb681/0x5b43`，以及 `size*5 < 0x2da0 ? size += 0x2da0 : size = size*2 + 0x5b43` 这样的**加性/乘性增长**。

`11680 = 1460 × 8`，`23360 = 1460 × 16`，`46720 = 1460 × 32`。**推断**：这些是「MSS 的倍数」，用于给分片尺寸/边界一个与网络包对齐的最小粒度；增长公式则是「小步加性、大值乘性」的自适应放大（类似 TCP 拥塞窗口的 AIMD 直觉）。另有按 `param_3`（包大小）对齐边界的取余运算：`*puVar13 = (param_3 - rem) + *puVar13`。

> 精确语义（单位、触发条件）受反编译精度限制，标为**推断**；但「分片尺寸由吞吐量推导 + 对齐到固定粒度」这一模式是清晰的。

> **b16 复核（IDA）**：“AIMD 式自适应放大”不成立。量子 Q 的初值固定为 11680。增长式作用在 `Insert` 里的一个局部切分候选值上，这个值由**被切连接自己的速率**推出（`[conn+0x3D0]` × −1000.0），取上限后减半。所以“按吞吐推导”成立，但依据是单个连接的速率，不是任务总速率。

### 2.3 切分有闸门，不会一次翻倍

插入路径上的前置条件包括：

- 当前剩余量必须 ≥ `0x5b43`（≈23 KB）才允许插入；
- 若传入 `param_3`（包大小），要求剩余 ≥ `param_3 * 3`；
- 有 `Need optional connections.` / `Optional connections not needed.` / `Optional connections have been created.` 三个状态的显式文案，说明「是否加连接」是独立决策，而不是无条件加。

对照 `SplitAllTask` 的实现（`concurrent_task/mod.rs`），它对 `get_incomplete_states()` 返回的**每一个** runner 都调用 `split()`，而 `split()` 内部没有 `active < max_concurrency` 的断言：

```rust
StrategyAction::SplitAllTask(planned_chunk_size) => {
    let states = self.manager.get_incomplete_states(Some(planned_chunk_size));
    for (runner_id, incomplete_range) in states {
        self.split(runner_id, incomplete_range);   // 每个都切 → 连接数瞬时翻倍
    }
}
```

这是一个真实的超限风险：`max_concurrency` 只在 `StrategyControl::execute` 的分支判定里用过一次，动作落地时不再校验。

> **复核（当前 `dev`）**：QuickStart 有 `2*current < max` 闸门，所以不会一次翻倍越界。真正的风险有两处：`split()` 先 resize 再 spawn，不是原子操作；`spawn_runner` 在槽位耗尽时 `expect` panic（改进方案 C2、P1-2）。

### 2.4 尾部分片会合并

字符串 `Unknown error during ChunksListItem::JoinWithNext().` 说明存在「与下一个分片合并」的操作。bolt-load 只有 `split_chunk` / `resize_runner_state`（只缩不扩），没有 merge，因此当剩余量 < `2 * min_chunk_size` 时该分片既不能再切、又占着一个 runner 槽位，直到结束。

> **复核**：`JoinWithNext` 合并的是**已经完成**的相邻链表项，作用是减少碎片，不是回收连接。bolt-load 的尾片仍然有连接在下载，并没有白占槽位。尾部的真正问题是慢连接拖尾，所以不做 merge（改进方案 §2.3）。

---

## 3. IDM 的错误处理（观测与推断）

### 3.1 连接状态机：先提交位点，再断开重连

0x58fdf0 是一个手写的非阻塞状态机（用 `WSAFDIsSet` 轮询 fd_set，而不是「一线程一连接」）。超时分支的关键三行在所有 `timeout (1|2|3)` 路径上重复出现：

```c
*puVar3      = *puVar3 + in_ECX[0x20];          // pos += pending
in_ECX[0x19] = in_ECX[0x19] + in_ECX[0x21] + carry;
in_ECX[0x20] = 0; in_ECX[0x21] = 0;             // pending = 0
local_9c = (**(code **)(*in_ECX + 0x18))(pSVar2); // 然后才 close/restart
```

即：**在途字节先记账成分片已下载量，再断开**。这保证重连的 Range 起点永远不会「回退」，也不会重复写同一段。

阈值上有 30 与 300 两个量级（`0x1e`、`0x12c`），另有 `No data, restarting y_sc` 基于「实测速率 vs 期望速率」的判定。**推断**：30/300 是「空闲多久重启」的两级阈值（配合 100 ms tick，对应 ~3 s / ~30 s），并且重启本身不是失败，不消耗「放弃预算」。

> **b16 复核（IDA）**：
> - “100 ms tick、约 3 s / 30 s”不成立。计时用 `_ftime32` 的秒差，所以阈值的单位就是**秒**。
> - 有两个状态机：一个的 body 空闲阈值是 30 s，另一个是 120 s，`response_state == 2` 时都是 300 s。
> - 重启路径上**没有任何延时**。

bolt-load 的对应实现：`runner` 有 `DEFAULT_IDLE_TIMEOUT = 30s`（`runner/mod.rs:33`），超时后 `StoppedReason::Failed(TaskError::Timeout)` → `AttemptFailure { retryable: true }` → `FailurePolicy::on_failure` 累加**全局连续失败**。也就是说 IDM 眼里的「正常重连」在 bolt-load 里是「一次失败」，会被计入 `max_consecutive_failures`。

### 3.2 失败分级：不止 retryable/unretryable

字符串清单里能看到的错误分支粒度：

- `The data transfer has been interrupted and the server does not support "resume".` → 明确「不能续传」，只能整文件重下；
- ~~`Try rollback, set chunksize to %I64d` → 416/短读这类问题回滚分片尺寸；~~ b16 复核：这条出现在续传前的分片文件重建里，作用是把超长的分片临时文件截到区间长度，和 416/短读无关；
- `The file chunk %d is reassigned to connection %d.` / `The file chunk of connection %d is reassigned to connection %d.` → 分片**改派**给其它连接，而不是让整任务失败；
- `ChunksList::AddAlternative` / `ApplyAlternative` / `CheckAlternative`（`Add alternative connection (%ld) for connection %ld`）→ 同一分片的**备用地址**（多 IP / 多 URL）故障转移；
- `IDM started to download the file using one connection because of data corruption when newly received data in several connections did not match the same data downloaded earlier.` → 多连接数据互相矛盾时，**降级为单连接**并重来；
- `Some chunks matched with start of file! Deleting all, except first.` / `Strart data for this chunk is matched with start of the file!!!` → 恢复时用「分片首字节 == 文件首字节」识别偏移错乱。

bolt-load 的失败模型是二维的（`retryable: bool` + `TaskError` 变体），并且：

- `FailurePolicy` 的计数是**全局连续**的（`failure_policy.rs:70-80`），没有「这个分片已经失败 N 次」的概念；
- 429/503 在「还有其它在途连接」时被**无条件容忍**（`is_congestion` + `tolerated`），但没有任何冷却/退避，靠 `ConcurrencyControlStrategy` 的 5 样本窗口事后降并发；
- 416（Range Not Satisfiable）、410、409 等都会落到 `reqwest.rs:108-114` 的 `_ => RetryableError::Io`，被无限重试，直到烧完 `max_consecutive_failures` 才以 `RetryLimitExceeded` 收场——错误信息还会指向「重试次数用完」，掩盖真实原因。

### 3.3 Range 语义校验是缺的（最高优先级）

`reqwest.rs` 的 `range_stream` 只做了两件事：发 `Range: bytes=start-(end-1)`、`error_for_status()`。它**没有**：

1. 校验响应是 `206`；
2. 校验 `Content-Range` 的起点等于请求的 `start`；
3. 校验 `Content-Range` 的总长与 meta 一致；
4. 发 `If-Range`（IDM 有 `If-Range:`、`if-range` 字符串，并持久化 `LastModified`、`FRFileSize`）。

后果：任何「忽略 Range 直接 200 全量返回」的服务器，`error_for_status()` 也通过，runner 会把**整个文件**当作区间 `start..end` 的数据写进去。`TaskRunner` 有 `total` 上限与 `ExceededTotalSize` 检查，但那是在写满 `end - start` 之后才报错——此时文件已经被污染；更糟的是如果 `end - start` 恰好大于文件长度，检查不会触发。

同时 `TaskRunner` 对「短读」有检查（`SmallerThanTotalSize`），但对「错位读」没有。

### 3.4 磁盘与容量错误

`Disk is full. Cannot add next chunks.`、`Not enough disk space to rebuild downloaded chunks.`、`Not enough disk space to store files chunks, canceling download.` —— 三条文案都出现在「新增/重建分片」的路径上，说明 IDM 把「加连接之前先看盘」当作前置检查。

bolt-load 的 `FileWriter::new` 会预分配，但 `spawn_runner` / `split` 没有再评估剩余空间；写失败会被 `check_completions` 包成 `TaskError::Other`（`concurrent_task/mod.rs:454-469`），既不可重试也不可诊断。

---

## 4. bolt-load 侧的具体问题（按文件定位）

### 4.1 切分判据（`strategy/dynamic_strategy.rs`）

```rust
if (total_download_speed - 2.0 * self.previous_total_download_speed).abs() > self.threashold1
    && context.current_concurrency.saturating_mul(2) < context.max_concurrency
```

- `abs()` 让「速度掉了一半」也满足条件，于是**降速时切分**（与意图相反）；
- `previous_total_download_speed` 只在真的切分时更新，正常阶段每次都被覆盖，等于「与上一 tick 比」；
- `threashold2 = 1 MiB/s` 是绝对值：50 KB/s 的链路永远不会触发 `SplitGivenTask`；
- `current_concurrency * 2 < max_concurrency` 这个闸门要求「立刻翻倍后仍不越界」，在 `max_concurrency = 4` 时只允许在 0/1 个连接时进入 QuickStart，一旦到 2 就永久落到 Normal（而 Normal 每次只切一个，于是切分节奏被腰斩）。

### 4.2 并发上限来源（`concurrent_task/mod.rs:495`）

```rust
let max_concurrency = std::thread::available_parallelism().map(|t| t.get()).unwrap_or(4);
```

连接数与 CPU 核数绑定：8 核机器 = 8 连接，且用户无法调。IDM 是「全局上限 + 按服务器上限」两级配置（`MaxConnectionsNumber = 8` + `Max. connections number for a server`）。

### 4.3 切分动作越界（`concurrent_task/mod.rs:690`）

见 §2.3。另外 `StrategyControl::execute` 里：

```rust
let runner_id = runner_id.expect("A empty chunk is not processed by concurrency control");
```

`find_chunk_to_split` 在存在空闲区间时会返回 `(None, range)`（`chunk_planner.rs:449-451`）。当前靠「并发控制策略同一 tick 会先返回 CreateTask」来规避，但这是**跨模块的隐式不变量**；一旦并发控制因 `has_recent_unretryable` 提前 return（`concurrency_control_strategy.rs:296-304`），就会 panic。

### 4.4 失败预算（`strategy/failure_policy.rs`）

- 全局 `consecutive`，没有 per-chunk / per-host 维度；
- `on_write_ok(written)` 只要有一次非空写就清零——一个健康连接可以持续「洗白」一个持续失败的分片；
- `no_progress_timeout = 60s` 是任务级，`stream_idle_timeout = 30s` 是 runner 级，两者与「重连退避」没有联动：失败后下一个 `strategy_tick`（1.5 s）就可能重建连接，等于**零退避**地打服务器。

### 4.5 Range 与元数据（`bolt-load-adapter/src/reqwest.rs`）

- `fetch_remote_meta` 用 `bytes=0-0` 探测，只看 `Content-Range`/`Accept-Ranges`，不保存 `ETag`/`Last-Modified`；
- `range_stream` 不校验状态码与 `Content-Range`；
- 错误映射把「未知 4xx」当可重试；
- 没有 `Retry-After` 读取。

---

## 5. 建议（按优先级）

> **已被 [`idm-improvement-plan.md`](./idm-improvement-plan.md) 取代**。本节保留作为历史记录，其中几条经复核不成立：
> - 第 3 条：409/410/412 不应一律归为 `ResourceChanged`；416 也不应“回滚分片大小”。
> - 第 6 条：“每连接增益”公式恒等于 0，因为 `per_runner_speed = speed / runners`。
> - 第 9 条：merge 的依据不成立。
> - 第 11 条：IDM 并不解析 `Retry-After`。
> - 第 12 条：切分前检查剩余空间有竞态，改为在预分配阶段解决。
>
> 第 1 条的示例代码还有一个问题：它放在 `error_for_status()` 之后，这时 416 已经被吞掉了。

### P0 —— 正确性，先补上会写坏文件的洞

1. **校验 `206` 与 `Content-Range` 起点**（adapter 层，`range_stream`）：
   ```rust
   let resp = …;                       // 已 error_for_status
   match resp.status() {
       StatusCode::PARTIAL_CONTENT => {
           let (s, e, total) = parse_content_range(&resp)?;     // "bytes s-e/total"
           if s != start || e + 1 > end || total != expected_total {
               return Err(AdapterError::Unretryable {
                   source: UnretryableError::RangeMismatch { requested: start..end, got: s..e },
               });
           }
       }
       StatusCode::OK => return Err(AdapterError::Unretryable {
           source: UnretryableError::RangeIgnored,          // 服务器忽略 Range
       }),
       StatusCode::RANGE_NOT_SATISFIABLE => return Err(AdapterError::Unretryable {
           source: UnretryableError::RangeNotSatisfiable,
       }),
       _ => {}
   }
   ```
   并在 runner 侧把 `RangeIgnored` 转成「整任务降级为单连接」的显式信号（对应 IDM 的单连接回退）。
2. **加 `If-Range`**：`BoltLoadAdapterMeta` 增加 `etag: Option<String>`、`last_modified: Option<String>`，`range_stream` 一并发送；服务端返回 `200` 即视为「资源已变」，走整文件重下而不是写偏移。
3. **拆分 4xx 语义**：`416` → `RangeNotSatisfiable`（可回滚分片大小）；`409/410/412` → `ResourceChanged`（终止或重下）；其余 4xx → 保留 `Retryable` 但**限制重试次数为 1**，避免无限循环。
4. **恢复时校验偏移**：`Progress.downloaded_chunks` 落盘时附带 `etag/last_modified/total`；恢复时若不一致，丢弃区间重下（对应 IDM 的 `Some chunks matched with start of file` 检查思路）。

### P1 —— 切分与并发策略

5. **连接数从配置来**：`TaskOptions { max_connections: usize, max_connections_per_host: usize }`，默认 `min(8, …)`（与 IDM 默认一致），`available_parallelism` 只作为「默认值」的一个参考项而不是硬绑定。
6. **切分判据改为「每连接增益」**：
   ```rust
   // 只有「新增连接能带来可观增量」时才切
   let marginal = context.speed - context.current_concurrency as f64 * context.per_runner_speed;
   let worth   = marginal > context.per_runner_speed * 0.35;      // <1 个连接的 35% 就不切
   let room    = context.current_concurrency + 1 <= context.max_concurrency;
   ```
   并去掉 `abs()`：只在**升速**时切；降速交给并发控制策略降级。
7. **切分动作原子化**：`split()` 内部断言 `active_runners + 1 <= max_concurrency`；`SplitAllTask` 最多切到上限的一半，或改为「每次 tick 只切一个」。
8. **目标分片大小 = 速度 × 时间窗**（IDM 的思路）：`planned = clamp(speed * 2.0, min_chunk_size, total / 4)`，且**对齐到 64 KiB**：
   ```rust
   let align = 64 * 1024;
   let split_pos = ((split_pos + align - 1) / align) * align;
   ```
9. **增加 merge**：`get_incomplete_states` 之外提供 `find_merge_candidate()`，当相邻两片剩余量之和 < 2×min 时合并，避免尾部分片白占一个连接。
10. **每分片失败预算**：`FailurePolicy` 增加 `HashMap<Range<u64>, u8>`（或 `RunnerId → attempts`），`attempts >= 3` 时该分片标记 `Stalled`，只允许在「并发降级 + 退避后」重试，而不是继续占用全局预算。
11. **退避与 `Retry-After`**：
    - 429/503：解析 `Retry-After`（秒数或 HTTP-date），写入该分片的 `not_before`，调度时跳过；
    - 连接失败：`delay = min(1s * 2^attempt, 30s) * jitter`，与 IDM「队列每 30 秒重试」的量级对齐；
    - 空闲超时重连不计入失败预算（IDM 的语义），bolt-load 应把 `TaskError::Timeout` 与真正的 IO 失败分开计数。

### P2 —— 可观测性与工程细节

12. **磁盘前置检查**：`split`/`spawn_runner` 前检查剩余空间（`total - written` 的 1.05 倍），失败即 `TaskError::DiskFull`（终止、不可重试），对应 `Disk is full. Cannot add next chunks.`。
13. **区分「在途/已提交」位点**：`ChunkState.downloaded` 目前由 `update_progress` 在数据帧到达时推进；建议增加 `committed`（写盘确认）与 `received`（帧到达）两个位点，重连/恢复时以 `committed` 为准——这正是 IDM 那三行 `pos += pending; pending = 0` 的等价物。
14. **`find_chunk_to_split` 的返回类型**：把 `Option<(Option<RunnerId>, Range<u64>)>` 拆成枚举（`Available(range)` / `Split(runner, range)`），消除 `expect` 的隐式不变量。
15. **状态落盘**：现在只有 `Progress`，没有分片级 journal。若要做崩溃恢复，建议把 `(range, committed, etag)` 以行式追加写到 `<file>.boltload.state`，而不是学 IDM 往注册表写 16 字节记录（注册表方案在崩溃一致性和可移植性上都更差）。

---

## 6. 不要照搬的部分

- **每连接一个临时文件 + 完成时合并**：bolt-load 的单文件区间写更省 I/O，也天然支持稀疏写；只要补上 §5.1/5.4 的校验，就没有 IDM 的合并阶段风险。
- **注册表作为分片 journal**：原子性、可移植性、可审计性都差。
- **手写 WSA 状态机**：Rust 的 async 已经把这块抽象掉了；IDM 这么做是时代原因。
- **具体常量**（11680 / 23360 / 46720、30/300 的时间单位）：这些是 IDM 在其网络模型下的经验值，直接抄没有意义；要抄的是「按吞吐推导 + 对齐粒度 + 分级超时」这三条结构。

---

## 7. 未确认（静态证据不足）

> **b16 复核后的状态**：
> - 第 2 条：初值 11680 已确认，但“MSS 倍数”的业务口径仍是推断。
> - 第 3 条：已解决。单位是秒，两个状态机分别是 30/300 s 和 120/300 s。
> - 第 5 条：部分解决。按站点的连接数存放在 `MCN\<scheme>://host[path]\number`，每个下载各自读取。
> - 第 6 条：`ApplyAlternative` 的选择规则只有 dsh 一个来源；改派的调用链仍未追完。

1. IDM 的**初始**分片数量公式（按文件大小阈值还是直接按连接数均分）。观测只到「插入单片时用剩余量的 1/2、且受 23 KB/3×包大小闸门约束」。
2. `0x2da0/0x5b40/0xb680` 的业务含义（MSS 倍数是强推断，但常量也可能来自其它口径）。
3. 30 / 300 两个超时值的单位（ms / 100ms / s）。行为上「分级、且重启不计失败」是明确的，绝对值不确定。
4. `MaxConnectionsNumber` 的运行期上限（注册表里是 8，UI 上通常允许更高，但未在二进制中定位到 clamp 边界）。
5. IDM 与服务器「按 host 的连接上限」的具体计数维度（host、IP、还是 URL 前缀）。
6. 反编译的 `FUN_0058fdf0` 是 HTTP 连接状态机的一部分；分片改派与 `AddAlternative` 的调用链未逐条追完，仅由字符串与函数名佐证。
