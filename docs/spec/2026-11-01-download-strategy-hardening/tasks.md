# 下载策略加固 任务

关联设计：`design.md`（同目录）

## 0. 全局约束

1. **每个提交都必须能编译，并通过 lefthook 的 pre-commit 检查**：
   - `cargo fmt --all -- --check`
   - `cargo clippy --all-targets --all-features -- -D warnings`

   每个 PR 合入前还要通过 `cargo test --workspace --all-features`。
2. **提交信息遵守 Conventional Commits**（`commitlint.config.ts`），scope 用 kebab-case。每个提交只做一件事。
3. **不保留兼容层**：错误类型改名、加变体，`TaskOptions` 字段改名，都在同一个提交里把调用方和测试一起改完。breaking change 要在提交信息里加 `!`，并写清迁移方式。
4. **测试不用 sleep**：
   - 退避、冷却、增益窗口相关的逻辑，都把 `now: Instant` 作为参数传入（design §8.1）。
   - 故障注入测试使用 `bolt-load-tests` 的服务器，不访问外网。
5. **开工前的决策**：PR-1 开工前，确认 design §3 的 D3；PR-3 开工前，确认 D2；PR-5 开工前，确认 D1；PR-7 开工前，确认 D5 和 D6；PR-8 开工前，确认 D4、D7、D8。确认结果先写回 `design.md`，再动代码。
6. **偏差记录**：实现和 design 不一致时，在 design 末尾追加“实现说明”一节，写明原因。不要悄悄偏离设计。

## 1. 提交与 PR 划分

| PR | 提交 | 任务 | 依赖 |
| --- | --- | --- | --- |
| PR-1 错误分类 | C1 `feat(core)!: mark adapter errors and meta non-exhaustive and add status variants` | T1 | — |
| | C2 `fix(adapter): classify HTTP statuses the same way in reqwest and ureq2` | T2 | |
| PR-2 Range 校验 | C3 `fix(adapter)!: reject range responses whose offsets cannot be proven` | T3 | PR-1 |
| | C4 `test(tests): add range fault scenarios to the test server` | T4 | |
| PR-3 单连接回退 | C5 `feat(task): fall back to a single connection when ranges are ignored` | T5 | PR-2 |
| PR-4 写错误 | C6 `feat(task)!: keep the io error kind and range of write failures` | T6 | — |
| PR-5 连接上限 | C7 `feat(task): make the connection limit configurable` | T7 | — |
| | C8 `fix(task): reserve a runner slot before splitting a chunk` | T8 | |
| PR-6 退避与预算 | C9 `feat(task): back off failed ranges and honour Retry-After` | T9 | PR-1、PR-5 |
| | C10 `feat(task): budget failures per range` | T10 | |
| PR-7 扩张控制器 | C11 `feat(task): add the doubling growth strategy behind unstable-strategy` | T11、T12 | PR-5、PR-6 |
| | C12 `feat(task): restart slow runners under the doubling strategy` | T13 | |
| | C13 `bench(task): compare the growth strategies` | T14 | |
| PR-7b 切换默认值 | C14 `feat(task)!: make doubling the default growth strategy` | T15 | PR-7，且 G7 达标 |
| PR-8 跨任务限额（D4） | C15 `feat(task): admit tasks and cap connections per host through shared limits` | T16 | PR-5 |
| | C16 `feat(runner): shape throughput with a shared token bucket` | T17 | |
| | C17 `feat(task): remember hosts that ignore ranges` | T18 | PR-3 |
| PR-9 续传 | C18 `feat(task): track written ranges separately from received ranges` | T19 | PR-1、PR-2 |
| | C19 `feat(core)!: carry validators in adapter meta and send If-Range` | T20 | |
| | C20 `feat(task): resume from a checked state file` | T21 | |

**合并约束**：

