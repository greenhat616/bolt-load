# IDM 多任务调度与节流剖析（队列 / 并发 / 限速 / 配额）

> 本文是 [`idm-strategy-full.md`](./idm-strategy-full.md) 的续篇，聚焦**多个下载同时进行时** IDM 如何限制并发、如何分配带宽、以及有没有「公平/平衡」机制。
>
> 产物与版本同前：`IDMan.exe` v6.43b15（SHA-256 `279ceed9aaa646650716ee326c0d996be4ce5db4b252cf5b2747f4938a7ce737`），Ghidra 12.1.4 静态分析 + 注册表/文件旁证。
> 结论分级：**观测**（伪代码/字符串/注册表/状态文件）、**推断**（由观测推出，未动态验证）、**未知**。
>
> **b16 复核勘误（2026-10-31）**：在 v6.43b16 上用 dsh 和 IDA 复核后，有三处结论需要更正：
> 1. **限速在数据面有整形**，但只针对单个下载（§2.4）。
> 2. **按站点的连接上限不是跨下载共享的**（§5.1）。
> 3. **失败预算按任务隔离**，“一个坏任务拖垮全局预算”的说法不成立（§7）。
>
> 证据地址见 [`idm-improvement-plan.md`](./idm-improvement-plan.md) §8 B3、B4。

---

## 0. 一句话结论

IDM 的多任务不是「调度器在任务之间公平分带宽」，而是**三层彼此独立的约束**叠加：

| 层 | 机制 | 粒度 | 默认值 | 性质 |
| --- | --- | --- | --- | --- |
| 队列层 | `FilesAtTheSameTime` | 每个队列 | **4** | 并发文件数上限（准入控制） |
| 任务层 | 连接倍增 + 85% 限速刹车 | 每个下载 | 全局 8 连接/文件 | 单任务抢占带宽 |
| 全局层 | Speed Limiter（单一标量）+ 日程配额（MB/时间窗） | 整个进程 | 关闭 | 上限/硬闸 |

**没有**加权公平、没有优先级队列、没有跨队列的全局连接预算——先启动的任务先占满，后来者按自己的倍增曲线并行抢占。

---

## 1. 队列层：`CDownloadQueue`（观测）

### 1.1 数据结构与持久化

`CDownloadQueue` 构造函数（`0x49eaa0`）读注册表 `HKCU\Software\DownloadManager\Queue`：

```c
RegQueryValueExA(hk, "Queue", …, buf, &len);      // 空格分隔的下载 ID 列表
this->items = …;                                  // 逐个 atoi 建链表/数组
this->files_at_the_same_time = 4;                 // 默认 4
RegQueryValueExA(hk, "FilesAtTheSameTime", …, &this->files_at_the_same_time, 4);
```

旁证（本机）：

- `HKCU\Software\DownloadManager\Queue` 只有一个值 `Queue`，内容是一串空格分隔的 ID；没有 `FilesAtTheSameTime` → **走默认 4**。
- `%APPDATA%\IDM\Scheduler\q_1.dt`（142 字节）是同一份 ID 列表的文本镜像：
  `73 74 206 209 211 218 284 296 330 403 440 …`（与 `Queue` 值一致）。
- `%APPDATA%\IDM\Scheduler\s_1.dt`（316 字节）是队列/日程参数的二进制块（未逐字段解析）。
- 每个下载记录里有 `queueID`（本机全部 = 1），说明「队列归属」写在下载项上，而不是队列持有下载。

**推断**：`Queue` 值是唯一的权威列表；`q_<id>.dt` 是同一数据的落盘副本（构造函数里两个来源都读，用于修复损坏）。

### 1.2 队列并发：只有 per-queue 上限

`FilesAtTheSameTime` 是**唯一**被观测到的「同时下载文件数」限制：

- 字段位置 `queue+0x78`；
- 没有任何跨队列的全局计数器（未发现「总活跃下载数」的全局变量或注册表项）；
- 因此**多个队列并行运行时，总并发 = Σ(各队列的 FilesAtTheSameTime)**。

