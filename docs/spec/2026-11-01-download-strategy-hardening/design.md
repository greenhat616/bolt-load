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
| G6 | 健康的连接不能“洗白”一个反复失败的区间：一个区间累计 `max_attempts_per_range` 次失败后，任务终止，错误里带上该区间最后一次的真实原因（计数规则见 §14.13） | 单元测试（T10） |
| G7 | 新的扩张控制器在三个场景里，完成时间都不比旧实现差，连接数也不比旧实现多：单连接限速、高速、对 429 敏感 | 基准测试（T14），结果写进 PR 描述 |
| G8（P2） | 注入 `Limits` 后，跨任务的总速率误差不超过 `limit` 的 ±10%（2 s 窗口）；限速等待不会触发空闲超时 | 集成测试（T15） |
| G9（P3） | 进程在任意时刻被杀掉后续传，最终 hash 正确；状态文件损坏、越界或重叠时，坏区间被丢弃并上报 | 集成测试（T19） |

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

  这样就去掉了 `strategy/mod.rs:88` 的 `expect`。
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

---

## 9. 扩张控制器与切分尺寸（P1-5、S1、P1-6、P1-7、S4）

### 9.1 开关

- 新增 crate 内部的 `GrowthStrategy { Legacy, Doubling }`。默认是 `Legacy`。
- 在 `#[cfg(any(test, feature = "unstable-strategy"))]` 下，通过 `TaskOptions::growth_strategy` 暴露出来，供测试和基准测试使用。
- 基准测试满足 G7 之后，另开一个 PR 把默认值切换为 `Doubling`，并删除 `Legacy` 和开关。

### 9.2 `Doubling` 控制器

```text
every grow_interval (2 s, aligned with the strategy tick):
  if allowed >= max_connections                         -> stop
  if congestion_until > now                             -> skip
  if any runner is Connecting or has no data this tick  -> skip
  if limits.speed_limit is set and speed >= 0.85 × limit -> skip
  if last_grow exists and speed_window_after < speed_window_before × 1.10
                                                        -> plateau: stop growing
  target = min(allowed × 2, max_connections)
  repeat (target − allowed): split(largest remaining) via §7.2
```

- 降速交给 `ConcurrencyControlStrategy` 处理，扩张控制器只负责增加连接。
- 增益的判断用扩张前后各一个稳定窗口的总速率来比较，不用每连接速率，因为 `per_runner_speed` 本身就是由总速率算出来的。

### 9.3 切分尺寸（S1）

```text
planned = clamp(donor_speed × split_window, min_chunk_size, remaining / 2)
donor_speed = 被切 runner 自己的速率，拿不到时用 throughput.per_runner_speed
split_window = 2 s
```

依据：IDM 的切分候选值，是由被切连接**自己的**速率推出来的（改进方案 §8 A1）。现在的 `current_speed × 2` 用的是任务总速率。按推理（还没有基准测试验证），它会让门槛随连接数近似线性增长。`Legacy` 模式保持原来的公式不变。

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

### 9.6 收尾对冲（D5，跟随 `Doubling`）

IDM 6.43 的做法（IDA 静态分析，2026-11-06，见改进方案 §8 A2）：一条连接下完自己的分片、或者正空闲时，如果已经没有可切的活，而另一条连接已经 2 s 没有收到数据，就从后者已经收到的位置起，重复下载它剩下的同一段。原连接不停。分叉之后谁先写入数据就用谁，另一条关掉。每个分片最多一个对冲，连接数不增加。另做过一次动态测试，只看到了对冲请求：它从停顿的位置开始，当时原连接还开着；胜负没有观察到。

bolt-load 只做这个收尾版。§9.5 的低速重启在剩余量不足 `2 × min_chunk_size` 时不生效，收尾阶段卡住的连接现在只能等 runner 的空闲超时（默认 30 s）。对冲补的就是这一段。

下文的“一对”指同一个 chunk 上的原 runner 和对冲 runner。“分叉点”指发起对冲时 planner 已经接受的连续位置，也就是这个 chunk 的 `downloaded.end`。

