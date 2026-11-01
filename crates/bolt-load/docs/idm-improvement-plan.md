# bolt-load 下载策略改进方案（基于 IDM 剖析）

> 输入：[`idm-strategy-full.md`](./idm-strategy-full.md)（权威）、[`idm-insights.md`](./idm-insights.md)、[`idm-multitask.md`](./idm-multitask.md)。
>
> 方法：三篇文档里对 bolt-load 的每一条断言都按**当前 `dev` 分支代码**（`5b76dae`）逐条复核，并交给 codex 独立做一轮对抗式核验，两边结论交叉比对后再定方案。文档里的行号大多已过期：`concurrent_task/mod.rs` 的逻辑已经拆到 `download_loop/` 下，runner 拆到了 `runner/run_loop/` 下。
>
> IDM 侧的未知项先用 `dsh --profile headless` 做了两轮补充静态分析，再用 IDA Pro（idalib + Hex-Rays，对 b16 副本）逐条复核，结论见 §8。注意：**本机 IDM 已经升级到 v6.43b16**（SHA-256 `75ac33a7…2145285`，6 490 480 字节，2026-10-23），不是三篇文档分析的 b15（`279ceed9…`）。文档里的地址索引在 b16 上**全部失效**，补充分析是按字符串重新定位的。
>
> 三篇原文档已经按 §8 的结论在原位加了 **「b16 复核」** 勘误。
>
> 本文只给方案，不改代码。可执行的规格见 [`docs/spec/2026-11-01-download-strategy-hardening/`](../../../docs/spec/2026-11-01-download-strategy-hardening/design.md)（`design.md` + `tasks.md`），两者冲突时以 spec 为准。

---

## 0. TL;DR

1. **最高优先级是数据正确性，不是速度**。两个 adapter 的 `range_stream` 都不校验 `206`/`Content-Range`。如果服务器忽略 Range、直接返回 `200`，runner 会按 `allowance` 把**文件开头那一段**截断后写进非零偏移，长度校验照样通过，最后得到一个**静默损坏**的文件。这一点比文档描述的更严重。
2. **错误分类目前是错的，而且两个 adapter 互相矛盾**：reqwest 把其它 4xx 当可重试，ureq2 当不可重试；reqwest 的 503 分支被 `is_client_error()` 挡住，**永远走不到**。
3. **调度层主要问题**：没有退避，也不读 `Retry-After`；失败预算只有任务级的“连续失败”计数，`RunnerId` 还会被复用；`max_concurrency` 只能取 CPU 核数，调用方改不了；切分判据用了 `abs()`，降速也会触发切分。
4. 文档里有几条结论**不成立或需要修正**（见 §2）。最关键的两条：`SplitAllTask` 越界在当前代码里并不会发生；“恢复时校验”针对的功能**还不存在**，代码里根本没有断点续传。
5. 推进顺序：**P0 响应完整性与错误分类 → P1 调度/重试/并发 → P2 跨任务限额（可选）→ P3 续传所需的位点与持久化**。IDM 的每片一个临时文件、注册表 journal、冗余对冲、MSS 常量都**不照搬**。
6. IDA 复核之后，策略上有 6 处调整，另有 1 处可选（汇总见 §4.0）。其中最重要的两处：
   - **切分尺寸改用单连接速率来估算**，不再用任务总速率；
   - **限速改成跨任务共享的令牌桶**，等待放在 emit 的背压路径上，这样不会被计成空闲超时。

   P0 的内容和整体优先级都不变。

---

## 1. 核验结果