**推断**：队列是「手动启动/停止」的（UI 字符串 `Start queue` / `Stop queue` / `Start queue processing`），主列表就是队列 1；多个队列同时处理时 IDM 不做仲裁。

**未知**：队列槽位的释放/启动时机（哪个事件触发 `StartNext`）、以及启动顺序是否严格按列表顺序。UI 提供 `Move to top/bottom`、`Remove item from download queue`，但下载记录里**没有优先级字段**（`cFlags` 是位域，未见权重语义），因此「优先级」实际由**队列内的位置**表达。

### 1.3 队列级重试

`tips.txt`：`IDM scheduler will retry downloading the file every 30 seconds.` —— 任务失败后的重试在**队列/调度器层**，30 s 一次，与连接级重连（无退避、不计数）是两套机制。

---

## 2. 全局限速：单标量 + 两级覆盖（观测）

### 2.1 存储与解析

- 注册表值：`bUseSpeedLimiter`（开/关）、`psnSpeedLimit`（数值）。
- 对话框 OnOK（`0x538450`）解析文本：`value * 0x400`（**KB/s → bytes/s**，1024 进制），并钳到下限：

```c
lVar = atoi(text) * 0x400;
if (lVar < 1) lVar = 0x2800;          // 10 KB/s 下限
DAT_00788fa8 = low32(lVar);           // 全局限速（bytes/s，64 位）
DAT_00788fb1 = (FUN_00405420() != 0); // 另一开关（UI 状态）
if (changed) PostMessage(hwnd, WM_COMMAND, 0x1462, 0);
```

- UI 文案：`Always turn on Speed Limiter on IDM startup`、`Remember Speed Limiter settings for this file`、`Speed Limiter...(Limited)`。

### 2.2 有效限速的取值顺序

`S2` 会话里的两个小函数（`0x5baa90` / `0x5baa60`）：

```c
bool limit_enabled(sess) {
    v = sess->limit_mode;                 // +0x1a8：0=跟随全局，1=强制开
    return v == 0 ? g_use_speed_limiter : (v == 1);
}

u64 effective_limit(sess) {
    if (sess->limit_override_flag)        // +0x178
        return sess->limit_override;      // +0x170（64 位）
    return g_speed_limit;                 // DAT_00788fa8/…fac
}
```

- **单文件覆盖**：每个下载可记住自己的限速（`+0x170`）和三态开关（`+0x1a8`）。
- **站点规则**：`FUN_004fee90` 对「host + path」做规则匹配（去尾部 `/`，精确串或 `*` 通配），命中返回规则值，否则回退到默认（`FUN_004ff038` / 全局值）。即 IDM 支持**按站点限速**。

### 2.3 限速对「连接扩张」的作用（重要）

`HTTPSession::TryStartNewOptionalConnections`（`0x5970d0`）与另一个扩张函数（`0x57c980`）都包含同一段：

```c
if (limit_enabled(sess)) {
    u64 cap = effective_limit(sess) * 0x55 / 100;    // 85%
    if (cap <= sess->speed_peak) return;             // 已经贴近限速 → 不再加连接
}
```

也就是说：**限速在这里不是「按字节整形」，而是「抑制 fan-out」**——一旦单任务速率达到限速的 85%，就停止为它扩连接。`+0x1d4` 是会话的**峰值速率**（进度函数里 `if (peak < inst) peak = inst`），所以这是「峰值触顶后不再扩容」。

> **b16 复核**：85% 刹车只是限速的一部分，数据面另有整形，见 §2.4。另外，限速开启时 IDM 还会压低初始连接数：HTTP 限速低于 3073 B/s 时只开 1 条连接（仅 dsh，`FUN_0059c7c0`，IDA 未复核）；FTP 启动时（`sub_57E070`）限速低于 10241 B/s 则最多 2 条。

### 2.4 数据面整形：未定位

- 进度函数（`0x5bb600`，`S2::UpdateProgress`）里，限速只用于**钳制上报速率**：
  `if (limiter_on && speed > limit) reported = limit;`