- C1 一改错误类型，两个 adapter 和 `failure_policy.rs` 里的 `match` 就编译不过了，所以 C1 必须把它们一起改完。C1 只做机械性的改名，分类行为的修正留给 C2。
- PR-4、PR-5 和 P0 的 PR 相互独立，可以并行开发。

---

## T1 — 错误类型与 meta（C1）

- [ ] `bolt-load-core/src/adapter/error.rs`：
  - `UnretryableError` 加上 `#[non_exhaustive]`；
  - 新增 `Congestion { status, retry_after }`，替换 `ServiceUnavailable`；
  - 新增 `Status { status }`、`RangeIgnored`、`RangeMismatch { requested, got }`、`RangeNotSatisfiable { total }`；
  - `Unauthorized`、`NotFound` 带上 `status`（design §5.2）。
- [ ] `RetryableError` 加上 `#[non_exhaustive]`，新增 `Status { status }`；同步更新 `UnretryableError::from_retryable_error`。
- [ ] 新增 `ContentRange { first, last, complete }`，放在 core 里，因为错误类型要引用它。
- [ ] `BoltLoadAdapterMeta` 加上 `#[non_exhaustive]`，提供 `new(content_size)` 和 `with_filename`；所有构造点（adapter、测试、`bolt-load-tests`）都改为调用它们。
- [ ] `failure_policy.rs:18` 的 `is_congestion` 和 `runner_manager.rs:902` 的测试构造，都改用 `Congestion`。
- [ ] 两个 adapter 原有的映射先机械地对应到新变体上，行为暂时不变。

**验证**

- `cargo test --workspace --all-features` 通过。
- `rg "ServiceUnavailable" crates` 无结果。

## T2 — `classify_status`（C2）

- [ ] 新增 `bolt-load-adapter/src/status.rs`：`classify_status(status, retry_after, now)`，按 design §5.1 的表实现；`Retry-After` 支持秒数和 HTTP-date 两种格式，结果夹在 `[0, 120 s]`。
- [ ] reqwest 的 `From<ReqwestError>`（`reqwest.rs:79-115`）：只要有状态码，就调用 `classify_status`，删掉嵌套在 `is_client_error()` 里的 match。
- [ ] ureq2 的映射（`ureq2.rs:79`）改为调用 `classify_status`。
- [ ] 两个 adapter 在出错时都要取出 `Retry-After` 头，传给 `classify_status`。

**验证**

- 表驱动测试覆盖 401、403、404、408、410、416、429、500、503、599、409、412、418。每一行都在 reqwest 和 ureq2 上各跑一次，断言两边分类相同（G2）。
- `Retry-After` 的测试用例：`"5"`、`"0"`、`"-1"`、`"999"`（夹到 120 s）、一个过去的 HTTP-date、一个未来的 HTTP-date、一个非法字符串。

## T3 — Range 校验与 meta 探测（C3）

- [ ] 新增 `bolt-load-adapter/src/range.rs`：`parse_content_range` 和 `validate_range_response`（design §4.1）。解析时容忍空白和大小写差异。
- [ ] 两个 adapter 的 `range_stream`：
  - 请求带上 `Accept-Encoding: identity`；
  - 先拿状态码和响应头调用 `validate_range_response`，通过之后才取 body。
- [ ] `fetch_remote_meta`：只有 `206` 加上合法的 `Content-Range`，才认定支持 Range；删掉 `ContentLength(0)` 那个分支（`reqwest.rs:245-246`）。ureq2 做同样的改动。
- [ ] 在 `stream.rs` 的注释里写明：`last < end − 1` 时，按短读路径处理。

**验证**

- `range.rs` 单元测试覆盖：
  - 合法的 `206`；
  - 起点不一致；
  - `last ≥ end`；
  - `complete` 不一致；
  - `complete` 为 `*`；
  - 缺少头；
  - `multipart/byteranges`；
  - 带 `Content-Encoding: gzip`；
  - `200`；
  - `416` 带 `*/t`，以及 `416` 不带这个头；
  - 大小写与空白的各种变体。

