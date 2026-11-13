# 下载策略加固 设计

**日期：** 2026-11-01
**基线：** `dev @ 5b76dae`
**来源：**
- [`idm-improvement-plan.md`](../../../crates/bolt-load/docs/idm-improvement-plan.md)：核验、方案与决策。
- IDM 逆向分析：[`idm-strategy-full.md`](../../../crates/bolt-load/docs/idm-strategy-full.md)、[`idm-insights.md`](../../../crates/bolt-load/docs/idm-insights.md)、[`idm-multitask.md`](../../../crates/bolt-load/docs/idm-multitask.md)。均已标注 b16 复核结论。

**范围：**
- `bolt-load-adapter`：Range 响应校验、状态码分类。
- `bolt-load-core`：错误类型、meta 字段。
- `bolt-load`：
  - 下载循环：连接上限、切分、退避、失败预算、扩张控制器。
  - runner：限速等待。
  - 任务：Range 不支持时回退到单连接、写错误、续传。
- `bolt-load-tests`：故障注入服务器。
- `bolt-load-http`（新增）：adapter 用的 HTTP 校验与解析，从 core 和 utils 搬来（§4.5）。
- `bolt-load-decode`（新增）：单连接下载的内容解码（§4.4）。

**不在范围：** 见 §13。
**权威顺序：** 本设计 > `tasks.md` > `idm-improvement-plan.md`。改进方案只作为背景和证据，两者冲突时以本设计为准。

---

## 0. 结论摘要

1. **数据正确性优先于速度。** 两个 adapter 的 `range_stream` 都不校验 `206` 和 `Content-Range`。服务器忽略 Range、返回 `200` 时，runner 会把文件开头那一段截短后写到非零偏移，长度校验照样通过，结果是一个静默损坏的文件。所以 adapter 在返回流之前，必须证明字节偏移是对的，否则拒收。
2. **状态码分类收拢成一个共享函数。** 两个 adapter 对“其它 4xx”的处理相反，reqwest 的 503 分支也走不到。改成共享的 `classify_status`，同时给公开的错误类型加上 `#[non_exhaustive]`。
3. **重连不算失败，但必须退避。** IDM 重连没有任何延时，它靠调度器层每 30 s 一次的重试兜底，bolt-load 没有这一层。退避状态挂在可用区间上；失败预算拆成三层：重启、每个区间的预算、任务级连续失败。
4. **连接上限和 CPU 核数脱钩**，公开为 `max_connections`，默认 8。切分要先占 runner 槽位，再改区间，最后 spawn；任何一步失败都整体回滚，不再 panic。
5. **扩张控制器和切分尺寸一起替换，放在开关后面做 A/B。** 新做法：2 s 冷却；全部 runner 都活跃才扩张；扩张后增益不到 10% 就停止扩张。切分尺寸改用单连接速率估算，不再用任务总速率。基准测试表现更好才切换为默认。
6. **跨任务限额（P2）和续传（P3）在同一份设计里定好边界**，按需实施。限速用跨任务共享的令牌桶，等待放在 emit 的背压路径上。续传前先做一致性检查，越界或重叠的区间直接丢弃。

---

## 1. 现状事实（已核对源码）

本节是写这份设计时（2026-11-01）的代码，留作记录。其中 `dynamic_strategy.rs`、`SplitAllTask` 和任务速度的切分尺寸已在 C14 删除，见 §14.20。

### 1.1 adapter 与 core

| 事实 | 位置 |
| --- | --- |
| reqwest 的错误映射只在 `is_client_error()` 内部做 `match`。429 和 503 合并成一个分支，但 503 不是 4xx，这个分支对它**走不到**。其它 4xx 归为 `RetryableError::Io` | `bolt-load-adapter/src/reqwest.rs:79-115` |
| ureq2 把 `status < 500` 映射为 `UnretryableIo`，和 reqwest 相反 | `bolt-load-adapter/src/ureq2.rs:79` |
| 两个 `range_stream` 都只调用 `error_for_status()`，不校验 `206` 和 `Content-Range` | `reqwest.rs:301-315`；`ureq2.rs:198` 起 |
| `fetch_remote_meta` 在 `ContentSize::ContentLength(0)` 时把 Range 判为可用 | `reqwest.rs:245-246` |
| `UnretryableError`、`RetryableError`、`BoltLoadAdapterMeta` 都没有 `#[non_exhaustive]`；`RetryableError` 只有 `Io` 一个变体 | `bolt-load-core/src/adapter/error.rs:39,88`；`adapter.rs:45` |
| `BoltLoadAdapterMeta` 只有 `content_size` 和 `filename` 两个字段 | `bolt-load-core/src/adapter.rs:45-50` |

### 1.2 runner 与下载循环

| 事实 | 位置 |
| --- | --- |
| runner 用 `allowance` 截断写入量，`validate_size` 只比较长度。偏移写错了发现不了 | `bolt-load/src/runner/run_loop/stream.rs:60,68,140-154` |
| 背压期间不推进空闲计时器：emit 等待下游时不轮询流，背压结束后重置计时器 | `runner/run_loop/emit.rs:25`；`stream.rs:172` |
| 连接上限取 `available_parallelism()`。`TaskOptions::max_concurrency` 带 `#[cfg(test)] pub(crate)`，只有测试能改 | `task/instance/concurrent_task/download_loop.rs:77-83`；`task/options.rs:111-113` |
| `max_consecutive_failures` 的默认值是 `max(8, 2 × available_parallelism)` | `task/options.rs:7-16` |
| `IdGenerator` 满了返回 `None`，`spawn_runner` 随后 `expect` panic | `task/instance/id.rs:43-46`；`download_loop/strategy_tick.rs:93-99` |
| `split()` 先调整区间再 spawn，两步不是原子的 | `download_loop/strategy_tick.rs:84-99` |
| `SplitAllTask` 逐个调用 `split()`，不再检查上限。QuickStart 的入口闸门是 `current * 2 < max` | `strategy_tick.rs:48`；`strategy/dynamic_strategy.rs:75-77` |
| 切分判据用了 `abs()`，降速也会触发切分；两个阈值都是固定的 1 MiB/s，而且都用严格的 `<`，最后一个槽位永远用不上 | `dynamic_strategy.rs:20-22,75-90` |
| 切分尺寸 `planned_chunk_size = current_speed * 2.0`，其中 `current_speed` 是任务总速率；并且要求候选区间 ≥ 这个尺寸 | `strategy/mod.rs:80-84` |
| `runner_id.expect(...)` 依赖“并发控制总会先返回动作”这个隐式不变量 | `strategy/mod.rs:88`；`concurrency_control_strategy.rs:109-125` |
| 切分对象是剩余最多的块 | `chunk_planner.rs:471` |
| `per_runner_speed` 等于总速率除以 runner 数 | `download_loop/throughput.rs:49` |
| 策略每 1.5 s 执行一次，runner 失败后没有退避 | `strategy/mod.rs:15` |
| 失败预算只有一个任务级的连续失败计数器。`on_write_ok` 会把它清零；有其它在途连接时，拥塞错误不计数 | `strategy/failure_policy.rs:31-75` |
| 元数据请求已经用 backon 做了指数退避 | `task/meta.rs:43` |
| 写错误被拍平成 `TaskError::Other { message }` | `download_loop/completion.rs:16-19` |
| 先更新进度，再提交写 | `runner_manager/data.rs:154` |
| 下载模式按 `is_range_stream_available()` 选择 | `task/instance/drive.rs:202` |
| 预分配用 `set_len`，在稀疏文件系统上不会真的占用磁盘 | `concurrent_task/file_writer.rs:221` |
| 没有续传入口，`Progress` 每次都从 0 开始 | `concurrent_task/mod.rs` |
| 故障注入设施已经存在 | `bolt-load-tests/src/adapter/{http_server,fault}.rs` |

---

## 2. 目标与成功判据

| # | 判据 | 验证方式 |
| --- | --- | --- |
| G1 | 不会出现“任务报 `Completed`，但文件内容错误”的情况。服务器忽略 Range、偏移错误、total 不一致、对 range 请求做了编码压缩时，最终文件 hash 要么正确，要么任务以对应的类型化错误失败 | 故障注入集成测试（T4） |
| G2 | reqwest 和 ureq2 对同一个状态码给出同一种分类 | `classify_status` 表驱动测试，两个 adapter 共用（T2） |
| G3 | 写错误保留 `io::ErrorKind`；磁盘满会直接终止任务，错误里带上区间 | 单元测试（T6） |
| G4 | 调用方可以设置 `max_connections`。在任何时序下都不会 panic，活跃 runner 数也不会超过上限 | 单元测试和性质测试（T7、T8） |
| G5 | 失败的区间要等退避结束才会重新领取；遇到 `Retry-After` 时，本任务的所有新连接都遵守它 | 用注入时钟的确定性测试（T9） |
| G6 | 健康的连接不能“洗白”一个反复失败的区间：一个区间累计 `max_attempts_per_range` 次记到预算上的失败后，任务终止，错误里带上该区间最后一次的真实原因（计数规则见 §14.13）。在已证实窗口之上、旁边有已证实连接时被拒绝的连接不记预算（§14.13a，确认 2026-11-13）；同时开多个连接的开始、客户端已关闭而服务器仍在计数的连接、运行中缩小的上限、接受后不发数据的服务器，都不保证能活下来 | 单元测试和下载循环测试（T10），窗口之内和之上的拒绝各有测试 |
| G7 | 新的扩张控制器在三个场景里（单连接限速、高速、对 429 敏感），完成时间不比旧实现差，峰值连接数不超过用满带宽所需连接数的两倍。“不比旧实现差”：两种策略逐轮交替运行，每轮 `Doubling` 减 `Legacy` 的差，取中位数，不超过 `Legacy` 完成时间中位数的 5%。“用满带宽所需的连接数”是每个场景的参照值，按构造或观察确定：单连接限速是总带宽 ÷ 单连接速率（T14 的 s1 是 16）；高速取 1，这是这台机器上观察到的参照值，不是构造出来的（服务器不限速，§14.17、§14.18 里两条连接并不比一条快）；对 429 敏感是服务器允许的并发数（s3 是 4）。2026-11-08 由维护者改定，原判据是“连接数也不比旧实现多”（§14.17、§14.18） | 基准测试（T14、T28），结果写进 PR 描述 |
| G8（P2） | 注入 `Limits` 后，跨任务的总速率误差不超过 `limit` 的 ±10%（2 s 窗口）；限速等待不会触发空闲超时 | 集成测试（T15） |
| G9（P3） | 进程在任意时刻被杀掉后续传，最终 hash 正确；状态文件损坏、越界或重叠时，坏区间被丢弃并上报 | 集成测试（T19） |
| G10 | 服务器静默限制连接数（拒绝或不应答超出的连接）时，任务完成，hash 正确；上限降到服务器接受的连接数之后，超出它的连接只有恢复步骤的试探：每个到期时刻至多一个，同时至多 `limit + 1` 个，间隔按退避拉开 | 故障服务器的端到端测试（T30） |

---

## 3. 决策

下表默认采用改进方案 §6 的建议值。“状态”一列写“待确认”的条目，要在对应 PR 开工前由维护者拍板；拍板结果改动这里，tasks 跟着调整。

| # | 决策 | 采用 | 状态 |
| --- | --- | --- | --- |
| D1 | `max_connections` 的默认值 | 固定 8，与 CPU 核数无关 | 已确认（2026-11-05） |
| D2 | Range 用不了时怎么办 | 自动截断文件，改用单连接从头下载，并通过 `TaskStatus` 上报降级。触发条件除了 `RangeIgnored`，还包括 range 响应带了内容编码，或者能看出被客户端解过码（§4.3），和 IDM 一致 | 已确认（2026-11-03，扩展到内容编码） |
| D3 | 其它 4xx 归为不可重试 | 采纳（对齐 ureq2）；408 例外 | 已确认（2026-11-01，随 PR-1 开工） |
| D4 | P2 什么时候做 | P1 完成之后再评估 | 已确认（2026-11-06） |
| D5 | 冗余对冲 | 只做收尾版：没有可切的活、某个 runner 2 s 没有收到数据时，用空闲槽位重复下载它剩下的部分，先交来数据的一方胜出（§9.6） | 已确认（2026-11-06，依据 IDM 的实际做法） |
| D6 | 切分尺寸改用单连接速率 | 和扩张控制器放在同一个开关后面，一起做 A/B | 已确认（2026-11-06） |
| D7 | 按 host 的能力缓存 | 随 P2 一起做，TTL 1 h，不持久化 | 已确认（2026-11-06） |
| D8 | 限速的公平性 | 所有 runner 共享一个桶，不做按任务的公平分配 | 已确认（2026-11-06） |
| D9 | 解码 crate 的后端 | `compression-codecs`，只在 crate 内部使用；gzip、deflate 默认开启，br、zstd 放在 feature 后面（§4.4） | 已确认（2026-11-03） |
| D10 | 在哪一层解码 | `full_stream` 把 `Content-Encoding` 和 `Content-Type` 交给引擎，由引擎决定是否解码；adapter 自己不解码（§4.4） | 已确认（2026-11-03） |
| D11 | HTTP 校验与解析放在哪 | 新 crate `bolt-load-http`，依赖 core：搬入 core 里只有 adapter 调用的函数，以及 utils 的 `http` 模块；新增 `ContentCoding`。core 只留 trait、meta、错误类型和 `ContentRange`（§4.5） | 已确认（2026-11-03） |
| D12 | 下载途中资源变了（`ResourceChanged`） | **已修订（2026-11-09，维护者）：引擎只检测，以 `DownloadError::ResourceChanged` 失败（临时文件照常删掉，不回退）；从头重来移到引擎之上（客户端或 job 层），见 #195。** 原决定：任务内从头重来一次，和 §4.3 的回退一样：丢掉已写的数据、截断、刷新 adapter、重新探测，用新的 meta 再下一次；同一个任务里再变一次，就以 `DownloadError::ResourceChanged` 失败。T20 的“文件被替换 → 从头下载、hash 与新文件一致”在 C19 里以“下载途中被替换”端到端验证，“续传之前被替换”留给 C20（§11.2） | 已修订（2026-11-09，维护者），见 #195 |
| D13 | adapter 的缓存怎么刷新 | **已修订（2026-11-09，维护者）：C19 不加 `refresh`；重来要的新 meta 怎么取，和重来一起在 #195 里定。** 原决定：`BoltLoadAdapter` 新增 `fn refresh(&self) {}`，默认什么也不做；reqwest 和 ureq2 覆盖它，丢掉缓存的探测结果。引擎在 D12 的从头重来之前调用，C20 在续传之前调用（§11.2） | 已修订（2026-11-09，维护者），见 #195 |
| D14 | 验证器绑在哪 | **已移到 #195（2026-11-09，lead 按维护者对 D12、D13 的修订一并移走）：C19 不加 `range_stream_if`；没有 `refresh`，adapter 缓存的验证器不会变，共享 adapter 的任务看到的是同一个。** 原决定：绑在每个任务上，随每个请求传：`BoltLoadAdapter` 新增可选的 `range_stream_if(start, end, expected)`，默认调用 `range_stream`，所以自定义 adapter 照常工作，只是看不出资源变了。引擎从自己的 meta 取验证器和总长，传给每个 range 请求；reqwest 和 ureq2 实现它，`If-Range` 和响应检查都用传进来的验证器，不用 adapter 的缓存。`refresh()` 只影响调用者下一次 `retrieve_meta`，从不改变别的任务的 range（§14.23） | 已移到 #195（2026-11-09，维护者） |
| D15 | 续传的公开 API | `TaskBuilder::resume(Resume)`；状态文件是 `<dest>.boltload`，所有 I/O 都在 `Initializing` 里。按 D16 的修订（2026-11-09），`Resume` 是 `Off`（默认）和 `On`，没有 `Auto`、`Required`（§11.5.2） | 已确认（2026-11-09，维护者），已按 D16 修订 |
| D16 | 续传时不一致（验证器不同、没有可用的验证器、`total`、`source` 不同、头部坏了等） | **已修订（2026-11-09，维护者，和 #195 同一原则）：任务只提供机制，要么续上，要么以 `DownloadError::ResumeRejected { reason }` 失败，不改动文件；从头来、问用户、清理都由客户端或 job 层决定。** 原决定：丢掉旧的状态和部分文件、从头下载，`Required` 时失败（§11.5.1、§11.5.10） | 已修订（2026-11-09，维护者） |
| D17 | 同一个目标的并发任务 | 整个任务期间对状态文件加独占的建议锁，`Off` 的任务也加，修 #152；批准新增一个锁的依赖（`fs4` 或 `fd-lock`，选哪个由实现定，§11.5.4 选了 `fs4`） | 已确认（2026-11-09，维护者） |
| D18 | 状态文件的序列化 | **已修订（2026-11-09，维护者）：二进制格式，带 CRC，不用 `serde`、`serde_json`。头部有 magic、版本、`total`、选定的验证器（种类和值）、可选的 `source` 和头部 CRC；记录定长、各带 CRC，写到一半的残片按记录边界截掉；批准一个小的 CRC32C crate（§11.5.5 选了 `crc32c`）。** 原决定：`resume` feature，默认打开，用 `serde` 和 `serde_json` | 已修订（2026-11-09，维护者） |
| D19 | 数据完整性 | 像磁盘或 ECC 内存那样：部分文件按定长的块各存一个 CRC32C；续传时重读每个保留的块并校验，对不上的块丢掉、重新下载（像坏扇区），校验过的块保留，每个丢掉的块都报告，原因是 `CrcMismatch`。不防篡改（§11.5.1、§11.5.6） | 已确认（2026-11-09，维护者） |
| D20 | 带着数据断开的流是否还降并发 | 不降：只有 `Started` 之前失败的尝试和 429/503 是证据（§8.3） | 已确认（2026-11-13） |
| D21 | 连续拒绝之后降到多少 | 降到已连接的 runner 数（§8.3） | 已确认（2026-11-13） |
| D22 | 服务器明确限连（429/503）之后是否继续试探 | 继续，按退避拉开间隔（§8.3） | 已确认（2026-11-13） |
| D23 | §8.3 的常量 | 按 §8.3 的表，之后用基准测试校准 | 已确认（2026-11-13） |
| D24 | 降级前要多少证据 | 连续三次被佐证的拒绝，加上第三次之后一个 tick 间隔的进度（§8.3） | 已确认（2026-11-13） |
| D25 | 拒绝与 DNS、TLS 失败的区分 | 暂时靠佐证；core 里类型化的连接原因以后再做 | 已确认（2026-11-13） |
| D26 | 是否检测接受连接之后不发数据或重置的服务器 | 暂不检测：需要 runner 报告是哪种超时 | 已确认（2026-11-13） |
| D27 | 拒绝和区间预算 | 在已证实窗口之上的拒绝不记预算，其余照旧；不承诺 §14.13a 列出的几种情况能活下来 | 已确认（2026-11-13） |

---

## 4. Range 响应校验（P0-1）

### 4.1 共享校验函数

位置：`bolt-load-core/src/adapter/range.rs`（新增），导出为 `bolt_load_core::adapter::validate_range_response`，reqwest 和 ureq2 都调用它。理由和 §5.1 相同：校验规则是 adapter 和引擎之间的约定，外部 adapter 只依赖 core 就能用上。
`ContentRange` 也在 core 里（错误类型 `RangeMismatch` 要引用它），解析通过 `impl FromStr for ContentRange` 提供，不放进 utils，因为 utils 不依赖 core。

> D11 之后，`validate_range_response`、`RangeHeaders` 和 `limit_range_body` 移到 `bolt-load-http`；`ContentRange` 和它的解析留在 core（§4.5）。

```rust
/// A parsed `Content-Range: bytes first-last/complete` header.
pub struct ContentRange {
    pub first: u64,
    pub last: u64,
    pub complete: Option<u64>, // `*` → None
}

impl FromStr for ContentRange { /* … */ }

/// Combined header values (repeated lines joined with ", ", RFC 9110 §5.3)
/// plus the parsed `Retry-After`.
#[non_exhaustive]
pub struct RangeHeaders<'a> {
    pub content_range: Option<&'a str>,
    pub content_type: Option<&'a str>,
    pub content_encoding: Option<&'a str>,
    pub retry_after: Option<Duration>, // passed on to classify_status
}

pub fn validate_range_response(
    status: u16,
    headers: &RangeHeaders<'_>,
    requested: Range<u64>,       // half-open
    expected_total: Option<u64>,
) -> Result<ContentRange, AdapterError>; // the range a valid 206 carries

/// Hold the body to `got`'s length: `RangeBodyTooLong` past it,
/// `RangeBodyTooShort` if it ends cleanly before.
pub fn limit_range_body(body: AnyBytesStream, got: ContentRange) -> AnyBytesStream;
```

| 响应 | 结果 |
| --- | --- |
| `206`，带单段 `Content-Range`，满足 `first == start`、`last < end`、`complete ∈ {None, expected_total}`，并且没有 `Content-Encoding`（或者是 `identity`） | `Ok(got)` |
| `206`，但 `Content-Range` 缺失、解析失败（包括出现多行）或对不上 | `Unretryable::RangeMismatch { requested, got: Option<ContentRange> }` |
| `206` + `multipart/byteranges` | `RangeMismatch`。我们只请求单段 |
| `206`，但带了非 identity 的 `Content-Encoding` | `RangeMismatch`。这时 `Content-Range` 描述的是编码后的字节 |
| `200` | `Unretryable::RangeIgnored` |
| `416` | `Unretryable::RangeNotSatisfiable { total }`，`total` 从 `Content-Range: bytes */t` 里取 |
| 其它 4xx、5xx | 交给 §5 的 `classify_status` |
| 其它（1xx、3xx、非 200/206 的 2xx） | `Unretryable::Status { status }` |

约束：

- 必须在调用 `error_for_status()` **之前**拿到状态码和响应头，否则 416、429、503 的响应头就丢了。两个 adapter 的 `range_stream` 都改成“先校验，再取 body”。
- 所有请求（HEAD、`full_stream`、range 请求、探测）都显式带上 `Accept-Encoding: identity`，和 IDM 的默认做法一致。不带这个头时，任何编码都算可接受（RFC 9110 §12.5.3），服务器返回 gzip 是合规的，文件里就会存成编码后的字节。`before_request` 仍然可以覆盖它。
- 每个头按合并后的值传入：重复的行用 `", "` 连起来，非 UTF-8 的值有损保留。这样第二行 `Content-Range` 会导致解析失败，分行写的 `Content-Encoding` 也都会被检查到。
- 会解压的 HTTP 客户端会同时删掉 `Content-Encoding`，响应头上看不出来（§14.2）。所以 adapter 用 `limit_range_body` 包住 body，要求 body 长度正好等于 `Content-Range` 声明的长度：
  - 超出时，报 `Unretryable::RangeBodyTooLong { got }`，越界的那块数据不交给 runner；
  - 干净地提前结束时，报 `Unretryable::RangeBodyTooShort { got, received }`。数据不可压缩时，解压后的 body 就是这样结束的。连接中途断开是 body 自己的错误，原样传出，仍然可以重试。

  长度取的是响应自己声明的长度，不是 runner 的预算，所以切分后预算调低不会被误判。
- `last < end − 1` 是合法的，表示服务器只给了一部分。runner 收完这部分后按短读处理，从已接收的位置重新请求。`last ≥ end` 视为 `RangeMismatch`。

### 4.2 meta 探测

- 只有 `bytes=0-0` 拿到 `206`，并且带有合法的 `Content-Range`，才认定支持 Range。
- 删掉 `ContentSize::ContentLength(0) => available = true` 这个分支（`reqwest.rs:245-246`）。
- HEAD 里的 `Accept-Ranges: bytes` 只算弱信号：可以凭它进入多连接模式，因为 §4.1 的运行期校验会兜底。

### 4.3 任务级回退（D2）

触发回退的错误（任意一个）：

| 错误 | 来自 | 说明 |
| --- | --- | --- |
| `RangeIgnored` | §4.1 | 服务器忽略 Range |
| 带编码的 206 | §4.1 | 客户端不解压，看得到非 identity 的 `Content-Encoding`。现在报的是 `RangeMismatch`；T5 为它新增 `RangeContentCoded { coding }` 变体，回退只认这个变体，不认其它的 `RangeMismatch` |
| `RangeBodyTooLong`、`RangeBodyTooShort` | §4.1 | 客户端解压后删掉了编码头，只能从长度看出来。服务器算错长度也会这样，这时回退到单连接同样安全 |
| 解不开的 body | §14.1 | 从中间开始的编码区间。现在是 `Unretryable::Io`；T5 为它新增 `BodyUndecodable` 变体，回退只认这个变体 |

步骤：

1. runner 收到上面任意一个错误后，下载循环把它升级为 `DownloadError::RangeUnsupported { reason }`，取消所有 runner，并且不再接收新的写入。`reason` 区分“忽略 Range”和“内容编码”。
2. `drive.rs` 在 `initialize` 之后的分支里捕获这个错误：等所有写入完成，截断目标文件到 0，然后以 `DownloadMode::Singleton` 重新运行一次。
3. 回退时**文件大小按未知处理**（`total = None`，下载到流结束为止）。meta 里的长度来自 HEAD 或探测，响应带编码时数的是编码后的字节，解码后就对不上。IDM 也是这样：响应带编码时，文件大小取实际写入的字节数（§14.2）。
4. `TaskStatus` 里要能观察到这次降级和原因：`TaskInfo` 新增 `fallback: Option<FallbackReason>`，回退时 `info` 换成 `mode: Singleton`、`fallback: Some(reason)` 的新值，接着发布一个进度从零开始的 `Downloading`。不新增状态，也不新增事件通道（`task-api-spec.md` 的 STATUS-9，选择理由见 §14.3）。
5. 每个任务最多回退一次。Singleton 模式用的是 `full_stream()`（`singleton_task.rs:144`），不经过 §4.1 的校验，所以回退后不会再触发这些错误。

另外两条规则：

- **meta 阶段就能看出编码时**（HEAD 或探测响应带非 identity 的 `Content-Encoding`，客户端不解压才看得到）：直接选 Singleton，大小按未知处理，不进入多连接。这对应 IDM 把连接数降为 1。
- **`full_stream` 收到非 identity 编码时**：
  - 客户端会解压：头已经被删掉，拿到的就是明文，直接写入；
  - 客户端不解压：引擎按 §4.4 解码（C22 起；之前以 `Unretryable::ContentCoded { coding }` 失败）。解不了的编码仍以 `ContentCoded` 失败，不把编码后的字节当成文件写进去。§4.4 的例外（`Content-Type` 本身就是这种压缩格式）在 T5 里就按原样写入。

### 4.4 内容编码的解码（D9、D10）

目标：不管 HTTP 客户端开没开解压，单连接下载都能拿到解码后的文件。外部的 feature 不可控：用户的依赖图里可能有别的 crate 打开客户端的解压，也可能没有，所以解码必须由 bolt-load 自己负责。range 响应永远不解码，遇到编码就走 §4.3 的回退，和 IDM 一致。

**位置：** 新 crate `bolt-load-decode`，对现有生态做一层薄封装。输入输出都是 `Stream<Item = Result<Bytes, E>>`，解码错误通过 `From<DecodeError>` 转成 `E`，不绑定 `AdapterError`，引擎和自定义 adapter 都能直接用。编码的枚举 `ContentCoding` 和它的解析在 `bolt-load-http`（D11，§4.5），这个 crate 依赖它，不再自己解析头。

```rust
use bolt_load_http::ContentCoding;

#[non_exhaustive]
pub struct DecodeOptions {
    pub buffer_size: usize,      // output buffer per stream, default 64 KiB
    pub max_output: Option<u64>, // optional bound against decompression bombs
}

/// Fails with `Unsupported` for `ContentCoding::Other`, a coding whose
/// feature is off, or more layers than the limit.
pub fn decode<S, E>(
    body: S,
    codings: &[ContentCoding],
    options: DecodeOptions,
) -> Result<Decoded<S>, DecodeError>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: From<DecodeError>;

#[non_exhaustive]
pub enum DecodeError {
    Unsupported { coding: String },
    Invalid { source: Arc<io::Error> },
    TrailingData,
    TooLarge { limit: u64 },
}
```

**缓冲与吞吐：**

- **输入不拷贝。** 传输层给的 `Bytes` 原样借用，用游标记录消费到哪里，没消费完的部分留到下一次 poll。
- **输出用一块固定的缓冲。** 每个流一块 `BytesMut`（默认 64 KiB，可配置），解码器直接写进它的空闲容量。写满、输入耗尽或者流结束时，`split().freeze()` 交出去，不拷贝。下游释放上一块 `Bytes` 之后，`reserve` 能复用同一块分配，稳定状态下不再分配内存。
- **每次 poll 最多产出一块输出。** 压缩比再高，单次 poll 的内存和耗时也有上限，背压自然传回网络读取。
- **内存上界** = 输出缓冲 + 解码器状态。gzip、deflate 的窗口是 32 KiB；brotli 最大 16 MiB；zstd 设 `window_log_max = 23`（8 MiB），与 RFC 9659 §3 一致：HTTP 的 zstd 编码器不得使用超过 8 MB 的窗口，解码器必须支持到 8 MB。

**解码规则：**

- 多层编码（例如 `Content-Encoding: gzip, br`）按相反的顺序串联解码器。层数上限 2，超出报 `Unsupported`。
- gzip 支持多成员（RFC 1952 允许）。
- `deflate` 按 RFC 9110 §8.4.1.2 是 zlib 格式，但有服务器发 raw deflate：先按 zlib 解，头部不是 zlib 时改按 raw deflate 解（置信度中：主流客户端都这样兼容，实现前核对 tower-http 的做法）。
- 流的结束标记之后还有非空数据：报 `TrailingData`。宁可失败，也不写不确定的字节。
- 解码错误一律不可重试。
- 不认识的编码：报 `Unsupported`，任务以类型化错误失败，不写入。

**例外（按原样写入，不解码）：** `Content-Type` 本身就是这种压缩格式，例如 `application/gzip` 或 `application/x-gzip` 配 `Content-Encoding: gzip`。这是常见的服务器误配置：把 `.tar.gz` 文件标成 gzip 编码，用户要的其实是压缩包本身。浏览器对这种情况也有特殊处理（置信度中）。

**在哪一层解码（D10）：**

- **方案 A（已选）：** `full_stream` 在返回 body 的同时带上 `Content-Encoding` 和 `Content-Type`（core 新增一个返回类型），由引擎决定是否解码。任何 adapter 都绕不过去。core 不能引用 `bolt-load-http`（会成环），所以这个返回类型带原始的头值，由引擎用 `ContentCoding::parse_list` 解析。代价是 `BoltLoadAdapter` trait 的破坏性变更，和 T5 的 `DownloadError` 变更放进同一个大版本。
- **方案 B：** adapter 在 `full_stream` 里自己调用解码 crate，类似 `limit_range_body`。trait 不变，但自定义 adapter 忘了调用，就会静默写进编码后的字节。

**后端（D9）：**

- **方案 A（已选）：** `compression-codecs`。它是 async-compression 拆出来的同步编解码层，tower-http 用的就是它，当前依赖图里已有 0.4.41。接口是推式的：`Decode::decode(&mut input, &mut output)`，不绑定运行时，正好和上面的缓冲设计对上。它的 0.4.x 接口还在演进（已经有 `Decode` 和 `DecodeV2` 两套），所以只在 crate 内部使用，不出现在公开 API 里，并且锁定次版本。
- **方案 B：** 直接用 flate2、brotli-decompressor、zstd（或纯 Rust 的 ruzstd）。控制更细，但代码更多。
- 两个方案都是 gzip、deflate 默认开启（IDM 只处理这两种），br、zstd 放在 feature 后面：compression-codecs 的 zstd 依赖 C 库 libzstd。

### 4.5 HTTP 辅助 crate（D11）

**为什么拆：**

- **core 应该只放 adapter 和引擎之间的契约**：trait、meta、错误类型。现在 core 里还有一批只有 adapter 调用的 HTTP 函数。已核对调用方：`classify_status`、`validate_range_response`、`limit_range_body`、`check_content_coding` 只被 reqwest、ureq2 和它们的测试调用。引擎只用到 `ContentRange` 和错误类型。
- **HTTP 头的解析分在两处。** 一处是 core，一处是 utils 的 `http` feature。之所以分开，只是因为 utils 不依赖 core（§4.1、§5.1）。结果是 `Retry-After` 在 utils 里解析，上限却在 core 里设。
- **T23 需要一个 content-coding 枚举，现有 crate 都没有**（已读 lock 里的源码）：
  - `http` 1.5 只有头名常量，比如 `CONTENT_ENCODING`。
  - `headers` 0.4.2 的 `ContentEncoding` 只有构造函数 `gzip()`、`brotli()`、`zstd()` 和一个 `contains(&str)`。它的内部字段私有，也没有迭代器，所以拿不到 coding 列表，也拿不到未知 coding 的名字。`contains` 用 `==` 比较，大小写敏感，而 RFC 9110 §8.4.1 规定 coding 名不区分大小写。
  - `headers::ContentRange` 比 core 的解析器宽松：单位比较大小写敏感，数字两侧不容许空白，不检查 `last < complete`，还接受 `bytes */N`。所以仍然用 core 自己的解析器。

**依赖方向：**

- `bolt-load-http` 依赖 core，因为它的函数返回 `AdapterError`。它还接手 utils `http` feature 的依赖：`http`、`headers`、`regex`、`language-tags`、`percent-encoding`、`httpdate`。
- core 不能依赖 `bolt-load-http`，否则成环，所以 core 不 re-export 搬走的函数。旧路径直接删除（全局约束 3，不保留兼容层）。
- 依赖 `bolt-load-http` 的有三方：
  - `bolt-load-adapter`；
  - `bolt-load-decode`，用它的 `ContentCoding`；
  - 引擎，T23 解码时用它解析 `full_stream` 交出的头值。

  引擎本来就无条件依赖 `bolt-load-adapter`，所以实际的编译量不变。
- utils 删掉 `http` 模块和 feature，只留 reader 和 telemetry。
- `BoltLoadAdapterMeta::content_encoding` 仍然是 `Option<String>`：core 引用不了 `ContentCoding`。

**搬迁：**

| 现在 | 之后 |
| --- | --- |
| core `adapter::{classify_status, MAX_RETRY_AFTER}` | `bolt_load_http::{classify_status, MAX_RETRY_AFTER}` |
| core `adapter::{validate_range_response, RangeHeaders, limit_range_body}` | `bolt_load_http::{validate_range_response, RangeHeaders, limit_range_body}` |
| core `adapter::check_content_coding` | `bolt_load_http::check_content_coding` |
| core `adapter::is_identity_coding` | 删除，改为 `ContentCoding::parse_list(value).is_empty()` |
| utils `http::{ContentDisposition, parse_retry_after, parse_extended_value, …}` | `bolt_load_http::header::{…}` |
| core `adapter::{ContentRange, InvalidContentRange}` 和 `FromStr` | 不动：错误变体和引擎都引用它 |

函数签名不变，仍然接收 `&str` 和 `u16`，不改成 `HeaderMap`：ureq2 用的不是 `http` 1 的类型，改了反而要多一层转换。

**`ContentCoding`：**

```rust
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentCoding {
    Gzip,
    Deflate,
    Brotli,
    Zstd,
    /// A coding this crate does not know, lowercased.
    Other(String),
}

impl ContentCoding {
    /// Parse a combined `Content-Encoding` value, in the order applied.
    /// Names match case-insensitively, `x-gzip` is `Gzip`, and `identity`
    /// and empty items are skipped.
    pub fn parse_list(value: &str) -> Vec<ContentCoding>;

    /// The registered name, such as `gzip` or `br`.
    pub fn as_str(&self) -> &str;
}
```

- **解析不会失败。** 认识不认识，由调用方决定：`check_content_coding` 把非空列表报成 `ContentCoded`（`.tar.gz` 的例外不变）；`decode` 把 `Other` 报成 `Unsupported`。
- **`ContentCoded { coding }` 和 `RangeContentCoded { coding }` 仍然带原始头值（`String`）**，日志里能看到服务器实际发了什么。
- **只解析响应头**，不处理 `Accept-Encoding` 的 q 值语法。

---

## 5. 状态码分类与错误类型（P0-2）

### 5.1 `classify_status`

位置：
- 分类函数在 `bolt-load-core/src/adapter/status.rs`（新增），导出为 `bolt_load_core::adapter::classify_status`。这个映射是 adapter 和引擎之间的约定（`Congestion` 触发降级，`Retryable` 计入重试），外部 adapter 只依赖 core 也能用上它，不必再依赖 `bolt-load-adapter`。
- `Retry-After` 的解析在 `bolt-load-utils` 的 `http::parse_retry_after`（新增）。它只做纯解析，不设上限。utils 不依赖 core，这样分层不会倒置。
- D11 之后，`classify_status`、`MAX_RETRY_AFTER` 和 `parse_retry_after` 都在 `bolt-load-http`。外部 adapter 依赖 core 和 `bolt-load-http`，仍然不必依赖 `bolt-load-adapter`（§4.5）。

```rust
// bolt-load-core
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);
pub fn classify_status(status: u16, retry_after: Option<Duration>) -> AdapterError;

// bolt-load-utils, feature `http`
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration>;
```

| 状态 | 分类 |
| --- | --- |
| 401 / 403 | `Unretryable::Unauthorized { status, message }` |
| 404 / 410 | `Unretryable::NotFound { status }` |
| 408 | `Retryable::Status { status }` |
| 416 | `Unretryable::RangeNotSatisfiable`（见 §4.1） |
| 429 / 503 | `Unretryable::Congestion { status, retry_after: Option<Duration> }`，替换 `ServiceUnavailable` |
| 其它 5xx | `Retryable::Status { status }` |
| 其它 4xx | `Unretryable::Status { status }`（D3） |

- `Retry-After` 支持两种格式：秒数和 HTTP-date，过去的日期算作 0。解析失败时当作 `None`。`classify_status` 只对 429/503 保留这个值，并夹到 `MAX_RETRY_AFTER`（120 s）以内。
- `Congestion` 留在 `UnretryableError` 里，和现在的 `ServiceUnavailable` 位置相同。这样 `failure_policy.rs:18` 的拥塞容忍和 `has_recent_unretryable` 降级逻辑都不需要改动。
- 416 **不做**“回滚尺寸”的处理。IDM 的 `Try rollback` 和 416 无关（改进方案 §8 A4）。收到 416 时先重新探测 meta：`total` 变了说明资源变了，任务终止；没变说明区间计算有 bug，直接报错，不重试。
- 409 / 412 **不**归为 `ResourceChanged`。`ResourceChanged` 只在 P3 的验证器比对失败时产生。

### 5.2 类型变更

```rust
#[non_exhaustive]
pub enum UnretryableError {
    Unauthorized { status: u16, message: String },
    NotFound { status: u16 },
    Congestion { status: u16, retry_after: Option<Duration> },
    Status { status: u16 },
    RangeIgnored,
    RangeMismatch { requested: Range<u64>, got: Option<ContentRange> },
    RangeNotSatisfiable { total: Option<u64> },
    // existing: Internal, Cancelled, Io, RangeStreamNotSupported, Whatever
}

#[non_exhaustive]
pub enum RetryableError {
    Io { source: Arc<io::Error> },
    Status { status: u16 },
}

#[non_exhaustive]
pub struct BoltLoadAdapterMeta { /* existing fields; P3 adds etag, last_modified */ }
```

`BoltLoadAdapterMeta` 加上 `#[non_exhaustive]` 之后，crate 外部就不能再用结构体字面量构造它，所以要同时提供 `BoltLoadAdapterMeta::new(content_size)` 和 builder 风格的 setter。这些都是 breaking change，在同一个 PR 里一次完成。

---

## 6. 写错误结构化（P0-3）

- `completion.rs:16-19` 改为 `TaskError::Write { range: Range<u64>, source: Arc<io::Error> }`。
- `ErrorKind::StorageFull` / `QuotaExceeded` 会终止任务。它们在 `Display` 里给出可操作的提示，不进入重试。
- 不做“每次切分前检查剩余空间”：这种检查只是建议性的，而且有竞态。要保证空间，应该在预分配时真正占用磁盘，这放到 P3。

---

## 7. 连接上限与容量安全的切分（P1-1、P1-2、S2）

### 7.1 公开配置

- `TaskOptions::max_concurrency`（测试专用）改名为 `max_connections: NonZeroUsize`，公开，默认值是 `DEFAULT_MAX_CONNECTIONS = 8`（D1）。`TaskBuilder` 增加 `max_connections(n)`。
- `RunnerManager::new`、`StrategyControl::new` 和 `IdGenerator` 的容量都改为跟随这个值。
- `max_consecutive_failures` 的默认值改为 `max(8, 2 × max_connections)`（S2）。

### 7.2 切分的三个步骤

```text
split(candidate):
  slot = manager.try_reserve_slot()?          // 1. 先占槽位，失败直接返回，不报错
  match planner.resize(candidate) {           // 2. 再调整区间
      Ok(new_range) => manager.spawn_in(slot, new_range),   // 3. 最后 spawn
      Err(_)        => manager.release(slot),               //    失败就回滚
  }
```

- `spawn_runner` 不再 `expect`。槽位不足时记一条 `trace` 并跳过。
- `find_chunk_to_split` 的返回类型改为：

  ```rust
  enum SplitCandidate {
      Unassigned(Range<u64>),
      Running { runner: RunnerId, range: Range<u64> },
  }
  ```

  这样就去掉了 `strategy/mod.rs:88` 的 `expect`。（`find_chunk_to_split` 和 `SplitCandidate` 只给 `Legacy` 用，C14 已随它删除，见 §14.20；扩张走 `largest_running_chunk`。）
- 所有闸门都改成 `current + n <= max` 的形式，让最后一个槽位也能用上。
- 不变量：任何时刻 `active_runners <= max_connections`，并且所有区间之和正好覆盖 `[0, total)`、互不重叠。T8 用性质测试来验证。

---

## 8. 退避与失败预算（P1-3、P1-4、S3）

### 8.1 区间上的退避状态

`ChunkPlanner` 里的可用区间记录增加两个字段：

```rust
struct AvailableRange {
    range: Range<u64>,
    attempts: u8,
    not_before: Instant,
}
```

- runner 失败、区间退回可用池时：`attempts += 1`，`not_before = now + backoff(attempts)`。
- `backoff(n) = min(0.5 s × 2^n, 30 s)`，再加满抖动。抖动用 backon 的实现，和 `meta.rs` 一样。
- 遇到 `Congestion { retry_after: Some(d) }` 时，延迟取 `max(d, backoff)`，上限 120 s。这个冷却还要写进**任务级**的 `congestion_until`：在它之前，所有新建连接都要等待，因为服务器限制的是连接数，不是某一个区间。
- 区间被切分后，新产生的区间 `attempts` 从 0 开始；相邻的可用区间合并时，取两者 `attempts` 的较大值。
- `ConcurrencyControlStrategy` 下发 `CreateTask` 时，跳过 `not_before > now` 的区间。`download_loop/deadline.rs` 增加一个唤醒点，取“最早的 `not_before`”和 `congestion_until` 中较早的那个，不必等 1.5 s 的 tick。
- 退避相关的方法都把 `now: Instant` 作为参数传入，沿用 `FailurePolicy::check(Instant::now())` 的写法（`download_loop/deadline.rs:32`）。这样测试可以直接构造时间点，不需要 sleep。

### 8.2 三层失败预算

| 层 | 条件 | 计入 | 兜底 |
| --- | --- | --- | --- |
| 重启 | 本次尝试已经收到 `> 0` 字节之后，出现 `Timeout` 或断流 | 不计入连续失败，只走 §8.1 的退避；`attempts` 照常增加 | `no_progress_timeout` |
| 每区间预算 | `attempts >= max_attempts_per_range`（默认 5，可以通过 `TaskOptions` 设置） | 任务终止，返回 `TaskError::RangeExhausted { range, source }`，`source` 是该区间最后一次的真实错误 | — |
| 任务级连续失败 | 保持现状 | `on_write_ok` 仍然会清零它，但有了第二层，健康连接就“洗白”不了坏区间 | — |

实现时预算改用独立的 `charged` 计数，重启和拥塞不计，见 §14.13。

不采纳“空闲超时一律不计失败”：如果没有进展判据，就可能无限重连下去。

### 8.3 静默的连接上限（PR-6a）

**问题。** 有的服务器限制连接数，却不回 429/503：超出的连接被直接拒绝（连接被关闭或重置），或者被接受后一直不应答。原来的并发控制按失败率降级，有三个缺陷：

- 降过一次之后停在 `Degraded`，只看失败率决定是否恢复，不会再降。服务器限 4 个、`max_connections = 8` 时，上限停在 7，超出的连接一直被拒。
- 恢复不验证：15 s 内没有新失败就升一级，不管试探的连接有没有用上。
- 任何不可重试的结果都会立即降级，包括带着数据断开的 `SmallerThanTotalSize`，它说的不是连接数。

**证据：只看 `Started` 之前失败的尝试（D20）。**

| 阶段 | 原因 | 分类 |
| --- | --- | --- |
| 连接中 | `RetryableError::Io` | 拒绝 |
| 连接中 | `TaskError::Timeout` | 拒绝（连接超时） |
| 任意 | `UnretryableError::Congestion`（429/503） | 拥塞 |
| 连接中 | `RetryableError::Status`、`ChannelClosed`、spawn 失败、其它不可重试的错误 | 无 |
| 传输中 | 其它一切 | 无 |

- `Started` 之前的 `Timeout` 只能是连接超时：这时还没有帧，drain 不会超时，所以 runner 不用改。
- `Started` 之后的都不算：那里的 `Timeout` 可能是空闲超时，也可能是 drain 超时（循环没及时取帧，与服务器无关），两者是同一个 `TaskError::Timeout`；收到响应头之后、第一个字节之前的重置，也分不清是“没发字节”还是“字节还在背压里没被接收”。
- 所以“接受连接之后不发数据或重置”的服务器**检测不到**（D26），照旧按区间退避处理。
- 拒绝和 DNS、TLS 失败分不开：adapter 把连接失败拍平成同一种错误。下面的“佐证”挡住了大部分（D25）。

**记录。** `RunnerManager` 持有 `AdmissionEvidence`，是一组固定的值，没有事件日志，不会溢出也不会丢弃：

| 值 | 由谁改 |
| --- | --- |
| `connected`：runner ID 的集合 | 生命周期的 `Started` 分支和测试的 `mark_started` 调 `on_started`；`release_runner` 调 `on_ended`，任何结束方式都经过它 |
| 连续拒绝 `{ count, level }` | 一次被佐证的拒绝；`level` 及以上的一次 `Started`；下文的重置 |
| `congestion_pending` | 一次拥塞；由并发控制取走 |
| 恢复步骤的两个标记 | 见 §8.3 的“恢复” |
| `proven_window` | 策略 tick 和失败时取已证实的连接数的最大值；降级时截到新上限（供 C39 使用） |
| 三个时刻：第三次拒绝、最近一次拒绝、本次占满的起点 | manager 没有时钟，只标为“待盖章”；循环在 `on_runner` 开头和每个策略 step 开头用自己的 `now` 调 `stamp(now)` |

- 每个结束的尝试都经过同一个钩子 `classify(phase, error)`：连接失败、runner 丢失、传输中的失败。拒绝只在连接中记录；拥塞在任何阶段都记录，包括 body 流里的 429/503。
- 还没盖章的时刻读作“未知”，未知时并发控制什么都不做。

**静默规则：连续三次拒绝（D21、D24）。**

- 分类时另有一个尝试**已证实**（连接中、本次尝试已有字节被接收），这次拒绝才算被**佐证**，并记下 `others`：此刻其它在途的尝试数。没有佐证的拒绝在这里不算数。
- `count` 是连续被佐证的拒绝次数，`level` 是其中最小的 `others`。
- 连续拒绝在以下情况清零：一次 `Started` 时至少有 `level` 个其它尝试在途（服务器在这个水平上接受了连接）；上限改变，或恢复步骤开始、结束；`STREAK_GAP` 内没有新的拒绝。间隔按循环处理每次拒绝的时刻计算，在盖章时检查：间隔之后才到的拒绝开始新的一轮，不会续上已经过期的一轮，即使中间还没有 tick。
- 低于 `level` 的成功、连续拒绝开始之前的成功都不算，所以很长的成功历史稀释不了它。
- 策略 tick 时同时满足以下条件就降：`count ≥ 3`；第三次拒绝之后至少一个 tick 间隔，有非空帧被接收（排除整体断网）；`1 ≤ connected < limit`；tick 不迟到；writer 能接收数据。降到 `max(connected, min_concurrency, 1)`，`proven_window` 截到新上限。

**拥塞规则（D22）。** 拥塞（只认类型化的 429/503）在下一个 step 生效：`limit := min(limit, max(active, min_concurrency, 1))`，这一个 step 只改上限，不建 runner。它永远不会升高上限。`create_tasks` 在拥塞待处理时不创建，原来是任何不可重试的结果都会挡住它。`SmallerThanTotalSize` 不再降级。

**迟到的 tick。** 距上一个 tick 超过两个间隔的 tick 是迟到的。循环先轮询 tick 再轮询 runner，卡顿之后 tick 看到的状态可能马上被排队的生命周期事件改掉，所以迟到的 tick 不执行静默规则。

**恢复：一次一步，要验证，按退避拉开（D22）。**

- 两个控制器之间只交换两个数：扩张一侧给出 `want`（目前恒为 `max_connections`），并发控制给出 `limit_cap`，step 进行中是 `limit + 1`，否则是 `limit`。`cap = min(limit_cap, want, max_connections)`，能新增的 runner 数一律按 `free = cap.saturating_sub(active)` 计算：建 runner、唤醒后的创建、dynamic 策略的切分都一样。`active > cap` 是合法状态，多出来的 runner 不会被停掉，下完自己的块就结束。
- 上限只能通过 step 升高。step 是对 `limit + 1` 的一次试探，在策略 tick 上开始，要求同时满足：没有 step 在进行；没有拥塞在挡新连接；`want > limit` 且 `limit < max_connections`；`now ≥ due`；`connected ≥ limit`；tick 不迟到。如果已经有超过 `limit` 个连接在活动，step 不新开连接，直接评判现有的连接：上一个 step 留下的试探连接就这样被下一个 step 计入，既不停掉也不忽略。
- step 有四个出口，每个 tick 按顺序检查：

| 检查 | 结果 |
| --- | --- |
| step 开始之后分类到一次被佐证的拒绝或一次拥塞 | 拒绝 |
| step 开始之后分类到一次没有佐证的拒绝（迟到的 tick 不看这一条） | 不确定 |
| `now ≥ started_at + STEP_DEADLINE` | 不确定 |
| `connected ≥ limit + 1` 连续保持了 `T_OBS`，此刻 `proven ≥ limit + 1`，tick 不迟到 | 接受 |

| 结果 | `limit` | 等待 | 下一个 `due` |
| --- | --- | --- | --- |
| 接受 | `limit + 1` | 回到基础值 | `now` |
| 拒绝 | 不变（拥塞时再按拥塞规则降） | 加倍，不超过上限 | `now + 等待`；拥塞时从拥塞结束算起 |
| 不确定 | 不变 | 不变 | `now + 等待` |

- step 被标记之后（被佐证或没有佐证的拒绝、拥塞），到下一个 tick 判定它之前，补充 runner 和切分都按 `limit` 而不是 `limit + 1` 计算：试探的区间退避结束得比 tick 早，也不会再开第二个试探；低于 `limit` 的连接照常补上。
- 两个标记按程序顺序记录，不比较时间：`begin_step()` 清空，之后分类到的拒绝或拥塞才会置位。和 step 同一个 `now`、但在它之前处理的拒绝不会拒绝它。
- 连续占满的起点来自生命周期，不来自 tick 碰巧看到的数：让 `connected` 达到 `limit + 1` 的那次 `Started` 把它标为待盖章；step 开始时已经达到，就取 step 的开始时间；任何一次 `release_runner` 让数目跌破，立即清空，还没盖章的也一样。所以一个 runner 在一个 tick 之后结束、替补在下一个 tick 之前连上，时钟重新开始；计时从试探连接连上算起，不从 step 开始算起。
- 静默降级之后，`due` 是降级时刻加基础等待；拥塞之后，`due` 从拥塞结束算起。任何结果之后，下一个 step 都在 300 s 之内到期。拒绝永远不会升高上限，只有被接受的 step 会，每次加 1。
- `max_connections = 1` 时不会有 step，也不会降级。

**常量（D23）。**

| 常量 | 值 |
| --- | --- |
| 连续拒绝 | 3 次 |
| 确认 | 第三次拒绝之后一个 tick 间隔的进度 |
| `STREAK_GAP` | `2 × connect_timeout + 30 s`（默认 90 s），容得下连续三次连接超时；溢出时取 `Duration::MAX` |
| 迟到的 tick | 距上一个 tick 超过 2 个间隔 |
| `T_OBS` | 连续占满 15 s（原来的 `recovery_observation_period`） |
| `STEP_DEADLINE` | `connect_timeout + T_OBS + 2` 个 tick（默认 48 s）；溢出时取 `Duration::MAX`，这样的截止时间永远到不了 |
| step 之间的等待 | 30 s（原来的 `recovery_delay`），每次拒绝加倍，上限 300 s |

都没有校准，留给基准测试。

**定时器。** 没有新的定时器，也没有新的唤醒点。所有判断都在策略 tick 上做，时间都是传入的 `now`，`strategy/` 里不调 `Instant::now()`。

**不做的事。**

- 接受连接之后不发数据或重置的服务器（D26）。
- 拒绝与 DNS、TLS 失败的区分（D25）。
- 一次短暂故障之后只剩一个连接还在下载：上限会降到 1，靠恢复步骤每 15 s 升一级（D21 的已知误判）。
- 排在满 writer 后面的帧在拒绝之后被接收，也算进度：第二种已知误判，上限是每个 runner 32 帧的队列。

---

## 9. 扩张控制器与切分尺寸（P1-5、S1、P1-6、P1-7、S4）

### 9.1 开关

- 新增 crate 内部的 `GrowthStrategy { Legacy, Doubling }`。默认是 `Legacy`。
- 在 `#[cfg(any(test, feature = "unstable-strategy"))]` 下，通过 `TaskOptions::growth_strategy` 暴露出来，供测试和基准测试使用。
- 基准测试满足 G7 之后，另开一个 PR 把默认值切换为 `Doubling`，并删除 `Legacy` 和开关。
- 已完成（PR-7b，C14，§14.20）：`Doubling` 是唯一的策略，`GrowthStrategy`、`TaskOptions::growth_strategy`、`TaskBuilder::growth_strategy`、这个 feature 和 `Legacy` 都已删除。上面三条是当时的做法，留作记录。

### 9.2 `Doubling` 控制器

```text
every grow_interval (2 s, aligned with the strategy tick):
  if congestion_until > now                             -> skip
  if any runner is Connecting or has no data this tick  -> skip
    (a running one silent for hedge_idle does not count, see below)
  if limits.speed_limit is set and speed >= 0.85 × limit -> skip
  if the last full doubling is not judged yet:
    if allowed < the runners right after it, or a running runner was let off as silent:
      if it reached max_connections when it was made    -> skip, keep it unjudged
      else                                              -> drop it, no verdict
    else if speed_window_after < speed_window_before × 1.10
                                                        -> plateau: stop growing
  if allowed >= max_connections                         -> stop
  target = min(allowed × 2, max_connections)
  repeat (target − allowed): split(largest remaining) via §7.2
```

- 上限处的规则（C28，2026-11-08 维护者决定，§14.18）：到了上限，上一次完整的翻倍照样判断。翻到了上限（按做出翻倍那个 tick 的上限算）、还没判断的翻倍，在活跃数回到翻倍后的数之前不扩张，所以 runner 下完不会补位；只有低速或静默重启停掉一个 runner 时，待判断的翻倍不下结论、直接丢掉（C29，2026-11-08 维护者决定，§14.19），重启的区间被重新领取后可以再切；上限以下的翻倍少了 runner 时不下结论，照常扩张（C11 的做法）。
- 静默的 runner 不再挡住扩张（C29，2026-11-08 维护者决定，§14.19）：一个正在运行的 runner 已经静默满 `hedge_idle`（2 s），并且它连上也至少这么久了，检查 5 就不算它。静默指 runner 没有读到 body，起点取 runner 自己记下的最近一次读（没有就是 spawn），正在推送一帧的 runner 不算静默（§9.5）；数据通道里有循环还没取走的帧的 runner 不算静默（它在交数据，只是循环落后了）；writer 满的时候谁都不算静默。静默的 runner 不贡献速度，这样的 tick 量不出翻倍的效果，所以和活跃数不够时一样处理：翻到上限的翻倍继续等，上限以下的不下结论、照常扩张。
- 降速交给 `ConcurrencyControlStrategy` 处理，扩张控制器只负责增加连接。
- 增益的判断用扩张前后各一个稳定窗口的总速率来比较，不用每连接速率，因为 `per_runner_speed` 本身就是由总速率算出来的。

### 9.3 切分尺寸（S1）

```text
planned = max(donor_speed × split_window, min_chunk_size)
donor_speed = 被切 runner 自己的速率，拿不到时用 throughput.per_runner_speed
split_window = 2 s
```

`planned` 是门槛，不是切下来的大小：剩余量 ≥ `planned` 的运行中 chunk 才切，切在一半处（`planned_split_size`，§14.14；原来的上界 `remaining / 2` 去掉了，理由见 §14.14）。

依据：IDM 的切分候选值，是由被切连接**自己的**速率推出来的（改进方案 §8 A1）。原来的 `current_speed × 2` 用的是任务总速率。按推理（还没有基准测试验证），它会让门槛随连接数近似线性增长。（`Legacy` 保留原公式直到 C14，§14.20 起已删除。）

### 9.4 超时（P1-6）

- 不照搬 IDM 的 30 / 120 / 300 s。现有的 `connect_timeout` 和 `stream_idle_timeout` 两级结构保持不变。
- “收到过数据之后的超时”走 §8.2 的重启语义。
- 约束：`stream_idle_timeout` 必须大于 §10.3 的单次限速等待上限。

### 9.5 低速 runner 重启（P1-7、S4，可选，跟随 `Doubling`）

触发条件，需要同时满足：

1. 这个 runner 在过去 `slow_window`（默认 10 s）里的速率，低于**本任务**中位数的 `slow_ratio`（默认 0.25）；
2. 它还有未完成的字节，而且剩余量 ≥ `2 × min_chunk_size`，快要收尾的 runner 不重启；
3. 任务内至少有 3 个活跃的 runner；
4. 没有启用限速。

处理：从已经收到的位置停掉这个 runner，区间退回可用池，`attempts += 1`，走退避。默认参数先按上面的值实现，再通过 T14 的基准测试校准，**不照搬** IDM 的 1.3、50 ms 这些常量。

静默的路径（C29，2026-11-08 维护者决定，§14.19）：一个正在运行的 runner 满足下面全部条件时，不需要完整窗口，也不需要 3 个 runner，同样从已经收到的位置停掉（`Slow`）：

1. 已经静默满 `silent_restart_after`（4.5 s，3 个 tick）：没有读到 body。runner 攒满 32 KiB 才交一帧，读得慢的 runner 可能很久不交帧，但它在读，不是卡住。所以 runner 在重置自己的空闲计时器的地方记下当时的时刻：每读到一个非空的块，以及推送完一个满缓冲、接着读同一个块时；正在推送（等循环取走数据）时算“此刻在活动”，和空闲计时器一样，等下游不算空闲（`ReadActivity`，和下载循环共享）。流结束、空闲超时、出错时最后推出去的那一帧不算读。静默的起点就是 runner 记下的最近一次读，没有就是 spawn；被接受的帧、writer 恢复可写都不重新算（读那些字节在前；被 writer 挡住的 runner 在推送，本来就算在活动）。writer 满的时候不判。
2. 它连上（`Started`）也至少这么久了：在 4.5 s 之前的某次采样里它已经在运行。还在连接的 runner 归连接超时管。
3. 剩余量 ≥ `2 × min_chunk_size`：剩得更少的交给收尾对冲（§9.6），对冲中的一对也不判。
4. 它的数据通道里没有循环还没取走的帧：有的话它在交数据，只是循环落后了，这时停它会把那些帧连同接收端一起丢掉。这一条只用于静默路径；低速路径照旧，排着帧的 runner 仍然算进中位数。
5. 不在拥塞期间，没有启用限速。

两条路径停掉 runner 时，`Doubling` 待判断的翻倍不下结论、直接丢掉，翻到上限的也一样（§9.2、§14.19）。低速和静默两条路径合起来每个 tick 最多停一个：先判低速，低速没停才判静默；静默的有多个时停静默最久的，一样久停 ID 小的。分类和低速一样：这次尝试有字节被接受就是重启，不记预算；没有就记预算。`slow_window`、`slow_ratio` 不变。

### 9.6 收尾对冲（D5，跟随 `Doubling`）

IDM 6.43 的做法（IDA 静态分析，2026-11-06，见改进方案 §8 A2）：一条连接下完自己的分片、或者正空闲时，如果已经没有可切的活，而另一条连接已经 2 s 没有收到数据，就从后者已经收到的位置起，重复下载它剩下的同一段。原连接不停。分叉之后谁先写入数据就用谁，另一条关掉。每个分片最多一个对冲，连接数不增加。另做过一次动态测试，只看到了对冲请求：它从停顿的位置开始，当时原连接还开着；胜负没有观察到。

bolt-load 只做这个收尾版。§9.5 的低速重启在剩余量不足 `2 × min_chunk_size` 时不生效，收尾阶段卡住的连接现在只能等 runner 的空闲超时（默认 30 s）。对冲补的就是这一段。

下文的“一对”指同一个 chunk 上的原 runner 和对冲 runner。“分叉点”指发起对冲时 planner 已经接受的连续位置，也就是这个 chunk 的 `downloaded.end`。

#### 触发

需要同时满足：

1. 没有可切的活，和 C11 的切分门槛对齐（理由见下文“排除”里的“和 C11 对齐”）：
   - 没有可领取的未分配区间（`get_eligible_ranges(now)` 为空，冷却中的不算）；
   - 候选 runner 自己的 chunk 剩余量 < `2 × min_chunk_size`，也就是 §9.5 低速重启管不到的那一段；
   - 其它未完成的 chunk，不论 Pending 还是 Running，剩余量都 < `planned_split_size`，即 C11 的切分门槛 `max(donor_rate × 2 s, min_chunk_size)`（§14.14）。Pending 的 chunk 一律向 `planned_split_size` 传 `None` 作 donor 速率，也就是用 `per_runner_speed` 代替：`RunnerRates` 给连续两次采样都还在连接的 runner 记的是 `Some(0.0)`，不是 `None`，直接用它，门槛就退到 `min_chunk_size`。这是 T27 借用 C11 的门槛，C11 自己从不切 Pending 的 chunk。正在停止的 runner（§14.15）的 chunk 和这次尝试对冲过的 chunk（`hedge` 不为空，包括分出胜负之后的）不算：前者的区间马上要退回，后者不能切（见“排除”）。
2. 能再开一条连接：活跃 runner（一对算两个）少于当前的并发上限（下载循环的 `max_concurrency`，并发控制降级后会变小）；`try_reserve_slot` 能占到槽位（§7.2）；不在 `congestion_until` 之前（§8.1）；没有启用限速。
3. 某个 runner 已经静默 `hedge_idle`（默认 2 s），静默的算法见下。
4. 它的 chunk 还有没收到的字节，这次尝试还没有对冲过（见“预算”），它也不在停止中（§14.15）。

有多个候选时，选静默最久的。每个 tick 最多发起一个对冲。发起对冲的函数自己再查一遍条件 1 和 2，和 `spawn_runner`、`split` 自己查拥塞一样（§14.12）。

胜负未分时，这个 chunk 不切分，T13 也不单独重启其中一方。原 runner 在这期间从 Pending 变成 Running，也一样。

静默的算法：

- 从 runner manager 最近一次接受这个 runner 的非空帧（planner 截断之后仍不为空）算起。还没交过数据的 runner（在连接、等响应头，或者已经 `Started` 但还没有数据），从它被 spawn 的时刻算起：manager 这一层没有“发出请求”的时刻。
- 现在 manager 里没有按 runner 的时间戳：`meters` 只记字节数，`built_at` 在 runner 内部。T27 要加一个，时间取下载循环处理事件时的 `now`。
- runner 平时攒满 32 KiB（`BUFFER_SIZE`）才交一帧；没攒满的部分，要到收够自己的区间、流结束、空闲超时或者流出错时才交出来。所以慢于 16 KiB/s 的 runner，在 manager 看来也会静默 2 s，收尾时也会被对冲。代价看 T14 记录的重复字节数。
- writer 满的时候，下载循环不取任何 runner 的数据（`poll_runner` 只在 `can_write()` 时取），这段时间不算静默：writer 满时不对冲；writer 恢复可写后，所有 runner 都从恢复的那一刻重新算。runner 自己在背压结束后也重置空闲计时器（§1.2），规则相同。
- 发起对冲不重置原 runner 的空闲超时，也不推迟任务的无进展截止时间（`no_progress_timeout`）。只有新接受的字节才推迟它，败方被丢掉的帧不算。

`hedge_idle` 先取 IDM 的 2 s，再通过 T14 的基准测试校准。IDM 在非收尾阶段还会按本次下载的卡顿统计（均值 + 3σ）对冲，这里不做（§13）。

#### 仲裁

- 对冲 runner 请求 `[分叉点, chunk 末尾)`，和原 runner 还没交来的是同一段。原 runner 照常运行。
- 分叉点取 planner 已经接受的位置，不取 runner 已经读到的位置。已经接受的字节在同一次事件处理里交给了 writer（§1.2：先更新进度，再提交写），它们保留不动。
- 胜负由 runner manager 一次决定：分叉之后，第一次从任一方取到非空帧时决定。这一步在吞吐、进度、无进展计时和 writer 看到这一帧之前，也就是在 `handle_data_frame` 更新 `meters` 之前。
- 分叉时原 runner 已经读到、但 manager 还没取走的数据（在它的通道或缓冲里）属于原 runner，从分叉点开始。如果先取到的是其中一帧，就是原 runner 先交来数据：原 runner 胜，这些帧照常写入。对冲先胜时，它们是败方的帧，丢弃。
- 空帧不决定胜负。runner 不发空帧，但 planner 截断之后可能为空。
- 两方都有帧可取时，原 runner 胜。`StreamGroup` 按内部 key 的顺序轮询，不保证先轮询原 runner，所以实现要自己保证：取到对冲的第一帧时，先看原 runner 的通道里有没有帧。
  - “可取”指 manager 能取：原 runner 的 `Started` 已经处理过，它的数据通道里有帧。原 runner 在 manager 看来还在连接时，它的数据接收端在 `connecting_data` 里，帧不算可取，对冲的帧先到就是对冲胜。
  - `StreamGroup`（futures-concurrency 7.7.1）只有 `insert`、`remove`、`contains_key`，不能按 key 取出组里的流。所以原 runner 的数据接收端要放在实现能直接查到的地方。
  - 分叉时 O 往往已经是 Running，它的接收端早已在组里。不能先 `remove` 再放到别处：`remove` 会把接收端连同排着的帧一起丢掉（`DataAggregator::unregister` 就是这样用的）。所以要从普通 runner 起就换存储，两种做法都行：
    - 数据接收端不放进 `StreamGroup`，放在按 runner ID 索引的表里，自己轮询，每个接收端注册自己的唤醒；
    - 或者每个接收端旁边再留一个能查“通道里有没有帧”的句柄。
  - 不管哪种，都要验证：查看或转移接收端时不丢帧，有新帧时循环照常被唤醒。
- 败方的帧一律丢弃，不写入，也不计入吞吐。败方被取消：不算失败，不退避，`attempts` 和 `charged`（§14.13）都不变。

#### 错误

胜负分出之前，按出错的一方和错误类别处理：

| 出错的一方 | 错误 | 处理 |
| --- | --- | --- |
| 一方，另一方还在 | 可重试 | 只结束这一方。区间不退回，不退避，`attempts` 和 `charged` 都不变，也不检查区间预算。按 §14.13 的分类计入任务级连续失败，见表下 |
| 一方，另一方还在 | 拥塞（429/503） | 只结束这一方。区间不退回，不退避，`attempts` 和 `charged` 都不变。另外立即按 §8.1 更新 `congestion_until`，并照常记入并发控制的反馈，和普通 runner 遇到拥塞一样（§14.12）。没有 `Retry-After` 时，延迟取 `backoff(chunk.attempts + 1)`，即这个区间此刻失败会用的退避；之后像 `back_off` 一样按 `next_retry` 提前唤醒。另一方算作在途，所以任务不终止，也不计入连续失败 |
| 任一方 | §4.3 的回退错误 | 照常回退：取消所有 runner，改用单连接 |
| 任一方 | 其它不可重试的错误 | 照常终止任务 |
| 最后一方 | 可重试或拥塞 | 区间按 §8.1 退回，只退避一次：`attempts + 1`；`charged` 最多加 1，规则见“预算”。先查这一对的预算，再按 §14.13 处理最后一方的错误，所以 429/503 也可能以 `RangeExhausted` 结束（见“预算”的判定顺序）。`RangeExhausted` 的 `source` 取这最后一个错误。前一方的 `Retry-After` 已经写进 `congestion_until`，它只推后、不提前（§14.12），所以较长的那个仍然有效 |

任务级连续失败按 §14.13 的分类计数，两方的失败都一样。“收到过数据”按出错的那个物理 runner 自己被接受的字节判断：原 runner 看它领取区间以来被接受的字节，对冲期间也就是 chunk 的已下载部分；对冲 runner 在胜负分出之前总是 0，因为它的第一个非空帧一被取到，胜负就分出了。

- 重启（这个 runner 收到过数据之后的超时、断流或提前结束）：不计入。分叉前交过数据的原 runner 后来超时，就是这种情况。
- 其它可重试的失败：计入，包括没收到过数据的失败、`ChannelClosed` 和可重试的状态码。胜负分出之前失败的对冲 runner 不会是重启。
- 拥塞，并且还有别的尝试在途（另一方也算）：容忍，不计入，这是 `FailurePolicy::on_failure` 现在的做法。没有别的尝试在途时，拥塞照常终止任务，和普通 runner 一样。

胜负分出之后，胜方的帧、切分以外的事件和超时都按普通 runner 处理，败方的事件按下面“状态转换”一节处理。例外是一方先失败、幸存方后来才交数据的情况（下文的状态 C′）：仲裁到此结束，但这次尝试的预算仍按一对算，见“预算”。

#### 预算

- 一个区间的每次尝试最多对冲一次。一次尝试从 runner 领取这个区间开始，到区间完成或被退回为止。
- 一方失败后，这次尝试不再对冲；胜负分出之后也不再对冲。区间被退回、重新领取之后，才可能再对冲。这比 IDM 严：IDM 分出胜负之后，同一个分片还能再对冲。
- 区间上的两个计数见 §14.13：`attempts` 只决定退避多长；`charged` 是每区间预算比较的数。对冲不是切分，两个都不重置：胜方继承这个区间的 `attempts` 和 `charged`。只有切分出的新区间才从 0 开始（§8.1、§14.13）。
- 一对 runner 算一次尝试。先失败的一方不改任何计数，只记下自己的失败类别。最后一方失败时，区间才退回：
  - `attempts + 1`，和 §8.1 一样；
  - `charged` 最多加 1：两方都不是重启，并且至少一方是记预算的失败时，加 1；否则不加。每一方都按 §14.13 的 `charges_range` 分类，“收到过数据”用它自己被接受的字节数（见“错误”表下）；
  - `source` 是最后一方的错误。和 `max_attempts_per_range` 比较的是 `charged`。
- 判定顺序（只对一对，是对 §14.13 顺序的扩展）：最后一方的错误是可重试的或者 429/503 时，先查预算。区间非空、并且这一对算完之后的 `charged` 达到 `max_attempts_per_range`，任务以 `RangeExhausted` 结束，`source` 是最后一方的错误，即使它是 429/503。没达到时，最后一方的错误按 §14.13 的顺序照常处理，其中的 `charged` 一律用这一对的值：429/503 在别的连接还在途时被容忍，照常更新 `congestion_until`；没有别的连接在途时照常终止任务。
  - 为什么要提前：§14.13 里被容忍的 429/503 在查预算之前就返回了（`FailurePolicy::on_failure`）。单个 runner 的 429/503 从不记预算，所以这不成问题；一对里，先失败的一方记预算、最后一方是 429/503 时，`charged` 会加 1，必须在这一步查。
  - 最后一方自己的 `AttemptFailure::charged` 只按它自己的类别算：它是 429/503 时不加；原 runner 先重启、对冲最后以记预算的失败结束时，它会加 1，而这一对应该是 0。所以 T27 在查预算和写冷却记录之前，都要换成这一对的值。
  - §4.3 的回退错误和其它不可重试的错误不走这一步，照常回退或终止任务（见“错误”表）。
- 各种顺序的结果：

  | 先失败 | 后失败 | `charged` |
  | --- | --- | --- |
  | 原 runner 没收到数据就超时 | 对冲 429/503 | +1 |
  | 对冲没收到数据就失败 | 原 runner 重启 | 0，和不对冲时一样 |
  | 原 runner 重启 | 对冲记预算的失败 | 0，区间已经变短 |
  | 429/503 | 429/503 | 0 |
  | 原 runner 交过数据之后 `ChannelClosed` | 对冲 429/503 | +1：`ChannelClosed` 记预算，也不算重启 |

- §14.13 的 `received` 取的是 chunk 已下载部分的长度，planner 不区分是哪个 runner 交的。对冲期间这些字节全是原 runner 交的，对冲 runner 自己是 0。所以 T27 要分别记两方被接受的字节数，不能直接用 `Released::received` 给对冲 runner 分类。
  - 不用另加计数，只要记下分叉点：分叉之后被接受的字节都来自同一方。O 自己的字节是“分叉点 − chunk 起点”，O 幸存或胜出时再加上分叉之后的部分，也就是整个已下载部分；H 自己的字节是“已下载末尾 − 分叉点”，H 交第一个非空帧之前是 0。
- 一方先失败、幸存的一方后来交了数据，也算分出胜负（没有败方要停）。仲裁到此结束，帧照常处理；但先失败一方的类别要留到这次尝试结束（区间完成或退回），幸存方之后的失败仍按一对算。不能当成普通 runner 处理：`ChannelClosed` 即使在交过数据之后也记预算（`failure_policy.rs` 的 `is_restart` 不含它）。比如 O 分叉前交过数据、后来超时（重启），H 交了数据之后丢失（`ChannelClosed`）：按一对算是 0，按普通 runner 算是 +1，在预算临界点上会误报 `RangeExhausted`。
- 两方都在交数据之前失败时，这一对记的预算不少于原 runner 单独失败时记的：原 runner 的失败记预算时，它不是重启，对冲 runner 没交过数据，也不会是重启，所以这一对加 1。状态 C′ 里幸存方交过数据后才重启时，这一对是 0；这时这次尝试有字节被接受，区间已经变短，和单个 runner 的重启一样。
- 不会没有进展地一直循环：
  - 每次尝试最多两个 runner（最多对冲一次），一方失败后不再补发。
  - 有一方是重启时，这一对不记预算。任一方被算成重启，都说明它自己有字节被接受，也就是这次尝试有字节被接受，所以退回的区间严格变短，和 §14.13 的论证一样。重启的可以是分叉前交过数据的 O，也可以是状态 C′ 里交过数据的幸存方（O 或 H）。
  - 没有重启、至少一方记预算时，`charged + 1`，受 `max_attempts_per_range` 约束。
  - 剩下的只有两方都是 429/503：和 §14.13 一样，只有别的连接还在途时才被容忍，`no_progress_timeout` 照常有效。

#### 状态转换（T27 开工前补齐）

这一节按 C12（`ee092fc`）之后的代码，写一个逻辑 chunk 挂两个 runner 时的状态转换，评审通过再实现。O 是原 runner，H 是对冲 runner。没写目录的文件都在 `crates/bolt-load/src/task/instance/concurrent_task/` 下，runner 的文件在 `crates/bolt-load/src/runner/` 下。

不管实现怎么写，都要守住：

- 每一方都只释放自己的资源和槽位，败方也一样。槽位在确认这个 runner 已经终止、或者没能启动之后，恰好回收一次。确认的方式有三种：它报告了 `Stopped`；它的生命周期通道没发 `Stopped` 就关闭了；它的 spawn 被拒绝。
- 败方不会完成、也不会释放这个逻辑区间。
- 败方迟到的事件（`Started`、数据、`Stopped`、失败）一律忽略，不去 planner 里查它的 chunk，更不当成失败处理。
- 活跃 runner 的计数两方都算。现在策略用的 `get_active_runners_count` 数的是活跃的 chunk，一对只算一个。

##### 代码现状

表里要处理的现状（C9）：

- 现在没法单独停掉一个 runner：所有 runner 共用任务的 cancel token。能做的只有丢掉它的 limit sender，它会以 `ChannelClosed` 停下；生命周期处理把这当成可重试的失败，先调用 `mark_failed`。runner 因取消停下时，处理也是先 `mark_failed`，再跳过失败上报（`runner_manager/lifecycle.rs`）。
  - C12 之后：每个 runner 有自己的停止开关（任务令牌的子令牌），`RunnerManager::stop_runner` 只停它一个；被停的 runner 报 `Cancelled` 时按低速重启处理，其余 runner 被取消时仍然跳过失败上报（§14.15）。已经决定怎么结束的 runner（拿着错误或已下完、在交最后一段缓冲或等帧被取走）不理这个开关。对冲的败方可以用同一个开关停下，但 T27 要记下停止的原因（低速重启还是对冲败方），manager 按原因处理，不能把被停之后的 `Cancelled` 一律当低速。
- spawn 被拒绝现在按不可重试的连接失败处理，任务终止（§14.11），处理时同样会 `mark_failed`。
- 生命周期通道没发 `Stopped` 就关闭时，`handle_runner_lost` 也是先 `mark_failed`，再释放槽位；任务还没完成时，上报一次 `ChannelClosed` 的可重试失败（`runner_manager/lifecycle.rs`）。

下面这些也按 C12 的代码核对过，表里要用到：

- planner 一个 chunk 只记一个 `runner_id`，`runner_to_chunk` 也是一个 runner 对一个 chunk。`update_progress`、`mark_running`、`mark_finished`、`mark_failed` 都按 runner ID 找 chunk（`chunk_planner.rs`）。manager 调用它们时都带 `expect`，找不到就 panic（`runner_manager/lifecycle.rs`、`runner_manager/data.rs`）。所以 H 的事件，以及不再是 owner 的一方的事件，都不能原样走这些路径。
- 处理 `Stopped` 时，manager 先调用 `release_runner`：ID、limit sender、生命周期接收端、数据接收端、停止开关和 `stopping` 项一起释放（`runner_manager.rs`）。生命周期接收端没了，manager 就再也收不到这个 runner 的事件，也不会再有它的通道关闭事件。
- 连接中的 runner，数据接收端放在 `connecting_data` 里，处理 `Started` 时才移进数据聚合器，这一步 `expect` 它还在（`runner_manager/lifecycle.rs`）。
- `PublishingStarted` 阶段不看停止开关（`run_loop.rs` 的 `poll_step`）。所以连接中被停的 runner，可能先报 `Started`，再报 `Cancelled`。
- runner 被取消（任务令牌或自己的停止开关）时不等 drain，直接报 `Stopped`（`run_loop/drain.rs` 的 `should_wait_data_drain`）。完成、流出错和超时要先等数据通道里的帧被取走。
- manager 丢掉一个 runner 的数据接收端之后：runner 交帧时 `poll_ready` 返回 false，以 `ChannelClosed` 结束（`run_loop/emit.rs`）；runner 在 drain 时，`poll_drained` 立即返回，报它原来的结果（`run_loop/drain.rs`）。依据是 async-ringbuf 0.3.10 的 `is_closed`：消费端被丢掉后为真，它的 `drop_close_cons` 测试验证了这一点。所以丢掉败方的数据接收端、再拉下它的停止开关，败方在哪个阶段都会很快停下：连接和读流时靠停止开关，交帧和 drain 时靠通道关闭。
- `update_progress` 收满 chunk 时把它改成 `Finished`，runner 这时还没停。`get_active_runners_count` 不再数它，它的 ID 要到 `Stopped` 才回收（§14.11）。
- `resize_runner_state` 发给 runner 的上限是“新末尾 − chunk 起点”（`chunk_planner.rs`）。runner 从自己请求的起点开始数字节，而 H 的请求从分叉点开始，两者差“分叉点 − chunk 起点”。runner 收到不小于当前上限的新上限时报 `ExceededTotalSize`（`runner/control.rs` 的 `apply_control`），manager 把它当完成处理（`handle_lifecycle`）。
- `on_failure` 的 `in_flight` 是 `allocated_runner_count()`，也就是占着 ID 的 runner 数。失败的 runner 在这之前已经释放（`download_loop/runner.rs`）。
- 下载循环只在 `handle_event` 里取 `now`。manager 在轮询里处理数据帧，拿不到 `now`；`DownloadedChunk` 也不带 runner ID。
- `RunnerRates::sample` 只采样 chunk 的 owner（`active_chunks` 的 `runner_id`）。`Doubling` 在“有 runner 在连接、或者这个 tick 没有数据”时不扩张（§14.14 的检查 5，即 `all_delivering`）。所以只要有一个 runner 静默了一个 tick，扩张就停着。

##### 新增的记录

- chunk 上，建议放进 `ChunkState`，这样 `PlannerGuard` 回滚时一起恢复：
  - 仲裁和预算分开记，因为两者结束的时刻不同：
    - `pair`，仲裁用：O 和 H 的 ID、H 的状态（Pending 或 Running）。一方出局时缩成只记幸存方的单方记录（不再记出局一方的 ID），T13 和扩张仍按它跳过幸存方；第一个非空帧到来时清掉，不论是两方都在时分出胜负，还是幸存方交来第一帧。
    - `hedge`，预算用：`Option<{ fork, first, owner_side }>`。`fork` 是分叉点，`first` 是先失败一方的类别（是不是重启、记不记预算），没有一方失败过时为空。`owner_side` 是当前 owner 是哪一方（`Original` 或 `Hedge`），发起对冲时是 `Original`，“交给 H”时改成 `Hedge`。最后一方失败时按它选 owner 自己的字节：`Original` 用 `Released::received`，`Hedge` 用“已下载末尾 − `fork`”。这里只记哪一方，不记 runner ID，因为 ID 回收后会被重用。发起对冲时写入，一直留到这次尝试结束：chunk 收满，或者区间被退回。它在，就说明这次尝试已经对冲过。区间退回后被重新领取、或者切分出新 chunk 时，都是新的 `ChunkState`，从空开始。
  - chunk 的 `runner_id` 仍是唯一的 owner，`runner_to_chunk` 也只映射 owner。
  - 另有一张 H → chunk 起点的表 `hedge_of`。它只收录还活着、而且不是 owner 的 H。H 出局、H 变成 owner（H 胜，或者 O 先出局）、H 变成败方、H 的 spawn 被拒绝，这四种情况都先删掉 H 的项。`runner_to_chunk` 也一样：O 不再是 owner 时（“交给 H”）先删掉 O 的项。这两步都在“回收”或“停败方”之前做，所以一个 ID 被回收、再分配给别的 runner 时，两张表里都已经没有它的旧项，新 runner 的事件不会被导向旧的一对。
- manager 上：
  - `stopping` 从 `HashSet` 改成 `HashMap<RunnerId, StopReason>`，`StopReason` 是 `Slow` 或 `Loser`，见“停止的原因”。
  - `last_delivery: HashMap<RunnerId, Instant>`，静默的起点：
    - spawn 时写一次，时刻取 `spawn_runner`、`split` 和发起对冲时传进来的 `now`；
    - 循环在 `on_runner` 里接受非空帧时，用 `handle_event` 的 `now` 更新，所以 `DownloadedChunk` 要带上 runner ID；
    - writer 从满变成可写时，全部改成 `now`。不能在处理 `WriterCompleted` 前后比较 `can_write()`：`poll_writer` 调用的 `PendingWriter::poll_completions` 在 `settle` 里就释放了写槽、提交了排着的写入（`file_writer/pending_writer.rs`），处理事件时 writer 已经可写，前后都是 `true`。所以 `poll_writer` 在调用 `poll_completions` 之前记下 `!can_write()`，随 `WriterCompleted` 事件带出“之前满过”；`handle_event` 看到它，就用自己的 `now` 全部重置；
    - `release_runner` 时删掉。
- 每一方被接受的字节不用另加计数，由 `hedge.fork` 算出，见“预算”。

##### 状态

| 状态 | chunk 的 owner | 两个 runner | 计数与字节 | 活跃数 | 占用的槽位 |
| --- | --- | --- | --- | --- | --- |
| A 分叉前 | O | O 是 Pending 或 Running，没有 H | `attempts`、`charged` 是领取时带来的；O 的字节等于已下载部分 | 1 | 1 |
| B 未决 | O | O、H 都在，各自是 Pending 或 Running | 和 A 一样；已下载部分停在分叉点，H 的字节是 0 | 2 | 2 |
| C 一方已失败 | 幸存的一方 X | 先失败的一方已回收 | 和 A 一样；`pair` 是只记 X 的单方记录，`hedge.first` 记着先失败一方的类别 | 1 | 1 |
| C′ 一方失败后幸存方交了数据 | X | X 照常下载 | 和 C 一样；`pair` 已清掉，`hedge.first` 还在 | 1 | 1 |
| D 已决 | 胜方 W | W 照常下载；败方 L 在停止 | W 继承 `attempts`、`charged`；`hedge.first` 为空 | 1，不数 L | W 一个；L 另占一个，直到它确认终止 |
| E 两方都失败 | 无，区间已退回 | 都已回收 | `attempts + 1`，`charged` 按“预算”加 0 或 1，区间冷却 | 0 | 0 |
| F 完成 | W，chunk 是 `Finished` | W 在 drain 或已停 | 不变；`hedge` 随这次尝试结束 | 0 | W 确认终止后回收 |

- 败方 L 不属于任何状态：它只是 `stopping` 里的一项 `Loser`，占着一个槽位，直到确认终止（见“停止的原因”）。chunk 之后怎么变（W 失败、区间退回、被新的尝试领取、收满），都不影响它；它的终止也不改 chunk 的状态。
- D 和 C′ 里 W（或 X）之后的失败，都按“预算”里一对的规则算 `charged`；D 里 `hedge.first` 为空，结果和普通 runner 一样。
- E 之后区间被重新领取，是新的尝试，可以再对冲。

##### 转换

记号：

- “回收 X”：`release_runner(X)`，恰好一次。它释放 X 的 ID、limit sender、生命周期接收端、数据接收端和停止开关，并删掉 `meters`、`last_delivery`、`stopping` 里 X 的项。不碰 planner。调用之前，X 在 `runner_to_chunk` 和 `hedge_of` 里都已经没有项。
- “停败方 L”：先从 `runner_to_chunk` 或 `hedge_of` 删掉 L；`stopping[L] = Loser`；拉下 L 的停止开关；拿掉 L 的数据接收端，排着的帧随之丢掉；删掉 `meters` 和 `last_delivery` 里 L 的项。L 的 ID、limit sender 和生命周期接收端留着，等它确认终止时再“回收 L”。
- “交给 X”：先删掉旧 owner 在 `runner_to_chunk` 的项、X 在 `hedge_of` 的项，再把 chunk 的 owner 改成 X、`runner_to_chunk` 加上 X，chunk 的状态改成 X 的状态（Pending 或 Running）。`hedge.owner_side` 改成 X 那一方。已下载部分、`attempts`、`charged`、`generation`、`hedge.fork` 和 `hedge.first` 都不变。
- “X 出局”：状态 B 里一方 X 失败、另一方 Y 还在。X 是 owner 时交给 Y，否则从 `hedge_of` 删掉 X；然后回收 X；`pair` 缩成只记 Y 的单方记录，不再记 X 的 ID（这时它只用来让 T13 和扩张跳过 Y，直到 Y 交来第一个非空帧时清掉）；`hedge.first` 记下 X 的类别：是不是重启（按 X 自己的字节），记不记预算（`charges_range`）。planner 不释放区间，不退避，`attempts` 和 `charged` 不变，不查区间预算。

触发（A → B）：

- 时机：每个策略 tick，在低速重启和策略动作（并发控制的 `CreateTask`、`Doubling` 的扩张）之后。它们先用槽位，发起对冲的函数再查一遍条件 1 和 2。
- 先占一个槽位，占不到就什么都不改。
- spawn H，请求 `[分叉点, allocated.end)`，带自己的停止开关。不在 planner 里分配区间（`spawn_in` 会分配，要另写一个），只记 `pair`、`hedge = { fork: 分叉点, first: 空, owner_side: Original }` 和 `hedge_of[H]`，并且 `last_delivery[H] = now`。
- 计数、O 的字节、O 的空闲超时和无进展截止时间都不变。
- spawn 被拒绝时，照现在的做法排进 `spawn_failures`，状态仍是 B，由 B 表里“H 的 spawn 被拒绝”一行处理。

状态 B（未决，两方都在）：

| 事件 | 下一状态 | 处理 |
| --- | --- | --- |
| O 交来非空帧 | D，W = O | 先停败方 H，清掉 `pair`。再照常处理这一帧：`meters`、`update_progress`、写入、`on_progress`，`last_delivery[O] = now`。计数不变 |
| H 交来非空帧，O 不可取：O 还在连接，或 O 的数据通道是空的 | D，W = H | 交给 H（Running）；停败方 O；清掉 `pair`；再照常处理 H 的帧。O 在分叉前交的字节留在 chunk 里。计数不变，由 H 继承 |
| H 交来非空帧，但 O 的通道里已经有帧；或者同一次轮询里两方都有帧 | D，W = O | H 的这一帧丢掉：不写入，不计 `meters`。停败方 H，清掉 `pair`。O 的帧照常取 |
| 空帧 | B | 不会出现：分叉后 chunk 还有剩余，帧从已下载的末尾接上，截断后仍不为空。出现了也不决定胜负 |
| O 的 `Started` | B | 照常：`mark_running(O)`，数据接收端移进聚合器 |
| H 的 `Started` | B | `pair` 里的 H 改成 Running，数据接收端移进聚合器。不调用 planner 的 `mark_running` |
| 任一方 X 报 `Finished`，或报当作完成的 `ExceededTotalSize` | C | 不会出现：runner 要等帧被取走才报完成，而取走第一帧就分出了胜负；B 里也不会 resize。出现了就按 X 以 `ChannelClosed` 失败处理，和 `handle_lifecycle` 里“连接中报完成”一样 |
| X 报 `Cancelled`，`stopping` 里没有它 | C | 只有任务取消才会这样，那时循环先处理取消事件，整个结束。万一先处理到：X 出局，不算失败，`hedge.first` 记为既不是重启、也不记预算 |
| X 可重试的失败，是重启。只能是分叉前交过数据的 O：超时、断流或提前结束 | C | X 出局，`hedge.first` 是重启。不计连续失败。结果采样照常记一次可重试的失败 |
| X 可重试的失败，记预算：连接失败、没有数据的超时或断流、可重试的状态码、`ChannelClosed` | C | X 出局，`hedge.first` 是记预算。连续失败加 1，到上限时任务以 `RetryLimitExceeded` 结束；不查区间预算。结果采样照常 |
| X 收到 429/503 | C | X 出局，`hedge.first` 既不是重启、也不记预算。按 §8.1 推后 `congestion_until`：有 `Retry-After` 时取它和退避中较长的，没有时取 `backoff(chunk.attempts + 1)`，上限 120 s，只推后不提前；然后像 `back_off` 一样按 `next_retry` 提前唤醒。结果采样记一次不可重试的失败，下一个 step 会降并发。不计连续失败 |
| X 的 §4.3 回退错误 | 结束 | 照常回退到单连接（`range_unsupported`） |
| X 的其它不可重试的错误 | 结束 | 照常终止任务 |
| X 的生命周期通道没发 `Stopped` 就关闭 | C | 按 X 以 `ChannelClosed` 失败处理：X 出局，`hedge.first` 是记预算，连续失败加 1 |
| H 的 spawn 被拒绝 | 结束 | 从 `hedge_of` 删掉 H，回收 H 一次，清掉 `pair`，O 的区间不释放。然后终止任务：被拒绝的 spawn 现在就按不可重试的连接失败（`TaskError::Other`）处理（§14.11），H 也一样（见“已决定”的 M4）。它总在分叉后的任何数据之前处理，因为 `poll_lifecycle` 最先报 spawn 失败 |
| writer 满 | B | 不取任何数据，所以分不出胜负。生命周期照常，上面的失败照常处理 |
| writer 恢复可写（`WriterCompleted` 带着“之前满过”） | B | `last_delivery` 全部改成 `now` |
| 任务取消 | 结束 | 循环以 `Cancelled` 返回；`finish` 取消任务令牌，所有 runner 报 `Cancelled`，但 manager 已经丢掉，不再处理。不退避，计数不变 |
| T13 的评判 | B | O 和 H 都不评判，也不算进中位数（`may_restart`） |
| 扩张选 chunk | B | 跳过这个 chunk（`largest_running_chunk` 的 `skip`） |
| 无进展截止时间到 | 结束 | 照常以 `NoProgress` 结束，对冲没有推迟它 |
| 策略 tick 的对冲判定 | B | 这个 chunk 已对冲过（`hedge` 在），O 和 H 都不是候选 |

状态 C（一方已失败，幸存的 X 是 owner）：

| 事件 | 下一状态 | 处理 |
| --- | --- | --- |
| X 交来非空帧 | C′ | 清掉 `pair`，照常处理这一帧。`hedge.first` 留着，见“预算” |
| X 的 `Started` | C | X 是 O 时，`mark_running(O)`。X 是 H 时，H 已经是 owner，`mark_running(H)`。数据接收端移进聚合器 |
| X 报 `Finished` | E | 不会出现，理由同 B；出现了按 `ChannelClosed` 处理，见下一行 |
| X 可重试的失败、429/503，或生命周期通道关闭（`ChannelClosed`） | E | 最后一方，按下面“最后一方失败”的顺序处理 |
| X 报 `Cancelled`，`stopping` 里没有它 | E | 只在任务取消时出现。照现在的做法 `mark_failed`，不报失败 |
| X 的 §4.3 回退错误、其它不可重试的错误 | 结束 | 照常回退或终止任务 |
| writer 满或恢复可写、任务取消、无进展截止时间到 | 同 B | 同 B |
| T13 的评判、扩张选 chunk、对冲判定 | C | 同 B：X 不评判，chunk 不切，不再对冲 |

状态 C′（一方失败后，幸存的 X 交了数据）：

| 事件 | 下一状态 | 处理 |
| --- | --- | --- |
| X 的数据帧 | C′ | 照常处理 |
| X 下完，报 `Finished` | F | 照常 `mark_runner_completed`；`hedge` 随这次尝试结束 |
| X 的任何失败，包括 T13 的低速停止 | E | 最后一方，按下面“最后一方失败”的顺序处理。X 自己的字节此时大于 0 |
| X 的 §4.3 回退错误、其它不可重试的错误 | 结束 | 照常回退或终止任务 |
| T13 的评判 | C′ | `pair` 已清掉，X 可以被评判；但剩余量 < `2 × min_chunk_size`，实际不会被停 |
| 扩张选 chunk、对冲判定 | C′ | `hedge` 还在：不切，不再对冲 |
| writer、任务取消、无进展截止时间到 | 照常 | 同 B |

最后一方失败（C、C′ 里 X 失败，以及 D 里 W 失败）：

1. 先存下分类要用的东西：`hedge.owner_side`、X 自己的字节、`hedge.first`。X 自己的字节：`owner_side` 是 `Original` 时用 `Released::received`；是 `Hedge` 时用“已下载末尾 − `hedge.fork`”，在 C 里是 0。
2. `mark_failed(X)` 退回 `[downloaded.end, allocated.end)`，`attempts + 1`，清掉 `hedge` 和 `pair`（有已下载部分时，chunk 截成 `Finished` 留下，不再带它们）。然后回收 X，和现在 `handle_lifecycle` 先 `release_runner`、再生成失败一样。
3. 这一对的 `charged` 增量：`hedge.first` 和 X 都不是重启、并且至少一方记预算时为 1，否则为 0。`hedge.first` 为空时（D 里的 W），就是 X 自己按 `charges_range` 的结果，和普通 runner 一样。`AttemptFailure::charged` 换成 `Released::charged` 加这个增量。
4. X 回收之后，下载循环才算“别的在途尝试”：`allocated_runner_count()` 减去 `stopping` 里的项（`Loser` 和 `Slow` 都减，见“已决定”的 M3）。X 自己已经不在里面。
5. 按“预算”的判定顺序处理：可重试或 429/503 时先查预算，再按 §14.13 调用 `on_failure`；连续失败按 X 自己的类别。比如 O、H 先后收到 503，预算没耗尽，也没有别的连接：第 4 步算出 0，503 不被容忍，任务以这个 503 结束。
6. 任务继续时 `back_off`，用 `attempts` 和这一对的 `charged`。

状态 D（已决，W 是 owner，L 在停止）：

| 事件 | 下一状态 | 处理 |
| --- | --- | --- |
| W 的数据帧、`Finished`、T13 的评判 | 照常 | 照 §14.11–§14.15 处理，`stopping` 里没有 W |
| W 的失败 | E | 按上面“最后一方失败”的顺序处理，`hedge.first` 为空，结果和普通 runner 一样 |
| 扩张选 chunk、对冲判定 | D | `hedge` 还在：不切，不再对冲（见“排除”） |
| writer 满或恢复可写、任务取消、无进展截止时间到 | 照常 | 和普通 runner 一样 |

败方 L（不随 chunk 的状态变化，见“停止的原因”）：

| 事件 | chunk | 处理 |
| --- | --- | --- |
| L 的 `Started` | 不变 | 只忽略，不回收：这不是终止的确认。L 的数据接收端已经丢掉；不碰 planner，也不 `expect` |
| L 的数据帧 | 不变 | 收不到：数据接收端已经丢掉。实现里仍要先按 `stopping` 分流，不调用 `update_progress` |
| L 报 `Stopped`，不论原因：`Finished`、`Cancelled`、可重试的错误、429/503、§4.3 回退错误、其它不可重试的错误 | 不变 | 回收 L 一次。不碰 planner，不报失败，不记结果采样，不退避，不改 `congestion_until`，计数不变。不管这时 W 还在下载、已经失败并退回了区间、区间已被新的尝试领取，还是 chunk 已收满，都一样 |
| L 的生命周期通道没发 `Stopped` 就关闭 | 不变 | 同上 |
| 任务完成，L 还没停 | — | 循环照常结束。`finish` 取消任务令牌并等 wait group，所以 L 停下之后任务才返回。L 的事件不再处理 |

- E：区间按 §8.1 冷却。冷却结束后，由唤醒或 `CreateTask` 重新领取，是新的 `ChunkState`：`hedge` 为空，带着 `attempts` 和 `charged`。
- F：W 报 `Finished` 时照常 `mark_runner_completed`，planner 完成时任务结束。

##### 停止的原因

- C12 的 manager 把“被停、报 `Cancelled`”一律当低速停止（`handle_lifecycle` 里的 `stopped_slow && kind.is_cancelled()`）。对冲的败方不能这样处理：
  - 败方不是 chunk 的 owner。O 输了时，owner 已经交给 H；H 输了时，H 从来不在 `runner_to_chunk` 里。
  - 走低速路径会对它调用 `mark_failed`，找不到 chunk 就 panic；即使找得到，也会释放胜方正在下载的区间。
- 所以 `stopping` 要记下原因：
  - `Slow`：由 `stop_runner`（T13）写入。处理和 C12 一样：报 `Cancelled` 时按低速停止，chunk 已收满时按完成，报自己的错误时按它的错误。
  - `Loser`：分出胜负时由新的 `stop_loser` 写入。它不要求 Running，也不要求还有剩余：败方可能还在连接，也可能已经读完、正在 drain。
- `handle_lifecycle`、`handle_runner_lost`、`take_spawn_failure` 和 `poll_data` 都先看 `stopping`。原因是 `Loser` 的，什么都不报：不查 `is_connecting`，不调用 planner，不记结果采样。只有终止的确认（`Stopped`、通道关闭、spawn 被拒绝）才回收它，而且只回收一次；`Started` 只忽略。
- 败方的清理只看 `stopping`，不看 chunk：W 还在下载、W 已经失败并退回了区间、区间已被新的尝试领取、chunk 已收满，处理都一样。
- `stopping` 的两种原因都从 `on_failure` 的在途数里减掉（M3）。
- 不会释放区间：`Loser` 分支不调用 planner 的任何方法。
- 只回收一次：三种确认互斥。
  - 处理 `Stopped` 时调用 `release_runner`，它丢掉生命周期接收端，之后不会再有这个 runner 的通道关闭事件；
  - 处理通道关闭时也调用它，之后也不会再有 `Stopped`；
  - spawn 被拒绝的 runner 没有通道，也就没有前两种事件。
  - `release_runner` 同时删掉 `stopping` 的项，ID 再分配给新 runner 时不带旧原因，C12 已经这样做。
- 迟到的帧：分出胜负时就丢掉了败方的数据接收端，排着的帧随之丢掉。之后不会再有它的帧，也不会有它的数据通道关闭事件，因为它已经不在组里。
- `may_restart` 和 `largest_running_chunk` 的 `skip` 看的“正在停止”，两种原因都算。

##### 排除

- T13 不评判一对中的任何一方：
  - `may_restart`（`download_loop/restart.rs`）在“不在停止中”之外，再要求它不在任何 `pair` 里：B 的两方，以及 C 里单方记录的幸存方。`slow_runner` 只拿到过滤后的列表，所以它们也不算进中位数和“至少 3 个”的人数（`slow_restart.rs` 在看候选的剩余量之前就用整个列表算这两样）。
  - H 本来也不会被评判：`RunnerRates` 只采样 owner，H 也没有完整的窗口。这里的排除是兜底。
  - 分出胜负之后，W 是普通 runner，可以被评判。但候选的剩余量 < `2 × min_chunk_size`，正好不满足 T13 的剩余量条件，所以实际上不会被停。
  - `stop_runner` 自己也拒绝有 `pair` 的 chunk 的 owner，再兜一层。
- 扩张和切分不选对冲过的 chunk。`largest_running_chunk` 的 `skip` 在“正在停止”之外，再跳过 `hedge` 不为空的 chunk，也包括 C′、D 里的 chunk。理由：
  - B、C 里切分会缩小 O 的上限，H 的请求却仍到原来的末尾，多下的部分会被截掉。
  - W 是 H 时，`resize_runner_state` 按 chunk 起点算上限，比 H 能收的多出“分叉点 − chunk 起点”。新上限不小于 H 当前的上限时，H 报 `ExceededTotalSize`，manager 把它当完成，chunk 没收满就被标成 `Finished`，数据就丢了。
  - 这样的 chunk 分叉时剩余量就不到 `2 × min_chunk_size`，只会越来越小，不切的损失不大。
  - （C27 时：`Legacy` 的 `SplitAllTask`、`SplitGivenTask` 和 `find_chunk_to_split` 不用改，`Legacy` 从不对冲。它们在 C14 已随 `Legacy` 删除，见 §14.20。）planner 的 `resize_runner_state` 对 `hedge` 不为空的 chunk 直接拒绝，再兜一层。
- 和 C11 对齐（触发条件 1）：
  - §9.6 原来要求所有未完成的 chunk，包括候选自己的，剩余量都 < `2 × min_chunk_size`。C11 实际的切分门槛是 `planned = max(donor_rate × 2 s, min_chunk_size)`，剩余量 ≥ `planned` 才切（`strategy_tick.rs` 的 `grow` 和 `planned_split_size`）。两者有两处不一致：
    - donor 快时，`planned` 大于 `2 × min_chunk_size`。比如 donor 10 MiB/s 时 `planned` 是 20 MiB：一个剩 5 MiB 的 chunk，C11 不会切（它 0.5 s 就下完），原写法却算“还有活”，挡住了对冲。
    - donor 慢于 1 MiB/s 时（`min_chunk_size` 默认 1 MiB），`planned` 在 `min_chunk_size` 和 `2 × min_chunk_size` 之间。剩余量落在这一段的 chunk，C11 会切，原写法却算“没活”。
  - 推荐：别的 chunk 用 C11 的门槛，候选自己的 chunk 用 `2 × min_chunk_size`。理由：
    - T27 跟随 `Doubling`。“可切”就该是 `Doubling` 认为值得切的，用同一个函数 `planned_split_size`，不另立标准。
    - 候选自己的 chunk 不能用 C11 的门槛：它静默，速率接近 0，门槛退到 `min_chunk_size`。剩 1–2 MiB 的静默 chunk 会因为自己“可切”而挡住自己的对冲。切它也没用，前一半还留给卡住的 runner。
    - 候选用 `2 × min_chunk_size`，正好接上 T13 管不到的那一段（§9.5 条件 2），这就是 D5 说的“收尾版”的范围。
    - Pending 的 chunk 向 `planned_split_size` 传 `None`，用 `per_runner_speed` 代替（见触发条件 1）。不能直接用 `RunnerRates` 的值：连续两次采样都在连接的 runner，速率是 `Some(0.0)`，门槛会退到 `min_chunk_size`。这样 T27 验证里的“剩余量够大的 Pending chunk”才按它连上之后的速度判断。C11 自己从不切 Pending 的 chunk，这里只是借用门槛。

##### 缺口与建议

已经在上文定下、不需要另外决定的：

1. 对冲在什么时候判定，§9.6 没写。这里定为每个策略 tick 判定一次。所以实际在静默满 2 s 之后的第一个 tick 发起，也就是静默 2–3.5 s 之间。这和 `Doubling` 只在 tick 上扩张一样（§14.14）；测试显式传入 `now`，“差 1 ns 不发起”照样能测。
2. 记静默时刻要知道是哪个 runner 交的帧：`DownloadedChunk` 加上 runner ID，在 `on_runner` 里用 `handle_event` 的 `now` 记。
3. 先看原 runner 的通道：`StreamGroup` 不能按 key 取流，见“仲裁”。
4. 先失败一方的 429/503 没有 `Retry-After` 时，`congestion_until` 用 `backoff(chunk.attempts + 1)`，见“错误”表。
5. 每一方的字节不用另加计数，由 `hedge.fork` 算出；但先失败一方的类别 `hedge.first` 要留到这次尝试结束，见“预算”和“新增的记录”。
6. 胜方是 H 时，切分会算错上限，见“排除”：用“扩张不选对冲过的 chunk”避开。另一种做法是在 chunk 上记 owner 请求的起点，按它算上限，改动更大，只有维护者想让对冲过的 chunk 还能切时才需要。
7. writer 恢复可写要在 `poll_completions` 之前判断，见“新增的记录”的 `last_delivery`。
8. Pending 的 chunk 用 `per_runner_speed` 算门槛，不用 `RunnerRates` 的 `Some(0.0)`，见触发条件 1。
9. `hedge_of` 和 `runner_to_chunk` 的旧项在回收 ID 之前删掉，见“新增的记录”。
10. 败方的清理不随 chunk 的状态变化，见“败方 L”表。

已决定（2026-11-07，用户确认）：

- **M1 收尾版的范围：保持 D5，不放宽。** 有两种情况，对冲和 T13 都不管，只能等 runner 的空闲超时（默认 30 s）。两种都写进 T14 的第 4 个场景去量：
  - 静默的 runner 剩不到 `2 × min_chunk_size`，但别的 chunk 还“可切”。这时 `Doubling` 也不扩张，因为它要求每个 runner 在这个 tick 都有数据（检查 5），所以一个卡住的小 chunk 会让扩张也停到空闲超时。C29 起检查 5 不算静默满 `hedge_idle` 的 runner，扩张照常，别的 chunk 更快下完，收尾一到就对冲（§14.19）；这个 runner 本身仍只归对冲管。
  - 静默的 runner 剩不少于 `2 × min_chunk_size`，但有完整窗口的 runner 不到 3 个，T13 不评判。收尾时常常只剩两三个 runner。C29 起由 T13 的静默路径重启（§9.5、§14.19）。
  - 理由：先按 D5 做收尾版，量出代价再决定放不放宽。放宽的方向是去掉条件 1 里“其它 chunk”那一条，或者去掉候选的剩余量上限。
- **M2 败方迟到的错误一律忽略。** 包括 §4.3 回退错误、其它不可重试的错误（比如 404、`RangeMismatch`）和 429/503：都不回退、不终止任务、不降并发、不推后 `congestion_until`。理由：这是不变量的要求；胜方正在同一个服务器上正常下载，败方的数据也没有写入。
- **M3 传给 `on_failure` 的在途数减去所有正在停止的 runner，`Loser` 和 C12 的 `Slow` 都减。** 理由：正在停止的 runner 不会再交数据，不是在途的尝试；不减的话，一个快要停下的 runner 就能让别的 runner 的 429/503 被容忍，之后只剩 `no_progress_timeout` 兜底。
  - 这也改变了 C12 的行为：以前慢 runner 在报 `Stopped` 之前一直算在途；T27 起不算。比如只剩一个被停的慢 runner 时，另一个 runner 收到 503，以前被容忍，现在任务以这个 503 结束（§14.15 已注明）。
  - 失败的那个 runner 自己也不算：“最后一方失败”的第 4 步在回收它之后才数，普通 runner 也一样（现在 `handle_lifecycle` 就是先 `release_runner`）。
  - 败方不算活跃但占槽位，降级之后实际连接数可能短暂超过新的上限。已经下完、还没报 `Stopped` 的 runner 现在也是这样（§14.11），接受。
- **M4 对冲的 spawn 被拒绝时终止任务。** 理由：被拒绝的 spawn 现在就按不可重试的连接失败处理（§14.11），H 不例外。被拒绝不一定是 runtime 在关闭：`ThreadedRuntimeImpl::Other` 会转发自定义 runtime 的拒绝，测试用的 `FailOnSpawnRuntime` 只拒绝指定的那一次。所以理由是“和现有分类一致”，不是“反正 runtime 要关了”。
- **M5 冷却中的区间不算“可切的活”，接受。** 条件 1 只看可领取的区间。最坏的情况：H 占了最后一个槽位，之后冷却结束，而 O 和 H 都一直静默。没有分出胜负，就没有败方腾出槽位；下一个 tick 也没有空闲槽位。这个区间要等到下面最早的一件事：
  - 别的 runner 下完或失败；
  - O 或 H 自己结束。每一方按自己所处的阶段计时（`run_loop/connect.rs`、`run_loop/stream.rs`）：还在连接时是连接超时（默认 30 s，从第一次被轮询算）；连上之后重新开始空闲超时（默认 30 s），上游每来一个非空的字节块就重置，不管它有没有攒够一帧交出来。所以一方可以先用掉将近 30 s 连上，再用 30 s 才空闲超时，两段串起来；上游一直有零星字节时，空闲超时根本不会到；
  - 任一方交来第一个非空帧：分出胜负，败方很快停下、腾出槽位；
  - 任务的无进展超时（默认 60 s，从最后一次有字节被接受算）。别的 runner 一直有进展时，它会一直往后推。
  - 只有两方都已经在读流、而且上游一个字节都不再来时，才是“最多约 30 s”（O 的空闲超时从它最后一次收到上游字节算）。一般情况下没有固定的上限，最终由上面这些计时器之一、或者无进展超时结束任务来收尾。
  - 理由：这种情况要求两方同时卡住，而且没有别的 runner 腾出槽位。O 自己的 chunk 不对冲也要等 O 的计时器；多出的代价是冷却中的区间跟着等，而不对冲时它能马上用上那个槽位。
  - 便宜的改法（只作选项，不实现）：发起对冲时，要求空闲槽位数大于冷却中区间的个数，给每个冷却中的区间留一个槽位。代价是冷却期间（最长 30 s）不对冲。

其它影响，不需要决定：

- B 里 H 在连接，`RunnerRates` 看不到它，所以 `Doubling` 的检查 5 不会因为它跳过扩张；活跃数却算上了它，会让 `Doubling` 的 `allowed` 多 1。收尾阶段本来就没有可切的 chunk，影响很小。
- 发起对冲让活跃数加 1，分出胜负又减 1。如果正有一次翻倍等着判断，减 1 会让这次判断作废（§14.14），和别的 runner 停下一样。C28 起，翻到上限的那次翻倍不作废：继续等，期间不扩张；上限以下的照旧作废（§14.18）。

---

## 10. 跨任务限额（P2，按 D4 决定是否实施）

### 10.1 注入方式

```rust
pub struct Limits { /* admission, per-host semaphores, token bucket, host caps */ }
impl TaskBuilder { pub fn limits(self, limits: Arc<Limits>) -> Self; }
```

不注入时，行为与现在完全一致。不新增全局单例。

### 10.2 准入与按 host 的连接数

| 能力 | 实现 |
| --- | --- |
| `max_active_tasks` | 异步信号量。`spawn` 之后，任务在 `Initializing` 阶段先获取许可，按 FIFO 准入 |
| `max_connections_per_host` | `HashMap<Host, Arc<Semaphore>>`。runner 建连前获取许可，停止时释放。扩张控制器把“拿不到许可”当作槽位已满处理 |

IDM 的按站点上限是每个下载各算各的，bolt-load 有意做成跨任务共享，比 IDM 更严格。

### 10.3 令牌桶（S5）

- 进程级共享。速率是 `limit`，桶深是 `max(limit × 0.25 s, 64 KiB)`。
- **等待放在 emit 路径上**：runner 在 `Emission` 推送一帧之前申请令牌。这和下游背压走同一条路径，`stream.rs:172` 会在背压结束后重置空闲计时器，所以限速等待不会被算成空闲超时。
- 单次等待上限是 `min(2 s, stream_idle_timeout / 4)`。超过上限就分段等待，每段之间检查取消和控制信号。
- 扩张联动：
  - 速率 ≥ 85% × limit 时不再扩张（§9.2）；
  - 首批连接数 `initial = clamp(ceil(limit / per_runner_estimate), 1, max_connections)`，没有速率估计时从 1 条起步；
  - 限速开启时，§9.5 的低速重启不生效。
- 公平性（D8）：所有 runner 竞争同一个桶，不做按任务的公平分配。

### 10.4 按 host 的能力缓存（S7，D7）

- 某个任务发生 §4.3 的回退后，在 `Limits.host_caps` 里记录 `host → SingleConnection`，TTL 1 h。之后发往同一 host 的任务，直接以 Singleton 模式起步。
- `TaskBuilder::force_multi_connection(true)` 可以豁免这条规则。
- 这条只在结构上参考了 IDM `sites_sf` 的 `&1` 标志位，语义是 bolt-load 自己定的。IDM 单连接降级的具体条件我们还不清楚。

---

## 11. 位点与续传（P3）

### 11.1 双位点

- `ChunkPlanner` 在 `downloaded`（已接收）之外，再维护一个 `written`：只有收到 `WriteCompletion` 之后才推进它。
- `Progress.downloaded_chunks` 改为上报 `written`。
- 需要持久化的时候，还要有第三个位点 `durable`（`fsync` 之后才推进），并且保证“先同步数据，再写检查点”。
- 实现（C18，§14.21）：`written` 是一组合并过的字节区间，不是每个 chunk 一个位点；`durable` 留给 C20 的状态文件。

### 11.2 资源身份

- `BoltLoadAdapterMeta` 增加 `etag: Option<String>`（只保存强 ETag）和 `last_modified: Option<String>`，都取自 GET（探测的 `bytes=0-0`），不取自 HEAD。
- 每个 range 请求都带上 `If-Range`（不只续传）：有强 ETag 就用 ETag；没有 ETag 时，用至少早于 `Date` 1 s 的 Last-Modified；只有弱 ETag 时什么都不发。
  - 返回 `206`：继续下载；`206` 带的验证器和发出去的冲突时，返回 `ResourceChanged`。
  - 返回 `200`：验证器和发出去的相同，是忽略 Range（§4.3 的回退）；不同或没有，说明资源已经变了，返回 `ResourceChanged`。这种情况**不**走 §4.3 的单连接回退。
- 维护者的决定（2026-11-09，§3 的 D12、D13、D14 已修订）：引擎只检测，任务丢掉已写的数据（删掉临时文件），以 `DownloadError::ResourceChanged` 失败。从头重来、刷新 adapter 缓存、共享 adapter 时每个任务的验证器，都移到引擎之上，见 #195。实现见 §14.23。
- C20 续传之前也要比对验证器（§11.5.7）：续传用新的 adapter 取新的 meta，比对头部存的验证器（种类和值），不一致就以 `ResumeRejected` 失败（D16 修订）；#195 定下 adapter 刷新的办法后一起改。

### 11.3 状态文件

`<file>.boltload` 采用逐行追加的 JSON Lines 格式：

- 第一行是头部：`{v, url, total, etag, last_modified}`（C20 改成二进制格式，去掉了 `url`，换成调用方给的可选 `source`，见 §11.5.5）；
- 之后每一行记录一个已写入并 `fsync` 过的区间。

**不使用**注册表或其它 KV 存储。

### 11.4 续传前的一致性检查（S6）

读入状态文件之后、发出任何请求之前，依次执行：

1. 区间按起点排序。和前面区间重叠的、超出 `[0, total)` 的区间，**整段丢弃**。单文件模型下没法判断重叠部分哪一份是对的，所以不做“截掉一半保留另一半”。
2. 目标文件的长度必须 `>= max(written.end)`。超出文件长度的区间丢弃。
3. 头部记录的 `total` 和重新探测到的 meta 不一致时，整个状态文件作废，从头下载。
4. 丢弃了哪些区间、丢弃的原因，都要通过 `TaskStatus` 或 tracing 上报，不能静默处理。
5. 可选：没有验证器（ETag、Last-Modified）时，用一个小的 range 请求取回前 2 KiB，和本地内容比对。

依据：IDM 在续传前重建分片时，会把超长的分片文件截短（改进方案 §8 A4）。

### 11.5 续传入口（C20）

> 状态：设计已审过（2026-11-09，codex 设计审查第三轮 90 分通过，之后按它的 Minor 和建议修订），可以开始实现（tasks.md T21）。维护者的决定是 §3 的 D15–D19；其余是 lead 的决定，在下文标明，11.5.12 汇总了 lead 对第二版遗留问题的决定。

#### 11.5.1 现状、约束和不做的事

- 临时文件（下文叫“部分文件”）是 `<dest>.downloading`，只由 `OutputFile::new` 推导（`output_file.rs:61`）。失败、取消时 teardown 删掉它（`commit` → `discard`，改名失败也删），成功时改名成目标文件。单连接模式以 `truncate(true)` 打开它（`singleton_task.rs:70`），并发模式的 writer 以 `create(true)` 打开并 `set_len(total)`（`file_writer.rs:280`）。
- 已写入的区间：C18 的 `ChunkPlanner::written`（§11.1、§14.21），只在 `WriteCompletion` 之后推进；writer 只在 `finalize` 时同步一次，没有中途的同步操作。`WriteCompletion` 只带区间（`pending_writer.rs:40`）。
- 资源身份：meta 的 `etag`（强）和 `last_modified`，取自探测的 GET（§11.2、§14.23）。真正用于 `If-Range` 的验证器（`bolt_load_http::Validator`）只存在 adapter 的缓存里，meta 里没有：原样保存的 `last_modified` 不代表它能用（弱 ETag、没有或重复的 `Date`、离 `Date` 不到 1 s 时都不能用）。adapter 只探测一次（§14.23、#195）。
- task-api-spec 的规则：builder 同步、不做 I/O（BUILD-5）；所有 I/O 都在 `Initializing` 里（§4.3）；一个 `Task` 是一次尝试（RERUN-1）；`TaskInfo` 在 `Initializing` 结束时发布（STATUS-3），回退时替换一次（STATUS-9）；teardown 删掉临时文件之后才发布终态（STATUS-6）；panic 时只发出取消信号、不等 worker（TASK-8）。
- 引擎不知道 URL，meta 里没有。§11.3 头部的 `url` 改成调用方给的可选 `source`。
- **不做的事（维护者，2026-11-09）**：续传只需要发现状态文件和部分文件之间的不一致，以及意外的损坏（掉电、写坏、磁盘坏块）。不防篡改：不签名、不混淆、不加 HMAC，这不是下载器的事。以后真要签名，由将来的客户端做，任务读的始终是原始的、不签名的状态。
- **分层（维护者，2026-11-09，和 #195 同一原则）**：任务只提供机制，“从头来”不由它决定。续传时要么续上，要么以 `DownloadError::ResumeRejected { reason, .. }` 失败；从头来、问用户、清理文件，都是客户端或 job 层的事。

#### 11.5.2 公开 API（D15，按 D16 修订）

```rust
/// Whether a task keeps what it downloaded when it does not finish, and
/// continues from what an earlier task kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Resume {
    /// Start from an empty file; delete it when the task does not finish.
    #[default]
    Off,
    /// Keep the partial file and a state file when the task does not
    /// finish; continue from them when they are there, or fail with
    /// `DownloadError::ResumeRejected` when they cannot be continued.
    On,
}

impl TaskBuilder {
    pub fn resume(self, resume: Resume) -> Self;
    /// An identity of the source, stored in the state file and compared on
    /// resume: a different one rejects it. Optional.
    pub fn resume_source(self, source: impl Into<String>) -> Self;
}

/// Remove what a task kept for `dest`: `<dest>.downloading` and
/// `<dest>.boltload`. Takes the destination's lock; fails with `InUse`
/// while a task holds it.
pub async fn discard_partial(dest: impl AsRef<Path>) -> Result<(), DownloadError>;
```

```rust
// A client's retry policy, above the task: resume, and start over only
// when the kept files cannot be continued.
let builder = Task::builder(fresh_adapter()).runtime(rt).save_to("big.iso").resume(Resume::On);
match builder.spawn()?.await {
    Err(DownloadError::ResumeRejected { reason, .. }) => {
        warn!("cannot resume: {reason:?}; starting over");
        bolt_load::task::discard_partial("big.iso").await?;
        Task::builder(fresh_adapter()).runtime(rt).save_to("big.iso").resume(Resume::On).spawn()?.await?;
    }
    other => { other?; }
}
```

- `Resume::On`：
  - 没有状态文件，或者状态文件是这一次加锁时刚建的、或是空的（之前的任务崩在写头部之前）：正常的新开始，不是拒绝（lead 决定）。`TaskInfo::resumed` 是 `None`。
  - 有状态文件：读入、检查（11.5.6），续上；不能续时以 `ResumeRejected` 失败，不改动任何文件。
- 状态文件是 `<dest>.boltload`，也是目标的锁文件（11.5.4），只由 `OutputFile` 推导。`save_into` 时目标路径在 `Initializing` 里解析出来，之后一样。
- builder 不做 I/O：`resume`、`resume_source` 只记下设置。
- `prefer_mode(Singleton)` 的任务在 `On` 时走并发引擎、只用 1 个连接、带条件的 range 请求（lead 决定，11.5.8）。
- 续传要新的 meta：用新的 adapter（文档写明，lead 决定）；#195 定下 adapter 刷新的办法后一起改。
- 曾考虑过、没有采用：`Auto` / `Required`（第一版）——是否从头来是上层的决定（D16 修订）；显式的 `state_file(path)` / `resume_from(path)`；放进 `TaskOptions::resume`。

#### 11.5.3 能续的条件

一次尝试只在下列条件都满足时写状态文件（下文叫“可续”）：`Resume::On`；meta 的总长已知、非零；meta 没有内容编码；adapter 能用 Range；meta 有可用的验证器（11.5.7）；走并发引擎。不满足时，`On` 的任务照常下载，但不写状态、不做检查点，结束时按 `Off` 的规则清理。

- 验证器超过格式的长度上限（11.5.5）也算“没有可用的验证器”：新开始时不可续，续传时 `ResumeRejected(NoValidator)`。这在任何破坏性的初始化之前判定。`resume_source` 的长度上限由 `TaskBuilder::validate` 检查（`BuildError::InvalidOptions`），不进到 `Initializing`。
- **`TaskInfo::resumable: bool`**（lead 决定，Q1）：这一次是否写状态文件。`Initializing` 结束时设置：可续（新开始的头部已落盘，或者续传的记录已写回，11.5.5）时为 `true`，否则（`Off`、不满足上面的条件）为 `false`。回退到单连接（STATUS-9）替换 `info` 时变成 `false`（11.5.8）。为此，可续的新开始的准备（建部分文件、写头部，11.5.5）在 `Initializing` 里做，在发布 `info` 之前。

#### 11.5.4 锁（D17，修 #152；C31）

- **加锁**（每个任务，`Off` 也一样）：在 `Initializing` 里、解析出目标路径之后：
  1. 先 `create_new` 打开 `<dest>.boltload`（读写）；已存在就以读写打开已有的。记下 `created`（这次是否新建，lead 决定）。
  2. `try_lock_exclusive`（`fs4`）。三类失败分开处理（lead 决定，Q3）：
     - **争用**（Unix `EWOULDBLOCK`，Windows `ERROR_LOCK_VIOLATION` 或打开时 `ERROR_SHARING_VIOLATION`）→ `DownloadError::InUse { path }`，不碰任何文件。
     - **不支持锁**（Unix `ENOLCK`、`EOPNOTSUPP`/`ENOTSUP`，Windows `ERROR_NOT_SUPPORTED`、`ERROR_INVALID_FUNCTION`；部分网络文件系统）→ `On` 以 `DownloadError::Io { op: IoOp::Lock, .. }` 失败；`Off` 打 `warn!`、不加锁继续（这时 #152 不受保护，也不建锁文件之外的任何东西）。
     - **权限或其它 I/O 错误**（`EACCES`、`EPERM`，Windows `ERROR_ACCESS_DENIED` 且不是删除挂起）→ 两种模式都以 `Io { op: IoOp::Lock }` 失败。
  3. 核对：路径现在指向的就是这个句柄打开的文件（`same-file`，已在依赖树里：Unix 比 `dev`+`ino`，Windows 用 `GetFileInformationByHandle` 比卷序列号和文件索引）。不是（锁住的是一个已经被删掉或换掉的文件）就放锁、关掉、重来，最多 3 次，之后按争用处理。Windows 上打开一个“删除挂起”的文件会得到拒绝访问，也按争用重来。
  4. 已有的文件长度为 0：当作“没有状态”（崩在写头部之前留下的空文件，没人持锁就是遗留物）。
- **只经这个句柄读写**（lead 决定）：Windows 的 `LockFileEx` 是强制锁，同一进程里另开的句柄也读写不了被锁住的文件。所以状态文件的解析、截断、追加、`set_len` 全部经加锁的读写句柄（`pread` / `pwrite` 式的定位读写），不另开句柄。
- **持有期间不删、不换**（审查 Major 1）：任务结束之前，状态文件只能经锁住的句柄清空（`set_len(0)` + `fsync`），不能删除或改名替换；否则别的任务可以在原路径上新建一个文件、加锁、核对通过，然后去改同一个部分文件。只在对部分文件的最后一次操作之后才删除它。
- **结束**：对部分文件的最后一次操作（改名、删除或保留）之后，按 11.5.5 的顺序删除或保留状态文件，然后放锁。
- **锁的所有权在 unwind 和取消之外**（审查 Major 8，lead 决定，C31 里做）：
  - 锁放在 `drive::spawn` 的闭包里、`catch_unwind` 之外的一个槽里；`Initializing` 拿到锁后放进槽。attempt panic 时，panic 处理先取消，再**等**所有持有部分文件或状态文件的 worker（runner、writer、检查点）退出——它们各持一个 drain 用的 `WaitGroup` 克隆——然后才按 11.5.9 清理、放锁、发布终态。这改变了 TASK-8 的“只发信号、不等”，task-api-spec 一并改。
  - 等待没有上限（lead 决定，Q2）：worker 都已被取消，只剩手上这一次文件操作。代价写明：某个 worker 卡死在一次文件操作上时，取消、panic 的收尾和锁都会一直挂着（互斥仍然成立）。drain 只等 `WaitGroup`，不依赖任何可能已在 unwind 中销毁的队列、通道或执行器：worker 在线程池（`blocking`、threaded runtime）上自己跑完，退出时放下自己的克隆。
  - `Initializing` 里的文件操作（加锁、读、截断、写头部、建部分文件）不和取消赛跑：`async_fs` 的操作落在 `blocking` 线程上，丢掉 future 停不下已经提交的操作。每一步之间检查取消；只有长的 CRC 校验（11.5.6）按块检查取消。
- **依赖：`fs4`**：`try_lock_exclusive` / `unlock` 作用在 `std::fs::File` 上，锁跟着句柄走，句柄关闭（包括进程崩溃）就释放；不用像 `fd-lock` 那样跨整个任务持有一个借用文件的 guard。只用同步的部分（`default-features = false`，feature 名实现时核对）；底层的 `rustix`、`windows-sys` 已经在依赖树里。标准库的 `File::try_lock` 在 1.89 才稳定，本仓库 MSRV 是 1.88（审查已核实），MSRV 提高之后可以换掉 `fs4`。
- **`Off` 的要求**：`Off` 现在也要在目标目录里建 `<dest>.boltload`（要能写目标目录，只读目录本来就建不了 `.downloading`，不是新限制）。文件系统不支持锁时 `Off` 不加锁继续（上面第 2 步）；权限错误照常失败。

#### 11.5.5 状态文件格式（D18 修订）和崩溃安全的顺序

**格式**（二进制，小端；`serde`、`serde_json` 不再需要）：

```text
header:
  magic        [u8; 8]  = b"BOLTLOAD"
  version      u16      = 1
  flags        u16      = 0 (reserved)
  block_size   u32      (D19, 11.5.6)
  total        u64
  validator    u8 kind (1 = strong ETag, 2 = Last-Modified) + u16 len + bytes (len <= 1024)
  source       u16 len (0xFFFF = none) + bytes (len <= 4096)
  header_crc   u32      CRC32C of every header byte before it
record (24 bytes, repeated until EOF):
  index        u64      block number
  len          u32      block length: block_size, or less for the last block
  data_crc     u32      CRC32C of the block's bytes in the partial file
  kind         u8 = 1 (complete block), [u8; 3] reserved = 0
  record_crc   u32      CRC32C of the 20 bytes before it
```

- 头部校验失败 → `ResumeRejected(BadHeader)`：magic、版本不对；`flags` 不为 0（不认识的标志）；验证器种类不是 1 或 2；长度超过上限；`header_crc` 不对；`block_size` 不是本版本支持的。
- 长度上限：验证器 1024 字节，`source` 4096 字节。写入方不会写出超过上限的头部：验证器超长按“没有可用的验证器”处理（11.5.3），`source` 超长在 `validate` 时就拒绝。
- 块的偏移一律用检查过的算术（`checked_mul`、`checked_add`）：`index * block_size` 溢出或超出 `total` 的记录按 `OutOfBounds` 丢弃，不会进到减法或 planner。
- 记录定长、各带 CRC：文件末尾不足 24 字节的残片（写到一半掉电）按记录边界截掉（审查 Major 4）；`record_crc` 不对、`kind`/保留字节不对、`index` 越界、`len` 和该块应有的长度不符（包括 0）的记录丢弃，报告带位置（11.5.10）。
- 同一块的完全相同的记录去重（正常情况下不会出现，见下面的检查点）。同一块出现 `data_crc` 不同的有效记录时，这一块在整个扫描里都标为冲突：已经接受的那条撤掉，之后再出现的任何记录（包括和最早那条相同的）都不接受，都按 `Conflict` 报告（审查第二轮 Minor 6）。块是对齐的，§11.4 第 1 步的“部分重叠”在这个格式里不会出现。
- 依赖：`crc32c`（维护者批准一个小的 CRC32C crate）。选它：x86_64 用 SSE4.2、aarch64 用 ARMv8 CRC 指令，其余平台软件实现；提供 `crc32c_append` 和 `crc32c_combine`（11.5.6 拼接块的 CRC 要用）；没有运行时依赖。版本和维护状态实现时核对。
- **不设 `resume` feature**（lead 决定，Q4）：没有了 `serde`，续传只多 `crc32c` 一个很小的依赖；锁（`fs4`）不管开不开续传都要。留一个 feature 只会多出要测的组合。

**顺序**（Unix 上新建、改名、删除之后都 `fsync` 父目录；Windows 没有目录同步，靠 NTFS 的元数据日志，写进已知限制）。下面的“作废”都是：经锁住的句柄 `set_len(0)`，再 `fsync` 状态文件。

- **新开始，可续**（审查 Major 2）：
  1. 有旧状态（长度非 0）先作废——在动部分文件之前；
  2. 以 `truncate(true)` 建或打开部分文件，`set_len(total)`，`fsync` 部分文件；
  3. 经锁住的句柄写头部，`fsync` 状态文件；
  4. `fsync` 父目录（两个文件的目录项都可能是新的）；
  5. 之后才开始下载和检查点。

  崩在 1 之前：旧状态还在、部分文件没动，仍然一致。崩在 1、2 之间或 2、3 之间：状态是空的（没状态），部分文件是什么都无所谓。崩在 3、4 之间：头部可能落盘而目录项没有，下次看不到部分文件 → `ResumeRejected(MissingPartial)`，安全。
- **新开始，不可续**（`Off`，或 `On` 但不满足 11.5.3）：有旧状态先作废；之后部分文件照现在的方式建（单连接 `truncate`，并发 `create` + `set_len`）；不写头部。
- **检查点**（可续时，见 11.5.6）：串行，同一时刻最多一个；追加位置 `end` 由任务记着（审查 Major 4）：
  1. 取此刻新完整的块（全部字节都在 `written` 里、还没记过的块）和它们的 CRC；
  2. writer `sync`（数据）；
  3. 从 `end` 开始经锁住的句柄写这些记录，`fsync` 状态文件；
  4. 成功：`end` 前移，这些块记为已记录（`durable`）。

  2 或 3 失败：经锁住的句柄 `set_len(end)` 回滚到上次的位置，再 `fsync` 状态文件，块仍算没记录，下次检查点再试。回滚的截断或它的 `fsync` 失败时，这个任务不再做检查点：状态文件在 `end` 之前仍然有效，之后可能留下的记录要么是残片（读入时按记录边界截掉）、要么 CRC 不对（丢掉）、要么是完整有效的记录——后者描述的块的数据已经在第 2 步同步过（第 2 步失败时第 3 步不会执行），续传时还会逐块校验，所以不会把没落盘的数据当真。检查点失败不让下载失败，只打 `warn!`（lead 决定，Q5）；数据写入本身出错时，writer 照常报 `Write`。
- **续传**（11.5.6 的检查之后，审查 Major 3、第二轮 Minor 3）：在改动部分文件之前（`set_len(total)`、任何写入），把清理过的记录写回去：
  1. 经锁住的句柄 `set_len(头部长度)`，`fsync` 状态文件——旧记录先确实没了；
  2. 按顺序写入接受的记录，`fsync` 状态文件；
  3. 之后才动部分文件。

  第 1 步改动状态文件之前，任务进入阶段 C（11.5.9）。崩在 1 之后、2 完成之前：状态里只有头部和接受记录的一个前缀，全是校验过的块；被丢的记录不会在下一次续传时又被当真。1 或 2 失败（包括截断成功而它的 `fsync` 失败）：任务以 `Io` 失败；部分文件没动过，状态文件是它实际剩下的样子（头部，加上接受记录的一个前缀）；头部没被改动，所以按 11.5.9 的规则 `kept_for_resume` 指向部分文件。只有第 1 步之前的失败才是 B2 的“两个文件都没动”。
- **完成**（审查第二轮 Major 1、Major 2）：
  1. writer `finalize`（同步）；
  2. 改名部分文件 → 目标文件。成功之后进入“已发布”阶段（P，11.5.9），任务的结果就是 `Finished`，之后的步骤都不改变它；
  3. `fsync` 目标所在的目录（Unix）；
  4. 截断状态：经锁住的句柄 `set_len(0)`；
  5. `fsync` 状态文件；
  6. 删除状态文件（仍持锁）；
  7. `fsync` 父目录；
  8. 放锁。

  第 3–7 步任何一步失败：打 `warn!`，照常做后面的步骤，结果仍是 `Finished`（审查第三轮 Minor 1，分开每一步）。

  **紧接着的下一次 `On`**（没有掉电，看到的是文件系统当前的样子；同步失败不影响它）：

  | 4 截断 | 6 删除 | 下一次 `On` 看到 | 结果 |
  | --- | --- | --- | --- |
  | 任意 | 成功 | 没有状态文件 | 新开始 |
  | 成功 | 失败 | 空的状态文件 | 新开始 |
  | 失败 | 失败 | 有效的状态，部分文件已改名 | `ResumeRejected(MissingPartial)`，由上层清理（目标文件已经完整） |

  **掉电之后**（没同步的操作可能丢）：
  - 改名没落盘（3 没完成或失败）：部分文件回到原名、目标文件不在。状态若已落盘作废（4、5 都完成）或已删除 → 新开始；状态仍有效（4 或 5 没完成）→ 正常续传：每块 CRC 都对，全部保留，再走一次完成。
  - 改名已落盘：状态有效（4 或 5 没完成、删除也没落盘）→ `ResumeRejected(MissingPartial)`；状态是空的 → 新开始；状态不在（删除已落盘）→ 新开始。
  - 所以下一次 `On` 只可能是：新开始（状态不在或为空）、正常续传（改名回退、状态有效）、`MissingPartial`（改名已生效、状态有效）。三种都安全。

  `Off` 的完成：改名 → 删除锁文件 → 放锁，和现在一样不同步目录（`Off` 没有需要一致的状态）。
- **回退到单连接**（11.5.8）：回退开始之前作废状态（不删，仍是锁），之后不再写；结束时删除。
- **丢弃**（不可续的失败、取消，`ResourceChanged`；`discard_partial` 也走这个顺序）：作废状态（`set_len(0)` + `fsync`）→ 删部分文件 → `fsync` 父目录 → 删状态文件 → `fsync` 父目录 → 放锁。
  - 作废失败：不删部分文件（否则留下一个有效的状态指向不存在的部分文件），任务按原来的错误结束，`warn!`；两个文件都留着，下一次 `On` 照常检查（CRC 会挡住不一致的块）。`discard_partial` 这时返回 `Io` 错误。
  - 删部分文件失败：`leftover_temp_file` 报告它（阶段 D 的现有规则）；状态已作废，下一次当作没有状态。`discard_partial` 返回 `Io` 错误。
  - 删状态文件失败：`warn!`，它已是空的，下一次当作没有状态（Q6）。

#### 11.5.6 数据完整性：按块的 CRC（D19）

- **块**：部分文件按 `block_size` 对齐分块，最后一块可以较短。默认 1 MiB（`2^20`），写进头部；续传时头部的 `block_size` 和本版本支持的不同（将来改默认值时）→ `ResumeRejected(BadHeader)`。
  - 取舍：块越小，坏一块要重下的越少；块越大，记录越少、检查点追加越少。1 MiB 时 1 GiB 文件 1024 条记录（24 KiB），100 GiB 2.4 MB；一个块坏了重下 1 MiB。4 MiB 记录少到四分之一，但崩溃时每个没写满的块最多丢的也变成 4 MiB。
- **只记完整的块**（lead 决定）：检查点只记全部字节都已写入的块，部分写入的块不记。代价按块算：崩溃时每个没写满的块最多丢 `block_size - 1` 字节已写的数据，要重下（审查第二轮 Minor 4：没写满的块有多少，取决于切分和重试留下的空洞，不只是连接数，这里不给更强的上界）。换来的是记录定长、不需要“部分块的记录被后来的完整记录取代”的规则。另一种做法是给部分写入的区段单独记 CRC，格式和替代规则都更复杂，不采用。
- **CRC 从内存里的数据算**：提交写入时对这一帧（planner 裁剪之后真正写入的字节）算 CRC32C，跨块边界的帧在边界处拆成几段分别算，随 `WriteCompletion` 带回（`WriteCompletion` 加 CRC 段）；只有成功的写才算，失败的写（包括写了一部分再出错的）一段都不算。对冲中败方的帧在 writer 之前就丢了（§9.6），不会进来。前提是 C18 的不变式：成功写入的区间互不重叠（审查第二轮确认）。
  - 帧最多 32 KiB（runner 攒满才交），最后一帧、中断时的帧、被 planner 裁剪的帧更小（审查第二轮 Minor 4）。
  - 每个没写满的块记一组按偏移排好、互不相邻的段 `(offset, len, crc)`；完成的写到达时，和左右相邻的段用 `crc32c_combine` 立刻合并，所以一块里的段数等于其中空洞数加一，按顺序写的块始终只有一段。块写满时只剩一段，就是整块的 CRC。
  - 上限：每块最多 64 段，全部块合计最多 16384 段。超过时，这一块（合计超了就挑段最多的块）丢掉它的段，改为“回读”：检查点时在数据同步之后从部分文件读出整块算 CRC。内存因此有界，块照样能记。
  - 不选“一律回读”：每个字节多读一遍；续传时本来就要回读校验。
- **续传时逐块校验**（维护者）：每个保留下来的块从部分文件读出来算 CRC32C，和记录比对。对不上的块丢掉、重新下载，像坏扇区一样，报告 `CrcMismatch`；对得上的保留。超出部分文件长度的块丢掉（`BeyondFileLength`）。这一步代替了第一版的“文件长度检查”和审查 Major 9 的“部分文件身份”：保留下来的字节本身都校验过了。
  - 代价：保留的每个字节读一遍（硬件 CRC 很快，瓶颈是磁盘）；大文件要一段时间，在 `Initializing` 里按块检查取消。
- **保证的范围**：能发现掉电、写坏、坏块、被外部改动过的块（意外的，11.5.1 的不做的事）；CRC32C 不是加密哈希，故意构造的碰撞不在范围内。

#### 11.5.7 验证器（审查 Major 5）

- meta 新增 `validator: Option<Validator>`：adapter 实际用于 `If-Range` 的那个（种类和值），`None` 表示没有可用的。`Validator` 从 `bolt-load-http` 移回 core（http 里留一个 re-export），adapter 探测时设置它（新提交，见 11.5.11）。
- 头部存这个验证器（种类和值）。续传时和新 meta 的 `validator` 比对：种类相同；强 ETag 逐字节相同；Last-Modified 按日期相同。不同 → `ResumeRejected(ValidatorMismatch)`；新 meta 没有可用的验证器 → `ResumeRejected(NoValidator)`（D16 修订）。不再拿原样保存的 `last_modified` 去比。
- 比对通过，续传的 range 请求照常带 adapter 的 `If-Range`；续传途中 `ResourceChanged` 照 D12 修订后的规则失败，按 11.5.5 的“丢弃”清理（数据属于旧版本）。

#### 11.5.8 单连接与回退（审查 Major 6）

- `full_stream` 的字节从不进检查点：单连接的响应没有验证器（`FullResponse` 不带），绑定不到头部的验证器上；内容编码也可能只在这个响应里出现。所以单连接模式（包括回退之后）从不写状态。
- `On` 的单连接下载（`prefer_mode(Singleton)`）改走并发引擎、最多 1 个连接、带 `If-Range` 的 range 请求，满足 11.5.3 时可续；不满足（比如不能用 Range）就照常走单连接、不可续。
- 回退到单连接：按 11.5.5 作废状态，之后不可续；结束时清理同不可续。
- 实际的 body 需要解码时（并发引擎里编码的 range 本来就会回退，§4.3），同上，不做检查点。

#### 11.5.9 结束时的文件和报告（审查 Major 7，按所有权的阶段）

阶段：A 还没拿到锁；B1 新开始，拿到锁、头部还没落盘（11.5.5 新开始的第 3 步完成之前）；B2 续传，拿到锁、还在检查（写回改动状态文件之前，这时任务没改动任何文件）；C 可续（新开始的头部落盘之后；续传在写回改动状态文件之前进入——写回不和取消赛跑，中途崩溃也只留下头部和校验过的前缀）；D 不可续（`Off`、不满足 11.5.3、回退之后）；P 已发布（改名成功之后，审查第二轮 Major 2）。

| 结果 | 阶段 | 部分文件和状态文件 | `kept_for_resume` | `leftover_temp_file` |
| --- | --- | --- | --- | --- |
| `Finished` | C、D → P | 改名；之后按 11.5.5 的“完成”作废并删除状态 | `None` | `None` |
| 改名之后的步骤出错（目录同步、截断、状态同步、删除、目录同步） | P | 目标文件保留；`warn!`；结果仍是 `Finished`（11.5.5 的“完成”） | `None` | `None` |
| `Panicked` | P | 同上；终态是 `Finished`，不是 `Failed(Panicked)` | `None` | `None` |
| `InUse`、失败、取消、`Panicked` | A | 不碰（还没拥有任何文件） | `None` | `None` |
| `ResumeRejected` | B2 | 不碰（任务没改动它们） | `None`（这一对续不上） | `None` |
| 失败、取消、`Panicked`（比如校验时） | B2 | 不碰 | `None`（还没确认可续；两个文件原样留着，下次照常检查） | `None` |
| 失败、取消、`Panicked` | B1 | 丢弃（11.5.5） | `None` | 删不掉的部分文件 |
| 写回失败（`Io`） | C | 部分文件没动；状态是实际剩下的（头部加接受记录的前缀，11.5.5 的“续传”） | 部分文件的路径（头部有效） | `None` |
| 失败、取消、`Panicked`（等 worker 退出后） | C | teardown 先做一次检查点，两者都留 | 部分文件的路径 | `None` |
| 改名失败（`Io { op: Rename }`） | C | 两者都留（部分文件是完整的，续传会全部校验通过、再改名） | 部分文件的路径 | `None` |
| `ResourceChanged` | C | 丢弃（数据属于旧版本） | `None` | 删不掉的部分文件 |
| 失败、取消、`Panicked`、改名失败 | D | 丢弃（和现在一样，`OutputFile::commit` 的各个错误路径不变） | `None` | 删不掉的部分文件 |

- `kept_for_resume` 只在“部分文件存在、状态文件头部有效”的一对上设置（lead 决定）。teardown 的检查点失败不影响它：状态在上次成功的检查点之前仍然有效。
- `OutputFile::commit` 要按阶段分支：阶段 C 时失败不删部分文件，改名失败也不删。
- **panic 也按这张表**（审查第三轮 Minor 3）：阶段和清理所需的东西（锁、两个路径、阶段；到了 P 还有任务的输出）放在 `catch_unwind` 之外的槽里，由 `Initializing` 和各个阶段转换时更新。panic 处理取消、等 worker 退出（11.5.4）之后，按槽里的阶段做表里 `Panicked` 那一行：A、B2 不碰任何文件，B1、D 丢弃，C 两者都留（不做 teardown 检查点：状态在上次成功的检查点之前有效），P 保留目标文件、完成剩下的“完成”步骤、终态发布 `Finished`。
- 状态文件删不掉（丢弃或完成时）：打 `warn!`，不新增状态字段（lead 决定，Q6）；它已经被作废（长度 0），下次加锁时当作没有状态（11.5.4 第 4 步）。作废本身也失败时的结果见 11.5.5 的“完成”和“丢弃”。

#### 11.5.10 续传前的检查和报告（§11.4）

在 `Initializing` 里，取到新的 meta、选好模式、解析出目标路径、拿到锁之后，发出任何 range 请求之前：

0. 状态文件是这次新建的或长度为 0 → 新开始（11.5.2）。部分文件不存在 → `ResumeRejected(MissingPartial)`。
1. 头部（11.5.5）→ 不对就 `BadHeader`。
2. 和新 meta 比对：`total`（`TotalMismatch`）、`source`（`SourceMismatch`）、验证器（`ValidatorMismatch`、`NoValidator`）；还有 11.5.3 的其它条件：内容编码（`ContentCoded`）、总长未知（`UnknownSize`）、不能用 Range（`RangeUnsupported`）。任何一条不过 → `ResumeRejected`，不改动文件（D16 修订）。§11.4 把 `total` 排在第 3 步，这里提前，因为它否决整个状态。
3. 逐条记录（11.5.5）：残片截掉、坏记录丢弃、完全相同的去重、冲突的两条都丢（§11.4 第 1 步：越界的 `index` 在这里丢，`OutOfBounds`）。
4. 逐块校验数据（11.5.6）：超出部分文件长度的丢（§11.4 第 2 步，`BeyondFileLength`），CRC 不对的丢（`CrcMismatch`）。
5. 写回清理过的记录（11.5.5），然后 `set_len(total)`，`ChunkPlanner` 以保留的块作为已下载、已写入、已持久化，只规划其余部分。
6. 报告（§11.4 第 4 步），不静默：

```rust
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ResumeReport {
    pub state_file: PathBuf,
    /// The ranges continued from, merged.
    pub kept: Vec<Range<u64>>,
    pub kept_bytes: u64,
    /// The records dropped, in file order.
    pub dropped: Vec<DroppedRecord>,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DroppedRecord {
    /// The byte offset of the record in the state file.
    pub position: u64,
    pub reason: DropReason,
    /// The block the record names, when it could be read.
    pub range: Option<Range<u64>>,
}

#[non_exhaustive]
pub enum DropReason { TornTail, BadRecord, Conflict, OutOfBounds, BeyondFileLength, CrcMismatch }

#[non_exhaustive]
pub enum RejectReason {
    MissingPartial, BadHeader, TotalMismatch, SourceMismatch, ValidatorMismatch, NoValidator,
    ContentCoded, UnknownSize, RangeUnsupported,
}

// DownloadError::ResumeRejected { reason: RejectReason, state_file: PathBuf }
```

- 记录在构造时就拒绝空的、反向的区间（`len == 0`、越界），不会进到减法或 planner 里（审查 Minor 10）。
- `TaskInfo::resumed: Option<ResumeReport>`：续上了才是 `Some`（哪怕所有记录都被丢了、`kept` 为空）；新开始、`Off` 时是 `None`。每个丢弃都打一条 `warn!`。放在 `TaskInfo` 里，是因为它发布之后每个快照都带着，不会因为状态通道合并而丢。
- 拒绝发生在 `Initializing`，不发布 `Downloading`：原因在 `ResumeRejected` 里（审查 Minor 11）。
- 回退（STATUS-9）替换 `info` 时 `resumed` 变成 `None`、`resumable` 变成 `false`：保留的区间随回退作废，进度从零开始。
- `TaskInfo` 新增的两个公开字段：

```rust
pub struct TaskInfo {
    // … existing fields …
    /// Whether this attempt keeps a state file for a later resume
    /// (design §11.5.3); `false` after a fallback.
    pub resumable: bool,
    /// What the task continued from, when it resumed (design §11.5.10).
    pub resumed: Option<ResumeReport>,
}
```

#### 11.5.11 测试（T21）

- **G9**：本地服务器、确定性内容；在随机字节位置（种子决定）取消任务，再用新的 adapter 续传，最终 hash 正确；至少 20 个种子，三种客户端都跑。位置包括：第一次检查点之前、检查点进行中、改名失败之后。
- **反复续传**：取消、续传、再取消、再续传若干次，hash 正确，每次的 `kept` 不减少（除非有块被丢）。
- **进程崩溃**：测试进程用 `std::process::Command` 再起一个自己（按环境变量进入“只下载”模式），在随机时刻杀掉，然后在父进程续传，hash 正确。
- **跨进程的锁**：子进程持锁时，父进程的任务以 `InUse` 失败；子进程被杀之后，父进程能拿到锁。
- **三种坏状态**（各一个测试，断言报告、`warn!` 和最终 hash）：冲突的记录（同一块不同 CRC）→ `Conflict`；越界的块号 → `OutOfBounds`；部分文件被截短 → `BeyondFileLength`。另加：部分文件里翻转一个字节 → 那一块 `CrcMismatch`、重新下载；残片 → `TornTail`、被截掉，之后的追加正常；坏的记录 CRC → `BadRecord`。
- **拒绝**：`total` 不同、`source` 不同、验证器不同、没有验证器、头部坏了、部分文件不在、内容编码、不能用 Range，各一个：以 `ResumeRejected` 失败，两个文件都没被改动。
- **检查点出错**：注入的 writer 让 `sync` 失败、注入的状态文件让追加或 `fsync` 失败：回滚到上次的位置，之后的检查点正常，续传 hash 正确；回滚也失败时不再做检查点，状态在上次之前仍然有效。
- **顺序**：新开始时，旧状态在部分文件被截断之前已经作废（注入的文件操作记录先后）；续传时，清理过的记录在 `set_len` 和第一次写之前已经写回。
- **锁（C31）**：同一进程两个任务；删锁文件的竞争（核对文件 ID）；panic 之后，worker 退出之前锁一直被持有（第二个任务 `InUse`），之后才释放；`Initializing` 里取消时锁正常释放；权限错误报 `Io { op: Lock }` 而不是 `InUse`；崩溃留下的空锁文件被当作没有状态。
- **结束时的文件**：11.5.9 表里每一行一个测试，断言两个文件是否存在、`kept_for_resume`、`leftover_temp_file`。
- **改名之后的每一步出错**（审查第二轮 Major 1、2，第三轮 Minor 1）：注入 11.5.5“完成”第 3–7 步各自失败，以及截断和删除同时失败；结果都是 `Finished`、目标文件完整、`kept_for_resume` 和 `leftover_temp_file` 都是 `None`；紧接着再跑一个 `On` 任务，按“紧接着的下一次 `On`”那张表断言：删除成功 → 新开始；截断成功、删除失败（包括截断后同步失败）→ 新开始；截断和删除都失败 → `ResumeRejected(MissingPartial)`。掉电后的“改名回退、状态有效”用构造的文件模拟：把目标文件放回部分文件的名字、留着有效的状态，`On` 正常续传、全部块保留、hash 正确。
- **写回失败**：注入写回的截断成功而 `fsync` 失败、或写记录失败：任务以 `Io` 失败，部分文件没动，状态是头部加接受记录的前缀，`kept_for_resume` 指向部分文件；之后再续传正常。
- **panic 的每个阶段**：在 A、B1、B2、C、D、P 各注入一次 panic，断言 11.5.9 表里 `Panicked` 那一行（P 的终态是 `Finished`）。
- **写回和回滚的同步**：注入的状态文件记录 `set_len` 和 `fsync` 的先后：写回时截断的 `fsync` 在第一条记录写入之前；回滚的截断之后有 `fsync`。
- **冲突的记录**：同一块的记录序列 `A,B,A`、`A,A,B`、`B,A,A`、`A,B,B` 等排列，这一块都不接受，每条都报 `Conflict`（完全相同的重复除外，见 11.5.5）。
- **格式上限**：不认识的 `flags`、验证器种类 → `BadHeader`；超长的验证器 → 新开始时不可续、续传时 `NoValidator`；超长的 `source` → `validate` 失败；块号乘出溢出 → `OutOfBounds`。
- **CRC 对文件**（直接比对：任务结束后，用状态文件的每条记录和部分文件逐块算的 CRC 对照）：完成乱序到达；一帧跨两块；写了一部分再出错的写；mmap 后端的 `sync`；被 planner 裁剪、反复切分留下的短帧；对冲败方的帧；段数超过上限后改为回读的块。
- **锁的三类失败**：争用 → `InUse`；不支持锁（注入）→ `On` 以 `Io { op: Lock }` 失败、`Off` 照常下载并 `warn!`；权限错误 → 两种模式都 `Io { op: Lock }`。
- **`discard_partial`**：按“丢弃”的顺序；作废失败时不删部分文件、返回错误；有任务持锁时 `InUse`。
- **`resumable`**：可续的新开始和续传为 `true`；`Off`、没有验证器、不能用 Range 时为 `false`；回退之后为 `false`。

#### 11.5.12 lead 对第二版遗留问题的决定（2026-11-09）

1. **Q1**：新增 `TaskInfo::resumable`（11.5.3、11.5.10）。`On` 但不满足 11.5.3 时照常下载、不可续，不以 `ResumeRejected` 失败——否则 `On` 在不支持 Range 的服务器上下不了文件。
2. **Q2**：panic 时等 worker 退出才放锁，不设上限（11.5.4，代价写明）；审查发现卡死的风险再议。
3. **Q3**：不支持锁的文件系统上，`Off` 打 `warn!`、不加锁继续，`On` 失败；争用、不支持锁、权限错误三类分开（11.5.4）。
4. **Q4**：不设 `resume` feature（11.5.5）。
5. **Q5**：检查点失败只 `warn!`，回滚或停用检查点，不让下载失败（11.5.5）。
6. **Q6**：状态文件删不掉只 `warn!`，不新增状态字段（11.5.9）。

#### 11.5.13 提交拆分（建议）

C20 太大，按顺序拆成五个提交，每一步树都是绿的：

1. **PR-9c 锁（#152）**：C31 `fix(task)!: lock a destination for the whole task`。`fs4`、`same-file`；11.5.4 的加锁、核对、`created`、空文件、只经句柄读写、`InUse`、`Io { op: Lock }`；锁槽在 `catch_unwind` 之外，panic 时等 worker 退出（改 TASK-8）；`Initializing` 的文件操作不和取消赛跑；结束时作废并删除锁文件；`Off` 在动部分文件之前作废旧状态。测试：11.5.11 的“锁”，加跨进程的锁。带 `!`：同一目标的第二个任务改为 `InUse` 失败；`Off` 会清掉旧的状态文件；目标旁边多了一个任务期间存在的 `<dest>.boltload`；锁文件的权限错误会让 `Off` 失败（不支持锁时 `Off` 照常继续）。
2. **PR-9d 续传**，四个提交：
   1. C34 `feat(core): carry the If-Range validator in adapter meta`。`Validator` 移回 core（http 里 re-export）；`BoltLoadAdapterMeta::validator`；两个 adapter 设置它。不破坏兼容（`#[non_exhaustive]` 的结构体加字段，类型路径保留 re-export）。
   2. C32 `feat(task): checksum and sync written blocks at checkpoints`。`crc32c`；writer 的 `sync`（三个后端）；`WriteCompletion` 带 CRC；块的 CRC 拼接；已记录的块（`durable`）；串行的检查点调度（8 MiB / 2 s / teardown）和回滚的钩子，交给 crate 内部的接收者；`TaskOptions` 的检查点字段。没有接收者时不调度、不同步。基准：C13 的基准加一组“打开检查点”，测同步和 CRC 的开销，记进 §14。
   3. C33 `feat(task): write a state file while downloading`。二进制格式；11.5.5 的新开始、检查点追加和回滚、回退、完成、丢弃的顺序；11.5.9 的表；`OutputFile::commit` 按阶段分支；`kept_for_resume`；`discard_partial`（走“丢弃”的顺序）；`On` 的单连接改走并发引擎（11.5.8）；CRC 段的合并和上限（11.5.6）在 C32。`Resume` 先是 crate 内部的（测试打开），只写不读。
   4. C20 `feat(task): resume from a checked state file`。公开 `Resume`、`resume`、`resume_source`；11.5.10 的读入、比对、逐块校验、写回；`ResumeReport`、`DroppedRecord`、`RejectReason`、`DownloadError::ResumeRejected`、`TaskInfo::resumed`、`TaskInfo::resumable`；STATUS-12、STATUS-13；11.5.11 的其余测试；README 和 `docs/task.md`。
3. 提交号暂定，冲突时顺延。task-api-spec §4.3 的初始化顺序和 TASK-8 的修改随 C31（锁、阶段槽）和 C33、C20（准备和检查）落地。每一步照常跑 fmt、四种 clippy 配置、两次工作区测试、doc；C32 另跑基准。

---

## 12. 风险与已知限制

| 风险 | 缓解 |
| --- | --- |
| P0-2 和 P0-1 改动了公开的错误类型，下游的 `match` 会编译失败 | 在同一个 PR 里加上 `#[non_exhaustive]`，以后再扩展就不会破坏兼容；CHANGELOG 标明 breaking |
| 有些服务器的 `Content-Range` 写得不规范（比如省略 `bytes` 前缀、带多余空格），会被严格校验拒掉 | 解析器容忍空白和大小写差异，其它格式偏差一律拒收。宁可回退到单连接，也不能写坏文件 |
| `max_consecutive_failures` 的默认值在多核机器上会变小（例如 32 核机器从 64 降到 16） | 加了每区间预算之后，连续失败计数只是最后一道保险，影响有限；写进 CHANGELOG |
| `Doubling` 在某些网络环境下可能比 `Legacy` 差 | 默认值切换以 T14 的基准测试数据为准，在那之前 `Legacy` 一直是默认。G7 按 §14.18 的判据达标后已切换（§14.19、§14.20），`Legacy` 已删除，没有退路 |
| 令牌桶粒度是一帧（emit 每次推送的大小），低限速时可能有抖动 | 桶深设下限；G8 的误差判据按 2 s 窗口计算 |
| HTTP 客户端自动解压时会删掉 `Content-Encoding`，§4.1 的编码检查看不到（见 §14.2） | 所有请求都带 `Accept-Encoding: identity`；adapter 关闭 ureq 的 `gzip`；`limit_range_body` 要求 body 长度等于 `Content-Range`，不符就报 `RangeBodyTooLong` 或 `RangeBodyTooShort`；解不开的 body 不重试。这些只保证任务失败而不写错。T5 起，会解压的客户端回退到单连接后能下完；C22 起，不解压的客户端由引擎解码后也能下完 |
| 服务器把 `.tar.gz` 这类压缩包标成 `Content-Encoding: gzip`，解码后用户拿到的是解压后的文件 | §4.4 的例外：`Content-Type` 本身就是这种压缩格式时，按原样写入 |
| D11 把 HTTP 函数从 core 和 utils 移到 `bolt-load-http`，外部 adapter 的 import 会编译失败 | 提交信息加 `!`，列出新旧路径对照（§4.5 的表）；函数签名不变，只改 import 和依赖 |
| 解压炸弹：很小的响应解出极大的文件 | 每次 poll 最多产出一块输出，内存有上界；`DecodeOptions::max_output` 可以给输出总量设上限 |
| 没有长度、靠关连接结束的 body 在第一个字节之前断开，会存成空文件（§14.7） | HTTP 层测不出这种截断，断在之后任何位置也一样放行；IDM 同样按完成处理。原来的 `Empty` 只拦下了断在第一个字节之前这一种，代价是合法的空文件全部失败 |
| IDM 的部分结论只有 dsh 一个来源（ECONNRESET 计数、HTTP 低限速时的初始连接数） | 本设计不依赖这些结论；需要用到的参数都通过自己的基准测试来定。对冲的触发条件和胜负规则由 IDA 静态分析确认（2026-11-06）。那次动态测试只看到第二个请求从停顿的位置开始，当时原连接还开着，没有看到胜负。§9.6 只借用它的结构，参数自己校准 |
| 静默规则的误判一：一次短暂故障之后只剩一个连接还在下载，它佐证了其它连接的拒绝，上限降到 1（§8.3） | 恢复步骤每 15 s 升一级；D21 接受这个代价 |
| 静默规则的误判二：排在满 writer 后面的帧在拒绝之后才被接收，也算作进度 | 只在 writer 能接收数据时降级；误差上限是每个 runner 32 帧的队列 |
| 服务器接受连接之后不发数据或重置，§8.3 检测不到（D26） | 照旧按区间退避和预算处理；要检测，runner 先得报告是哪种超时 |
| §8.3 的常量都没有校准（D23） | 用基准测试校准；在那之前按设计值 |

---

## 13. 不在范围

- 收尾阶段以外的冗余对冲。IDM 在还有可切的活时，也会按本次下载的卡顿统计（均值 + 3σ）对冲；这里只做 §9.6 的收尾版（D5）。
- 每个分片一个临时文件、最后 `JoinFile` 合并；用注册表做 journal。
- 尾片合并（`find_merge_candidate`）：依据不成立，见改进方案 §2.3。
- IDM 的常量：11680 / 23360 / 46720、30 / 120 / 300 s、`2Q+3`、64 KiB 对齐。
- 每次切分前检查剩余磁盘空间。
- 日程配额（MB / 时间窗）、优先级队列、按任务加权的公平分配。
- 自适应量子 Q。IDM 的 Q 只在两档之间切换（11680 B 和 116800 B，2026-11-06 更正，原先以为是固定值）；切分尺寸这里改用 §9.3 的单连接速率估算。

---

## 14. 实现说明

### 14.1 读 body 的错误按原因分类（PR-2）

T4 的截断场景暴露出两个 adapter 不一致：body 读到一半连接断开，reqwest 报 `Retryable::Io`，ureq2 的 `UreqStream` 却把所有读错误都报成 `Unretryable::Io`，任务直接失败（违反 G2）。PR-2 补了一个提交，两个 adapter 都按原因分类：

- **ureq2** 按 `ErrorKind` 分：`UnexpectedEof`、`ConnectionReset`、`ConnectionAborted`、`BrokenPipe`、`TimedOut`、`Interrupted`、`NetworkDown` 归为 `Retryable::Io`，其它（例如解码失败的 `InvalidData`、`InvalidInput`）仍是 `Unretryable::Io`。
- **reqwest** 只要编译了任何一种解码器，所有 body 错误都会报成 `is_decode()`，明文 body 断开也一样，所以光看这个标志分不清。区分靠错误链（实测 reqwest 0.13.5 + tower-http 0.6.11）：
  - 连接断开：链上有 `hyper::Error`，最底下是 `UnexpectedEof`（`IncompleteBody`）→ `Retryable::Io`；
  - 数据解不开：链上只有解码器自己的 `io::Error`，头部不对是 `InvalidData`，流没结束就没数据了是 `UnexpectedEof` → `Unretryable::Io`。重试只会再次解不开。

  为了识别 `hyper::Error`，adapter 在 `reqwest` feature 下直接依赖 hyper 1（reqwest 本来就依赖它，不增加编译量）。

  改之前，解不开的 body 按可重试处理：服务器无视 `identity` 时，会解压的客户端拿到从中间开始的编码区间，会一直重试，直到 60 s 无进展超时（`NoProgress`）才失败。§14.2 记录了实测结果。

### 14.2 自动解压绕过编码检查（PR-2）

按 RFC 9110 §8.4，`Content-Encoding` 是表示本身的属性，`Content-Range` 数的是编码后的字节。服务器无视 `Accept-Encoding: identity`、仍然返回编码数据时，会出现两个问题：起点不为 0 的区间单独解不开；起点为 0 的区间能解出明文，但明文的长度和偏移都和 `Content-Range` 对不上。

**会解压的客户端会把编码藏起来（已读源码核实）：**

- ureq 2 默认开启 `gzip` feature：响应带 `Content-Encoding` 就自己解压，并删掉 `Content-Encoding` 和 `Content-Length`（`ureq-2.12.1/src/response.rs:606-608`）。
- reqwest 0.13 的解压层（tower-http 0.6）只看**响应**的 `Content-Encoding` 和 Client 配置，不看请求里的 `Accept-Encoding`，解完同样删掉这两个头（`tower-http-0.6.11/src/decompression/future.rs:40-71`）。adapter 用的是用户传入的 `Client`，用户开了 `gzip` 就会解压。

这时 §4.1 的编码检查看不到任何问题。最坏的情况是单连接请求 `0..total`：明文被截断到 `total` 写进文件，任务却报告成功（违反 G1）。

**PR-2 的处理：**

1. 所有请求都带 `Accept-Encoding: identity`（§4.1）。守约的服务器不会再编码。
2. adapter 依赖 ureq 2 时关闭默认 feature（只保留 `tls`）。如果用户的依赖图里有别的 crate 打开了 `gzip`，feature 合并后它还会生效，所以这一条只能算补充。reqwest 的 `Client` 由用户提供，adapter README 写明要关闭解压（`no_gzip()` 等方法在对应 feature 关闭时也存在）。
3. `limit_range_body`（§4.1）要求 body 长度正好等于 `Content-Range` 声明的长度。这个检查在 core 里，自定义 adapter 也能用上。
   - 数据可压缩时，明文比编码数据长，报 `RangeBodyTooLong`。上面那种静默写错因此变成一个有类型的失败。
   - 数据不可压缩时，gzip 反而让数据变长，解出的明文比 `Content-Range` 短，报 `RangeBodyTooShort`。
4. 解不开的 body 不再重试（§14.1）。不从 0 开始的编码区间单独解不开，reqwest 以前把它当成可重试的错误。
5. 测试：
   - `RangeFault::EncodedRepresentation` 对所有请求都返回 gzip 编码后的完整表示，长度和区间都按编码后的字节算。
   - `bolt-load-adapter/tests/content_coding.rs`：
     - 会解压的 reqwest，数据可压缩时得到 `RangeBodyTooLong`；数据不可压缩时，完整区间得到 `RangeBodyTooShort`，从中间开始的区间得到 `Unretryable::Io`；
     - 不解压的 reqwest 和 ureq2 都看到编码头，得到 `RangeMismatch`。
   - `bolt-load-adapter/tests/body_decoding.rs`：解不开的 body 不可重试，连接断开可以重试，无论客户端解不解压。
   - `range_faults.rs`：两个客户端在 `Gzip` 和 `EncodedRepresentation` 场景下都以 `RangeMismatch` 失败。

   T5 把其中带编码的 `RangeMismatch` 换成 `RangeContentCoded`，解不开的 `Unretryable::Io` 换成 `BodyUndecodable`，并让这些场景回退到单连接（§14.3）。

**原先的盲区（已处理）：** 第 3、4 条之前，数据不可压缩时，解出的明文比 `Content-Range` 短，会被当成短读。任务没有写错完成，而是卡住了：剩下的区间从压缩流中间开始，解不开；reqwest 又把解码错误当成可重试的，于是一直重试，60 s 无进展后以 `NoProgress` 失败。实测（reqwest 0.13.5、gzip、4 KiB 到 4 MiB 的随机数据）每次都是这样，没有留下文件。现在第一个响应就以 `RangeBodyTooShort` 失败，从中间开始的区间以 `Unretryable::Io` 失败。

**还没解决的：** 上面这些都只是让任务失败。要像 IDM 那样把文件下完，还需要两件事，见 §4.3 和 §4.4：

- 遇到这些错误就回退到单连接，文件大小按未知处理；
- 客户端不解压时，自己解码。

**IDM 的做法（IDA 分析 IDMan.exe）：**

- 默认对所有请求发 `Accept-Encoding: identity`（`sub_58B500`，`0x58c452`–`0x58c4c9`）。只有几个特定的任务标志会让它改发 `gzip, deflate`、不发这个头，或者发自定义值。
- 响应的 `Content-Encoding` 以 `gzip`、`x-gzip`、`deflate` 或 `x-deflate` 开头时，给任务打上"已编码"标志（`sub_58C9C0`，`0x58cd6e`–`0x58ce05`）。之后：
  - 在写文件这一层流式解码（`sub_570340`）；
  - 连接数降为 1（`sub_59C7C0`，`0x59feae`–`0x59fed5`）；
  - 文件大小取磁盘上实际写入的字节数，不再用 `Content-Length`（`sub_59A170`，`sub_59A790`）。
- 结论：IDM 不报错，而是降到单连接。bolt-load 采用同样的策略：T5 的单连接回退（D2）同时覆盖"带非 identity 编码的 206"。各个分支的结构置信度高，几个标志位的具体含义置信度低。

### 14.3 单连接回退（PR-3）

**状态里怎么体现降级。** `task-api-spec.md` 的状态模型只有一个 watch 通道，状态是电平触发的，中间值可能被合并。候选有三个：新增一个状态、在 `TaskStatus` 上加字段、在 `TaskInfo` 上加字段。选了最后一个：

- 降级改变的是初始化的决定（模式、总大小），这正是 `TaskInfo` 描述的内容。
- `TaskInfo` 原来只在初始化结束时设一次。现在回退时整体替换一次，`StatusPublisher` 里从 `OnceLock` 改成 `Mutex<Option<Arc<_>>>`。`TaskOutput::mode` 取自最终的 `info`，所以回退后是 `Singleton`。
- 新增状态会破坏 STATUS-1 的状态图，而且观察者可能因为合并根本看不到它；字段在最终状态里一直保留，不会丢。
- 回退后进度从零开始，`downloaded` 会变小。STATUS-9 把这一点写成规则：只会和 `info` 的替换一起发生。

**meta 怎么带出编码。** §4.3 要求 meta 阶段看到编码就直接走 Singleton，但原来的 `BoltLoadAdapterMeta` 没有地方放。新增 `content_encoding: Option<String>`（结构体本来就是 `#[non_exhaustive]`，不算破坏性变更），`with_content_encoding` 会丢掉 `identity`。

- reqwest：探测请求被 `RangeContentCoded` 拒掉时，记下探测响应的编码；HEAD 响应有编码时优先用 HEAD 的。
- ureq2：取 HEAD 响应的编码。
- 引擎：编码存在时，即使 `prefer_mode(Singleton)`，总大小也按未知处理，因为 meta 里的长度数的是编码后的字节；只有不是用户指定的 Singleton 时，才记 `fallback: Some(ContentCoded)`。

**错误类型。** `DownloadError::RangeUnsupported` 除了 `reason`，还带触发它的 `source: AdapterError`，回退时写进日志。回退之后任务不会再以它结束，它只在引擎内部传递。

**错误分类。** `RangeBodyTooLong`、`RangeBodyTooShort`、`BodyUndecodable` 都记为 `FallbackReason::ContentCoded`：它们绝大多数来自会解压的客户端。服务器算错长度也会触发，这时回退到单连接同样安全。ureq2 只把 `InvalidData` 记为 `BodyUndecodable`，`InvalidInput` 仍是 `Unretryable::Io`，不回退。

**契约函数。** 和 `validate_range_response` 一样放在 core：`check_content_coding(content_encoding, content_type)` 给 `full_stream` 用，`is_identity_coding` 给 meta 用。§4.4 的例外只在只有一层编码、并且 `Content-Type` 正好是这种编码的文件格式时成立：gzip、x-gzip 对应 `application/gzip` 和 `application/x-gzip`（RFC 6713），deflate 对应 `application/zlib`（RFC 6713），zstd 对应 `application/zstd`（RFC 8878）。br 没有注册的文件格式类型，不设例外。

**回退时的文件。** 并发阶段失败后，`OutputFile::commit` 照常删掉临时文件；Singleton 用同一个路径重新创建，打开时还会截断。并发阶段删除失败留下的文件，同样会被截断覆盖，所以不单独上报。

**测试。**

- `RangeFault::ReplacedAfterShortRange`：第一个 range 请求只回一半，之后所有请求都以 `200` 返回另一份更短的内容（原内容的最后 1/16）。替换内容必须比并发阶段写下的数据短：第一版用原内容的后一半（128 KiB），比并发阶段写下的 96 KiB 还长，Singleton 从头写就把残留盖住了。去掉"删除临时文件"和"打开时截断"这两层保护之后，那一版测试照样通过。缩短之后，同样的改动会让测试失败，留下的是 98304 字节的文件。
- `range_faults.rs` 加了会解压的 reqwest 客户端，每个场景都跑三个客户端。

### 14.4 HTTP 辅助 crate（PR-3a）

C23 按 §4.5 的表搬迁，有两处表里没有写到：

- **`ContentRange::unsatisfied_complete` 公开。** `validate_range_response` 要从 416 的 `bytes */total` 里取总长度。这个解析原来是 core 里的 `pub(super)` 函数，和 `ContentRange` 共用 `bytes` 单位与数字的解析。§4.5 把 `ContentRange` 的解析留在 core，所以把它改成 `ContentRange` 的公开关联函数，不在 `bolt-load-http` 里再写一份。
- **core 自己判断 `identity`。** `BoltLoadAdapterMeta::with_content_encoding` 要丢掉 `identity`，而 core 引用不了 `bolt-load-http`。所以 core 里加了一个私有的 `is_identity`，只有几行。`bolt-load-http` 的 `is_identity_coding` 照常搬走，到 C24 由 `ContentCoding::parse_list` 取代。

### 14.5 解码 crate（PR-3b，C21）

- **后端接口。** `compression-codecs` 0.4.41 的 `DecodeV2` 是推式的：`decode` 从 `PartialBuffer<&[u8]>` 读，写进 `WriteBuffer`，返回编码是否结束；另有 `flush`、`finish` 和 `reinit`。crate 只在内部用它，依赖写成 `0.4.41`，不会升到 0.5。
- **`Invalid` 带 `Arc<io::Error>`。** §4.4 写的是 `io::Error`。改成 `Arc` 是为了让 `DecodeError` 能 `Clone`，和 `AdapterError` 一致，C22 也可以直接把它放进 `BodyUndecodable`。
- **`deflate` 的两种格式。** tower-http 0.6.11 只按 zlib 解（`decompression/body.rs` 用的是 `ZlibDecoder`），不兼容 raw deflate。这里按 §4.4 实现：先看头两个字节，是合法的 zlib 头（RFC 1950 §2.2：CM 为 8，窗口不超过 32 KiB，校验值能被 31 整除）就按 zlib 解，否则按 raw deflate 解。浏览器的做法没有在本次核对源码，置信度仍为中。
- **多成员。** gzip 的多个成员（RFC 1952 §2.2）和 zstd 的多个帧（RFC 8878 §3.1，包括可跳过帧）依次解码。tower-http 对 zstd 打开了 `multiple_members`，对 gzip 没有打开。一段编码结束之后，如果紧接着的字节不可能开始一个新的成员（gzip 不是 `0x1f`，zstd 不是 `0x28` 或 `0x50`–`0x5f`），报 `TrailingData`；可能开始但内容无效，报 `Invalid`。其它编码结束之后再有数据，一律报 `TrailingData`。
- **空 body。** 带编码的空 body 解成空文件，和 tower-http 一致（它的 `decompress_empty` 测试）。
- **截断。** 输入结束时编码还没结束，`finish` 的错误一律记为 `UnexpectedEof`：flate2 在这种情况下报的是 `BufError`，不是 EOF。
- **公开常量。** 新增 `MAX_LAYERS = 2` 和 `DEFAULT_BUFFER_SIZE = 64 KiB`，并重新导出 `ContentCoding`。`buffer_size` 为 0 时按 1 处理。
- **输出缓冲用安全代码。** 每次往缓冲里写之前，先 `resize` 到 `buffer_size`。这会把空闲部分清零，每块多一次 memset，但不需要 `unsafe`。
- **解码器手里还有输出时。** 输出缓冲写满时，解码器可能还留着已经解出的数据。下一轮在读新输入之前先调用 `flush`。去掉这一步，`hands_out_all_it_can_before_the_body_continues` 会失败：body 发完数据之后不再发送，也不结束，这时解码结果必须能全部取出。gzip 和 zstd 在输出满时不会读完最后的输入（用 7 字节的输出缓冲、300 KB 的输入探测过），所以这一步对它们没有作用。
- **验证。** proptest（新增的 dev 依赖）对每种编码随机切块，`buffer_size` 在 0 到 5000 之间随机取；另有逐字节输入、两层编码、多成员、截断、损坏、多余数据、`max_output` 和 8 MiB 全零输入的测试。任务里的基准测试，以及 `range_faults.rs` 的 `EncodedRepresentation`，要用到引擎里的解码路径，放在 C22 做。

### 14.6 引擎接入（PR-3b，C22）

- **返回类型。** core 新增 `FullResponse { body, content_encoding, content_type }`，`full_stream` 改为返回它。它是 `#[non_exhaustive]`，用 `new` 和 `with_*` 构造。头值是合并后的原始值，core 不过滤 identity，由引擎解析。`content_encoding` 描述交出来的 body：客户端解压并删掉头时为 `None`。
- **决定解什么。** `bolt-load-http` 的 `check_content_coding` 换成 `codings_to_decode(content_encoding, content_type) -> Vec<ContentCoding>`，按应用的顺序返回要解的编码。没有编码，或者属于 §4.4 的例外，返回空列表。adapter 不再做检查，只交出头值；旧函数按全局约束 3 直接删除。
- **错误。**
  - `decode` 报 `Unsupported`（不认识的编码、对应 feature 没开、超过两层）时，任务以 `ContentCoded { coding }` 失败，`coding` 仍是合并后的 `Content-Encoding`。这个变体的含义从“客户端没解”改成“任务解不了”。
  - 流里的 `Invalid` 和 `TrailingData` 记为 `BodyUndecodable`：`Invalid` 直接带上它的 `Arc<io::Error>`，其余包成 `InvalidData`。没有新增变体。`DecodeError` 到 `AdapterError` 的转换在引擎里，用一个私有的包装类型实现，解码 crate 仍不依赖 core 的错误类型（§4.4）。body 自己的错误原样传出。
  - 引擎不设 `max_output`：文件边解边写到磁盘，内存只有每层一块缓冲，所以不会出现 `TooLarge`。
- **解码后大小未知。** meta 里的大小数的是编码后的字节。meta 没看到编码而 GET 带了编码时（比如服务器只对 GET 编码），单连接的 `total` 原本是已知的。runner 读到 `total` 就停，所以解码后更长的文件会被截断，任务却以 `Completed` 结束，违反 G1；解码后更短的，会报 `SmallerThanTotalSize`。做法：
  - singleton 的 connector 调用 `full_stream` 并解码，用一个 `AtomicBool` 记下是否解了码。
  - runner 的 `run_after_started(ready)` 改为由 `ready` 交出 `total`，替换 builder 的值。singleton 不再在 builder 上设 `total`，而是在 `Started` 之后决定：解了码就把 `total` 置为 `None`，于是不预分配文件，进度里的 `total` 也变成未知；然后把 `total` 交给 runner。
  - `task-api-spec.md` 新增 STATUS-10 记下这条规则。`info` 不变，因为这不是回退。
  - `test_a_coded_full_response_is_decoded_with_an_unknown_total` 覆盖解码后更长和更短两种情况。去掉置 `None` 那一行，这个测试失败。
  - 当时的已知限制：解码后为空的文件（比如 gzip 编码的空文件）走 runner 现有的规则，大小未知又没有收到字节，以 `Empty` 失败。大小未知的空 body 本来就是这样，这次没改，§14.7 解决。
- **feature。** 引擎新增 `gzip`、`deflate`（默认开启）和 `br`、`zstd`，转发到 `bolt-load-decode`，后者以 `default-features = false` 引入。关掉 `gzip` 后，gzip 编码的完整响应以 `ContentCoded` 失败。
- **验证。**
  - `range_faults.rs`：`EncodedRepresentation` 在三个客户端上都完成，hash 正确，回退原因是 `ContentCoded`。
  - adapter 测试：不解压的客户端在 `FullResponse` 里报告 `gzip`，body 是编码后的字节；解压的客户端报告 `None`，body 是原文。
  - 基准测试 `crates/bolt-load-decode/benches/against_reqwest.rs`：本地服务器上的同一份 gzip 响应，每次迭代解出 32 MiB，一边由 reqwest 自己解码，一边由不解压的 reqwest 加 `decode` 解码。Windows 11 本机，criterion 中位数如下，text 跑了两次：

    | 数据 | reqwest | `decode` | 比例 |
    | --- | --- | --- | --- |
    | counter（`u64` 计数，压缩比高） | 330 MiB/s | 365 MiB/s | 110% |
    | text（62 个常用词随机排列） | 248、277 MiB/s | 233、270 MiB/s | 94%、98% |

    置信区间约 ±8%，“不低于 90%”的结论置信度为中高。只测了 gzip：dev 依赖的 reqwest 只开了 gzip，故障服务器也只发 gzip。br 和 zstd 没有测。

### 14.7 空 body（#153）

- **问题。** 大小未知（meta 的大小为 0，或者 §14.6 解了码）的流正常结束、却一个字节都没收到时，runner 报 `Empty`。所以 `Content-Length: 0`、空的 chunked、靠关连接结束的空 body，以及解码后为空的文件，都下载失败。
- **IDM 的做法。** 用本地服务器对 v6.43b16 实测了这五种响应：IDM 都写出 0 字节文件，注册表里的 `Status` 都是 3，和其它已完成的下载相同。它请求时带 `Accept-Encoding: identity`，服务器照样编码时自己解码；对编码的响应只发一次请求，不再跟 `Range`。
- **做法。** runner 在 `total` 未知时接受任何长度，包括 0（STATUS-11）。流不带错误地结束，说明 HTTP 层已经认为 body 完整：`Content-Length` 收满、chunked 收到最后一块、编码的尾部校验通过。原规则只拦得下“靠关连接结束的 body 在第一个字节之前断开”，这种截断任何客户端都测不出来，断在第一个字节之后照样放行（§12）。
  - singleton 在 `Started` 时已经建好临时文件，完成时报告 `total = Some(downloaded)`，所以空 body 得到 `total == Some(0)` 的空文件。
  - 并发模式的 runner 总有已知的 `total`，不受影响。
  - `TaskError::Empty` 和 `is_empty` 不会再用到，直接删除。`TaskError` 是公开类型，又不是 `#[non_exhaustive]`，所以提交信息带 `!`。
  - meta 的大小仍用 0 表示未知，#153 的方案 1（改成 `Option<u64>`）没有做。它解决不了 chunked、关连接和解码后为空这几种情况，只是让 `Content-Length: 0` 从一开始就显示大小 0。续传需要比较大小时再改。
- **验证。**
  - `tests/empty_body.rs` 用原始 TCP 服务器发上面五种响应和 `204 No Content`，在三个客户端上各跑一个完整任务，都以 0 字节文件和 `total == Some(0)` 完成。两种 gzip 响应只在 `gzip` feature 打开时跑。三个客户端移到 `tests/common/mod.rs`，与 `range_faults.rs` 共用。
  - 单元测试：runner 对没有任何项、只有空帧的流，`total` 未知或为 0 时都完成，为 3 时报 `SmallerThanTotalSize`；`test_zero_size_file_handling` 取消 ignore；`test_zero_size_selects_singleton_mode` 改为断言下载完成；`test_a_coded_full_response_is_decoded_with_an_unknown_total` 加上解码后为空的情况。
  - 把旧规则放回去，这四个单元测试和 `empty_body.rs` 都失败。
- **204。** 完整下载接受 4xx、5xx 以外的所有状态码，所以 `204 No Content` 也存成空文件。用户 2026-11-05 确认保留这个行为，不要求只接受 200；`empty_body.rs` 覆盖了它。IDM 怎么处理 204 没有测。

### 14.8 写错误（PR-4）

- **做法。** 并发模式的写完成按 §6 改为 `TaskError::Write { range, source: Arc<io::Error> }`，保留区间和 `io::ErrorKind`。写入器的错误先取出背后的 `io::Error`；工作队列关闭不是 io 错误，记为 `BrokenPipe`，和提交到已关闭队列的写一致。
- **与 §6 的偏差：singleton 不报 `Write`。** singleton 写的是 `async_fs::File`：写入先进 `blocking::Unblock` 的 8 MiB 管道，再由后台线程落盘，落盘失败要到之后某次写或 flush 才报出来。这个失败对应不到区间，所以 singleton 继续报 `DownloadError::Io`（`IoOp::Write` 或 `IoOp::Flush`），同样保留 `ErrorKind`。两种模式的形状不同，`task.md` 和 task API spec 都写明了。
- **提示。** `StorageFull` 和 `QuotaExceeded` 在 `Write` 和 `Io` 的消息末尾加一句提示：腾出空间或提高配额，再重新下载。`Io` 也加，因为 singleton 的磁盘满落在 `Io` 上。
- **不重试。** 写失败不经过 `FailurePolicy`：下载循环遇到第一个失败的写完成就结束。不只 `StorageFull` 和 `QuotaExceeded`，所有写失败都这样，和改动前一致。`Write` 的 `is_retryable()` 为 false。
- **没有做。** 并发模式创建文件、预分配（`set_len`）和 finalize 的失败仍是 `Transfer(Other { message })`，`ErrorKind` 丢失。§6 只覆盖写完成，这几处由 PR-4a 改成 `Io`（§14.9）。
- **验证。**
  - `completion.rs` 的单元测试：注入一个只让 4..8 失败的 writer，失败的 kind 分别是 `StorageFull`、`QuotaExceeded`、`PermissionDenied`。经 `PendingWriter` 写 0..4、4..8、8..12 后，`check_completions` 返回 `Write`，区间是 4..8，`ErrorKind` 和消息都不变。队列关闭的写报 `BrokenPipe`。
  - 下载循环的优先级测试改为要求写失败以 `Write { range: 0..1 }` 结束循环。
  - 消息测试：两种 kind 带提示，其它 kind 不带；`Write` 不可重试。
  - 把 `Other` 的映射放回去，三个测试失败；去掉提示，两个消息测试失败。

### 14.9 并发模式的文件错误（PR-4a）

- **问题。** §14.8 只改了写完成。并发模式创建临时文件、预分配（`set_len`）和 finalize（等写入线程退出，再 `sync_all`）的失败仍是 `Transfer(Other { message })`，`ErrorKind` 丢失。在不建稀疏文件的文件系统上（例如 Windows 的 NTFS），预分配会实际占用空间，磁盘满可能先在这一步报出来（推断，未实测）。
- **做法。** 用户 2026-11-05 决定把这几处一并迁移，单开 PR。它们和 singleton 一样报 `DownloadError::Io`，路径是临时文件，保留 `io::ErrorKind`：
  - 打开或创建失败报 `Create`。给写入线程复制句柄、建立映射的失败也走这条路，一并报 `Create`。
  - 预分配失败报 `Allocate`。compio writer 不预分配，没有这一步。
  - finalize 失败报 `Flush`：它等写入线程退出，再把文件同步到磁盘，对应 singleton 的 flush。工作队列关闭记为 `BrokenPipe`，和 §14.8 一致。
  - `StorageFull` 和 `QuotaExceeded` 因此在这几步也带 §14.8 的提示。
  - 类型没有变：`DownloadError` 是 `#[non_exhaustive]`，`IoOp` 也没加变体。但按 `Transfer(Other)` 匹配这些失败的代码会匹配不上，PR 里写明了。
- **保留的 `Other`。** 其余的 `Other` 都不是文件错误，背后没有 `io::Error` 可以保留，所以不改：
  - 写入器队列满（`writer_full`）、写完成通道关闭：都是内部不变式被破坏。
  - runtime 拒绝 spawn runner：只有一段消息，作为这个 runner 的连接失败交给 `FailurePolicy`。
  - singleton 的 runner 在连接前报完成：runner 在 `Started` 之前的停止都带错误，按流程不会发生。
  - writer 构建参数校验失败（`Validation`）：`FileWriter::new` 选出的 writer 都拿到了文件或路径，只有测试用的 writer 会报。
  - 原来“并发任务没有大小”这一处，改为直接传入大小，删掉了。
- **验证。**
  - 在不存在的目录里建文件：报 `Io { op: Create }`，kind 是 `NotFound`，路径是临时文件。default features（mmap）和 all-features（compio）下都跑。
  - pool writer 把文件设为 `u64::MAX` 字节：报 `Io { op: Allocate }`。`FileWriter::new` 选的 writer 可能不预分配，所以这里直接指定 pool。
  - 注入一个 finalize 失败的 writer：报 `Io { op: Flush }`，kind 和消息不变。
  - 把建文件或 finalize 的映射改回 `Other`，或者把 `AllocateFile` 映射成 `Create`，各自对应的测试失败。

### 14.10 连接上限（PR-5，C7）

- **D1。** 用户 2026-11-05 确认：`max_connections` 默认固定为 8，与 CPU 核数无关。
- **做法。**
  - `TaskOptions` 新增公开字段 `max_connections: NonZeroUsize`，默认值 `DEFAULT_MAX_CONNECTIONS = 8`，由 `TaskOptions::max_connections(n)` 和 `TaskBuilder::max_connections(n)` 设置。测试专用的 `max_concurrency` 删掉了。
  - `DownloadLoop::new` 改为读这个值，`RunnerManager`、`StrategyControl` 和 `PendingWriter` 的容量都随它确定。`available_parallelism()` 和 `DEFAULT_MAX_CONCURRENCY`（4，只在查询 CPU 核数失败时使用）一起删掉。
  - 失败预算：`max_consecutive_failures` 改为 `Option<usize>`。`None` 表示用默认值，任务开始时由 `resolved_max_consecutive_failures()` 按 `max(8, 2 × max_connections)` 算出。这样 `max_connections` 和 `max_consecutive_failures` 的设置顺序不影响结果，显式设置的值也不会被覆盖。字段类型变了，所以提交信息带 `!`。
  - 默认值的变化：连接上限从 CPU 核数变成 8，失败预算从 `max(8, 2 × CPU 核数)` 变成 16。4 核机器上，连接上限从 4 变成 8，预算从 8 变成 16；32 核机器上，连接上限从 32 变成 8，预算从 64 变成 16（§12 已列出）。
  - `TaskBuilder::max_connections` 只改 `options` 里的这一项，之后再调用 `options(...)` 会整体替换掉它，文档里写明了。
  - 内部仍叫 `max_concurrency`：它同时是策略在运行中调低、再调回的当前上限，起点是 `max_connections`。
- **验证。**
  - 默认值是 8，失败预算默认 16。`max_connections` 为 1、4、5、8、32 时，预算分别是 8、8、10、16、64；显式设置的 3 不随它变化。
  - `FailurePolicy` 在 `max_connections = 16`、未设预算时，第 32 次连续失败才终止任务。
  - 下载循环的测试改为像任务一样，用 `DownloadLoop::new` 和 `max_connections` 构造；原来是构造之后再改字段。`max_connections = 1` 时，策略在 2 MiB/s 下跑 64 个 tick 也不拆分；每个 tick 之后，活跃 runner 数都不超过上限。
  - 整个任务级别的测试要单连接下载 4 s 以上，策略才会拆分，太慢，所以“只有一个 runner”放在下载循环里测。
  - 变异检查：下载循环不读配置（固定为 8）、默认预算不随 `max_connections` 变化、`FailurePolicy` 不用解析后的值、`TaskBuilder::max_connections` 不写入，各自都有测试失败。
  - `rg available_parallelism crates/bolt-load/src/task` 只剩 pool writer 的写入线程数，与连接数无关。

### 14.11 容量安全的切分（PR-5，C8）

- **问题。** 旧的 `split()` 先缩小原 runner，再去分配新 runner 的 ID；分配不到就 `expect` panic。策略的闸门看的是活跃 chunk 数，而 runner 的 ID 要等它停止才释放。所以 chunk 已经下完、`Stopped` 还没处理的 runner 仍占着 ID，策略却以为还有空位。旧代码在这个时序下会 panic：`strategy_tick.rs:99`，`failed to allocate runner with chunk`。用下面的测试在旧代码上复现过。
- **做法。**
  - `RunnerManager` 新增 `try_reserve_slot()`（原来的 `allocate_runner`，返回 `#[must_use]` 的 `RunnerSlot`）、`release(slot)` 和 `spawn_in(slot, range, spawn)`。`spawn_in` 分配不到区间时会释放槽位，返回 `false`。`spawn_runner_with_chunk` 只剩测试在用，改为 `#[cfg(test)]`。
  - `split()` 分三步：先占槽位，占不到就记一条 `trace` 并返回，什么都不改；再 resize，失败就释放槽位；最后 `spawn_in`。如果第三步分配不到后半段，后半段留作未分配区间，由并发控制策略之后为它创建 runner，不会丢失。
  - `spawn_runner`（`CreateTask`）占不到槽位时记一条 `trace` 后跳过，不再 `expect`。
  - `SplitGivenTask` 的目标 runner 已经没有 chunk 时跳过，去掉了 `expect("task id not found")`。tasks.md 没列这一项，但它也是同一类 panic。
  - `find_chunk_to_split` 返回 `SplitCandidate::{Unassigned, Running { runner, range }}`。策略只处理 `Running`，`Unassigned` 交给并发控制策略，去掉了 `strategy/mod.rs:88` 的 `expect`。
  - 闸门：QuickStart 改为 `current × 2 <= max`；Normal 改为 `current < max`，等价于 `current + 1 <= max`，这样写是因为 clippy 的 `int_plus_one` 会拒绝 `+ 1 <=`。最后一个槽位现在也能用上。
- **验证。**
  - 回归测试：
    - 槽位全被占用时，`SplitAllTask`、`SplitGivenTask` 和 `CreateTask` 什么都不改：区间不变，不发限额，不丢区间。腾出一个槽位后，`CreateTask` 又能成功。
    - 4 个连接中，2 个 runner 已经下完但还没停止，策略 tick 时不拆分；其中一个停止后，同一次 `SplitAllTask` 里第一个拆分用掉这个槽位，第二个跳过。旧代码在第一次 tick 就 panic。
    - `test_rejected_split_spawn_reclaims_its_slot_once`：拆分出的 runner 被 runtime 拒绝，在下载循环处理之前一直占着槽位，期间另一次拆分用的是别的槽位。runner manager 的生命周期轮询报告这次拒绝后，只收回它一个槽位；原 runner 保持缩小，后半段重新可分配。被拒绝的 spawn 会让任务终止，下载循环不会再建 runner；测试随后手动建一个 runner，确认收回的槽位和后半段都能再分配。
  - 闸门的单元测试：2/4 时 QuickStart 拆分全部，3/4 时 Normal 拆分一个，4/4 和 1/1 时不拆分。
  - 性质测试：`proptest`（工作区已在用，bolt-load 新增为 dev 依赖），256 组，每组最多 64 步。随机交替执行建 runner（包括区间已被占用的情况）、`Started`、进度、完成、失败、`SplitAllTask`、`SplitGivenTask`（包括不存在的 runner）、`ChangeMaxConcurrency`（1 到 2 × 上限）和真实策略的 tick。每一步之后都检查：
    - 不 panic；
    - 占用的 ID 数和活跃 runner 数都不超过 `max_connections`；
    - 每个被占用的 ID 都属于持有 chunk 的 runner，没有泄漏；
    - chunk 和未分配区间恰好覆盖 `[0, total)`，互不重叠；
    - 已接收的字节数不减少。
  - 变异检查，每种都有测试失败：
    - 先 resize 再占槽位；
    - resize 失败后不释放槽位；
    - 区间被占用时不释放槽位；
    - 占不到槽位就 panic；
    - `SplitGivenTask` 找不到 runner 就 panic；
    - 恢复旧的 QuickStart 闸门；
    - 恢复旧的 Normal 闸门。

### 14.12 区间退避与 Retry-After（PR-6，C9）

- **做法。**
  - 数据模型：可用区间仍是 chunk 之间的空隙，`get_available_ranges` 的含义不变。`ChunkPlanner` 旁边加了一张表 `cooldowns: BTreeMap<u64, Cooldown { end, attempts, not_before }>`，只记失败后退回的区间。
    - 记录总在空隙里，互不重叠，也不相接，相接的会合并。
    - 不在任何记录里的可用字节是“新的”：`attempts = 0`，随时可以领取。
    - `not_before` 过了以后，记录仍然保留，直到被领取，这样 `attempts` 才能带进 chunk。
    - 新增 `get_eligible_ranges(now)`：空隙减去还没结束的冷却。结束了的冷却和旁边的新字节连成一段。
  - `attempts`：
    - `ChunkState` 增加 `attempts`，表示这个区间之前失败过几次。领取时，取它覆盖到的记录里最大的 `attempts`，新字节算 0。
    - `mark_failed` 改为返回 `Released { range, attempts }`，`attempts` 是 `chunk.attempts + 1`，饱和加。
    - 合并：退回的区间和它重叠或相接的记录合成一条，`attempts` 取较大值，`not_before` 取较晚值。这样任何字节都不会早于它原来的 `not_before` 被领取。旁边的新字节不并进来，仍然是新的。
    - 部分领取：领取的部分带走覆盖到的记录里最大的 `attempts`；记录在领取范围以外的部分，保留原来的 `attempts` 和 `not_before`。正常流程不会领取冷却中的区间（见门控），只有直接调用才会出现。
    - 切分：后半段从 0 开始。`resize` 让出的尾部原来属于 chunk，没有记录；`split_chunk` 的新 chunk 也从 0 开始。原 chunk 保留自己的 `attempts`。
  - 两步记录：runner manager 处理失败时只释放区间，`AttemptFailure` 带上 `range` 和 `attempts`。下载循环在 `on_runner` 里拿到 `now`，再算退避、写记录，两步之间没有别的代码运行。任务取消时 runner 不报失败，所以不退避。`attempts` 只决定退避；T10 的每区间预算不比较它，而是比较另一个计数 `charged`，见 §14.13。
  - 退避函数：backon 1.6.0（`Cargo.lock`）的 `ExponentialBuilder`，最小 0.5 s，×2，最大 30 s，`with_jitter()`。
    - 第 n 次失败（从 1 数）的基础延迟是 `min(0.5 s × 2^(n−1), 30 s)`。
    - backon 的抖动是加性的：`d + d × U[0, 1)`，结果落在 `[d, 2d)`；而且抖动加在 `max_delay` 截断之后。所以和 `meta.rs` 一样，结果再截到 30 s。第 n 次的等待在 `[d, min(2d, 30 s)]` 之间，从第 7 次起恒为 30 s。
    - 测试用 `Backoff::without_jitter()`（只在测试里有）验证精确值；开着抖动时验证上下界。
  - `congestion_until`：遇到 `Congestion { retry_after }` 时，延迟是 `min(max(retry_after, backoff), 120 s)`；没有 `Retry-After` 时就是 backoff。这个区间的 `not_before` 和任务级的 `congestion_until` 取同一个时刻。`congestion_until` 只会推后，不会提前。
  - 门控：
    - 拥塞期间，`StrategyControl::execute` 交给并发控制的可用区间为空：它仍然可以降并发，但不会下发 `CreateTask`。dynamic 策略的切分也跳过，因为切分也要开新连接。
    - 不拥塞时，并发控制只拿到 `get_eligible_ranges(now)`。
    - `DownloadLoop::spawn_runner` 和 `split` 自己也检查：拥塞时什么都不做，`spawn_runner` 还跳过和冷却记录重叠的区间。策略、唤醒和直接调用都绕不过。
    - `find_chunk_to_split` 只看可领取的区间。
  - 唤醒点：
    - 放在 `DeadlineSource` 里，没有新增事件源，事件优先级不变。计时器设在“无进展的截止时间”和“下一个可能开新连接的时刻”中较早的那个。
    - 下一个可能开新连接的时刻：拥塞中取 `congestion_until`；否则取晚于 `now` 的最早的 `not_before`。
    - 失败退避时，如果新的唤醒时刻早于计时器，就立即把计时器提前。
    - 醒来后调用 `StrategyControl::create_tasks`：只做并发控制在下一个 tick 会做的创建，每个可领取的区间一个 runner，最多到空闲槽位数，不推进任何策略的状态。第一次 step 之前不创建。一个不可重试的失败（比如 503）还没被下一个 step 拿去降并发时，也不创建；否则唤醒先把连接补满，降级就会把补满后的数当成新上限。为此 `RunnerOutcomeSampler` 新增了只读的 `unretryable_pending`。
    - 只设置晚于 `now` 的时刻。因为没有空闲槽位而没建成的区间不会让循环空转，由 1.5 s 的 tick 接手。
  - `now` 的传递：`handle_event` 取一次 `Instant::now()`，传给 `on_deadline`、`on_strategy_tick`、`on_runner`、`apply`、`split` 和 `spawn_runner`。`ConcurrencyControlContext` 增加 `now`，`has_recent_unretryable` 改为传入 `now`。sampler 记录结果时仍然在内部取当前时间，没有改。
  - 测试服务器：`bolt-load-tests` 新增 `RangeFault::CongestedOnce`：除探测外的第二个 range 请求回 503 和 `Retry-After: 2`，其它 range 响应限速 4 MiB/s，让任务在第一个连接还在传输时切分。新增 `create_range_fault_server_with_log(content, hold)`，返回服务器收到的请求和到达时间。`hold` 是可选的 `fault::Gate`：给了它，`CongestedOnce` 除探测外的第一个 range 响应先发出响应头，再把 body 停在这个 gate 上，直到测试放行；其它用法传 `None`，行为不变。`EncodedRepresentation` 用的 gzip 内容改为第一次用到时才编码，大文件的测试不再白白压缩一遍。
- **与 §8.1 的偏差。**
  - 没有用 `AvailableRange` 列表，而是在空隙旁边放冷却表。规则相同：`attempts` 跟着区间走，切分从 0 开始，合并取较大值。
  - §8.1 没定的两条：合并时 `not_before` 取较晚值；部分领取的规则见上。
  - `attempts` 是 `u32`，不是 `u8`。
  - 第一次失败的基础延迟是 0.5 s，即 backon 的第一个延迟；§8.1 的 `0.5 s × 2^n` 在 n = 1 时是 1 s。
  - 不是满抖动（AWS 的 full jitter 在 `[0, d]` 里取值）。用的是 backon 的加性抖动，再截到 30 s，见上。
  - 唤醒时刻：§8.1 说取最早的 `not_before` 和 `congestion_until` 中较早的那个。拥塞期间这里直接取 `congestion_until`，因为更早结束的冷却醒来也什么都做不了。
  - §8.1 只要求 `ConcurrencyControlStrategy` 跳过冷却区间。这里 `spawn_runner` 和 `split` 也检查；`find_chunk_to_split` 也跳过冷却区间，否则冷却期间（最长 30 s，拥塞时 120 s）dynamic 策略一直不会切分。
  - 唤醒后的创建要等不可重试的失败被降级的 step 用掉，见上。
- **已知限制。**
  - `Retry-After` 最多等 120 s，比默认的 `no_progress_timeout`（60 s）长。如果这期间没有别的连接在下载，任务会以 `NoProgress` 结束。这里没有改无进展的判定。
  - 并发控制收到不可重试的失败后，下一个 step 只降并发、不创建 runner。503 之后，最早要到这个 step 之后才会重连；`Retry-After` 比 tick 间隔短时，实际等待可能更长。`wake_up_before_the_degradation_leaves_the_retry_to_a_later_tick` 按显式时间走了一遍这个顺序。
  - 任务开始时只有一个完整的未分配区间，要等 dynamic 策略切分，才会第一次同时开两个连接。切分取决于速度采样和上一次的速度：QuickStart 阶段要求 `|speed − 2 × 上一次的速度|` 超过阈值，Normal 阶段要求 `|speed − (上一次的速度 + 每连接速度)|` 超过阈值。切分还要求剩余区间够大（QuickStart 至少 `max(2 × speed, 2 × min_chunk_size)`，Normal 至少 `min_chunk_size`）、并发够用（QuickStart 要求活跃 runner 数翻倍后不超过上限，Normal 要求小于上限），并且有空闲槽位。这些都由运行时的采样决定，公开 API 没法强制：慢机器上第一个连接可能在切分之前就下完了，503 根本不会出现。所以 503 场景的测试放在下载循环里，由测试自己切分（见验证）。原来通过公开 API 的 `retry_after_holds_back_new_connections` 删掉了。
- **验证。**
  - `Backoff` 的单元测试：不加抖动时依次是 0.5、1、2、4、8、16、30、30 s；加抖动时 200 次取样都在 `[d, min(2d, 30 s)]` 之内，并且确实有抖动；`Retry-After: 2` 时 `not_before` 和 `congestion_until` 都是 `now + 2 s`，`now + 2 s − 1 ns` 时仍拥塞；取 `Retry-After` 和 backoff 中较长的；120 s 上限；`congestion_until` 不会提前。
  - `ChunkPlanner` 的单元测试（显式传入 `now`）：
    - 失败的区间在 `not_before − 1 ns` 时不可领取，在 `not_before` 时可以，并且和后面的新字节连成一段；
    - 重新领取的区间带上失败次数，再失败时继续往上数；
    - 部分领取带走 `attempts`，剩下的两段保留原来的冷却；
    - 相邻的失败区间合并，`attempts` 取较大值，`not_before` 取较晚值，补上中间的空隙后三段合成一段；
    - 切分出的后半段 `attempts` 为 0，旁边的冷却不受影响；
    - 冷却中的区间不是切分候选，冷却结束后才是；
    - `PlannerGuard` 回滚时冷却表一起恢复。
  - 并发控制：第一次 step 之前、不可重试的失败还没被 step 用掉时，`create_tasks` 不创建；降级之后按空闲槽位创建。
  - 下载循环的确定性测试（显式传入 `now`）：
    - 连接中的 runner 丢失后，区间冷却 0.5–1 s，计时器设在冷却结束时；`not_before − 1 ns` 时 tick 和唤醒都不领取，计时器仍设在原处；`not_before` 时唤醒领取它，`attempts` 为 1，计时器回到无进展截止时间。第二次失败冷却 1–2 s，由 tick 领取，`attempts` 为 2。
    - 一个 runner 在传输、另一个收到 503 和 `Retry-After: 2`：任务继续，`congestion_until` 和区间的 `not_before` 都是 `t0 + 2 s`。在 `t0`、`t0 + 1 s`、`t0 + 2 s − 1 ns`，`CreateTask`、`SplitGivenTask`、`SplitAllTask`、2 MiB/s 下的 tick 和唤醒都不开新连接；到 `t0 + 2 s`，切分和创建都成功。
    - 拥塞结束时唤醒为退回的区间建 runner，不等 tick；拥塞期间唤醒什么都不做。
    - 冷却结束时没有空闲槽位：唤醒什么都不建，计时器回到无进展截止时间，不会空转；腾出槽位后由 tick 创建。
    - 已知限制的顺序：503 不带 `Retry-After`，冷却 0.5–1 s，比 tick 间隔短。冷却结束时有空闲槽位，但降级还没发生：唤醒什么都不建，计时器回到无进展截止时间（晚于冷却结束），不会在同一时刻反复醒来；下一个 tick 只把并发降到 1，不建 runner；再下一个 tick 才为退回的区间建 runner，`attempts` 为 1。
    - 一个真实计时器的测试：循环自己运行，丢失的 runner 在冷却结束后被重建，期间没有任何策略 tick。它要等真实时间（约 0.5–1 s，等的是循环自己的计时器）；下面 T4 的 503 场景测试也要等真实时间，其余测试都显式传入 `now`。
  - 性质测试：T8 的 `split_properties` 加了失败（可重试，或 503 带 0–4 s 或不带 `Retry-After`）、时间流逝（0–5 s）和唤醒三种操作。除了原来的断言，每一步之后还检查：
    - 冷却记录都在空隙里，非空，`attempts ≥ 1`，相接的已合并；
    - 计时器不晚于下一个可能开新连接的时刻；
    - 新建的 runner 不是在拥塞期间建的，没有领取冷却中的区间，`attempts` 等于它领取到的记录里最大的 `attempts`；
    - 失败时 `attempts` 加 1，退回的区间冷却至少 0.5 s。
  - T4 的 503 场景：`retry_after_holds_back_the_requests_to_the_server`，下载循环用真实计时器、reqwest adapter 和故障服务器的 `CongestedOnce` 下载 8 MiB，需要 `reqwest` 和 `tokio` 两个 feature。
    - 顺序由测试决定，不靠时间赛跑：
      - 服务器带着一个 gate 启动。第一个 runner 收到响应头，body 停在 gate 上。
      - 测试自己逐个处理循环的事件（`poll_event` 和 `handle_event`），直到第一个 runner 进入 Running，然后切分它。切分出的请求是第二个，也就是被拒的那个。
      - 测试继续逐个处理事件，直到循环记下拥塞。这时断言：只占着 1 个槽位，第一个 runner 仍是 Running，一个字节都还没收到。所以 503 被处理时，一定还有连接在途，`FailurePolicy` 不会因此结束任务。
      - 然后测试放行 gate，再交给 `run()` 跑完。
    - 从服务器记录的请求时间判断：503 之后的每个请求都至少晚 2 s，被拒的区间确实重试了，文件 hash 正确。前两个请求的范围和状态码也有断言，前提不成立时会直接说明。
    - 收尾先于断言：先 `finish`、`finalize` 并停掉服务器，再检查结果。服务器的句柄在 drop 时 abort，测试 panic 时也不会留着服务器，或者让临时文件一直被占用。
    - 去掉 gate 再跑，“503 被处理时只有第一个 runner 在途、还没收到数据”这条断言就会失败。
    - 实测：503 在 0.9–1.3 ms，1.5 s 的 tick 降级，重试在 2.003–2.018 s，是唤醒触发的；连跑 5 次，每次约 3.05 s。
    - `range_faults_never_complete_with_wrong_content` 也通过公开 API 跑 `CongestedOnce`：256 KiB 的文件不会切分，所以正常完成。
  - 已有测试：没有断言依赖“失败后立即重试”，断言都没有改；签名变了的调用点只是加上 `now` 参数。`termination.rs` 里重试相关的测试（重试次数用完、暂时失败后恢复、连接超时后重试）都通过。
  - 变异检查，每种都有测试失败：
    - 不退避（可重试的失败不冷却）：`failed_range_waits_for_its_backoff`、`loop_retries_after_the_backoff_without_a_tick`、`wake_up_without_a_free_slot_does_not_spin`、性质测试和 `Backoff` 的单元测试；
    - 失败时 `attempts` 不加 1：4 个 `ChunkPlanner` 测试、5 个下载循环测试和性质测试；
    - 合并时不取较大的 `attempts`，或不取较晚的 `not_before`：`test_adjacent_cooldowns_merge`；
    - 不记录拥塞：`retry_after_holds_back_every_new_connection`、`wake_up_before_the_degradation_leaves_the_retry_to_a_later_tick` 和 4 个 `Backoff` 测试；
    - 失败后不提前计时器：5 个下载循环测试和性质测试；
    - 醒来后不创建 runner：3 个下载循环测试；
    - `spawn_runner` 不跳过冷却区间：性质测试；
    - `split` 不检查拥塞：`retry_after_holds_back_every_new_connection`；
    - 领取时不带上 `attempts`：4 个 `ChunkPlanner` 测试、4 个下载循环测试和性质测试；
    - 唤醒创建不等降级：`test_create_tasks_waits_for_the_first_step_and_the_degradation` 和 `wake_up_before_the_degradation_leaves_the_retry_to_a_later_tick`。

### 14.12a 静默的连接上限（PR-6a，C35–C38）

- **做法。**
  - C35：新增 `strategy/admission_evidence.rs`，`RunnerManager` 持有它。生命周期的 `Started` 分支和 `mark_started` 调 `on_started`，`release_runner` 调 `on_ended`。每个结束的尝试都经过 `record_attempt_end(phase, error)`：连接失败（含 spawn 失败）、runner 丢失、传输中的失败；取消不算失败，不经过它。佐证用的“已证实”数来自 `ChunkPlanner::get_proven_runners_count()`：`Running` 且本次尝试 `downloaded` 非空的块。`on_runner` 开头先 `stamp(now)`。连续拒绝分两部分：已盖章的和还没盖章的；盖章时先检查已盖章的最近一次拒绝是否已过 `STREAK_GAP`，过了就丢掉，再并入新的拒绝，所以间隔之后才到的拒绝不会续上过期的一轮，不必等 tick。这一步不读证据，行为不变。
  - C36：`bolt-load-tests` 新增 `create_limited_range_server(content, limit, mode, bytes_per_sec)` 和 `LimitMode::{Close, Hang}`。用 axum 的 `Listener` 包一层：超过上限的连接，`Close` 在接受后立即关闭，`Hang` 接受后一直持有、不读也不答。`ConnectionLog` 记录每个连接的服务、关闭和拒绝，以及同时服务的连接数。range 响应按给定速率发送，原来的限速响应改为同一个 `paced` 函数。
  - C37：并发控制读证据。`StrategyControl::execute` 和 `create_tasks` 改为接收 `LoopSignals`（拥塞、最近进度、writer 能否接收），`FailurePolicy` 新增 `last_progress_at()`。拥塞规则取代立即降级，静默规则取代按失败率的降级，在 `Normal` 和 `Degraded` 里都执行；旧的恢复仍然读 sampler。`RunnerOutcomeSampler` 的 `unretryable_pending` 和 `has_recent_unretryable` 删掉，由证据的 `congestion_pending` 和 `take_congestion` 代替。
  - C38：并发控制改为 §8.3 的 step；`ConcurrencyControlContext` 增加 `want`、`congestion_until`，去掉 `max_concurrency`，`LoopSignals` 增加 `congestion_until`。`StrategyControl` 记住 `max_connections` 作为 `want`，dynamic 策略的切分闸门改为 `min(max_concurrency, cap)`。删掉旧的 `Degraded`、`Recovering`、失败率阈值、`degradation_step`、`min_samples`，以及 `RunnerOutcomeSampler` 和它的全部记录点；`strategy/` 的非测试代码里不再有 `Instant::now()`。
- **与 §8.3 之外的约定的偏差。**
  - 被接受的 step 不再下发 `ChangeMaxConcurrency`：新的 `limit` 等于原来的 `limit_cap`，循环的上限不用变。
  - 不带 step 的拥塞也把 `due` 推到拥塞结束加当前等待之后：服务器刚说过已满，不马上试探。
  - 迟到的 tick 只处理拒绝和截止时间，没有佐证的拒绝留给下一个 tick。
  - 测试里的 writer 改为每次写入一个任务、停在测试的 runtime 里：循环自己的 writer 把写入交给后台线程，完成的时机测试控制不了，重放就不确定。
  - 重排到 PR-7b 之后，`Doubling` 是唯一的扩张策略，并且会对冲和重启 runner。`admission.rs` 的模拟循环因此把 `min_chunk_size` 设为整个文件（扩张不切分）、关掉对冲（只在测试里有的开关），健康的 runner 每个 tick 都报告交付，不算沉默：只有测试安排的 runner 连接。上限和对冲、扩张的配合由 `hedge.rs` 和 `growth.rs` 的测试覆盖。
  - G10 的端到端测试由测试自己切分出第四个连接：任务只有在 dynamic 策略的速度采样需要时才会切分，测试无法强制（§14.12 的同一限制）。
- **验证。**
  - 单元测试（注入 `now`）：
    - 证据：分类表的每一行；同水平的连接结束连续拒绝，较低水平的不结束，开始之前的成功不算；没盖章的时刻读作未知；丢失会清掉没盖章的占满；step 开始时已占满就从开始算起；标记按程序顺序；降级截断 `proven_window`。
    - manager：分类经过生命周期，13 种结束方式；`Started` 两条路径都记录；完成、失败、丢失、取消都离开 `connected`。
    - 并发控制（`strategy/concurrency_control_strategy/tests.rs`）：三次拒绝加进度才降，降过还能再降，旧的成功稀释不了，没有进度不降，`STREAK_GAP` 后失效，没有佐证不算，带数据的短流不降，拥塞不升高上限，迟到的 tick 和满 writer 不降；step 要 `want`、到期、`connected ≥ limit`、没有拥塞；从试探连接的 `Started` 算 15 s 才接受；两个 tick 之间的替换、writer 满时的丢失都会重新计时；迟到的 tick 不接受；截止时间结束为不确定且等待不变；下一个 step 计入留下的试探连接，不新开连接；同一时刻之前处理的拒绝不拒绝 step；被佐证的拒绝拒绝，没有佐证的不确定，拥塞拒绝并降级、从拥塞结束算起；拒绝的 step 依次相隔 30、60、120、240、300、300 s；一个连接永远不 step。
    - 原有的三个单元测试保留断言，只改准备步骤：`test_immediate_degradation_never_drops_to_zero` 和 `test_create_tasks_waits_for_the_first_step_and_the_degradation` 把 sampler 里的不可重试结果换成证据里的拥塞；`test_a_pending_unretryable_failure_triggers_once` 改为证据的 `take_congestion` 测试。
  - 下载循环（`download_loop/tests/admission.rs`，健康的 runner 由测试驱动，循环自己建的 runner 通过拒绝或不应答的 adapter 连接，经过生命周期结束）：
    - 8 个里服务器接受 4 个、其余被拒绝或不应答：在第三次拒绝之后第一个间隔一个 tick 的 tick 降到 4，之后 25 s 内没有新连接。
    - 同样的场景跑 300 s：只有 step 的试探连接超过 4，每个 step 一个，都被拒绝，每个 step 在到期后的第一个 tick 开始，拒绝之后的等待依次加倍；上限在 4 和 5 之间交替，从不更高。
    - 传输中的 503 在下一个 step 降到 4，之后 25 s 不升；step 进行中的 503 拒绝它，等待加倍并从拥塞结束算起。
    - 丢失的 runner、四个连接同时断开后重连被拒、拒绝与成功交替：上限都不变。
    - writer 满 15 s，期间拒绝照常到达、runner 的结束照常处理，但不接收数据，静默规则等待；写入完成后的下一个 tick 降级。
    - 性质测试：连接、交付、完成、拒绝、带数据后失败、丢失、拥塞、填满和清空 writer、按时和迟到的 tick、唤醒，128 组，每组最多 400 步。测试自己维护已连接的 runner 和 step 的连续占满（从它报告的事件算），用这个模型而不是证据判断：模型和证据的连接数、占满起点必须一致；上限在 `[1, max_connections]` 之内，新增 runner 不超过 cap；只在按时的 tick、模型的连续占满达到 15 s 之后加 1；静默降级只在 writer 能接收数据、第三次拒绝之后有进度时发生，拥塞立即降；step 在截止之后的第一个 tick 结束；传输中的失败、丢失、完成和 writer 不产生证据，也不改上限；循环的上限等于 cap；同样的操作在同样的时刻重放，下发的动作序列相同。
  - 端到端（reqwest、tokio、C36 的服务器，真实计时器）：`max_connections = 8`，服务器限 3、每个连接 256 KiB/s，`Close` 和 `Hang`（连接超时 300 ms）各一次：下载完成，BLAKE3 正确，服务器同时服务的连接不超过 3，上限从 8 降到 3，之后不再升高；第一个 step 至少在降级 30 s 之后开始，两者都是并发控制自己的决策时刻（循环处理事件时的 `now`）（G10）。服务器的记录不和循环的时刻比较：不应答的连接，服务器记下拒绝的时间可能晚于循环的时刻。step 开了哪些试探、什么时候开，只在注入时间的测试里断言。每个约 20 s。
  - 已有测试：`retry.rs` 的断言都没有改。manager 测试里读 sampler 的断言改读证据（旧：`connect_failed_*`、`stream_closed_*`、`total` 的计数；新：连续拒绝为 0、没有待处理的拥塞、`connected` 为 0）；`test_custom_timer_survives_other_events` 改为断言丢失的连接让它的区间退避了一次。
  - 回归：两次拒绝在 0.1 s 和 0.2 s，tick 每 1.5 s 一次，第三次在 90.3 s（间隔已过、下一个 tick 之前），之后有进度：不降级（证据和并发控制各一个测试）。`connect_timeout = Duration::MAX` 时 `STREAK_GAP` 和 `STEP_DEADLINE` 饱和，不 panic，step 只按标记或接受结束。
  - 回归：step 的试探在它的 tick 被拒、区间只退避 1 s（比 tick 短），唤醒时不再开第二个试探，下一个 tick 拒绝这个 step（下载循环）；被标记的 step 在 tick 之前不补试探的位置，低于 `limit` 的照常补（单元，两种标记）。去掉这条规则，两个测试都失败。
  - 变异检查：接受时不看连续占满，4 个单元测试和性质测试失败；释放时不清空占满，性质测试失败。
- **已知限制。** 见 §8.3 的“不做的事”和 §12。

### 14.13 每区间失败预算（PR-6，C10）

- **做法。**
  - 三层都在 `FailurePolicy::on_failure` 里判定，顺序如下：
    1. 不可重试的失败照旧：别的连接还在途时，429/503 交给并发控制，任务继续；其它不可重试的失败直接结束任务。
    2. 重启：本次尝试收到过数据之后的超时或断流，只退避，然后返回。它不计入连续失败，也不记到区间的预算上。
    3. 每区间预算：区间记过的失败数达到 `max_attempts_per_range` 时，任务以 `RangeExhausted` 结束。
    4. 任务级连续失败：和原来一样，`on_write_ok` 会清零。
  - 两个上限在同一次失败上同时达到时，返回 `RangeExhausted`。它说明是哪个区间，还带着这个区间最后一次的错误，比“连续失败太多”更有用。
  - “收到过数据”指这次尝试有字节被 planner 接收，也就是 chunk 的已下载部分不为空：
    - 被截掉的字节不算，例如 runner 读到缩小后的上限之前多发的部分；
    - 还在通道里、没被取走就超时的字节也不算；
    - 一个 chunk 只属于一次尝试，领取时已下载部分是空的。所以失败时已下载部分的长度，就是这次尝试交付的字节数。`mark_failed` 把它放进 `Released::received`，runner manager 再把它带到 `AttemptFailure::received`。
    - 这样定义，每次重启都会让区间变短，重启不可能在没有进展的情况下一直进行。
  - “超时或断流”对应的错误（按 §14.1 的分类）：
    - `TaskError::Timeout`：空闲超时、数据排空超时和连接超时。连接超时发生在连上之前，一定没有数据，所以总是计入；
    - `RetryableError::Io`：连接失败（没有数据，计入），或者 body 读到一半断开，例如连接被重置、EOF 来得太早；
    - `TaskError::SmallerThanTotalSize`：body 正常结束，但没到区间末尾。
    - 不算重启：可重试的状态码（`RetryableError::Status`），它在第一个字节之前就到了；`ChannelClosed`，它表示 runner 丢了，是本地的问题，不是连接断了。
  - 两个计数。区间上有两个失败计数，名字和规则以这里为准：
    - `attempts`（C9，不变）：这个区间失败过几次。每次任务挺过去的失败都加 1，包括被容忍的 429/503。它只决定退避多长（§8.1、§14.12）。
    - `charged`（本次新增）：这个区间记到预算上的失败数，每区间预算只比较它。
      - 加 1 的失败：可重试、并且不是重启的失败。也就是连接失败、没有数据的超时、`ChannelClosed`、可重试的状态码，以及没有数据时的断流。
      - 不加的失败：重启（收到过数据之后的超时或断流），以及 429/503。其它不可重试的失败直接结束任务，也不加。
      - 判定写在 `failure_policy.rs` 的 `charges_range` 里。
    - 两个计数随区间走的规则相同：
      - 存在 `ChunkState::charged` 和 `Cooldown::charged` 里，和 `attempts` 并排；
      - 相接或重叠的冷却记录合并时，各自取较大值；
      - 新 chunk 领取区间时，各自取它覆盖到的记录里的最大值，新字节算 0；
      - 切分出的后半段（`resize` 让出的尾部和 `split_chunk` 的新 chunk）都从 0 开始，原 chunk 保留自己的值；
      - `PlannerGuard` 回滚时随整个 planner 一起恢复。
    - 所以总有 `charged ≤ attempts`。
    - 传递：`mark_failed` 在 `Released::charged` 里返回失败之前的值，因为 planner 不知道失败的原因。runner manager 构造 `AttemptFailure` 时调用 `charges_range`，算出包含这次在内的 `AttemptFailure::charged`。`back_off` 把 `attempts` 和 `charged` 一起写进冷却记录。
  - 空区间：runner 在区间下完之后才失败时，退回的区间是空的。这时没有可耗尽的东西，不检查预算。连续失败照常按上面的规则算：重启不计，其余可重试的失败计入。
  - 公开 API 的变化：
    - 新增 `TaskError::RangeExhausted { range, attempts, source }`。消息是 `bytes {start}..{end} failed {attempts} times, last error: {source}`，`source()` 指向最后一次的错误，写法和 `RetryLimitExceeded` 一样。`TaskError` 不是 `#[non_exhaustive]`，加变体是破坏性变更，所以提交信息带 `!`。
    - 新增 `TaskOptions::max_attempts_per_range: NonZeroU32` 和常量 `DEFAULT_MAX_ATTEMPTS_PER_RANGE`（5）。`TaskOptions::max_attempts_per_range(n)` 和 `TaskBuilder::max_attempts_per_range(n)` 设置它，和 C7 的 `max_connections` 一样；之后再调用 `options(...)` 会整体替换掉它。`TaskOptions` 是 `#[non_exhaustive]`，加字段不破坏兼容。
    - 类型本身保证至少为 1，所以不需要构建时检查。等于 1 时，区间第一次记到预算上的失败就结束任务；重启和 429/503 仍然只退避。
    - `DownloadError::Transfer` 的文档加上了这种情况。没有别的地方按 `TaskError` 的变体做映射，所以不用改别的代码。
- **与 §8.2 的偏差。**
  - 预算比较的是 `charged`，不是 `attempts`，所以有两个计数。
    - §8.2 的表里，重启一行写的是“不计入连续失败，只走 §8.1 的退避；`attempts` 照常增加”，兜底一栏是 `no_progress_timeout`，不是预算。每区间预算一行又写 `attempts >= max_attempts_per_range`。两处放在一起是矛盾的：如果预算直接数 `attempts`，重启就不只是退避了。
    - 具体的后果：网络不稳定时，一个大区间每次都收到一些数据、然后断开，断 5 次任务就失败了。原来的代码在这种情况下会一直下完。
    - 429/503 也会悄悄耗掉预算。服务器限制连接数时，并发控制降级 30 s 后会多开一个连接试探，可能每次都落在同一个区间上。这个区间被拒 4 次之后，再来一次普通的失败，任务就结束了。
    - 改进方案也是这么分的：P1-4 第 1 条说重启“整体由 `no_progress_timeout` 兜底”；P1-7 的 S4 第 3 条说重启“不计入失败预算……但计入 `attempts`”。
    - 所以 `attempts` 仍按 §8.1 增加，C9 的退避和它的断言都不变；预算另外计数。
  - 不记预算的失败怎么收尾：
    - 重启一定会结束：重启要求这次尝试有字节被 planner 接收，所以每次重启都让区间严格变短。区间只有有限个字节，重启的次数也就有限。没有进展的失败都不是重启，都要记预算。
    - 429/503（§8.2 没说计不计入，这里不计入）：只有别的连接还在途时才被容忍；没有别的连接时，它像原来一样直接结束任务。被容忍期间，`no_progress_timeout` 也照常有效。
  - G6 仍然成立：它针对的是“健康连接掩盖一个反复失败的区间”。没有进展的失败都记到这个区间的预算上，别的连接写成功也不会清零它。每次都有进展的区间不算反复失败，它会一点点下完。C39 起，在已证实窗口之上的拒绝例外，不记预算（§14.13a，确认 2026-11-13）；它说的是服务器的连接数，不是这个区间。
  - `RangeExhausted` 比 §8.2 多一个 `attempts` 字段，用来在消息里写出次数。它等于 `charged`，也就是上限，可能比决定退避的 `attempts` 小。`source` 是这次失败的错误。
  - `range` 是最后一次失败退回的区间。区间收到过数据时，它只是还没下完的那部分。
- **已知限制。**
  - 重启只受 `no_progress_timeout` 约束。服务器每次只发很少的字节就停住时，每次重启都有进展，无进展超时不会触发，下载会很慢但不会失败。以后 §9.5 的低速重启可以处理这种情况。
  - 区间被切分后，新的一半从 0 开始计数（§8.1 的规则，C9 已经这样做）。一个坏区间被切成几段时，每段都有自己的预算。
  - runner 丢失（`ChannelClosed`）总是记到预算上，即使它之前收到过数据。
- **验证。**
  - `FailurePolicy` 的单元测试（`AttemptFailure` 都用 runner manager 的构造函数生成，记不记预算走真实的判定）：
    - `test_a_failing_range_ends_the_task_despite_healthy_writes`（G6）：连续失败上限是 2。区间先后以 5 种不同的错误失败，每两次之间别的连接写成功一次。前 4 次任务继续，第 5 次以 `RangeExhausted { range, attempts: 5 }` 结束，`source` 是第 5 次的错误；
    - `test_without_writes_the_consecutive_limit_ends_the_task_first`：同样的失败，中间没有写入时，第 2 次就以 `RetryLimitExceeded` 结束；
    - `test_a_timeout_after_data_is_not_a_consecutive_failure`：两个上限都是 1。收到数据之后的超时、连接重置和提前结束都不结束任务，连续失败数保持 0，预算也不记；
    - `test_a_timeout_without_data_still_counts`：没有数据的超时计入连续失败，第 2 次以 `RetryLimitExceeded` 结束；
    - `test_what_restarts_and_what_is_charged`：上面的错误分类表，以及不可重试的失败（包括 429/503）都不记预算；
    - `test_the_range_budget_ends_at_exactly_its_limit`：上限是 3 时，记到第 2 次继续，第 3 次结束；上限是 1 时，第一次记的失败就结束，而重启仍然只退避；
    - `test_restarts_and_congestion_are_not_charged`：区间已经失败 6 次，但都是重启或 429/503，预算还是 0；之后第 2 次记的失败才结束任务；
    - `test_the_range_budget_comes_before_the_consecutive_limit`：两个上限同时达到时，返回 `RangeExhausted`；
    - `test_a_complete_range_is_not_exhausted`：退回的区间为空时，不检查预算，但仍计入连续失败。
  - `ChunkPlanner`：`test_charged_failures_are_carried_like_attempts` 里 `charged` 和 `attempts` 不相等，验证两者各自取较大值、领取时带走、切分出的区间从 0 开始，以及 `mark_failed` 返回失败之前的 `charged` 和收到的字节数。C9 测试里的辅助函数改为每次失败都记预算，所以 C9 的合并、部分领取和切分测试也同时覆盖了 `charged`。
  - runner manager：`test_a_failure_reports_only_the_bytes_the_planner_accepted` 直接检查 `received` 的边界，三个 runner 都以超时结束：
    - 数据排空超时，帧还都在通道里：`received = 0`，这次失败记预算（`charged = 1`）；
    - 接收了一帧，下一帧还在通道里：`received` 只算接收的 100 字节，这次是重启（`charged = 0`），退回的是剩下的部分；
    - chunk 缩小到已下载部分之后 50 字节，runner 读到新上限之前又发了 200 字节：planner 只接收 50 字节，`received` 是 150，不是 300。
  - 下载循环的测试（显式传入 `now`）：
    - `a_failing_range_ends_the_task_while_another_runner_writes`：两个连接，连续失败上限 2，每区间上限 3。一个测试控制的 runner 下载前半段；后半段的 runner 由循环自己创建，用真实的 runner 和 adapter 连接，连接失败（可重试的 io 错误）。每次失败后，另一个 runner 交付 1 KiB，写入完成，连续失败清零；区间在 `not_before − 1 ns` 时不重试，在 `not_before` 时由唤醒重试。第 3 次的 runner 丢失（`ChannelClosed`），任务以 `RangeExhausted { range: 后半段, attempts: 3 }` 结束，`source` 是 `ChannelClosed`，不是前两次的 io 错误；
    - `a_timeout_after_data_restarts_the_range`：两个上限都是 1。runner 交付 1 KiB 后超时，`AttemptFailure` 带着 `received = 1024`、`charged = 0`，任务继续；剩下的部分冷却，`attempts = 1`、`charged = 0`。冷却结束后的 step 重建 runner，它没有数据就丢失了，这是区间第一次记的失败，任务以 `RangeExhausted` 结束。这个测试检查 `received` 从 planner 一路传到了 `FailurePolicy`。
  - 性质测试：`Fail` 新增超时一种（有数据时是重启），失败改用 `AttemptFailure::new` 构造。每次失败后检查：`mark_failed` 返回失败之前的 `charged`；`charged` 只在 `ChannelClosed`、或没有数据的超时时加 1，429/503 和重启不加；冷却记录的 `charged` 不小于这次的值。每一步之后还检查：冷却记录和每个 runner 的 chunk 都满足 `charged ≤ attempts`；新建的 runner 的 `charged` 等于它领取到的记录里最大的 `charged`；切分出的后半段（落在这一步之前已有 chunk 的范围里）`attempts` 和 `charged` 都是 0。
  - `TaskError` 的消息和 `source()`、默认值 5、两个设置方法各有单元测试。
  - 已有测试：C9 关于 `attempts` 的断言一条都没改。只有机械的改动：`Released` 和 `Cooldown` 的字面量加了新字段（C9 的 503 测试里是 `charged: 0`，同时验证了 503 不记预算）；C9 测试的 `cool_down` 调用多了一个参数，值和 `attempts` 相同。`termination.rs` 里 `RetryLimitExceeded` 的测试把连续失败上限设成 2 或 3，比 5 小，结果不变。
  - 变异检查，每种都有测试失败：
    - 去掉预算检查：6 个测试，包括两个下载循环测试；
    - `>=` 改成 `>`：同样 6 个测试；
    - 收到数据之后的超时计入连续失败：`test_a_timeout_after_data_is_not_a_consecutive_failure` 和 `a_timeout_after_data_restarts_the_range`；
    - 没有数据的超时不计入：3 个 `FailurePolicy` 测试和性质测试；
    - `source` 不是最后一次的错误（改用上一次失败的错误）：`test_a_failing_range_ends_the_task_despite_healthy_writes` 和 `a_failing_range_ends_the_task_while_another_runner_writes`；
    - 429/503 也记预算：`test_restarts_and_congestion_are_not_charged`、`test_what_restarts_and_what_is_charged`、`retry_after_holds_back_every_new_connection` 和性质测试；
    - 重启也记预算：`test_a_timeout_after_data_is_not_a_consecutive_failure`、`test_what_restarts_and_what_is_charged`、`a_timeout_after_data_restarts_the_range` 和性质测试；
    - 切分出的后半段继承前面 chunk 的 `charged`：`test_split_range_starts_without_attempts` 和性质测试；
    - 有数据时 `received` 报 0：2 个 `ChunkPlanner` 测试、`test_a_failure_reports_only_the_bytes_the_planner_accepted` 和 `a_timeout_after_data_restarts_the_range`；
    - 领取时不带走 `charged`：3 个 `ChunkPlanner` 测试、`a_failing_range_ends_the_task_while_another_runner_writes` 和性质测试；
    - 合并时不取较大的 `charged`：`test_adjacent_cooldowns_merge`；
    - `FailurePolicy` 不读配置（固定为 5）：5 个测试；
    - `TaskBuilder::max_attempts_per_range` 不写入：`test_max_attempts_per_range_sets_the_option`；
    - 先查连续失败、再查预算：`test_the_range_budget_comes_before_the_consecutive_limit` 和 `a_timeout_after_data_restarts_the_range`；
    - 区间为空时也检查预算：`test_a_complete_range_is_not_exhausted`。

### 14.13a 已证实窗口之上的拒绝不记预算（PR-6，C39）

- **决策（D27，确认 2026-11-13）。** 服务器静默限连时，超出它接受数的连接被拒绝，这不是这个区间的问题。以前这样的拒绝每次都记到区间的预算上：并发控制还没降上限之前，一个区间就能被拒五次，任务以 `RangeExhausted` 结束（§8.3 的问题 A）。
- **规则。** 在 manager 给失败分类的那一刻，用那一刻的状态决定，之后不再更正。同时满足以下三条的失败不记预算：
  - 是拒绝（§8.3 的分类：连接中的 `RetryableError::Io` 或连接超时）；
  - 此刻另有一个已证实的尝试；
  - 此刻至少有 `max(a, 1)` 个其它尝试在途，`a` 是已证实窗口：降上限以来，在策略 tick 和失败时看到的同时已证实的尝试数的最大值，降上限时截到新上限。
- 免记的失败照常：`attempts` 加 1，照常退避，照常算一次连续失败；只是 `charged` 不加。其它失败照旧（§14.13），429/503 仍然不记。

| 失败 | 记预算吗 |
| --- | --- |
| 在已证实窗口之上、旁边有已证实的连接时被拒绝 | 不记 |
| 在窗口之内被拒绝（其它在途的尝试少于 `a`） | 记 |
| 没有任何连接已证实时被拒绝：一开始就开多个连接、续传时有多个区间、整体断网 | 记 |
| `Started` 之后没有数据的失败；丢失的 runner；可重试的状态码 | 记，同 §14.13 |
| 重启；429/503 | 不记，同 §14.13 |

- **做到了什么。** 静默限制不变时，满足这三条的拒绝不耗预算。问题常见的形态正好满足：扩张越过上限时，先前的连接都在下载；之后恢复步骤每次多试一个连接。
- **没有保证的（不承诺能活下来）：**
  - 一开始就同时开多个连接：第一个 step 为每个可用区间建一个 runner。新下载只有一个区间，续传可能有很多。在任何连接交付之前，每次拒绝都没有佐证，都要记：第一次被拒各记一次，所以 `max_attempts_per_range = 1` 时在这里就结束，和以前一样；第一次重试在 0.5–1 s 之后，如果那时还没有连接交付，它也要记，预算 2 就可能在第一次交付之前用完。
  - 客户端已关闭、服务器仍在计数的连接（ureq2 的 issue #207）：这时的拒绝在窗口之内，要记。
  - 运行中缩小的上限：拒绝在窗口之内，要记，直到上限降下来。降下来要连续三次拒绝和一个 tick 的进度；一个区间单独重试时，在 0、0.5、1.5、3.5 s 失败（不加抖动），所以可能先记四次；中间如果在那个水平上连上过一个连接，还会更多。预算小于 5 时，一次缩小就可能结束任务；5 在这个过程里还剩一次，但这是算出来的，不是保证。
  - 接受连接之后不发数据或重置的服务器（§8.3、D26）。
- 坏区间仍然会被耗尽：在窗口之内，它从第一次失败起就记。没被证实过的区间在窗口之上免记，直到上限降到其它连接的数目，之后在窗口之内的下一次重试开始记。只有一种形态计数不会前进：这个区间只作为 step 的试探被尝试，例如 `max_connections = 2`、旁边一个很长的健康区间。这和“服务器只接受一个连接”分不开。
- **做法。** `AdmissionEvidence::exempts(class, proven, others)` 在 `on_attempt_ended` 记下同一次失败之后判定；`record_attempt_end` 返回 `Admission { class, exempt }`。`AttemptFailure::new` 仍按 `charges_range` 算 `charged`，`with_admission` 对免记的拒绝减回去，并带上 `class` 和 `exempt` 供诊断。`charges_range` 仍然只看错误和字节。
- **验证。**
  - 证据的单元测试：免记只给被佐证、在窗口之上的拒绝；窗口在降级时截断。
  - 下载循环：
    - `refusals_above_the_proven_window_are_not_charged`：静默限 4，预算 1、2、3、5，300 s 里所有拒绝都不记，任务不结束。
    - `a_refused_range_inside_the_proven_window_is_exhausted`：五个已证实，一个重置之后它的区间在四个其它在途时被拒，每次都记，预算 B 时在第 B 次结束（B = 1、2、3、5）。
    - `a_range_never_proven_is_charged_once_inside_the_window`：没被证实过的区间在窗口之上免记；上限降到 4 后，一个健康连接下完，下一次重试在窗口之内，记一次。
    - `a_resumed_start_charges_its_first_refusals`：四个区间在任何连接交付之前同时连接，每个被拒一次、各记一次；四个健康连接紧接着交付（在第一次重试之前），预算 1 时结束，2、3、5 时继续，之后的拒绝免记，上限降到 4。这个结果依赖交付先于重试。
    - `a_resumed_start_with_late_delivery_charges_until_corroborated`：同样的开始，但健康连接到第一个 tick（1.5 s）才交付。不加抖动时重试在 0.5 s 和 1.5 s，后者在 tick 的交付之前，所以预算 2、3 在任何佐证之前就以 `RangeExhausted` 结束；预算 5 活下来，每个区间记 3 次，之后的拒绝免记，上限降到 4。
    - `a_shrinking_limit_charges_until_it_is_lowered`：八个已证实，服务器从 8 变成 7：单独重试的区间每次被拒都记，直到上限降到 7；预算 1、2、3 在第 B 次结束，5 时活下来。
    - G6 的下载循环测试 `a_failing_range_ends_the_task_while_another_runner_writes` 改为先让两个连接都证实（窗口为 2），失败的区间在窗口之内被拒，第三次记的失败以 `RangeExhausted` 结束。原来的形态（失败的区间从没被证实）改成 `a_range_never_proven_is_exempt_above_the_window`：第一次拒绝时什么都没证实，记一次；之后在窗口之上，不记；丢失照常记，任务不结束。
    - 性质测试新增不变量：`charged` 每次失败至多加 1；只有拒绝能免记，免记的不加；不免记的拒绝加 1。
  - 端到端：两种方式都在两个连接已证实之后才切出被拒的连接，所有失败的 `charged` 都是 0。
  - 变异检查：去掉免记，4 个下载循环测试失败。
### 14.14 Doubling 扩张控制器与切分尺寸（PR-7，C11）

- **做法。**
  - 开关（§9.1）：
    - 新增 `GrowthStrategy { Legacy, Doubling }`，默认 `Legacy`，标了 `#[non_exhaustive]`。新增 feature `unstable-strategy`。
    - 打开这个 feature（或者在测试里）时，才有 `TaskOptions::growth_strategy` 字段、`TaskOptions::growth_strategy(s)` 和 `TaskBuilder::growth_strategy(s)`，`GrowthStrategy` 也才从 `bolt_load::task` 导出。不打开时，这个类型只在 crate 内可见，任务一律用 `Legacy`，由 crate 内部的 `TaskOptions::resolved_growth_strategy()` 决定。
    - 带 cfg 的字段和几个 trait：`Clone`、`PartialEq`、`Debug` 都是派生的，字段在时自动带上它；`Default` 是手写的，同样带 cfg，取 `Legacy`。
    - `TaskOptions` 是 `#[non_exhaustive]`，别的 crate 既不能用结构体字面量构造它，也不能不写 `..` 就解构它。所以依赖图里有一个 crate 打开了这个 feature（cargo 会合并 feature），也不会让其它 crate 编译失败；它们不设置这个字段，拿到的还是 `Legacy`，行为不变。
    - 不打开 feature 时，`Doubling` 这个变体从来不会被构造出来，标了 `expect(dead_code)`。`Doubling` 的代码照样编译，只是选不到。
  - 每个策略 tick 的顺序：
    - 和 `Legacy` 一样，先走并发控制（`ConcurrencyControlStrategy`）；它有动作，这个 tick 就不扩张。
    - 然后 `Doubling` 依次检查，任何一条不满足就不扩张：
      1. 之前停滞过（见下）。
      2. 距上次扩张不到 2 s（`GROW_INTERVAL`）。
      3. `allowed` 为 0，或者已经达到上限。
      4. 拥塞，即 `now < congestion_until`（§8.1）。这是 `Doubling` 自己的检查；`split` 也会再查一遍。
      5. 有 runner 正在连接，或者在这个 tick 里没有字节被接受。
      6. 设了限速，并且速度达到限速的 85%。现在还没有限速，`speed_limit` 恒为 `None`，这里只是给 P2 留的接口。
      7. 停滞判断，见下。
    - 都通过时，`target = min(allowed × 2, 上限)`，发出 `StrategyAction::Grow(target − allowed)`。
  - “与 tick 对齐”：扩张只在 tick 上决定，选上次扩张之后至少 2 s 的第一个 tick。tick 间隔是 1.5 s，所以实际上每隔一个 tick（3 s）最多扩张一次。
  - `allowed` 取活跃 runner 数，也就是 Pending 和 Running 状态的 chunk 数（`get_active_runners_count`）。不取下载循环当前的并发上限 `max_concurrency`：这个上限从 `max_connections` 起步，取它的话，第一个 tick 就“已达上限”了。IDM 的 `started` 数的也是正在用的连接（`idm-strategy-full.md` §3.2）。
  - 上限取下载循环当前的 `max_concurrency`，不直接用 `max_connections`：
    - `max_concurrency` 从 `max_connections` 起步，只有并发控制会改它。遇到不可重试的失败时，立即降到当前的活跃数；失败率高时每次减 1；恢复时每次加 1，最多回到 `max_connections`。
    - `Doubling` 只加 runner，不改上限，所以降级一直有效；上限恢复之后，扩张才能用上恢复出来的部分。
    - 两个策略就这样组合：减少连接归并发控制，增加连接归 `Doubling`，两者受同一个上限约束。
    - 重排到 PR-6a 之后（§8.3）：`Doubling` 的上限是 `min(max_concurrency, refill_cap)`，`refill_cap` 即 `cap = min(limit_cap, want, max_connections)`（`want` 目前恒为 `max_connections`）；恢复 step 被拒绝或不确定的标记置上之后、下一个 tick 判定之前，它是 `limit` 而不是 `limit + 1`。所以 `Doubling` 可以用 step 的试探位置开一个连接，但不会在已被标记的 step 上再开。上限不再每 15 s 加 1，而是 step 被接受才加 1：试探连接要连上并交付 15 s。
  - 速度窗口：
    - 每个策略 tick 采样一次，时间用 tick 传进来的 `now`，策略里不调用 `Instant::now()`。
    - 任务速度 = 这个 tick 里 planner 接受的字节数 ÷ tick 的时长。比较的是整个任务的速度，不是每个 runner 的速度。
    - 扩张前的窗口，是决定扩张的那个 tick；扩张后的窗口，是之后第一个通过检查 1–6 的 tick。两个窗口都是一个 tick 长（1.5 s）。
  - 停滞：
    - 只判断完整的翻倍，也就是这次扩张加的 runner 数等于扩张前的活跃数。扩张后窗口的速度低于扩张前的 1.10 倍，就是停滞。停滞之后，这次下载不再扩张。
    - 被上限截断、或者有切分被拒绝的扩张，照样算扩张，2 s 的间隔从它算起，但不判断，也不会解除已有的停滞。连接数只多一小步，本来就不能指望总速度多 10%。比如并发控制把上限一格一格地恢复（11 到 12），每条连接各 1 MiB/s 时增益只有 9%，判断的话就会永久停在 12。
    - 每次翻倍只判断一次。判断时，如果活跃 runner 比翻倍刚完成时少（有 runner 下完了或者失败了），这个窗口就量不出这次翻倍的效果。这时不下结论，这次判断作废，可以继续扩张。C28 起，翻到上限的翻倍改为：不下结论，也不扩张，等到活跃数回到翻倍后的数再判；上限以下的翻倍不变（§14.18）。
    - 翻倍到上限之后，检查 3 先拦住，所以这次翻倍不会被判断。等 runner 下完、活跃数降下来，判断作废，`Doubling` 会再切分，补回到上限。C28 起改为：上限处照样判断，没判断的翻倍不补位（§14.18）。
  - 落地：
    - `Grow(n)` 由下载循环执行。每次都选剩余最多的 Running chunk，先看它够不够大（见切分尺寸），再走 T8 的 `split`：先占槽位，再缩小原 runner，最后 spawn。
    - 遇到 chunk 不够大，或者 `split` 被拒绝（拥塞、没有空闲槽位、缩小失败），就停下。
    - 下载循环把实际加了几个 runner 报回控制器（`StrategyControl::on_grown`）。一个都没加，不算扩张：不更新上次扩张的时间，下一个 tick 可以再试。加了一部分，也算扩张，更新上次扩张的时间，但不判断停滞（见上）。
    - `split` 和 `spawn_in` 改为返回有没有 spawn 出 runner；`Legacy` 的调用点不看返回值。
  - 每个 runner 的速率（T12）：
    - 在 `download_loop/throughput.rs` 的 `RunnerRates` 里。只有 `Doubling` 采样，每个策略 tick 一次，用 tick 的 `now`。
    - 读的是 planner 接受的字节。一个 chunk 只属于一次尝试，所以 chunk 已下载部分的增长，就是这个 runner 被接受的字节；被截掉的字节不算。
    - 速率 = 两次采样之间的增长 ÷ 两次采样的间隔。窗口是一个 tick，也就是 1.5 s。
    - 一次尝试用（ID，chunk 的 generation）识别。planner 每交出一个 chunk，就给它一个新的 generation（单调递增的计数）；chunk 缩小时不变，切分出的新 chunk 拿新的。所以切分后原 runner 的速率还在；同一个 ID 没收到数据就失败、再领回同一个起点时，也是新的尝试，从没有速率开始。只用（ID，起点）识别是不够的：那样新尝试会接着用旧尝试的基线。第一次采样到的 runner 也没有速率。
    - 同一次采样还给出任务速度，以及“有没有 runner 在连接、或者这个 tick 没有数据”。T13 和 T27 可以复用这个采样。
  - 切分尺寸（T12）：
    - `planned = max(donor_speed × 2 s, min_chunk_size)`。`donor_speed` 是被切 runner 自己的速率；还没有速率时，用 `throughput.per_runner_speed`。
    - `planned` 是门槛，不是切出来的大小：chunk 的剩余量至少是 `planned` 才切，也就是被切的 runner 按自己的速度还要下 2 s 以上。切点仍然在一半。
    - 这就是 S1 的本意：`Legacy` 的门槛按任务总速率算，`Doubling` 按 donor 自己的速率算。
    - `Legacy` 不变：`planned_chunk_size` 仍然是 `current_speed * 2.0`，`get_incomplete_states` 里 `2 × min_chunk_size` 的下限也不变。
- **与 §9.2、§9.3 的偏差。**
  - 上限用下载循环当前的 `max_concurrency`，不是 `max_connections`。没有降级时两者相等；降级之后，`Doubling` 不会越过降级后的上限（见上）。
  - §9.2 没说停滞什么时候判、判几次。这里每次翻倍只判一次；判的时候活跃 runner 少于翻倍刚完成时的数，就不判。停滞之后不再恢复。
  - §9.2 对每次扩张都要求 10% 的增益，这个数是按翻倍定的。这里只判断完整的翻倍；被上限或拒绝截断的扩张只加了一小步，不能指望多 10%，所以不判断（2026-11-07 与 lead 确认）。
  - 每次扩张只看剩余最多的那个 chunk。它不够大就停下，不再去找别的 chunk。
  - §9.3 的 `clamp(donor_speed × 2 s, min_chunk_size, remaining / 2)` 改为 `max(donor_speed × 2 s, min_chunk_size)`，并且只当门槛用（2026-11-06 与 lead 确认）：
    - 为什么去掉上界 `remaining / 2`：切分总在一半处切，`planned` 只决定一个 chunk 值不值得切。代码里 `Legacy` 的 `planned_chunk_size` 也只是这样一个门槛。当门槛用时，`planned` 永远不超过剩余量的一半，剩余量也就永远够，这个门槛永远不起作用，donor 的速率也就不影响任何结果。
    - 为什么不把 `planned` 当成切出来的大小（新 runner 分到的，或者原 runner 留下的）：bolt-load 的 runner 不像 IDM 的连接那样会被复用，下完自己的 chunk 就停，不会接着去切别的块。短的那一边大约 2 s 就下完，它的槽位空着，`Doubling` 只好不停地再切，连接数会一直在 2 附近来回。
  - 85% 的限速闸门：限速还没有实现，只留了接口。
- **已知限制。**
  - 节奏是 3 s，不是 2 s：tick 是 1.5 s，扩张要等上次扩张之后至少 2 s 的第一个 tick。第一个 tick 在开始时立即触发，由并发控制建第一个 runner；之后最快在 1.5 s、4.5 s、7.5 s 翻倍，7.5 s 才到 8 条连接。`Legacy` 的 QuickStart 每个 tick 都可以翻倍，最快 4.5 s 到 8 条。T14 的基准测试会量出差别。
  - 停滞之后不再扩张，下完的 runner 也不补。收尾阶段，连接数会随着 runner 下完逐渐减少。没停滞时，下完的 runner 会通过正常的扩张补上。
  - 只判断完整的翻倍，所以扩张如果一直只是部分的，就永远不会下结论：比如上限一格一格地恢复，或者在上限处反复 8 → 7 → 8（一个 runner 下完、再切一个补上）。这时即使真的停滞了（总速度不再变化）也发现不了，循环会一直切分补位。这是接受了的盲区：连接数不超过当前上限，每次补位之间至少隔 2 s。T14 要量它的代价。`test_refills_at_the_limit_are_never_judged` 和 `doubling_refills_a_step_at_a_time_without_any_gain` 把这个行为固定下来。C28 起，到上限的完整翻倍会被判断，盲区只剩被截断的扩张，前一个测试改为 `test_refills_after_a_truncated_growth_are_never_judged`（§14.18）。
  - 剩余最多的 chunk 优先：它不到自己的门槛就停下，不再看别的 chunk。别的 chunk 剩得少一些、但 runner 更慢，门槛更低，本来可以切，也不会被选上。T14 要测连接速度不一的场景。
  - 速率只看一个 tick（1.5 s）。runner 攒满 32 KiB 才交一帧，所以慢于约 21 KiB/s（32 KiB ÷ 1.5 s）的 runner，有的 tick 里看起来没有数据，那个 tick 就不扩张。C29 起静默满 2 s 的 runner 不再挡住扩张，但这样的 tick 不判断翻倍（§14.19）。
  - writer 满的时候，下载循环不取 runner 的数据，这个 tick 看起来也没有数据，不扩张。这是有意的：瓶颈在写入时，加连接没有用。
  - `planned` 的下限是 `min_chunk_size`，所以剩余量在 `min_chunk_size` 到 `2 × min_chunk_size` 之间、donor 又慢的 chunk 也会被切，两半都小于 `min_chunk_size`。`Legacy` 的 `SplitGivenTask` 也是这样（只要求剩余量 ≥ `min_chunk_size`）。T27 的“没有可切的活”原来按 `2 × min_chunk_size` 定义，和这里的门槛不一样；§9.6 的触发条件 1 已经改为对齐（见 §9.6 的“排除”）。
- **验证。**
  - `DoublingStrategy` 的单元测试，速度序列和时间点都由测试构造，不 sleep：
    - `test_steady_growth_doubles_up_to_the_limit`：每个 runner 加 1 MiB/s，按 1.5 s 的 tick 走，连接数依次是 1、2、4、8，然后停在上限；
    - `test_the_target_is_capped_at_the_limit`：上限 6 时是 1、2、4、6；
    - `test_no_growth_at_the_limit`：活跃数达到或超过上限（包括降级后的上限）时不扩张，低于上限时只扩到上限；
    - `test_growth_stops_below_a_ten_percent_gain`：增益正好 1.10 时继续扩张，1.0999 时停滞，之后速度再高也不扩张；
    - `test_no_growth_when_the_speed_drops`：扩张后速度下降，停滞；
    - `test_a_tick_with_fewer_runners_does_not_judge_the_growth`：扩张后有 runner 下完，这次不判，继续扩张；下一次扩张照常判，停滞后不再扩张；
    - `test_runners_that_stopped_at_the_limit_are_replaced`：扩到上限时不判；runner 下完之后补回上限；
    - `test_refills_at_the_limit_are_never_judged`：在上限处反复 8 → 7 → 8，总速度一直不变，每次都不判断，每次都补回 8（已知限制里的盲区）；
    - 这三个测试在 C28 改写了，见 §14.18；
    - `test_no_growth_while_a_runner_is_inactive`：有 runner 不活跃时不扩张，扩张的判断也等到全部活跃的 tick；
    - `test_no_growth_while_congested`：拥塞时不扩张，也不算扩张，下一个 tick 就能扩张；
    - `test_no_growth_without_a_speed`：第一次采样没有速度，不扩张；
    - `test_the_grow_interval`：上次扩张后 2 s − 1 ns 不扩张，2 s 扩张；按 1.5 s 的 tick，下一个 tick 不扩张，再下一个扩张；
    - `test_a_refused_split_is_no_growth`：一个都没加不算扩张，下一个 tick 再试；加了一部分算扩张，2 s 从这次算起；
    - `test_a_truncated_growth_is_not_judged`：4 个 runner 要加 4 个、只加了 2 个，之后速度没变，照样扩张；上限从 12 一格一格恢复到 13、16，每条连接 1 MiB/s，11 到 12、12 到 13 都不判断，照样扩张；之后一次完整的翻倍（16 到 32）没有变快，停滞；
    - `test_no_growth_near_the_speed_limit`：限速接口，速度到 85% 时不扩张，低于时扩张；
    - `test_planned_split_size_follows_the_donor`：donor 都是 3 MiB/s 时，2 个 runner 和 8 个 runner（每 runner 平均速度不同）算出的 `planned` 相同，都是 6 MiB；没有速率时用平均速度；不低于 `min_chunk_size`，NaN 和无穷大也有定义。
  - `ChunkPlanner` 的单元测试 `test_every_claim_is_a_new_generation`：缩小和切分时原 chunk 的 generation 不变，切出的新 chunk 拿新的；没收到数据就失败、同一个 ID 领回同一个起点时，也拿新的。
  - `RunnerRates` 的单元测试（`throughput.rs`）：速率的窗口是两次采样之间的时间；被截掉的字节不算；新 runner 有数据就算活跃、但没有速率；在连接的 runner、这个 tick 没有数据的 runner 都不算活跃；下完的 chunk 不再采样；切分后速率还在；ID 换了 chunk 就没有速率；`test_a_retry_on_the_same_start_starts_without_a_rate`：没收到数据就失败、同一个 ID 领回同一个起点时，第一次采样没有速率，第二次才有；`test_claims_from_a_merged_cooldown_are_new_attempts`：两个相邻的区间都没收到数据就失败，冷却记录合成一条；runner 0 先领取其中一部分、下完，再领取剩下的部分，三次的 generation 互不相同，每次新尝试的第一次采样都没有速率；同一时刻采样两次，没有速度也没有速率。
  - 下载循环的测试（`download_loop/tests/growth.rs`，用脚本化的 runtime，runner 停在 spawn 处，由测试标记开始、交付字节，每个 tick 都传入时间）：
    - `doubling_grows_through_split_up_to_the_limit`：`max_connections = 4`，经过真实的 `split`，连接数是 1、2、4；第二个 tick 太早不扩张；到 4 之后每个 tick 都不超过 4 个槽位；区间始终恰好覆盖整个文件；
    - `doubling_counts_only_the_splits_a_slot_took`：一个槽位被占着时，2 到 4 只加了 1 个；下一次不判断这次截断的扩张，但没有空闲槽位，什么都没加，也不算扩张；槽位放出来之后，1.5 s 后的 tick 就补上，不用再等 2 s；
    - `doubling_refills_a_step_at_a_time_without_any_gain`：同样的恢复过程，但总速度固定（每个 tick 一共 512 KiB，平分给运行中的 runner）。每一步都不判断，所以没有停滞，循环照样扩张到 16（已知限制里的盲区）；
    - `doubling_grows_back_as_the_limit_recovers_a_step_at_a_time`：经过真实的 `StrategyControl` 和 `ConcurrencyControlStrategy`。8 个 runner，`max_connections = 16`；一个 503 让并发控制把上限降到 8；30 s 后开始恢复，每 15 s 加 1。每个 runner 每个 tick 交同样多的字节，总速度和 runner 数成正比，10 个以后每一步的增益都不到 10%。上限依次是 8 到 16，循环跟着一格一格扩张，最后是 16 个 runner，没有停滞；任何时候活跃数都不超过当时的上限；（重排后：上限由 503 降到 8，恢复按 §8.3 的 step 走，每个 step 的试探由 `Doubling` 的切分打开，交付 15 s 后被接受；跑 300 s，上限依次是 8 到 16）
    - `doubling_opens_no_trial_for_a_marked_step`（重排时加入）：上限 8 降到 4 后的 step 里，一次没有佐证的拒绝把它标为不确定；迟到的 tick 不判定它，`Doubling` 也不扩张。
    - `doubling_waits_for_every_runner_and_the_congestion`：runner 在连接、或者这个 tick 没有数据时不扩张；503 带 `Retry-After: 2` 之后，拥塞期间的 tick 不扩张；`congestion_until − 1 ns` 时策略不发出 `Grow`，`congestion_until` 时发出，那个 tick 扩张；
    - `doubling_stops_at_a_plateau`：两个 runner 合起来不比一个快，停滞，之后速度再高也不扩张；
    - `doubling_judges_the_gain_at_ten_percent`：速度是量出来的，有浮点误差，所以在 1.10 两边各差 1 KiB：高的扩张，低的不扩张；
    - `doubling_splits_only_a_chunk_of_the_planned_size`：14 MiB/s 时 `planned` 是 28 MiB，剩下的比它少 1 B，不切，也不算扩张；1 s 后的速率算出的 `planned` 正好等于剩余量，在一半处切，原 runner 收到新上限；
    - `planned_split_size_follows_the_donor_rate`：每个 runner 3 MiB/s，任务速度分别是 6 MiB/s（2 个 runner）和 24 MiB/s（8 个 runner），`planned` 都是 6 MiB；还没有速率时用每 runner 的平均速度；
    - `legacy_is_the_default_and_unchanged`：不设开关和设成 `Legacy` 时，都不采样，策略仍是 dynamic：1.5 MiB/s 时 `SplitAllTask(3 MiB)`，降到 0.5 MiB/s 时因为 `abs()` 仍然 `SplitAllTask(1 MiB)`。
  - `Legacy` 的单元测试 `test_legacy_thresholds_and_abs`：QuickStart 正好 1 MiB/s 不切、多 1 B/s 切，降速也切；Normal 阶段两边各在 1 MiB/s 处切换。
  - 性质测试 `splits_stay_within_the_limit_and_cover_the_file` 改为随机选 `Legacy` 或 `Doubling`，原来的断言都对 `Doubling` 成立：不超过上限，槽位不泄漏，区间恰好覆盖文件，拥塞期间不建连接。
  - `TaskOptions` 和 `TaskBuilder` 的设置方法各有单元测试；默认值是 `Legacy`；派生的 `Clone`、`PartialEq` 包含这个字段。
  - 已有测试：只有机械的改动，`execute` 多了一个参数（`Legacy` 传 `None`）。
  - 变异检查，每种都有测试失败：
    - 去掉停滞判断：`test_growth_stops_below_a_ten_percent_gain`、`test_no_growth_when_the_speed_drops`、`test_a_tick_with_fewer_runners_does_not_judge_the_growth`、`test_a_truncated_growth_is_not_judged`、`doubling_stops_at_a_plateau` 和 `doubling_judges_the_gain_at_ten_percent`；
    - 1.10 改成 1.0：同样 6 个里的 5 个（降速那个不受影响）；
    - 去掉 `Doubling` 自己的拥塞检查：`test_no_growth_while_congested` 和 `doubling_waits_for_every_runner_and_the_congestion`；
    - 去掉不活跃 runner 的检查：`test_no_growth_while_a_runner_is_inactive` 和 `doubling_waits_for_every_runner_and_the_congestion`；
    - 目标不按上限截断：`test_the_target_is_capped_at_the_limit`、`test_no_growth_at_the_limit`、`test_runners_that_stopped_at_the_limit_are_replaced`、`test_a_truncated_growth_is_not_judged` 和 `doubling_grows_back_as_the_limit_recovers_a_step_at_a_time`；
    - 截断的扩张也判断停滞：`test_a_truncated_growth_is_not_judged` 和 `doubling_grows_back_as_the_limit_recovers_a_step_at_a_time`；
    - 尝试只用（ID，chunk 起点）识别，不用 generation：`test_a_retry_on_the_same_start_starts_without_a_rate`；
    - 门槛改用任务速度：`planned_split_size_follows_the_donor_rate`；
    - 去掉门槛（剩余量少于 `planned` 也切）：`doubling_splits_only_a_chunk_of_the_planned_size`；
    - `planned` 可以低于 `min_chunk_size`：`test_planned_split_size_follows_the_donor`；
    - `Legacy` 改动：去掉 `abs()`（`test_legacy_thresholds_and_abs`、`legacy_is_the_default_and_unchanged`、`test_strategy_tick_while_finished_runners_keep_their_slots`）；QuickStart 阈值改成 0.5 MiB/s（`test_legacy_thresholds_and_abs`）；`planned` 改成速度 × 3（`legacy_is_the_default_and_unchanged`）；`Legacy` 走 `Doubling`（`legacy_is_the_default_and_unchanged` 和两个原有的 dynamic 策略测试）；默认值改成 `Doubling`（6 个测试）；
    - 判断停滞时不看活跃数：`test_a_tick_with_fewer_runners_does_not_judge_the_growth` 和 `test_runners_that_stopped_at_the_limit_are_replaced`；
    - 一个都没加也算扩张：`test_a_refused_split_is_no_growth`、`doubling_counts_only_the_splits_a_slot_took` 和 `doubling_splits_only_a_chunk_of_the_planned_size`；
    - 去掉 2 s 间隔：`test_the_grow_interval`、`test_a_refused_split_is_no_growth`、`doubling_grows_through_split_up_to_the_limit` 和 `doubling_judges_the_gain_at_ten_percent`。

### 14.15 低速 runner 重启（PR-7，C12）

- **做法。**
  - 只在 `Doubling` 下生效。只有 `Doubling` 采样每个 runner 的字节数（`RunnerRates`），`Legacy` 没有这份数据，不重启任何 runner。
  - 单独停掉一个 runner：
    - 以前所有 runner 共用任务的 cancel token（取消令牌），没法只停一个（§9.6 末尾）。
    - 现在 spawn runner 时，除了任务令牌，再给它一个任务令牌的子令牌，作为它自己的停止开关（`RunnerChannels::stop`，runner 一侧是 `TaskRunnerBuilder::stop_token`）。取消子令牌只停这一个 runner；取消任务令牌仍然停掉全部。
    - runner manager 记下每个 runner 的停止开关。`stop_runner` 只停正在运行（Running）、还有剩余字节的 runner，每个只停一次，并记下它“正在停止”。
    - 不用“丢掉 limit sender”的办法：那样 runner 以 `ChannelClosed` 结束，按 §14.13 收到过数据也要记预算。
  - 停止开关什么时候生效：
    - runner 还没决定怎么结束时，停止开关和任务令牌一样让它停下，报 `Cancelled`。包括：在连接；在读流（缓冲里的字节随之丢掉）；在交一段缓冲，而当前这一项（adapter 一次交来的数据）后面还有内容。
    - runner 已经决定怎么结束时，停止开关不打断它，它照原样结束。包括：流已经结束、出错或空闲超时，它正在交最后一段缓冲；控制信号出错之后它正在交缓冲；它在等数据通道里的帧被取走（drain）。它最后报自己的错误，或者报完成。停止开关只是不改写这个结果；任务取消、交付失败和原有的 drain 超时照旧生效，比如帧一直没被取走，drain 超时后仍然报 `Timeout`。
    - 任务令牌照旧，在任何时候都让 runner 报 `Cancelled`。
    - 为什么这样分：已经决定的结果是真实的。比如不可重试的错误被改成 `Cancelled`，就会被当成低速停止，变成可重试的 `Timeout`，有数据时连预算都不记。已经下完、正在等帧被取走的 runner 报完成，区间也不用重下。
  - runner 报告 `Stopped` 时，manager 先看它是不是被停的：
    - 被停的、报的是 `Cancelled`：按低速处理（`AttemptFailure::slow`）。区间从 planner 已经接受的位置退回，下载循环照常退避（§14.12）。还在它数据通道里、没被取走的帧，随通道一起丢掉。
    - 在注意到停止之前自己下完了：报 `Finished`，或者报 `Cancelled` 但 planner 已经接受了整个 chunk。两种都按完成处理：不算失败，什么都不释放，所以区间不会被释放两次。
    - 自己出错了：按它自己的错误处理。包括停止开关被拉下时，它已经拿着错误、正在交最后一段缓冲或等帧被取走的情况（见上）。
    - 槽位在处理 `Stopped` 时回收，和别的 runner 一样（§14.11）。
  - 分类（§8.2、§14.13）：低速停止报为可重试的 `TaskError::Timeout`，因为这个 runner 在 10 s 里远远落后于别的 runner。所以失败策略把它当超时处理：
    - 这次尝试有字节被接受：是重启。不记预算，不算连续失败，`attempts + 1`，然后退避。
    - 没有字节被接受：和没数据的超时一样，`charged + 1`，也算一次连续失败。所以一直不交数据的 runner 不会每 10 s 被重启一次、却永远耗不尽预算：到 `max_attempts_per_range` 次时任务以 `RangeExhausted` 结束，`source` 是 `Timeout`。
    - `AttemptFailure` 多了一个 `slow` 字段，区分低速停止和 runner 自己的超时，只用于日志和测试。
    - 并发控制的结果采样（`RunnerOutcomeSampler`）不记这次停止：这是下载循环自己的决定，不是服务器出错，不应该让并发降级。
  - 窗口字节（`RunnerRates::window_bytes`）：
    - `RunnerRates` 保留最近一个窗口的采样，不再只留上一次。
    - 窗口的起点，是“最后一次采样之前至少 `slow_window`（10 s）”的那些采样里最晚的一次。tick 是 1.5 s，所以窗口实际是 10.5 s。所有 runner 用同一个窗口。
    - 窗口字节 = 窗口内这个 runner 被 planner 接受的字节。窗口一样长，所以比字节就是比速率。
    - 只有在窗口起点已经在运行、并且一直是同一次尝试（同一个 generation）的 runner 才有窗口字节。比窗口年轻的、窗口起点还在连接的、窗口内换了尝试的，都不评判。
    - 时间都取 tick 传进来的 `now`。
  - 判定（`strategy/slow_restart.rs`）：每个策略 tick 一次，在扩张之前。依次检查：
    1. 没有限速（P2 之前恒为真）；
    2. 不在拥塞期间（新增，见偏差）；
    3. 有窗口字节的 runner 至少 3 个（`MIN_JUDGED_RUNNERS`）；
    4. 中位数取这些 runner 的窗口字节；
    5. 候选：`窗口字节 + F < 0.25 × max(中位数 − F, 0)`（严格小于，正好在边界上不算“低于”），并且剩余量 ≥ `2 × min_chunk_size`（正好 2 倍算）。F 是一帧，即 runner 攒够才交出的缓冲大小 `BUFFER_SIZE`（32 KiB），见决定；
    6. 有多个候选时，只停最慢的一个；字节相同时取 ID 小的。
  - 已经在停止的 runner 不参与评判，也不算进中位数。这一步在下载循环的 `may_restart` 里，T27 要在同一个地方排除对冲中的一对 runner（§9.6）。
  - 与 C11 的配合：
    - 重启在扩张之前判定。扩张选“剩余最多的 Running chunk”时，跳过正在停止的 runner（慢 runner 的剩余往往最多），所以同一个 tick 的扩张不会去切它。
    - 重启不调用 `on_grown`，不算扩张。
    - 被停的 runner 在处理 `Stopped` 之前仍然是活跃 runner，也还占着槽位。之后活跃数少 1。如果正有一次翻倍等着判断，这次判断作废（§14.14：活跃数少于翻倍刚完成时，不下结论）。这是合理的：少了一个 runner，这个 tick 量不出翻倍的效果，既不能说停滞，也不能说有增益。C28 起，翻到上限的那次翻倍不作废：等到活跃数回来再判，期间不扩张；区间冷却结束后被重新领取，活跃数就回来了。上限以下的照旧作废（§14.18）。
    - 区间冷却结束后，由唤醒或并发控制的 `CreateTask` 补回 runner，也可能由 `Doubling` 的扩张补回。补回的 runner 是新的尝试，要再满一个窗口才会被评判。
- **决定。**
  - 每个 tick 最多重启一个，停最慢的。原因：停掉一个连接后，其余连接的速率会变（带宽重新分配），下一个 tick 用新的数据再判断，比一次停掉好几个稳妥，也不会对同一个服务器同时发出一批重连。几个慢 runner 要几个 tick（每个 1.5 s）才停完，和 10 s 的窗口相比不算慢。
  - 至少 3 个有完整窗口的 runner，中位数只从它们算。2 个的中位数就是平均值，一个快 runner 就能定下门槛。它们都是活跃 runner，所以 §9.5 的条件 3（至少 3 个活跃 runner）一定满足，代码里不再单独检查活跃数。
  - 偶数个时，中位数取中间两个的平均。这是通常的定义；4 个以上时，最慢的那个不会落在中间两个里。
  - 两边各让出一帧（2026-11-07 与 lead 确认）：
    - runner 攒满一帧（32 KiB）才交出去。一个窗口里数到的字节，可能比真实的少一帧（窗口结束时还在缓冲里），也可能多一帧（窗口开始前已经读到、窗口里才交出的那一帧）。
    - 所以候选按它最多可能交了多少算（加一帧），中位数按最少可能交了多少算（减一帧），再比较。
    - 例子：3 个 runner 都是 2 KiB/s，一个窗口约 21 KiB。缓冲的相位不同，两个各交了一帧，第三个一帧也没交，数到的是 32 KiB、32 KiB、0。不让这一帧，第三个会被停；它的缓冲被丢掉，`received = 0`，还要记预算，`max_attempts_per_range = 1` 时任务直接以 `RangeExhausted` 结束，其实什么问题都没有。让出一帧后，没有谁被判慢。
    - MiB/s 的速度下，一帧的影响量不出来。别的 runner 都很快、自己一帧都没交的 runner，照样被停。
    - 没有再加单独的下限（比如“中位数低于若干帧就不判”）：边界本身已经有这个效果，中位数不到 5 帧时谁都不会被判慢（见已知限制），再加一个常量只是重复。
  - 已经决定怎么结束的 runner，停止开关不打断它（2026-11-07 与 lead 确认，见做法）。runner 同时拿着任务令牌和自己的停止开关，所以能分清是任务取消还是只停它一个。拿着 `Ok(())` 在等帧被取走的 runner 也不打断，它最后报完成。
  - 会不会连锁重启：
    - 每次重启都要有 3 个有完整窗口的 runner。被停的不再评判，补回的要等 10 s 以上才会被评判。所以只剩 3 个时停掉一个，剩下 2 个，不会接着停第二个。
    - runner 多时，可能连续几个 tick 各停一个。但每一个都要低于中位数的 25%。去掉最慢的那个，中位数只会不变或变大，所以后面被停的确实比别人慢很多，这正是要重启的。
    - 同一个区间反复慢：有数据时，每次重启都让区间变短（§14.13）；没数据时，每次都记预算，最多 `max_attempts_per_range` 次。
- **与 §9.5 的偏差。**
  - 条件 3 改为“至少 3 个有完整窗口的 runner”，比“至少 3 个活跃 runner”严（2026-11-07 与 lead 确认）。
  - 新增：拥塞期间（`congestion_until` 之前）不重启。退回的区间在拥塞结束前反正不能重连，停掉一个还在交数据的连接只会更慢（2026-11-07 与 lead 确认）。
  - 比的是窗口字节，并且两边各让出一帧，不是直接拿速率和中位数的 25% 比（见决定）。
  - 窗口和 tick 对齐：从至少 10 s 之前的最近一次采样算起，1.5 s 的 tick 下是 10.5 s。
  - §9.5 只写了 `attempts += 1`。这里按 §14.13 再分两种：有数据的停止是重启，不记预算；没数据的停止记预算。
  - 低速停止没有新的公开错误类型：`TaskError` 是公开的、不是 `#[non_exhaustive]`，加变体会破坏兼容。它在 crate 内部用 `AttemptFailure::slow` 区分，对外报 `Timeout`（2026-11-07 与 lead 确认）。
- **已知限制。**
  - 被停 runner 已经读到、但 manager 还没取走的数据会丢掉，区间从已接受的位置重新下载。丢掉的是：数据通道里的帧（最多 32 帧，每帧最多 32 KiB）、runner 自己的缓冲（最多 32 KiB），以及当前这一项还没放进缓冲的部分。adapter 一项可以有好几 MiB，所以没有固定的上限。
  - 慢链接上不重启：中位数每个窗口不到 5 帧（160 KiB，约 15 KiB/s）时，谁都不会被判慢，连一帧都没交的 runner 也不会。这时的字节数分不清“慢”和“缓冲相位不同”。这样的 runner 只受空闲超时（默认 30 s）约束。
  - writer 满时下载循环不取数据，这段时间所有 runner 的字节一起偏少。按比例比较，不会因此误判，但可能推迟发现。
  - 默认值 10 s 和 0.25 还没有经过基准测试校准，留给 T14。T14 的结果见 §14.17：`slow_window` 建议缩短，`slow_ratio` 没有数据。
  - 已经决定怎么结束的 runner 不理停止开关：正在交最后一段缓冲时，它要等数据通道有空位；在 drain 时，最多等 5 s 的 drain 超时。这段时间它一直占着槽位，也不会被再次评判。
  - 被停的 runner 的生命周期通道没发 `Stopped` 就关闭时，仍按丢失处理（`ChannelClosed`，记预算）。正常的 runner 被取消时总会发 `Stopped`。
  - manager 把“被停、报 `Cancelled`”一律当低速停止。T27 的对冲败方也要用这个停止开关，那时要先记下停止的原因（低速重启还是对冲败方），再分别处理，见 §9.6 的说明。
  - 被停的 runner 在报 `Stopped` 之前仍占着 ID，所以仍算在 `on_failure` 的在途数里：只剩它一个时，别的 runner 的 429/503 会被容忍。T27 起在途数减去所有正在停止的 runner，这时任务以那个 429/503 结束（§9.6 的 M3）。
- **验证。**
  - `slow_restart.rs` 的单元测试，字节数都由测试构造：
    - `test_a_runner_below_a_quarter_of_the_median_is_restarted`：正好在边界上（`字节 + F = 0.25 × (中位数 − F)`）不重启，中位数多 4 B 就重启；候选是 0 或 1 MiB 都试；
    - `test_runners_a_frame_apart_are_not_slow`：速率相同、只是相位不同的 runner，`[F, F, 0]`、`[2F, 2F, F]`、`[2F, 2F, 0]` 和 4 个的情况都不重启；
    - `test_a_silent_runner_next_to_fast_ones_is_restarted`：别的都在 7 MiB 左右，一帧没交或只交了一帧的都被停；
    - `test_a_runner_near_its_end_is_not_restarted`：剩余量比 `2 × min_chunk_size` 少 1 B 不重启，正好相等重启；
    - `test_no_restart_with_two_runners`：两个 runner 差得再远也不重启，加上第三个就重启；
    - `test_the_median_of_an_even_count`：偶数个取中间两个的平均，正好在边界上不重启、多 1 B 重启；改取偏下或偏上的中间值，结果都会不同；
    - `test_only_the_slowest_is_restarted`：一次只停最慢的，字节相同取 ID 小的，最慢的快下完时停次慢的；
    - `test_no_restart_under_a_speed_limit_or_a_congestion`、`test_median`。
  - `RunnerRates` 的单元测试：
    - `test_window_bytes_need_a_full_window`：比 10 s 少 1 ns 时没有窗口字节，正好 10 s 时有；按 1.5 s 的 tick，窗口是 10.5 s，只保留 8 次采样；
    - `test_window_bytes_belong_to_a_running_attempt`：窗口起点还在连接的、窗口内换了尝试的都没有窗口字节，切分后还有。
  - runner 的单元测试 `test_a_stop_keeps_the_end_a_runner_decided`，runner 由测试手动推进：
    - 停止开关让在读流的、在交缓冲而当前项后面还有内容的 runner 报 `Cancelled`；
    - 不打断已经决定的结果：流出错后在 drain 的，报原来的流错误；流结束后在 drain 的，报完成；流出错后在交最后一段缓冲（数据通道满）的，报原来的流错误；控制信号出错后在交缓冲的，报 `ExceededTotalSize`；
    - 同样 6 种情况，取消任务令牌时都报 `Cancelled`。
  - `FailurePolicy`：`test_a_slow_runner_is_classified_like_a_timeout`。有数据的低速停止连续 8 次，都不记预算、不算连续失败；没数据的记预算，算连续失败，第 2 次以 `RangeExhausted` 结束，`source` 是 `Timeout`。
  - runner manager：
    - `test_a_stopped_runner_is_a_slow_attempt`：还在连接的、没有停止开关的都不停；只停一次；退回已接受位置之后的部分，通道里的帧丢掉；槽位在 `Stopped` 时回收；不记入结果采样；回收的 ID 再用时不带“正在停止”。
    - `test_a_runner_that_completes_while_stopping_finishes`：被停之后 chunk 被全部接受，报 `Finished` 或 `Cancelled` 都按完成处理；扩张跳过正在停止的 runner。
  - 下载循环的测试（`download_loop/tests/restart.rs`，用脚本化的 runtime，显式传入时间）。除最后一个外，另外 3 个 runner 每个 tick 各交 1 MiB：
    - `a_slow_runner_is_stopped_and_its_range_retried`：被评判的是一个真实的 runner，停在 spawn 处，由测试手动推进。它交 64 KiB 后停住。满 10 s 之前 1 ns 不停；满 10 s 时下载循环通过它自己的停止开关停掉它，槽位一直占到它报 `Stopped`。它报 `Cancelled`，区间从 64 KiB 处退回，`attempts = 1`、`charged = 0`。连续失败上限和每区间上限都是 1，只要记了其中一个，任务就会结束，但任务照常继续。冷却结束前 1 ns 不领取，结束时由唤醒领取，`attempts = 1`、`charged = 0`。
    - `a_slow_runner_without_data_exhausts_its_range`：真实 runner 一直不交数据。每次停止 `charged` 加 1，第 3 次以 `RangeExhausted { attempts: 3 }` 结束，`source` 是 `Timeout`。
    - `a_stop_keeps_the_error_a_runner_holds`：真实 runner 交了 96 KiB 后流以不可重试的错误结束，3 帧还在通道里等着被取走时被判慢、被停。它继续等帧被取走，然后报原来的流错误：`slow = false`、不可重试，任务以这个错误结束，没有冷却记录。
    - `a_runner_that_completes_while_stopping_is_no_failure`：决定停止之后、runner 注意到之前，它的 chunk 被全部接受。报 `Finished` 或 `Cancelled` 都不算失败，不释放、不退避，槽位只回收一次。在这之前，下一个 tick 不再评判它，扩张也不选它。
    - `legacy_never_restarts`：同样的场景跑 30 s，`Doubling` 停掉慢 runner，`Legacy` 不采样，也不停。
    - `runners_at_the_same_low_rate_are_never_restarted`：3 个真实 runner 都是 2 KiB/s，窗口开始时缓冲里分别有 30、15、0 KiB。第一个完整窗口数到的是两个 32 KiB 和一个 0；之后每个窗口每个 runner 交一帧或不交。跑 30 s，没有一个被停。
  - 已有测试：只有机械改动，测试里构造的 `RunnerChannels` 加了 `stop`。
  - 变异检查，每种都有测试失败：
    - 去掉门槛（候选不再和中位数比）：`test_a_runner_below_a_quarter_of_the_median_is_restarted`、`test_a_runner_near_its_end_is_not_restarted`、`test_runners_a_frame_apart_are_not_slow`、`test_the_median_of_an_even_count` 和 `runners_at_the_same_low_rate_are_never_restarted`；
    - `<` 改成 `<=`：`test_a_runner_below_a_quarter_of_the_median_is_restarted` 和 `test_the_median_of_an_even_count`；
    - 候选不加一帧：`test_a_runner_below_a_quarter_of_the_median_is_restarted`、`test_runners_a_frame_apart_are_not_slow` 和 `test_the_median_of_an_even_count`；
    - 中位数不减一帧：`test_a_runner_below_a_quarter_of_the_median_is_restarted` 和 `test_the_median_of_an_even_count`；
    - 两边都不让一帧：上面这三个测试，以及 `runners_at_the_same_low_rate_are_never_restarted`；
    - 去掉剩余量门槛：`test_a_runner_near_its_end_is_not_restarted` 和 `test_only_the_slowest_is_restarted`；
    - 去掉“至少 3 个”：`test_no_restart_with_two_runners`；
    - 去掉完整窗口的要求（取上一次采样作起点）：两个 `RunnerRates` 测试，以及 `a_slow_runner_is_stopped_and_its_range_retried`、`a_slow_runner_without_data_exhausts_its_range` 和 `runners_at_the_same_low_rate_are_never_restarted`；
    - 有数据的停止也记预算：`test_a_slow_runner_is_classified_like_a_timeout`、`test_a_stopped_runner_is_a_slow_attempt` 和 `a_slow_runner_is_stopped_and_its_range_retried`；
    - 没数据的停止不记预算：`test_a_slow_runner_is_classified_like_a_timeout` 和 `a_slow_runner_without_data_exhausts_its_range`；
    - 改为丢掉 limit sender 来停（runner 以 `ChannelClosed` 结束）：`test_a_stopped_runner_is_a_slow_attempt` 和 4 个下载循环测试；
    - 停止开关在 drain 时也打断 runner、盖掉它拿着的结果：`a_stop_keeps_the_error_a_runner_holds`；
    - 停止开关打断已经决定结果、正在交最后一段缓冲的 runner：`test_a_stop_keeps_the_end_a_runner_decided`；
    - 控制信号出错之后，停止开关仍然打断：`test_a_stop_keeps_the_end_a_runner_decided`；
    - `Legacy` 也采样并重启：`legacy_never_restarts` 和 `legacy_is_the_default_and_unchanged`；
    - 扩张不跳过正在停止的 runner：`test_a_runner_that_completes_while_stopping_finishes` 和 `a_runner_that_completes_while_stopping_is_no_failure`；
    - 不检查“停止之前已经下完”：同样这两个测试；
    - 偶数个时中位数取偏下的中间值：`test_median` 和 `test_the_median_of_an_even_count`；
    - 去掉拥塞门槛、去掉限速门槛：各自都让 `test_no_restart_under_a_speed_limit_or_a_congestion` 失败。
    - 已知会偶发失败的 `test_save_into_rejects_hostile_filenames` 在其中 2 次里也失败了，和这些变异无关。

### 14.16 收尾对冲（PR-7，C27）

- **做法。** 按 §9.6 实现，只在 `Doubling` 下生效。
  - 记录（§9.6“新增的记录”）：
    - `ChunkState::hedge: Option<Hedge>` 是这次尝试的对冲记录，含 `fork`、`owner_side`、`first`，从分叉一直留到这次尝试结束：chunk 收满时（`update_progress` 和 `mark_finished`，状态 F），或者 `mark_failed` 退回区间时清掉。收满之后、`Stopped` 之前出错的 runner 按普通 runner 分类：区间已空，不查预算（`a_failure_after_the_last_byte_exhausts_nothing`）。它在，这次尝试就不再对冲。
    - §9.6 的 `pair` 用同一个记录里的两个字段表示：`undecided`（还没有取到分叉之后的非空帧）和 `partner`（两方都在时的 H，含它是否已报 `Started`）。状态 B 是 `undecided` 且有 `partner`；状态 C 是 `undecided`、没有 `partner`，也就是只剩幸存方的单方记录；第一个非空帧到来时 `undecided` 变成 `false`。
    - planner 加了 `hedge_of`（H → chunk 起点），只收录活着、不是 owner 的 H。`fork_hedge`、`hand_over`、`drop_partner`、`decide` 在改 owner 之前先删旧项，所以 ID 被回收、重用时，两张表里都没有它的旧项。这些都在 planner 里，`PlannerGuard` 回滚时一起恢复。
    - `get_active_runners_count` 加上 `hedge_of` 的项数：两方都在时一对算两个。败方不算活跃，但占着槽位。
    - manager 的 `stopping` 改成 `HashMap<RunnerId, StopReason>`（`Slow` 或 `Loser`）；`last_delivery` 记静默的起点。数据聚合器里的接收端由 `Arc<Mutex<_>>` 共享（`SharedReceiver`）：`StreamGroup` 照常轮询它，仲裁也能通过 `take_first_data` 直接从里面取帧。
  - 触发（`download_loop/hedge.rs`）：
    - 每个策略 tick 在低速重启和策略动作之后调用 `hedge_stalled_runner`，所以策略建的 runner 先用槽位。
    - `hedge_candidate` 每次都重查全部条件：有 `RunnerRates`（只有 `Doubling` 有）、writer 能接数据、不拥塞、活跃数低于 `max_concurrency`、没有可领取的区间；候选的 chunk 没对冲过、剩余量大于 0 且小于 `2 × min_chunk_size`，runner 不在停止中，静默不少于 `HEDGE_IDLE`（2 s），并且除了它自己的 chunk，没有别的 chunk 能切。先按全部条件筛出候选，再取静默最久的，一样久时取 ID 小的；先取静默最久的再查条件，会漏掉合格的候选（`the_longest_silence_among_runners_that_qualify`）。
    - 别的 chunk 按 C11 的门槛 `planned_split_size` 判断能不能切；Pending 的 chunk 传 `None`，即用 `per_runner_speed`。正在停止的 runner 的 chunk、`hedge` 不为空的 chunk 都不算。
    - 占到槽位后由 `RunnerManager::spawn_hedge` 发起：planner 的 `fork_hedge` 返回 `[分叉点, chunk 末尾)`，H 从这里请求；`last_delivery[H] = now`。不分配新的区间，不动计数和无进展截止时间。
  - 静默：
    - `spawn_in` 和 `spawn_hedge` 多了 `now` 参数，spawn 时写 `last_delivery`。
    - `DownloadedChunk` 加了 `runner_id`；`accept_chunk` 接受非空帧时用 `handle_event` 的 `now` 调用 `note_delivery`。
    - `poll_writer` 在调用 `poll_completions` 之前读 `!can_write()`，随 `LoopEvent::WriterCompleted(_, was_full)` 带出；`record_completions` 看到 `was_full` 并且现在可写，就 `restart_silences(now)`。`handle_event` 拆出 `handle_event_at(event, now)`，测试可以传自己的时间。
  - 仲裁（`runner_manager/data.rs` 的 `on_data_frame`），在 `meters`、planner 和 writer 之前：
    - 帧来自 `Loser`：丢掉。
    - 帧来自 H（`partner_chunk` 找得到）：空帧丢掉，不决定胜负；非空帧先看 O——O 已连上（接收端在聚合器里）时，`take_first_data` 从 O 的通道里依次取帧，丢掉空帧，取到第一个非空帧就停：取到了，O 胜，H 这一帧丢掉，O 的这一帧当场照常处理，后面的帧留在通道里按顺序取；没取到，H 胜：`hand_over`、`decide`，再照常处理 H 的帧。
    - 帧来自 owner：非空、并且 `undecided` 时，有 `partner` 就先让它出局并停掉，再 `decide`；然后照常处理。
    - “停败方”（`stop_loser`）：拉下停止开关，记 `Loser`，从数据聚合器或 `connecting_data` 拿掉接收端（排着的帧一起丢掉），删掉 `meters`、`last_delivery` 的项；ID、limit sender、生命周期接收端留到确认终止。
  - 生命周期（`runner_manager/lifecycle.rs`）：
    - 先看 `Loser`：`Started` 只忽略；`Stopped`、通道关闭、spawn 被拒绝都由 `handle_loser_ended` 回收一次，不报任何东西。
    - H 的 `Started` 只标记 `partner.running`，并把数据接收端移进聚合器，不调用 planner 的 `mark_running`。
    - 两方都在时（`has_other_side`），任一方的 `Stopped`、通道关闭、spawn 被拒绝都走 `handle_side_failed`：O 出局时 `hand_over`，H 出局时 `drop_partner`；回收它；`record_side_failure` 记下 `first`；结果采样照常记；返回 `SideFailed`。B 里任一方、C 里幸存方报 `Finished` 都不该出现（`debug_assert`），出现了按 `ChannelClosed` 处理，和状态表一样。
    - owner 的其它失败都经过 `fail_attempt`：先取 `owner_bytes`（按 `owner_side` 选 `Released::received` 或“已下载末尾 − `fork`”）和 `hedge.first`，再 `mark_failed`，再由 `AttemptFailure::of_attempt`（低速停止是 `slow_of_attempt`）算这一对的 `charged`：`first` 和这一方都不是重启、并且至少一方记预算时加 1；没有 `first` 时就是 `charges_range`。`received` 报这一方自己的字节。
  - 下载循环（`download_loop/runner.rs`）：
    - `on_side_failure`：先查 §4.3 的回退；再 `FailurePolicy::on_side_failure`（429/503 容忍，重启不计，其它可重试的失败计入连续失败，其它不可重试的错误终止任务）；429/503 时 `Backoff::hold_congestion`（用 `SideFailed::attempts`，即 `chunk.attempts + 1`）推后 `congestion_until`，并按 `next_retry` 提前唤醒。不退避、不写冷却记录。
    - `on_failure` 的在途数改成 `RunnerManager::in_flight()`：占着 ID 的 runner 数减去 `stopping` 的项（M3）。失败的 runner 在这之前已经回收。
  - `FailurePolicy::on_failure` 先查预算：可重试或 429/503、区间非空、`charged` 达到上限时以 `RangeExhausted` 结束；之后才是容忍 429/503、重启和连续失败。
  - 排除：
    - `may_restart` 排除 `is_in_undecided_pair` 的 runner，它们也不进中位数；`stop_runner` 拒绝 `undecided` 的 chunk 的 owner。
    - `largest_running_chunk` 跳过 `hedge` 不为空的 chunk；`resize_runner_state` 和 `split_chunk` 拒绝这样的 chunk。
  - 测试辅助：`FileWriter::recording()`（只在测试里）记下每次写入的区间，写入通过 spawn 的任务进行，测试决定它们什么时候完成；`RangeFault::StalledOnce`（`bolt-load-tests`）对第一个起点大于 0 的 range 请求只发 body 的前一半，然后既不发也不关闭。它不在 `RangeFault::ALL` 里，`from_name` 另外认它。
- **与连接上限的接口（重排到 PR-6a 之后，§8.3、§14.13a）。** 一对里的每一侧失败时，都经过同一个分类钩子 `record_attempt_end`，用那一刻的状态分类：在已证实窗口之上被拒绝的一侧，`SideFailure.charged` 为 false；第二侧也一样，由 `of_attempt` 带上它自己的免记。一对的公式不变：两侧都不是重启、且任一侧记预算时，记一次。对冲 runner 的 `Started` 和别的 runner 一样经过 `on_started`；被击败、连接中的一侧晚到的 `Started` 也记进 `connected`（它占着一个连接，直到结束），在 `release_runner` 时离开。“已证实”只看 chunk 当前 owner 自己这一侧被接收的字节：对冲接手 chunk 时，原 runner 在分叉之前交付的字节不算它的（`a_hedge_handed_the_chunk_is_not_proven_by_the_originals_bytes`、`a_beaten_side_started_late_is_connected_until_it_ends`）。对冲的闸门除了 `max_concurrency`（即 `limit_cap`），也看 `refill_cap`：恢复 step 已被拒绝或不确定的标记置上之后，不再用试探的位置对冲。测试：`each_side_of_a_pair_is_classified_when_it_fails`（先免记再记、反过来、两侧都免记：各记 1、1、0 次）和 `a_hedge_opens_no_connection_above_the_limit`（降到两个时不对冲；step 被标记之后也不对冲）。去掉一侧的免记或对冲闸门里的 `refill_cap`，对应的测试失败。
- **与 §9.6 的偏差。**
  - `pair` 不是独立的记录，而是 `Hedge` 里的 `undecided` 和 `partner`，含义和状态表相同（见做法）。
  - 预算先查的顺序对所有失败都生效，不只对一对。单个 runner 的重启和 429/503 从不加 `charged`，区间还活着时 `charged` 小于上限，所以对它们没有区别；已有的测试一条都没改。
  - 看原 runner 的通道用的是 §9.6 给的第二种做法的变体：接收端不移动，仲裁通过共享的句柄直接取 O 的第一个非空帧，空帧不算（只看“通道里有没有帧”的话，排着的空帧也会让 O 胜）。所以没有“转移时丢帧或丢唤醒”的风险；`the_original_wins_a_tie` 验证取到的帧当场写入、后面的帧照常按顺序取走，`queued_empty_frames_do_not_win_a_tie` 验证排着的只有空帧时 H 胜。
  - 只有经过 `spawn_in` 或 `spawn_hedge` 起的 runner 有 `last_delivery`，没有的不是候选。生产代码只用这两个入口；测试里用 `allocate_runner_with_chunk` 建的 runner 不会被对冲。
  - 静默一样久的候选取 ID 小的，§9.6 没说。
- **已知限制。**
  - 对冲只在策略 tick 上判定，实际在静默满 2 s 之后的第一个 tick，也就是 2–3.5 s。
  - M1：卡住的小 chunk 旁边还有可切的 chunk 时、或者有完整窗口的 runner 不到 3 个时，对冲和低速重启都不管，只能等空闲超时（T14 第 4 个场景去量）。量出的代价见 §14.17：第一种 +56%，第二种 +29.8 s。
  - M5：冷却中的区间不算“可切的活”；两方都卡住、H 占了最后一个槽位时，这个区间要等一方的计时器（连接超时之后还可以再接一个空闲超时）、别的 runner 停下，或者无进展超时。
  - 败方在确认终止之前占着槽位，但不算活跃；降级之后实际连接数可能短暂超过新的上限，和已经下完、还没报 `Stopped` 的 runner 一样（§14.11）。
  - 败方读到的数据会被丢掉，重复下载的字节数由 T14 量。§14.17 报的是服务器多发的总字节数，不区分来源，拆不出单次对冲的量。
  - 每个数据接收端都放在 `Arc<Mutex<_>>` 里，聚合器每取一帧都要加一次锁。锁只在同一个线程里用，不会争用，但热路径上的开销还没量过，留给 T14。T14 单独量的无争用加锁约 8.5 ns（不是在 crate 里量的），端到端量不出差别（§14.17）。
  - `RunnerRates` 只采样 owner，看不到还在连接的 H，所以 `Doubling` 的检查 5 不会因为它跳过扩张；活跃数却算上了它。收尾阶段没有可切的 chunk，影响很小。
- **验证。**
  - 下载循环的测试（`download_loop/tests/hedge.rs`，脚本化的 runtime，显式传入时间；runner 由测试持有通道，触发时 spawn 的 H 是停在 spawn 处的真实 runner；写入用 `FileWriter::recording()` 记录，`Harness::written` 断言没有字节被写两次）：
    - 触发：`a_runner_silent_for_two_seconds_is_hedged`（差 1 ns 不发起，正好 2 s 发起，H 经 spawner 起在 chunk 剩下的部分，两方都算活跃，无进展截止时间不变）、`legacy_never_hedges`、`a_connecting_runner_is_silent_from_its_spawn`、`the_runner_silent_longest_is_hedged_first`（每次调用只发起一个）、`no_hedge_without_a_free_connection_or_with_a_range_left`（有可领取的区间、没有空闲槽位、到了并发上限、拥塞期间）、`the_candidate_has_less_than_two_minimum_chunks_left`（正好 `2 × min_chunk_size` 不发起，少 1 B 发起）、`no_hedge_while_another_chunk_may_be_split`（别的 chunk 剩余量正好等于它的 `planned_split_size` 不发起，少 1 B 发起）、`a_connecting_chunk_is_judged_by_the_per_runner_speed`（连续采样两次、速率是 `Some(0.0)` 的 Pending chunk 按 `per_runner_speed` 判断）、`a_stopping_runner_is_not_hedged`、`a_full_writer_restarts_every_silence`（真实的 `PendingWriter`：写满时不对冲；一次写入完成后经 `poll_writer` 和 `handle_event_at` 处理，静默从那一刻重算，差 1 ns 不发起）、`a_rejected_hedge_ends_the_task`（H 的 spawn 被拒绝：槽位回收一次，区间不释放，任务以 `TaskError::Other` 结束）。
    - 仲裁：`the_first_frame_decides_the_pair`（两种胜方；败方停掉，迟到的帧和 `Finished` 被忽略，槽位在它确认时回收一次；写入的区间不重叠，正好覆盖文件）、`the_original_wins_a_tie`（排在前面的空帧被跳过）、`queued_empty_frames_do_not_win_a_tie`、`a_connecting_original_loses_to_the_first_frame`（迟到的 `Started` 被忽略）、`an_empty_frame_decides_nothing`、`a_loser_ends_without_a_trace`（迟到的 `Started` 不回收；`Finished`、`Cancelled`、可重试的错误、503、§4.3 回退错误、其它不可重试的错误、通道关闭都只回收一次，不回退、不终止、不降并发、不改 `congestion_until`、不计连续失败、不记结果采样）、`a_loser_ends_whatever_became_of_the_chunk`（W 在下载、W 已失败、区间已被重新领取、chunk 已收满但任务没完成）、`a_reused_id_is_a_new_runner`（H 先出局、O 输掉，两种 ID 重用）。
    - 错误与预算：`one_side_fails_and_the_other_runs_on`（重启不计连续失败，没数据的失败计 1；区间不退回、不冷却；之后不再对冲）、`a_congested_side_holds_connections_back`、`an_unretryable_side_ends_the_task`、`the_winner_inherits_the_counts`、`a_pair_is_charged_at_most_once`（§9.6 预算表的五种顺序，加上对冲用自己的 0 字节分类的两种顺序和两方都记预算）、`the_first_failure_outlives_the_survivors_frame`、`the_survivor_fails_after_its_own_data`（状态 C′ 的 `Timeout` 和 `ChannelClosed`，H 的字节从分叉点算）、`the_budget_of_a_pair_comes_first`（临界点的四种情况）、`the_last_side_is_not_in_flight`、`rounds_of_failed_pairs`、`stopping_runners_are_not_in_flight`（`Loser` 和 `Slow`）。
    - 排除：`a_hedged_chunk_is_neither_restarted_nor_split`（B 里 O 从 Pending 变成 Running 也不切、不停；D 里两种胜方都不切；另一个候选不把它们当成可切的活）、`the_survivor_of_an_undecided_pair_is_not_judged`（状态 C 的 O 不进中位数：算上它，慢 runner 就不够慢）。
    - 计时：`the_hedge_never_moves_the_no_progress_deadline`、`a_backed_off_range_waits_for_a_side_to_end`（M5 的长路径：O 快到连接超时才连上，再等一个空闲超时；期间区间不被领取，唤醒不空转）。
  - 集成测试：`a_stalled_connection_is_hedged_in_the_endgame`，需要 `reqwest` 和 `tokio` 两个 feature。真实计时器、reqwest adapter 和 `RangeFault::StalledOnce`：第二个 chunk 的连接发了一半就停住，不断开。顺序由测试决定：测试逐个处理事件，先不处理策略 tick；停住的响应把 body 停在一个 gate 上，等第一个 chunk 完成再放行，再等停住的 runner 交的整块缓冲都被 planner 接受（480 KiB，最后一块留在它的缓冲里），然后才放开策略 tick。所以停顿一定在收尾阶段，之前也不会提前对冲。对冲由循环的状态证明：停住的 runner 还在途时就以 `Loser` 被停，而空闲超时结束的 runner 会是一次失败；服务器的请求记录里，最后一个 chunk 先被请求一次，再从已接受的位置（起点 + 480 KiB）被请求一次。任务完成，hash 正确。不卡墙钟时间，只留一个 120 s 的死锁看门狗。
    - 另有 `the_longest_silence_among_runners_that_qualify`（触发）和 `a_failure_after_the_last_byte_exhausts_nothing`（预算），见上。
  - 共享接收端的唤醒：`test_frames_taken_by_the_arbitration_keep_wake_ups_and_order`（数据聚合器，计数的 waker）：从外面取帧时跳过空帧、剩下的帧不动；聚合器发现通道空了返回 `Pending` 之后，新帧照常唤醒它；通道满时等着的生产者，在从外面取走一帧之后被唤醒、写入；帧的内容各不相同，验证顺序不乱。
  - 单元测试：`test_a_hedge_joins_the_chunk_of_its_original`（planner：分叉、活跃数、拒绝 resize 和切分、`hand_over`、`drop_partner`、`owner_bytes`、`mark_failed` 清掉 `hedge`）；`FailurePolicy` 的 `test_a_pair_is_charged_at_most_once`、`test_a_congestion_charged_for_a_pair_exhausts_its_range`、`test_a_side_failure_counts_only_towards_the_consecutive_limit`。
  - 已有测试：只有机械改动——`record_completions` 多了两个参数，测试里构造的 `RunnerTick` 和 `DownloadedChunk` 多了新字段。
  - 变异检查，每种都有测试失败（整个 bolt-load lib，all features）：
    - 平局判给对冲（不取原 runner 排着的帧）：`the_original_wins_a_tie`；
    - 排着的空帧也算原 runner 的帧（`take_first_data` 不跳过空帧）：`queued_empty_frames_do_not_win_a_tie` 和 `the_original_wins_a_tie`；
    - 先取静默最久的、再查“别的 chunk 能不能切”：`the_longest_silence_among_runners_that_qualify`；
    - 原 runner 的空帧也决定胜负：`an_empty_frame_decides_nothing` 和 `a_stalled_connection_is_hedged_in_the_endgame`；对冲的空帧决定胜负：`an_empty_frame_decides_nothing`；
    - 败方迟到的帧不丢掉（去掉 `on_data_frame` 里对 `Loser` 的检查）：`the_hedge_never_moves_the_no_progress_deadline`；
    - 败方的槽位从不回收：`the_first_frame_decides_the_pair`、`a_loser_ends_without_a_trace`、`a_loser_ends_whatever_became_of_the_chunk` 和 `a_reused_id_is_a_new_runner`；分出胜负时就回收（确认终止时再回收一次）：前三个，以及 `a_connecting_original_loses_to_the_first_frame` 和 `stopping_runners_are_not_in_flight`；
    - 这一对按最后一方自己的 `charged` 算（不看 `first`）：`a_pair_is_charged_at_most_once`、`the_budget_of_a_pair_comes_first` 和 `the_first_failure_outlives_the_survivors_frame`；
    - 幸存方交来第一帧时清掉 `first`：`the_first_failure_outlives_the_survivors_frame` 和 `test_a_hedge_joins_the_chunk_of_its_original`；
    - writer 是否满过在 `poll_completions` 之后才读：`a_full_writer_restarts_every_silence`；
    - Pending 的 chunk 用 `RunnerRates` 的 `Some(0.0)`：`a_connecting_chunk_is_judged_by_the_per_runner_speed`；
    - 最后一方失败时把它自己算在途（在途数加 1）：`the_last_side_is_not_in_flight` 和 `stopping_runners_are_not_in_flight`；
    - 正在停止的 runner 算在途：`stopping_runners_are_not_in_flight`；
    - T13 评判一对中的一方：`the_survivor_of_an_undecided_pair_is_not_judged`；
    - 扩张切对冲过的 chunk（`largest_running_chunk` 不跳过）：`a_hedged_chunk_is_neither_restarted_nor_split` 和 `test_a_hedge_joins_the_chunk_of_its_original`；
    - 发起对冲时推迟无进展截止时间：`a_runner_silent_for_two_seconds_is_hedged` 和性质测试 `splits_stay_within_the_limit_and_cover_the_file`；
    - `Legacy` 也对冲（没有 `RunnerRates` 时用空的速率表）：`legacy_never_hedges` 和性质测试；
    - 静默的边界 `>=` 改成 `>`：10 个触发、排除和计时的测试，以及性质测试。

### 14.17 基准测试（PR-7，C13）

- **做法。**
  - `crates/bolt-load/benches/growth_strategy.rs`，`harness = false`，需要 `unstable-strategy`、`tracing` 和 `tokio` 三个 feature（`required-features`）。只有带 `--bench` 时才跑，`cargo test --benches` 什么都不做。`GROWTH_RUNS`（默认 10）、`GROWTH_SCENARIOS`、`GROWTH_STRATEGIES` 选次数、场景和策略；`GROWTH_TRACE` 打印下载循环和策略的 trace 事件，诊断用。
  - 服务器是 `bolt-load-tests` 的进程内 `SimpleTestAdapter`，不是 HTTP 服务器：bench 命令不带 `reqwest` feature。新加的服务器模式：
    - `speed_burst(window)`：限速可以突发多长时间的流量，默认 1 s（原来的行为）。限速器等的是计时器，Windows 上约 15.6 ms 一跳；突发量小于一跳的流量时，限速就被压成“每跳一个突发”。第一版用 64 KiB 的固定突发，16 MiB/s 的总限速实际只有约 4.2 MiB/s（约等于 64 KiB ÷ 15 ms，这是推断），8 条连接和 4 条一样快。现在 bench 用 62 ms：限速从第一秒起就有效，又比计时器的一跳长得多。
    - `stall_once_at(offset)`：第一个读到这个绝对位置的流停在那里，不再发送，也不关闭；之后的流照常通过。终点在它之前、或起点在它之后的流不算“读到”。`stalled_at()` 记下停住的时刻。
    - `paced_region(range, rate)`：落在这个区间里的字节，每个流按这个速率发送，叠加在其它限速之上。
    - 计数：`peak_streams()`（同时打开的 body 流的峰值，取每个流自己加 1 之后的计数，不另读一次，所以不会因为别的流恰好关闭而少算）、`congested()`（因为 `max_concurrent_streams` 回的 429 次数）、`served_bytes()`（所有流发出的字节数）、`streams()`（每个流的创建时刻和请求的区间）。
    - 这些都有单元测试（`bolt-load-tests`）：`test_stall_once_stops_only_the_first_stream_to_reach_it`（终点在停顿位置之前、起点在之后的流不停；第一个读到的流正好停在那里；之后的流、共享状态的 clone 都通过；`clone_reset_count` 之后重新计）、`test_counters_of_streams_bytes_and_refusals`、`test_paced_region`、`test_counter_guard_counts_its_own_increment`。
  - 每次运行记录：
    - 完成时间：从调用 `spawn` 之前到任务结束。之后核对文件内容、删掉文件，这两步不计时。
    - 峰值连接数、429 次数：服务器的计数。
    - 重复字节：服务器发出的字节减去文件大小。它包括所有被丢掉的字节：败方读到的、切分时原 runner 已经读过新上限的、停住的 runner 缓冲里的。所以不对冲的场景也不为 0。它不区分来源，量不出单次对冲的代价。
    - 对冲次数、胜负、取消延迟、低速重启次数：crate 已有的 tracing 事件（`hedging runner`、`wins its pair over`、`beaten runner … ended`、`stopped for being slow`）。bench 装一个只收这几个模块的 tracing layer（`Targets` 过滤），按消息文本数数、记时刻。取消延迟是分出胜负的事件到败方 `beaten runner … ended` 的时间。没有加新的计数器，crate 也没有改。
    - 败方的迟到帧：manager 丢掉败方帧时的 `dropping a frame of beaten runner` 事件数。停败方时还排在它通道里的帧随接收端一起丢掉，没有事件，不在这个数里。
    - 停顿等待：服务器记下停住的时刻，到之后第一个请求覆盖停顿位置的流被创建，就是这个区间被重新请求的时间（对冲或者空闲超时之后的重试）。
  - 探针的自检。按文本读事件，消息一改名就会读成 0，所以：
    - 启动时检查 crate 源码（`include_str!`）里还有它读的每一条消息，再用一组样本消息检查解析（三种胜负消息、迟到帧、低速重启、配不上对的胜负和败方结束）。
    - 停顿场景的每次运行都必须真的停住、停顿位置也必须被重新请求，否则这次运行直接失败。
    - s4 和 s4a 在 `Doubling` 下必须有对冲：对冲次数至少 1，每次对冲都分出胜负，每个败方都有结束事件，否则失败。
    - 配不上对的胜负和败方结束记为 `unmatched`，在表里报出来。一次运行可能没有、也可能有好几个样本的指标（取消延迟、停顿等待）带样本数 `n`；没有样本时写 `n/a`，不写 0。
  - 两种策略交替运行（Legacy、Doubling、Legacy……），各 10 次，让机器上的漂移两边都受。表里是中位数 / p90（最近秩）。
- **场景和大小。** 每次运行几秒到几十秒；文件大到扩张和收尾都起作用。`min_chunk_size` 是 1 MiB（`DEFAULT_MIN_CHUNK_SIZE`），runner 的空闲超时是默认的 30 s。
  - s1：每条连接 1 MiB/s，总带宽 16 MiB/s，128 MiB。
    - `s1_max32`：`max_connections = 32`，总带宽才是扩张的上限：16 条连接正好用满。`max_connections` 取默认的 8 时，8 条连接只有 8 MiB/s，总带宽用不上。
    - `s1_max8`：同样的服务器，默认上限 8。
    - 大小：`Doubling` 在 1.5、4.5、7.5、10.5 s 翻倍，10.5 s 到 16 条，这时已经下了约 45 MiB。128 MiB 让它在 16 条连接上再跑几秒，也让下一次翻倍（16 到 32）发生。
  - s2 `s2_unlimited`：不限速，adapter 每项 64 KiB，4 GiB。1 GiB 时 1.4 s 就下完，第一次扩张（1.5 s）都等不到；4 GiB 约 3–6 s，能经过一两次扩张。
  - s3 `s3_429_beyond_4`：每条连接 2 MiB/s，第 5 条同时在途的连接回 429（没有 `Retry-After`），96 MiB。48 MiB 时 `Doubling` 在第 3 次翻倍（7.5 s，4 到 8）之前就快下完了，429 根本碰不到；96 MiB 时 7.5 s 还剩一半多。
  - s4 `s4_endgame_stall`：每条连接 2 MiB/s，64 MiB，第一个读到 63 MiB 的流停住。拥有文件末尾的 runner 在大家都快下完时停住，剩 1 MiB，正是 §9.6 的收尾。
  - M1 的两种情况：
    - s4a `s4a_stall_while_splittable`（M1 的第一种）：32 MiB，前 16 MiB 每条连接 512 KiB/s，后 16 MiB 4 MiB/s，第一个读到 31 MiB 的流停住。快的那一半先读到末尾、停住，剩 1 MiB；这时慢的 chunk 还各剩几 MiB，按 C11 的门槛（`max(512 KiB/s × 2 s, 1 MiB)` = 1 MiB）都可切。
    - s4b `s4b_two_runners`（M1 的第二种）：`max_connections = 2`，每条连接 2 MiB/s，32 MiB，第一个读到 28 MiB 的流停住。拥有后一半的 runner 停住时还剩 4 MiB（≥ `2 × min_chunk_size`），任务里只有两个 runner。
  - s4c `s4c_stall_midway`（额外加的）：和 s4 一样，但停在 40 MiB，剩下的远多于 `2 × min_chunk_size`，任务里有 4 个 runner。这是 T13 低速重启本该管的情况，用来校准 `slow_window`。
  - 无停顿的对照：`s4_nostall`、`s4a_nostall`、`s4b_nostall` 是去掉停顿的同一设置（s4c 的对照就是 `s4_nostall`）。
- **重排说明（2026-11-14）。** 下面的数字是在 PR-6a 的并发控制（§8.3）之前测得的：那时降级按失败率，恢复每 15 s 加 1；现在恢复要经过验证的 step，并按退避拉开，s3 这类遇到 429 的场景受影响最大。重排时没有重跑，这些表要在 C41 里重测，在那之前只作参考。
- **结果。** 一次完整的运行，每组 10 次，中位数 / p90。时间单位 s，取消延迟 ms；“停顿等待”见上；`n/a` 是没有这一项的样本。所有运行的 `unmatched` 都是 0，败方的迟到帧都是 0，低速重启都是 0 次。

  | 场景 | 策略 | 完成时间 | 峰值连接 | 429 | 对冲（分出胜负） | 重复 KiB | 取消延迟 | 停顿等待 |
  | --- | --- | --- | --- | --- | --- | --- | --- | --- |
  | s1_max32 | Legacy | 33.90 / 38.39 | 6.5 / 7 | 0 / 0 | 0 | 32 / 48 | n/a | n/a |
  | s1_max32 | Doubling | 15.95 / 16.03 | 32 / 32 | 0 / 0 | 0 | 176 / 224 | n/a | n/a |
  | s1_max8 | Legacy | 33.89 / 35.40 | 6 / 7 | 0 / 0 | 0 | 32 / 48 | n/a | n/a |
  | s1_max8 | Doubling | 21.09 / 21.11 | 8 / 8 | 0 / 0 | 0 | 32 / 32 | n/a | n/a |
  | s2 | Legacy | 4.53 / 5.41 | 1.5 / 2 | 0 / 0 | 0 | 8 / 48 | n/a | n/a |
  | s2 | Doubling | 4.50 / 5.72 | 1.0 / 2 | 0 / 0 | 0 | 0 / 32 | n/a | n/a |
  | s3 | Legacy | 16.10 / 16.12 | 4 / 4 | 1 / 1 | 0 | 17 / 26 | n/a | n/a |
  | s3 | Doubling | 15.57 / 15.63 | 4 / 4 | 4 / 4 | 0 | 16 / 32 | n/a | n/a |
  | s4 | Legacy | 42.41 / 42.72 | 5 / 5 | 0 / 0 | 0 | 16 / 20 | n/a | 30.67 / 30.94，n=10 |
  | s4 | Doubling | 12.56 / 12.58 | 8 / 8 | 0 / 0 | 1（1） | 42 / 52 | 0.04 / 0.09，n=10 | 3.56 / 3.58，n=10 |
  | s4_nostall | Legacy | 12.08 / 12.11 | 5 / 5 | 0 / 0 | 0 | 12 / 20 | n/a | n/a |
  | s4_nostall | Doubling | 9.08 / 9.11 | 8 / 8 | 0 / 0 | 0 | 24 / 48 | n/a | n/a |
  | s4a | Legacy | 65.95 / 66.03 | 2 / 2 | 0 / 0 | 0 | 0 / 0 | n/a | 30.88 / 30.98，n=10 |
  | s4a | Doubling | 18.32 / 18.33 | 4 / 4 | 0 / 0 | 1（1） | 32 / 32 | 0.03 / 0.03，n=10 | 12.99 / 13.00，n=10 |
  | s4a_nostall | Legacy | 35.29 / 35.31 | 2 / 2 | 0 / 0 | 0 | 0 / 0 | n/a | n/a |
  | s4a_nostall | Doubling | 11.78 / 11.80 | 6 / 6 | 0 / 0 | 0 | 0 / 0 | n/a | n/a |
  | s4b | Legacy | 37.77 / 38.04 | 2 / 2 | 0 / 0 | 0 | 0 / 0 | n/a | 30.62 / 30.90，n=10 |
  | s4b | Doubling | 38.49 / 38.60 | 2 / 2 | 0 / 0 | 0 | 0 / 0 | n/a | 30.67 / 30.87，n=10 |
  | s4b_nostall | Legacy | 8.72 / 8.73 | 2 / 2 | 0 / 0 | 0 | 0 / 0 | n/a | n/a |
  | s4b_nostall | Doubling | 8.72 / 8.73 | 2 / 2 | 0 / 0 | 0 | 0 / 0 | n/a | n/a |
  | s4c | Legacy | 35.62 / 35.85 | 5 / 5 | 0 / 0 | 0 | 0 / 8 | n/a | 30.62 / 30.83，n=10 |
  | s4c | Doubling | 38.72 / 38.82 | 4 / 4 | 0 / 0 | 0 | 20 / 20 | n/a | 30.76 / 30.93，n=10 |

  - 稳定性：限速的场景很稳，`Doubling` 10 次的最小–最大一般在 0.1 s 以内（s4 是 10.99–12.58，差的是对冲落在哪个 tick）。s1 的 `Legacy` 在 32.0–39.2 s 之间，因为它扩张靠的是速度的抖动（见下）。s2 不稳：两种策略都是 2.9–6.4 s。`Legacy` 前 5 轮用了 2 条连接、后 5 轮 1 条；`Doubling` 前 4 轮 2 条、后 6 轮 1 条。1 条的运行一般更快。原因用 `GROWTH_TRACE` 看了一次 `Doubling`（4 次运行）：只用 1 条的那次，1.5 s 的 tick 量到 1.81 GiB/s，要扩张，但 runner 只剩 1.28 GiB，少于按它自己的速率算的门槛（速率 × 2 s = 3.64 GiB），切分被拒绝（`skip growing`），之后它在 2.8 s 下完；用了 2 条的那 3 次，1.5 s 时的速率只有 0.98–1.01 GiB/s，剩下的多于门槛，就切了。也就是说，第一个 1.5 s 跑得快不快（这台机器上在 1.0–1.8 GiB/s 之间变化，原因没查）决定了用 1 条还是 2 条。`Legacy` 的门槛也是速度 × 2 s，估计是同样的原因，但没有单独 trace 确认。另外，runner 下完之后任务还要约 1.3 s 才结束（文件收尾），两种连接数都一样。
  - 机器：Windows 11，32 个逻辑核，64 GiB 内存，`nightly-2026-11-06`（rustc 249c1e4e3），release。
- **分析。**
  - s1 `Legacy` 扩张得少。`dynamic_strategy.rs` 里的两个条件（`threashold1`、`threashold2` 都是 1 MiB/s，`prev` 是上一个 tick 的总速度）：
    - QuickStart：`(speed − 2 × prev).abs() > 1 MiB/s`，并且 `2 × current ≤ max` 时 `SplitAllTask`；不满足就转入 Normal。
    - Normal：`(speed − (prev + per_runner_speed)).abs() > 1 MiB/s`，并且 `current < max` 时 `SplitGivenTask`（剩余最多的 runner）；每个 tick 都把 `prev` 更新为这次的速度。
    - 速度稳定时，Normal 的差（带符号）约等于“减去一个 runner 的速度”。每条连接 1 MiB/s 时，绝对值就落在 1 MiB/s 的门槛上，过不过门槛取决于抖动：量出的每 runner 速度比 1 MiB/s 高几个百分点、或者总速度比上个 tick 掉了一点时才过。总速度大幅下降（runner 下完）也会让绝对值变大，所以也会触发切分。
    - 用 `GROWTH_TRACE` 看了一次（`s1_max8`）：1.5 s 的 QuickStart 翻倍到 2（`|1.02 − 0|` MiB/s 过了门槛）；3.0 s 时 `|1.85 − 2 × 1.02|` 不到 1 MiB/s，转入 Normal。之后只在 6.1、15.1、18.1、24.2 s 各切一个，30.2 s 和 33.2 s 是 runner 下完、总速度下降时切的；其余 tick 的绝对值都没过 1 MiB/s。峰值 6，平均 3.8 MiB/s。
    - `Doubling` 不看绝对门槛，只看翻倍前后的增益比例。
  - s1_max32 `Doubling` 开到了上限 32。用 `GROWTH_TRACE` 看了一次：
    - 1.5、4.5、7.6、10.6 s 依次翻到 2、4、8、16；13.6 s 判 8 到 16 的增益（16.1 对 8.0 MiB/s，过了 10%），又翻到 32。
    - 15.1 s 的 tick 时活跃 runner 是 32，等于上限。`runners_to_add` 在“活跃数已达上限”这一步就返回了，没有走到判断增益那一步，所以 16 到 32 这次翻倍没有被判断，也没有停滞。
    - 32 个 runner 在 15.2–15.9 s 之间全部下完，之后没有 tick，所以这次运行里没有补位。
    - 控制器在上限处的实际行为（C13 时的代码，这次运行没走到；C28 改了，见 §14.18）：活跃数降到 32 以下之后，那次翻倍等着判断，但活跃数少于翻倍刚完成时的 32，按 §14.14 不下结论、判断作废，然后按 `min(活跃数 × 2, 32)` 补位。也就是说，下载还长的话，它会一直把连接数补回 32，不会因为“16 到 32 没有增益”而停下（§14.14 已知限制里的盲区）。
    - 所以这里的 32 是 `max_connections`，不是控制器算出来的；“比用满带宽所需多一倍”只是这个设置下的观察，不是控制器的保证。
  - s3：`Doubling` 从 4 翻到 8 时，4 个新连接都拿到 429（4 次），之后连接数一直是 4；`Legacy` 只碰到 1 次。连接数都没超过 4，完成时间 `Doubling` 快 0.5 s。
  - s4（T27）：`Doubling` 的停顿等待是 2.04–3.59 s，在 `hedge_idle` 2 s 加最多一个 tick（1.5 s）的范围里。完成时间比无停顿的对照慢 3.5 s；`Legacy` 要等 30 s 的空闲超时，慢 30.3 s。每次都只有一次对冲、都分出了胜负；败方都是停住的原 runner，迟到帧都是 0；取消延迟 0.03–0.21 ms（停住的 runner 在读流，停止开关立即生效）。
  - 重复字节：s4 `Doubling` 是 42 / 52 KiB（最小–最大 32–56），对照是 24 / 48 KiB；s4a `Doubling` 是 32 / 32 KiB，对照是 0 / 0 KiB（有一次 20）。两组运行是独立的，这些差只是观察到的总量之差，不能当成单次对冲的上限。
  - s4a（M1 的第一种）：`Doubling` 的停顿等待是 13 s：停住之后，慢的 chunk 一直可切，对冲不发起；同时 `Doubling` 因为停住的 runner 这个 tick 没有数据（检查 5）不再扩张，峰值停在 4（对照是 6）。慢的 chunk 降到门槛以下之后才对冲。完成时间比对照慢 6.5 s（+56%）。`Legacy` 在这个场景只开 2 条连接（每条 512 KiB/s，速度的变化过不了 1 MiB/s 的门槛），再加 30 s 空闲超时。
  - s4b（M1 的第二种）：两种策略都等满 30 s 的空闲超时，`Doubling` 比对照慢 29.8 s（4.4 倍）。
  - s4c：T13 一次都没有触发，两种策略都等满空闲超时，`Doubling` 比对照慢 29.6 s。用 `GROWTH_TRACE` 看了一次：4 个 runner 在 0、1.5、4.5、4.5 s 起，停住的那个剩 11.7 MiB（40 MiB 到 51.75 MiB）；别的 3 个在 10.6 s 下完，这时最早的 runner 才刚满 10.5 s 的窗口，之后有完整窗口的 runner 不到 3 个，T13 再也判不了；停住的 runner 在 34.8 s 空闲超时，区间退回、重下。停住的 runner 让 `Doubling` 也不扩张（峰值 4），所以没有更多的 runner 来凑够 3 个。
  - 收尾对冲在不停顿的场景里一次都没有发起（s1、s2、s3 和三个对照的对冲次数都是 0）：`hedge_idle` 2 s 在这些速度下没有误判。
- **G7 结论。** 这里是按原判据的结论：G7 要求在单连接限速、高速、对 429 敏感三个场景里，`Doubling` 完成时间不比 `Legacy` 差，连接数也不比 `Legacy` 多。2026-11-08 维护者改了 G7（§2），按新判据的结论见 §14.18：四个 G7 场景都达标。
  - s1 单连接限速：**未达标**。时间达标（s1_max32 15.95 对 33.90 s，s1_max8 21.09 对 33.89 s），但连接数更多：s1_max32 是 32 对 6–7，s1_max8 是 8 对 6–7。`Legacy` 连接少，是因为它在这个速度下扩张不起来（见分析）。`Doubling` 在 max32 时一直开到上限，16 到 32 那次翻倍在上限处没有被判断。
  - s2 高速：**无法判定**。连接数中位数 1.5 对 1.0，完成时间中位数 4.53 对 4.50 s，p90 5.41 对 5.72 s。同一轮里 `Doubling` 减 `Legacy` 依次是 −0.17、+0.85、−0.70、−0.02、−1.58、+0.74、−0.71、−0.05、+1.32、−0.13 s，中位数 −0.09 s，正负都有，散布远大于差。没有约定的容差，“不比 `Legacy` 差”就无从成立，也无从否定。
  - s3 对 429 敏感：**达标**。时间 15.57 对 16.10 s，连接数都是 4。`Doubling` 的 429 次数是 4 对 1（不在 G7 的判据里）。
  - （C13 当时的结论，已过时：）所以 tasks.md 里 PR-7b（C14，切换默认值）的前提“G7 达标”当时不满足，留给维护者决定：接受 s1 的连接数，还是改 G7 的判据，或者先改 `Doubling`；s2 要先约定一个容差。维护者 2026-11-08 改了 G7，按新判据的结论见 §14.18。
- **Mutex 的开销（§14.16 的已知限制）。**
  - 现有的 `coordination` bench 是自成一体的模型：它自己重写了每帧的记账，不用 crate 里的 `DataAggregator`，C12 和 C27 跑的是同一份代码，比不出 C27 的变化，所以没有跑它。`runner_loop` 只有 runner 一侧。没有现成的 bench 走真实的接收路径。
  - 所以用 s2（不限速，真实的 `DataAggregator`、planner 和 writer，每帧 32 KiB）在 C12（`ee092fc`）和 C27（`4391ed0`）上各跑 `Legacy`，两边交替。C12 上用的是同一个 bench 文件，只关掉了启动时对对冲消息的源码检查（C12 没有这些消息）。10 轮，每轮每边 2 次，每边 20 次。连接数不同的运行快慢差得多（只用 1 条连接的一般更快，原因没查），所以按峰值连接数分开比：1 条连接（每边 11 次）C12 的中位数 / p90 是 3.82 / 5.20 s，C27 是 3.64 / 4.88 s；2 条连接（每边 9 次）C12 是 5.14 / 6.54 s，C27 是 5.18 / 6.77 s。20 次合起来的中位数是 5.06 对 4.96 s。差有正有负，都远小于两边各自 3.0–6.8 s 的散布，量不出来。
  - 单独量的无争用 `std::sync::Mutex` 加锁、解锁：约 8.5 ns（5 轮，每轮 5000 万次，8.51–8.58 ns）。这不是在 crate 里量的：`SharedReceiver` 在每次轮询时都加锁，返回 `Pending` 的轮询也加，所以“每帧一次锁”只是估计。按这个估计，s2 约 900 MiB/s 时每秒约 29 000 帧，合计每秒约 0.25 ms，即约 0.025% 的时间，远在 s2 的噪声以下。
- **校准建议（只建议，不改默认值）。**
  - `hedge_idle`（2 s）：保留。收尾停顿的代价是 2.0–3.6 s（含一个 tick 的对齐），没有误判。降低它能省下的最多是 2 s 里的一部分，但这里没有数据说明更短的值不会在慢链接上误判（§9.6 提到慢于 16 KiB/s 的 runner 也会显得静默），所以不建议改。
  - `slow_window`（10 s）：建议缩短，或者给“完全静默”的 runner 另开一条路。s4c 里一个剩 11.7 MiB 的连接停了 30 s，T13 一直没机会判：窗口要 10.5 s，而 `Doubling` 的 runner 大多是 4.5 s、7.5 s 才切出来的，停住之后扩张又停了，凑不齐 3 个有完整窗口的 runner。候选：`slow_window` 改为 3 个 tick（4.5 s）；或者对一个 tick 都没有交数据、静默已满 `hedge_idle` 的 runner，不要求完整窗口。哪一种都要另外补测试，也要重新看 §14.15 里“一帧的让步”在短窗口下够不够。
  - `slow_ratio`（0.25）：没有数据。这里的停顿都是“完全不发”，没有“慢但还在发”的连接；要校准它，需要再加一个场景（比如一条连接限在别人的 10%–30%）。
- **待决定的事项。**
  1. G7 在 s1 不达标（连接数），s2 无法判定，见上。已决定（2026-11-08）：改 G7，见 §2 和 §14.18。
  2. 已决定（2026-11-08）：给 T13 加一条重启静默 runner 的路径，`Doubling` 的检查 5 不再因为静默满 `hedge_idle` 的 runner 停止扩张，另开 PR（C29，§14.19）；对冲仍只在收尾。原来的说明：M1 的两种情况代价都很大：第一种 +56%，并且停住的 runner 让 `Doubling` 停止扩张；第二种 +29.8 s（4.4 倍），等于空闲超时。§9.6 写过放宽的方向（去掉条件 1 里“其它 chunk”那一条，或者去掉候选的剩余量上限）。另一个和第一种相关的改法：`Doubling` 的检查 5 不再因为一个已经静默满 `hedge_idle` 的 runner 跳过扩张。
  3. `slow_window` 的缩短，见校准建议。已决定（2026-11-08）：`slow_window` 和 `slow_ratio` 不变，改用第 2 项的静默路径（§14.19）。
  4. 已决定（2026-11-08）：修上限处的判断，见 §14.18。原来的说明：在上限处的翻倍不被判断：s1_max32 里 16 到 32 的翻倍没有增益，但活跃数等于上限时控制器不判断；降到上限以下时活跃数又少于翻倍刚完成时的数，判断作废、按两倍补位。长一些的下载里，连接数会一直被补回上限。可以接受（连接数不超过 `max_connections`），也可以改成在上限处也判断，或者停滞之后收回多开的那一步。
- **已知限制。**
  - 服务器在进程内，没有网络、TCP 慢启动和连接建立的延迟；HTTP 的情况（`reqwest`、`ureq2`）没有量。
  - 一台机器、一个时间段；s2 受写盘影响大，结果不稳。
  - 停顿是“完全不发也不关”。慢但还在发的连接、反复短暂停顿的连接都没有量（`slow_ratio` 因此没有校准）。
  - 取消延迟只量到了败方是停住的原 runner 的情况（每次都是）；对冲 runner 输掉、败方还在读流时的延迟没有出现。
  - 重复字节按服务器发出的字节算，不区分来源（见做法）。
  - 探针按消息文本读事件；启动时的源码检查只防得住改名，防不住“消息还在、但不再在这个时刻发出”。s4、s4a 的必有对冲检查能发现对冲事件的这种变化，低速重启事件没有这样的场景（这次所有场景都是 0 次）。
- **验证。**
  - `cargo bench -p bolt-load --features unstable-strategy --bench growth_strategy` 跑完全部 11 个场景，每组 10 次，每次都核对了下载的内容，探针的自检都通过。
  - `bolt-load-tests` 的改动只加了新的选项和计数，原有选项的行为不变（`speed_burst` 不设时还是 1 s 的突发）；新功能各有单元测试（见做法）。

### 14.18 上限处的翻倍（PR-7c，C28）

- **决定。** 2026-11-08 维护者对 §14.17 待决定事项 1、4 的决定：改 G7（§2），修 `Doubling` 在上限处的过冲。“没判断的翻倍不补位”只针对翻到上限的翻倍，上限以下照旧（第一轮评审指出：全都不补位的话，一个 chunk 正常下完就会让上限以下的扩张永远停下）。
- **做法。**
  - 做出翻倍时，`Proposal` 记下那个 tick 的上限（`max_concurrency`）；`on_grown` 记下完整的翻倍时，算出它有没有翻到这个上限（`Growth::at_limit`，翻倍后的 runner 数 ≥ 上限）。用记下的上限，不用判断时的上限：并发控制可能在这之间改它。
  - `runners_to_add` 的顺序：
    1. 已经停滞：不扩张。
    2. 距上次扩张不到 2 s（`GROW_INTERVAL`）：不扩张。
    3. 没有活跃 runner：不扩张。
    4. 拥塞、有 runner 不交数据、还没有速度、速度达到限速的 85%：不扩张。
    5. 有待判断的完整翻倍：
       - 活跃数少于翻倍后的数，并且它翻到了上限：不判断，也不扩张，翻倍继续等着判断，所以下完的 runner 不补位；
       - 活跃数少于翻倍后的数，在上限以下：这个 tick 量不出它，不下结论，丢掉它，接着往下走（C11 的做法）；
       - 否则判断：速度低于翻倍前的 1.10 倍就停滞。
    6. 活跃数达到上限：不扩张。
    7. 都通过时，`target = min(活跃数 × 2, 上限)`。
  - 和 C11 比：上限检查从第 3 步挪到判断之后，所以到了上限也判断；翻到上限的翻倍少了 runner 时由“判断作废、照常扩张”改为“继续等、不扩张”。上限以下的翻倍不变。
  - 判断时打一条 debug 日志（有增益、停滞两种），`GROWTH_TRACE` 能看到。
- **现在哪些扩张会被判断。**
  - 完整的翻倍：在之后第一个通过第 2、4 步、并且活跃数不少于翻倍后的数的 tick 判断，到了上限也判断，并发控制把上限降到翻倍后的数以下时也判断。
  - 翻到上限、还没判断时活跃数降下来（runner 下完、失败、对冲败方让出）：不判断，也不扩张，直到活跃数回来。被 T13 停掉时例外，见 §14.19（C29）。失败或被停掉的 runner 的区间冷却结束、被重新领取后，活跃数就回来了；下完的 runner 不会回来，所以这样的下载此后不再扩张，连接数随 runner 下完逐渐减少。
  - 上限以下的完整翻倍、判断前活跃数降下来：不下结论，丢掉，照常扩张，和 C11 一样。下完的 chunk 没有留下可领取的区间，只有切分能把连接数补回来，这一步不能挡。
  - 被上限或拒绝的切分截断的扩张：仍然不判断（§14.14，`on_grown` 没改），之后照样补位。§14.14 的盲区只剩这一种：比如上限 6 时从 4 扩到 6，或者上限一格一格地恢复。
  - 判断过、有增益的翻倍：之后在上限处有 runner 下完，照样补位。
- **与 §9.2 的偏差。** §9.2 已经按上面改写。判断仍要等 2 s 的间隔（第 2 步），和别的翻倍一样：翻倍之后的速度窗口是之后第一个通过检查的 tick。
- **已知限制。**
  - 翻倍是先开连接再判断，所以峰值仍可能是用满带宽所需的两倍（s1_max32 的 16 到 32）。新的 G7 正好允许两倍。
  - 判断要等 2 s：翻到上限之后不到 2 s 下载就结束，这次翻倍就不会被判断（见下面的 s1_max32）。
  - 翻到上限、待判断时活跃数回不到翻倍后的数，就再也不扩张，上限恢复了也一样。这是“到了上限不补位”的代价。2026-11-08 维护者决定（C29，§14.19）收窄了这一条：低速或静默重启停掉 runner 时，待判断的翻倍直接丢掉，重启的区间可以再切；runner 正常下完、失败、对冲败方让出时仍按这一条等着。
  - 被截断的扩张仍不判断，盲区还在（见上）。
- **结果。** s1_max32、s1_max8、s2、s3 各 10 次，两种策略交替，中位数 / p90（最小–最大）。G7 一栏是 bench 打印的：逐轮差（`Doubling` − `Legacy`）的中位数、它占 `Legacy` 中位数的百分比，以及 `Doubling` 所有运行里的最大峰值对“两倍所需连接数”。

  | 场景 | 策略 | 完成时间 s | 峰值连接 | 429 | G7 |
  | --- | --- | --- | --- | --- | --- |
  | s1_max32 | Legacy | 34.45 / 38.37（32.35–42.90） | 6.0 / 7.0 | 0 / 0 | |
  | s1_max32 | Doubling | 15.90 / 15.95（15.81–16.00） | 32 / 32 | 0 / 0 | 逐轮差中位数 −18.53 s（−53.8%）；峰值 32 ≤ 2 × 16：**达标** |
  | s1_max8 | Legacy | 34.26 / 37.27（32.69–39.87） | 6.0 / 6.0 | 0 / 0 | |
  | s1_max8 | Doubling | 21.08 / 21.09（21.05–21.09） | 8 / 8 | 0 / 0 | −13.19 s（−38.5%）；峰值 8 ≤ 2 × 16：**达标** |
  | s2 | Legacy | 3.39 / 4.43（2.60–4.68） | 1.0 / 1.0 | 0 / 0 | |
  | s2 | Doubling | 3.30 / 4.75（2.60–6.11） | 1.0 / 1.0 | 0 / 0 | −0.07 s（−2.1%），逐轮差 −0.97 到 +1.43 s；峰值 1 ≤ 2 × 1：**达标** |
  | s3 | Legacy | 16.07 / 16.08（16.04–16.08） | 4.0 / 4.0 | 1 / 1 | |
  | s3 | Doubling | 15.56 / 15.58（15.54–15.59） | 4 / 4 | 4 / 4 | −0.51 s（−3.2%）；峰值 4 ≤ 2 × 4：**达标** |

  - 所需连接数：s1 是 16（16 MiB/s ÷ 1 MiB/s，构造出来的），s3 是 4（第 5 条并发连接回 429，构造出来的）；s2 取 1，是这台机器上观察到的参照值，不是构造出来的：服务器不限速，§14.17 里用了 2 条连接的运行并不比 1 条的快，这次 20 次运行都只用了 1 条。
  - 量出来的结果和 §14.17 比：`Doubling` 的时间和峰值都没变（s1_max32 15.90 对 15.95 s，峰值都是 32；s1_max8 21.08 对 21.09 s；s3 15.56 对 15.57 s）；`Legacy` 的 s1 在它靠抖动扩张的散布之内；s2 两种策略这次都只用了 1 条连接，比 §14.17 快，散布仍大。
  - 策略内部的状态不一定没变，逐个场景看（各用 `GROWTH_TRACE` 看了一次 `Doubling`）：
    - s1_max32：13.6 s 翻到 32（上限），15.1 s 的 tick 离它不到 2 s，不判断；32 个 runner 在 15.9 s 前全部下完，之后没有 tick。所以这次翻倍没有被判断，状态和 C13 一样，峰值仍是 32。下载更长的话，16.6 s 的 tick 会判断它：15.1 s 时总速度是 16.5 MiB/s，不到翻倍前 16.0 MiB/s 的 1.10 倍（17.6），照这个速度就是停滞，之后下完的 runner 不补。
    - s1_max8：7.6 s 从 4 翻到 8（上限），10.6 s 在上限处判断，有增益（7.9 对 4.0 MiB/s）。C13 的代码在上限处不判断，这次翻倍一直待判断。之后 8 个 runner 在 21.0 s 一起下完，中间没有 runner 下完，所以两种代码都不会补位，量出来的一样，内部状态不同。
    - s3：7.5 s 从 4 翻到 8（上限 8），loop 报回加了 4 个，`on_grown` 记下一次翻到上限的完整翻倍；这 4 个新 runner 随即都拿到 429，并发控制把上限降到 4。之后活跃数是 4，少于 8：这次翻倍一直等着，不判断；C13 的代码会丢掉它，按 `min(4 × 2, 4)` 也加不了 runner。两种代码都不扩张，量出来的一样，内部状态不同。
    - s2：两种策略都没有扩张（第一个 1.5 s 跑得快，剩下的不到 2 s 的量，切分被拒绝，§14.17），没有翻倍。
- **G7 结论（新判据）。** 四个 G7 场景都达标。按 tasks.md，PR-7b（切换默认值）还依赖 PR-7c 和 PR-7d（C29，重启静默 runner，§14.19），要等它们合入。
- **验证。**
  - `doubling_strategy.rs` 的单元测试，显式传入 `now`：
    - `test_a_doubling_to_the_limit_is_judged_there`：上限 8，从 4 翻到 8 没有增益，在上限处停滞；之后上限升到 16、速度翻倍，也不再扩张；有 runner 下完也不补。有增益时上限拦住，有 runner 下完之后补 1 个。上限被降到 6（活跃数 8 高于上限）时同样判断，没有增益就停滞。
    - `test_a_doubling_to_the_limit_waits_for_its_runners`：上限 8，从 4 翻到 8 之后有 runner 下完（6、7 个），不判断，也不补位；回到 8 个时判断：没有增益就停滞，之后少一个也不补；有增益时之后少一个就补 1 个。
    - `test_a_doubling_below_the_limit_that_loses_runners_is_not_judged`：上限 16，从 4 翻到 8 之后只剩 6 个，不下结论，照常扩张到 12；下一次翻倍照常判断，没有增益就停滞（C11 的行为，原来的 `test_a_tick_with_fewer_runners_does_not_judge_the_growth`）。
    - `test_the_limit_of_a_doubling_is_the_one_it_was_made_under`：在上限 16 下从 4 翻到 8，之后上限降到 8、剩 7 个：不算翻到上限，补 1 个；在上限 8 下翻到 8，之后上限升到 16、剩 7 个：算翻到上限，等着不扩张，回到 8 个时判断，有增益，翻到 16。
    - `test_a_doubling_reported_past_the_limit_reached_it`：上限 6 时要加 2 个，loop 报回加了 4 个（超过当时的上限），仍算翻到上限：剩 7 个时等着，回到 8 个时判断、有增益，按升高后的上限翻到 16。
    - `test_refills_after_a_truncated_growth_are_never_judged`：上限 6，从 4 扩到 6 是被截断的扩张，不判断；在上限处反复 6 → 5 → 6，速度不变，每次都补回 6（保留的盲区）。
  - 下载循环测试（`download_loop/tests/growth.rs`，经过真实的 `split`）：
    - `doubling_judges_the_doubling_at_the_limit`：上限 2，翻到 2 没有增益，在上限处停滞；一个 runner 下完之后，剩下的那个速度再高也不被切分补位。
    - `doubling_below_the_limit_grows_on_after_a_runner_completes`：上限 16，1、2、4、8 翻上来，判断 4 到 8 的 tick 之前，剩余最少的那个 chunk 下完、runner 停下，别的 chunk 还剩大量可切的活，没有可领取的区间；判断的 tick 不下结论，照常翻到 14。
    - `doubling_to_the_limit_does_not_refill_before_it_is_judged`：同样的过程，上限 8；判断之前一个 runner 下完，之后 10 个 tick 活跃数一直是 7，不补位。
  - 变异检查（整个 bolt-load lib，all features，每次之前删掉 `proptest-regressions`），每种都有测试失败：
    - 到了上限就不判断（上限检查放回判断之前）：`test_a_doubling_to_the_limit_is_judged_there` 和 `test_a_doubling_to_the_limit_waits_for_its_runners`；
    - 翻到上限的翻倍也照样补位（不等）：`test_a_doubling_to_the_limit_waits_for_its_runners`、`test_the_limit_of_a_doubling_is_the_one_it_was_made_under` 和 `doubling_to_the_limit_does_not_refill_before_it_is_judged`；
    - 上限以下的翻倍也等着不扩张：`test_a_doubling_below_the_limit_that_loses_runners_is_not_judged`、`test_the_limit_of_a_doubling_is_the_one_it_was_made_under` 和 `doubling_below_the_limit_grows_on_after_a_runner_completes`；
    - 判断时才读上限（`growth.runners >= 当前上限`）：`test_the_limit_of_a_doubling_is_the_one_it_was_made_under`；
    - `at_limit` 的 `>=` 改成 `>`：`test_a_doubling_to_the_limit_waits_for_its_runners`、`test_the_limit_of_a_doubling_is_the_one_it_was_made_under` 和 `doubling_to_the_limit_does_not_refill_before_it_is_judged`；
    - 整个改回 C11 的代码：`test_a_doubling_to_the_limit_is_judged_there`、`test_a_doubling_to_the_limit_waits_for_its_runners`、`test_the_limit_of_a_doubling_is_the_one_it_was_made_under`、`doubling_judges_the_doubling_at_the_limit` 和 `doubling_to_the_limit_does_not_refill_before_it_is_judged`。
    - 上限检查的 `>=` 改成 `>`：没有测试失败，因为这是等价变异。活跃数等于上限时，`target = min(活跃数 × 2, 上限)` 就是活跃数本身，要加的 runner 数是 0，`step` 不发出 `Grow`；留下的提议在下一个 `step` 开头被清掉。活跃数高于上限时两种写法都提前返回。
  - 性质测试 `splits_stay_within_the_limit_and_cover_the_file` 的修正：第一次变异检查时，它在没有变异的代码上也找到一个失败的序列（`max_connections = 4`、`Doubling`，两次切分之后一个 runner 剩不到 2 MiB 且静默满 2 s、别的 chunk 不到门槛，下一个 tick 发起了对冲），报“a slot leaked”。C13 的提交（`9bc7766`）上用同一个序列也失败。原因在测试：它只把拥有 chunk 的 runner 算作占着槽位，对冲 runner H 在分出胜负之前不拥有 chunk，被算成泄漏。对冲本身是对的（§9.6 的触发条件都满足）。现在把“没有 chunk、但在未分胜负的一对里或者正在停止”的 runner 也算作占着槽位，这个序列通过。另把这个序列写成确定的测试 `a_hedge_after_two_splits_runs_to_the_end`（`download_loop/tests/hedge.rs`），一直走到结束：对冲发起后槽位 4 个；原 runner 先交帧、胜出，H 被停、还占着槽位；H（停在 spawn 处的真实 runner）被轮询后报告停止，槽位回收到 3 个，不再“正在停止”、也不在一对里；原 runner 收满、结束，对冲记录清掉，槽位 2 个；两个 ID 都能再分给新的 runner。
  - bench：`cargo bench -p bolt-load --features unstable-strategy --bench growth_strategy` 打印每个 G7 场景的逐轮差中位数、它占 `Legacy` 中位数的百分比、`Doubling` 的最大峰值对两倍所需连接数，以及结论。所需连接数写在场景上（`fills`）。

### 14.19 重启静默 runner（PR-7d，C29）

- **决定。** 2026-11-08 维护者对 §14.17 待决定事项 2、3 的决定：给 T13 加一条重启静默 runner 的路径，不需要 3 个完整窗口；`Doubling` 的检查 5 不再因为静默满 `hedge_idle` 的 runner 停止扩张。对冲仍只在收尾，`slow_window`、`slow_ratio` 不变。§9.6 的不变量和 M1–M5 都没有动。
- **做法。**
  - 活动的信号（`runner::ReadActivity`）：runner 和下载循环共享的一个小状态（`Arc<Mutex<…>>`：最近一次读的 `Instant`，和“正在推送”的标记；每次改动都在一次加锁里做完，循环要么看到改之前、要么看到改之后）。runner 在重置空闲计时器的地方记下时刻：读到非空的块（`on_item`）时，和推送完一个满缓冲、接着读同一个块时；开始推送一帧（`flush` 把一帧交给 `Emission`）时置上“正在推送”。推送结束（`on_emitted`）时：接着读同一个块的，在同一次加锁里记下时刻、清掉“正在推送”（`resume_reading`）；否则只清掉标记。分两次做的话，循环的 tick 可能正好落在两次之间，看到一个既不在推送、也没有新时刻的 runner，把推送了 4.5 s 以上的健康 runner 当成静默停掉（第四轮评审）。空块不记；流结束、空闲超时、出错时推出的最后一帧也不记（这些推送之后 runner 不再读）。`note_at` 取已记时刻和新时刻中较晚的。spawner 给每个 runner 一个，经 `RunnerChannels::reads` 交给 manager，manager 收下时记上 spawn 的时刻。runner 的其余行为不变。
  - `RunnerManager::silent_since(id, now)` 就是 runner 记下的最近一次读（没有读过就是 spawn）；正在推送的 runner 是 `now`。被接受的帧和 writer 恢复都不算：runner 读那些字节在前，被 writer 挡住的 runner 在推送。循环不在 tick 上记什么。对冲的静默仍只按帧算（`last_delivery`，writer 恢复时重新算，§9.6 不变）。
  - 重启放开上限处的翻倍（2026-11-08 维护者决定）：低速或静默路径停掉一个 runner 时，循环调 `StrategyControl::on_restarted`，`Doubling` 把待判断的翻倍不下结论地丢掉，翻到上限的和上限以下的都一样。翻到上限的翻倍原来要等活跃数回来（C28），s4c 那样别的 runner 都下完时，重启的区间被重新领取后只有一个 runner 下；现在它被重新领取后，扩张照常切它。上限以下的翻倍原本在活跃数不够时也会丢掉，但被停的 runner 在停下来之前还占着活跃数，低速重启时它还在交数据，这样的 tick 会用一个被放弃的 runner 判断翻倍，所以也一起丢掉。别的事件（runner 正常下完、失败、对冲败方让出）不放开，仍按 C28。
  - 静默的路径（`download_loop/restart.rs` 的 `restart_silent_runner`），只在 `Doubling` 下（`Legacy` 没有 `RunnerRates`），每个策略 tick 在低速路径没停 runner 时才判。候选要同时满足：
    1. 状态是 Running，剩余量 ≥ `2 × min_chunk_size`，不在停止中，不在未分胜负的一对里（`may_restart`，C12 的条件，没有改）；数据通道里没有循环还没取走的帧（`RunnerManager::has_queued_data`，只看不取）；
    2. `silent_since` 不晚于 `now − SILENT_RESTART_AFTER`（4.5 s）：这么久 runner 没有读到 body，也不在推送；
    3. 在 `now − 4.5 s` 或更早的最近一次采样里，它已经在运行同一次尝试（`RunnerRates::running_since`）；
    - 另外：不在拥塞期间，writer 可写，没有限速（P2 之前恒为真）。
    - 有多个候选时停 `silent_since` 最早的，一样早停 ID 小的。按 `Slow` 停（`stop_runner`），之后和低速停止完全一样：`AttemptFailure::slow`，有数据是重启、不记预算，没数据记预算，退避，M3 里不算在途。
  - 每个 tick 最多停一个：`restart_runner` 先判低速，低速停了就不判静默。
  - 检查 5（`RunnerRates::sample_at`）：一个 running runner 这个 tick 没有字节被接受，但 `silent_since` 不晚于 `now − hedge_idle`（2 s），并且在 `now − 2 s` 或更早的采样里已经在运行，就不算它“没交数据”；`TickSample::silent` 记下有这样的 runner。writer 满时谁都不放过，数据通道里有没取走的帧的也不放过。连接中的 runner 照样挡住扩张。和静默路径用同一个信号：在读、只是还没攒满一帧的 runner 是在交数据，不放过它；什么都没读满 2 s 的才算静默。
  - `Doubling`（`runners_to_add`）：有静默 runner 的 tick 和“活跃数少于翻倍后的数”一样处理，都是量不出待判断的翻倍：翻到上限的翻倍继续等、不扩张（C28 的规则）；上限以下的不下结论、丢掉，照常扩张。
  - 判断出静默停止时打 debug 日志 `stopping silent runner`，丢掉翻倍时打 `a restart drops the doubling`；bench 的探针多读这一条，表里多一列“静默停止请求”。
- **设计上的取舍。**
  - 阈值 4.5 s（3 个 tick），比 `hedge_idle`（2 s）长：收尾时对冲先到。更主要的是结构上的隔离：对冲的候选剩余量都 < `2 × min_chunk_size`，而静默路径只看 ≥ `2 × min_chunk_size` 的，对冲中的一对也排除，所以两者从不争同一个 runner。
  - 对冲因为没有空闲槽位发不出去时：这个 runner 剩余量 < `2 × min_chunk_size`，静默路径不碰它；对冲每个 tick 重试，槽位空出来就发；一直没有槽位就等空闲超时（30 s）。这和 M1 的决定一致：剩得少的只归对冲管。
  - 为什么要读的信号：manager 只看得到整帧（32 KiB），runner 没有按时间把缓冲交出来的机制。只看帧，“读得慢”和“卡住”分不开：一个 5 KiB/s 的 runner 要 6.4 s 才攒满第一帧，4.5 s 时被停、缓冲丢掉，每次都没有字节被接受，每次都记预算，最后 `RangeExhausted`。上一版用“参照速率”猜（自己的、别人的中位数、任务平分的速率，按 5 帧算），评审指出两处会错：快慢混在一起时，慢的那个被别人的中位数判成卡住；别的 runner 下完、采样滚出窗口之后才连上的 runner，三种参照都是 0，卡住了也只能等空闲超时。读的信号从源头上把两者分开，参照速率整个去掉了。
  - 为什么由 runner 自己记：runner 本来就有这个信号，它的空闲计时器在非空块和推送之后重置，等下游时不算空闲（`emit.rs:25`）。上一版由循环从副作用里推：一个计数，循环每个 tick 看它动没动、记成这个 tick 的时刻。评审几轮的问题大多出在这里：读的时刻最多晚一个 tick（1.5 s，s4b 的停止跟着晚）；取 `now` 和读计数的先后会把读记早（上一轮改成先读后取时间）；背压时 runner 不读也不交帧，要另外看数据通道是不是空的。现在 runner 在重置空闲计时器的同一个地方记下 `Instant::now()`，推送期间直接报“此刻在活动”，循环只拿来比，这几处都不用推了。
    - 也不再取 `last_delivery`。上一版取它和 runner 的时刻中较晚的，s4b、s4c 的停止被推后了：一次扩张让 writer 短暂写满，恢复时 `restart_silences` 把所有 runner 的帧静默重新算（§9.6），停住的 runner 也跟着重新算（加日志看了一次：s4b、s4c 里停住的 runner 的 `silent since` 都正好是最后一次 writer 恢复的时刻；它在停住之后没有交过帧，切分也没有逼出帧：runner 只在读到块、流结束、空闲超时、出错时推送）。writer 恢复对 runner 的读没有影响：被 writer 挡住的 runner 在推送，算在活动；没被挡住的照常读、照常记。所以静默路径和检查 5 只看 runner 的读，writer 恢复只重新算对冲的静默。writer 满的时候仍然不判（第 2 条）。
    - 推送之后只在接着读同一个块时（`resume_item`，空闲计时器也在这里重置）记时刻：流结束、空闲超时、出错时推出的最后一帧不是新的读，记了会让静默晚算。
    - 时刻是 runner 的 `Instant`，和循环的 `now` 同一个时钟；循环的 `now` 在 tick 开始时取，runner 在那之后记的时刻会晚于 `now`，只会让静默算短。
    - 测试里时间是显式传入的：`Harness` 的 runner 由测试持有共享的 `ReadActivity`，按测试的时钟调 `note_at`、`start_emitting`、`emitted_at`；`restart.rs` 里的真实 runner 按真实时钟记，测试用 `RunnerManager::read_activity`（仅测试）按测试的时钟补记（`note_at` 只取较晚的，真实时钟记下的更早的时刻不起作用）。
  - 数据通道里有没取走的帧时不判。runner 卡在等通道腾位置时报“此刻在活动”，这一种情况由 runner 自己的信号覆盖了。但还剩一种：runner 读完、推完几帧，之后流停住，而循环一直没取走那几帧；最后一次读之后 4.5 s 它看起来是静默的，这时停它，`Stopped` 会先于排着的帧被处理（生命周期在数据之前），帧随接收端丢掉，没有字节被接受的尝试还会记预算。writer 满时不判；writer 可写时循环会取数据，所以剩下的只有循环本身 4.5 s 没取数据，很少见，但会丢数据、错记预算，而检查只是看一眼通道（不取），所以静默路径和检查 5 仍然先看通道是不是空的。writer 恢复只重新算对冲的静默（§9.6）。低速路径不加这一条：它按窗口字节和中位数比，把排着帧的快 runner 排除掉会让被评判的 runner 不到 3 个，低速重启就被压住了；被评判的慢 runner 本身一直在交数据，不会是这种情况。对冲不受影响：败方的帧按设计就是丢掉的；对冲的触发只看帧，一个通道里排着帧的 runner 被对冲时，仲裁先取原 runner 排着的帧，原 runner 胜，只浪费一次连接，没有改。
  - 背压算活动：runner 在等数据通道腾出位置时没有读，但这不是卡住（runner 自己的空闲计时器这时也不走）。推送期间是“此刻在活动”，推送完接着读时记下时刻。
  - 静默从 `silent_since` 算：writer 满的时候不判；writer 恢复不重新算（见上），writer 满时 runner 多半在推送，本来就算在活动。
  - 还在连接的 runner 不算：连接超时（默认 30 s）管它；在这里停它一定是“没数据”，每次都记预算，连接慢的服务器会被耗尽预算而失败。连上之后的静默也只从它运行的那次采样算（第 3 条），连接用的时间不算进去。代价是对齐到 tick：连上后最多再晚 1.5 s。
  - 有数据之后的静默停止不记预算：这次尝试有字节被接受，退回的区间严格变短，和 §14.13 的重启一样不会没有进展地循环。没数据就记预算，所以一个总是一连上就停住的区间，最多 `max_attempts_per_range` 次（每次约 4.5 s 加退避）就以 `RangeExhausted` 结束（`a_slow_runner_without_data_exhausts_its_range`），而不是每次等 30 s。
  - 检查 5 与判断：静默的 runner 不贡献速度。如果在这样的 tick 判断翻倍，1 到 2 这一步里新 runner 一停住，增益就是 1.0，会被误判为停滞，此后再也不扩张。所以这样的 tick 不判断。上一版让翻倍一律等着，评审指出上限以下会重现 M1 的冻结：一个剩得少、静默路径不管、对冲又因为别的 chunk 还可切而不发的 runner，会让扩张一直停着。现在和 C28 的活跃数规则一致：只有翻到上限的翻倍等着（它本来就不能再扩张），上限以下的直接丢掉、照常扩张。在有静默 runner 的 tick 决定的翻倍，翻倍前的速度少算了静默的那份：判断时它若已恢复，增益偏高，最多多翻一次；链路已经饱和时，两边都是同一带宽，不会误判出增益。
- **与 §9.5、§9.2 的偏差。** 两节已经按上面改写。读的信号是实现时加的，维护者的决定里只说“静默”。
- **已知限制。**
  - 读的信号是“流有没有给出非空的块”。适配器自己缓冲、或者 TCP 层有字节但流不交出来的时候看不到；流一直给空块也不算读。
  - 读的信号来自 runner 自己的时钟，tick 只看已经记下的时刻。runner 读到一块、还没记下就被调度走时，这期间的 tick 看到的仍是上一次读；如果那之前确实已经静默满 4.5 s，它会被停，哪怕这一块刚到。同样，采样之后才到的读也赶不上这次停止。这只发生在正好卡在阈值上的读，那之前的静默是真的，和已修好的推送竞态不同。
  - 剩余量 < `2 × min_chunk_size` 的卡住 runner 仍然只靠对冲：M1 的第一种（s4a）里它本身不会被重启，只是检查 5 不再冻结扩张，别的 chunk 下得更快，收尾更早到来、更早对冲。
  - 阈值没有在真实网络上校准；HTTP 没有量。
- **结果。** 静默只按 runner 自己记的读、重启放开上限处的翻倍之后，s1、s2、s3、s4、s4a、s4b、s4c 和三个无停顿的对照全部重跑，每组 10 次，两种策略交替，中位数 / p90。“重启”是按低速停止、区间退回的 runner 数（低速和静默路径合计），“静默停止请求”是静默路径发出的停止请求数；被要求停止的 runner 可能在注意到之前就下完或失败，所以后者不一定包含在前者里。“重复 KiB”是 adapter 发出的超过文件大小的字节。

  | 场景 | 策略 | 完成时间 s | 峰值连接 | 对冲 | 重复 KiB | 停顿等待 s | 重启 / 静默停止请求 |
  | --- | --- | --- | --- | --- | --- | --- | --- |
  | s1_max32 | Legacy | 35.19 / 38.04 | 6.5 / 7.0 | 0 | 30 / 32 | n/a | 0 / 0 |
  | s1_max32 | Doubling | 15.94 / 16.01 | 32 / 32 | 0 | 160 / 224 | n/a | 0 / 0 |
  | s1_max8 | Legacy | 33.86 / 36.15 | 6.5 / 7.0 | 0 | 32 / 48 | n/a | 0 / 0 |
  | s1_max8 | Doubling | 21.08 / 21.09 | 8 / 8 | 0 | 32 / 48 | n/a | 0 / 0 |
  | s2 | Legacy | 4.82 / 5.30 | 2.0 / 2.0 | 0 | 40 / 48 | n/a | 0 / 0 |
  | s2 | Doubling | 4.74 / 5.40 | 2.0 / 2.0 | 0 | 32 / 48 | n/a | 0 / 0 |
  | s3 | Legacy | 16.10 / 16.12 | 4 / 4 | 0 | 21 / 34 | n/a | 0 / 0 |
  | s3 | Doubling | 15.61 / 15.63 | 4 / 4 | 0 | 32 / 48 | n/a | 0 / 0 |
  | s4 | Legacy | 42.47 / 42.62 | 5 / 5 | 0 | 10 / 12 | 30.71 / 30.88 | 0 / 0 |
  | s4 | Doubling | 11.80 / 12.55 | 8 / 8 | 1 | 50 / 60 | 2.81 / 3.56 | 0 / 0 |
  | s4_nostall | Legacy | 12.07 / 12.09 | 5 / 5 | 0 | 10 / 20 | n/a | 0 / 0 |
  | s4_nostall | Doubling | 9.09 / 9.10 | 8 / 8 | 0 | 16 / 16 | n/a | 0 / 0 |
  | s4a | Legacy | 65.87 / 65.96 | 2 / 2 | 0 | 0 / 0 | 30.81 / 30.93 | 0 / 0 |
  | s4a | Doubling | 12.36 / 12.37 | 8 / 8 | 1 | 32 / 32 | 6.95 / 6.97 | 0 / 0 |
  | s4a_nostall | Legacy | 35.27 / 35.29 | 2 / 2 | 0 | 0 / 0 | n/a | 0 / 0 |
  | s4a_nostall | Doubling | 11.78 / 11.78 | 6 / 6 | 0 | 0 / 0 | n/a | 0 / 0 |
  | s4b | Legacy | 37.78 / 37.92 | 2 / 2 | 0 | 0 / 0 | 30.63 / 30.79 | 0 / 0 |
  | s4b | Doubling | 13.84 / 13.99 | 2 / 2 | 0 | 16 / 32 | 6.20 / 6.34 | 1 / 1 |
  | s4b_nostall | Legacy | 8.73 / 8.74 | 2 / 2 | 0 | 0 / 0 | n/a | 0 / 0 |
  | s4b_nostall | Doubling | 8.73 / 8.74 | 2 / 2 | 0 | 0 / 0 | n/a | 0 / 0 |
  | s4c | Legacy | 35.67 / 35.75 | 5 / 5 | 0 | 8 / 8 | 30.67 / 30.75 | 0 / 0 |
  | s4c | Doubling | 13.23 / 13.29 | 8 / 8 | 0 | 49 / 58 | 6.78 / 6.87 | 1 / 1 |

  - 所有运行的 `unmatched` 都是 0。
  - 误重启：s1、s2、s3 和三个无停顿的对照，两种策略的每一次运行，重启和静默停止请求都是 0。
  - 和读的时刻按 tick 记的那一版（s4a 13.11、s4b 16.87、s4c 12.78 s）、以及只把信号交给 runner 的那一版（s4a 12.36、s4b 15.26、s4c 17.19 s）比（`Doubling`）：
    - s4b 13.84 s，停顿等待 6.20 s（9.25、7.60 s）：静默从停住的那一刻算，writer 恢复不再把它推后。
    - s4c 13.23 s，停顿等待 6.78 s（7.36、9.61 s）：`GROWTH_TRACE` 看了一次，停住的 runner 约 6.1 s 停住，10.58 s（停住后 4.5 s 的第一个 tick）被静默停止，同一个 tick 丢掉翻到 8 的待判断翻倍；区间退避 0.8 s 后在 11.38 s 被重新领取，12.08 s 扩张把它切成两半，13.17 s 下完。10 次都由静默路径收尾（每次 1 次静默停止，0 次对冲）。比对照（s4_nostall）慢 4.1 s；按 tick 记的那一版有 6 次靠对冲收尾，中位数 12.78 s 略快，但那一版的快来自切分恰好把停住的 chunk 切得够小，不是静默路径本身。
    - s4c 的重复字节 49 / 58（32–58）KiB，上一版 46 / 52（28–56）。每次至少 32 KiB：停住的 runner 缓冲里正好满一缓冲（32 KiB）还没推出去，停止时丢掉，区间重新请求时再发一次。多出来的是切分的超读：被切的 runner 在收到新上限之前已经从流里多读了一点，超过新上限的部分丢掉，对冲和切分都会这样。放开翻倍之后，重新领取的区间在尾部多被切一次（这次的 trace 里 32 + 26 KiB），所以中位数略升，在预期之内。
    - s4a 12.36 s 不变（对冲收尾）；s4 11.80 / 12.55 s，1 次对冲，在原来的范围里（11.03–12.56）。
    - s1、s2、s3：时间和峰值都在原来的范围里；`Legacy` 不变。
  - 和 §14.17、§14.18 比（`Doubling`）：s4a 12.36 对 18.32 s；s4b 13.84 对 38.49 s；s4c 13.23 对 38.72 s。
  - G7（bench 打印的，§14.18 的判据）：s1_max32 −54.8%、峰值 32 ≤ 32；s1_max8 −37.9%、8 ≤ 32；s2 +0.6%（逐轮差 −0.89 到 +0.63 s）、2 ≤ 2；s3 −3.1%、4 ≤ 8。四个都达标。
- **验证。**
  - runner 的单元测试 `test_read_activity_follows_the_body`：开始时没有时刻；空块不记；非空块记下当时的时刻；把数据通道推满之后，runner 等循环取，`last_active(now)` 就是 `now`；取走一帧后，runner 接着读同一个块，记下时刻。`test_a_resumed_runner_is_never_seen_silent`：推满数据通道、等循环取，取走一帧之后，`ReadActivity` 记下的每一个循环可能读到的状态（测试时记下每次加锁改完的状态）都是“在推送”或者“取走之后记过时刻”，没有一个会让这时的 tick 把它算成从推送之前就静默。`test_a_forced_push_is_no_read`：读到 100 B 后空闲超时，最后一帧推出去，记下的仍是读到那一块的时刻。`test_read_activity_keeps_the_bytes`：空块、100 B、空块、两个缓冲多 10 B 的块，数据照常。
  - 下载循环测试（`download_loop/tests/silent.rs`，用 hedge 测试的 `Harness`，runner 由测试持有通道和共享的 `ReadActivity`，按测试的时钟记活动，显式传入时间）：
    - `a_silent_runner_is_restarted`：静默差 1 ns 到 4.5 s 时不停，正好 4.5 s 停；交过 64 KiB 的 `attempts = 1`、`charged = 0`，没交过的 `charged = 1`；
    - `a_slow_runner_that_reads_is_never_silent`：5 KiB/s，每 5 个 tick 才攒满一帧，旁边一个快的；30 s 里从不被停，没有冷却记录，4 帧都进来了，`charged = 0`；
    - `a_pushing_runner_is_never_silent`：一直在推送的 runner 7 个 tick（10.5 s）都不停；推送在某一时刻结束，从那时起差 1 ns 到 4.5 s 不停，正好 4.5 s 停；
    - `a_late_runner_alone_is_restarted`：别的 runner 1.5 s 就下完了，它 21 s 才连上（早先的采样都已滚出窗口），什么都不读：25.5 s 差 1 ns 不停，25.5 s 停；
    - `a_runner_near_its_end_is_left_to_the_hedge`：剩余量 `2 × min_chunk_size − 1` 不停，正好 `2 × min_chunk_size` 停；
    - `a_hedged_runner_is_not_restarted`、`a_runner_is_silent_from_the_tick_it_runs_at`、`a_full_writer_is_no_silence`、`no_silent_restart_while_congested`、`one_silent_runner_is_stopped_a_tick`、`the_slow_and_the_silent_restart_share_a_tick`、`legacy_never_restarts_a_silent_runner`；
    - `a_stall_is_measured_from_the_last_read`：runner 读完一帧后停住，接着 writer 写满又恢复；它在最后一次读之后 4.5 s 停，差 1 ns 不停，比“writer 恢复之后 4.5 s”早；
    - `a_slow_restart_releases_the_doubling_at_the_limit`：上限 4，从 2 翻到 4，一个 runner 下完，翻倍等着；慢的 runner 在窗口满时被低速路径停下，同一个 tick 扩张就为空出的槽位切了一次（它的区间还在退避），活跃数比 A、C 多一个；
    - `a_restart_releases_the_doubling_at_the_limit`（s4c 的形状）：上限 4，从 2 翻到 4，翻到上限、待判断；别的 runner 都下完，活跃数 1，扩张等着；停住的 runner 被静默停止，区间退避后被重新领取，新 runner 交数据的那个 tick，扩张把它切开，活跃数 2；
    - `a_silent_runner_does_not_hold_the_growth_back`：静默 1.5 s 时仍挡住扩张，2 s 时放行，扩张到上限并且切了静默 runner 的 chunk；连接中、刚连上、连上 1.5 s 的仍挡住；在两个 tick 之间读过的，从读的时刻算，差 1 ns 到 2 s 仍挡住，满 2 s 放行；正在推送的一直挡住，推送结束后差 1 ns 到 2 s 仍挡住，满 2 s 放行；writer 满时谁都不放行。
  - `download_loop/tests/growth.rs` 的 `a_small_stalled_chunk_does_not_freeze_the_growth`：上限 8，翻到 4（上限以下，待判断），一个 runner 剩 1 MiB 时停住（静默路径不管，别的 chunk 还可切，对冲也不发）；判断的 tick 是静默的 tick，丢掉这次翻倍，照常翻到 8，那个 runner 没被停。
  - `queued_frames_are_no_silence`（`silent.rs`）：一个 runner 的数据通道排满 32 帧（循环落后了），之后没有活动，writer 空闲；2 s 时不放过它，下一个 tick 晚到 8 s 也不停它；之后循环把 32 帧都取走，全部被接受，`charged = 0`，没有冷却记录。
  - `doubling_strategy.rs` 的 `test_a_restart_drops_the_doubling_it_waits_for`：翻到上限的翻倍在一个 runner 下完后等着；`on_restarted` 之后不下结论、照常补位；上限以下、runner 数够、没有增益时，`on_restarted` 之后也不判断，照常扩张。
  - `test_a_reused_id_reads_by_its_own_activity`（`runner_manager.rs`）：spawn 时 `silent_since` 是 spawn 的时刻；旧尝试推送时是 `now`；留着旧尝试的 `ReadActivity`，ID 释放、再分给新的 runner 之后，在旧的上记时刻、置“正在推送”，静默都不变；在新的上记才更新。
  - `a_full_writer_is_no_silence` 改了含义：writer 满时照样不停；恢复之后，从 spawn 起就停住的 runner 在第一个 tick 就被停（它的静默是它自己的，和 writer 无关），对冲的静默照样从恢复时重新算（`last_delivery` 是恢复的时刻）。
  - 测试里交数据的 runner 也要记读：`Harness::deliver` 和 `growth.rs`、`restart.rs` 里模拟交数据的地方，在 `note_delivery` 之外按测试的时钟调 `note_at`（`growth.rs` 的 `deliver` 没有时间参数，置“正在推送”，表示一直在交）。
  - T13 原有测试（`download_loop/tests/restart.rs`）：两个卡住的 runner 按静默在 4.5 s 停；`a_stop_keeps_the_error_a_runner_holds` 里的 runner 带着错误、帧还排着：静默路径不碰它，窗口满时低速路径照旧停它，停止不盖掉它拿着的错误；
  - `queued_frames_do_not_hide_a_slow_runner`（`silent.rs`）：三个 runner，一个快的在每个 tick 都有一帧排着没取走；它照样算进中位数，慢的那个在窗口满时（10.5 s）被低速路径停下，快的不被停。
  - `a_slow_runner_is_stopped_and_its_range_retried`：真实 runner 读完 96 KiB 之后才取测试的 `t0`，并按 `t0` 补记活动，所以静默从 `t0` 算，和原来一样在 4.5 s 停。`a_slow_runner_without_data_exhausts_its_range` 就是“一连上就停住的区间，第 3 次以 `RangeExhausted` 结束”；`runners_at_the_same_low_rate_are_never_restarted` 照样通过（靠读的信号；三个真实 runner 每个 tick 读 3 KiB，测试按 tick 的时刻补记）；`a_slow_runner_that_delivers_is_stopped_on_its_window` 覆盖低速路径。
  - 单元测试：`test_running_since`（`throughput.rs`）；`test_a_tick_with_a_silent_runner_judges_nothing`（`doubling_strategy.rs`：上限以下，有静默 runner 的 tick 不判断，即使没有增益也不停滞，照常扩张；翻到上限的翻倍在静默的 tick 一直等，没有静默的 tick 才判断，停滞就不补位，有增益就补位）。
  - 集成测试 `tests/silent_restart.rs`（需要 `unstable-strategy` 和 `tokio`）：真实的下载，8 MiB，每条连接 2 MiB/s，上限 2，一个连接读到 6 MiB 停住、剩 2 MiB；停住的位置在 25 s 内被重新请求（空闲超时要 30 s），文件内容正确。这是冒烟测试，计时由上面的下载循环测试保证。
  - 变异检查（整个 bolt-load lib，all features，每次之前删掉 `proptest-regressions`），共 28 组，每组都有测试失败：
    - 阈值边界 `<=` 改成 `<`：`a_silent_runner_is_restarted`、`a_full_writer_is_no_silence`、`one_silent_runner_is_stopped_a_tick`、`a_pushing_runner_is_never_silent` 和 T13 的两个测试；
    - 对冲的候选也重启（去掉 `2 × min_chunk_size`）：`a_runner_near_its_end_is_left_to_the_hedge`；
    - writer 满的时间算作静默（去掉 writer 可写的条件）：`a_full_writer_is_no_silence`；检查 5 在 writer 满时也放行：`a_silent_runner_does_not_hold_the_growth_back`；
    - 一个 tick 停两个（低速和静默都判）：`the_slow_and_the_silent_restart_share_a_tick`；
    - 拥塞期间也停：`no_silent_restart_while_congested` 和性质测试 `splits_stay_within_the_limit_and_cover_the_file`；
    - 检查 5 仍然挡住扩张：`a_small_stalled_chunk_does_not_freeze_the_growth` 和 `a_silent_runner_does_not_hold_the_growth_back`；
    - `Legacy` 也采样（于是也重启）：`legacy_never_restarts_a_silent_runner`、`legacy_never_restarts`、`legacy_never_hedges` 和 `legacy_is_the_default_and_unchanged`；
    - 有数据之后的静默停止也记预算：`a_silent_runner_is_restarted`、`a_slow_runner_is_stopped_and_its_range_retried`，以及 C12 的两个单元测试；
    - 静默从 spawn 算（去掉“连上至少 4.5 s”）：`a_runner_is_silent_from_the_tick_it_runs_at`、`a_late_runner_alone_is_restarted`、`the_slow_and_the_silent_restart_share_a_tick` 和 `doubling_waits_for_every_runner_and_the_congestion`；
    - 静默仍取 `last_delivery` 和读中较晚的（被接受的帧、writer 恢复重新算静默）：`a_full_writer_is_no_silence` 和 `a_stall_is_measured_from_the_last_read`；
    - 每次推送结束都记时刻（流结束、空闲超时推出的帧也算读）：`test_a_forced_push_is_no_read`；
    - 推送完接着读时不记：`test_read_activity_follows_the_body`；
    - 不记 spawn 的时刻：`a_silent_runner_is_restarted`、`a_late_runner_alone_is_restarted`、`a_full_writer_is_no_silence` 等十个测试；
    - 静默路径停止时不放开翻倍：`a_restart_releases_the_doubling_at_the_limit`；低速路径停止时不放开：`a_slow_restart_releases_the_doubling_at_the_limit`；
    - 推送结束、接着读时分两次加锁（先清标记、再记时刻）：`test_a_resumed_runner_is_never_seen_silent`；
    - 忽略活动的信号（只按帧算静默）：`a_slow_runner_that_reads_is_never_silent`、`runners_at_the_same_low_rate_are_never_restarted`、`a_pushing_runner_is_never_silent`、`a_silent_runner_does_not_hold_the_growth_back` 和 `test_a_reused_id_reads_by_its_own_activity`；
    - 循环不把“正在推送”当成此刻在活动：`a_pushing_runner_is_never_silent`、`a_silent_runner_does_not_hold_the_growth_back`、`test_a_reused_id_reads_by_its_own_activity` 和 `test_read_activity_follows_the_body`；runner 不报“正在推送”：`test_read_activity_follows_the_body`；
    - 空块也记时刻：`test_read_activity_follows_the_body`；
    - 有静默 runner 的 tick 也判断翻倍：`test_a_tick_with_a_silent_runner_judges_nothing`；上限以下的翻倍在静默的 tick 也等着：`a_small_stalled_chunk_does_not_freeze_the_growth` 和 `test_a_tick_with_a_silent_runner_judges_nothing`；
    - 静默路径不看排着的帧：`queued_frames_are_no_silence` 和 `a_stop_keeps_the_error_a_runner_holds`；检查 5 不看排着的帧：`queued_frames_are_no_silence`；
    - 排着的帧也挡住低速路径（放进 `may_restart`）：`queued_frames_do_not_hide_a_slow_runner` 和 `a_stop_keeps_the_error_a_runner_holds`；
    - C12 的 `may_restart` 变异照样被抓住：评判一对中的一方，`the_survivor_of_an_undecided_pair_is_not_judged`；评判正在停止的 runner，`one_silent_runner_is_stopped_a_tick`。
    - 其中两次（阈值边界、空块也记时刻）已知会偶发失败的 `test_save_into_rejects_hostile_filenames` 也失败了，和变异无关。

### 14.20 `Doubling` 成为唯一的策略（PR-7b，C14）

- **决定。** 2026-11-09 实施时对 T15 未写明的几点的决定（实施负责人定的，维护者没有另外指示）：
  - bench 保留，只跑 `Doubling`（场景、探针、无停顿的对照），去掉 `Legacy` 一列和 G7 的判定；最后一次两种策略的对比以 §14.19 为准。
  - §14.14–§14.19 留作历史；README 和 `docs/task.md` 只讲 `Doubling`。
  - 保留 `Strategy` trait 和 `StrategyControl`，只删 `Growth` 这个枚举。
  - 只有 `Legacy` 用的切分动作和 planner 方法一起删掉（`SplitAllTask`、`SplitGivenTask`、`get_incomplete_states`、`find_chunk_to_split`），它们的测试改走 `split()` 或 `Grow`；删掉的覆盖逐条写明。
  - `GrowthStrategy` 和 options、builder 上的方法整个删掉；破坏性改动写进 README 和提交信息的 `BREAKING CHANGE` 脚注。
  - `idm-*.md` 两份分析文档不改，T15 的 `rg` 检查排除它们。
  - `tests/silent_restart.rs` 只要求 `tokio`。
  - 端到端测试暴露的真问题在这个 PR 里修；只断言 `Legacy` 节奏的测试改写，并列出新旧断言；要改设计的问题先报告，不在这里修。
- **做法。**
  - `StrategyControl` 直接持有 `DoublingStrategy`；`execute` 去掉 `Legacy` 分支和两个速度参数（任务速度、平均每 runner 速度只喂给 `Legacy`），tick 的采样改为必有（`&TickSample`）。`is_doubling()`、`Growth`、`dynamic_strategy.rs`（`DynamicStrategy`、阈值和它的两个单元测试）删掉。
  - `DownloadLoop::rates` 从 `Option<RunnerRates>` 改为 `RunnerRates`，`Legacy` 不采样的分支（低速重启、静默重启、对冲、`planned_split_size`）随之去掉。
  - `ThroughputSampler::speed` 删掉：只有 `Legacy` 读它；`per_runner_speed` 留着，对冲和切分的计划大小还要用（§9.3）。吞吐计时器和进度事件不变。
  - `StrategyAction::SplitAllTask`、`SplitGivenTask` 和 `apply` 里的分支删掉，`ChunkPlanner` 和 `RunnerManager` 的 `get_incomplete_states`、`find_chunk_to_split`，以及 `SplitCandidate` 一起删掉。`largest_running_chunk` 的文档原来引用 `get_incomplete_states`，改为直接说明“连接中的 runner 不切”。
  - `Cargo.toml` 删掉 `unstable-strategy`；bench 的 `required-features` 只剩 `tracing`、`tokio`（都是默认 feature），运行命令是 `cargo bench -p bolt-load --bench growth_strategy`。
  - bench：去掉策略一维（`GROWTH_STRATEGIES`、表里的策略一列、交替运行）和 G7 判定（`fills`、`g7_verdict`、`G7_TOLERANCE`）；“应当对冲”的检查对每次运行都做。
  - `tests/silent_restart.rs` 改为 `#![cfg(feature = "tokio")]`，不再设策略。
- **破坏性改动。**
  - 打开 `unstable-strategy` 的 crate 不再能编译（cargo 报找不到 feature）。
  - `GrowthStrategy`、`TaskOptions::growth_strategy`（字段和方法）、`TaskBuilder::growth_strategy` 删除；这几个原来都要打开那个 feature 才有，默认用户的 API 不变。
  - 默认用户的行为变了：扩张改为翻倍（§9.2），并且有低速重启、静默重启（§9.5）和收尾对冲（§9.6）。
  - 仓库没有 CHANGELOG，破坏性改动写在 crate 的 README 和提交信息里。
- **测试的变化。** lib 测试（all features）从 573 个变为 563 个。
  - 删掉，没有替代（行为不存在了）：
    - `dynamic_strategy.rs` 的 `test_every_slot_up_to_the_limit_is_used`、`test_legacy_thresholds_and_abs`：`Legacy` 自己的阈值和用满槽位。用满槽位由 `doubling_strategy.rs` 的 `test_steady_growth_doubles_up_to_the_limit` 和下面的 `test_growth_splits_only_running_runners` 覆盖。
    - `options.rs`、`builder.rs` 的 `test_growth_strategy_sets_the_option`；`test_default_options_are_valid` 去掉对 `growth_strategy` 的两条断言。
    - `legacy_never_hedges`（`hedge.rs`）、`legacy_never_restarts_a_silent_runner`（`silent.rs`）：`Legacy` 不采样、不对冲、不重启。
    - `legacy_never_restarts`（`restart.rs`）：`Legacy` 一半同上；`Doubling` 一半（真实 runner 停住后被静默停止，chunk 仍是 Running）由 `a_slow_runner_is_stopped_and_its_range_retried` 和 `a_slow_runner_without_data_exhausts_its_range` 覆盖。
    - `chunk_planner.rs` 的 `test_find_chunk_to_split`、`test_get_incomplete_states`、`test_split_candidates_skip_ranges_backing_off`：方法删掉了。它们守的性质：连接中的 runner 不切，由 `largest_running_chunk` 保证（`test_growth_splits_only_running_runners` 每轮都查连接中的 runner 没收到上限）；退避中的区间不妨碍切分，`Grow` 只切运行中的 chunk、根本不看未分配的区间，所以这条在结构上成立，不再单独测；“挑剩得最多的 chunk”由 `runner_manager.rs` 里 `largest_running_chunk` 的已有测试覆盖。
  - 改写（旧断言 → 新断言）：
    - `legacy_is_the_default_and_unchanged` → `doubling_is_the_default`（`growth.rs`）：旧：默认和 `Legacy` 都不采样，1.5 MiB/s 时 `SplitAllTask(3 MiB)`，降到 0.5 MiB/s 时 `SplitAllTask(1 MiB)`；新：默认 options 的循环在每个 runner 都交数据的 tick 上发出 `("doubling", [Grow(1)])`。
    - `probe_strategy` → `probe_growth`，`test_strategy_splits_only_running_runners` → `test_growth_splits_only_running_runners`（`tests.rs`）：旧：`Legacy` 每轮 quick start 的 `SplitAllTask` 只切 runner 0；新：每轮 `Grow(上限)`，第一轮从 runner 0 切出 `min(上限 − 1, 13)` 个（16 GiB 对半切 13 次后剩下的不到计划的 4 MiB），之后各轮不再切；每轮都查连接中的 runner 没收到上限、runner 数不超过上限。`test_one_connection_is_never_split`：旧：64 个 `Legacy` tick 没有切分；新：64 轮 `Grow(1)` 一个也没加。
    - `test_no_free_slot_skips_splits_and_new_runners`：旧：`SplitAllTask(1)`、`SplitGivenTask(第一个)`、`SplitGivenTask(7)`（不存在的 runner）都不变；新：`Grow(2)` 不变，`split(第一个)` 返回 false。去掉的覆盖：按 ID 切一个不存在的 runner，现在没有动作按 ID 选 runner。
    - `test_strategy_tick_while_finished_runners_keep_their_slots`：旧：4 MiB/s 的 `Legacy` tick 先不切（没有空槽位），runner 3 停下后切了 runner 0；新：`Grow(2)` 先不切，runner 3 停下后 runner 0、1 中正好一个被切（两者剩的一样多，切哪个取决于 planner 的顺序），另一个没收到上限。
    - `test_rejected_split_spawn_reclaims_its_slot_once`：`SplitGivenTask(0)` 换成同样的调用 `split(0, 0..HALF)`。
    - `retry_after_holds_back_every_new_connection`（`retry.rs`）：旧：拥塞期间 `SplitGivenTask(0)`、`SplitAllTask(1)`、2 MiB/s 的 `Legacy` tick 都不切；新：`split(0)` 返回 false、`Grow(2)` 和 `Doubling` 的 tick 都不切；拥塞结束时 `split(0)` 切。`retry_after_holds_back_the_requests_to_the_server`：`SplitGivenTask(0)` 换成 `split(0, 剩下的)`。
    - `a_hedge_after_two_splits_runs_to_the_end`（`hedge.rs`）：`SplitAllTask(0)` 和 `SplitGivenTask(o)` 换成两次 `split(o, 剩下的)`，切分序列不变。
    - 性质测试 `splits_stay_within_the_limit_and_cover_the_file`：不再随机选策略；`Op::SplitAll(planned)` 换成 `Op::Grow(n)`，`Op::SplitGiven(id)` 直接调 `split`；`Op::Tick` 不再设任务速度。
    - 其余测试只有机械改动：去掉 `growth_strategy(Doubling)`，`rates` 不再是 `Option`，`execute` 少两个参数。
  - 端到端测试（`range_faults`、`empty_body`、`custom_adapter`、`custom_timer`、`silent_restart`、`bolt-load-tests` 和各 crate 的测试）第一次在默认的 `Doubling` 下跑，全部通过，没有要改的。
- **T15 的检查。** `rg "threashold|DynamicStrategy|unstable-strategy" crates -g '!crates/bolt-load/docs/idm-insights.md' -g '!crates/bolt-load/docs/idm-improvement-plan.md'` 无结果。排除的两份是 IDM 的分析文档，引用的是当时的代码。
- **结果。** 切换之后全部场景各跑 10 次（`cargo bench -p bolt-load --bench growth_strategy`），中位数 / p90，和 §14.19 的 `Doubling` 对比。

  | 场景 | 完成时间 s | §14.19 | 峰值连接 | 对冲 | 停顿等待 s | 重启 / 静默停止请求 |
  | --- | --- | --- | --- | --- | --- | --- |
  | s1_max32 | 15.95 / 16.01 | 15.94 / 16.01 | 32 | 0 | n/a | 0 / 0 |
  | s1_max8 | 21.10 / 21.12 | 21.08 / 21.09 | 8 | 0 | n/a | 0 / 0 |
  | s2 | 5.56 / 6.95 | 4.74 / 5.40 | 2 | 0 | n/a | 0 / 0 |
  | s3 | 15.57 / 15.58 | 15.61 / 15.63 | 4 | 0 | n/a | 0 / 0 |
  | s4 | 11.07 / 12.56 | 11.80 / 12.55 | 8 | 1 | 2.08 / 3.56 | 0 / 0 |
  | s4_nostall | 9.08 / 9.09 | 9.09 / 9.10 | 8 | 0 | n/a | 0 / 0 |
  | s4a | 12.36 / 12.38 | 12.36 / 12.37 | 8 | 1 | 6.96 / 6.98 | 0 / 0 |
  | s4a_nostall | 11.78 / 11.79 | 11.78 / 11.78 | 6 | 0 | n/a | 0 / 0 |
  | s4b | 13.75 / 13.96 | 13.84 / 13.99 | 2 | 0 | 6.11 / 6.32 | 1 / 1 |
  | s4b_nostall | 8.73 / 8.74 | 8.73 / 8.74 | 2 | 0 | n/a | 0 / 0 |
  | s4c | 13.14 / 13.18 | 13.23 / 13.29 | 8 | 0 | 6.62 / 6.69 | 1 / 1 |

  - 除 s2 外都在 §14.19 的范围里；所有运行的 `unmatched` 都是 0，s1、s2、s3 和无停顿的对照没有重启。
  - s2 慢了 0.8 s（中位数），是 s2 已知的噪声，不是默认路径的变化：
    - 行为没变：10 次的峰值都是 2，没有 429、对冲、重启，重复字节 0–48 KiB，和 §14.19 一样。变的只有时间，而且是逐次漂移：前 7 次 5.30–5.75 s，后 3 次 6.43–7.11 s。
    - 紧接着只跑 s2 再跑 10 次，代码不变：4.83 / 6.58 s（3.81–7.38），中位数回到 §14.19 附近，同样是前几次快（3.81–4.19 s）、最后几次慢（5.87–7.38 s）。§14.18 已经记过 s2 很不稳；§14.19 那一轮的 `Legacy` 是 4.82 / 5.30，上一轮（C29 的第二版）`Doubling` 是 5.18 / 6.53。s2 每次往磁盘写 4 GiB、约 800 MiB/s，受机器当时的写回和内存情况影响（这次跑的时候机器内存紧张）。
    - `Doubling` 走的代码没有变：它原来就采样（`rates` 一直是 `Some`），`execute` 里 `Doubling` 的分支原样保留，删掉的只有 `Legacy` 的分支和只给 `Legacy` 用的任务速度。
- **检查。** fmt；clippy 四种配置（workspace all-features all-targets、workspace 默认 all-targets、`-p bolt-load --lib --no-default-features --features smol`、`-p bolt-load --lib --no-default-features --features tokio`；原来的第三种要 `unstable-strategy`，换成了 smol 这一种）都只有原有的三个“未使用依赖”警告；`cargo test --workspace --all-features --locked`（lib 563）和 `cargo test --workspace --locked`（lib 532）全过；`RUSTDOCFLAGS=-Dwarnings cargo doc --no-deps --all-features -p bolt-load` 是 18 个错误（17 个链接解析不了、1 个链接到私有项），比原来少 1 个：`docs/task.md` 里指向 `DynamicStrategy` 的链接没了。
- **变异检查**（整个 bolt-load lib，all features）：
  - 扩张也切连接中的 runner（`largest_running_chunk` 改为看所有活跃的 chunk）：`test_growth_splits_only_running_runners`；
  - 扩张不看计划的切分大小：`doubling_splits_only_a_chunk_of_the_planned_size`、`test_growth_splits_only_running_runners` 和性质测试；
  - 拥塞时也切：`retry_after_holds_back_every_new_connection` 和性质测试；
  - 每个 tick 不再采样（`rates` 不再必有之后的关键一步）：34 个测试失败。

### 14.21 已写入的区间（PR-9，C18）

- **决定。** 实现时 lead 对 T19 未写明的几点的决定：
  - `written` 用一组合并过的区间，不用每个 chunk 一个位点（下面“做法”说明原因）。
  - 一批完成里有失败的写，其余成功的照样记下，再返回第一个失败；`writer.finish()` 冲刷时完成的写，任务失败或取消也照样记下。
  - `Progress.downloaded` 仍是已接收的字节，`downloaded_chunks` 改为已写入的区间，两者的差别写进 `Progress` 的文档。
  - 单连接任务不变，仍报 `0..downloaded`。
  - 回退到单连接时照旧丢掉并发那一次的进度。
  - 任务失败或取消时，最后的 `downloaded_chunks` 是 teardown 之前写入成功的部分（原来是已接收的部分）。这是有意的行为变化。
  - 提交类型原定 `feat(task)`，在提交信息里写明含义的变化。第一轮评审之后，lead 改为 `feat(task)!`：一个公开字段的含义变了，使用者看得到不同的结果却不会有编译错误，按 tasks.md §0 第 3 条要加 `!` 并写清迁移方式。
- **做法。**
  - `ChunkPlanner::written`（`WrittenSet`）：排好序、互不重叠也不相邻的区间。`note_written(range)` 把一个写完的区间并进去，空区间不算。只在 `DownloadLoop::record_completions` 里、按 writer 的 `WriteCompletion` 推进（`RunnerManager::note_written`）。
  - 为什么是区间集合而不是 §11.1 说的“位点”：`PendingWriter` 同时有最多 `max_connections` 个写在后台运行，同一个 chunk 的写会乱序完成；`WriteCompletion` 只带区间，不带 runner 或 chunk。每个 chunk 一个位点就得另外缓存“前面还没写完”的完成。集合也不受 chunk 怎么切、重试、对冲的影响。
  - `debug_assert`：写完的区间必须落在已接收的区间里（`written ⊆ downloaded`）。已接收的字节从不丢（`accepted_bytes` 只增不减，切分、失败、退避、对冲都保留 `downloaded`），所以生产路径上它不会失败。唯一失败的是一个测试：`test_custom_timer_survives_other_events` 绕过 planner 直接往 writer 写 `0..1`。测试改为先让 planner 接收这 1 个字节，再反复写它（集合对同一区间是幂等的），并把 spawn 的区间从 `0..LOOP_TOTAL` 改为 `1..LOOP_TOTAL`。断言没有放宽。
  - `completion.rs`：`check_completions` 改为 `settle_completions(completions, on_written)`，每个成功的完成都交给 `on_written`，第一个失败作为任务的 `TaskError::Write` 返回，后面的成功照样交出去。
  - 进度：`publish_progress` 报 `written` 的区间；`finish()` 在丢掉 manager 之前把 `written` 拿出来，`writer.finish()` 冲刷完成的写并进去，再报出去。所以完成的任务仍报 `[0..total]`，失败或取消的任务报 teardown 之前写入成功的区间。
  - `runner_manager/data.rs` 的 `handle_data_frame`（T19 写的 `:154`，现在在 `update_progress` 那里）加了注释：进度先于写入，只有 `downloaded` 跑在前面，`written` 跟着 writer 的完成走。
  - `RunnerManager::get_downloaded_ranges` 没有调用者了，删掉；`ChunkPlanner::get_downloaded_ranges` 留着（`debug_assert` 和测试用）。
- **各种情况和 `written` 的关系。**
  - 切分：总在被切 runner 的 `downloaded.end` 或之后切，而 `written ⊆ downloaded`，所以切分点之后没有写过的字节。
  - 对冲（§9.6）：败方的帧，包括晚到的，在 `on_data_frame` 里就丢掉了，到不了 planner 和 writer，不会有完成；两方只按 planner 接收的区间写同一个 chunk，没有字节写两次。`hedge.rs` 测试的 `Harness::written` 现在每次都核对 planner 的 `written` 和 writer 实际写过的区间一致。
  - 重启、停止：区间从 `downloaded.end` 退回；之前的字节要么写完了，要么还在路上，它们的完成在区间退回之后照样记进集合；重试从 `downloaded.end` 开始，不重叠。
  - 合并、领取冷却中的区间：集合和 chunk 怎么排没关系。
  - 回退到单连接、从头再来：`downloaded` 从不在一个 `ChunkPlanner` 里清零或丢掉；唯一丢掉它的是回退，这时整个并发那一次（`DownloadLoop`、它的 `ChunkPlanner` 和 `written`）随之丢掉，单连接从自己的计数从零开始报。没有别的“从头再来”的路径（资源变了之后从头重来见 #195；C19 只让任务以 `ResourceChanged` 失败）。所以重置是结构上的：没有一行重置代码，也就没有可以漏掉的一行。`test_a_fallback_starts_with_nothing_written`（`status_contract.rs`）：range 先正常下 3 MiB（越过 adapter 1 s 的突发），然后报 `RangeIgnored`；并发那一次报过已写入的区间，回退之后单连接的每个状态都是 `[0..downloaded]`，而且 `downloaded` 从零单调增长，最后是 `[0..SIZE]`。
- **`downloaded_chunks` 保证什么、不保证什么。** 它是 teardown 之前 writer 确认写入成功的区间，偏保守：
  - 一个失败的写可能已经把一部分字节写进了文件（`pool_writer.rs` 的 `test_write_all_at_fails_when_nothing_is_written`：1 个字节落盘之后才报 `WriteZero`），这部分不报；
  - 不保证持久：没有 `fsync`（那是 C20 的 `durable`）；
  - 不保证文件还在：失败或取消的任务会删掉临时文件。
- **迁移。** 原来并发任务的 `downloaded_chunks` 是已接收的区间。要已接收的字节总数，用 `Progress.downloaded`；`downloaded_chunks` 当作“确认写入成功的区间”来读。`Progress` 的文档里写了同样的说明。
- **不在 C18。** §11.1 的第三个位点 `durable`（`fsync` 之后才推进）和状态文件一起在 C20 做。C18 的回退没有重置代码（见上），真正的“重置之后不留旧区间”的测试，跟着 C20 的状态文件一起写：那时重置是实际的代码（丢弃或重写状态文件）。C19 原来要在任务内从头重来时写这个测试；重来移到引擎之上以后（§14.23），这个测试也跟着移到 #195。
- **测试。**
  - `chunk_planner.rs`：`test_written_ranges_merge_in_any_order`（乱序、重复、空区间的合并）；`test_written_lags_downloaded`（写完之前 `written` 落后于 `downloaded`；后完成的写先记；切分、失败、退避之后再完成的写照样记，`downloaded` 不变）。
  - `completion.rs`：`a_failed_write_keeps_its_range_and_io_error` 加上“失败的写不记、它前后的写都记”（`[0..4, 8..12]`）；`successful_writes_count_their_bytes` 核对交出的区间；`a_closed_writer_queue_is_a_broken_pipe` 核对什么也没交出。
  - `download_loop/tests/written.rs`：`written_lags_downloaded_until_the_write_completes`（两帧接收了、写还停着：planner 和发布的进度都是“已接收 64 KiB、已写入空”；写完之后都是 `[0..64 KiB]`）；`the_final_flush_counts_its_writes`（第二个写完成了但循环还没处理，`finish()` 的冲刷把它算进最后的进度）。`Harness` 现在留着状态的接收端，循环才会发布进度。
  - `hedge.rs` 的 `Harness::written` 每次核对 planner 和 writer 一致（对冲的各个测试都经过它）。
  - 第一轮评审之后加的，结束时没成功的几种（`written.rs`，`Harness::failing` 用一个让指定区间写失败的记录 writer，`FileWriter::recording_failing`）：
    - `a_cancelled_task_counts_its_flushed_writes`：取消的任务，冲刷时完成的写照样算，任务的结果仍是 `Cancelled`；
    - `a_failed_task_keeps_its_error_over_the_flush`：任务已经因为 `Timeout` 失败，冲刷里一个写失败、一个成功：结果仍是 `Timeout`，成功的区间算，失败的不算（`[0..32 KiB, 64..96 KiB]`）；
    - `the_first_failed_write_of_the_flush_is_the_error`：冲刷里两个写都失败，后面那个区间先完成：任务的错误是先完成的那个（`64..96 KiB`），不是偏移小的那个；两个都不算。

### 14.22 性质测试里的失败按循环的方式处理（PR-9a，C30）

- **起因。** 性质测试 `splits_stay_within_the_limit_and_cover_the_file` 找到一个序列（`max_connections = 8`，缩到 15 步），在 `ChunkPlanner::mark_failed` 的 `debug_assert`（“一对里先失败的一方不该走这里”）上失败。最后几步是：一个还在连接的 runner（7）静默满 2 s、没有可切或可领取的区间，被对冲（对冲 2，§9.6 允许对冲连接中的 runner）；接着测试的 `Op::Fail` 挑中 7，直接调 `RunnerManager::mark_failed(7)`。
- **出现在哪些提交上**（同一序列写成确定的测试，在各提交上跑）：C27（`4391ed0`）和 C13（`9bc7766`）上更早一步就失败了，是 C28 修掉的那个预言误报（对冲 H 没算占着槽位）；C28（`5d9fac6`）、C29（`823497c`）、C14（`ea69115`）和 C18 上都在这个 `debug_assert` 上失败。也就是说，这个状态从 C27 加入对冲起就能到达，C28 修好预言之后才显出来；C14 把 `SplitAll`、`SplitGiven` 换成 `Grow(n)`（一步就开出好几个连接中的 runner），让 256 个随机用例碰上它的概率大了很多。
- **不是产品的问题。** 真实的循环里，一对中任何一方失败，都先经过 `has_other_side`，走 `handle_side_failed`：另一方留下 chunk，什么也不释放、不退避。所有收到失败的路径都先查这一条：`Stopped`（`lifecycle.rs`）、spawn 被拒、生命周期流断开。`chunk_planner.mark_failed` 的其它调用者都在这个检查之后（`fail_attempt`、`handle_connect_failed`、取消），或者到不了一对中的一方：低速和静默重启不判一对中的一方（`may_restart`），对冲不挑正在停止的 runner；`release_runner_full` 只在测试里编译。所以 release 构建里（`debug_assert` 关掉时）没有一条路径会对一对中的一方调 `mark_failed`。问题在测试的驱动：`Op::Fail` 绕过了循环的处理。
- **做法**（lead 的决定：单独一个 PR，放在 C18 之后、C19 之前；dev 上没有对冲、`Grow` 和 C28 的预言，放不上去）：
  - `RunnerManager::report_stopped(runner_id, reason)` 和 `fail_runner(runner_id, error)`（只在测试里编译）：把一个 `Stopped` 事件交给生命周期的处理（`handle_lifecycle`），和 runner 的生命周期通道报上来时一样，包括对一对的处理、对被打败一方的处理。
  - `Op::Fail` 改为对 runner 调 `fail_runner`，再把得到的 `RunnerTick` 交给 `download.on_runner(tick, now)`，和循环一样。能失败的 runner 除了连接中、运行中的 chunk 主人，还加上未分胜负的对冲和被打败的一方。断言分两种：
    - 普通的失败：尝试数加 1，计费按错误分类（`ChannelClosed` 记 1、有数据后的 `Timeout` 不记、拥塞不记），退回的区间进入退避，冷却不早于 `MIN_RANGE_BACKOFF`；
    - 一对中的一方或被打败的一方：没有 `failure`（一对中的一方有 `side_failure`），冷却不变，另一方留下同一个 chunk、不再有伙伴，槽位收回。
  - `Op::Progress` 改为把一帧交给 `RunnerManager::on_data_frame`（循环从通道取帧时走的路，第一帧决定一对的胜负），不写入：循环的 writer 在测试里停着。`Op::Finish` 先用 `on_data_frame` 交完剩下的字节，再 `report_stopped(Finished)`，结果交给 `on_runner`。
  - `on_runner` 可能因为失败策略结束任务；步骤这时返回 `ControlFlow::Break`，序列停下。为了让序列不太早结束，性质测试的 `max_consecutive_failures` 设为 2^20，`max_attempts_per_range` 设为最大值：这个性质关心的是循环的状态，不是什么时候放弃。
  - 缩出来的序列写成确定的测试 `the_first_side_of_a_pair_fails_as_a_side`：先到达“7 连接中、被 2 对冲、未分胜负”，再分别让 7 和 2 失败，另一方都留下同一个区间、没有伙伴、冷却不变。不提交 `proptest-regressions` 文件。
- **验证。** 改完之后用 4096 个用例跑了一次性质测试（临时改 `with_cases`，跑完改回 256：显式的 `with_cases` 不看 `PROPTEST_CASES`），没有新的失败，5.4 s。另加日志数了一次：4096 个用例里有 3629 次失败，其中被打败的一方 1 次，一对中的一方 0 次，第一帧决定胜负 1 次。也就是说，随机序列很少走到未分胜负的一对，一对的失败主要靠上面的确定测试覆盖。让性质测试更常到达一对（比如更小的 `TOTAL` 或更偏向 `Advance` 的分布）留作以后的事，这里没有做。
- **第一轮评审之后（lead 的决定：针对一对的确定测试代替偏置生成器，偏置不进这个 PR）。**
  - 一对的最后一方：第一方失败之后，留下的一方没有伙伴了，走普通失败的断言，但它带着这一对的计费（§9.6，`hedge.first`）：两方都不是重启、并且至少一方记预算时，这一对记一次。原来的断言按最后一方单独算，比如 7 先以 `Retryable` 失败、2 再以 `Congestion` 失败，产品记 1，断言要 0。现在 `fail()` 用 `hedge.first` 和最后一方的分类算期望值；一方的分类（是不是重启、记不记预算）按 design §8.2 写成测试自己的表（`Failure::class`，丢失记、超时有自己的字节就是重启否则记、拥塞都不），不调产品的 `charges_range`、`is_restart`。一方失败时还核对留下的一方记下的 `hedge.first` 和表一致。
  - `a_pair_is_charged_once_for_both_sides`：一个运行中、自己交过字节的 runner（0）被对冲（1，连接中），两方各自先失败，三种失败（`Retryable`、`Timeout`、`Congestion`）两两组合，共 18 种：这一对算 1 次尝试，按上面的规则最多记 1 次。原始一方的 `Timeout` 是重启，对冲一方的 `Timeout` 记预算。`the_first_side_of_a_pair_fails_as_a_side` 也走到第二次失败：先 `Retryable` 再 `Congestion`，记 1 次。
  - 结束的一步也查：一步让循环结束时，照样跑 `check()` 和“已接收的字节不减少”，再停。能让生成的序列结束的只有两种：`Finish` 把文件下完（`on_runner` 返回 `Break`），和没有别的尝试在路上时的拥塞（产品按设计结束任务）；别的 `Err` 都算失败。
  - `Progress` 和 `Finish` 收下非空的区间时，照 `on_runner` 的做法调 `policy.on_progress(now)` 和 `manager.note_delivery`，对冲的静默从这时重新算。仍然不写入。
  - `Start` 也可以挑连接中的对冲（走真实的 `Started` 处理，`report_started`），`Progress` 也可以挑运行中的对冲，所以对冲一方也能交帧、赢下一对。
  - 变异检查：产品里一对的计费不看第一方、一方失败不记分类，各让 7 个和 6 个测试失败（包括上面两个确定测试）；测试里按最后一方单独算，让两个确定测试失败。
  - 4096 个用例再跑一次：没有失败，5.7 s。计数：3692 次失败里，对冲一方失败 1 次，被打败的一方 0 次，一对的最后一方 0 次；对冲连上 1 次，一对由交帧决定 0 次。随机序列仍然很少到达一对，一对的组合由上面的确定测试覆盖。
- **第二轮评审之后。** 对冲一方能连上、交帧之后，预言里两处按“主人”记的状态错了：
  - 接管不是新连接：`Before` 原来只记 chunk 的主人。对冲赢下一对（或原始一方失败、对冲接手）之后，它的 ID 成了新的主人，`check()` 把它当成新开的连接，按切分的尾巴、领取冷却的规则要求计数是 `(0, 0)`，也会在拥塞期间误报“连接”。按 §9.6，接手的一方继承 chunk 的计数。现在 `Before` 也记下每个未分胜负的对冲和它的 chunk（起点、尝试数、记预算的次数）；新主人如果是上一步的对冲、并且 chunk 起点相同，就是接管：计数必须不变，不做新连接的检查。确定测试 `a_hedge_that_wins_keeps_the_counters_of_its_chunk`：一个区间失败一次、记了预算（`(1, 1)`），退避之后重新领取，下到只剩 `MIN - 1` 被对冲，对冲连上、先交 1 个字节赢下：它接手的 chunk 仍是 `(1, 1)`，原来那一方成了被打败的一方，它晚到的帧被丢掉。
  - 失败的对冲找错伙伴：对冲失败时，原来按“任何一个记了第一方失败、没有伙伴的主人”去找留下的一方，有两对时会找到另一对。现在在 `fail_runner` 之前就定下另一方：主人的另一方是它的对冲，对冲的另一方是 `partner().runner_id` 等于它的那个主人。确定测试 `each_pair_keeps_its_own_first_failure`：两对同时存在，第一对的对冲以 `Timeout` 失败（没有自己的字节，记预算），第二对的以 `Congestion` 失败（都不），各自的原始一方记下各自的分类。
  - 再查一遍同类的问题，又找到一处：新开的对冲也是一个新连接，原来的“拥塞期间不开新连接”只查新的主人，不查新的对冲。现在 `check()` 也查：上一步没有的对冲，上一步不能处于拥塞中。另外 `Progress` 也可以挑被打败的一方，核对它晚到的帧被丢掉（原来生成器覆盖不到）。其它按主人记的状态没有问题：槽位的计数已经把未分胜负的对冲和正在停止的 runner 算进去了；`Finish` 只挑主人，没分胜负的一方不可能下完。
  - 变异检查：接管当新连接查（原来的预言），让 `a_hedge_that_wins_keeps_the_counters_of_its_chunk` 失败；按原来的方式找留下的一方，让 `each_pair_keeps_its_own_first_failure` 失败；产品接受被打败一方晚到的帧，让 `a_hedge_that_wins_keeps_the_counters_of_its_chunk` 和已有的 `the_hedge_never_moves_the_no_progress_deadline` 失败；产品在拥塞期间也对冲，让已有的 `no_hedge_without_a_free_connection_or_with_a_range_left` 失败（随机序列很少走到，新加的预言检查在 4096 个用例里没有碰上）。
  - 4096 个用例再跑一次：没有失败，5.7 s。计数：3700 次失败里，对冲一方 1 次，一对的原始一方 1 次，一对的最后一方 1 次，被打败的一方 0 次；接管 1 次；对冲连上、对冲交帧、被打败一方的晚帧都是 0 次。

### 14.23 验证器与 `If-Range`（PR-9b，C19）

- **范围。** C19 只做检测：带上 `If-Range`，认出资源变了，任务失败。从头重来移到引擎之上，见 #195。
- **决定。**
  - 维护者（2026-11-09，§3 的 D12、D13 已修订，D14 移到 #195）：
    - 引擎只检测，以 `DownloadError::ResourceChanged` 失败；
    - 不在任务内从头重来，不加 `BoltLoadAdapter::refresh`，也不加按请求传验证器的 `range_stream_if`；
    - 原因写在 #195：共享 adapter 的缓存、重来时的取消、重来的策略。
  - lead（实现时、codex 第一轮之后）：
    - 一个带 `If-Range` 的请求收到 `200`：比较 `200` 自己的验证器和发出去的那个，相同是 `RangeIgnored`（服务器忽略 Range，照旧回退），不同或没有是 `ResourceChanged`。不带 `If-Range` 的 `200` 仍是 `RangeIgnored`。
    - 一个带 `If-Range` 的请求收到 `206`，而 `206` 带的验证器和发出去的冲突（ETag 强比较不相等，或者发的是日期、`Last-Modified` 解析后不相等），是 `ResourceChanged`；`206` 没带验证器，照常接受。
    - 只用强 ETag；有合法的弱 ETag 时什么都不发，不退到日期（RFC 9110 §13.1.5：只有没有实体标签时才能用日期）。`Last-Modified` 离 `Date` 不到 1 s、晚于 `Date`，或者响应没有 `Date`，不能用于 `If-Range`（§8.8.2.2）。验证器按服务器发来的原样保存、原样发回。
    - ETag 按 §8.8.3 的完整文法检查；`ETag`、`Last-Modified`（还有判断要用的 `Date`）重复出现时当作没有。
    - 验证器只取自 GET：ureq2 也发一个无条件的 `bytes=0-0` GET，即使 HEAD 说支持 Range；reqwest 探测失败、回到 HEAD 时，用探测响应（2xx 时）的验证器，不用 HEAD 的。
    - `ResourceChanged` 从不走回退；teardown 照常删掉临时文件。
    - 提交是 `feat(core)!`：新的错误变体是加的，但行为变了：range 请求带 `If-Range`，资源变了任务失败，不再回退。
    - `ReplacedAfterShortRange` 不带验证器，继续测回退；带验证器的场景用新的 fault。
- **做法。**
  - core：`BoltLoadAdapterMeta` 增加 `etag`、`last_modified` 和 `with_etag`、`with_last_modified`。`with_etag` 丢掉弱 ETag，所以弱 ETag 时 `meta.etag` 是 `None`；`last_modified` 照样保存，即使它不能用于 `If-Range`（C20 比对状态文件时还用得上）。`UnretryableError::ResourceChanged` 是新变体（枚举本来就是 `#[non_exhaustive]`）。trait 不变。
  - `bolt-load-http` 的 `validator` 模块：
    - `Validator`（`StrongEtag`、`LastModified`，不是 `non_exhaustive`：`If-Range` 只有这两种）。
    - `is_strong_etag`、`strong_etag`：按 §8.8.3 的文法（`opaque-tag` 里没有转义，逗号和反斜杠是字面字符，引号结束标签，所以 `"a", "b"` 不是实体标签）。
    - `if_range_validator(etag, last_modified, date) -> Option<Validator>`：强 ETag；有弱 ETag 时 `None`；否则至少早于 `Date` 1 s 的 `Last-Modified`。解析不了的值当作没有。
    - `has_validator(sent, etag, last_modified)`：`200` 是否带着发出去的验证器（ETag 强比较，日期解析后比较，所以格式可以不同）。
    - `conflicts_with(expected, etag, last_modified)`：`206` 的验证器是否冲突：同一种、存在、能解析、不相等。
    - `RangeHeaders` 增加 `if_range: Option<&Validator>`（发出去的）、`etag`、`last_modified`（响应的；重复时传 `None`，合并成一个值也解析不了，结果一样）。`validate_range_response` 先查 `206` 的冲突（冲突时即使 `Content-Range` 不对也报 `ResourceChanged`），再按原来的规则检查，`200` 按上面的规则分类。
  - reqwest：缓存不变（`OnceCell<Context>`），`Context` 多存一个 `if_range: Option<Validator>`。`range_stream` 用取到的 context：带上 `If-Range`，把发出去的值和响应的验证器交给 `check_range`。`ETag`、`Last-Modified`、`Date` 只取单个字段行，重复或不是 UTF-8 时当作没有。
  - ureq2：缓存从 HEAD 响应改成 `Probed { meta, if_range, is_range_stream_available }`：HEAD（必须成功）加一个无条件的 `bytes=0-0` GET（可以失败），只探测一次。验证器来自 GET（2xx 时）；总长来自证明了偏移的 206 的 `Content-Range`，否则用 HEAD 的 `Content-Length`；文件名、编码来自 HEAD。能否用 Range：HEAD 有 `Accept-Ranges: bytes`，或者 GET 是证明了偏移的 206。`range_stream` 和 reqwest 一样。
  - 引擎：`download_loop/runner.rs` 的 `mode_ending`：`ResourceChanged` 变成 `DownloadError::ResourceChanged`，和回退的错误一样结束并发模式（一方失败、对冲一方失败都算），但不走回退；任务照常 teardown（删掉临时文件）后失败。那里有 `TODO(#195)`。`drive.rs` 不变。
- **共享 adapter。** 克隆的 builder 共享一个 adapter。没有 `refresh`，adapter 缓存的验证器取到之后就不变，所以共享它的任务发的是同一个验证器；资源变了，每个任务各自失败。一个新的任务用同一个 adapter 实例，拿到的还是旧版本的 meta 和验证器，会马上又失败：重来要换一个 adapter，或者等 #195 定下的办法。
- **已知限制。**
  - 单连接（包括回退）用 `full_stream`，没有 `If-Range`，看不出资源变了。
  - 服务器不发验证器时，大小不变的替换看不出来；`206` 不带验证器时照常接受，只能靠服务器正确执行 `If-Range`。
  - 检测要靠一次成功、带验证器的 `bytes=0-0` 探测。探测失败或被拒（reqwest 回到 HEAD 的路径也一样）时，adapter 缓存的 meta 没有验证器，之后它的 range 都不带 `If-Range`，在这个 adapter 的整个生命期里都看不出资源变了。测试：`a_rejected_probe_leaves_no_validator`、`a_rejected_probe_leaves_ranges_unconditional`。
  - adapter 只探测一次。`ResourceChanged` 之后用同一个 adapter（或者克隆的 adapter、builder）再下，拿到的还是旧的验证器，马上又失败；要换一个新的 adapter。README 和 `docs/adapter.md` 这样写，`a_later_task_on_the_same_adapter_keeps_the_old_validator` 记下现在的行为，#195 要改它。
  - 自定义 adapter 不发 `If-Range` 时，资源变了照旧不会被发现。
  - ureq2 取 meta 时多发一个 GET。
- **移到 #195。**
  - 从头重来，以及它和回退怎么组合（C19 草拟的规则记在 #195）。
  - T20 原来的验证“文件被替换 → 从头下载、hash 与新文件一致”；C19 改为验证任务以 `ResourceChanged` 失败、不留文件。
  - C18 留下的“重置之后不留旧区间”的测试（§14.21）。
  - 重来时的取消、共享 adapter 时每个任务的验证器（codex 第一轮的 Major 2、Major 4，Minor 6、Suggestion 9 也随之不再适用）。
- **测试。**
  - `bolt-load-http`：
    - `validator.rs` 的五个单元测试：文法（合法的逗号、反斜杠、obs-text；非法的列表、内部引号、空白）；选哪个验证器（含弱 ETag 不退到日期、合并的值）；相等、恰好 1 s、晚于 `Date`、没有 `Date`、三种日期格式；`has_validator`；`conflicts_with`。
    - `range.rs` 的 `a_200_with_another_validator_is_a_changed_resource` 和 `a_206_with_a_conflicting_validator_is_a_changed_resource`（冲突的 ETag、冲突的日期、没有验证器分开测）。
  - 两个 adapter 的单元测试 `validators_come_from_single_fields`（构造的响应）：没有 `Date`、重复的 `Date`、`Last-Modified`、`ETag`，一行里的列表，弱 ETag。
  - 测试服务器（`bolt-load-tests`）：`RangeFault::VALIDATED` 有 18 个 fault。每个响应的 `Date`、`ETag`、`Last-Modified` 来自同一个快照，`Date` 固定为 `VALIDATED_DATE`（不用 HTTP 栈加的当前时间）；判 `If-Range` 时日期解析后比较。
    - 原有：`ChangedMidDownload`、`ChangedThenIgnoresRange`、`IgnoreRangeWithValidators`、`WeakEtag`、`RecentLastModified`（改成 `Last-Modified` 等于 `Date`）。
    - 新增：`LastModifiedOneSecondOld`、`FutureLastModified`、`ObsoleteDateFormats`（`Date` 用 asctime，`Last-Modified` 用 RFC 850，探测以外的 206 用 IMF-fixdate）、`ChangedIgnoringIfRange`、`ChangedByDateIgnoringIfRange`（忽略 `If-Range`，用新版本的 206 回答，大小不变）、`PartsWithoutValidators`、`HeadValidatorsDiffer`、`HeadValidatorsDifferThenChanged`、`IgnoreRangeWithHeadValidators`、`RepeatedEtag`、`ListEtag`、`RejectedProbe`（`bytes=0-0` 探测得到 403）、`HeadSizeDiffers`（HEAD 的 `Content-Length` 多 16 字节）。
    - 单元测试 `validated_dates_in_every_format`。
  - `bolt-load-adapter/tests/if_range.rs`，两个 adapter 都跑：
    - 强 ETag 进 `If-Range`，变了之后是 `ResourceChanged`。
    - 弱 ETag 什么都不发；`Last-Modified` 等于、晚于 `Date` 不发，恰好早 1 s 发，旧格式原样发回、和 IMF 格式的 206 不冲突；重复的、列表的 ETag 退到日期。
    - 忽略 Range、验证器相同（HEAD 的验证器不同也一样）是 `RangeIgnored`；HEAD 和 GET 的验证器不同时发 GET 的。
    - 冲突的 206（ETag、日期）是 `ResourceChanged`，没有验证器的 206 通过。
    - 总长来自证明了偏移的 206，不用 HEAD 说的（`the_total_comes_from_the_validated_206`）。
    - 探测被拒：没有验证器，range 不带 `If-Range`（`a_rejected_probe_leaves_no_validator`）。
  - `bolt-load/tests/resource_changed.rs`，三种客户端都跑，每次都查临时文件已经删掉。`sent` 列出每个数据 range（探测以外）的 `If-Range`，没带的是 `None`，所以漏发也看得出来：
    - 下载途中被替换（HEAD 的验证器不同也一样）：以 `ResourceChanged` 失败，不留文件，发过的 `If-Range` 只有 `"v1"`。
    - 变了之后忽略 Range：同样失败，不回退。
    - 忽略 `If-Range`、发新版本的 206（按 ETag、按日期）：同样失败，不拼两个版本；没有验证器的 206，一次下完。
    - 忽略 Range、验证器相同（HEAD 的不同也一样）：照常回退，hash 一致。
    - HEAD 和 GET 的验证器不同的稳定服务器：一次下完、只发 `"v1"`。
    - 探测被拒：一次下完，每个 range 都不带 `If-Range`。
    - 两个克隆的 builder 共享一个 adapter：只探测一次，两个任务都下完，每个 range 都带 `"v1"`。
    - 同一个 adapter 上，变了之后再开一个任务：仍然带旧的 `"v1"`，马上以 `ResourceChanged` 失败，只探测一次（记下 #195 要改的行为）。
  - `runner.rs` 的 `a_changed_resource_ends_the_mode_without_a_fallback`。
- **变异检查**，全部被抓到：
  - 弱 ETag 也发：`validator.rs` 的测试、两个 adapter 的 `validators_come_from_single_fields`、`a_weak_etag_sends_no_validator`；
  - 弱 ETag 退到日期：`validator.rs` 的测试、两个 adapter 的 `validators_come_from_single_fields`；
  - 验证器相同的 `200` 也当成变了：`range.rs` 的测试、`a_200_with_the_validator_sent_ignores_the_range`、`the_validator_of_a_get_is_sent_not_that_of_a_head`、`ignored_ranges_with_the_same_validator_fall_back`；
  - 忽略 `206` 的冲突：两个 `a_206_with_a_conflicting_validator_is_a_changed_resource`；
  - 没有验证器的 `206` 也当成冲突：`range.rs`、`validator.rs` 的测试、`parts_of_another_version_are_not_mixed_in`；
  - 日期按字符串比较：`range.rs`、`validator.rs` 的测试、`a_last_modified_a_second_before_date_is_sent`；
  - 文法接受任意内容：`validator.rs` 的测试、两个 adapter 的单元测试；
  - reqwest 取重复字段的第一个：`validators_come_from_single_fields`；
  - ureq2 用 HEAD 的验证器：`the_validator_of_a_get_is_sent_not_that_of_a_head`、`head_validators_that_differ_from_a_get_are_not_sent`、`a_resource_changed_mid_download_fails_the_task` 等六个；
  - reqwest 回到 HEAD 时用 HEAD 的验证器：`a_200_with_the_validator_sent_ignores_the_range`、`ignored_ranges_with_the_same_validator_fall_back`；
  - `ResourceChanged` 走回退：`a_changed_resource_ends_the_mode_without_a_fallback`、`a_resource_changed_mid_download_fails_the_task`、`a_changed_resource_that_ignores_ranges_does_not_fall_back`、`parts_of_another_version_are_not_mixed_in`。

### 14.24 测试服务器绑定系统分配的端口（测试设施）

- **问题。** `bolt-load-tests` 的两个测试服务器（`create_http_server_with_file_size`、`create_range_fault_server_with_log`）先用 `portpicker::pick_unused_port()` 挑一个空闲端口，再去绑定它。两步之间别的进程（包括并行跑的其他测试）可以先占走这个端口，绑定就失败：Windows 上是 `os error 10048`。C19 的完整测试里出现过几次，和改动无关。
- **做法。** 新函数 `bind_local()` 绑定 `127.0.0.1:0`，由系统在绑定时分配端口，再从 `local_addr()` 读出端口号。两个服务器都用它，公开函数的签名不变（仍返回端口号）。`portpicker` 没有别的用处，从 `bolt-load-tests` 的依赖里去掉，`Cargo.lock` 随之少了 `portpicker` 和它带来的 `rand 0.8`、`rand_chacha 0.3`、`rand_core 0.6`。
- **其他地方。** 整个工作区里没有别的先挑后绑：`tests/empty_body.rs` 和 `bolt-load-adapter/tests/body_decoding.rs` 本来就绑定端口 0，没有写死的端口。
- **验证。** `if_range` 和 `resource_changed` 各连续跑 40 次，没有失败，没有 10048；完整的工作区测试（全部 feature 782 个、默认 714 个）也没有。

### 14.25 恶意文件名测试忽略外部扫描器的副本（测试设施）

- **问题。** `test_save_into_rejects_hostile_filenames` 断言下载目录里只有 `evil.bin`。在一台测试机上，30 次里有 2 次目录里还多了一个 `EVIL.BIN.DOWNLOADING.tmp`，紧跟在 `evil.bin.downloading` 改名成 `evil.bin` 之后出现。bolt-load 里没有任何代码构造这个名字：临时文件只有 `<目标>.downloading`（小写，没有 `.tmp`），由 `async_fs::rename`（直接调 `std::fs::rename`，Windows 上是 `MoveFileExW`，不另造临时名）发布或 `remove_file` 删掉；这个测试用的是内存里的 `SimpleTestAdapter`，不起 HTTP 服务器；tempfile 的名字是 `.tmp<随机>`。所以怀疑是机器上的外部程序，比如杀毒软件（那台机器上运行着 Kaspersky）。这只是推测：观察到的现象不能说明是哪个进程建的文件，证实需要排除目录或 Process Monitor 跟踪。
- **做法。** 测试先断言 `evil.bin` 存在、内容（hash 和长度）正确，`evil.bin.downloading` 不存在，`temp_dir` 里没有逃出去的 `evil.bin`；再列目录，只在 Windows 上去掉名字（不分大小写）恰好等于 `evil.bin.downloading.tmp` 的项，也就是观察到的那一个；剩下的必须恰好是 `["evil.bin"]`。不用 `*.tmp` 通配，也不放过没观察到的名字（评审建议，lead 采纳）。
- **会不会掩盖真问题。** 只有产品自己在 Windows 上写出 `evil.bin.downloading.tmp`（文件或目录）时才会被掩盖；产品从不构造 `.tmp` 名字。其它平台上不放过任何多余的项。别的任何多余文件仍然让测试失败。`resource_changed.rs` 本来就只查产品自己的文件名（`out.bin`、`out.bin.downloading`），不用改。

### 14.26 目标路径的锁（PR-9c，C31）

- **范围。** §11.5.13 里 C31 的部分：每个任务（现在都是 `Resume::Off`）在整个任务期间对 `<dest>.boltload` 加独占锁，修 #152。续传的阶段（B1、B2、C）留给 C33。
- **依赖。** `fs4` 1.1.0（`default-features = false, features = ["sync"]`，`sync` 就是它的默认 feature，所以只拿到同步的 API；MSRV 1.75；依赖 `rustix` 1、`windows-sys` 0.61，`Cargo.lock` 里都已经有，只新增 `fs4` 一项）。`same-file` 1（已在依赖树里）做文件身份的核对。Unix 上直接依赖 `libc`（已在依赖树里）取“不支持锁”的错误码；Windows 的几个错误码写成常量，不扩大 `windows-sys` 的 feature。`File::try_lock` 和 `File::unlock` 用 `FileExt::try_lock(&file)` 这种写法调用：标准库的同名方法要 1.89，本仓库 MSRV 是 1.88。
- **加锁**（`task/instance/destination_lock.rs`）：
  1. 先 `create_new` 打开（读写），已存在就打开已有的，记下 `created`；打开时被删掉了（`NotFound`），或者 Windows 上共享冲突（`ERROR_SHARING_VIOLATION`）、删除挂起（`ERROR_DELETE_PENDING`，或者 `ERROR_ACCESS_DENIED` 而路径已经查不到元数据），就等 20 ms 重来。`ERROR_ACCESS_DENIED` 而元数据还查得到的，是真的没有权限，报 `Io { op: Lock }`（第一轮审查 Minor 5）。元数据也可能因为没有权限而查不到，所以这个判断会错：重试用完时，最后一次打开如果还是这种被当作删除挂起的拒绝，报 `Io { op: Lock }` 和那个错误，不报 `InUse`（第二轮审查 Minor 2）。没有用 NT 状态码区分，那要多一个依赖。
  2. `try_lock`：争用 → `DownloadError::InUse { path: dest }`；不支持锁（Unix `ENOLCK`、`EOPNOTSUPP`、`ENOTSUP`，Windows `ERROR_NOT_SUPPORTED`、`ERROR_INVALID_FUNCTION`，或 `ErrorKind::Unsupported`）→ 现在的任务打 `warn!`、不加锁继续（`Unsupported::Fail` 留给 C33 的 `On`）；其它错误 → `Io { op: IoOp::Lock, path: <dest>.boltload }`。两种情况都**不删**锁文件，即使是自己刚建的：没拿到锁，它这时可能已经是别的任务的了（第一轮审查 Major 1）；留下一个空文件，下一个任务当作没有状态。
  3. 核对：`same_file::Handle` 比较锁住的句柄和路径现在指向的文件；路径指向别的文件或者没有文件就放锁、重来，最多 3 次，之后按 `InUse`；查询本身出错（句柄复制、元数据）报 `Io { op: Lock }`，不当作争用（第一轮审查 Minor 6）。
  4. 锁住之后，已有内容的锁文件经锁住的句柄清空（`set_len(0)` + `sync_all`），在动临时文件之前。空的（崩溃留下的）直接用。
  - 状态文件只经这个句柄读写；结束时先删路径、再放锁（`DestinationLock::release`），只在对临时文件的最后一次操作之后调用。
- **所有权**（`task/instance/ownership.rs`）：`drive::spawn` 在 `catch_unwind` 之外建一个 `Ownership`，记着锁、临时文件和阶段（`Unowned`、`Transfer`、`Published`，即设计的 A、D、P），还有一个 drain 用的 `WaitGroup`。
  - `run` 在解析出目标路径之后加锁：加锁和清空不和取消赛跑（`blocking::unblock` 之后才看取消），锁一拿到就交给 `Ownership`；之后若已取消，先删掉临时文件（这时它已归这个任务，可能是早先的任务留下的；删不掉报 `leftover_temp_file`），再放锁、以 `Cancelled` 结束（第一轮审查 Minor 4）。
  - 正常结束（成功、失败、取消、回退之后）：teardown 完成后 `release`，再发布终态。`release` 在同一次加锁里先交出临时文件（阶段回到 `Unowned`、清掉临时路径），再删锁文件、放锁：之后再有 panic，清理什么都不动，不会删掉后来的任务的临时文件（第一轮审查 Major 2）。
  - `OutputFile::commit` 改名成功后把阶段设为 `Published`，并记下完成时的进度（`commit` 收一个 `completed`，由传输在 commit 之前算好）；这个进度在 `release` 之后也保留（第一轮审查 Minor 7）。
  - 并发模式的 writer 的每个 worker（pool、mmap、compio）都持有一个 drain 的克隆，关掉文件之后才放下（`FileWriter::new_drained`）。
  - 先关文件、再放 drain 的顺序在每条路径上都成立（第二轮审查 Major 1）：闭包各自捕获的值，销毁顺序没有规定（Rust reference，Destructors），只有正常返回时的显式 `drop` 才排得了序，worker panic 或者排队的任务没运行就被丢掉时不行。所以用 `Drained<T> { value, _drain }`（`drained_file.rs`）把文件和 drain 的克隆放在一个结构体里，字段按声明顺序销毁；闭包整个拿走它，先写 `let held = held;`，不按字段分开捕获。pool 和 mmap 的 worker 套两层，`Drained::new(Drained::new(file, worker_guard), drain)`，依次放下文件、worker 自己的 guard、drain。compio 的 worker 在 `compio_file_writer_task` 里自己开关文件，那一帧先于闭包的捕获展开，不用改。
  - 单连接模式（第一轮审查 Major 3）：原来的 `async_fs::File` 把写交给一个后台 worker，attempt 没了之后它还拿着文件，而且没有办法知道它什么时候关掉。改成 `task/instance/drained_file.rs` 的 `DrainedFile`：每次操作（建、`set_len`、写）各是一个 `blocking` 任务，持有文件和一个 drain 的克隆，先放文件、再放克隆；写要等到真正写进文件才返回，所以没有缓冲、也不再需要 `flush`（单连接的写错误都是 `Io { op: Write }`）。unwind 丢掉一个正在执行的写的 future 停不下它，但它持有 drain，panic 处理会等它写完、放下文件。
  - panic：`report_panic` 先取消，再 `wait_blocking` 等 drain，然后按阶段：`Unowned` 什么都不动；`Transfer` 删临时文件（删不掉报 `leftover_temp_file`）；之后放锁。改过名的（有记下的完成进度）终态发布 `Finished` 和那个进度，否则 `Failed(Panicked)`。等待没有上限（Q2）。清理和 `release` 不能并发：清理先读阶段和临时路径、再删文件。现在不会并发，因为每个 `release` 都在 `run` 里、由同一个线程的 `block_on` 执行，`report_panic` 在 `catch_unwind` 返回之后才跑（第二轮审查建议 5，写在 `clean_up_after_panic` 的文档里）。
  - 状态发布器不再记临时文件（`StatusPublisher::temp_file` 删掉），panic 时由 `Ownership` 负责。
- **错误。** `DownloadError::InUse { path }`（目标路径）和 `IoOp::Lock` 都是新的（两个枚举本来就是 `#[non_exhaustive]`）。目录不可写时，原来以 `Io { op: Create }` 报临时文件，现在先以 `Io { op: Lock }` 报锁文件。
- **测试。**
  - `destination_lock.rs` 的单元测试：第二把锁 `InUse`、放锁后可再拿；旧内容被清空；崩溃留下的空文件被接管；打开和加锁之间文件被换掉，重新打开并锁住新的；每次都被换掉，最后 `InUse`；不支持锁时不加锁继续，`Unsupported::Fail` 时 `Io { op: Lock }`，都留下空的锁文件；权限错误两种都 `Io { op: Lock }`；目录不存在 `Io { op: Lock }`；A 建了文件、B 先锁住、A 加锁出错（权限、不支持锁各一次，用 `Barrier` 控制先后）：A 不删 B 的文件，C 仍然 `InUse`；身份核对：路径指向别的文件或没有文件是 `false`，核对出错时 `Io { op: Lock }`、锁照样放掉；Windows 上 `ERROR_ACCESS_DENIED` 按元数据分辨删除挂起和真的没有权限。
  - `ownership.rs`：panic 之后，worker 退出之前锁一直被持有、临时文件还在，之后才删、才放；按阶段清理（`Unowned` 不动，`Published` 不删临时路径、带回完成进度）；`release` 之后再清理，后来的任务的临时文件和锁都不受影响。
  - `drained_file.rs`：一个正在执行的写（钩子挡住），它的 future 和文件都被丢掉（模拟 unwind），drain 在写完之前不结束（直接 poll `drain.wait()`，不靠另一个线程有没有跑到），写完之后结束、数据在文件里；`Drained` 在闭包 panic 和闭包没运行就被丢掉两种情况下，值都先于 drain 的克隆放下（值在自己的 `Drop` 里记下 drain 是否还开着，只记不断言，免得 unwind 中二次 panic）。
  - `drive.rs`：下载中 panic → `Failed(Panicked)`、临时文件和锁文件都没了；改名之后 panic → `Finished`、进度就是记下的完成进度、目标文件不动。
  - `concurrent_task`：writer 的 worker 持有 drain，`finalize` 之前等待不会结束；`file_writer.rs` 对每种 writer（pool，以及启用时的 mmap、compio）各测一次，因为按 feature 选出来的只有一种。
  - `task/tests/destination_lock.rs`（第一个任务停在适配器的门上，由测试放行，不靠时间，第一轮审查建议 8；它的流空闲超时设成 1 小时，超过所有 watchdog，不会自己超时放锁，第二轮审查 Minor 3）：#152 的场景（第二个任务以 `InUse` 失败、不碰它的临时文件，放行后第一个照常完成、内容正确）；成功、失败、取消、panic 之后锁文件和临时文件都没了，下一个任务能跑；旧的状态和临时文件被清空、从头下载（停在门上时锁文件已是空的）；崩溃留下的空锁文件被接管；加锁时取消，锁照样放掉、早先留下的临时文件被删掉、`leftover_temp_file` 为空；不支持锁时照常下载、留下空锁文件；权限错误 `Io { op: Lock }`、什么都没写。
  - `tests/destination_lock.rs`（跨进程）：测试二进制以环境变量再起一个自己，子进程的 body 一直等它的标准输入（父进程掌握），进入 `Downloading` 时打印标记；父进程的任务 `InUse`；杀掉子进程后，父进程的任务接管目标路径（子进程留下的锁文件和临时文件），下载正确、不留锁文件。
- **变异检查**，全部被抓到：
  - 从不加锁：单元测试的第二把锁、文件被换掉，`ownership` 的等待，#152 的场景，跨进程；
  - 去掉文件身份的核对：两个“文件被换掉”的单元测试；
  - panic 时先放锁、再等 worker：`test_the_lock_waits_for_the_workers`；
  - 不清空旧状态：`test_a_stale_state_is_invalidated`（下载中锁文件必须是空的）；
  - 在对临时文件的最后一次操作之前删锁文件、放锁：#152 的场景、旧状态、跨进程。
  - 第一轮审查之后加的：加锁失败时删掉自己建的锁文件（`test_a_failed_lock_leaves_the_holder_s_file`、`test_other_errors_fail`）；`release` 不交出临时文件（`test_a_cleanup_after_the_release_touches_nothing`）；单连接的写在执行完之前放下 drain（`test_a_dropped_write_holds_the_drain_until_it_ends`）；加锁之后取消不删临时文件（`test_cancelled_while_locking`）；改名之后 panic 用最后一次采样的进度（`test_a_panic_after_the_rename_finishes_the_task`）；身份核对出错当作“别的文件”（`test_an_identity_error_is_an_error`，测试用 `LockHooks::identity` 注入错误）。
  - 第二轮审查之后加的：`Drained` 的两个字段换序（`test_a_drained_value_drops_before_its_drain`）；重试用完在“当作删除挂起的拒绝”上仍报 `InUse`（`test_running_out_on_an_access_denial_is_an_error`）；pool、mmap、compio 的 worker 各自不持有 drain（`test_every_writer_kind_holds_the_drain_group`）。
- **没覆盖的。** Windows 上真实的 ACL 拒绝和删除挂起的文件，测试只用构造的错误码检查分类。

### 14.27 meta 带上 `If-Range` 的验证器（PR-9d，C34）

- **范围。** §11.5.13 里 C34 的部分（§11.5.7 的第一条）：`Validator` 移回 core，`BoltLoadAdapterMeta` 增加 `validator`，两个 adapter 探测时设置它，range 请求的 `If-Range` 和 `check_range` 也从它取。任务引擎还没有用这个字段做持久化或续传，C33 写状态文件、C20 续传比对时才用。
- **移动了什么。**
  - `Validator`（`StrongEtag`、`LastModified`，`as_str`，derive 不变）从 `bolt-load-http/src/validator.rs` 移到 `bolt-load-core/src/adapter/validator.rs`，路径是 `bolt_load_core::adapter::Validator`，`bolt_load::adapter` 的 glob re-export 也带上它。`bolt-load-http` 的 `validator` 模块 `pub use` 它，`bolt_load_http::Validator` 照旧可用。
  - core 只多了比较：`Validator::matches(&other)`，给 C20 比对头部存的验证器和新 meta 的（§11.5.7）：种类相同；强 ETag 逐字节相同；`Last-Modified` 解析成日期后相同，所以格式可以不同；解析不了的日期只和同一个字符串相同（自反）。`==`（derive 的）仍按字符串比较，文档写明两者的区别。
  - 解析和挑选（`is_strong_etag`、`strong_etag`、`if_range_validator`、`has_validator`、`conflicts_with`、`RangeHeaders`）留在 `bolt-load-http`：core 用不到它们，D11 把 HTTP 头的处理放在 http。
  - 依赖：core 新增 `httpdate = "1"`（日期比较要用；没有依赖，`Cargo.lock` 里已有，只多了 core 指向它的一行）。
- **meta 字段。** `BoltLoadAdapterMeta::validator: Option<Validator>`：adapter 在 `If-Range` 里实际发的那个，种类和值；`None` 表示没有可用的、range 不带 `If-Range`。`with_validator` 照原样保存，不再过滤。`new` 设成 `None`。
- **一个来源。** 两个 adapter 删掉了缓存里单独的 `if_range`（reqwest 的 `Context::if_range`、ureq2 的 `Probed::if_range`）。`with_validators` 只返回 meta，用 `if_range_validator` 算出的值调用 `with_validator`；`range_stream` 从缓存的 `meta.validator` 取 `If-Range` 和交给 `check_range` 的值。所以 meta 报的和 range 发的是同一个值，不会各算一次。探测被拒（reqwest 回到 HEAD 而探测不是 2xx、ureq2 的 GET 失败）时不调用 `with_validators`，`validator` 是 `None`，range 也不带 `If-Range`。
- **兼容性。** 不破坏：结构体本来就是 `#[non_exhaustive]`，加字段不影响下游；`Validator` 的旧路径靠 re-export 保留，定义和 derive 不变；新增的 `with_validator`、`Validator::matches` 都是加的。没有公开项改名或改签名（adapter 里改掉的 `with_validators`、`Context`、`Probed` 都是私有的）。提交是 `feat(core)`，不带 `!`。
- **测试。**
  - core `validator.rs` 的四个单元测试：`as_str`；强 ETag 逐字节（大小写、尾随空白都不同）；日期按日期比较（三种格式互相相同，另一个日期、解析不了的不同，解析不了的只等于自己）；种类不同不相同（同样的值也不相同）。
  - http `validator.rs` 原有的五个测试不变，照常通过。
  - 两个 adapter 的 `validators_come_from_single_fields` 改成检查 `meta.validator`。
  - `bolt-load-adapter/tests/if_range.rs`，两个 adapter 都跑：
    - 新增 `the_meta_names_the_validator_sent`：强 ETag（`HeadValidatorsDiffer`，HEAD 的不同，meta 是 GET 的 `"v1"`）；只有 `Last-Modified`（`LastModifiedOneSecondOld`，以及 `ObsoleteDateFormats`，RFC 850 格式原样保存）；弱 ETag、`Last-Modified` 等于或晚于 `Date`、探测被拒都是 `None`。每种情况发两个 range，每次的 `If-Range` 都等于 `meta.validator` 的值（`None` 时不带）。
    - `a_200_with_the_validator_sent_ignores_the_range` 加查 `meta.validator` 是 `"v1"`（reqwest 回到 HEAD 的路径）；`a_rejected_probe_leaves_no_validator` 加查 `None`。
    - codex 第一轮之后加的：`a_cloned_adapter_shares_the_validator`：探测之前克隆的 reqwest adapter 和原来的共享探测，报同一个 `meta.validator`、发同一个 `If-Range`，只探测一次。`UreqAdapter` 不是 `Clone`，只能经 `Arc` 共享同一个实例，没有可测的克隆。`a_failed_range_request_keeps_the_validator`（两个 adapter）：测试的钩子（reqwest 的 `before_request` 改端口，ureq2 的 `call` 改发到别处）把一个 range 发到一个接受连接后立刻关掉的端口，得到 `Retryable`；之后 `meta.validator` 不变，重发的 range 带同一个 `If-Range`，钩子看到两次都是 `"v1"`，只探测一次。不靠时间：失败由测试切换，关连接的端口一直在监听。
- **变异检查**（`--all-features` 跑 core、http、adapter 三个 crate 的全部测试），全部被抓到：
  - core 的 `with_validator` 丢掉值：`the_meta_names_the_validator_sent`、两个 `validators_come_from_single_fields` 和 `if_range.rs` 的另外七个（`If-Range` 也跟着没了）；
  - reqwest、ureq2 各自把 meta 的验证器设成 `None`：同上，各自的单元测试；
  - meta 的验证器取自 HEAD 而不是 GET：reqwest 回到 HEAD 的路径被 `a_200_with_the_validator_sent_ignores_the_range` 抓到；ureq2 被它、`the_meta_names_the_validator_sent`、`the_validator_of_a_get_is_sent_not_that_of_a_head` 抓到。meta 和 `If-Range` 同源，所以“只有 meta 取自 HEAD”这种变异已经写不出来；
  - 弱 ETag 当成验证器：`the_meta_names_the_validator_sent`、`a_weak_etag_sends_no_validator`、两个 `validators_come_from_single_fields`、http 的 `test_the_validator_for_if_range`；
  - `matches` 把日期当字符串比较：`test_dates_match_as_dates`；
  - `matches` 的 ETag 不分大小写：`test_entity_tags_match_byte_for_byte`；
  - `matches` 不看种类：`test_another_kind_does_not_match`。
  - codex 第一轮之后加的：reqwest 的克隆不共享探测（`clone` 时换一个新的 `OnceCell`）：`a_cloned_adapter_shares_the_validator`；range 请求失败时清掉缓存的探测（ureq2 的 `range_stream` 出错时把 `probed` 置空）：`a_failed_range_request_keeps_the_validator`。另外重跑了“meta 的验证器取自 HEAD（reqwest 回到 HEAD 的路径）”，加了断言之后照样被 `a_200_with_the_validator_sent_ignores_the_range` 抓到。
- **自定义请求。** adapter 的 `before_request`（ureq2 还有 `call`）在 adapter 设好 `If-Range` 之后执行，可以改掉它；reqwest 客户端的默认头也可以在 `meta.validator` 是 `None` 时带上一个。响应的检查总是用缓存的 `meta.validator`，不看实际发了什么，所以这样的自定义会让另一个版本的 range 被当成这一个，或者让这一个被当成 `ResourceChanged`。行为不变（C19 起就是这样），只在两个 `before_request` 的文档和 adapter 的 README 里写明：自定义不能替换 adapter 设的 `If-Range`，也不能在没有验证器时加一个（codex 第一轮的建议）。
- **没做的。** 任务引擎还没有用 `meta.validator` 做持久化或续传：状态文件的头部（C33）和续传的比对（C20）以后用它。`TaskBuilder::meta` 交进来的 meta 里的 `validator` 只是调用方说的，adapter 发的 `If-Range` 仍来自它自己探测的 meta；C20 比对时要用 adapter 取的新 meta，见 §11.5.7。

### 14.28 检查点：块的 CRC 与数据同步（PR-9d，C32）

- **范围。** §11.5.13 里 C32 的部分：writer 的 `sync`（三个后端）；写入带 CRC 段；按块的 CRC 拼接、上限和回读；已记录的块（`durable`）；串行的检查点调度（8 MiB / 2 s / teardown）和回滚；`TaskOptions` 的两个检查点字段。接收者是 crate 内部的接口，真正写状态文件的接收者在 C33；现在没有任何任务交出接收者，所以生产路径上不调度、不算 CRC、不同步，行为和性能都不变。单连接模式不在 C32：§11.5.8 规定单连接（包括回退之后）从不写状态，`On` 的单连接改走并发引擎是 C33 的事。
- **依赖。** `crc32c`，`Cargo.toml` 写 `0.6.8`，`Cargo.lock` 用 `cargo update --precise` 锁在 0.6.8。核对（只有 crates.io 的 API 一个来源）：仓库 `zowens/crc32c`，MIT/Apache-2.0，下载约 9900 万次；0.6.8 发布于 2024-06-09，0.6.9 发布于 2026-11-02（实现时只有 8 天，没选它，以后再升）；没有运行时依赖，只有构建依赖 `rustc_version`（`Cargo.lock` 里已有）。提供 `crc32c`、`crc32c_append` 和 `crc32c_combine(crc1, crc2, len2)`；x86_64 用 SSE4.2，aarch64 用 CRC 指令，其余平台软件实现（§11.5.5）。本机（release）对内存里 256 MiB 算一次约 14 ms，约 17–18 GiB/s。
- **writer 的 `sync`。** `FileRangeWriter::sync(&self)`：同步到目前为止已完成的写的数据，之后照常写。
  - pool：`Command::Sync` 经队列交给一个 worker，它 `sync_data` 自己的句柄（worker 共用同一个文件，一次同步覆盖所有句柄的写）；其它 worker 照常写。第一版用克隆的句柄在单独的 `blocking` 线程上同步，第三轮审查之后改掉，见下。
  - mmap：新命令 `Command::Sync`，唯一的 worker `mmap.flush()`：Unix 上 `msync(MS_SYNC)`，Windows 上 `FlushViewOfFile` 再 `FlushFileBuffers`（memmap2 0.9.12 的实现，已核对源码）。同步期间这个 worker 不写，见下面的基准。
  - compio：`Command::Sync`，worker `sync_data`；已完成的写都已经落进文件。
  - null：经队列回一个成功；slow（测试）：`sync_data`；recording（测试）：记次数，可以注入失败，并记下同步时已写的区间。
  - trait 的默认实现返回 `Unsupported` 错误（不是成功），所以不会同步的 writer 永远不会让检查点记下没同步的数据。新错误 `FileWriterError::Sync`，`op()` 归为 `IoOp::Flush`；它只进 `warn!`，不让任务失败。
- **每帧的 CRC 段。** `WriteCompletion` 加 `crcs: Vec<CrcSegment>`（`CrcSegment { offset, len, crc }`，`file_writer/crc.rs`）。`PendingWriter::checksum(block_size)` 打开之后，`CompletionGuard` 在写成功时对这一帧（planner 裁剪之后真正写入的字节）算 CRC32C，在块边界处拆成几段；失败的写（包括写了一部分再出错的）、被丢弃或 panic 的写一段都不带。CRC 在后端的 worker 线程（直接提交）或写任务里算，不在下载循环里算。没有打开时一段都不算。
- **按块的段**（`checkpoint/blocks.rs` 的 `BlockCrcs`）：每个没写满的块一组按偏移排好、互不重叠也不相邻的段；成功的写到达时和左右相邻的段用 `crc32c_combine` 合并。上限：每块 64 段、合计 16384 段；超过时这一块（合计超了就挑段最多的块）丢掉段、改为回读，之后这块的段不再保存。块写满时取出它的 CRC：只剩一段且覆盖整块 → `Known(crc)`；回读 → `ReadBack`。段重叠、写满的块不是恰好一段，都只有 bug 才会出现：`debug_assert!` 抓，生产路径退回到回读（结果仍然正确）。
- **检查点**（`checkpoint.rs` 的 `Checkpoints`，由 `DownloadLoop` 持有）：
  - 写完成时 `note_written(range, crcs)`：段交给 `BlockCrcs`，碰到的块记为“待查”，累计写入字节。
  - 调度：距上一次检查点开始写了 `checkpoint_bytes`（默认 8 MiB），或过了 `checkpoint_interval`（默认 2 s），而且有新完整的块（全部字节在 `written` 里、还没记过）时开始一次。在写完成、检查点结束和策略节拍（1.5 s）时检查，没有单独的计时器：没有新的写完成，就没有新完整的块（实现者的决定）。没有完整的块时不开始、不同步。
  - 串行：同一时刻最多一个，正在运行时到期的那次等它结束之后再开始（接收者在运行中的检查点手里，结构上也开不了第二个）。检查点的 future 由循环在 writer 之后、策略节拍之前轮询，结束时是 `LoopEvent::Checkpoint`。
  - 一次检查点（§11.5.5 的顺序）：先取出完整的块和它们的 CRC → writer `sync` → 在 `blocking` 线程上为回读的块从部分文件读出整块算 CRC（`BlockReader`，生产是 `FileBlocks(tmp_path)`）→ `receiver.record(blocks)`。成功：这些块进 `durable`（按块号的区间集合）。`sync`、回读或 `record` 失败：调用 `receiver.roll_back()`；成功则块回到“完整未记录”，带着取出时的 CRC，下一次到期的检查点再记，打 `warn!`；回滚也失败则这个任务不再做检查点（丢掉接收者和所有段），打 `warn!`。`sync` 失败也回滚（§11.5.5：第 2 或第 3 步失败都回滚）。
  - 接收者接口：`CheckpointReceiver { record(&mut self, &[CheckpointBlock]) -> io::Result<()>; roll_back(&mut self) -> io::Result<()> }`，`CheckpointBlock { index, len, crc }`，在 `blocking` 线程上一次一个地调用。追加位置 `end`、`set_len(end)` 和 `fsync` 由 C33 的实现负责。
  - teardown：`DownloadLoop::finish` 冲刷 writer（冲刷里完成的写照样带 CRC）之后，先等正在运行的检查点，再把剩下的完整块记一次，到期与否都记（§11.5.9 的“teardown 先做一次检查点”）。成功、失败、取消都做；`ResourceChanged` 只等正在运行的那次、不开新的，数据属于旧版本（§11.5.9 那一行是丢弃；第一轮审查之后的修改，见下）；回滚失败之后不做。
  - 接入：`ConcurrentTaskInner::checkpoint_to(receiver)`（crate 内部，C33 调用，现在只有测试调用）；`download_to` 有接收者时建 `Checkpoints` 交给 `DownloadLoop::with_checkpoints`，后者打开 `PendingWriter` 的 CRC。
- **`TaskOptions`。** 新增 `checkpoint_bytes: NonZeroU64`（默认 8 MiB）和 `checkpoint_interval: Duration`（默认 2 s，`validate` 拒绝 0），各有 setter，`docs/task.md` 的表里列出。结构体本来就是 `#[non_exhaustive]`。
- **兼容性。** `TaskOptions` 的字段是加的。`file_writer` 模块为了基准是公开的：`WriteCompletion` 加了公开字段 `crcs`，`FileWriterError`（不是 `non_exhaustive`）加了变体 `Sync`，`FileRangeWriter` 加了带默认实现的 `sync`，`CrcSegment` 是新的。在外面构造 `WriteCompletion`、或者穷举匹配 `FileWriterError` 的代码会编译失败；crate 里的基准不受影响。按 §0 第 3 条算破坏兼容，提交是 `feat(task)!`（lead 决定，codex 第一轮同意）。
- **基准。** C13 的基准（`growth_strategy`）走公开的 `Task` API，续传公开（C20）之前打不开检查点，所以 C32 没有给它加组（实现者的决定；C20 之后可以加 `Resume::On` 的一组）。改用一个 `#[ignore]` 的测试直接驱动引擎：`concurrent_task/checkpoint_bench.rs` 的 `checkpoint_overhead`，`cargo test -p bolt-load --release --lib -- --ignored --nocapture checkpoint_overhead`（默认 feature，256 MiB 用 mmap writer）。进程内 `SimpleTestAdapter` 不限速、8 条连接、临时目录里的真实文件，从开始到 writer 同步完成计时；“打开”的接收者每次检查点把 24 字节的记录追加到另一个文件并 `sync_all`，模拟 C33。三组交替运行、各 7 次（先各热身一次），中位数 / p90：

  | 组 | 中位数 | p90 | 吞吐（中位数） | 比关闭 | 检查点、记录（每次） |
  | --- | --- | --- | --- | --- | --- |
  | 关闭（不算 CRC，只有最后一次同步） | 0.315 s | 0.333 s | 813 MiB/s | — | 0、0 |
  | 只在 teardown（每次写算 CRC，最后多一次检查点） | 0.353 s | 0.378 s | 725 MiB/s | +12% | 1、256 |
  | 默认（每 8 MiB 或 2 s） | 0.504 s | 0.518 s | 507 MiB/s | +60% | 32、256 |

  - 环境：一台 Windows 11 机器，NTFS，临时目录所在的盘，数据只跑了一轮；关闭组自己就有双峰（0.21 s 和 0.31 s 各几次），机器很吵，数字只看量级（置信度：中）。前一轮 5 次的结果同一量级（关闭 0.208 s，默认 0.404 s）。
  - CRC 本身：256 MiB 约 14 ms，和 0.3 s 的下载比约 5%，和“只在 teardown”组的 +12% 同一量级（那一组还多一次检查点和记录文件的同步）。
  - 默认调度的开销主要是 32 次同步：本地不限速时 8 MiB 只要约 10 ms，检查点几乎一个接一个；mmap 后端只有一个 worker，同步期间写在它后面排队（推断，没有单独测）。网络下载的速度低得多，8 MiB 之间的时间长得多，同步占的比例相应小；这只是推断，没有测限速的场景。C33 接上真正的状态文件之后应在限速场景下再测一次，必要时调大默认的 `checkpoint_bytes`。
- **测试。**
  - `file_writer/crc.rs`：`test_a_frame_splits_at_block_boundaries`（块内一段、跨边界拆开、每段自己的 CRC、空帧没有段）。
  - `checkpoint/blocks.rs`：按顺序写只有一段、最后就是整块的 CRC；一个洞两边合并；性质测试 `test_segments_merge_in_any_order`（任意切法、任意顺序，包括较短的最后一块：途中每一段都等于它覆盖的字节的 CRC、互不相邻，最后整块的 CRC 正确）；每块 64 段的上限之后回读、之后的段不再保存；合计 16384 段的上限之后挑段最多的块回读、其它块不动。
  - `checkpoint/tests.rs`（记录的 writer 把字节写进内存，记录的接收者检查每个记录的块在记录之前已经同步过）：按字节到期、按时间到期（不到时间不开始）；没有完整的块不开始、不同步；串行（运行中到期的那次等它结束，只记之后完成的块）；同步失败 → 只有回滚、没有记录，下一次重记；记录失败 → 记录、回滚，到期之后用同样的 CRC 重记；回滚失败 → 不再有检查点，teardown 也不做；超过上限的块在同步之后回读、只读一次、CRC 正确，没超的块不读；teardown 等正在运行的那次、再记剩下的块（包括较短的最后一块）；不打开就不算 CRC、失败的写不带 CRC；每个后端（pool、null，启用时 mmap、compio）的 `sync` 成功且数据在文件里；`FileBlocks` 读区间。
  - `download_loop/tests/checkpoint.rs`（`Harness::checkpointed`：内存里的文件、块 64 KiB、帧的内容按位置和端各不相同）：对冲（原连接先到、对冲先到两种，败方的帧丢掉）、被裁剪的帧（一个 chunk 的末尾和下一个 chunk 的开头在同一块里）、重启的区间（一个 runner 在块中间失败，另一个下完其余）之后，每个完整的块恰好记一次、CRC 等于内存里那一块的 CRC、记录之前已同步，没完整的块不记；失败的写不带 CRC、所在的块不记，前后的块照记；循环按 `checkpoint_bytes` 开始检查点、以事件结束、块进 `durable`；teardown：取消、失败时记，`ResourceChanged` 不记也不同步；没有检查点时写不带 CRC、不同步（teardown 也不）。
  - `concurrent_task/mod.rs` 的 `the_checkpoints_of_a_download_match_its_file`：真实的下载（特性选出的 writer、真实的 runner、5 MiB + 123 字节），每一块（包括较短的最后一块）恰好记一次，CRC 等于临时文件里那一块的 CRC。
  - `options.rs`：默认值，`checkpoint_interval` 为 0 时 `validate` 失败。
- **变异检查**（`--all-features` 跑 bolt-load 的 `checkpoint`、`crc`、`concurrent_task::tests` 单元测试），全部被抓到：
  - 失败的写也带 CRC：`test_crcs_only_for_successful_writes_when_asked`、`a_failed_write_is_not_recorded`；
  - 拼接用错长度（左段的长度）：`blocks.rs` 的三个测试（含性质测试）、`test_a_block_past_the_cap_reads_back_after_the_sync`、循环里的三个 CRC 对文件的测试、`the_checkpoints_of_a_download_match_its_file`；
  - 记录之前不同步：`test_a_checkpoint_after_enough_bytes`、`test_a_failed_sync_rolls_back_and_the_next_retries`、`test_a_failed_rollback_stops_the_checkpoints`、`test_a_block_past_the_cap_reads_back_after_the_sync` 和循环里的六个测试；
  - 回读在同步之前：`test_a_block_past_the_cap_reads_back_after_the_sync`；
  - 失败的检查点也把块记为 `durable`：同步失败、记录失败的两个测试；
  - 没有接收者也算 CRC（循环总是打开 CRC）：`no_crc_and_no_sync_without_checkpoints`；
  - 回滚失败之后照样做检查点：`test_a_failed_rollback_stops_the_checkpoints`；
  - 没有每块 64 段的上限：`blocks.rs` 和 `checkpoint/tests.rs` 的两个上限测试；
  - 不做 teardown 检查点：`test_the_teardown_checkpoint`、上限测试、`the_checkpoints_of_a_download_match_its_file` 和循环里的四个测试；
  - `ResourceChanged` 也做 teardown 检查点：循环的 `the_teardown_checkpoint`；
  - 时间不会让检查点到期：`test_a_checkpoint_after_enough_time`、`test_a_failed_record_rolls_back_and_the_next_retries`。
  - 没有单独做“串行”的变异：接收者在运行中的检查点手里，去掉 `running` 的判断也开不了第二个。
- **第一轮审查之后的修改**（codex 第一轮 84 分，一个 Major）：
  - **Major：teardown 等正在运行的检查点。** 原来 `ResourceChanged` 时 `Checkpoints` 连同正在运行的检查点一起丢掉；`blocking::unblock` 的任务被丢掉时不会停，`record`、`roll_back` 或回读可能还在跑，teardown 已经在删文件。现在：
    - `DownloadLoop::finish` 在任何结果下都先等正在运行的检查点（`Checkpoints::settle`），再交出文件；`ResourceChanged` 只是不再开新的 teardown 检查点。
    - 检查点的阻塞部分持有这次尝试的 drain 组（C31 的 `Ownership::drain`，经 `OutputFile::drain` 交给 `Checkpoints::new`），直到它结束、并且它的返回值（接收者、结果）已经放下：返回元组里 drain 放在最后，即使 future 被丢掉、没人读结果，也是先放接收者再放 drain。panic 处理等 drain 时也就等它。
    - 测试：`teardown_waits_for_a_running_checkpoint`（`ResourceChanged`、取消、普通失败、成功各一次：接收者停在 `record()` 里（`RecordGate`，测试放行，不靠时间），`finish` 一直挂起，放行之后才结束；`ResourceChanged` 只有那一次记录，其它三种再记 teardown 的块）；`test_a_running_checkpoint_holds_the_drain`（停在 `record()` 里时丢掉 `Checkpoints`，像 unwind 那样，drain 不结束，放行之后才结束）。
  - **Minor：停了之后不再攒状态。** 回滚失败之后 `note_written` 直接返回，不再记段、待查的块和字节；循环收到“停止”的结果后让 `PendingWriter` 不再给之后提交的写算 CRC（`stop_checksumming`）。测试：`test_a_failed_rollback_stops_the_checkpoints` 加查停了之后再写，段、待查、待记都是 0；`no_crc_after_the_checkpoints_stopped`（循环里停了之后的写不带 CRC，状态一直是空的）。
  - **建议：回读失败。** `MemoryBlocks` 可以注入读失败。`test_a_failed_read_back_rolls_back`：回读失败 → 只有回滚、没有记录，块仍待记，到期之后记下、CRC 正确；回滚也失败 → 停止、什么都不留。
  - **建议：限速场景的基准。** `checkpoint_overhead` 改成三个场景（`CHECKPOINT_BENCH_SCENARIOS` 选择），并记下每次检查点的 writer `sync` 用时（测试构建里 `Checkpoints` 的 `sync_times`）。mmap 只有一个 worker，同步期间它不写，所以 `sync` 用时就是 writer 停顿的上限（推断：写可能本来就在排队以外的地方等）。每组 5 次，中位数 / p90，同一台机器，单独运行：

    | 场景 | 组 | 中位数 | p90 | 比关闭 | 检查点 | 每次同步（中位数 / p90 / 最大） | 每次运行的同步合计（中位数） |
    | --- | --- | --- | --- | --- | --- | --- | --- |
    | 本地不限速，256 MiB | 关闭 | 0.317 s | 0.333 s | — | 0 | — | — |
    | | 只在 teardown | 0.348 s | 0.376 s | +9.6% | 1 | 62.3 / 84.0 / 84.0 ms | 62 ms |
    | | 默认 | 0.489 s | 0.506 s | +54.1% | 32 | 5.4 / 8.8 / 11.3 ms | 194 ms |
    | 100 MiB/s，256 MiB | 关闭 | 2.407 s | 2.490 s | — | 0 | — | — |
    | | 只在 teardown | 2.402 s | 2.411 s | −0.2% | 1 | 19.1 / 28.4 / 28.4 ms | 19 ms |
    | | 默认 | 2.399 s | 2.400 s | −0.4% | 32 | 6.1 / 9.7 / 13.9 ms | 208 ms |
    | 20 MiB/s，64 MiB | 关闭 | 3.121 s | 3.132 s | — | 0 | — | — |
    | | 只在 teardown | 3.123 s | 3.128 s | +0.1% | 1 | 4.3 / 18.3 / 18.3 ms | 4 ms |
    | | 默认 | 3.119 s | 3.123 s | −0.1% | 8 | 3.6 / 8.2 / 12.6 ms | 39 ms |

    - 限速时（100 MiB/s、20 MiB/s）默认调度的开销在噪声之内：同步发生在下载进行中，单次约 4–6 ms（最大约 14 ms），每 8 MiB 一次，100 MiB/s 时约每 80 ms 一次，合计约 0.2 s / 2.4 s，没有拖慢完成时间（限速器在等，writer 有空）。
    - 不限速的本地下载仍是 +54%：8 MiB 只要约 10 ms，检查点一个接一个，每次 5 ms 的同步占了一半的时间，加上和写抢磁盘。
    - 只测了一台 Windows 机器上的 NTFS 和 mmap writer，没有测更慢的盘、pool 和 compio，也没有测只刷脏区间的办法；置信度：中。
  - **留给 C33 的未决项。** C32 不改调度。C33 接上真正的状态文件、打开检查点之前，按这些数字核对默认值：本地或很快的源上 8 MiB 太频繁；可以考虑检查点之间的最小间隔（比如上一次结束后至少几百毫秒）、更大的 `checkpoint_bytes`，或者让 mmap 只刷上次以来写过的区间（`flush_range`），再测一次；也要在更慢的盘上测同步的停顿。
  - 变异检查（编号接在前面之后）全部被抓到：`ResourceChanged` 丢掉正在运行的检查点：`teardown_waits_for_a_running_checkpoint`；阻塞部分不持有 drain：`test_a_running_checkpoint_holds_the_drain`；停了之后照样记写：`test_a_failed_rollback_stops_the_checkpoints`、`no_crc_after_the_checkpoints_stopped`；停了之后照样算 CRC：`no_crc_after_the_checkpoints_stopped`。原来的“回滚失败之后照样做检查点”改成同时去掉 `note_written` 的判断（否则没有新的完整块，什么都不会发生），被 `test_a_failed_rollback_stops_the_checkpoints`、`no_crc_after_the_checkpoints_stopped` 抓到；只去掉“停止”标志、保留 `note_written` 的判断的变异是等价的（停了之后没有新的块可记），没被抓到，这是预期的。回读失败的测试也抓到了“失败的检查点也把块记为 `durable`”和“时间不会让检查点到期”。
- **第二轮审查之后的修改**（codex 第二轮 90 分，一个 Major）：第一轮返回的 `(receiver, outcome, drain)` 元组只在闭包正常返回时有序；阻塞任务在运行之前被丢掉、或者闭包 panic 时，闭包各自捕获的值按未规定的顺序销毁，drain 可能先于接收者或读取器放下。现在按 C31 的 `Drained` 做：接收者和读取器放进 `Work`，`start` 里就包成一个 `Drained<Work>`（drain 是它的最后一个字段），整个交给 `run`，闭包先写 `let mut held = held;` 整个拿走、整个返回；`run` 拿回之后先放读取器、再放 drain（`Drained::into_parts`），接收者交回 `Checkpoints`（它自己持有 drain）。测试（接收者和读取器各带一个探针，放下时记下 drain 是否还开着，只记不断言）：`test_the_work_drops_before_its_drain`（接收者 panic、读取器 panic、正常完成三种）、`test_a_checkpoint_dropped_before_it_ran_drops_its_work_first`（还没运行就丢掉）。变异：`Drained` 两个字段换序、返回时先放 drain 再放读取器，都被抓到；“阻塞部分不持有 drain”仍被 `test_a_running_checkpoint_holds_the_drain` 抓到。另外，测试里等闸门的地方都加了上限（`recv_timeout`、`tokio::time::timeout`）。
- **第三轮审查之后的修改**（codex 第三轮 91 分，一个 Major）：pool 的 `sync` 原来克隆句柄、在单独的 `blocking` 线程上 `sync_data`，那个线程不持有 drain：尝试 unwind 时 worker 退出、drain 结束，panic 处理就可能在同步还拿着句柄时放锁（§11.5.4）。改为经队列（`sync_through`、`Command::Sync`），由已经持有 drain 的 worker 同步（选了这个，不另包 `Drained`：mmap、compio 本来就这样）。其它新的阻塞操作逐个核对过：mmap、compio 的同步都在持有 drain 的 worker 里；`FileBlocks` 的回读在检查点的阻塞闭包里打开、读完、关掉部分文件，闭包持有 `Drained<Work>`；slow writer（只在测试里）在调用它的 future 里同步，没有另起任务；null 没有文件。测试 `test_a_held_sync_holds_the_drain`（pool_writer.rs）：worker 停在同步前的钩子里，丢掉同步的 future 和 writer，drain 在 0.5 s 内不结束（空闲的 worker 早已退出；没有信号能说明“没结束”，所以这是唯一带时间的断言，正确的实现总是通过），放行之后结束。变异“worker 把同步交给一个不持有 drain 的线程”被它抓到。另外按建议加了 `test_an_unread_result_drops_its_work_before_its_drain`：阻塞部分完成（用一个记录唤醒的 waker 得知，不 poll 结果），丢掉 future 而不读结果，接收者和读取器都在 drain 结束之前放下；变异“闭包在返回之前就放掉 drain”被它和 `test_the_work_drops_before_its_drain` 抓到。`test_the_work_drops_before_its_drain` 的注释改为只说它覆盖的情况（结果被读走）。
- **第四轮审查**（codex 第四轮 PASS，97 分）：剩下的 Minor 已修：`test_an_unread_result_drops_its_work_before_its_drain` 的阻塞部分可能在第一次 poll 之前就完成，第一次 poll 就是 Ready；现在接收者在 `record` 里等测试放行，第一次 poll 返回 Pending 之后才放行，再等唤醒、不读结果地丢掉 future（连续 20 次通过，变异“闭包在返回之前就放掉 drain”仍被抓到）。
- **之后的修改。** C33a 用真正的状态文件重测之后，默认的 8 MiB / 2 s 保留，另加检查点之间至少 0.5 s 的间隔（`MIN_SPACING`），见 §14.29.1。
- **没做的。** 状态文件、追加位置 `end` 和 `set_len`/`fsync` 的顺序在 C33；没有任务交出接收者，生产路径上 `durable` 现在没人读；单连接不做检查点（§11.5.8）。

### 14.29 状态文件（PR-9d，C33）

C33 拆成两个提交（实现者提议，lead 同意，各自审查、各自提交）：C33a 是格式和写入方，没有行为变化；C33b 把它接进任务（`Resume` 仍是 crate 内部的、阶段、提交、完成、丢弃、回退、panic 清理、`On` 的单连接改走并发引擎、`kept_for_resume`、`discard_partial`）。这一节先写 C33a。

- **lead 的决定**（2026-11-10，不是维护者的决定）：`kept_for_resume` 和 `discard_partial` 在 C20 公开 `Resume` 之前都是 crate 内部的：`Resume` 不公开时，公开的 `kept_for_resume` 永远是 `None`，`discard_partial` 也没有用处。C33a、C33b 都不算破坏兼容（tasks.md §0 第 3 条）；C33b 要再核对 `Off` 的文件、开销和错误确实不变。读回状态文件只在测试里有一个解码器，真正的读取和拒绝规则是 C20 的。

#### 14.29.1 C33a：格式和写入方

- **范围。** `task/instance/state_file.rs`：§11.5.5 的二进制格式（头部、24 字节的记录，小端，CRC32C）和写入方 `StateFile`；C32 的 `CheckpointReceiver` 由它实现，成为真正的检查点接收者；Unix 上同步父目录的辅助函数；检查点的最小间隔（C32 留下的未决项）。现在没有任务打开它：只有测试和基准用它，生产代码里的这些项标 `expect(dead_code)`，C33b 接上。
- **格式。** 和 §11.5.5 的表逐字段一致：`magic`、`version = 1`、`flags = 0`、`block_size`（u32）、`total`、验证器（种类 1 = 强 ETag、2 = Last-Modified，u16 长度，最多 1024 字节）、`source`（u16 长度，`0xFFFF` 表示没有，最多 4096 字节）、`header_crc`（前面所有头部字节的 CRC32C）；记录：`index`、`len`、`data_crc`、`kind = 1` 加 3 个保留的 0、`record_crc`（前 20 字节的 CRC32C）。`encode_header` 对超长的验证器、`source` 返回 `HeaderError`，什么都不写（C33b 据此判“没有可用的验证器”，§11.5.3）。读回是 C20 的事；测试里有一个只供测试的解码器（`state_file::tests::decode_header`、`decode_records`），按 §11.5.5 校验 CRC。
- **写入方 `StateFile`**（文件操作经一个可以注入的 `StateIo`，生产是 `LockedFile(Arc<File>)`，即 C31 锁住的句柄，`DestinationLock::handle` 交出它的一个 `Arc`；放锁仍由 `DestinationLock::release` 做，谁还拿着句柄都一样）：
  - `write_header`：在偏移 0 写头部，`sync_all`；`end` 移到头部之后（新开始的第 3 步，§11.5.5）。调用方之前已经作废过（`set_len(0)` + `fsync`）。
  - `append`（检查点第 3 步）：从 `end` 写这次的记录，`sync_all`，成功之后 `end` 才前移。
  - `roll_back`：`set_len(end)`，`sync_all`；失败时返回错误，C32 的检查点据此停止（文件在 `end` 之前仍然有效，之后是残片或完整而有效、数据已同步过的记录，§11.5.5 的分析）。
  - `invalidate`：`set_len(0)`，`sync_all`，`end = 0`。
  - 用 `sync_all` 而不是 `sync_data`：追加改变文件长度。
- **父目录同步**：`sync_parent_dir`，Unix 上打开父目录 `fsync`；Windows 没有目录同步，什么都不做，靠 NTFS 的元数据日志（已知限制，§11.5.5）。C33b 按 §11.5.5 的顺序调用。
- **检查点的默认调度**（C32 的未决项，这里定下，实现者的决定）：保留 8 MiB / 2 s，另加最小间隔：一次检查点结束之后至少 0.5 s 才开始下一次（`checkpoint::MIN_SPACING`，不是 `TaskOptions` 的字段；teardown 的检查点不受它限制）。依据是用真正的状态文件（`StateFile` 写到另一个文件、每次 `sync_all`）重测的结果：`checkpoint_overhead` 加了两组，每组 5 次，中位数，release，默认 feature（mmap writer），单独运行：

  | 场景 | 关闭 | 只在 teardown | 8 MiB / 2 s，无间隔（C32） | 8 MiB / 2 s + 0.5 s 间隔（新默认） | 32 MiB / 2 s，无间隔 |
  | --- | --- | --- | --- | --- | --- |
  | 本地不限速，256 MiB | 0.309 s | 0.350 s（+13.2%，1 次） | 0.489 s（+58.1%，32 次） | 0.358 s（+15.8%，2 次） | 0.385 s（+24.5%，8 次） |
  | 100 MiB/s，256 MiB | 2.439 s | 2.452 s（+0.5%，1 次） | 2.393 s（−1.9%，32 次） | 2.393 s（−1.9%，6 次） | 2.406 s（−1.4%，8 次） |
  | 20 MiB/s，64 MiB | 3.126 s | 3.132 s（+0.2%，1 次） | 3.113 s（−0.4%，8 次） | 3.116 s（−0.3%，7 次） | 3.126 s（−0.0%，2 次） |

  - 间隔把本地不限速的开销从 +58% 降到 +16%，和“只在 teardown”（每次写都算 CRC、最后一次检查点）的 +13% 接近：剩下的主要是 CRC 和最后一次同步，不再是检查点的频率。限速的两个场景都在噪声之内，而且 20 MiB/s 时间隔几乎不起作用（7 次对 8 次），慢的源每 8 MiB 照样记一次。
  - 加大 `checkpoint_bytes` 到 32 MiB 也能降本地的开销（+25%），但慢的源也一起变稀（20 MiB/s 时只有 2 次），崩溃时最多要重下的数据变成四倍；间隔只在检查点本来就太密时起作用，所以选间隔。
  - 代价：快的源在两次检查点之间最多多写约 0.5 s 的数据（本地约 400 MiB，100 MiB/s 时约 50 MiB），崩溃时要重下；这些数据本来下得也快。
  - 没有试 mmap 只刷脏区间（`flush_range`）：间隔已经把开销降到 teardown 那一档；留给以后，如果慢盘上的测量需要。
  - `Off` 不受影响：没有接收者时不算 CRC、不同步、不调度（C32 的测试 `no_crc_and_no_sync_without_checkpoints` 仍然成立），关闭组和 C32 时一致（0.309 s 对 0.317 s，噪声之内）。
  - 环境同 §14.28：一台 Windows 机器，NTFS，mmap writer；置信度：中。
- **测试。**
  - `state_file/tests.rs`（`MemoryIo`：内存里的文件，记下每个操作，按编号注入失败，失败的写可以写出一部分，记下最后一次同步时的内容）：头部逐字节（含 CRC）；种类 2 和没有 `source`、空 `source`；长度上限（1024、4096 刚好可以，多一个字节不行）；记录逐字节；头部之后追加两次，操作顺序是写、同步、写、同步、写、同步，同步的内容就是全部；追加的写或同步失败（写出半条记录）时 `end` 不动，回滚是 `set_len(end)` 加同步，回滚之后的追加从同一位置写；回滚失败返回错误，文件在 `end` 之前有效，之后的残片被解码器丢掉；头部的写或同步失败返回错误、`end` 不动；作废是 `set_len(0)` 加同步；经真实文件的句柄写头部、记录、回滚、作废；经 `DestinationLock` 锁住的句柄写，期间另一个任务 `InUse`，放锁之后锁文件没了；父目录同步。
  - `concurrent_task/mod.rs` 的 `the_state_file_of_a_download_records_its_blocks`：真实的下载，状态文件作为接收者，结束后文件是有效的头部加每块恰好一条有效记录、之后没有别的字节，CRC 等于临时文件里那一块的 CRC。
  - `checkpoint/tests.rs` 的 `test_checkpoints_keep_their_minimum_spacing`：上一次结束之后不到间隔时不开始，过了才开始。测试的 `Schedule` 用 0 间隔，原有测试不变。
- **变异检查**（`--all-features` 跑 `state_file`、`checkpoint`、`concurrent_task::tests`），全部被抓到：追加没同步就移动 `end`；回滚什么都不做（失败的追加之后没有回滚）；回滚不同步；头部不同步；记录不同步；作废不同步；头部 CRC 跳过 magic；记录 CRC 只覆盖 16 字节；长度上限差一；忽略最小间隔。
- **第一轮审查之后的修改**（codex 第一轮 PASS，94 分，两个 Minor、一个建议）：最小间隔的测试改为从检查点自己记下的结束时刻（`ended`）算时间，差 1 ns 不开始、恰好到间隔开始（原来拿固定的 `t0 + 20 s` 和真实的 `Instant::now()` 比）；`checkpoint_interval` 的文档和 `docs/task.md` 改为：0.5 s 的间隔只约束定期的检查点，teardown 的那次不等它；`DestinationLock::handle` 和 `release` 的文档写明：经句柄对状态文件的每个操作都要在 `release` 之前结束，否则句柄比锁活得长；Windows 上写用的是 `seek_write`，它会移动所有克隆共用的文件游标，任何依赖游标的用法都要和它串行。C33b 接上写入方之后，加一个用真实句柄、带闸门的测试：一个状态文件操作停住时 teardown 不放锁，放行之后才放。
- **兼容性。** 没有公开项变化：`StateFile`、格式、`DestinationLock::handle`、`MIN_SPACING`、`Schedule::min_spacing` 都是 crate 内部的；`TaskOptions` 只改了文档。行为上，检查点只有测试打开，所以生产路径不变。按 tasks.md §0 第 3 条不算破坏兼容，提交是 `feat(task)`。

#### 14.29.2 C33b：可续任务写状态文件

- **范围。** §11.5.13 的 C33 去掉 C33c 的两项（`On` 加 `prefer_mode(Singleton)` 改走并发引擎、`discard_partial`，实现者提议，见下）：crate 内部的 `Resume::{Off, On}`（`TaskBuilder::resume` 只在测试里）、§11.5.3 的可续判断、可续的新开始、阶段 B1/C/D、`OutputFile::commit` 按阶段分支、完成的 8 步、回退之前作废、丢弃的顺序、panic 按阶段清理、crate 内部的 `kept_for_resume`。只写不读：读回、校验、续传是 C20。
- **C33c（实现者提议，lead 同意）。** `On` 的单连接改走并发引擎（§11.5.8）只改模式选择，`discard_partial` 只是复用这里的丢弃顺序的另一个入口，两者都还是 crate 内部的；拆出去让 C33b 好审。C33b 里 `On` 加 `prefer_mode(Singleton)` 按 §11.5.3 不可续（不走并发引擎），照 D 处理。
- **可续的判断**（`drive::resumable_header`）：`Resume::On`、模式是并发（已经意味着总长已知非零、没有内容编码、能用 Range）、meta 有 `validator`，并且头部能编码（验证器不超过 1024 字节；`source` 现在总是 `None`，C20 加 `resume_source`）。不满足时照 `Off` 下载：阶段 D，不写状态。
- **锁。** `Resume::On` 一律用 `Unsupported::Fail`（Q3：不支持锁时 `On` 失败），可续与否都一样；`Off` 仍是 `Proceed`。
- **新开始，可续**（`drive::start_resumable`，在 `Initializing` 里、发布 `info` 之前，不和取消赛跑）：C31 已在加锁之后作废旧状态（第 1 步）；进入阶段 B1；`truncate(true)` 打开部分文件、`set_len(total)`、`sync_all`；经锁住的句柄写头部、`sync_all`（`StateFile::write_header`），这时进入阶段 C；同步父目录；`StateFile` 交给 `ConcurrentTaskInner::checkpoint_to`，成为检查点的接收者。每一步之后检查取消。任何一步失败：任务以 `Io` 失败（部分文件的打开和 `set_len` 是 `Create`，同步是 `Flush`；头部的写和同步是状态文件路径上的 `Write`；目录同步是 `Flush`，实现者的选择），按阶段清理：B1 丢弃，C 两者都留（第一轮审查之后的修改，见下）。
- **阶段**（`Ownership`）：`Unowned`（A）、`Fresh`（B1）、`Resumable`（C）、`Transfer`（D）、`Published`（P）；B2 是 C20 的。另记 `durable`：任务写过状态文件，它的结束要同步目录。`release`：任务保留文件时（`keep`，第一轮审查之后从阶段 C 分出来）只放锁、不删状态文件（`DestinationLock::release_keeping`）；写过状态文件的其它情况删状态文件、同步目录、放锁（`release_synced`，每步失败 `warn!`）；其它照旧（`release`）。
- **提交**（`OutputFile::commit`，阶段 C 时走 `commit_resumable`，其它阶段不变）：
  - 成功：改名；进入 P；同步目录、截断状态、同步状态（`Ownership::complete_resumable`，每步失败 `warn!`，结果仍是 `Finished`）；放锁时删状态文件、同步目录、放锁。即 §11.5.5“完成”的第 2–8 步（第 1 步 `finalize` 在提交之前）。
  - 改名失败：两者都留，`kept_for_resume` 是部分文件，任务以 `Io { op: Rename }` 失败。
  - `ResourceChanged`：丢弃（作废 → 删部分文件 → 同步目录 → 放锁时删状态文件 → 同步目录）。teardown 不做检查点（C32）。
  - 回退（`RangeUnsupported`）：同样先作废、再删部分文件，任务进入 D，之后照常回退到单连接，结束时删状态文件。作废失败时不回退（实现者的决定）：单连接会以 `truncate` 重写部分文件，而有效的状态还指着它；所以两者都留、`kept_for_resume` 是部分文件，任务以状态文件上的 `Io { op: Lock }` 失败。
  - 其它失败、取消：两者都留，`kept_for_resume`；teardown 先做的检查点在 C32。
- **丢弃**（`Ownership::discard_resumable`，ResourceChanged、B1 的失败、B1 的 panic 都用它）：作废（经锁住的句柄 `set_len(0)` + `sync_all`）→ 成功才删部分文件、同步目录 → 放锁时删状态文件、同步目录。作废失败：不删部分文件、两者都留，`warn!`，记下保留（放锁时不删状态文件）；任务按原来的错误结束，部分文件报在 `leftover_temp_file`，`kept_for_resume` 不设（实现者的决定：任务本来要丢弃它）。
- **panic**（TASK-8）：A 不动；B1 按丢弃的顺序；C 两者都留、不做 teardown 检查点、`kept_for_resume`；D 删临时文件；P 补完完成的步骤（同步目录、截断、同步），终态 `Finished`。等 drain 的部分不变（C31、C32）。
- **`kept_for_resume`**：`TaskStatus` 上 crate 内部的字段，`StatusPublisher::finish_keeping` 设置（lead 决定，§14.29 开头）；C20 公开。
- **钩子**（`file_steps.rs`）：`FileHooks` 在每个文件步骤之前被调用（`Step` 列出 §11.5.5 的步骤），生产里是空的；状态文件的写、截断、同步经 `HookedFile`，同样先过钩子。测试用 `StepLog` 记下顺序、按次数或“某步之后”注入失败、在某一步停住。只有可续任务的步骤过钩子，`Off` 不经过它们。
- **`Off` 不变的核对**（tasks.md §0 第 3 条，lead 要求）：
  - 代码路径：`Off` 用 `Unsupported::Proceed`（照旧）；`Ownership` 的阶段一直是 `Transfer`，`durable` 为假，所以 `OutputFile::commit` 走原来的 `commit`，`release` 走原来的 `DestinationLock::release`；`ConcurrentTaskInner` 没有接收者，C32 保证不算 CRC、不同步、不调度。新的钩子只在可续任务的步骤里调用。
  - 测试：`test_a_task_that_keeps_no_state_takes_none_of_the_steps`（`Off` 的并发、单连接，以及 `On` 但不可续的两种：没有验证器、偏好单连接，都一个文件步骤都没走，结束时只有目标文件；`Off` 失败时删掉临时文件、不设 `kept_for_resume`）；原有的全部测试不改就通过（C31 的锁、作废、panic 的测试都还在）；C32 的 `no_crc_and_no_sync_without_checkpoints`。
  - 开销：`Off` 没有多出系统调用：多的只是一次阶段判断和一个空的钩子字段。错误：`Off` 的错误路径没有改动。所以不算破坏兼容，提交是 `feat(task)`。
- **已知限制。** Windows 没有目录同步（`sync_parent_dir` 什么都不做），靠 NTFS 的元数据日志，§11.5.5 已写明。
- **测试。**
  - `task/tests/resume_state.rs`（真实的任务、真实的文件，源是按区间给内容、在第三块中间可以出错或停在闸门的适配器，1 条连接，每 1 MiB 一次检查点；`StepLog` 记下文件步骤）：
    - 可续的下载：新开始的五步、之后只有成对的写和同步（检查点）、完成的六步，顺序都对；结束时只剩目标文件。
    - 失败和取消（取消在块 0、1 都已写入之后）：两者都留，`kept_for_resume` 是部分文件，状态文件是有效的头部加两条记录、之后没有别的字节，每条记录的 CRC 等于部分文件里那一块的 CRC；锁已放。
    - `ResourceChanged`：两者都删，最后五步是丢弃的顺序。
    - 作废失败：部分文件留着并报在 `leftover_temp_file`，状态文件仍有有效的头部，锁已放。
    - 新开始的步骤各自失败：头部落盘之前的四步，任务以对应的 `Io` 失败，两者都删，失败之后是丢弃的顺序；目录同步失败时已在 C，两者都留、`kept_for_resume`。
    - 改名失败：两者都留，状态记了全部 4 块。
    - 改名之后的步骤各自失败（目录同步、截断、同步、删除、删除后的目录同步，截断加删除、同步加删除）：都是 `Finished`，目标文件完整；留下的状态文件和 §11.5.5“紧接着的下一次 `On`”的表一致（删除成功则没有；截断成功、删除失败则是空的；截断和删除都失败则是有效的头部）。
    - 回退：作废、删部分文件在前，之后单连接完成，结束时删状态文件；作废失败时不回退，以 `Io { op: Lock }` 失败、两者都留。
    - 不写状态的任务（见上）一个文件步骤都不走。
    - lead 要求的核对 `test_a_task_that_cannot_resume_ends_as_one_with_resume_off`：`On` 但不可续（偏好单连接、总长未知、没有验证器）的任务，成功和失败两种，结束状态、错误（去掉临时目录后的文本）、目标文件、部分文件、状态文件、`kept_for_resume`、`leftover_temp_file`、文件步骤都和同样的 `Off` 任务一模一样；唯一的差别是 `On` 要求锁（不支持锁时失败，见上）。
    - `On` 在不支持锁的文件系统上以 `Io { op: Lock }` 失败，一个文件步骤都没走。
    - §14.29 许诺的带闸门的真实句柄测试 `test_a_held_state_write_keeps_the_lock`：第一次检查点的追加停在闸门（经真实的锁住的句柄），这时取消任务；另一个任务拿同一个目标是 `InUse`，任务没有结束；放行之后它以 `Cancelled` 结束、两者都留、锁已放。
  - `ownership.rs` 的 `test_a_panic_cleans_up_by_phase`：B1 按丢弃的顺序删两者；C 两者原样留下、`kept_for_resume`、锁已放；P 补完截断、同步、删除；B1 作废失败时两者都留、部分文件报为留下。
- **变异检查**（`--all-features` 跑 `resume_state`、`ownership`、`destination_lock`、`drive`、`output_file`），全部被抓到：
  - 头部在部分文件同步之前写（lead 列的）：顺序测试、新开始失败的测试、失败和取消的测试；
  - 动部分文件之前不作废旧状态（lead 列的）：C31 的 `test_a_stale_state_is_invalidated`；
  - 作废失败也删部分文件（lead 列的）：作废失败的测试、回退的测试、panic 的测试；
  - 新开始之后不同步目录：顺序测试等三个；
  - 可续任务失败时丢弃文件：失败和取消的测试、闸门测试；
  - `ResourceChanged` 时保留文件：两个丢弃的测试；
  - 回退之前不作废：回退的测试；
  - 保留文件的放锁也删状态文件：六个测试；
  - 改名之后不截断状态：顺序测试、改名之后失败的测试、panic 的测试；
  - 新开始失败时保留文件：新开始失败的测试；
  - 可能续传的任务在不支持锁时照样继续：`test_resuming_needs_the_lock`（第一版只断言错误种类，变异后在新开始里也以 `Io { op: Lock }` 失败，没被抓到；加了“一个文件步骤都没走”之后抓到）；
  - 偏好单连接也算可续：不写状态的任务的测试；
  - C 的 panic 丢弃文件：panic 的测试。
  - “记录先于数据同步”和“追加失败之后不回滚”由 C32、C33a 的测试和变异覆盖。
- **第一轮审查之后的修改**（codex 第一轮 82 分，三个 Major，各自修掉）：
  - **新开始里的取消。** `start_resumable` 收任务的取消令牌，在每一步完成之后检查（不和正在执行的步骤赛跑，C31 的规则）。头部落盘之前看到取消（B1）：丢弃两者；之后（C）：两者都留、`kept_for_resume`。测试 `test_a_cancelled_new_start_follows_its_phase`：在 `SyncPartial`、`SyncDirAfterStart` 各停一次，取消、放行，停住的那一步照常结束，之后分别按 B1、C 处理。
  - **B1 → C 的时刻。** 头部同步成功之后立刻进入 C（§11.5.9：新开始的第 3 步完成之后），不再等目录同步；初始化里出错的清理按阶段（`end_failed_start`）：B1 丢弃，C 两者都留并报 `kept_for_resume`。目录同步失败、或者在那一步 panic，都保留这一对（新开始失败的测试改了这一项，见下）。
  - **作废分成截断和同步。** 新的 `Ownership::invalidate_state` 经锁住的句柄先截断（`Step::Invalidate`）、再同步（新的 `Step::SyncInvalidation`），结果是 `Invalidation::{Done, Intact, Unsynced}`：截断失败时状态原样、头部完好；截断成功而同步失败时状态现在是空的、掉电后可能回来，两种都不是可用的一对。“保留文件”不再靠阶段 C 表示，改成 `Ownership` 上单独的 `keep` 标志（`keep_files`），放锁时据此不删状态文件。回退：`Intact` 时以 `Io { op: Lock }` 失败、两者都留、`kept_for_resume`；`Unsynced` 时同样失败、两者都留，但部分文件报在 `leftover_temp_file`，不报 `kept_for_resume`。丢弃（`ResourceChanged`、B1）走同一个作废，两种失败都保留部分文件、报为 `leftover_temp_file`。加锁之后对旧状态的作废（C31，`DestinationLock::invalidate`）不受影响：它失败时任务在动部分文件之前就以 `Io { op: Lock }` 失败，不声称任何东西可续。测试 `test_an_invalidation_reports_what_it_left`（回退、`ResourceChanged` 各自截断失败、同步失败四种）。
  - **建议：真实任务里的 panic。** `StepLog::panic` 在某一步之后 panic 一次。`test_a_panic_at_each_step_follows_its_phase`：在 `SyncPartial`（B1：两者都删）、`SyncDirAfterStart`（C：两者都留、`kept_for_resume`）、第一次检查点的写（C：同上，状态有效）、丢弃里删部分文件（作废之后已是 D：两者都删）、改名之后的截断（P：`Finished`，只剩目标文件）各 panic 一次；每次都核对终态、两个报告字段、留下的文件、锁能再拿到。
  - 改动的测试：`test_a_failed_new_start_discards_both_files` 里目录同步失败现在是两者都留、`kept_for_resume`、状态只有头部；丢弃的顺序多了 `SyncInvalidation`。
  - 新的变异，全部被抓到：新开始看不到取消（取消的测试）；C 在目录同步之后才开始（新开始失败、panic 的测试）；初始化出错一律丢弃（取消、新开始失败的测试）；没同步的作废报为可用的一对（`test_an_invalidation_reports_what_it_left`）；作废不同步（六个测试）。原有的 13 个按新代码改了文本，重跑，仍全部被抓到。
- **第二轮审查之后的修改**（codex 第二轮 90 分，一个 Major）：回退或 `ResourceChanged` 的作废截断成功之后、同步结束之前 panic，原来的 panic 清理看到的仍是阶段 C，会把空的状态报成可续。现在 `Ownership` 记下作废的进度：截断成功之后、同步和它的钩子之前就记下 `cut`；panic 清理在 C 而 `cut` 时两者都留、报 `leftover_temp_file`、不报 `kept_for_resume`。进入 D 仍在作废落盘（`Invalidation::Done`）之后。其它两步之间的 panic 逐个核对过：B1 的清理里作废中途 panic，阶段仍是 B1，panic 清理再作废一次；作废落盘之后已是 D（删临时文件，状态文件随放锁删掉）；完成的步骤在改名之后已是 P；检查点的回滚截到 `end`，之前的状态仍有效；头部同步成功和进入 C 之间没有会 panic 的操作。测试：`test_a_panic_after_the_cut_reports_no_pair`（回退、`ResourceChanged` 各一次，在 `SyncInvalidation` panic：两者都留、状态为空、只报 `leftover_temp_file`、锁能再拿到）；按建议加了 `test_a_new_start_whose_discard_fails_to_sync_leaves_both`（B1 的失败、取消、panic 三种，丢弃的作废同步失败：两者都留、状态为空、只报 `leftover_temp_file`、锁能再拿到）。新的变异“截断之后不记下 `cut`”被 `test_a_panic_after_the_cut_reports_no_pair` 抓到；原有 18 个重跑，仍全部被抓到。
- **第三轮审查**（codex 第三轮 PASS，98 分，没有 Critical、Major、Minor）：按它的建议加了 `test_a_new_start_retries_its_invalidation_after_a_panic`，直接覆盖 B1 的作废重试：新开始在头部落盘之前失败，丢弃的作废截断之后、同步时 panic，panic 处理再作废一次。第二次作废成功时两者都删、两个报告字段都不设；第二次截断失败时两者都留、状态为空、只报 `leftover_temp_file`；两种都核对步骤顺序、留下的文件、两个报告字段和锁能再拿到。

#### 14.29.3 C33c：单连接的可续任务改道，`discard_partial`

- **范围。** C33 的最后一部分（C33b 拆出来的两项）：`On` 而偏好单连接的任务改走并发引擎（§11.5.8）；crate 内部的 `discard_partial`（lead 决定，C20 公开）。C33 至此完成，tasks.md 勾上。
- **改道**（`drive::initialize`，在选模式的那一步，即 task-api-spec §4.3 的第 2 步）：`prefer_mode(Singleton)` 而且 `Resume::On`、头部能编码（有验证器且不超长）、总长已知非零、没有内容编码、源能用 Range 时，模式是并发，`Initialized::one_connection` 为真，传输的 `max_connections` 设为 1（`drive::transfer_options`）。之后照 C33b 的可续流程走：新开始、检查点、完成、回退、丢弃都一样；`TaskInfo::mode` 报 `Concurrent`。任何一条不满足就照旧走单连接、不可续，和同样的 `Off` 任务一模一样（§11.5.3、§11.5.8）。其它会选到单连接的情况（内容编码、总长为零或未知、不能用 Range）本来就不满足 §11.5.3，不改道（实现者核对 §11.5.8 之后的结论）。
- **`discard_partial(dest)`**（`task/instance/discard.rs`，crate 内部）：按 C33b 的丢弃顺序。取锁（`Unsupported::Fail`，有任务持锁时 `InUse`、不支持锁或权限错误时 `Io { op: Lock }`）→ 作废（截断、同步）→ 删部分文件 → 同步目录 → 删状态文件 → 同步目录 → 放锁。作废失败（截断失败或同步失败）：不删部分文件、两者都留，返回状态文件上的 `Io { op: Lock }`（§11.5.5）；删部分文件失败：返回部分文件上的 `Io { op: Remove }`；部分文件或两个文件都不在：不是错误；状态文件删不掉只 `warn!`（Q6）。为此 `Ownership::discard_resumable` 的结果多了删部分文件的错误（`Discarded::removal`），`IoOp` 加了 `Remove`（`#[non_exhaustive]` 的枚举上加变体，现在只有 crate 内部的 `discard_partial` 用它）。
- **兼容性**（tasks.md §0 第 3 条）：`Resume` 和 `discard_partial` 都是 crate 内部的，`Off` 的模式选择没变（改道的条件第一条就是 `Resume::On`），所以公开的行为不变；公开 API 只多了 `IoOp::Remove` 这个新变体，枚举本来是 `#[non_exhaustive]`。不算破坏兼容，提交是 `feat(task)`。
- **测试。**
  - `task/tests/resume_state.rs`：`test_a_single_connection_task_that_may_resume_goes_through_ranges`（`On` 加偏好单连接：`info.mode` 是并发，同时打开的 range 流最多一个；成功时文件步骤是新开始、检查点、完成，结束时只剩目标文件；失败时两者都留、状态是有效的头部加两条记录）；`test_a_single_connection_task_without_resume_is_unchanged`（`Off` 加偏好单连接：仍是单连接，一个 range 都没请求，一个文件步骤都没走）。`test_a_task_that_cannot_resume_ends_as_one_with_resume_off` 改成六种不可续的情况（偏好单连接时：不能用 Range、总长未知、没有验证器；不偏好时：总长未知、不能用 Range、没有验证器），并且比较的结果里加了下载用的模式；`test_a_task_that_keeps_no_state_takes_none_of_the_steps` 里 `On` 加偏好单连接的一项改成不能用 Range 的源。
  - `drive.rs` 的 `test_a_routed_task_runs_with_one_connection`：改道的任务以 1 个连接运行，其它的选项不动。整合测试里“同时打开的 range 流最多一个”抓不到连接数没有限制：下载在策略第一次扩张（1.5 s 一跳）之前就结束了，所以由这个单元测试抓。
  - `task/instance/discard/tests.rs`：两者都删、顺序是丢弃的顺序；部分文件不在、两者都不在都不是错误；有任务持锁时 `InUse`、什么都不动；作废的截断失败（状态头部完好）和同步失败（状态为空）都保留部分文件、返回 `Io { op: Lock }`；删部分文件失败返回 `Io { op: Remove }`、状态已删；不支持锁时 `Io { op: Lock }`、一个文件步骤都没走；公开入口 `discard_partial` 本身；每次之后锁都能再拿到。
- **变异检查**（`c33c-mutate.py`），全部被抓到：可续的单连接任务不改道；改道的任务不限一个连接（`test_a_routed_task_runs_with_one_connection`）；不能用 Range 也改道；`Off` 也改道；没有可用的验证器也改道；总长未知也改道；`discard_partial` 不加锁继续；删不掉的部分文件报成成功；作废失败报成成功；作废失败照样删部分文件；`discard_partial` 留下状态文件。C33b 的 19 个变异在新代码上重跑，仍全部被抓到。
- **第一轮审查之后的修改**（codex 第一轮 88 分，一个 Major）：
  - **Major：重试可能和上一次的流重叠。** 槽在 runner 报告停止时释放，而已经连上的流原来要等 worker 退出才销毁；销毁比重试的退避还慢时，单连接的任务会同时开着两个流。选的是最小的改动：runner 在 `begin_stop` 里一律先丢掉输入（连接中的连接或已连上的流），再报告停止，所以槽释放时旧的流已经没了（实现者的选择；另一种做法是槽一直留到 runner 的 teardown 结束，要改 manager 的槽记账，牵扯更多）。停止之后不再读流（排空只等数据通道），所以对普通并发任务的影响只是流关得更早：对冲的败方、重启、低速重启、切分都在停止时就放掉连接，不再等排空；槽的记账（C28、C29、T8 的预留）不变。全部原有测试不改就通过。测试 `test_a_retry_waits_for_the_old_stream`：改道的单连接任务，第一个流在 `HALF` 之后可重试地失败，它的销毁停在闸门；停住期间 3 s 里（远超 500 ms 的退避）没有任何新的 range 请求，同时活着的流最多一个；放行之后重试、完成、内容正确。“没有请求”没有信号可等，所以这是唯一带时间的断言，正确的实现总是通过。变异“停止的 runner 留着流直到退出”被它抓到。
  - Minor：task-api-spec §4.3 第 5 步里无条件的“单连接改走并发引擎”删掉，条件只写在第 2 步。
  - 建议：`test_coded_or_oversized_metadata_cannot_resume`：内容编码的 meta、1025 字节的验证器，偏好单连接与否、成功与失败，`On` 都和 `Off` 一模一样；恰好 1024 字节的验证器可续（走新开始，模式是并发）。变异“恰好 1024 字节被拒”被它和 C33a 的上限测试抓到。
  - 建议：`test_later_failures_are_only_logged`（`discard_partial`）：删部分文件之后的目录同步、删状态文件、之后的目录同步各自失败，都只 `warn!`、后面的步骤照做、结果是成功、锁能再拿到；状态文件删不掉时留下的是空的（Q6）。目录同步失败的策略（实现者的决定，和完成时一致）：只记日志、不报错，因为两个文件都已经处理完，同步失败只影响掉电之后能不能看到删除，而掉电后看到的组合按 §11.5.5 都安全。变异“目录同步失败就停下丢弃”被它抓到。
  - 变异：C33c 的 14 个（新加 3 个）全部被抓到；C33b 的 19 个重跑，仍全部被抓到。
- **已知限制：ureq2 适配器可能短暂多开一个连接**（lead 的决定，不是维护者的决定；codex 第二轮 91 分的 Major）：
  - C33c 保证的是：被占用的 runner 槽只有一个，并且在 `Stopped` 发布之前，上一个引擎拥有的输入（连接中的 future 或已连上的流）已经销毁。runner 本身不保证只有一个：槽释放之后，停止的 worker 可能还在退出。物理 socket 的数量还取决于客户端的连接池；对 reqwest 以及任何“丢掉流就关掉或归还连接”的适配器，引擎这一侧同时在用的连接最多一个。
  - ureq2 适配器把请求（`unblock(request)`）和响应体的读（`CrossRuntimeStream`，内部是 `blocking::Unblock`）放在 `blocking` 线程池上。丢掉 future 或流并不能停下已经在跑的阻塞调用，线程一直拿着响应和 socket，直到这次调用返回。所以空闲超时或取消之后，重试会在旧连接还开着的时候打开新连接；服务器卡住而 agent 没设读超时（ureq 默认没有）时，这个重叠可以一直持续。这对所有并发任务都一样（连接数可以短暂超过 `max_connections`），不是 C33c 引入的，在 C33c 之前就存在。
  - 修复需要适配器层面的“拆除完成”信号：槽要一直保留到阻塞调用退出、放掉响应为止，并且异步地等它。这不属于 C33c，留给后续 issue。在那之前，适配器 README 说明怎样限定重叠的时长：`AgentBuilder::timeout_read` 只限定单次卡住的 socket 读，服务器一点点地发（比如 chunk extension）就一直触发不了，也管不了卡住的写；更全面的是 `AgentBuilder::timeout`，整个请求的期限，包括 DNS、连接、重定向和读响应体（所以要大于最长的一次 range 传输）。按 ureq 2.12.1 的文档，慢的 DNS 解析仍可能超过它，因为 DNS 请求没法被打断。
- **第二轮审查之后的修改**（codex 第二轮 91 分）：runner 核心的改动被确认没有问题（数据不丢；对冲、切分、重试偏移、已写范围和 CRC 都不受影响），文档、新测试和目录同步的策略也确认没有问题。唯一的 Major（ureq2）按 lead 的决定记为上面的已知限制，适配器不改，README 里加了已知限制的说明。按建议，`test_a_retry_waits_for_the_old_stream` 显式用 `worker_threads = 2` 的运行时：只有一个 worker 时，阻塞的析构会卡住 manager 处理 `Stopped`，旧的实现也可能碰巧通过。变异 [11] 重跑，仍被它抓到。
- **第三轮审查之后的修改**（codex 第三轮 PASS 96 分；它接受把 ureq2 的重叠留到后续，因为 8d38f5e 上就已经有、C33c 没有扩大它）：两处文档修改。Minor：README 和上面的已知限制改写，`timeout_read` 只限定单次卡住的 socket 读，推荐 `AgentBuilder::timeout` 作为整个请求的期限，并保留 ureq 关于 DNS 的说明。建议：保证改写为“被占用的 runner 槽只有一个，上一个引擎拥有的输入在 `Stopped` 发布之前销毁”，不再说“runner 最多一个”。

### 14.30 续传（PR-9d，C20）

C20 拆成三个提交（实现者提议）：C20a 是状态文件的读取和续传前的检查，crate 内部、没有行为变化；C20b 把它接进任务（续传仍是 crate 内部的）；C20c 公开续传的 API、文档和端到端测试。三个都不算破坏兼容（tasks.md §0 第 3 条）：公开的新增项要么是新的类型（`#[non_exhaustive]`），要么是已经 `#[non_exhaustive]` 的 `DownloadError`、`TaskInfo`、`TaskStatus` 上的新变体、新字段；`Resume` 默认 `Off`，`Off` 的路径不变。

- **比对用 adapter 的新 meta**（实现者对 §14.27 的理解，lead 同意，2026-11-11）：有状态文件时，续传一律和 adapter 自己的 `retrieve_meta` 比对，即使调用方用 `TaskBuilder::meta` 交了 meta：交进来的 meta 只是调用方说的，range 请求的 `If-Range` 来自 adapter 探测的 meta。后果（C20c 的文档和 README 写明同样的条件）：两个 adapter 都缓存探测结果，同一个 adapter 实例续传时比对的是当初的探测。那之后资源变了，只有续传还要发 range 请求时，才会被 `If-Range` 发现、以 `ResourceChanged` 失败；所有块都保留下来（没有要下的）时，续传不发任何请求，远端的变化发现不了，任务把旧版本的文件改名成目标文件。所以要按当前的资源检查（包括全部保留的情况），调用方必须用新的 adapter，和 C34 的重试建议一致。

#### 14.30.1 C20a：读取和检查

- **范围。** §11.5.10 的第 1–4 步和第 6 步的报告，以及 §11.5.5“续传”的写回，都是纯粹的函数或 `StateFile` 的方法，只有测试调用；任务还不读状态文件（C20b）。
- **报告的类型**（`task/resume.rs`）：`ResumeReport`、`DroppedRecord`、`DropReason`、`RejectReason` 和 §11.5.10 的定义逐字段一致，现在是 crate 内部的（C20c 公开）。另外 derive 了 `PartialEq`、`Eq`（两个枚举还有 `Copy`、`Hash`）、实现了 `Display`（实现者的选择，便于比较和报错）。
- **读取**（`state_file::read`）：
  - `read_state`：经锁住的句柄按偏移读（Unix `read_at`，Windows `seek_read`），不另开句柄（§11.5.4）；按头部限定长度、按块流式读（codex 第一轮、第二轮的 Major，下面的界是实现者的决定）：
    - 长度取自锁住的句柄的元数据。短于最短的头部（`MIN_HEADER_LEN` = 33：空的验证器、没有 `source`）→ 一个字节都不读，`BadHeader`。
    - 否则先只读最长头部那么多字节（`MAX_HEADER_LEN` = 5153：1024 字节的验证器、4096 字节的 `source`；文件更短就读整个文件），解析头部（坏 → `BadHeader`），再按头部算这个文件的界 `len_bound`，超过 → `Oversized`，记录一条都不读。
    - 不超过时，记录按块读：每次最多 `CHUNK_RECORDS` = 2730 条（65,520 字节），用同一个缓冲区，所以读取的内存和文件长度无关；随记录数增长的只有解析的结果（有效记录、丢弃的记录、保留的记录），都用 `try_reserve` 分配，失败是 `OutOfMemory` 的 I/O 错误；排序用不分配内存的 `sort_unstable_by_key`（有效记录按块号、再按位置；丢弃的记录按位置；位置唯一，所以结果和稳定排序一样）。随记录数增长的分配都是可失败的；其余的分配（头部的缓冲区、验证器和 `source` 的字符串、一个块的缓冲区）有固定的上限，和记录数无关。峰值是解析的结果加一个块。
    - 界的推导：写入方在一次运行里每块最多记一次（检查点只记没记过的完整块；追加失败而回滚成功时那一批被截掉、下次再记；回滚也失败时检查点停止，留下的那一批不会再记；teardown 的检查点也只记没记过的块）；续传先把保留的记录写回（`rewrite` 截到头部再写），之后只记没保留的块，反复续传也一样。所以合法的文件最多是头部、每块一条记录、加一个残片。界取“头部 + (2 × 块数 + 1) 条记录”，留一倍的余量（算错时宁可多读，而不是把合法的文件拒掉）；计算用饱和算术，`total` 到 `u64::MAX` 也不溢出。
    - 没有和头部无关的绝对上限（第二轮的修改）：第一轮的 64 MiB 上限会拒掉合法的文件。64 MiB 只放得下约 280 万条记录，即 1 MiB 的块、约 2.67 TiB 的文件（第一轮写成了 2.6 PiB，单位算错了）；3 TiB 的下载在最后一块之前取消，会留下 3,145,727 条记录（头部之后 75,497,448 字节）。写入方（`resumable_header`、追加）也没有对应的上限，所以读取方不能设。
    - 拒绝的原因新加 `RejectReason::Oversized`（实现者的选择：已有的原因都说的是别的事；枚举是 `#[non_exhaustive]`，还没公开）。拒绝后上层丢弃、从头来，安全。这是一致性和健壮性的界，不是防篡改（§11.5.1）。
  - `parse_header`：magic、版本、`flags` 为 0、`block_size` 等于本版本的 `BLOCK_SIZE`、`total` 不为 0（写入方从不写 0：可续的下载总长已知、非零；实现者的决定，lead 同意）、验证器种类 1 或 2、验证器不超过 1024 字节、`source` 是 `0xFFFF` 或不超过 4096 字节、两者都是 UTF-8、`header_crc` 对；任何一条不对、或者文件短于头部（包括写到一半的头部）→ `BadHeader`。
  - `scan`：头部之后逐条记录。不足 24 字节的尾巴 → `TornTail`（位置是它的起点，截掉）；`record_crc` 不对 → `BadRecord`（块号不可信，`range` 为 `None`）；块号乘出溢出或起点不小于 `total` → `OutOfBounds`（`range` 为 `None`）；`kind`、保留字节、`len`（包括 0、最后一块的长度）不对 → `BadRecord`（带块的区间）。`len` 不对归到 `BadRecord` 是实现者的决定（lead 同意）：§11.5.10 没有单独的原因。
  - 块的区间（`block_range`）只用检查过的算术：`checked_mul`、`checked_sub`，末端是 `start + min(block_size, total - start)`，不会溢出；所以 `total` 接近 `u64::MAX` 时最后一块照样有效。
  - 去重和冲突（§11.5.5）：一块第一次出现的有效记录先保留；和它完全相同的记录静默丢掉；出现 `data_crc` 不同的有效记录时，这一块标为冲突：保留的那条和这一条都报 `Conflict`，之后这一块的任何记录也都报 `Conflict`。所以 `A,A,B` 报两条（冲突之前的重复已经静默去掉），`A,B,A`、`B,A,A`、`A,B,B` 报三条。冲突会补报更早的那条，所以丢弃的列表最后按位置排序，保证“按文件顺序”。
- **比对**（`resume_check::compare`，§11.5.10 第 2 步）：顺序是总长未知（`UnknownSize`）、内容编码（`ContentCoded`）、不能用 Range（`RangeUnsupported`）、总长（`TotalMismatch`）、`source`（`SourceMismatch`，一边有一边没有也算不同）、没有可用的验证器（`NoValidator`，超过 1024 字节也算没有）、验证器（`ValidatorMismatch`，用 C34 的 `Validator::matches`：种类相同、强 ETag 逐字节、日期按日期）。先查“这个资源根本不能续”的几条，是实现者的选择：否则总长未知（0）会被报成 `TotalMismatch`。能不能用 Range 由调用方传进来（C20b 问 adapter）。
- **逐块校验**（`resume_check::verify`，第 4 步）：按块号的顺序，每块之前问一次是否取消（取消 → `VerifyError::Cancelled`）；块的末端超过部分文件的长度 → `BeyondFileLength`，不读；否则读出整块算 CRC32C，不等 → `CrcMismatch`。读失败 → `VerifyError::Read`，任务失败（C20b 映射成 `Io`），不当作不匹配：读错可能是暂时的或权限问题，静默丢掉会把能续的块全部重下（实现者的决定）。保留的记录在传进来的 `Vec` 里原地前移、截断，不另分配；丢弃的记录接在扫描丢弃的列表后面，用 `try_push`；没有内存 → `VerifyError::OutOfMemory`（新的变体，实现者的选择：和读失败一样让任务失败，C20b 映射成 `Io`，但没有区间可报）。
- **报告**（`resume_check::report`，第 6 步）：保留的记录原地按块号排好（`sort_unstable_by_key`，不分配）、相邻的合并成区间（`try_push`），`kept_bytes` 是它们的和；丢弃的记录按位置排序（同样不分配）。没有内存 → `OutOfMemory` 的 I/O 错误。
- **写回**（`StateFile::rewrite`，§11.5.5“续传”的第 1、2 步）：`set_len(头部长度)` → 同步 → 从头部之后写入保留的记录（每次最多 `CHUNK_RECORDS` 条，经同一个 65,520 字节的缓冲区，不按记录总数分配；记录以迭代器传入，调用方不用另建一个数组）→ 同步；之后的检查点追加在它们后面（`end` 指到最后一条之后）。没有保留的记录时只截断、同步。截断失败时旧的记录还在；截断之后的任何失败只留下头部和保留记录的一个前缀（测试逐步注入失败）。经 `HookedFile` 时依次是 `TruncateState`、`SyncState`、每批一个 `WriteState`、`SyncState`，没有新增 `Step`。C32 的检查点追加（`append`）不变：它的每一批本来就受检查点的大小限制。
- **可失败分配的范围**（lead 的决定，2026-11-11，不是维护者的决定）：在 C20 的续传路径里，随状态文件的记录数或块数增长的分配一律可失败（读取、扫描、逐块校验、报告、写回）；C20 之外的引擎结构（planner、已写入的区间等）不在范围内。
- **测试。** `state_file/read/tests.rs`：写出的头部逐字段读回（两种验证器、有无 `source`、空的 `source`）；恰好 1024 字节的验证器和 4096 字节的 `source` 能读；坏头部的每一种（magic、版本、`flags`、块大小、`total` 为 0、种类 0 和 3、1025 字节的验证器、非 UTF-8 的验证器和 `source`、4097 字节的 `source`、超出文件的 `source` 长度、CRC），加上头部的每个前缀和每个字节翻转一位；有效记录按块号保留（包括较短的最后一块）；1 到 23 字节的残片；坏记录的每一种，前后的记录照常保留；越界（`total` 之后、乘法溢出）以及 `total = u64::MAX` 的最后一块；重复；冲突的六种排列（`A,B`、`A,B,A`、`A,A,B`、`B,A,A`、`A,B,B`、`A,B,C`），前后别的块不受影响；冲突补报之后仍按文件顺序；界是 `(2 × 块数 + 1)` 条记录（`total = u64::MAX` 时照算、不截断），`MAX_HEADER_LEN`、`MIN_HEADER_LEN` 等于最长、最短头部的实际长度；恰好在界上的文件整个读入，长一个字节的、长度 `u64::MAX` 的只读了 `MAX_HEADER_LEN` 就以 `Oversized` 拒绝（测试的读取方只在声明的长度里按需生成字节，每次最多给 4 KiB，记下读了多少、头部之后最大的一次缓冲区）；3 TiB 的下载在最后一块之前取消时写入方留下的文件（3,145,727 条不同的记录，头部之后 75,497,448 字节，超过 64 MiB）整个读入、全部保留，头部之后最大的一次读取正好是一个块（65,520 字节）；最长的头部（5153 字节）经每次 4 KiB、每隔一次 `Interrupted` 的读取读回，比 `MAX_HEADER_LEN` 短的合法文件（头部、两条记录、残片）读回；短于 `MIN_HEADER_LEN` 的文件（包括空的）一个字节不读、`BadHeader`；坏头部只读 `MAX_HEADER_LEN`；跨第一个块边界（第 2729、2730、2731 条记录）的重复和冲突（`A,A,其它`、`A,B,其它`、`A,A,B`、`B,A,A`、`A,B,A`）和块内一样判定，保留的位置和丢弃的顺序都对；同一块的 3000 条相同记录分散在文件里时保留第一条，再加一条不同的只报第一条和那一条；一个满的块之后 1 到 23 字节的残片在块的末尾截掉；读失败是 I/O 错误；经 `DestinationLock` 真正锁住的句柄写入、再读回 3000 条记录（Windows 上只能经这个句柄，codex 第一轮的建议）。`resume_check/tests.rs`：同一资源能续（日期换一种格式也算相同）；每个拒绝原因，以及总长未知不报成另一个总长、恰好 1024 字节的验证器可用；逐块校验（翻转一个字节 → `CrcMismatch`）；超过文件末尾一个字节的块不读、`BeyondFileLength`；每块之前问取消、取消后不再读；读失败是错误；报告的合并和排序。`state_file/tests.rs`：写回的四步顺序、之后的追加接在后面；空的写回；写回每一步失败时文件的样子；超过两批（2 × 2730 + 5 条）的写回按顺序写出全部记录、每批一次写入、最后同步一次，第二批失败时只留第一批、`end` 在头部。`resume_check/tests.rs` 另有：校验把丢弃的记录接在已有的后面，保留的保持原来的顺序。`resume.rs`：每个原因的消息互不相同。
- **变异**：51 个全部被抓到（第四轮加了“写回只写一批”“每批都写在头部之后”“写回之后 `end` 停在头部”“校验保留的是先读的记录”“报告不给保留的块排序”，“写回追加在旧记录之后”改成“写回之后 `end` 停在头部”；第三轮加了“有效记录只按块号排序”，由新的“一块的很多条记录”测试抓到；第一轮之后去掉“`read_all` 只读一个缓冲区”，加了“读取只调用一次”“超过界照读”“恰好在界上被拒”“头部和整个文件一起读”“界没有余量”；第二轮去掉 64 MiB 的两个，加了“记录一次读进一个缓冲区”“读完一个块就停”“短于任何头部的文件照读”“`Interrupted` 的读取失败”“一块的记录不按文件顺序”，冲突、残片的几个按新的代码重写）。包括：`total` 为 0 的头部被接受；残片不报；记录的 CRC、种类、保留字节、长度各自不查；偏移用 `wrapping_mul`；起点等于 `total` 的块算在界内；重复当冲突；冲突保留第一条、不补报第一条、冲突之后的记录被保留；丢弃的列表不排序；头部的 CRC、`flags`、块大小不查；1024 字节的验证器、4096 字节的 `source` 被拒；`source` 长度不设上限；比对的每一条去掉（总长未知、内容编码、Range、总长、`source`、超长的验证器）、验证器按字符串比；文件长度的比较差一、不查文件长度、不查块的 CRC、不问取消、读失败当作丢弃；报告不合并、不排序；写回不同步截断、不截断、追加到旧记录之后。
- **第一轮审查之后的修改**（codex 第一轮 88 分）：Major：读状态文件之前限定长度（`read_state`，界和 `Oversized` 见上）。Minor：§14.30 里 `If-Range` 发现变化的说法加上条件：只有续传还要发 range 请求时；全部保留时发现不了，所以要用新的 adapter（C20c 的 README 同样写）。建议：经 `DestinationLock` 真正锁住的句柄读回的测试。
- **第二轮审查之后的修改**（codex 第二轮 89 分）：Major：64 MiB 的绝对上限会拒掉合法的文件（单位也算错了，见上），去掉；保留先读头部、按头部的界；记录改为按块流式读，随记录数增长的结构都可失败地分配；读之前只剩“短于最短头部”的检查。加了 3 TiB 写入方输出（超过 64 MiB）被接受的回归测试，用按需生成字节的读取方，不往磁盘写 75 MB。冲突的判定改成先收集有效记录、按块号稳定排序、逐块按文件顺序处理，和原来逐条处理的规则一样（同样的测试不改就通过）。建议：最长的头部经 4 KiB 的短读和 `Interrupted` 读回；比最长头部短的合法文件读回。
- **第三轮审查之后的修改**（codex 第三轮 90 分）：Major：稳定排序会按记录数分配临时内存、分配失败时直接中止，改成不分配的 `sort_unstable_by_key`，键加上位置；核对了读取的其它分配都和记录数无关或可失败。建议：块边界上的重复、冲突和残片的回归测试。
- **第四轮审查之后的修改**（codex 第四轮 90 分）：Major：读取之外随记录数增长的分配也改成可失败或有界：逐块校验原地保留、`try_push` 丢弃的记录、新的 `VerifyError::OutOfMemory`；报告原地不分配地排序、`try_push` 区间；写回按批写、不按总数分配（范围按 lead 的决定，见上）。Minor：“整个读取都可失败地分配”改成准确的说法。建议：`scan` 只有测试调用，改成 `#[cfg(test)]`（它在内存里扫，分配失败时 panic），`&[u8]` 的 `ReadAt` 也只在测试里。
- **第五轮审查**（codex 第五轮 PASS 97 分，没有 Critical、Major、Minor）：按它的两条建议加了测试。`test_a_rewrite_torn_inside_a_batch`：第二批写到一半（一条完整记录加 7 字节）失败，`read_state` 读回第一批和那一条、尾巴是 `TornTail`，`end` 仍在头部；重试的写回成功，之后的追加落在正确的偏移。`test_a_stopped_verification_keeps_the_dropped_records`：直接调 `verify`，丢弃列表里已有一条；丢弃一块、保留一块之后取消或读失败，已有的那条和已完成的丢弃都在、没有重复，停下之后不再读。

#### 14.30.2 C20b：任务从状态文件续传

- **范围。** §11.5.10 的检查接进任务（`Resume` 仍是 crate 内部的，测试打开）：阶段 B2、和 adapter 自己取的 meta 比对、逐块校验、写回、按保留的块给 planner、已写入的区间、进度和检查点的“已记录”做种子，只下载缺的部分；拒绝和新开始两条路。公开 API、文档和端到端测试是 C20c。
- **公开的结果类型提前到 C20b**（lead 的决定，2026-11-11，不是维护者的决定；实现者提出了两种做法，lead 选了这一种，另一种“crate 内部的临时结果加占位错误、C20c 再换”是一次性的管道，被否决）：`DownloadError::ResumeRejected { reason: RejectReason, state_file }` 是公开枚举的变体，Rust 不允许它带 crate 内部的类型，`TaskInfo::resumed: Option<ResumeReport>` 同理。所以 C20b 公开 `ResumeReport`、`DroppedRecord`、`DropReason`、`RejectReason`（从 `bolt_load::task` 导出）、`DownloadError::ResumeRejected`、`TaskInfo::{resumed, resumable}`，以及新的 `IoOp::Read`（读状态文件、校验时读部分文件失败）。逐项核对过：四个新类型都是 `#[non_exhaustive]`；`DownloadError`、`IoOp`、`TaskInfo` 本来就是 `#[non_exhaustive]`；工作区里（包括 `bolt-load` 的集成测试、测试 crate、示例）没有对它们的穷举 `match` 或结构体字面量（`#[non_exhaustive]` 本来也不允许 crate 之外这样写），四种 clippy 配置和两次工作区测试都通过。所以提交是 `feat(task)`，不带 `!`。这些项的文档都写明续传还不能打开（C20c）：在那之前 `resumed` 总是 `None`、`resumable` 总是 `false`，没有任务以 `ResumeRejected` 失败。`ResumeRejected` 的消息按最终的样子写：`cannot resume from <state file>: <reason>`，原因如 `the partial file next to it is missing`、`the resource changed: its validator is another`（有测试）。C20c 剩下 `Resume`、`resume`、`resume_source`、`kept_for_resume`、`discard_partial`、STATUS-12/13、README、`docs/task.md` 和端到端测试。
- **加锁之后的分支**（`drive::lock_destination`）：`Resume::On`、锁文件不是这次新建的、长度非 0 → 有状态可续（`Locked::ToResume`）：`Ownership::own_to_resume` 登记锁和临时文件、直接进入新的阶段 `Phase::Checking`（B2），不作废状态、不碰部分文件；同时记下 `durable`（之后会写状态文件，结束时删除要同步目录）。否则照旧（`own` 进入 D、作废旧状态，`Locked::Fresh`）。`On` 而状态可续时，不管这一次的新开始能不能续（比如没有验证器、不能用 Range），都走续传的检查，由比对给出原因（§11.5.2）。
- **B2 的结束**：拒绝、失败、取消、panic 都不碰两个文件、都不报告（`kept_for_resume`、`leftover_temp_file` 都是 `None`），放锁时保留状态文件（`keep_files` → `release_keeping`）。加锁之后、续传开始之前就取消的（在锁的钩子里取消），同样保留两者。`end_failed_start` 加了 B2 的分支；`clean_up_after_panic` 加了 `Phase::Checking`。
- **和 §11.5.9 的表逐格核对**（lead 要求）：`ResumeRejected`、B2：两个文件不碰，`kept_for_resume`、`leftover_temp_file` 都是 `None` —— 一致（拒绝发生在 B2，§11.5.5、§11.5.10 都说不改动文件、不丢弃、不从头来；从头来是上层的事）。失败、取消、`Panicked`（比如校验时）、B2：不碰、都是 `None` —— 一致。写回失败（`Io`）、C：部分文件没动，状态是实际剩下的（头部加接受记录的前缀），`kept_for_resume` 是部分文件 —— 一致（包括写回之后 `SizePartial` 失败，按 lead 的要求算阶段 C）。失败、取消、`Panicked`、C：两者都留、`kept_for_resume` —— 一致（传输开始之前还没有 teardown 检查点可做）。B2 不需要 C33b 那样的 `cut` 标记：进入 C 在写回的截断之前，截断之后的任何结束都按 C 处理，而 C 里留下的总是有效的头部加校验过的记录前缀，是一对能续的文件；截断之前（B2）旧的一对原样有效。
- **续传的顺序**（`drive::resume_checked`，每一步之间检查取消，不和文件步骤赛跑）：
  0. 部分文件不在 → `ResumeRejected(MissingPartial)`；读它的元数据失败 → `Io { Inspect }`。
  1、3. `Step::ReadState` 之后经锁住的句柄 `read_state`（C20a 的读取、界和流式扫描）：坏头部、超长 → `ResumeRejected`；读失败 → `Io { Read }`（状态文件的路径）。
  2. 和 adapter 自己的 `retrieve_meta` 比对（lead 同意的 §14.27 的理解，§14.30），取 meta 和取消赛跑（网络，不是文件步骤），能不能用 Range 问 adapter；`compare` 不过 → `ResumeRejected`。取 meta 失败 → `Metadata`。
  4. 部分文件用自己的只读句柄逐块读回（每块之前 `Step::ReadPartial`），C20a 的 `verify`：每块之前问取消；读失败（包括打不开）→ `Io { Read }`（部分文件的路径，消息里带块的区间、保留错误的种类）；没有内存 → 同样是 `Io { Read }`。之后 `report`，每条丢弃打一条 `warn!`；交给传输的区间可失败地复制一份。
  5. 进入阶段 C（在第一次改动状态文件之前），`StateFile::rewrite` 经锁住的句柄写回保留的记录（截断 → 同步 → 分批写 → 同步）；失败 → `Io { Write }`。写回之后检查取消（写回作为一个整体，不在中间停）；然后 `Step::SizePartial`：打开部分文件、`set_len(total)`，失败 → `Io { Allocate }`。最后再检查一次取消。
  - 之后 `info` 的模式是并发；偏好单连接的任务用 1 个连接（和新开始的改道一样，§11.5.8）。`StateFile` 成为检查点的接收者，`end` 在写回的记录之后。
- **SizePartial**（实现者的决定）：writer 本来也会 `create` + `set_len(total)`，单独一步是为了让测试按钩子的顺序确认“写回在 `set_len` 之前”（§11.5.11），以及在写回之后、传输之前有一个能注入失败的点。阶段 C，失败时两者都留、`kept_for_resume` 是部分文件。
- **种子**：`ConcurrentTaskInner::resume_from(kept)` 让进度从保留的区间开始（`downloaded = kept_bytes`、`downloaded_chunks = kept`，STATUS-12）；`download_to` 把进度里的区间交给 `DownloadLoop::resume_from`：`ChunkPlanner::seed_kept` 把每个区间变成一个已完成的块（已下载、已接受、已写入），`get_available_ranges` 只剩空洞；`Checkpoints::seed_durable` 把这些块记为已记录。全部保留时 `DownloadLoop::run` 一开始就结束（planner 已完成），不发任何请求，直接走完成。planner 和已写入的区间按 lead 的范围决定不要求可失败分配（§14.30.1）。
- **info**：`resumable` = 这一次写状态文件（可续的新开始或续传）；`resumed` = 续传的报告；回退到单连接时两者变成 `false`、`None`。
- **测试**（`task/tests/resume_from.rs`，真实的任务和文件，`StepLog` 的钩子；`resume_state.rs` 的辅助函数改成 `pub(super)`，`Source` 加了 adapter 的 meta 和记下请求的区间）：
  - 失败后留下的一对续传：步骤是 `ReadState`、两次 `ReadPartial`、写回的四步、`SizePartial`，然后是检查点和完成；请求的区间都在 `2 × BLOCK` 之后；报告保留 `0..2 × BLOCK`、没有丢弃；`resumable` 为真；每个 `Downloading` 状态的进度都从保留的区间开始。
  - 改名失败留下的一对（每块都记了）续传时一个请求都不发，直接完成。
  - 拒绝：部分文件不在、坏头部、超长、总长不同、ETag 不同、验证器种类不同、没有验证器、内容编码、不能用 Range；两个文件逐字节不变、没有报告、没有 `info`、没有请求、只走了 `ReadState`，锁已放。
  - 调用方的 meta 和 adapter 的不同时，以 adapter 的为准（两个方向）。
  - 丢弃的记录：翻转一个字节（`CrcMismatch`）、部分文件被截短（`BeyondFileLength`）、残片（`TornTail`）、坏记录（`BadRecord`）、冲突（`Conflict`，两条）、越界（`OutOfBounds`）：写回之后（在 `SizePartial` 注入失败让任务停住）状态文件只有保留的记录；续传到底时报告的丢弃逐条正确、丢掉的块重新下载、内容正确。
  - 续传的每一步失败：读状态、读块（B2）两个文件不变、不报告；写回的截断、同步、写、`SizePartial`（C）部分文件不变、`kept_for_resume` 是部分文件、状态是有效的头部加保留记录的前缀；之后再续传都能完成。
  - 取消：在读状态、第一块、最后一块（之后的取消由校验之后的检查看到）停住时取消 → B2 两者不变；在写回的写入停住时取消 → 写回做完、`SizePartial` 不做、两者都留。加锁时取消（锁的钩子里）→ 两者不变。
  - panic：读状态、读块（B2）两者不变、不报告；写回、`SizePartial`（C）两者都留、`kept_for_resume`。
  - 再次续传：续传后又失败，保留不少于上次；第三次完成。
  - 续传后回退到单连接：完成，`info` 的 `resumed` 为 `None`、`resumable` 为假，状态文件已删。
  - 偏好单连接的任务续传：只请求 `2 × BLOCK..SIZE` 一个区间，最多一个连接。
  - 空的状态文件当作没有状态（新开始）；`Off` 遇到有效的一对会作废它、从头下载。
  - §11.5.5“紧接着的下一次 On”的表：删除成功、只有截断成功 → 新开始；截断和删除都失败 → `MissingPartial`。
  - 部分文件比总长长（`SizePartial` 截到总长；总长已经比对过，所以截掉的只是这个下载之外的字节）和比总长短（长到总长）：都续传完成、内容正确。
  - `resumable`：`On` 的新开始为真，`Off` 为假。
  - 检查点的单元测试：保留的块从一开始就算已记录，之后只记新写完的块。
- **变异**：26 个，25 个被抓到（第一轮之后加了“范围检查不和取消赛跑”“进度从零开始”“回退之后说自己可续”，“用调用方的 meta 比对”按新的代码重写；“回退留着报告”去掉：报告不再被复制，回退的 info 从一开始就没有它）。没抓到的一个是“保留的块不记为已记录”（`DownloadLoop::resume_from` 不调 `seed_durable`）：在任务层面看不出来，因为保留的块从不被写，也就从不进检查点的候选（`touched`），不会再被记一次；`durable` 只有测试读。`seed_durable` 本身由检查点的单元测试覆盖。被抓到的包括：空状态当作可续、`Off` 续传、加锁时取消丢弃一对、B2 的 panic 清理、续传以 D 登记、续传不记 `durable`、检查失败时丢弃、不比对、用调用方的 meta 比对、不校验、不写回、写回之后才进入 C、不做 `SizePartial`、保留的区间又下载、全部保留时照跑循环、种子不算写入、种子不算下载、不发布报告、`resumable` 总为假、回退留着报告、校验之后和写回之后的取消、部分文件不在时照读。
- **第一轮审查之后的修改**（codex 第一轮 86 分）：
  - Major：取 meta 和别的网络步骤赛跑取消，但随后的 `is_range_stream_available` 没有：adapter 卡在那里时，取消了的任务会一直停在 B2、拿着锁。改成取 meta、问 Range、比对放在同一个 `until_cancelled` 里。核对了续传路径上其余的 `await`：都是 `blocking::unblock` 的文件步骤（按 §11.5.4 不赛跑，步骤之间检查取消）。测试 `test_a_resume_cancelled_in_the_range_check_ends`：adapter 在第二次范围检查（续传的那次）卡在闸门，取消后任务结束，两个文件逐字节不变、都不报告、只走了 `ReadState`、锁已放。
  - Major：`TaskInfo::resumed` 有值时，原来的 `info.clone()` 会深拷贝报告（保留和丢弃的列表），是写回之后一次和记录数同量级的不可失败分配。改成把报告随 `info` 一次移进发布的状态（`StatusPublisher::downloading` 把它包进 `Arc<TaskInfo>`，之后的快照只复制 `Arc`）；回退要的 `info` 在移走之前单独构造，只复制 meta 和路径，`resumable` 为假、`resumed` 为 `None`。没有改公开的形状：`TaskInfo::resumed` 仍是 `Option<ResumeReport>`，C20c 最简单（实现者的选择）。核对了其余复制 `TaskInfo`、`TaskStatus` 的地方：状态通道和快照里都是 `Arc<TaskInfo>`，只有测试里 `info.clone()` 复制 `Arc`。
  - Minor：进度的测试原来靠记录器看到 `Downloading`，而且只检查“不少于”。第一轮改成在闸门处等状态通道的第一次变化，第二轮按 codex 的意见再改（见下）。
  - 建议：再次续传的测试改成 `test_resuming_again_keeps_what_was_added`：第二次续传多下完一块（第 2 块）才在第 3 块里失败，检查状态文件记了第 0、1、2 块；第三次续传保留 `0..3 × BLOCK`，只请求 `3 × BLOCK..SIZE`，内容正确。为此测试的 `Source` 加了 `fault_at`（默认 `HALF`）和范围检查的闸门。
- **第二轮审查**（codex 第二轮 PASS 96 分，一个 Minor，按规则提交前修掉）：状态通道只保留最新的值，测试线程慢过 250 ms 时，进度采样会在闸门还关着的时候把错误的零进度换成正确的种子进度，变异就可能漏过。改成只在测试里存在的握手：`StatusPublisher` 在发布每个 `Downloading` 时、传输开始之前，把它的进度记进测试给的 `Starts`（`TaskBuilder::record_starts`，都是 `#[cfg(test)]`）；`test_the_progress_starts_with_what_was_kept` 断言记下的恰好是一次 `Progress::new(Some(SIZE), 2 × BLOCK, [0..2 × BLOCK])`，不再看通道里当时是什么。`Fault::GateFirst` 因此不再需要，删掉。变异“进度从零开始”重跑三次都被抓到；全部 26 个变异重跑一次，结果不变（25 个被抓到，1 个是上面说的等价变异）。另外更新了 `Resume` 过时的文档（“只写不读”）。

#### 14.30.3 C20c：公开续传

- **范围。** 公开 `Resume`（`Off`、`On`，`#[non_exhaustive]`，默认 `Off`）、`TaskBuilder::resume`、`TaskBuilder::resume_source`、`TaskStatus::kept_for_resume`、`bolt_load::task::discard_partial`；task-api-spec 的 STATUS-12、STATUS-13 定稿；README 和 `docs/task.md` 加“续传”一节；端到端测试。去掉了 C20b 那些公开项文档里“还不能打开”的说法。
- **`resume_source`**（§11.5.2）：存进状态文件的头部，续传时和这一次给的比对，不同（包括一边有一边没有）→ `ResumeRejected(SourceMismatch)`。超过 4096 字节由 `TaskBuilder::validate` 拒绝（`BuildError::InvalidOptions`，沿用已有的变体，原因是 `the resume source is longer than 4096 bytes`），不进到 `Initializing`。
- **文档**写明（lead 要求）：同一个 adapter 上续传，比对的是它缓存的探测；之后资源变了，只有续传还发 range 请求时才会被 `If-Range` 发现（`ResourceChanged`），全部保留的续传不发请求，发现不了；要按当前的资源检查，用新的 adapter。续传的说明提到单连接，所以同时写了 ureq2 可能短暂多占一个连接，链接 #207。
- **不破坏兼容的核对**（对最终的公开 API 逐项，tasks.md §0 第 3 条）：`Resume` 是新的 `#[non_exhaustive]` 枚举；`resume`、`resume_source` 是 `TaskBuilder` 的新方法；`discard_partial` 是新函数；`kept_for_resume` 是 `#[non_exhaustive]` 的 `TaskStatus` 的新字段；`BuildError` 没有新变体（沿用 `InvalidOptions`）；C20b 公开的项不变。`Off` 是默认值，行为不变。所以提交是 `feat(task)`，不带 `!`。测试 crate（`bolt-load-tests`，`publish = false`）的 `RangeFault` 加了 `Stable`，不是公开 API。
- **测试设施**（实现者的选择）：`bolt-load-tests` 加了 `RangeFault::Stable`（`"v1"` 的验证器，按请求回答 range，资源从不变）和 `create_range_fault_server_held_at(content, position, gate)`：覆盖 `position` 的 range 响应先发它之前的字节，再停在闸门，直到放行。
- **测试**：
  - G9（`tests/resume.rs`）：21 个种子，每个种子用 SplitMix64 定一个字节位置（不是第 0 字节，探测要它），每个种子在三种客户端（reqwest、解码的 reqwest、ureq2）上都跑，共 63 次（lead 的决定：按更严的理解，种子乘客户端；一共约 4.7 s）。第一个任务在那个位置被服务器停住时取消：`Cancelled`、`kept_for_resume` 是部分文件、状态文件在；第二个任务用新的 adapter 续传：完成、hash 正确、`resumed` 有值、`resumable` 为真、旁边不留文件；每种客户端都至少有一个种子保留了块。
  - `discard_partial`：取消之后丢弃，两个文件都没了，下一个任务是新开始、内容正确。
  - 进程崩溃和跨进程的锁（`src/task/tests/resume_crash.rs`）：测试把这个测试二进制自己再起一次当子进程（`--exact`，环境变量传目标和杀的位置），子进程用 `StepLog` 的闸门停在某一步（或者等任务结束），打印一行，父进程读到这一行就 `kill`（Windows 是 `TerminateProcess`，Unix 是 `SIGKILL`）：不跑任何清理，和崩溃一样；没有任何靠时间的同步。四个位置：第一次检查点写记录之前（只有头部）、第一次检查点写完记录、同步之前（检查点进行中）、改名失败、任务结束之后（每块都记了，两个文件都留着）、改名之前。每个下载至少有一次检查点（teardown 那次），所以每个位置不管下载多快都会到达（实现者原来另有一个“第二次检查点之前”的位置，依赖时间上有两次检查点，去掉了）。子进程下载中停着的时候，父进程 `On` 和 `Off` 的任务都以 `InUse` 失败；杀掉之后，状态文件有有效的头部和那个位置该有的记录数；父进程续传：完成、内容正确、没有丢弃、保留的字节等于记录的块、只请求其余的字节、锁已放。
  - **§11.5.11 G9 列的三个位置，各由哪个测试覆盖**（lead 要求逐一对应，都经真实的进程或公开 API）：第一次检查点之前 → 崩溃测试的第一个位置（子进程在第一次检查点写记录之前被杀），以及 G9 里位置落在第一块写完之前的种子；检查点进行中 → 崩溃测试的第二个位置（记录已写、同步之前被杀）；改名失败之后 → 崩溃测试的第三个位置（子进程的任务改名失败、保留两个文件之后被杀，父进程续传：每块都保留、一个请求都不发、改名完成）。C20b 的 `test_a_complete_pair_downloads_nothing` 在同一进程里用钩子覆盖同一种情况。
  - `resume_source`：同一个来源续上；另一个或没有 → `SourceMismatch`、两个文件不变；4096 字节通过验证，4097 字节不通过。
- **变异**：4 个，全部被抓到：来源不存进头部、不比对来源、4096 字节被拒、来源不设上限。
- **第一轮审查之后的修改**（codex 第一轮 PASS 88 分，按规则修掉四个 Minor 和一个建议）：
  - 崩溃测试的子进程原来用 `StepLog::gate`，它 30 s 后自己放行，负载高时子进程可能越过要杀的位置。加了只在测试里的 `StepLog::hold`：不设期限，一直等到发送端发送或被丢弃；子进程把发送端留到进程结束，所以停住的那一步只能等被杀。父进程的看门狗和 `KillOnDrop` 不变。
  - G9 原来在服务器的闸门到达时就取消，前缀可能还在客户端的缓冲里，取消时不一定保留了块，`kept_any` 没有保证。改成按协议：位置在第一块之后超过 64 KiB（runner 最多攒着不到一帧，32 KiB，留了余量）的种子，位置之前的每个字节最终都会写入，第一块也在其中，所以在取消之前等任务报告的已写入区间里有一整块（`downloaded_chunks`，teardown 的检查点会记它）；这些种子续传时 `kept_bytes` 至少是一块。每种客户端都有这样的种子（种子在三种客户端上都跑）。断言来自这个协议，不靠时间。G9 因此变成约 15 s（等进度采样报告已写入的区间）。
  - `TaskBuilder::prefer_mode`、`docs/task.md`、README 原来说 `Singleton` 总会被采用；补上例外：`Resume::On` 而可续的任务以 1 个连接走并发模式，好保留状态文件（§11.5.8）。
  - ureq2 的限制原来写得太轻。改为：放弃一个区间时还在运行的阻塞请求或读取，要等调用返回才放开连接；在那之前任务的连接数可以超过 `max_connections`，反复重试或重启时可以有好几个；agent 没设超时、服务器卡住时可以一直这样；链接 #207 和 bolt-load-adapter README 里设超时的说明。
  - 建议：`probe_send.rs` 的 `Attempt` 初始化补上 `resume`、`resume_source`、`file_hooks`。核对方式：照 CI `hra-probe` 的两个命令，用单独的 target 目录，`RUSTFLAGS="--cfg bolt_load_probe_send"` 和加上 `-Zhigher-ranked-assumptions` 各跑一次 `cargo +nightly-2026-11-06 check -p bolt-load --tests`，两个都编译通过。
- **第二轮审查**（codex 第二轮 PASS 97 分，没有 Critical、Major、Minor）：按它的建议，G9 等待用的 `holds_a_block` 原来把最后那块 12,345 字节的短块也算作一整块，等到的条件推不出 `kept_bytes >= BLOCK`；改成要求一个区间覆盖整整 `BLOCK` 字节（`first + BLOCK <= range.end`，不再截到总长），和断言一致。G9 连跑三次都通过。