- 在 `effective_limit` 的调用者里，除了各协议的 fan-out 门限，没有发现「按配额 sleep/暂停 socket」的路径；`bUseSpeedLimiter` 全局变量也只有 `limit_enabled` 一处读取。
- **推断**：字节级整形可能在别处（socket 读写层或定时器里按「已发字节 vs 预算」跳过 recv），但静态调用图上没有连到 `effective_limit`。
- **未知**：限速到底是「硬整形」还是「通过减少连接数近似实现」。这一点对「多任务下限速是否全局共享」影响很大。

> **b16 复核（IDA 确认，更正上面的“未定位”）**：数据面**有**整形，而且是**按单个下载**算的。
> - 位置：`sub_59A790` 的 `0x59B80E–0x59BA2D`；`sub_57D9C0`、`sub_5B9990` 里有同样的结构，分别对应不同协议。`effective_limit`（b16 `0x5BA450`）共有 14 个 xref。
> - 算法：
>   1. `elapsed_ms` 由 `_ftime32` 算出，即从基准时刻到现在的毫秒数；`rate = bytes × 1000 / elapsed_ms`。
>   2. 若 `rate > limit`，`delay = (rate − limit) × elapsed_ms / limit`，上限 `0x2EE0`（12 s）。
>   3. 再根据当前速度 `[sess+0x1D0]` 与 `limit/2`、`limit × 9/10` 的关系缩短 delay（`/20` 或逐次减半）。
>   4. 最后以 delay 为超时调用 `select()`，等在 socket 上。
> - 结论：
>   - 这是“**按平均速率的超出量推算等待时间**”的整形，不是令牌桶。
>   - 每个下载都拿**完整**的 limit，不做 N 等分，所以 N 个并发下载的总速率上限是 N × limit（“不分摊”确认）。
>   - 单次等待上限 12 s，低于 30 s 的空闲超时，所以限速等待不会触发重连。

---

## 3. 任务层：两条 fan-out 曲线（观测）

同一进程里存在两个「加连接」控制器，条件略有差异：

| 函数 | 冷却 | 目标 | 额外前提 |
| --- | --- | --- | --- |
| `0x5970d0`（TryStartNewOptionalConnections） | `now - last <= 1` 秒 → 跳过（≈2 s） | `allowed*2`；加速档 `2→3, 4→6, 6→8` | `started == active`（所有已启动连接都活跃）；未达 85% 限速 |
| `0x57c980` | `4 < now - last`（≈5 s） | `allowed*2`，上限 `slots` | 额外要求 `allowed/2 < 某计数`；未达 85% 限速 |

两者共同点：

- 都要 `allowed < slots`（slots = `MaxConnectionsNumber`，默认 8）；
- 都要求「没有卡住的连接」（`started > active` 直接返回）；
- 都受 85% 限速闸门约束；
- 都**只增不减**：没有发现缩减 `allowed` 的路径（限速/拥塞时靠「不再增加」而非「主动回退」）。

**推断**：`0x5970d0` 是快速路径（服务器/网络状况好时 2 s 翻倍），`0x57c980` 是保守路径（5 s 翻倍）；IDM 在两处分别服务于不同会话类型或不同阶段。

---

## 4. 全局层：日程「下载限额」（观测 + 推断）

注册表 `HKCU\Software\DownloadManager\Scheduler`（本机：`isLimitEnabled=0`、`m_MBytes=200`、`m_hours=5`、`showLimitExceededWarning=1`）。

保存函数（`0x49cc20`）在写配置时还算了一个派生值：

```c
bytes = m_MBytes * 0x100000 / 100 * 0x62;   // 98% of quota
this->stop_threshold = bytes;               // +0x18
```

UI 文案（`0x6b9cc8`）描述了行为：

> “From %d:%02d to %d:%02d you downloaded %ld MB. All IDM downloads have been stopped at %d:%02d because you had exceeded your download limits … All stopped downloads will be resumed automatically at %02d:%02d.”

**观测结论**：