## T4 — 故障注入（C4）

- [ ] `bolt-load-tests/src/adapter/http_server.rs` 和 `fault.rs` 新增以下服务器行为：
  - 忽略 Range，返回 `200` 全量；
  - `206` 但起点偏移一个字节；
  - `206` 但 `complete` 不一致；
  - 一律返回 `416`；
  - 返回 `503` 并带 `Retry-After: 2`；
  - 对 range 请求返回 gzip 编码；
  - body 中途截断。
- [ ] 集成测试：对上面每一种行为，分别用两个 adapter 各跑一次完整任务。断言要么最终文件 hash 正确，要么任务以对应的类型化错误失败，**绝不允许**出现 `Completed` 但 hash 错误（G1）。
- [ ] 回退相关的场景，在 PR-2 里先断言为失败（返回 `RangeIgnored`）；PR-3 合入后，再改成断言回退成功。

**验证**

- `cargo test -p bolt-load-tests` 通过。

## T5 — 单连接回退（C5）

- [ ] 在 `DownloadLoop` 里，把 runner 返回的 `RangeIgnored` 升级为 `DownloadError::RangeUnsupported`：取消所有 runner，等写入全部完成。
- [ ] `task/instance/drive.rs`（第 202 行附近）捕获这个错误：截断目标文件，以 `DownloadMode::Singleton` 重新运行。每个任务只回退一次。
- [ ] 在 `TaskStatus` 里表达这次降级：先读 `crates/bolt-load/docs/task-api-spec.md` 的状态模型，再选择新增字段或复用现有字段，并在 design 里记录选择结果。
- [ ] `crates/bolt-load/docs/task-api-spec.md` 补充一条规则，说明这次降级在状态中如何体现。

**验证**

- T4 中“忽略 Range”的场景改为断言：最终 hash 正确，并且观察到了降级。
- 新增测试：回退之后，文件里不残留多连接阶段写入的数据（用前后内容不同的测试文件来验证）。

## T6 — 写错误结构化（C6）

- [ ] 新增 `TaskError::Write { range, source: Arc<io::Error> }`，替换 `completion.rs:16-19` 里的 `Other`。
- [ ] `StorageFull` 和 `QuotaExceeded` 在 `Display` 里给出明确提示；`FailurePolicy` 不对它们重试。

**验证**

- 用一个注入写失败的 writer 做单元测试：断言 `ErrorKind` 和 `range` 都被保留，任务以 `Write` 失败结束。

## T7 — 连接上限（C7）

- [ ] `TaskOptions::max_concurrency`（`options.rs:111-113`）改为公开的 `max_connections: NonZeroUsize`。新增 `DEFAULT_MAX_CONNECTIONS = 8`（D1），`TaskBuilder::max_connections`。
- [ ] `download_loop.rs:77-83` 改为读取这个配置，不再调用 `available_parallelism()`。
- [ ] `default_max_consecutive_failures()`（`options.rs:13-16`）改为 `max(8, 2 × max_connections)`。由于它依赖 `max_connections`，计算时机改为 build 阶段。
- [ ] 更新 README 和 crate 文档里关于并发的描述。

**验证**

- 单元测试：
  - 默认值是 8；
  - 设置为 1 时，始终只有一个 runner；
  - 失败预算的默认值随 `max_connections` 变化。
- `rg "available_parallelism" crates/bolt-load/src/task` 只剩下与连接数无关的用法，或者没有结果。

## T8 — 容量安全的切分（C8）

- [ ] `RunnerManager` 新增 `try_reserve_slot()` 和 `release(slot)`。`split()` 改为三步：先占槽位，再 resize，最后 spawn；任何一步失败都回滚（design §7.2）。
- [ ] `spawn_runner`（`strategy_tick.rs:93-99`）删掉 `expect`，槽位不足时记一条 `trace` 后跳过。
- [ ] `find_chunk_to_split` 改为返回 `SplitCandidate`；删掉 `strategy/mod.rs:88` 的 `expect`。
- [ ] `dynamic_strategy.rs:77,90` 的闸门改为 `current + n <= max`。

