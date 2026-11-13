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
5. **开工前的决策**：PR-1 开工前，确认 design §3 的 D3；PR-3 开工前，确认 D2；PR-3a 开工前，确认 D11（已确认）；PR-3b 开工前，确认 D9 和 D10（已确认）；PR-5 开工前，确认 D1；PR-7 开工前，确认 D5 和 D6；PR-8 开工前，确认 D4、D7、D8。确认结果先写回 `design.md`，再动代码。
6. **偏差记录**：实现和 design 不一致时，在 design 末尾追加“实现说明”一节，写明原因。不要悄悄偏离设计。

## 1. 提交与 PR 划分

| PR | 提交 | 任务 | 依赖 |
| --- | --- | --- | --- |
| PR-1 错误分类 | C1 `feat(core)!: mark adapter errors and meta non-exhaustive and add status variants` | T1 | — |
| | C2 `fix(adapter): classify HTTP statuses the same way in reqwest and ureq2` | T2 | |
| PR-2 Range 校验 | C3 `fix(adapter)!: reject range responses whose offsets cannot be proven` | T3 | PR-1 |
| | C4 `test(tests): add range fault scenarios to the test server` | T4 | |
| PR-3 单连接回退 | C5 `feat(task)!: fall back to a single connection when ranges are ignored or content-coded` | T5 | PR-2 |
| PR-3a HTTP 辅助 crate | C23 `refactor(http)!: move the HTTP helpers from core and utils into bolt-load-http` | T24 | PR-3 |
| | C24 `refactor(http): parse content codings into ContentCoding` | | |
| PR-3b 内容解码 | C21 `feat(decode): add a streaming content decoder crate` | T23 | PR-3a |
| | C22 `feat(task)!: decode content-coded full downloads` | | |
| PR-3c 空 body | C25 `fix(runner)!: finish an empty body of unknown size` | T25 | PR-3b |
| PR-4 写错误 | C6 `feat(task)!: keep the io error kind and range of write failures` | T6 | — |
| PR-4a 文件错误 | C26 `fix(task): report the file errors of the concurrent mode as Io` | T26 | PR-4 |
| PR-5 连接上限 | C7 `feat(task)!: make the connection limit configurable` | T7 | — |
| | C8 `fix(task): reserve a runner slot before splitting a chunk` | T8 | |
| PR-6 退避与预算 | C9 `feat(task): back off failed ranges and honour Retry-After` | T9 | PR-1、PR-5 |
| | C10 `feat(task): budget failures per range` | T10 | |
| PR-6a 静默限连 | C35 `refactor(task): record how connects end beside the outcome sampler` | T30 | PR-6（C9） |
| | C36 `test(tests): add a silent connection limit to the fault server` | | |
| | C37 `fix(task): lower the connection limit after refused connects` | | |
| | C38 `fix(task): validate the connection limit's recovery steps and back them off` | | |
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

- [x] `bolt-load-core/src/adapter/error.rs`：
  - `UnretryableError` 加上 `#[non_exhaustive]`；
  - 新增 `Congestion { status, retry_after }`，替换 `ServiceUnavailable`；
  - 新增 `Status { status }`、`RangeIgnored`、`RangeMismatch { requested, got }`、`RangeNotSatisfiable { total }`；
  - `Unauthorized`、`NotFound` 带上 `status`（design §5.2）。
- [x] `RetryableError` 加上 `#[non_exhaustive]`，新增 `Status { status }`；同步更新 `UnretryableError::from_retryable_error`。
- [x] 新增 `ContentRange { first, last, complete }`，放在 core 里，因为错误类型要引用它。
- [x] `BoltLoadAdapterMeta` 加上 `#[non_exhaustive]`，提供 `new(content_size)` 和 `with_filename`；所有构造点（adapter、测试、`bolt-load-tests`）都改为调用它们。
- [x] `failure_policy.rs:18` 的 `is_congestion` 和 `runner_manager.rs:902` 的测试构造，都改用 `Congestion`。
- [x] 两个 adapter 原有的映射先机械地对应到新变体上，行为暂时不变。