#### 触发

需要同时满足：

1. 没有可切的活：没有可领取的未分配区间；所有未完成的 chunk，不论 Pending 还是 Running，剩余量都 < `2 × min_chunk_size`。
2. 能再开一条连接：活跃 runner（一对算两个）少于当前的并发上限（下载循环的 `max_concurrency`，并发控制降级后会变小）；`try_reserve_slot` 能占到槽位（§7.2）；不在 `congestion_until` 之前（§8.1）；没有启用限速。
3. 某个 runner 已经静默 `hedge_idle`（默认 2 s），静默的算法见下。
4. 它的 chunk 还有没收到的字节，并且这次尝试还没有对冲过（见“预算”）。

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
- 败方的帧一律丢弃，不写入，也不计入吞吐。败方被取消：不算失败，不退避，`attempts` 和 `charged`（§14.13）都不变。

#### 错误

胜负分出之前，按出错的一方和错误类别处理：

| 出错的一方 | 错误 | 处理 |
| --- | --- | --- |
| 一方，另一方还在 | 可重试 | 只结束这一方。区间不退回，不退避，`attempts` 和 `charged` 都不变，也不检查区间预算。按 §14.13 的分类计入任务级连续失败，见表下 |
| 一方，另一方还在 | 拥塞（429/503） | 只结束这一方。区间不退回，不退避，`attempts` 和 `charged` 都不变。另外立即按 §8.1 更新 `congestion_until`，并照常记入并发控制的反馈，和普通 runner 遇到拥塞一样（§14.12）。另一方算作在途，所以任务不终止，也不计入连续失败 |
| 任一方 | §4.3 的回退错误 | 照常回退：取消所有 runner，改用单连接 |
| 任一方 | 其它不可重试的错误 | 照常终止任务 |
| 最后一方 | 可重试或拥塞 | 区间按 §8.1 退回，只退避一次：`attempts + 1`；`charged` 最多加 1，规则见“预算”。先查这一对的预算，再按 §14.13 处理最后一方的错误，所以 429/503 也可能以 `RangeExhausted` 结束（见“预算”的判定顺序）。`RangeExhausted` 的 `source` 取这最后一个错误。前一方的 `Retry-After` 已经写进 `congestion_until`，它只推后、不提前（§14.12），所以较长的那个仍然有效 |

任务级连续失败按 §14.13 的分类计数，两方的失败都一样。“收到过数据”按出错的那个物理 runner 自己被接受的字节判断：原 runner 看它领取区间以来被接受的字节，对冲期间也就是 chunk 的已下载部分；对冲 runner 在胜负分出之前总是 0，因为它的第一个非空帧一被取到，胜负就分出了。

- 重启（这个 runner 收到过数据之后的超时、断流或提前结束）：不计入。分叉前交过数据的原 runner 后来超时，就是这种情况。
- 其它可重试的失败：计入，包括没收到过数据的失败、`ChannelClosed` 和可重试的状态码。胜负分出之前失败的对冲 runner 不会是重启。
- 拥塞，并且还有别的尝试在途（另一方也算）：容忍，不计入，这是 `FailurePolicy::on_failure` 现在的做法。没有别的尝试在途时，拥塞照常终止任务，和普通 runner 一样。

胜负分出之后，胜方就是这个 chunk 的普通 runner，按原来的规则处理。败方的事件按下面“待 T27 开工前补齐”的不变量处理。

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
- 这一对记的预算，不少于原 runner 单独失败时记的：原 runner 的失败记预算时，它不是重启，对冲 runner 在胜负分出之前也不会是重启，所以这一对加 1。
- 不会没有进展地一直循环：
  - 每次尝试最多两个 runner（最多对冲一次），一方失败后不再补发。
  - 有一方是重启时，这一对不记预算。重启的一定是原 runner，这次尝试有字节被接受，所以退回的区间严格变短，和 §14.13 的论证一样。
  - 没有重启、至少一方记预算时，`charged + 1`，受 `max_attempts_per_range` 约束。
  - 剩下的只有两方都是 429/503：和 §14.13 一样，只有别的连接还在途时才被容忍，`no_progress_timeout` 照常有效。