| # | 文档断言 | 结论 | 当前证据 |
| --- | --- | --- | --- |
| C1 | 并发上限 = `available_parallelism()`，不可配 | **成立** | `download_loop.rs:77`；`TaskOptions::max_concurrency` 带 `#[cfg(test)] pub(crate)`（`options.rs:111-113`），只有测试能改 |
| C2 | `SplitAllTask` 不校验上限，一个 tick 就能翻倍越界 | **部分成立**：动作本身确实不再校验 | QuickStart 有 `2*current < max` 闸门（`dynamic_strategy.rs:77`），且只切 Running 的 chunk；即使真越界，`IdGenerator` 满了返回 `None`（`id.rs:45`），`spawn_runner` 会 **`expect` panic**（`strategy_tick.rs:98`），所以后果是“崩溃”而不是“悄悄越界”。另外 `split()` 先 `resize` 再 `spawn`：spawn 一旦失败，后半段区间就没人负责了 |
| C3 | `abs()` 让降速也触发切分；阈值是绝对的 1 MiB/s | **成立** | `dynamic_strategy.rs:75,88`；两档闸门都用严格 `<`，最后一个连接槽永远用不上 |
| C4 | `runner_id.expect` 会因为 `has_recent_unretryable` 提前返回而 panic | **触发路径不成立，隐式不变量确实存在** | `handle_immediate_degradation` 总会返回非空的 `ChangeMaxConcurrency`（`concurrency_control_strategy.rs:109-126`），`execute` 走不到 `expect`（`strategy/mod.rs:88`）。仍然建议改成枚举 |
| C5 | 尾部分片只切不合 | **成立，但这算不上缺陷** | IDM 的 `JoinWithNext` 合并的是**已完成**的链表项；bolt-load 的尾片照样有连接在下，没有白占槽位。真正的尾部问题是“慢连接拖尾”，要靠改派/对冲解决，不是 merge |
| C6 | 不校验 `206`/`Content-Range`，`200` 全量响应会写到 `range.start` | **成立，后果更严重** | `reqwest.rs:301-315`、`ureq2.rs:198-212`；runner 用 `.min(allowed)` 截断（`runner/run_loop/stream.rs`），`validate_size` 只比长度，**发现不了偏移错了** |
| C7 | 其它 4xx 都当 `Retryable` | **reqwest 成立，ureq2 相反** | `reqwest.rs:108-114` → `RetryableError::Io`；`ureq2.rs:79` 把 `<500` 映射为 `UnretryableIo`。reqwest 的 503 分支在 `is_client_error()` 里面，**不可达**，所有 5xx 都落到兜底的 `RetryableIo`。预算耗尽时 `RetryLimitExceeded.source` 会带上最后一次的错误（`failure_policy.rs:74`），原因没有完全丢失 |
| C8 | 不解析 `Retry-After`，失败后零退避 | **成立（传输阶段）** | 下一个 1.5 s tick 就可能重建连接（`strategy/mod.rs:15`）；元数据请求已经用 backon 做了指数退避加抖动（`meta.rs:43`），可以复用 |
| C9 | 空闲超时计为失败，进的是任务级连续计数，任何成功写都会清零 | **成立** | `runner/mod.rs:29`、`error.rs:56`（Timeout 可重试）、`failure_policy.rs:54-75`。预算按任务隔离，不跨任务 |
| C10 | meta 不记录 ETag/Last-Modified，不发 `If-Range`，恢复时不校验 | **前提过期** | `BoltLoadAdapterMeta` 只有 `content_size`/`filename`（`bolt-load-core/src/adapter.rs:45`）；但**没有续传入口**，`Progress` 总是从 0 开始（`concurrent_task/mod.rs:71`），`downloaded_chunks` 只用于上报 |
| C11 | 切分/新建 runner 前不检查磁盘，写错误被吞成 `TaskError::Other` | **成立** | `completion.rs:17` 只保留了一个格式化后的字符串，`io::ErrorKind` 没了 |
| C12 | 没区分“已接收”和“已落盘”两个位点 | **成立，但目前不会导致数据损坏** | `data.rs:154` 先 `update_progress` 再提交写；写失败会直接终止任务，`finish` 也会等写完。一旦要做续传，这就是**硬前置条件** |
| C13 | 多任务：没有准入控制、全局限速、按 host 的连接上限 | **成立** | 每个任务独立 spawn（`task/builder.rs:150`）。`idm-multitask.md` §7 说“一个坏任务会拖垮全局预算”是**错的**：预算本来就是按任务算的 |

**codex 与本方的分歧**：只有 C1。我起初看漏了 `#[cfg(test)]`，codex 的判断正确，表中已经采纳。其余各条两边结论一致。

---

## 2. 对 IDM 文档的修正

1. `idm-insights.md` §1、§4.2：“`max_concurrency` 无配置项”这句话本身没错，但 `TaskOptions` 里已经有一个仅测试可见的同名字段，P1 的改动直接把它公开即可。
2. `idm-insights.md` §2.3、§4.3：“一个 tick 连接数翻倍并越界”在当前闸门下不会发生；真正的风险是 `split()` 的**先 resize 后 spawn 不是原子操作**，再加上 `spawn_runner` 里的 `expect`。
3. `idm-insights.md` §2.4、§5.9：建议的 merge 依据不成立（IDM 合并的是已完成的项）。不应该做 `find_merge_candidate`。
4. `idm-insights.md` §5.6：“每连接增益” `speed - concurrency * per_runner_speed` 恒等于 0，因为 `per_runner_speed` 本身就是 `speed / runners`（`throughput.rs:49`）。增益只能通过**扩张前后两个窗口的对比**来衡量。
5. `idm-insights.md` §5.8：“目标分片大小 = 速度 × 时间窗”在 bolt-load 里已经有了（`strategy/mod.rs:79` 的 `speed * 2.0`）。dsh 补充分析（§8 A1）表明：IDM 的 Q 是**常量 11680 B**，并不会自适应；会随吞吐变化的是**切分点**，由该连接的平均速率推出，再按 1460×8/16/32 的台阶对齐。所以文档里“Q 按 AIMD 增长”的说法不成立，“切分点按连接速率推导”成立。
5a. `idm-strategy-full.md` §4.6、`idm-insights.md` §3.2：`Try rollback, set chunksize` 位于**续传前的分片文件重建**流程里（`Rebuilding old downloaded files before resume`），与网络响应、与 416 都无关。触发条件是某个分片临时文件的长度大于它的区间（`size > next.startPos − cur.startPos`），处理是用 `SetFilePointer` 加 `SetEndOfFile` 把多余部分截掉（§8 A4，IDA 已核实）。
5b. `idm-strategy-full.md` §4.2：IDM 有两个连接状态机。`sub_58FFB0` 的 body 阶段是 30 s（`status == 2` 时 300 s，握手阶段阈值未复核）；`sub_579D30` 是握手 30 s、body **120 s**（`status == 2` 时 300 s）。原文的 30/300 只描述了其中一个（§8 B2）。
5c. `idm-strategy-full.md` §3.7、`idm-multitask.md` §6：`sites_sf` 是**标志位**，不是连接数，这一点成立；但 dsh 说的“`factorio.com = 2` 表示强制单连接”**不成立**。IDA 显示：`&1 → +0x36E`、`&2 → +0x36D`、`&4 → +0x36F`。`+0x36E` 的作用是让该站点**豁免**“降级为单连接”的规则，`&2`（也就是 factorio.com 的值）的含义未知。按站点的连接数存放在 `MCN\<scheme>://<host>[path]\number` 里（§8 B4）。
5d. `idm-strategy-full.md` §3.5：`ApplyAlternative` 选**已提交字节更多**的那一份，不是“谁先完成用谁”（§8 A2）。
6. `idm-strategy-full.md` §7：`reconnect_backoff = 0` 不应该照搬。IDM 能这么做，是因为有调度器层 30 s 无限重试兜底；bolt-load 没有这一层，零退避只会反复冲击服务器。
7. `idm-strategy-full.md` §6 “切分对象”一行：bolt-load 已经用 `max_by_key(remaining)` 挑剩余最多的块（`chunk_planner.rs:463-471`），resize 失败也会事务性回滚。这里和 IDM 的差距比文档说的小。
8. `pos += pending` 只能说明“重连起点不回退”，**不能**说明“已经持久化到磁盘”。不要用它来论证 committed 位点的设计。
9. `idm-insights.md` §1 把 `Retry-After` 写成 IDM 的已知行为，而完整版 §8.5 明确标注“未验证”。以完整版为准。