**验证**

- `cargo test --workspace --all-features` 通过。
- `rg "ServiceUnavailable" crates` 无结果。

## T2 — `classify_status`（C2）

- [x] 新增 `bolt-load-core/src/adapter/status.rs`：`classify_status(status, retry_after: Option<Duration>)`，按 design §5.1 的表实现，`retry_after` 夹到 120 s 以内。
- [x] 新增 `bolt-load-utils` 的 `http::parse_retry_after(value, now)`：支持秒数和 HTTP-date 两种格式。
- [x] reqwest 的 `From<ReqwestError>`（`reqwest.rs:79-115`）：只要有状态码，就调用 `classify_status`，删掉嵌套在 `is_client_error()` 里的 match。
- [x] ureq2 的映射（`ureq2.rs:79`）改为调用 `classify_status`。
- [x] 两个 adapter 在出错时都要取出 `Retry-After` 头，用 `parse_retry_after` 解析后传给 `classify_status`。

**验证**

- 表驱动测试覆盖 401、403、404、408、410、416、429、500、503、599、409、412、418。每一行都在 reqwest 和 ureq2 上各跑一次，断言两边分类相同（G2）。
- `Retry-After` 的测试用例：`"5"`、`"0"`、`"-1"`、`"999"`（夹到 120 s）、一个过去的 HTTP-date、一个未来的 HTTP-date、一个非法字符串。

## T3 — Range 校验与 meta 探测（C3）

- [x] 新增 `bolt-load-core/src/adapter/range.rs`：`validate_range_response`（design §4.1）；`ContentRange` 实现 `FromStr`。解析时容忍空白和大小写差异。
- [x] 两个 adapter 的 `range_stream`：
  - 请求带上 `Accept-Encoding: identity`；
  - 先拿状态码和响应头调用 `validate_range_response`，通过之后才取 body。
- [x] `fetch_remote_meta`：只有 `206` 加上合法的 `Content-Range`，才认定支持 Range；删掉 `ContentLength(0)` 那个分支（`reqwest.rs:245-246`）。ureq2 做同样的改动。
- [x] 在 `stream.rs` 的注释里写明：`last < end − 1` 时，按短读路径处理。
- [x] 审查后补充（design §4.1、§14.2）：
  - 重复的头先合并再校验；
  - `limit_range_body` 让超出 `Content-Range` 的 body 报 `RangeBodyTooLong`，干净地提前结束的 body 报 `RangeBodyTooShort`；
  - 所有请求都带 `Accept-Encoding: identity`；
  - adapter 关闭 ureq 的默认 `gzip` feature；
  - ureq2 的 meta、Range 探测和总长度校验共用同一次 HEAD。

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
  - 重复的 `Content-Range`、分行的 `Content-Type`、非 UTF-8 的编码值；
  - body 超出 `Content-Range`，以及 body 在 `Content-Range` 之前干净结束；
  - `200`；
  - `416` 带 `*/t`，以及 `416` 不带这个头；
  - 大小写与空白的各种变体。

## T4 — 故障注入（C4）

- [x] `bolt-load-tests/src/adapter/http_server.rs` 和 `fault.rs` 新增以下服务器行为（`RangeFault`、`create_range_fault_server`）：
  - 忽略 Range，返回 `200` 全量；
  - `206` 但起点偏移一个字节；
  - `206` 但 `complete` 不一致；
  - 一律返回 `416`；
  - 返回 `503` 并带 `Retry-After: 2`；
  - 对 range 请求返回 gzip 编码；
  - 对所有请求返回 gzip 编码后的完整表示，长度和区间都按编码后的字节算（`EncodedRepresentation`）；
  - body 中途截断。