1. 限额是**全局**的（所有下载一起停），不是 per-task；
2. 判据是「时间窗内累计下载字节 ≥ 98% × 配额」；
3. 超限动作是**停止所有下载**，窗口结束后自动恢复；
4. 与限速（bytes/s）是两个独立机制：一个管速率，一个管总量。

**未知**：配额累计的统计口径（是否包含已取消任务、是否按完成量还是接收量）。

---

## 5. 跨任务竞争与「平衡」的真实情况

### 5.1 有没有公平分配？

**没有观测到任何跨任务的带宽分配逻辑。** 具体表现：

- 每个下载独立执行自己的倍增曲线，上限是 `MaxConnectionsNumber`（8）——4 个并发文件理论峰值 32 条连接；
- 全局限速是一个标量，每个会话把它当作**自己的**上限来用（`effective_limit` 无除法），因此若数据面不做聚合整形，N 个并发下载的总速率上限是 N × limit；（b16 复核：数据面整形只针对单个下载，不做聚合，所以 N × limit 成立）
- 没有优先级权重、没有按剩余大小/预计完成时间排序的证据；队列顺序即优先级；
- ~~服务器维度的连接上限（`Max. connections number for a server`，R7 的 `sites_sf`/`MCN`）是**唯一**跨任务共享的约束（同一 host 的多个下载会互相挤占）。~~
  **b16 复核**：按站点的连接数存放在 `MCN\<scheme>://host[path]\number`（写入函数是 `sub_4FF600`），每个下载**各自**取 `min(全局, 站点)`。dsh 没有找到跨下载共享的计数器，所以同一站点的多个下载合起来会超过站点上限。这一条是推断，可信度中。`sites_sf` 是标志位，不是连接数：`&1` 让该站点豁免单连接降级。**结论：IDM 没有任何跨任务共享的连接约束。**

### 5.2 有没有「让路」机制？

- 队列准入：`FilesAtTheSameTime` 之外不再启动新文件（推断：槽位满时队列等待）。
- 调度器配额：超限时**全体停止**（硬闸），不是「降速」。
- 站点例外列表（`ExceptionServers`）只影响「是否接管浏览器下载」，不参与带宽。
- **未发现**「浏览器活动时自动降速」之类的自适应策略（除了历史上 UI 里的 Speed Limiter 开关）。

### 5.3 多队列并行

- 每个队列有独立的 `FilesAtTheSameTime`，可同时处理；
- 没有全局并发上限 → 队列越多，总连接/总文件数越多；
- 队列的「日程」参数（`s_<id>.dt`）只在该队列被启动时生效。

---

## 6. 参数与默认值汇总（观测）

| 参数 | 存储 | 默认 | 单位/范围 | 作用域 |
| --- | --- | --- | --- | --- |
| `FilesAtTheSameTime` | `HKCU\…\Queue` | **4** | 个文件 | 每队列 |
| `MaxConnectionsNumber` | `HKCU\…` | **8** | 条连接 | 每文件 |
| 服务器连接上限 | `MCN\<scheme>://host\number`（b16 复核） | 未设置 | 条连接 | 每站点，每个下载各自生效 |
| `sites_sf\<host>` | `HKCU\…\sites_sf` | — | 位标志（`&1` 豁免单连接降级，`&2`/`&4` 含义未知） | 每站点 |
| 单次限速等待上限 | 硬编码 `0x2EE0` | 12 s | ms | 每下载 |
| `bUseSpeedLimiter` | `HKCU\…` | 0（本机） | bool | 全局 |
| `psnSpeedLimit` | `HKCU\…` | — | bytes/s（输入 KB/s，×1024，下限 10 KB/s） | 全局 |
| 单文件限速覆盖 | 会话 `+0x170`/`+0x1a8` | 跟随全局 | 三态 | 每文件 |
| 站点限速规则 | 内存规则表 | 无 | bytes/s | 每站点/路径 |
| `isLimitEnabled` | `…\Scheduler` | 0（本机） | bool | 全局 |
| `m_MBytes` / `m_hours` | `…\Scheduler` | 200 / 5 | MB / 小时 | 全局 |
| 停止阈值 | 派生 `+0x18` | 98% × 配额 | bytes | 全局 |
| 队列重试间隔 | 硬编码（tips.txt） | 30 s | — | 队列项 |