**验证**

- 性质测试（`proptest`；如果工作区还没有这个依赖，就用固定种子的随机序列代替）：随机交替执行 split、完成、失败、`ChangeMaxConcurrency`。断言：
  - 不会 panic；
  - `active <= max`；
  - 所有区间之和覆盖 `[0, total)`，且互不重叠。
- 回归测试：`IdGenerator` 满的时候执行 `SplitAllTask`，不会 panic，也不会丢失区间。

## T9 — 区间退避与 `Retry-After`（C9）

- [ ] `ChunkPlanner` 的可用区间增加 `attempts` 和 `not_before`。切分时，新区间的 `attempts` 从 0 开始；合并时取较大值（design §8.1）。
- [ ] runner 失败、区间退回可用池时，计算退避：使用 backon 的 `ExponentialBuilder`，参数为 0.5 s、×2、上限 30 s、满抖动。
- [ ] 增加任务级的 `congestion_until`：遇到 `Congestion { retry_after }` 时更新它，取值 `max(retry_after, backoff)`，上限 120 s。
- [ ] `ConcurrencyControlStrategy` 下发 `CreateTask` 时，跳过还在冷却期的区间；在 `congestion_until` 之前，不新建任何连接。
- [ ] `download_loop/deadline.rs` 增加一个唤醒点，取“最早的 `not_before`”和 `congestion_until` 中较早的那个。

**验证**

- 确定性测试（显式传入 `now`）：
  - 区间失败之后，在 `not_before` 之前不会被重新领取，之后会；
  - `Retry-After: 2` 时，2 s 内本任务没有任何新连接；
  - 切分和合并之后，`attempts` 符合规则。
- T4 的 `503 + Retry-After` 场景：断言两次请求之间的间隔 ≥ 2 s（用服务器记录的请求时间来判断）。

## T10 — 每区间预算（C10）

- [ ] `FailurePolicy` 改为三层（design §8.2）：
  - 收到过数据之后的 `Timeout` 或断流，不计入连续失败；
  - `attempts >= max_attempts_per_range` 时，返回 `TaskError::RangeExhausted { range, source }`；
  - 任务级连续失败计数保持不变。
- [ ] 新增 `TaskOptions::max_attempts_per_range`，默认 5。

**验证**

- 单元测试：
  - 一个区间一直失败、其它连接持续写成功时，任务以 `RangeExhausted` 终止，`source` 是该区间最后一次的错误（G6）；
  - 收到过数据之后的空闲超时，不增加连续失败计数；
  - 从未收到数据的超时，仍然计入。

## T11 — `Doubling` 扩张控制器（C11）

- [ ] 新增 crate 内部的 `GrowthStrategy { Legacy, Doubling }`。新增 feature `unstable-strategy`，在 `#[cfg(any(test, feature = "unstable-strategy"))]` 下暴露 `TaskOptions::growth_strategy`。
- [ ] 新增 `strategy/doubling_strategy.rs`，按 design §9.2 实现：
  - 2 s 冷却；
  - 有 runner 不活跃时跳过；
  - `congestion_until` 和 85% 限速条件下跳过（限速部分在 P2 之前恒为 false）；
  - 扩张后增益不到 10% 就停止扩张；
  - 每次扩张 `target = min(2 × allowed, max)`，通过 T8 的 `split` 逐个落地。
- [ ] `Legacy` 的路径保持原样，包括 `abs()` 和原来的阈值，作为 A/B 的对照组。

**验证**

- 构造速率序列做单元测试，覆盖以下情况：
  - 速率稳定增长时，连接数按 1→2→4→8 增长；
  - 增益低于 10% 时停止扩张；
  - 存在不活跃的 runner 时不扩张；
  - 在 `congestion_until` 之前不扩张；
  - 降速时不扩张。