---

## 3. 设计原则

- **连接可以牺牲，数据不能被污染**：凡是不能证明字节偏移正确的响应，一律拒收，而且拒收要发生在 adapter 返回流之前。
- **重连有代价但不等于失败**：有进展之后的空闲超时只算一次“重启”，但必须有退避，并由 `no_progress_timeout` 兜底，不能无限豁免。
- **失败要带上真实原因**：不要把状态码、响应头和 `io::ErrorKind` 拍平成字符串。
- **单文件区间写保持不变**：比 IDM 的“每片一个文件再合并”更好。
- **启发式参数改动要先有基准测试**：切分/扩张策略的替换以 A/B 数据为准，不靠类比 IDM。

---

## 4. 分阶段方案

### 4.0 策略调整一览（IDA 复核后）

| # | 调整 | IDM 依据（§8） | 改动位置 | 可信度 |
| --- | --- | --- | --- | --- |
| S1 | 切分尺寸改用**单连接速率 × 时间窗**估算，不再用任务总速率 × 2 | A1：切分候选值由**被切连接自己的速率** `[conn+0x3D0]` 推出 | P1-5 | IDM 侧：高。对 bolt-load 有收益：中，属于推理，需要基准测试验证 |
| S2 | 失败预算的默认值跟随 `max_connections`，不再跟随 CPU 核数 | —（D1 的连带改动） | P1-1 | 高（代码事实） |
| S3 | 重启不计失败，但必须有退避；退避同时适用于 runner 的重建和区间的重新领取 | B2：IDM 重启**零延时**，靠调度器层 30 s 兜底，bolt-load 没有这一层 | P1-3、P1-4 | 高 |
| S4 | 低速重启的判据改为和**本任务**的速率比较，而且只有“还有在途字节”时才判 | A5：`1.3 × [sess+0x1D0]` 和 `[sess+0x1D4]` 都是会话级的量 | P1-7 | 结构：高。阈值：自己定 |
| S5 | 限速改成跨任务令牌桶：等待放在 emit 路径上、单次等待有上限、受限时压低扩张 | B3：IDM 按单个下载整形，单次最长 12 s（低于 30 s 的空闲超时），外加 85% 刹车，并在低限速下压低初始连接数（HTTP 部分仅 dsh） | P2 | 高 |
| S6 | 续传前做一致性检查：越界或重叠的已写区间一律丢弃 | A4：IDM 在重建阶段截断超长的分片文件 | P3 | 高 |
| S7（可选） | 按 host 缓存“不支持 Range”，后续任务直接以单连接起步，并允许用户豁免 | B4：`sites_sf` 的 `&1` 豁免单连接降级（其它位的含义未知） | P2 | IDM 侧：中。属于 bolt-load 自己的设计 |

**维持不变的结论**：
- 不做冗余对冲（A2 仍未定位）；
- 不引入自适应 Q（A1）；
- 不照搬 30/120/300 这几个超时值（B2）；
- 416 不做“回滚尺寸”（A4 证明 IDM 的回滚和 416 无关）。

### P0：响应完整性与错误分类（必须做，先于一切）

#### P0-1 Range 响应校验（两个 adapter 都要改）

在 `bolt-load-adapter` 里抽一个共享的 `validate_range_response(status, headers, requested: Range<u64>, expected_total: Option<u64>) -> Result<(), AdapterError>`，reqwest 和 ureq2 都调用它：

| 响应 | 处理 |
| --- | --- |
| `206` 且 `Content-Range: bytes s-e/t`，满足 `s == start`、`e < end`、`t == total`（`t` 为 `*` 时跳过） | 放行 |
| `206` 但 `Content-Range` 缺失或对不上 | `Unretryable::RangeMismatch { requested, got }` |
| `206` + `multipart/byteranges` | `RangeMismatch`（我们只请求单段） |
| `200` | `Unretryable::RangeIgnored` |
| `416` | `Unretryable::RangeNotSatisfiable { total: Option<u64> }`（从 `Content-Range: bytes */t` 里取） |

要点：

- **必须在 `error_for_status()` 之前看状态码和响应头**，否则 416/429/503 的响应头就丢了。
- range 请求要显式带 `Accept-Encoding: identity`。如果响应带了 `Content-Encoding`，`Content-Range` 描述的是编码后的字节，偏移就对不上；遇到这种情况一律当 `RangeMismatch` 处理。
- `fetch_remote_meta`：只有 `bytes=0-0` 拿到 `206` 加合法 `Content-Range`，才能认定“支持 Range”。删掉 `ContentLength(0) => available = true` 这个分支（`reqwest.rs` 的 meta 探测）。HEAD 里的 `Accept-Ranges` 只能算弱信号，可以保留，因为上面的运行期校验会兜底。
- **任务级回退**：`RangeIgnored` 在 `DownloadLoop` 里升级为 `DownloadError::RangeUnsupported`。`drive.rs` 的 `initialize`（第 202 行附近）捕获后，取消所有 runner，**截断目标文件**，再以 `DownloadMode::Singleton` 从头下载一次。这对应 IDM 的“一个连接从头下载”。是否自动回退见 §6 决策 D2。