- [x] 集成测试（`crates/bolt-load/tests/range_faults.rs`）：对上面每一种行为，分别用两个 adapter 各跑一次完整任务。断言要么最终文件 hash 正确，要么任务以对应的类型化错误失败，**绝不允许**出现 `Completed` 但 hash 错误（G1）。
- [x] 回退相关的场景，在 PR-2 里先断言为失败（返回 `RangeIgnored`）；PR-3 合入后，再改成断言回退成功。
- [x] 截断场景暴露出 ureq2 把 body 中途断开报成不可重试，已单独修复（design §14.1）。
- [x] 会解压的客户端：`bolt-load-adapter/tests/content_coding.rs` 断言解压后的 range body 以 `RangeBodyTooLong`（可压缩）或 `RangeBodyTooShort`（不可压缩）失败，从中间开始的编码区间以 `Unretryable::Io` 失败；不解压的客户端以 `RangeMismatch` 失败（design §14.2）。

**验证**

- `cargo test -p bolt-load-tests` 通过。

## T5 — 单连接回退（C5）

- [x] core 新增 `UnretryableError` 变体，让回退只匹配确定的原因（design §4.3）：
  - `RangeContentCoded { coding }`：`validate_range_response` 在只有编码这一个问题时返回它，不再返回 `RangeMismatch`；
  - `BodyUndecodable`：reqwest 的 `is_undecodable`、ureq2 的 `InvalidData` 改为返回它，不再返回 `Unretryable::Io`；
  - `ContentCoded { coding }`：`full_stream` 收到客户端没有解掉的编码时返回它。
- [x] 在 `DownloadLoop` 里，把 runner 返回的 `RangeIgnored`、`RangeContentCoded`、`RangeBodyTooLong`、`RangeBodyTooShort`、`BodyUndecodable` 升级为 `DownloadError::RangeUnsupported { reason }`：取消所有 runner，等写入全部完成。
- [x] `task/instance/drive.rs`（第 202 行附近）捕获这个错误：截断目标文件，以 `DownloadMode::Singleton` 重新运行，大小按未知处理（`total = None`）。每个任务只回退一次。
- [x] meta 阶段：HEAD 或探测响应带非 identity 编码时，直接选 Singleton，大小按未知处理。
- [x] 两个 adapter 的 `full_stream`：响应带非 identity 编码时，返回 `ContentCoded`，不写入；`Content-Type` 本身就是这种压缩格式时例外（design §4.4）。T23 落地后改为解码。
- [x] 在 `TaskStatus` 里表达这次降级：先读 `crates/bolt-load/docs/task-api-spec.md` 的状态模型，再选择新增字段或复用现有字段，并在 design 里记录选择结果。
- [x] `crates/bolt-load/docs/task-api-spec.md` 补充一条规则，说明这次降级在状态中如何体现。
- [x] 实现时补充（design §14.3）：
  - `BoltLoadAdapterMeta` 新增 `content_encoding`，两个 adapter 从 HEAD（reqwest 还有探测响应）里取；
  - core 新增 `check_content_coding` 和 `is_identity_coding`；
  - 选择结果：`TaskInfo::fallback`，回退时替换一次 `info`（STATUS-9）；
  - 故障服务器新增 `ReplacedAfterShortRange`，`range_faults.rs` 加了会解压的 reqwest 客户端。

**验证**

- T4 中“忽略 Range”和 `Gzip` 的场景改为断言：最终 hash 正确，并且观察到了降级。
- `EncodedRepresentation`：
  - 会解压的客户端：回退后最终 hash 正确；
  - 不解压的客户端：以 `ContentCoded` 失败（T23 落地后改为 hash 正确）。
- 新增测试：回退之后，文件里不残留多连接阶段写入的数据（用前后内容不同的测试文件来验证）。

## T24 — HTTP 辅助 crate（C23、C24）

编号接在 T23 之后，但顺序上紧跟 T5，排在 T23 之前。按 design §4.5（D11）实施。