## T12 — 切分尺寸（C11）

- [ ] 在 `Doubling` 模式下，`planned_chunk_size` 按 design §9.3 计算：`clamp(donor_speed × 2 s, min_chunk_size, remaining / 2)`。`donor_speed` 取被切 runner 自己的速率；如果 runner 还没有速率样本，就用 `per_runner_speed`。
- [ ] 如果 `RunnerManager` 目前没有按 runner 统计的速率，先在 `throughput.rs` 里补上这项统计。
- [ ] `Legacy` 模式保持 `current_speed * 2.0`（`strategy/mod.rs:80`）不变。

**验证**

- 单元测试：
  - 总速率相同时，8 个 runner 和 2 个 runner 算出的 `planned` 相同（只要 donor 的速率相同）；
  - `planned` 夹在下限和上限之间。

## T13 — 低速 runner 重启（C12）

- [ ] 只在 `Doubling` 模式下生效。触发条件和处理方式见 design §9.5，默认值 `slow_window = 10 s`、`slow_ratio = 0.25`、至少 3 个活跃 runner、限速开启时不生效。
- [ ] 重启时，区间从已经接收的位置退回可用池，`attempts += 1`，然后走 T9 的退避。

**验证**

- 单元测试：
  - 一个 runner 持续慢于中位数的 25% 时会被重启；
  - 剩余字节不足 `2 × min_chunk_size` 时不重启；
  - 只有 2 个 runner 时不重启。

## T14 — 基准测试（C13）

- [ ] 在 `crates/bolt-load/benches/` 新增 `growth_strategy.rs`，用 `bolt-load-tests` 的限速服务器，构造三个场景：
  1. 单连接限速 1 MiB/s、总带宽 16 MiB/s；
  2. 不限速的本地高速场景；
  3. 连接数超过 4 条就返回 429。
- [ ] 对每个场景，记录 `Legacy` 和 `Doubling` 两种策略的完成时间、峰值连接数、429 次数。每组至少跑 10 次，取中位数和 p90。
- [ ] 结果写进 PR-7 的描述，并作为 design §9.5 默认参数的校准依据。如果参数需要调整，同步改 design。

**验证**

- `cargo bench -p bolt-load --features unstable-strategy --bench growth_strategy` 能跑完。
- 对照 G7 判断是否达标，并在 PR 描述里写明结论。

## T15 — 切换默认值（C14，G7 达标之后）

- [ ] 把 `Doubling` 改为默认策略，删掉 `Legacy`、`dynamic_strategy.rs` 和 `unstable-strategy` 这个 feature。
- [ ] 测试里凡是依赖旧扩张节奏的断言，都按新策略改写。

**验证**

- `rg "threashold|DynamicStrategy|unstable-strategy" crates` 无结果。

---

## T16 — 准入与按 host 的连接数（C15，P2）

- [ ] 新增 `bolt_load::Limits`（`max_active_tasks`、`max_connections_per_host`）和 `TaskBuilder::limits(Arc<Limits>)`；不注入时，行为与现在完全一致。
- [ ] 准入：任务在 `Initializing` 阶段获取许可，按 FIFO 顺序。
- [ ] 按 host 限制：runner 建连前获取许可，停止时释放。扩张控制器把“拿不到许可”当作槽位已满处理。

**验证**

- 集成测试：
  - `max_active_tasks = 2` 时提交 4 个任务，任何时刻最多只有 2 个在下载；
  - 两个任务下载同一个 host、`max_connections_per_host = 3` 时，服务器观察到的并发连接数不超过 3。

## T17 — 共享令牌桶（C16，P2）