#### P0-2 统一状态码分类

在 `bolt-load-core`（或 `bolt-load-adapter/src/lib.rs`）实现 `classify_status(StatusCode, &HeaderMap) -> AdapterError`，两个 adapter 共用：

| 状态 | 分类 |
| --- | --- |
| 401 / 403 | `Unauthorized` |
| 404 / 410 | `NotFound` |
| 408 | `Retryable` |
| 416 | `RangeNotSatisfiable`（见 P0-1） |
| 429 / 503 | `Congestion { status, retry_after: Option<Duration> }`（替换现在的 `ServiceUnavailable`） |
| 其它 5xx | `Retryable` |
| 其它 4xx（含 409 / 412） | `Unretryable`，**保留状态码**。对齐 ureq2 现在的行为，并修掉 reqwest 的无限重试 |

- 不要按文档建议把 409/410/412 一律归为 `ResourceChanged`：412 只会在发了 `If-Match`/`If-Unmodified-Since` 时出现，而我们并不发这两个头。`ResourceChanged` 只在 P3 的验证器比对失败时才产生。
- 416 也不要盲目“回滚分片尺寸”：先重新探测 meta，如果 `total` 变了就是资源变了，要终止任务；没变则说明区间计算有 bug，应该报错而不是重试。
- `UnretryableError`、`RetryableError`、`BoltLoadAdapterMeta` 目前都没有标 `#[non_exhaustive]`，这次加变体/字段本身就是 breaking change。建议**在同一个 PR 里顺手加上 `#[non_exhaustive]`**，以后再扩展就不会破坏兼容。

#### P0-3 写错误结构化

- `completion.rs:17` 改为 `TaskError::Write { range, source: Arc<io::Error> }`。
- `ErrorKind::StorageFull` / `QuotaExceeded` 直接终止任务，并给出明确的诊断信息。
- **不做**“每次切分前检查剩余空间”：这个检查是建议性的，而且有竞态（剩余空间随时可能被别的进程占用）。如果要保证空间，应该在预分配阶段一次性预留（`file_writer.rs:221` 的 `set_len` 在稀疏文件系统上不会真的占盘），放到 P3 和续传一起做。

#### P0-4 对抗性测试（与 P0-1/2 同 PR）

扩展 `bolt-load-tests/src/adapter/http_server.rs` 和 `fault.rs`，加入以下几种服务器行为：

- 忽略 Range，直接返回 200；
- 206 但起点偏移错误；
- 206 但 total 不一致；
- 416；
- 503 带 `Retry-After: 2`；
- 对 range 请求返回 gzip 编码；
- 返回中途截断的 body。

断言：**最终文件 hash 正确，或者任务以对应的类型化错误失败**。绝不允许出现“`Completed` 但内容错误”。

---

### P1：调度、重试与并发

#### P1-1 公开连接上限

- 把 `TaskOptions::max_concurrency` 改名为 `max_connections: Option<NonZeroUsize>` 并公开，同时加 builder 方法。
- 默认值见决策 D1。`IdGenerator` 的容量、`StrategyControl::new` 都跟随这个值。
- 并发控制策略的 `initial_max_concurrency` 仍然只当作上限，降级/恢复逻辑不变。
- **S2**：`max_consecutive_failures` 的默认值现在是 `max(8, 2 × available_parallelism)`（`options.rs:13-16`），也和 CPU 绑定。应改为 `max(8, 2 × max_connections)`，保持原来的意图：一整波并行连接同时失败，也不会耗尽预算。

#### P1-2 容量安全的切分

- `split()` 改成**先保留 runner 槽位，再 resize，最后 spawn**；任何一步失败都要回滚。新增 `RunnerManager::try_reserve_slot()`。
- `spawn_runner` 里的 `.expect("failed to allocate runner with chunk")` 改为在槽位不足时 `trace` 后跳过，不再 panic。
- `find_chunk_to_split` 的返回类型改成：

  ```rust
  enum SplitCandidate {
      Unassigned(Range<u64>),
      Running { runner: RunnerId, range: Range<u64> },
  }
  ```

  这样 `strategy/mod.rs:88` 的 `expect` 就没有存在的必要了。
- 两处闸门都改为 `current + n <= max`，让最后一个槽位也能用上。

#### P1-3 退避与 `Retry-After`

- `ChunkPlanner` 的可用区间带上 `not_before: Instant` 和 `attempts: u8`。runner 失败、区间退回可用池时：`attempts += 1`，`not_before = now + backoff(attempts)`。
  - `backoff`：0.5 s × 2^n，上限 30 s，加满抖动（复用 `meta.rs` 已经在用的 backon）。
  - `Congestion { retry_after: Some(d) }` 用 `max(d, backoff)`，上限 120 s。
- **区间身份**：`RunnerId` 会被复用，区间又会被切分，所以计数要挂在可用区间这条记录上。切分产生的新区间从 0 开始计数；相邻可用区间合并时取 `max(attempts)`。
- `ConcurrencyControlStrategy` 下发 `CreateTask` 时过滤掉 `not_before > now` 的区间。`download_loop/deadline.rs` 增加一个“最早的 not_before”唤醒点，避免只能等 1.5 s 的 tick。
- 429/503 带 `Retry-After` 时，同一任务内**所有**新建连接都要遵守这个冷却时间（服务器拒绝的是连接数，不是某个区间）。
- 对照：IDA 确认 IDM 重连时**完全没有退避**（§8 B2），它靠调度器层 30 s 的重试兜底。bolt-load 没有这一层，所以退避必须做在这里。