#### 待 T27 开工前补齐

一个逻辑 chunk 挂两个 runner 的具体状态机，要等 T11–T13 改完 planner 和策略再定：两方各自的状态转换、败方的清理、对冲 runner 的 spawn 被拒绝、败方的生命周期通道没发 `Stopped` 就关闭、败方还没停下任务就完成。T27 动代码之前，先按 T13 之后的代码在这里补一张状态转换表，评审通过再实现。不管表怎么写，都要守住：

- 每一方都只释放自己的资源和槽位，败方也一样。槽位在确认这个 runner 已经终止、或者没能启动之后，恰好回收一次。确认的方式有三种：它报告了 `Stopped`；它的生命周期通道没发 `Stopped` 就关闭了；它的 spawn 被拒绝。
- 败方不会完成、也不会释放这个逻辑区间。
- 败方迟到的事件（数据、`Stopped`、失败）一律忽略，不去 planner 里查它的 chunk，更不当成失败处理。
- 活跃 runner 的计数两方都算。现在策略用的 `get_active_runners_count` 数的是活跃的 chunk，一对只算一个。

表里要处理的现状（C9）：

- 现在没法单独停掉一个 runner：所有 runner 共用任务的 cancel token。能做的只有丢掉它的 limit sender，它会以 `ChannelClosed` 停下；生命周期处理把这当成可重试的失败，先调用 `mark_failed`。runner 因取消停下时，处理也是先 `mark_failed`，再跳过失败上报（`runner_manager/lifecycle.rs`）。
  - C12 之后：每个 runner 有自己的停止开关（任务令牌的子令牌），`RunnerManager::stop_runner` 只停它一个；被停的 runner 报 `Cancelled` 时按低速重启处理，其余 runner 被取消时仍然跳过失败上报（§14.15）。已经决定怎么结束的 runner（拿着错误或已下完、在交最后一段缓冲或等帧被取走）不理这个开关。对冲的败方可以用同一个开关停下，但 T27 要记下停止的原因（低速重启还是对冲败方），manager 按原因处理，不能把被停之后的 `Cancelled` 一律当低速。
- spawn 被拒绝现在按不可重试的连接失败处理，任务终止（§14.11），处理时同样会 `mark_failed`。
- 生命周期通道没发 `Stopped` 就关闭时，`handle_runner_lost` 也是先 `mark_failed`，再释放槽位；任务还没完成时，上报一次 `ChannelClosed` 的可重试失败（`runner_manager/lifecycle.rs`）。

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

### 11.2 资源身份

- `BoltLoadAdapterMeta` 增加 `etag: Option<String>`（只保存强 ETag）和 `last_modified: Option<String>`。
- adapter 的 context 缓存要能在多次尝试之间刷新。现在的 `OnceCell` 只初始化一次。
- 续传请求带上 `If-Range`：有强 ETag 就用 ETag，否则用 Last-Modified。
  - 返回 `206`：继续下载。
  - 返回 `200`：说明资源已经变了，返回 `ResourceChanged`，丢弃已写区间，从头下载。这种情况**不**走 §4.3 的单连接回退。

### 11.3 状态文件

`<file>.boltload` 采用逐行追加的 JSON Lines 格式：

- 第一行是头部：`{v, url, total, etag, last_modified}`；
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

---

## 12. 风险与已知限制

