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
| G6 | 健康的连接不能“洗白”一个反复失败的区间：一个区间累计 `max_attempts_per_range` 次失败后，任务终止，错误里带上该区间最后一次的真实原因 | 单元测试（T10） |
| G7 | 新的扩张控制器在三个场景里，完成时间都不比旧实现差，连接数也不比旧实现多：单连接限速、高速、对 429 敏感 | 基准测试（T13），结果写进 PR 描述 |
| G8（P2） | 注入 `Limits` 后，跨任务的总速率误差不超过 `limit` 的 ±10%（2 s 窗口）；限速等待不会触发空闲超时 | 集成测试（T15） |
| G9（P3） | 进程在任意时刻被杀掉后续传，最终 hash 正确；状态文件损坏、越界或重叠时，坏区间被丢弃并上报 | 集成测试（T19） |

---

## 3. 决策

下表默认采用改进方案 §6 的建议值。“状态”一列写“待确认”的条目，要在对应 PR 开工前由维护者拍板；拍板结果改动这里，tasks 跟着调整。

| # | 决策 | 采用 | 状态 |
| --- | --- | --- | --- |
| D1 | `max_connections` 的默认值 | 固定 8，与 CPU 核数无关 | 待确认 |
| D2 | Range 用不了时怎么办 | 自动截断文件，改用单连接从头下载，并通过 `TaskStatus` 上报降级。触发条件除了 `RangeIgnored`，还包括 range 响应带了内容编码，或者能看出被客户端解过码（§4.3），和 IDM 一致 | 已确认（2026-11-03，扩展到内容编码） |
| D3 | 其它 4xx 归为不可重试 | 采纳（对齐 ureq2）；408 例外 | 已确认（2026-11-01，随 PR-1 开工） |
| D4 | P2 什么时候做 | P1 完成之后再评估 | 待确认 |
| D5 | 冗余对冲 | 不做 | 待确认 |
| D6 | 切分尺寸改用单连接速率 | 和扩张控制器放在同一个开关后面，一起做 A/B | 待确认 |
| D7 | 按 host 的能力缓存 | 随 P2 一起做，TTL 1 h，不持久化 | 待确认 |
| D8 | 限速的公平性 | 所有 runner 共享一个桶，不做按任务的公平分配 | 待确认 |
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

处理：从已经收到的位置停掉这个 runner，区间退回可用池，`attempts += 1`，走退避。默认参数先按上面的值实现，再通过 T13 的基准测试校准，**不照搬** IDM 的 1.3、50 ms 这些常量。

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
| `Doubling` 在某些网络环境下可能比 `Legacy` 差 | 默认值切换以 T13 的基准测试数据为准，在那之前 `Legacy` 一直是默认 |
| 令牌桶粒度是一帧（emit 每次推送的大小），低限速时可能有抖动 | 桶深设下限；G8 的误差判据按 2 s 窗口计算 |
| HTTP 客户端自动解压时会删掉 `Content-Encoding`，§4.1 的编码检查看不到（见 §14.2） | 所有请求都带 `Accept-Encoding: identity`；adapter 关闭 ureq 的 `gzip`；`limit_range_body` 要求 body 长度等于 `Content-Range`，不符就报 `RangeBodyTooLong` 或 `RangeBodyTooShort`；解不开的 body 不重试。这些只保证任务失败而不写错。T5 起，会解压的客户端回退到单连接后能下完；C22 起，不解压的客户端由引擎解码后也能下完 |
| 服务器把 `.tar.gz` 这类压缩包标成 `Content-Encoding: gzip`，解码后用户拿到的是解压后的文件 | §4.4 的例外：`Content-Type` 本身就是这种压缩格式时，按原样写入 |
| D11 把 HTTP 函数从 core 和 utils 移到 `bolt-load-http`，外部 adapter 的 import 会编译失败 | 提交信息加 `!`，列出新旧路径对照（§4.5 的表）；函数签名不变，只改 import 和依赖 |
| 解压炸弹：很小的响应解出极大的文件 | 每次 poll 最多产出一块输出，内存有上界；`DecodeOptions::max_output` 可以给输出总量设上限 |
| 没有长度、靠关连接结束的 body 在第一个字节之前断开，会存成空文件（§14.7） | HTTP 层测不出这种截断，断在之后任何位置也一样放行；IDM 同样按完成处理。原来的 `Empty` 只拦下了断在第一个字节之前这一种，代价是合法的空文件全部失败 |
| IDM 的部分结论只有 dsh 一个来源（对冲、ECONNRESET 计数、HTTP 低限速时的初始连接数） | 本设计不依赖这些结论；需要用到的参数都通过自己的基准测试来定 |

---

## 13. 不在范围

- 冗余对冲（hedged download），以及 IDM 的 `AddAlternative` / `ApplyAlternative`（D5）。
- 每个分片一个临时文件、最后 `JoinFile` 合并；用注册表做 journal。
- 尾片合并（`find_merge_candidate`）：依据不成立，见改进方案 §2.3。
- IDM 的常量：11680 / 23360 / 46720、30 / 120 / 300 s、`2Q+3`、64 KiB 对齐。
- 每次切分前检查剩余磁盘空间。
- 日程配额（MB / 时间窗）、优先级队列、按任务加权的公平分配。
- 自适应量子 Q：IDA 已经证实 IDM 的 Q 是固定值。

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