#### P1-4 失败预算重构

`FailurePolicy` 改为三层：

1. **重启**：runner 在本次尝试里接收过 `> 0` 字节之后才发生的 `Timeout` 或断流，不计入连续失败，只走 P1-3 的退避。整体由 `no_progress_timeout` 兜底。
2. **每区间预算**：`attempts >= max_attempts_per_range`（默认 5）时终止任务，错误里带上**该区间最后一次的真实原因**。
3. **任务级连续失败**：保留，作为最后一道保险。`on_write_ok` 依然会清零，但因为有了第 2 层，健康连接“洗白”坏区间的问题就被堵住了。

不采纳“空闲超时一律不计失败”：如果没有进展判据，就会出现无限重连。

IDM 的对应做法（§8 B2）：每个会话有一个 ECONNRESET 计数器，累计到 3 次就让这个连接退役，把它的分片改派给别的连接，并从已提交位置继续（§8 A3）。这和第 2 层“每区间预算”的思路一致，可以作为默认值 5 的参照。

#### P1-5 扩张控制器替换（需要基准测试支撑）

用 IDM 的结构替换 `DynamicStrategy`，但参数自己定：

```text
every grow_interval (默认 2 s，与 tick 对齐):
  if allowed >= max_connections                  -> stop
  if any runner is Connecting/Stalled            -> skip   # IDM: started > active 时不扩
  if speed_after_last_grow < speed_before * 1.10 -> plateau, stop growing  # 增益 <10%
  target = min(allowed * 2, max_connections)
  split the largest remaining chunks (target - allowed) times, each through P1-2
```

- 去掉 `abs()` 和两个绝对 1 MiB/s 阈值。增益用扩张前后两个稳定窗口来比，不用“每连接速度”。
- **S1 切分尺寸**：现在是 `planned_chunk_size = current_speed * 2.0`（`strategy/mod.rs:80`），其中 `current_speed` 是**任务总速率**，并且要求 `suggested_range >= planned_chunk_size`（第 84 行）。
  - 问题（推理，未经基准测试验证）：连接数 N 越大，门槛就越接近 N 倍，高并发时反而更难切。而新连接实际只会以大约单连接的速率下载切走的那一半。
  - IDM 的切分候选值也是由被切连接**自己的**速率推出的（§8 A1）。
  - 改为 `planned = clamp(per_runner_speed × grow_window, min_chunk_size, …)`。其中 `per_runner_speed` 优先用被切 runner 自己的速率，拿不到时退回 `throughput.rs` 的平均值。
  - 这一改动和扩张控制器放在同一个 feature flag 后面，一起做 A/B。
- 降速交给 `ConcurrencyControlStrategy` 处理，扩张控制器只负责增加连接。
- 落地方式：先放在内部 feature flag 后面。用 `bolt-load-tests` 的限速服务器跑 A/B，覆盖低速单连接受限、高速、429 敏感三种场景，比较完成时间和连接数，数据更好才替换默认实现。

#### P1-6 超时分级（小改动）

bolt-load 已经有 `connect_timeout`（相当于握手阶段）和 `stream_idle_timeout`（相当于 body 阶段），两级结构已经具备，**不需要照搬 IDM 的 30 s / 120 s / 300 s**。只要按 P1-4 把“有进展后的超时”改成重启语义即可。

另一个约束来自 P2：`stream_idle_timeout` 必须**大于**限速时的单次最长等待（IDM 是 12 s 对 30 s）。现有实现已经在背压时重置空闲计时器（`runner/run_loop/stream.rs:172`，“Backpressure is not idleness”），只要限速等待也走这条路径，这一约束就自动满足（见 P2）。

#### P1-7 低速连接重启（可选，与 P1-5 一起做基准测试）

IDM 的 `y_sc`（§8 A5）不只看“有没有数据”，还会比较“按速率推算本该收到的字节数（再加 50 ms 余量）”和“实际收到的字节数”：差得太多就主动重连。bolt-load 只有“完全没数据”这一种空闲超时。可以考虑在 `download_loop/throughput.rs` 里加一个判据：单个 runner 的速率持续 N 秒低于任务内中位数的某个比例，就从已接收位置重启它，走 P1-3 的退避。阈值必须用基准测试来定，**不照抄 1.3 / 50 ms 这些常量**。

**S4**：IDA 显示 IDM 的判据有三个前提。bolt-load 的版本应该保留这三条结构，只是不照抄常量：
1. 比较的基准是**本任务**的速率（`[sess+0x1D0]`/`[sess+0x1D4]` 都是会话级的），不是全局的；
2. 只有“还有在途字节、位置还在分片范围内”时才判，快要收尾的 runner 不重启；
3. 这种重启不计入失败预算，但要走 P1-3 的退避，并且同一个区间连续因为慢而重启时要计入 `attempts`，避免反复重启。

另外，**限速开启时禁用这个判据**：限速下单连接本来就慢，不能把它当成异常。

---

### P2：跨任务限额（可选，按产品需要）

为 `bolt_load::Client`（或新增一个 `Scheduler`）挂一个共享的 `Arc<Limits>`，通过 `TaskBuilder` 注入；不注入时行为与现在一致：

| 能力 | 实现 | 对应 IDM |
| --- | --- | --- |
| `max_active_tasks` | 异步信号量，`TaskBuilder::spawn` 先获取许可，按 FIFO 准入 | `FilesAtTheSameTime = 4` |
| `max_connections_per_host` | `HashMap<Host, Semaphore>`；runner 建连前获取，停止时释放；扩张控制器把“拿不到许可”当作槽位已满 | `Max. connections number for a server` |
| 全局限速 | 进程级令牌桶，runner 在 `emit` 前取令牌；扩张控制器在 `speed >= 0.85 × limit` 时停止扩张 | `psnSpeedLimit` 加 85% 刹车 |