| 风险 | 缓解 |
| --- | --- |
| P0-2 和 P0-1 改动了公开的错误类型，下游的 `match` 会编译失败 | 在同一个 PR 里加上 `#[non_exhaustive]`，以后再扩展就不会破坏兼容；CHANGELOG 标明 breaking |
| 有些服务器的 `Content-Range` 写得不规范（比如省略 `bytes` 前缀、带多余空格），会被严格校验拒掉 | 解析器容忍空白和大小写差异，其它格式偏差一律拒收。宁可回退到单连接，也不能写坏文件 |
| `max_consecutive_failures` 的默认值在多核机器上会变小（例如 32 核机器从 64 降到 16） | 加了每区间预算之后，连续失败计数只是最后一道保险，影响有限；写进 CHANGELOG |
| `Doubling` 在某些网络环境下可能比 `Legacy` 差 | 默认值切换以 T14 的基准测试数据为准，在那之前 `Legacy` 一直是默认 |
| 令牌桶粒度是一帧（emit 每次推送的大小），低限速时可能有抖动 | 桶深设下限；G8 的误差判据按 2 s 窗口计算 |
| HTTP 客户端自动解压时会删掉 `Content-Encoding`，§4.1 的编码检查看不到（见 §14.2） | 所有请求都带 `Accept-Encoding: identity`；adapter 关闭 ureq 的 `gzip`；`limit_range_body` 要求 body 长度等于 `Content-Range`，不符就报 `RangeBodyTooLong` 或 `RangeBodyTooShort`；解不开的 body 不重试。这些只保证任务失败而不写错。T5 起，会解压的客户端回退到单连接后能下完；C22 起，不解压的客户端由引擎解码后也能下完 |
| 服务器把 `.tar.gz` 这类压缩包标成 `Content-Encoding: gzip`，解码后用户拿到的是解压后的文件 | §4.4 的例外：`Content-Type` 本身就是这种压缩格式时，按原样写入 |
| D11 把 HTTP 函数从 core 和 utils 移到 `bolt-load-http`，外部 adapter 的 import 会编译失败 | 提交信息加 `!`，列出新旧路径对照（§4.5 的表）；函数签名不变，只改 import 和依赖 |
| 解压炸弹：很小的响应解出极大的文件 | 每次 poll 最多产出一块输出，内存有上界；`DecodeOptions::max_output` 可以给输出总量设上限 |
| 没有长度、靠关连接结束的 body 在第一个字节之前断开，会存成空文件（§14.7） | HTTP 层测不出这种截断，断在之后任何位置也一样放行；IDM 同样按完成处理。原来的 `Empty` 只拦下了断在第一个字节之前这一种，代价是合法的空文件全部失败 |
| IDM 的部分结论只有 dsh 一个来源（ECONNRESET 计数、HTTP 低限速时的初始连接数） | 本设计不依赖这些结论；需要用到的参数都通过自己的基准测试来定。对冲的触发条件和胜负规则由 IDA 静态分析确认（2026-11-06）。那次动态测试只看到第二个请求从停顿的位置开始，当时原连接还开着，没有看到胜负。§9.6 只借用它的结构，参数自己校准 |

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
  - G6 仍然成立：它针对的是“健康连接掩盖一个反复失败的区间”。没有进展的失败都记到这个区间的预算上，别的连接写成功也不会清零它。每次都有进展的区间不算反复失败，它会一点点下完。
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
  - 速度窗口：
    - 每个策略 tick 采样一次，时间用 tick 传进来的 `now`，策略里不调用 `Instant::now()`。
    - 任务速度 = 这个 tick 里 planner 接受的字节数 ÷ tick 的时长。比较的是整个任务的速度，不是每个 runner 的速度。
    - 扩张前的窗口，是决定扩张的那个 tick；扩张后的窗口，是之后第一个通过检查 1–6 的 tick。两个窗口都是一个 tick 长（1.5 s）。
  - 停滞：
    - 只判断完整的翻倍，也就是这次扩张加的 runner 数等于扩张前的活跃数。扩张后窗口的速度低于扩张前的 1.10 倍，就是停滞。停滞之后，这次下载不再扩张。
    - 被上限截断、或者有切分被拒绝的扩张，照样算扩张，2 s 的间隔从它算起，但不判断，也不会解除已有的停滞。连接数只多一小步，本来就不能指望总速度多 10%。比如并发控制把上限一格一格地恢复（11 到 12），每条连接各 1 MiB/s 时增益只有 9%，判断的话就会永久停在 12。
    - 每次翻倍只判断一次。判断时，如果活跃 runner 比翻倍刚完成时少（有 runner 下完了或者失败了），这个窗口就量不出这次翻倍的效果。这时不下结论，这次判断作废，可以继续扩张。
    - 翻倍到上限之后，检查 3 先拦住，所以这次翻倍不会被判断。等 runner 下完、活跃数降下来，判断作废，`Doubling` 会再切分，补回到上限。
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
  - 只判断完整的翻倍，所以扩张如果一直只是部分的，就永远不会下结论：比如上限一格一格地恢复，或者在上限处反复 8 → 7 → 8（一个 runner 下完、再切一个补上）。这时即使真的停滞了（总速度不再变化）也发现不了，循环会一直切分补位。这是接受了的盲区：连接数不超过当前上限，每次补位之间至少隔 2 s。T14 要量它的代价。`test_refills_at_the_limit_are_never_judged` 和 `doubling_refills_a_step_at_a_time_without_any_gain` 把这个行为固定下来。
  - 剩余最多的 chunk 优先：它不到自己的门槛就停下，不再看别的 chunk。别的 chunk 剩得少一些、但 runner 更慢，门槛更低，本来可以切，也不会被选上。T14 要测连接速度不一的场景。
  - 速率只看一个 tick（1.5 s）。runner 攒满 32 KiB 才交一帧，所以慢于约 21 KiB/s（32 KiB ÷ 1.5 s）的 runner，有的 tick 里看起来没有数据，那个 tick 就不扩张。
  - writer 满的时候，下载循环不取 runner 的数据，这个 tick 看起来也没有数据，不扩张。这是有意的：瓶颈在写入时，加连接没有用。
  - `planned` 的下限是 `min_chunk_size`，所以剩余量在 `min_chunk_size` 到 `2 × min_chunk_size` 之间、donor 又慢的 chunk 也会被切，两半都小于 `min_chunk_size`。`Legacy` 的 `SplitGivenTask` 也是这样（只要求剩余量 ≥ `min_chunk_size`）。T27 的“没有可切的活”按 `2 × min_chunk_size` 定义（§9.6），和这里的门槛不一样，T27 开工时要对齐。
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
    - `doubling_grows_back_as_the_limit_recovers_a_step_at_a_time`：经过真实的 `StrategyControl` 和 `ConcurrencyControlStrategy`。8 个 runner，`max_connections = 16`；一个 503 让并发控制把上限降到 8；30 s 后开始恢复，每 15 s 加 1。每个 runner 每个 tick 交同样多的字节，总速度和 runner 数成正比，10 个以后每一步的增益都不到 10%。上限依次是 8 到 16，循环跟着一格一格扩张，最后是 16 个 runner，没有停滞；任何时候活跃数都不超过当时的上限；
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
    - 被停的 runner 在处理 `Stopped` 之前仍然是活跃 runner，也还占着槽位。之后活跃数少 1。如果正有一次翻倍等着判断，这次判断作废（§14.14：活跃数少于翻倍刚完成时，不下结论）。这是合理的：少了一个 runner，这个 tick 量不出翻倍的效果，既不能说停滞，也不能说有增益。
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
  - 默认值 10 s 和 0.25 还没有经过基准测试校准，留给 T14。
  - 已经决定怎么结束的 runner 不理停止开关：正在交最后一段缓冲时，它要等数据通道有空位；在 drain 时，最多等 5 s 的 drain 超时。这段时间它一直占着槽位，也不会被再次评判。
  - 被停的 runner 的生命周期通道没发 `Stopped` 就关闭时，仍按丢失处理（`ChannelClosed`，记预算）。正常的 runner 被取消时总会发 `Stopped`。
  - manager 把“被停、报 `Cancelled`”一律当低速停止。T27 的对冲败方也要用这个停止开关，那时要先记下停止的原因（低速重启还是对冲败方），再分别处理，见 §9.6 的说明。
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