---

## 7. 与 bolt-load 的对照

| 维度 | IDM | bolt-load 现状 | 影响 |
| --- | --- | --- | --- |
| 并发文件数 | 每队列 `FilesAtTheSameTime`=4 | 无队列概念；`TaskManager` 可任意并发 | 多任务时无准入控制，容易同时打满 |
| 每文件连接数 | 全局 8（+ 每站点上限） | `available_parallelism()`（CPU 核数） | 见前文，需改为配置项 |
| 全局限速 | 单标量 + 单文件/站点覆盖 + 85% fan-out 刹车 | 无 | bolt-load 缺「总带宽上限」这一层 |
| 限速整形 | b16 复核：按单个下载做延时整形（单次最长 12 s，`select()` 等待），外加 85% 扩张刹车 | 无 | 若要实现，需显式设计聚合令牌桶 |
| 优先级/公平 | 无；队列顺序即优先级 | 无 | 若需要，需自建 |
| 全局配额 | 日程层 MB/时间窗，超限全停 | 无 | 可作为可选特性 |
| 失败重试层级 | 连接级不计数、无延时 + 队列级 30 s | **每个任务**一个连续失败预算 | ~~多任务时一个坏任务可能拖垮全局预算~~ 复核：预算按任务隔离，不会互相影响；真正的问题在任务内部（没有退避、没有按区间的预算） |

**给 bolt-load 的多任务建议（只列方向，不落代码）**

1. 引入 **job/queue 层**：`max_active_jobs`（默认 4）+ 启动顺序 = 提交顺序；不做公平分配，但保证「不无限并发」。
2. 引入 **全局带宽预算**（可选）：一个进程级令牌桶；任务从桶里取额度，取不到就降低自己的并发或暂停读；**不要**像 IDM 那样只在 fan-out 处刹车——除非确认聚合整形在别处。（b16 复核：IDM 有数据面整形，但只针对单个下载、不做聚合。bolt-load 应该做**跨任务**的令牌桶，单次等待要有上限，而且限速等待不能计入空闲超时。）
3. 让 **每任务连接上限** 与 CPU 解耦，并叠加 **每 host 上限**（跨任务共享），这是唯一能自然避免「同站多任务互相打死」的机制。
4. 失败预算**按任务**隔离，避免一个任务的重试吃掉全局额度；队列级再做低频兜底重试（IDM 是 30 s）。（复核：bolt-load 现在就是按任务隔离的，这一条已经满足。）
5. 若要「配额/时间窗」，按全局字节计数 + 阈值停止/恢复，语义与 IDM 对齐即可。

---

## 8. 未验证 / 边界

> **b16 复核后的状态**：
> - 第 3 条已解决：整形位置在 `sub_59A790` 等三个函数里，限速不做 N 等分。
> - 第 6 条部分解决：`MCN` 的结构和 `sites_sf` 的 `&1` 位已确认，`&2`/`&4` 的含义仍未知。
> - 其余各条未复核。

1. 队列步进器（`StartNext`）的实现与触发事件；槽位释放是否严格在「下载结束/失败」时。
2. 队列内启动顺序是否严格按 `q_<id>.dt` 的顺序（UI 支持上移/下移，推测是）。
3. 限速的数据面整形位置；以及**多任务下限速是否被 N 等分**（现有证据偏向「不切分」，但没有反证）。
4. `s_<id>.dt` 的字段布局（316 字节，含时间窗/动作/重试等，未逐字段解析）。
5. 多队列并行时是否存在隐藏的全局连接预算（未发现，但不能排除在 UI 层做准入）。
6. 站点连接上限（`sites_sf` 的数值、`MCN` 结构）与限速规则的存储格式。
7. 调度器配额的统计口径（完成量 vs 接收量、是否含取消任务）。