和 IDM 的区别（§8 B3、B4）：

- IDM 把全局限速值**原样复制**到每个下载会话里，不做分摊，所以 N 个下载的总速率上限是 N × limit。它**确实在数据层做了整形，但只按单个下载算**（IDA 已核实，§8 B3）：每轮根据“从基准时刻起的平均速率”算出超出部分，`delay = (rate − limit) × elapsed / limit`，上限 12 s，再用带超时的 `select()` 等在 socket 上。除此之外还有扩张刹车（85%）和压低初始连接数：HTTP 低于约 3 KB/s 时 1 条、否则 2 条（仅 dsh）；FTP 低于 10 KB/s 时 2 条（IDA）。
- IDM 的按站点上限是**每个下载各自**取 `min(全局, 站点)`，没有跨下载共享的计数器，所以同一站点的多个下载合起来会超过站点上限。

结论：bolt-load 按上表做**跨任务共享**的 host 信号量和聚合令牌桶，这样做比 IDM 更严格，是有意为之。令牌桶可以借鉴 IDM 的“按平均速率超出量推算延时并设上限”的做法，但要放在跨任务层面。可以额外提供按站点的连接数规则（对应 `MCN`）。日程配额（MB/时间窗）不在本方案范围内。

**S5 限速的落地细节**：

1. **算法用令牌桶，不用 IDM 的平均速率法**。IDM 的算法以“从基准时刻起的平均速率”为依据，速率抖动之后要过很久才收敛，突发量也不受控（这是推理）。令牌桶的桶深取 `limit × 0.25 s`，下限 64 KiB，突发量就能确定下来。
2. **等待放在 emit 路径上**：runner 在 `Emission` 推送前申请令牌，拿不到就等待。这和下游背压走同一条路径，空闲计时器本来就会在背压后重置（`stream.rs:172`），所以限速等待**不会**被误判成空闲超时。单次等待上限取 `min(2 s, stream_idle_timeout / 4)`；超过上限就分段等待，每段之间重置计时器，并检查取消和控制信号。
3. **和扩张联动**：
   - 已有的 85% 刹车不变；
   - 新建 runner 时，`initial_connections = clamp(ceil(limit / per_runner_speed_estimate), 1, max_connections)`。没有速率估计时，从 1 条连接起步。这对应 IDM 在低限速下压低初始连接数的做法；
   - 限速开启时，P1-7 的低速重启判据不生效。
4. **多任务分配**：不做加权公平，所有 runner 竞争同一个桶。这和 IDM 一样不做公平分配，但总量受控，比 IDM 的 N × limit 更严格。如果以后需要公平分配，可以在桶前面加一层按任务的 DRR，现在不做。

**S7（可选）按 host 的能力缓存**：

- P0-1 的 `RangeUnsupported` 回退发生后，在 `Client` 级的 `HostCaps` 里记录 `host → SingleConnection`，TTL 默认 1 h。之后发往同一 host 的任务直接以 `DownloadMode::Singleton` 起步，省掉一次失败的多连接尝试。
- 提供 `TaskBuilder::force_multi_connection(bool)` 作为豁免开关，对应 IDM `sites_sf` 的 `&1`。
- IDM 的单连接降级条件（`+0x363 && !+0x364`）具体是什么，我们**还不知道**（§8 B4）。所以这一条是 bolt-load 自己的设计，只在结构上参考了 IDM，不是照搬它的语义。

---

### P3：位点与续传（做续传功能前的硬前置条件）

1. **双位点**：`ChunkPlanner` 在 `downloaded`（已接收）之外再维护 `written`（收到 `WriteCompletion` 之后才推进）。`Progress.downloaded_chunks` 改为上报 `written`。如果要持久化，还要区分 `durable`（`fsync` 之后），并保证“先同步数据，再发布检查点”。
2. **资源身份**：`BoltLoadAdapterMeta` 增加 `etag: Option<String>`（只保存强 ETag）、`last_modified: Option<String>`。adapter 的 context 缓存要能在多次尝试之间**刷新**，现在的 `OnceCell` 只初始化一次。
3. **续传请求**：range 请求带 `If-Range`（有强 ETag 就用 ETag，否则用 Last-Modified）。`206` 继续下载；`200` 说明资源已变，丢弃已写区间、从头开始，**而不是**把它当成 P0-1 里的“服务器不支持 Range”。
4. **状态文件**：`<file>.boltload`（JSON 或逐行追加），记录 `url, total, etag, last_modified, written_ranges`。**不学 IDM 写注册表**。
5. **内容指纹（可选）**：恢复时用一个小的 range 请求取文件开头 2 KiB，和本地内容比对，作为验证器缺失时的兜底。这是 IDM 的思路，代价是多一次请求。
6. **S6 续传前的一致性检查**（参照 §8 A4：IDM 在重建阶段把超长的分片文件截掉）。读入状态文件后、发出任何请求之前，依次执行：
   - `written_ranges` 按起点排序后，必须互不重叠，并且都落在 `[0, total)` 内。违反的区间整段丢弃，不做“截一半保留”，因为单文件模型下没办法确认重叠部分哪一份是对的。
   - 目标文件的长度必须 `>= max(written.end)`，否则把超出文件长度的区间丢弃。这种情况通常是崩溃发生在 `set_len` 之前。
   - 状态文件里的 `total` 必须和重新探测到的 meta 一致，不一致就整体作废、从头下载（属于资源变化）。
   - 丢弃了哪些区间要通过 `TaskStatus` 或日志上报出来，不能静默处理。