- [x] C23：只搬迁，不改行为。
  - 新 crate `crates/bolt-load-http`，加入 workspace 的 `members`，依赖 core 和原 utils `http` feature 的依赖。
  - 按 design §4.5 的表搬迁，单元测试跟着文件一起走。在根模块 re-export 函数，utils 的头解析放进 `header` 模块。
  - core 删除 `status.rs`、`range.rs`、`coding.rs` 和对应的 re-export，`ContentRange` 不动。
  - utils 删除 `http` 模块和 feature，同步改 `Cargo.toml` 的 description。
  - `bolt-load-adapter` 改为依赖 `bolt-load-http`，去掉 `bolt-load-utils/http`。改 import 的地方有：两个 adapter，以及 `tests/status_classification.rs` 等测试。
  - 提交信息列出新旧路径对照，作为迁移说明。
- [x] C24：新增 `ContentCoding` 和 `parse_list`（design §4.5）。`check_content_coding` 和 range 校验里的编码检查都改用它，`is_identity_coding` 删除。现有实现已经不区分大小写，所以行为不变。
- [x] 文档：
  - 新 crate 的 README：从 core README 移过来函数说明、响应表和 “Content codings” 一节。
  - core README 只留错误变体表。
  - 更新 utils README 和 adapter README 里的路径。
  - 更新根 README 的 crate 表。
  - `bolt-load-adapter/README.md` 写自定义 adapter 的部分，改为依赖 core 和 `bolt-load-http`。
- [x] 实现时补充（design §14.4）：`ContentRange::unsatisfied_complete` 改为公开；core 用一个私有的 `is_identity` 处理 meta 的编码。

**验证**

- C23 之后，全量测试不改断言就通过：只有 import 变了。
- `cargo tree -p bolt-load-core` 里不出现 `bolt-load-http`、`http`、`headers`。
- C24 的单元测试覆盖这些输入：大小写混合、`x-gzip`、`identity` 和空项、多层编码、未知编码（`Other`，小写）、前后空白。
- `RUSTDOCFLAGS=-Dwarnings cargo doc` 不新增警告，包括搬走之后的文档内链。

## T23 — 内容解码（C21、C22）

编号接在 T22 之后，但顺序上紧跟 T24。

- [x] 新 crate `crates/bolt-load-decode`，依赖 `bolt-load-http`（取 `ContentCoding`），按 design §4.4 实现 `DecodeOptions`、`decode` 和 `DecodeError`：
  - 后端按 D9 的结论选；gzip、deflate 默认开启，br、zstd 放在 feature 后面；
  - 输入不拷贝，输出用固定大小的 `BytesMut`，每次 poll 最多产出一块；
  - 多层编码、多成员 gzip、zlib 和 raw deflate 两种格式、结束后的多余数据、`max_output`。
  - 实现说明见 design §14.5。
- [x] C22：按 D10 的结论接入。方案 A：core 给 `full_stream` 新增带 `Content-Encoding` 和 `Content-Type` 的返回类型，由 Singleton 路径解码，两个 adapter 和测试 adapter 一起改。
- [x] T5 的 `ContentCoded` 失败改为解码；`Unsupported` 的编码仍然报错。
- [x] 更新 adapter README：用户不必再为了正确性关闭客户端的解压。
- [x] 实现时补充（design §14.6）：解码后大小按未知处理，`total` 在连接后才交给 runner（STATUS-10）；引擎新增 `gzip`、`deflate`、`br`、`zstd` feature。

**验证**

- 性质测试：随机数据 × 每种编码 × 随机切块（包括逐字节输入），解码结果与原文一致。
- 损坏的输入、截断的流、结束后的多余数据、超出 `max_output`，各自报对应的错误。
- 内存：解压比很高的输入，输出块的大小不超过 `buffer_size`。
- 基准测试：同一份数据，吞吐不低于 reqwest 自带解码的 90%（criterion）。
- `range_faults.rs` 的 `EncodedRepresentation`：两个客户端都完成，hash 正确。

## T25 — 空 body（C25）

