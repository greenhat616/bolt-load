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
| D2 | 遇到 `RangeIgnored` 时怎么办 | 自动截断文件，改用单连接从头下载，并通过 `TaskStatus` 上报降级 | 待确认 |
| D3 | 其它 4xx 归为不可重试 | 采纳（对齐 ureq2）；408 例外 | 已确认（2026-11-01，随 PR-1 开工） |
| D4 | P2 什么时候做 | P1 完成之后再评估 | 待确认 |
| D5 | 冗余对冲 | 不做 | 待确认 |
| D6 | 切分尺寸改用单连接速率 | 和扩张控制器放在同一个开关后面，一起做 A/B | 待确认 |
| D7 | 按 host 的能力缓存 | 随 P2 一起做，TTL 1 h，不持久化 | 待确认 |
| D8 | 限速的公平性 | 所有 runner 共享一个桶，不做按任务的公平分配 | 待确认 |

---

## 4. Range 响应校验（P0-1）

### 4.1 共享校验函数

位置：`bolt-load-core/src/adapter/range.rs`（新增），导出为 `bolt_load_core::adapter::validate_range_response`，reqwest 和 ureq2 都调用它。理由和 §5.1 相同：校验规则是 adapter 和引擎之间的约定，外部 adapter 只依赖 core 就能用上。
`ContentRange` 也在 core 里（错误类型 `RangeMismatch` 要引用它），解析通过 `impl FromStr for ContentRange` 提供，不放进 utils，因为 utils 不依赖 core。

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

1. runner 收到 `RangeIgnored` 后，下载循环把它升级为 `DownloadError::RangeUnsupported`，取消所有 runner，并且不再接收新的写入。
2. `drive.rs` 在 `initialize` 之后的分支里捕获这个错误：等所有写入完成，截断目标文件到 0，然后以 `DownloadMode::Singleton` 重新运行一次。
3. `TaskStatus` 里要能观察到这次降级（具体字段见 T5，沿用 `task-api-spec.md` 的状态模型，不新增事件通道）。
4. 每个任务最多回退一次。Singleton 模式用的是 `full_stream()`（`singleton_task.rs:144`），不经过 §4.1 的校验，所以回退后不会再触发 `RangeIgnored`。

---

## 5. 状态码分类与错误类型（P0-2）

### 5.1 `classify_status`

位置：
- 分类函数在 `bolt-load-core/src/adapter/status.rs`（新增），导出为 `bolt_load_core::adapter::classify_status`。这个映射是 adapter 和引擎之间的约定（`Congestion` 触发降级，`Retryable` 计入重试），外部 adapter 只依赖 core 也能用上它，不必再依赖 `bolt-load-adapter`。
- `Retry-After` 的解析在 `bolt-load-utils` 的 `http::parse_retry_after`（新增）。它只做纯解析，不设上限。utils 不依赖 core，这样分层不会倒置。

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