---

## 5. PR 拆分

| PR | 内容 | 依赖 | 是否破坏 API | 规模估计 |
| --- | --- | --- | --- | --- |
| 1 | P0-2 状态码分类 + 错误类型加 `#[non_exhaustive]` + P0-4 部分测试 | — | 是（core 错误类型） | 中 |
| 2 | P0-1 Range 校验（两个 adapter）+ meta 探测修正 + 对抗测试 | 1 | 否 | 中 |
| 3 | P0-1 任务级 `RangeUnsupported` 回退到 Singleton | 2 | 是（`DownloadError` 新变体） | 小 |
| 4 | P0-3 写错误结构化 | — | 是（`TaskError` 新变体） | 小 |
| 5 | P1-1 公开 `max_connections`（含 S2 预算默认值）+ P1-2 容量安全切分 + `SplitCandidate` | — | 否（新增 API；预算默认值在多核机器上会变小） | 中 |
| 6 | P1-3 退避 / `Retry-After` + P1-4 预算三层化（S3） | 1, 5 | 否 | 大 |
| 7 | P1-5 扩张控制器 + S1 切分尺寸 + P1-7 低速重启（S4），都放在 feature flag 后面，附基准测试 | 5, 6 | 否 | 大 |
| 8 | P2 共享 `Limits`：准入 + host 信号量 + 令牌桶（S5），可选加 S7 `HostCaps` | 5 | 否（可选注入） | 大 |
| 9 | P3 双位点 → 验证器 → 一致性检查（S6）→ 续传 | 1, 2 | 是 | 大 |

每个 PR 合入前都要通过 `cargo test --workspace` 和 `cargo clippy --workspace --all-targets`；PR 2、3、6 还需要新增的故障注入测试全部通过。

---

## 6. 需要拍板的决策

| # | 决策 | 选项 | 建议 |
| --- | --- | --- | --- |
| D1 | `max_connections` 的默认值 | A：固定 8（IDM 默认值，与 CPU 无关）；B：`min(CPU, 8)`；C：保持 CPU 核数 | **A**。连接数取决于网络和服务器，和 CPU 无关；对 32 核机器来说，32 条连接很容易触发 429 |
| D2 | 遇到 `RangeIgnored` 时 | A：自动截断文件并改用单连接从头下载；B：直接以类型化错误失败，由调用方决定 | **A**，同时通过 `TaskStatus` 把降级事件上报出去 |
| D3 | 其它 4xx 归为不可重试 | 对齐 ureq2 现有行为 | 采纳；408 例外 |
| D4 | P2 跨任务限额做不做 | 现在做 / 等有多任务 UI 之后再做 | 建议在 P1 完成后再评估 |
| D5 | 冗余对冲（hedged download） | 做 / 不做 | **暂不做**。IDM 的触发条件在 b16 上没能定位（§8 A2）。等 P1 落地后，如果基准测试显示尾延迟仍然明显，再考虑“endgame 改派” |
| D6 | S1 切分尺寸改用单连接速率 | A：和 P1-5 一起放在 feature flag 后，A/B 之后再决定；B：单独先合入 | **A**。它会改变切分节奏，必须和扩张控制器一起评估 |
| D7 | S7 按 host 的能力缓存 | 做 / 不做；TTL 取多少 | **随 P2 一起做**，TTL 1 h；不做持久化 |
| D8 | 限速的公平性 | A：所有 runner 共享一个桶，不做公平分配；B：按任务 DRR | **A**。先保证总量受控，有多任务 UI 的需求时再加 B |

---

## 7. 明确不做

- 每片一个临时文件再 `JoinFile` 合并：单文件区间写更省 I/O。
- 用注册表或类似的 KV 存逐片位点：用 P3 的状态文件。
- `find_merge_candidate` 尾片合并：依据不成立，见 §2.3。
- IDM 的常量：11680/23360/46720、30/300、2Q+3、64 KiB 对齐。在单文件 + 页缓存的写入模型下没有收益；mmap/direct I/O 需要的对齐由 writer 自己负责。
- 手写 socket 状态机。

---

## 8. IDM 未知项的补充分析（b16：dsh headless + IDA 复核）

分析方法：
1. 用 `dsh --profile headless` 对本机 `IDMan.exe`（v6.43b16）做静态分析，分两组问题，各开一个独立会话。地址全部失效，所以按“字符串 → 指针/立即数搜索 → 反编译”重新定位。
2. 用 IDA Pro（idalib + Hex-Rays，打开的是 b16 的**副本**）逐条复核 dsh 的结论。IDA 定位到的字符串地址与 dsh 完全一致，说明两边看的是同一份二进制。

可信度说明：“IDA 复核”一列写“确认”或“更正”的条目，有两个独立工具链的证据，可信度为**高**；只有 dsh 单一来源的条目，可信度为**中**。