编号接在 T24 之后，顺序上紧跟 T23。修复 [#153](https://github.com/greenhat616/bolt-load/issues/153)，也解除 design §14.6 记下的已知限制。

- [x] runner 在 `total` 未知时接受任何长度，包括 0；删除 `TaskError::Empty`（design §14.7、STATUS-11）。
- [x] 取消 `test_zero_size_file_handling` 的 ignore。

**验证**

- `tests/empty_body.rs`：`Content-Length: 0`、空的 chunked、靠关连接结束的空 body，gzip 编码的空内容（带长度、chunked），以及 `204 No Content`，三个客户端都以 0 字节文件和 `total == Some(0)` 完成。
- 把旧规则放回去，这些测试失败。

## T6 — 写错误结构化（C6）

- [x] 新增 `TaskError::Write { range, source: Arc<io::Error> }`，替换 `completion.rs:16-19` 里的 `Other`。singleton 的写是缓冲的，失败对应不到区间，仍报 `Io`（design §14.8）。
- [x] `StorageFull` 和 `QuotaExceeded` 在 `Display` 里给出明确提示；`FailurePolicy` 不对它们重试。写失败不经过 `FailurePolicy`，下载循环遇到第一个失败的写就结束。

**验证**

- 用一个注入写失败的 writer 做单元测试：断言 `ErrorKind` 和 `range` 都被保留，任务以 `Write` 失败结束。

## T26 — 并发模式的文件错误（C26）

编号接在 T25 之后，顺序上紧跟 T6。补上 design §14.8 记下的缺口。

- [x] 并发模式创建文件、预分配和 finalize 的失败改为 `DownloadError::Io`，分别是 `Create`、`Allocate`、`Flush`。路径是临时文件，保留 `io::ErrorKind`（design §14.9）。
- [x] 清点其余的 `Other`：都不是文件错误，保留；“并发任务没有大小”一处删掉（design §14.9）。

**验证**

- 在不存在的目录里建文件，报 `Io { op: Create }`，kind 是 `NotFound`。
- pool writer 把文件设为 `u64::MAX` 字节，报 `Io { op: Allocate }`。
- finalize 失败的 writer 报 `Io { op: Flush }`，保留 kind。
- 把任一处映射改回去，对应的测试失败。

## T7 — 连接上限（C7）

- [x] `TaskOptions::max_concurrency`（`options.rs:111-113`）改为公开的 `max_connections: NonZeroUsize`。新增 `DEFAULT_MAX_CONNECTIONS = 8`（D1），`TaskBuilder::max_connections`。
- [x] `download_loop.rs:77-83` 改为读取这个配置，不再调用 `available_parallelism()`。
- [x] `default_max_consecutive_failures()`（`options.rs:13-16`）改为 `max(8, 2 × max_connections)`。由于它依赖 `max_connections`，计算时机改为 build 阶段：`max_consecutive_failures` 改为 `Option<usize>`，`None` 在任务开始时按 `max_connections` 算出（design §14.10）。
- [x] 更新 README 和 crate 文档里关于并发的描述。

**验证**

- 单元测试：
  - 默认值是 8；
  - 设置为 1 时，始终只有一个 runner（在下载循环里测，见 design §14.10）；
  - 失败预算的默认值随 `max_connections` 变化。
- `rg "available_parallelism" crates/bolt-load/src/task` 只剩下与连接数无关的用法，或者没有结果。

## T8 — 容量安全的切分（C8）

- [x] `RunnerManager` 新增 `try_reserve_slot()` 和 `release(slot)`。`split()` 改为三步：先占槽位，再 resize，最后 spawn（design §7.2、§14.11）。
  - 占不到槽位，或者 resize 失败：原 runner 的区间不变，占到的槽位立即释放。
  - resize 之后失败：原 runner 不再恢复，保持缩小后的区间。后半段分配不到时，槽位立即释放；spawn 被 runtime 拒绝时，下载循环之后处理这次失败，收回新槽位。两种情况下，后半段都重新变成可分配的区间。
- [x] `spawn_runner`（`strategy_tick.rs:93-99`）删掉 `expect`，槽位不足时记一条 `trace` 后跳过。`SplitGivenTask` 找不到 runner 时也跳过。
- [x] `find_chunk_to_split` 改为返回 `SplitCandidate`；删掉 `strategy/mod.rs:88` 的 `expect`。
- [x] `dynamic_strategy.rs:77,90` 的闸门改为 `current + n <= max`。

**验证**

- 性质测试（`proptest`；如果工作区还没有这个依赖，就用固定种子的随机序列代替）：随机交替执行 split、完成、失败、`ChangeMaxConcurrency`。断言：
  - 不会 panic；
  - `active <= max`；
  - 所有区间之和覆盖 `[0, total)`，且互不重叠。
- 回归测试：`IdGenerator` 满的时候执行 `SplitAllTask`，不会 panic，也不会丢失区间。

## T9 — 区间退避与 `Retry-After`（C9）

- [x] `ChunkPlanner` 的可用区间增加 `attempts` 和 `not_before`。切分时，新区间的 `attempts` 从 0 开始；合并时取较大值（design §8.1）。用空隙旁边的 `cooldowns` 表实现，合并时 `not_before` 取较晚值（design §14.12）。
- [x] runner 失败、区间退回可用池时，计算退避：使用 backon 的 `ExponentialBuilder`，参数为 0.5 s、×2、上限 30 s、满抖动。backon 的抖动是加性的，不是满抖动，见 design §14.12。
- [x] 增加任务级的 `congestion_until`：遇到 `Congestion { retry_after }` 时更新它，取值 `max(retry_after, backoff)`，上限 120 s。
- [x] `ConcurrencyControlStrategy` 下发 `CreateTask` 时，跳过还在冷却期的区间；在 `congestion_until` 之前，不新建任何连接。`spawn_runner` 和 `split` 自己也检查。
- [x] `download_loop/deadline.rs` 增加一个唤醒点，取“最早的 `not_before`”和 `congestion_until` 中较早的那个。拥塞期间直接取 `congestion_until`（design §14.12）。

**验证**

- 确定性测试（显式传入 `now`）：
  - 区间失败之后，在 `not_before` 之前不会被重新领取，之后会；
  - `Retry-After: 2` 时，2 s 内本任务没有任何新连接；
  - 切分和合并之后，`attempts` 符合规则。
- T4 的 `503 + Retry-After` 场景：断言两次请求之间的间隔 ≥ 2 s（用服务器记录的请求时间来判断）。

## T30 — 静默的连接上限（C35–C38）

- [x] C35：`AdmissionEvidence` 和每个结束的尝试都经过的分类钩子（design §8.3）；不读它，行为不变。
- [x] C36：故障服务器的连接上限，超出的连接被关闭或不应答，并记录同时服务的连接数。
- [x] C37：拥塞规则和静默规则取代按失败率的降级；`SmallerThanTotalSize` 不再降级。
- [ ] C38：恢复步骤要验证、按退避拉开间隔；删掉 `RunnerOutcomeSampler`。

**验证**

- 单元测试（注入 `now`）：分类表的每一行；连续拒绝由同水平的连接结束、不被旧的成功稀释、`STREAK_GAP` 后失效；没有佐证的拒绝不算；没有进度、迟到的 tick、writer 满时不降；拥塞不升高上限。
- 下载循环的测试（真实的连接失败经过生命周期）：4 个连接在下载、其余的连接被拒绝或不应答时，上限降到 4，之后不再有连接超过 4；传输中的 503 降级；丢失的 runner、短暂故障、拒绝与成功交替都不降级。
- 端到端（reqwest、tokio、C36 的故障服务器）：`max_connections = 8`、服务器限 3，关闭和不应答两种方式各一次：下载完成，BLAKE3 正确，服务器同时服务的连接不超过 3，上限降到 3（G10）。恢复步骤的试探个数和间隔由注入时间的测试检查。

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