- [ ] 在 `Limits` 里新增 `speed_limit: Option<NonZeroU64>`。令牌桶的参数见 design §10.3：速率为 `limit`，桶深为 `max(limit × 0.25 s, 64 KiB)`。
- [ ] runner 在 `Emission` 推送前申请令牌。单次等待上限为 `min(2 s, stream_idle_timeout / 4)`，超过上限就分段等待；每段之间都能响应取消和控制信号。
- [ ] 扩张联动：
  - 速率达到 85% 的限速时，不再扩张；
  - 首批连接数按 `ceil(limit / per_runner_estimate)` 计算；
  - 限速开启时，T13 的低速重启不生效。

**验证**

- 集成测试：
  - 3 个任务共享 2 MiB/s 的限速，按 2 s 窗口统计的总速率在 ±10% 以内（G8）；
  - 把 `stream_idle_timeout` 设成 3 s、限速设成 64 KiB/s，任务能够完成，不出现超时重连。

## T18 — 按 host 的能力缓存（C17，P2，D7）

- [ ] `Limits` 新增 `host_caps`。发生 T5 的回退之后，记录 `host → SingleConnection`，TTL 1 h；之后发往这个 host 的任务直接以 Singleton 模式起步。
- [ ] 新增 `TaskBuilder::force_multi_connection(bool)` 作为豁免开关。

**验证**

- 集成测试：
  - 对同一个“忽略 Range”的服务器提交第二个任务时，服务器看不到 range 请求；
  - TTL 过期后，会重新尝试多连接；
  - 设置豁免后，照常尝试多连接。

---

## T19 — 双位点（C18，P3）

- [ ] `ChunkPlanner` 增加 `written` 位点，只有收到 `WriteCompletion` 之后才推进。`Progress.downloaded_chunks` 改为上报 `written`。
- [ ] 进度先于写入更新（`runner_manager/data.rs:154`）的行为保持不变，但在注释里写明：只有 `downloaded` 位点受它影响。

**验证**

- 单元测试：写入还没完成时，`written` 落后于 `downloaded`；写入失败时，`written` 不前进。

## T20 — 验证器与 `If-Range`（C19，P3）

- [ ] `BoltLoadAdapterMeta` 增加 `etag`（只保存强 ETag）和 `last_modified`；两个 adapter 都在探测阶段读取这两个值。
- [ ] adapter 的 context 缓存（`OnceCell`）改为可以在多次尝试之间刷新。
- [ ] 续传时，range 请求带上 `If-Range`。这时收到 `200`，返回 `ResourceChanged`，不走 T5 的回退。

**验证**

- 故障注入：
  - 续传之前服务器替换了文件：任务丢弃旧数据、从头下载，最终 hash 与新文件一致；
  - 只有弱 ETag 时，退而使用 `Last-Modified`。

## T21 — 状态文件与续传（C20，P3）

- [ ] `<file>.boltload` 使用 JSON Lines 格式（design §11.3）。区间在 `fsync` 之后才追加写入；任务完成后删除这个文件。
- [ ] 续传前的一致性检查，按 design §11.4 的四步执行。丢弃了哪些区间、为什么丢弃，都要上报。
- [ ] 新增续传入口。具体 API 形状先在 design 里补一节再实现，必须和 `task-api-spec.md` 的 builder 规则一致。

**验证**

- 集成测试：
  - 在随机字节位置取消任务，再续传，最终 hash 正确（G9，至少 20 个种子）；
  - 人为构造状态文件的三种问题——重叠、越界、长度超过目标文件——断言对应的区间被丢弃并上报，最终 hash 正确；
  - `total` 不一致时，从头下载。

---

## T22 — 收尾

- [ ] 每个 PR 合入之后，在 `crates/bolt-load/docs/idm-improvement-plan.md` §5 的表里，把对应的行标记为已完成，并链接到这份 spec。
- [ ] 全部完成之后，更新 `crates/bolt-load/README.md` 和根目录 `README.md`，补充新配置项 `max_connections`、`max_attempts_per_range`、`Limits`，以及续传的说明。