| # | 问题 | 结论（以 IDA 为准） | IDA 复核 | 关键证据（b16 地址） | 对方案的影响 |
| --- | --- | --- | --- | --- | --- |
| A1 | 量子 Q | Q 初值是 `0x2DA0` = 11680 B（1460×8）。增长式 `5c < 0x2DA0 ? c + 0x2DA0 : 2c + 0x5B43` 作用在 `Insert` 里一个**局部切分候选值 c** 上：c 由连接速率 `[conn+0x3D0]` 乘 −1000.0 推出，再取 `min(c, 上限)`，减半后写入新分片的起点。没找到 Q 的其它写入点 | **确认**（初值与增长式所在位置）；“只有一个写入点”仍是推断 | `0x559A81`（`mov [eax+0C0h],2DA0h`）；`sub_5605D0` 中 `0x560BE7–0x560C1A` | §2.5 修正；不引入自适应 Q |
| A2 | 冗余对冲 | `ApplyAlternative` 选**已提交字节更多**的一份；触发条件（`CheckAlternative`）未定位 | 未复核 | `0x564820`、`0x55F4D0` | D5 维持暂不做 |
| A3 | 分片改派 | 连接停止时把分片交回链表重新分配，从已提交位置继续（推断） | 未复核（IDA 里改派字符串没有直接 xref） | `0x5B2210`（dsh） | 印证 P1-3 |
| A4 | `Try rollback` | 位于**续传前分片文件重建**（`sub_563570`，日志 `Rebuilding old downloaded files before resume`）。条件：某个分片临时文件长度 > `next.startPos − cur.startPos`，先记录 `Chunk order violation`，再把文件截到区间长度（`SetFilePointer` + `SetEndOfFile`）。与网络响应、与 416 都无关 | **更正** dsh 的推断（dsh 认为是 Content-Range 不匹配） | `0x56458A`、`0x5645CE`、`0x5645FB`、`0x56466A` | §2.5a；P0-2 的“416 不回滚尺寸”成立 |
| A5 | `y_sc` 重启 | 判据是吞吐不足：比较 `1.3 × [sess+0x1D0]`、`[sess+0x1D4]`，以及“预期量 × 1000 + 50”与实收字节数；还要求仍有在途字节、位置在分片范围内。重启经由 `sub_5842E0`，这里不计数 | **确认**结构和常量（1.3 / 1000.0 / 50.0 已按字节读出） | `0x591AB1–0x591B82`；`0x6DC3F8`、`0x6E2BE0`、`0x70D090` | P1-7（可选） |
| B1 | `Retry-After` | 头名只出现在 `.rdata` 的静态头表里，没找到读取它的值的代码 | **确认**（唯一 xref 是 `0x713B38`/`0x713B58` 两个数据槽） | 字符串 `0x712AFC` | 不影响 P1-3 |
| B2 | 重连退避 | 重启路径（`sub_574590 → sub_5743F0 / sub_574360`、`sub_5842E0`）**没有任何延时**，也没有 `Sleep`。两个状态机的阈值：`sub_58FFB0` body 30 s / 300 s（握手阈值未复核）；`sub_579D30` 握手 30 s、body 120 s / 300 s。ECONNRESET 计数器 3 次（dsh 报告，IDA 未复核） | **确认**无延时；**更正**阈值（dsh 只报了 120/300） | `0x591A75–0x591A90`（`and edx,0FFFFFEF2h; add edx,12Ch`）；`0x57AE22–0x57AE28`（`and ecx,0FFFFFF4Ch; add ecx,12Ch`） | §2.5b；P1-3 的“必须加退避”成立 |
| B3 | 限速数据面 | **有按单个下载的字节级整形**：`elapsed_ms` 从 `_ftime32` 算出，`rate = bytes × 1000 / elapsed_ms`；超出时 `delay = (rate − limit) × elapsed_ms / limit`，上限 `0x2EE0` = 12 s，再按当前速度和限速的关系缩短（`/20` 或逐次减半），最后用带超时的 `select()` 等待。各下载各用完整的限速值，不分摊。限速开启时还会压低初始连接数（HTTP `< 3073 B/s` → 1 条，仅 dsh；FTP `< 10241 B/s` → 2 条，IDA 确认） | **更正** dsh 的“没有整形”；“不分摊”确认 | `sub_59A790` `0x59B80E–0x59BA2D`；同样结构见于 `sub_57D9C0`、`sub_5B9990`；FTP 启动 `sub_57E070` | P2 修正：令牌桶可以参照这种延时算法，但要做成跨任务共享 |
| B4 | 按站点上限 | `sites_sf\<host>` 是标志位：`&1 → +0x36E`、`&2 → +0x36D`、`&4 → +0x36F`。`(+0x363 && !+0x364) && (!+0x36E \|\| +0x353)` 时把 `slots` 设为 1，所以 `+0x36E` 是“**不要**降级为单连接”的豁免标志。`MCN\<scheme>://%s\number` 存每个站点的连接数（scheme ∈ http/https/ftp/rtmp/rtmpt/mms）。每个下载各自读取，没有共享计数器（dsh 推断） | **确认** reader/writer 与位解码；**更正**位语义（dsh 说 `=2` 表示强制单连接） | `sub_5DA420`（唯一调用者 `0x59CBEF`）；`0x59CBFB–0x59CC24`；`0x59FE77–0x59FED5`；`sub_4FF600` | §2.5c；P2 保留共享 host 信号量的设计 |

**仍未解决的问题**（都不阻塞方案）：

1. A2 的对冲触发条件：找到 `sub_564820` 的调用者，读调用前的判断条件。
2. A3：改派由哪种事件触发；B2 的 ECONNRESET 计数器尚未经 IDA 复核。
3. B1：静态头表的分发函数；或者做一次动态测试，让服务器返回 `503 + Retry-After: 5`，测量 IDM 的重连间隔。
4. B4：`+0x363`/`+0x364`/`+0x353` 以及 `sites_sf` 的 `&2`（`+0x36D`）、`&4`（`+0x36F`）分别代表什么。

另外，原来三篇文档里的地址和 SHA-256 都对应 b15。如果要在 b16 上继续分析，应该以上表的地址为准。三篇文档里被更正的结论已经在原位标了 **「b16 复核」**。IDA 数据库保存在 scratchpad 的 `idm-b16\IDMan.exe.i64`，对应本机二进制的副本。
