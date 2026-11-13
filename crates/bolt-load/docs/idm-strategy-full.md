# IDM 6.43b15 下载策略全景剖析（切分 / 并发 / 冗余 / 错误恢复）

> 目标：把本机安装的 IDM 作为「已完成工程化的下载器参照实现」，尽可能完整地还原它在**分片切分、连接扩张、冗余对冲、错误恢复、状态持久化**上的策略，并给出与 bolt-load 的逐项差异与可移植性判断。
>
> 本文只做调研，不改动 bolt-load 代码。
>
> 结论分级：
> - **观测**：直接来自 IDMan.exe 的伪代码/字符串/注册表/磁盘状态；
> - **推断**：由观测到的常量与控制流推出、但未动态验证的语义；
> - **未知**：静态证据无法确认。

> **b16 复核勘误（2026-10-31）**：本机 IDM 已升级到 v6.43b16（SHA-256 `75ac33a7…2145285`，6 490 480 字节），下文的 b15 地址在 b16 上全部失效。我们用 `dsh --profile headless` 和 IDA Pro（idalib + Hex-Rays）在 b16 上做了复核，被更正的结论都在原位用 **「b16 复核」** 标出；证据地址见 [`idm-improvement-plan.md`](./idm-improvement-plan.md) §8。要点：
> 1. Q **不会**自适应：初值固定为 11680。增长式作用在 `Insert` 里的一个局部切分候选值上，这个值由**连接速率**推出（§3.4）。
> 2. `Try rollback, set chunksize` 发生在**续传前重建分片文件**的阶段，用来截掉比自身区间更长的临时文件，和 416 无关（§4.6）。
> 3. 有两个连接状态机：一个的 body 空闲阈值是 30 s，另一个是 120 s，`response_state == 2` 时都是 300 s（§4.2）。
> 4. 重连路径上**没有任何延时**（§4.8）。`Retry-After` 只出现在静态头名表里，没有被读取（§8）。
> 5. `sites_sf` 是标志位，`&1` 让该站点**豁免**单连接降级；按站点的连接数存放在 `MCN\<scheme>://host\number` 里（§3.7）。
> 6. `ApplyAlternative` 留下的是**已提交字节更多**的那一份，不是“谁先完成用谁”（§3.5，仅 dsh 一个来源，IDA 未复核）。
>
> §6 的 bolt-load 一列引用的是旧版代码结构（`concurrent_task/mod.rs`），当前代码的核验结果以改进方案 §1 为准。

---

## 0. 策略全貌（TL;DR）

IDM 的下载器不是「把文件均分成 N 段、N 个线程跑完」这种教科书模型，而是：

1. **单连接起步**：整个文件先是一个 chunk（`[0, size)`）。
2. **几何扩张**：一个独立的 fan-out 控制器（`HTTPSession::TryStartNewOptionalConnections`）每隔 ≥ 2 秒把「允许的连接数」**翻倍**（2→4→8），直到配置上限；
   扩张的前置条件是「所有已启动连接都在活跃传输」且「总速率还没顶到限速的 85%」。
3. **每加一个连接就切一刀**：新连接不预先分配固定大小的段，而是从当前**剩余最多**的 chunk 上切走它剩余量的一半（边界按「包大小」对齐），并且要求最大剩余量 ≥ `2×Q+3`（Q 为 chunk 量子）。
4. **冗余对冲**：对慢/可疑的 chunk，IDM 会让第二个连接（可能来自备用地址）**重复下载同一区间**（`AddAlternative`），~~谁先完成用谁的文件~~（`ApplyAlternative`）。**b16 复核**（仅 dsh）：保留已提交字节更多的那一份。
5. **失败不等于重来**：连接级超时（~~30 s / 300 s~~ **b16 复核**：30 s 或 120 s，`response_state == 2` 时为 300 s，秒级）只触发「提交已收字节 → 重连」，**不消耗失败预算**，也**不做退避**；分片可被改派给其它连接；只有「无法续传 / 数据不一致 / 磁盘不足」才上升到任务级。
6. **每片一个临时文件**，完成时 `JoinFile` 合并；分片位点写进注册表（`<id>` 与 `<id>a` 两条 16 字节记录），用于崩溃恢复与备选回滚。
7. **恢复时做内容一致性校验**：`If-Range` + 保存的 `Last-Modified`/`FileSize`，以及对文件前 2 KiB 的指纹比对，防止「服务器换文件后接着写」把文件写脏。
8. **多任务靠「队列槽位 + 全局限速 + 日程配额」三层约束**：每个队列一个 `FilesAtTheSameTime`（默认 4）控制并发文件数；限速是单一全局标量（可被单文件、单站点覆盖），并作为**连接扩张的 85% 刹车**；日程层用「时间窗 + MB 配额」做全局硬闸，超限停掉所有下载。**没有跨任务的公平分配**。

一句话：**IDM 的切分是「按需、倍增、以最大剩余块为对象、带冗余对冲」的；错误处理是「连接可牺牲、数据不可污染」的。**

> 多任务（队列并发、全局限速、日程配额）单独成篇，见 [`idm-multitask.md`](./idm-multitask.md)。

---

## 1. 证据、产物与方法

| 项 | 值 |
| --- | --- |
| 产物 | `C:\Program Files (x86)\Internet Download Manager\IDMan.exe` |
| SHA-256 | `279ceed9aaa646650716ee326c0d996be4ce5db4b252cf5b2747f4938a7ce737` |
| 格式 / 架构 | PE32（x86），6 201 712 字节 |
| 版本 | `v6.43b15 Full`（`HKCU\Software\DownloadManager\idmvers`） |
| 分析工具 | REA / Ghidra 12.1.4（headless，default auto-analysis） |
| 运行期旁证 | `HKCU\Software\DownloadManager`、`%APPDATA%\IDM\DwnlData\<user>\<name>_<id>\`、`tips.txt` |

### 1.1 关键代码地址索引（本文引用）

| 地址 | 识别出的对象 | 关键证据 |
| --- | --- | --- |
| `0x55aea0` | `ChunksList::NeedNewConnection` | 以 `2×quantum+3` 与「最大剩余块」比较 |
| `0x55f930` | 分片链初始化 / 从注册表恢复首片 | 读注册表 `"0"`、读文件前 `0x800` 字节 |
| `0x560440` | `ChunksList::Insert`（插入新分片） | `Insert next chunk in list`、`Change chunk size to %ld` |
| `0x5646cc` → `0x564670` | `ChunksList::AddAlternative` | `Add alternative connection (%ld) for connection %ld` |
| `0x55e9e0` | `ChunksList::ApplyAlternative` | `Apply alternative for connection %ld`、`MoveFileW` 覆盖 |
| `0x559b50` | `ChunksList::FindMaxFreeChunk` | 返回「剩余最多的块」及其剩余量 |
| `0x57cbe0` | `ChunksList::CheckAlternative` | 扫描连接表，挑「该给冗余的那个连接」 |
| `0x5970d0` | `HTTPSession::TryStartNewOptionalConnections` | 连接数翻倍、2 s 冷却、85% 限速闸门 |
| `0x58fdf0` | HTTP 连接状态机（非阻塞） | `Connection restarted because of timeout (1/2/3)`、`No data, restarting y_sc` |
| `0x566d70` | 秒级「距上次活动」计时 | `_ftime32_s` → 取 `time_t`（秒）字段 |
| `0x5b2ae0` | 「Change chunk size」日志点 | `ntohl` 解析后写入 `+0x3488` |
| `0x5dcb84` | 发送 `If-Range:` 的请求构造点 | 字符串 `If-Range:` 的唯一 xref |

会话中产生的 Evidence：`ev_36be70…`（fan-out 状态机）、`ev_baedce…`（InitFirst）、`ev_eacf59…`（AddAlternative）、`ev_bc5d19…`（ApplyAlternative）、`ev_0c2106…`（FindMaxFreeChunk）、`ev_090901…`（NeedNewConnection）、`ev_f09972…`（CheckAlternative）、`ev_1ad81f…`（HTTP 状态机）、`ev_f065c3…`/`ev_8ecc26…`（计时器）。

---

## 2. 对象模型（观测 + 推断）

### 2.1 连接对象（`HTTPSession` 家族的实例）

- 连接对象**步长 0x3508**（13 576 字节）：`conn = base + i * 0x3508`。
- 下载会话持有：
  - `+0x1b0`：连接槽总数（= 配置的最大连接数，本机 8）；
  - `+0x1b8`：当前「允许」的连接数（fan-out 的受控量，只增不减）；
  - `+0x1bc`：上一次扩张的时间戳（秒）；
  - `+0x1c0/+0x1c4`：新槽位初始化的区间端点；
  - `+0x274`：连接数组基址；
  - `+0x13c`：响应状态（`2` 表示「已拿到完整响应头/进入传输」的某个态）；
  - `+0x25c/+0x25d`：暂停/恢复标志。
- 每个连接内部：
  - `+0x18/+0x1c`：**已确认位置**（64 位，相对文件）；`+0x20/+0x24`：**在途未确认字节**；
  - `+0x29`：连接状态机状态号；`+0x67`：指向会话；`+0xd40`：HTTP/2 相关上下文指针；
  - `+0x78/+0x7c`：当前分片终点（64 位）；
  - `+0xa8`：指向所属分片项（`ChunksListItem`）。
- 关键不变量（观测）：`pos` 只增；重连前执行 `pos += pending; pending = 0`。

### 2.2 分片链表（`ChunksList` / `ChunksListItem`）

- 分片项是**单向链表**，`next` 指针所在字段一旦插入就不再改动（`Insert` 里对 `local_160[0x16]` 的单向链接）。
- 每个分片项记录：**自增 ID**、`start`、`end`、当前 `pos`、`next`、`alternative` 指针、标志位（是否已有备选、自己是不是备选）、**该分片自己的临时文件名**。
- 临时文件名规则（观测）：`<base>`、`<base>1`、`<base>2` …（用 `%s%ld` 拼出，ID 从 1 起）。
  磁盘旁证：`DwnlData\a6320\QQ9.7.17.29225_209\` 下有 `QQ9.7.17.29225.exe`、`…exe1` … `…exe18` 共 **19** 个分片文件（配置的连接上限是 8，说明 **chunk 数 ≠ 连接数**，分片会随会话累积）。
- 会话级字段（推断，来自 `NeedNewConnection` 与 `Insert` 的用法）：
  - `+0xc0/+0xc4`：**分片量子 Q**（64 位）——用于切分下限、对齐与「是否值得再切」；
  - `+0xcc`：强制/无长度模式（直播流、分块传输）；
  - `+0x94`：分片 ID 计数器；`+0x88`：首分片；`+0x8c`：尾分片；`+0xe0`：当前「备选」项。

### 2.3 持久化：注册表既是配置也是 journal

- 下载元数据：`HKCU\Software\DownloadManager\<id>`，含 `FileSize`、`Downloaded`、`Status`、`LastModified`、`FRFileSize`、`lastResult`、`Speed`、`dwnlrs` 等。
  旁证（本机 id=206）：`FileSize=0x05F5E100`（100 000 000）、`Downloaded=0x055F8440`、`LastModified=Wed, 26 Aug 2020 00:15:36 GMT`、`lastResult=下载已被用户取消`。
- 分片位点：**键名 = 分片号**，值 = 16 字节 `{id, startPos(64), nextID(64)}`（`RegSetValueExA(..., 0x10, ...)`，日志 `Adding record to registry with ID %ld, startPos %I64d, nextID %ld`）。
- 备选位点：键名 = `<父分片号>a`，值同样 16 字节（`Adding record %s to registry ...`）。
- 首次初始化会写入 `"0"` 记录，并用 `0xffffffff` 表示「没有后继」。
- 推断：**每个分片插入/改派都会同步写一条记录**，因此进程崩溃后可以按 ID 顺序重建链表（`ChunksList::InsertFromReg`）。

---

## 3. 切分策略（完整）

### 3.1 起点：单连接、整文件一个 chunk

`ChunksList` 初始化（`0x55f930`）里，首分片 ID=0（列表计数器置 0），后续观测到它被赋成覆盖 `[0, 文件长度)` 的区间（文件长度来自 `GetFileSize` 的 64 位拼接）。随后：

- 若存在续传状态（注册表有 `"0"` 记录），从记录恢复 `id/start/nextID`；
- 若临时文件已存在且长度 > 0，**读取其前 2 KiB** 并存入指纹对象（`ReadFile(..., 0x800, ...)` → `FUN_0055b150`），供后续校验；
- 把 `0` 记录重置/写回。

所以 IDM 不做「一开始就切 N 段」，**N 是一步步涨上去的**。

### 3.2 扩张控制器：`TryStartNewOptionalConnections`（`0x5970d0`）

这是整个切分策略的「油门」。伪代码化后：

```text
on_timer / on_event:
  if not (allowed < slots)                 : return   # 上限
  if response_state == 2                    : return   # 已在某种终态/特殊态
  if paused and not resumed                 : return
  # 限速闸门：若设置了全局限速 L，则当前速度 >= 0.85*L 时不再加连接
  if speed_limit_enabled:
      if current_speed >= speed_limit * 0x55 / 100:   # 0x55 = 85
          return
  # 特殊性判断（HTTP/2、分块传输等）后可继续
  started = 0; active = 0
  for c in connections where slot.used:
      started += 1
      if c.state == 6 and (c.pending > 0 or c.pending_hi > 0): active += 1
      else if forced and (c.extra_pending != 0): active += 1
  now = now_seconds()
  if now - last_grow <= 1: return          # 冷却：秒级，实际约 2 s
  if started == slots: return              # 全部槽已启用
  if started > active: return              # 有连接不活跃 → 不加
  target = allowed * 2
  if accelerated_mode:                     # 会话标志 +600
      target = {2->3, 4->6, 6->8}[allowed]
  target = min(target, slots)
  for i in allowed .. target:
      start_connection(i)                  # 内部分配分片（见 3.3）
  allowed = target
```

观测要点：

- **翻倍**：`local_18 = local_1c * 2`；加速档位表 `2→3、4→6、6→8`（与「低速链路少加一个、高速链路加到 8」的经验一致）。
- **2 秒冷却**：`now - last <= 1` 直接返回（秒级计时，故实际约 2 s 一次）。
- **「全部活跃」才扩张**：`started > active` 就返回。也就是说，**不会在已有连接卡住时继续加连接**——先修好/重连，再谈扩容。
- **85% 限速闸门**：`0x55/100` 出现在限速启用分支，说明 IDM 把「限速」当成 fan-out 的硬约束，而不是仅在写入端节流。
- 上限 = 连接槽总数（`+0x1b0`），本机配置 `MaxConnectionsNumber = 8`。

### 3.3 切分点选择：`NeedNewConnection` + `Insert`

扩张出来的新连接并不拿「均分后的第 k 段」，而是走：

**Step 1 — `NeedNewConnection(0)`（`0x55aea0`）**

```c
if (list->forced) return 1;
max_free = FindMaxFreeChunk(&size, packet);   // 返回剩余最多的块
return size >= 2 * Q + 3;                     // Q = 分片量子
```

即：**只有当「剩余最多的那个块」还装得下两个量子（再加 3 字节余量）时，才认为值得再开一个新连接**。

**Step 2 — `Insert(conn, 0, packet, offset)`（`0x560440`）**

- 取到 `max_free` 的剩余量 `S`；
- 新分片拿走 `S/2`（`__alldiv(S, 2)`），并按 `packet`（包大小）对齐边界：
  `start += (packet - (start - offset) % packet)`；同时把新分片自己的 `end` 对齐；
- 原分片被截短，`pos` 保持不变；
- 拒绝条件（观测）：
  - 目标块剩余 `< 0x5b43`（23 363 字节）→ 不插；
  - 传入了 `packet` 时，要求剩余 `>= 3 * packet`；
  - 插入会让「当前块」成为空块（`endPacket < startPacket`）→ 记录 `Err in Chl:Ins` 并放弃。
- 新分片立即：分配临时文件 `<base><id>`、写注册表记录、链接进链表、把连接指向它。

**Step 3 — 备选优先**

`0x578370` 这个「启动下一块」的状态里，先 `NeedNewConnection`，再 `CheckAlternative`：

```text
need = NeedNewConnection(0)
alt  = CheckAlternative(&a, &b, need)
if (no alt)          -> Insert()            # 普通切分
else                 -> AddAlternative(...) # 冗余对冲（见 3.5）
```

**推断的完整语义**：当「剩余最多的块」很大时，IDM 优先做冗余（同一区间并行两份），只有没有可用备选时才切分。这解释了为什么在实际下载里能看到「分片数远大于连接数」。

### 3.4 目标量子 Q 与几组常量

`Insert` 里出现的常量：

| 常量 | 十进制 | 与 1460（典型 MSS）的关系 |
| --- | --- | --- |
| `0x2DA0` | 11 680 | 1460 × 8 |
| `0x5B40` | 23 360 | 1460 × 16 |
| `0xB680` | 46 720 | 1460 × 32 |
| `0x5B43` | 23 363 | 23360 + 3 |
| `0xB681` | 46 721 | 46720 + 1 |

并且有增长式：

```c
if (Q * 5 < 0x2DA0) Q += 0x2DA0;
else                Q  = Q * 2 + 0x5B43;
```

**推断**：

- 这一族常量是「以网络包为单位的量化粒度」：8/16/32 个 MSS 的台阶；
- `Q` 的更新是「小值加性、大值乘性」的自适应放大（AIMD 风格），`+3` 是给对齐留的余量；
- `2Q+3` 的切分阈值、`3×packet` 的插入闸门、`23 KB` 的绝对下限，三者是同一个「粒度体系」的不同侧面。

**未知**：`Q` 的初值与更新频率（写入点未定位），以及它是否直接等于「目标分片大小」还是「对齐单位」。可以确定的是：**分片大小不是 `总大小 / 连接数`，而是一个会随吞吐自适应的量子**。

> **b16 复核（IDA 确认）**：上面的“AIMD 自适应 Q”**不成立**。
> - Q 的初值是 `mov [eax+0C0h], 2DA0h`（11680 B，b16 `0x559A81`），没有找到其它写入点。“没有其它写入点”是推断。
> - 增长式 `5c < 0x2DA0 ? c + 0x2DA0 : 2c + 0x5B43` 作用在 `Insert`（b16 `sub_5605D0`，`0x560BE7–0x560C1A`）里的一个**局部切分候选值 c** 上。c 由该连接的速率 `[conn+0x3D0]` 乘以 −1000.0 推出，再取 `min(c, 上限)`，减半后作为新分片的起点。
> - 所以会随吞吐变化的是**切分点**，依据是**被切连接自己的速率**，不是任务总速率。Q 本身是一个固定的粒度。

### 3.5 冗余对冲：`AddAlternative` / `CheckAlternative` / `ApplyAlternative`

这是 IDM 最不像「常规下载器」的地方。

**`CheckAlternative`（`0x57cbe0`）**：遍历连接表（步长 15 000 = `0x3A98` 的另一个视图），统计各状态连接数，挑出一个「值得对冲」的连接：

```text
候选：state ∈ {4, 6}，且已绑定分片，且分片 flags(+0x5c)==0 且 (+0x14)==0
选择：记录「距上次数据的最长时间」最大的那个
阈值：need==0 时，仅当该时间 < 2 才返回候选；need!=0 时，再比较该连接的吞吐
```

**`AddAlternative(item, conn)`（`0x564670`）**：

1. 校验分片临时文件已有内容（`GetFileSize > 0`）；
2. 把原分片**按 `pos` 切开**：`[start, pos)` 保留为原分片，`[pos, end)` 成为新分片；
3. 让新连接从 `pos` 开始下载同一段 `[pos, end)`；
4. 新分片被挂成原分片的 **alternative**（`item->alt`、`alt->parent`），`alt_state` 标记 1/2；
5. 注册表写 `<id>a` 记录（`{altId, startPos, nextID}`）。

**`ApplyAlternative(item)`（`0x55e9e0`）**：

1. 删除备选自己的临时文件；
2. 用 `MoveFileW(alt.tmp, item.tmp)` 把备选文件**覆盖**到主分片上；
3. 把主分片的 `end` 更新为备选的 `end`，把备选从链表摘除，删除 `<id>a` 注册表记录，状态归零。

**推断的语义**：这是一次「对冲式重复下载（hedged download）」——对慢或可疑的分片，用第二个连接（可能是备用 IP/域名）**重复下载同一区间**，谁先完成就采用谁的数据；另一份被丢弃。它用带宽换「尾延迟」与「连接质量不确定性」。

> **b16 复核（只有 dsh 一个来源，IDA 未复核）**：
> - `ApplyAlternative`（b16 `0x55F4D0`）比较两份的已提交字节数，保留**更多**的那一份，并不是“谁先完成用谁”。
> - `CheckAlternative` 的触发条件在 b16 上还没有定位。

备注：`AddAlternative` 里 `param_1[5] = 1` 与 `FUN_00568f20(0)` 说明原连接在切出备选后会被「停/让位」，避免同一区间被同一连接重复读。

### 3.6 合并：`JoinWithNext`

字符串 `Unknown error during ChunksListItem::JoinWithNext().` 与 `ChunksList::JoinFile` 表明存在两种合并：

- **链表合并**（`JoinWithNext`）：当相邻分片都完成后合并区间，减少链表碎片；
- **文件合并**（`JoinFile`）：下载完成（或需要组装）时，把 `<base>1..n` 按顺序拼进 `<base>`。

`FindMaxFreeChunk` 只考虑 `item[5] == 0` 的项，说明被标记为「已合并/已关闭」的项不再参与切分计算。

### 3.7 连接上限与「按服务器」限制

- 全局：`MaxConnectionsNumber`（默认 8，本机 8）。fan-out 的 `slots` 直接来自这里。
- 按服务器：UI 文案 `Max. connections number for a server`；RTTI 类名 `.?AVCMaxConnNumbers@@`、`.?AVCAddMaxConnDlg@@`；注册表存在 `MCN` 键（本机为空）与 `sites_sf`（本机：`factorio.com = 2`、`deepseek.com = 2`）。
- 推断：`sites_sf` 的数值是「该站点的连接数上限/档位」，`MCN` 是按 host 存上限的容器；运行时 `min(全局, 站点)` 生效。

> **b16 复核（IDA 确认读写函数和位解码）**：`sites_sf` 不是连接数，是**标志位**。
> - 读取函数是 `sub_5DA420`，唯一的调用者在 `0x59CBEF`。位解码在 `0x59CBFB–0x59CC24`：`&1 → +0x36E`，`&2 → +0x36D`，`&4 → +0x36F`。
> - `0x59FE77–0x59FED5`：满足 `(+0x363 && !+0x364) && (!+0x36E || +0x353)` 时，把连接槽数设为 1。所以 `&1` 的作用是让该站点**豁免**“降级为单连接”的规则。
> - `&2`（也就是本机 `factorio.com`、`deepseek.com` 的值）的含义未知。
> - 按站点的连接数由 `sub_4FF600` 写入 `MCN\<scheme>://<host>[path]\number`，scheme ∈ http/https/ftp/rtmp/rtmpt/mms。
> - 每个下载各自读取这个值，没有跨下载共享的计数器。这一条是 dsh 的推断。

---

## 4. 错误处理（完整）

### 4.1 连接状态机（`0x58fdf0`）

手写非阻塞状态机，围绕 `WSAFDIsSet` 轮询 fd_set，状态号（`conn->state`）至少包括：

| 状态 | 含义（推断） | 超时行为 |
| --- | --- | --- |
| 0 | 待发起 / 等待连接建立 | 20 s 相关检查 + 200 tick |
| 1 | 等待写就绪（已连上，准备发请求） | 30 s → `timeout (2)` |
| 4 | 等待首字节 / 首包 | 30 s → `timeout (4 分支)`；或 `1e`/`12c` 分支 |
| 6 | 传输中（body） | 30 s 或 **300 s** → `timeout (3)` |
| 0x10 | 收尾 / 读最后一段 | 30 s |
| 0x1C..0x20 | TLS / HTTP2 / 特殊握手阶段 | 30 s → `timeout (1)` |
| 0x26 | HTTP/2 数据流 | 30 s |
| 0x27 | 某种完成态 | — |

**所有** `timeout` 分支的形状都是一样的（这是最关键的观测）：

```c
conn->pos   += conn->pending;      // 在途字节先记账
conn->pos_hi = conn->pos_hi + conn->pending_hi + carry;
conn->pending = 0; conn->pending_hi = 0;
close_and_restart(conn);           // 然后才断开重连
```

含义：**重连的 Range 起点永不回退，也永不重复写同一段**。同时「重启连接」在 IDM 里是**维护动作**，不是失败。

### 4.2 两级空闲超时：30 s / 300 s（秒级）

- 计时器 `FUN_00566d70` → `_ftime32_s()` 取 `time_t`（秒）字段相减，粒度 1 s。
- 阈值：`0x1E` = 30，`0x12C` = 300。
- 传输态（state 6）的判定：

```c
limit = (response_state != 2) ? 30 : 300;
if (idle > limit) restart("Connection restarted because of timeout (3)");
```

即：**头部/握手阶段 30 s 无进展就重连；已经进入 body 阶段则容忍到 5 分钟**（慢速大文件很常见）。

此外还有 `No data, restarting y_sc`：当「已收到字节数 > 0 且实测速率低于期望值的 1.3 倍关系」时也会主动重启——这是一条**吞吐异常**判据，不只是「完全没数据」。

> **b16 复核（IDA）**：
> - **超时阈值**：b16 上有**两个**连接状态机，上面的 30/300 只描述了其中一个。`response_state == 2` 应理解为“某个特定响应态”，不是“已进入 body”。
>   - `sub_58FFB0`：body 空闲 30 s，`response_state == 2` 时 300 s（`0x591A75–0x591A90`，`and edx,0FFFFFEF2h; add edx,12Ch`）。握手阶段的阈值没有复核。
>   - `sub_579D30`：握手 30 s（`cmp eax,1Eh`）；body 空闲 **120 s**，`response_state == 2` 时 300 s（`0x57AE22`，`and ecx,0FFFFFF4Ch; add ecx,12Ch`）。
> - **`y_sc`**（`0x591AB1–0x591B82`）：判据比较三样东西：`1.3 × [sess+0x1D0]`、`[sess+0x1D4]`，以及“按速率推算的预期量 × 1000 + 50”与实收字节数。前提是仍有在途字节，且位置还在分片范围内。重启走 `sub_5842E0`，**不计数**。三个常量 1.3（`0x6DC3F8`）、1000.0（`0x6E2BE0`）、50.0（`0x70D090`）已按字节读出核对。

### 4.3 断链 / 短读 / 提前 EOF

- `Connection has been closed by server.` / `Connection failed, restarting...` / `Connection restarted because of timeout` 三条文案分别对应「对端关闭」「连接失败」「超时」三类事件，处理动作都是 restart。
- `Last packet of this connection is skipped. Complete.`：最后一个包不完整时**丢弃该包**并认为分片完成（避免为了几字节重连）。
- `Unable to download the last chunk, ignore.`：最后一小块失败时**直接忽略**（对 `>10` 字节的尾部做了特判），保证任务能收尾。

### 4.4 数据一致性：三道校验

1. **请求侧**：`If-Range:`（唯一 xref `0x5dcb84`）+ 持久化的 `LastModified`/`FRFileSize`；
2. **响应侧**：`Content-Range` / `multipart/byteranges` / `Accept-Ranges` 的解析；服务器不吃 Range 时走单连接回退（`IDM started to download the file using one connection because of data corruption…`）；
3. **落盘侧**：恢复时用文件前 2 KiB 指纹比对；`Strart data for this chunk is matched with start of the file!!!`、`Some chunks matched with start of file! Deleting all, except first.`——一旦发现某分片的起始数据等于文件头，说明偏移错乱，**删掉除首片外的所有分片重来**。

第 3 条是 IDM 相对多数下载器最「狠」的一处：它不信任自己的元数据，直接拿内容做交叉验证。

### 4.5 分片改派

字符串 `The file chunk %d is reassigned to connection %d.` / `The file chunk of connection %d is reassigned to connection %d.` 共 4 个调用点（`0x574619`、`0x58475b`、`0x5a5c08`、`0x5b2896`，分别对应不同协议实现）。

**推断**：某连接失败/退出时，它未完成的分片不会让任务失败，而是**转交给另一个存活连接**继续（携带已下载位点）。这使「单连接失败」几乎不影响整体进度。

> **b16 复核（只有 dsh 一个来源）**：dsh 报告每个会话有一个 ECONNRESET 计数器，累计 3 次就让该连接退役，把分片改派出去（b16 `0x5B2210` 附近）。IDA 里改派相关的字符串没有直接 xref，这一条**未复核**。

### 4.6 Range 相关的失败分类

- ~~416（`Range Not Satisfiable`）/ 尾部越界 → `Try rollback, set chunksize to %I64d`：**回滚分片尺寸**后重试，而不是重试同一请求；~~
  **b16 复核（IDA 确认，更正）**：`Try rollback` 位于 `sub_563570`，也就是续传前的分片文件重建（日志 `Rebuilding old downloaded files before resume`）。
  - 触发条件：某个分片临时文件的长度 > `next.startPos − cur.startPos`，且长度 > 0。
  - 处理：先记录 `Chunk order violation`（`0x56458A`），再记录 `Try rollback`（`0x5645CE`），然后用 `SetFilePointer` + `SetEndOfFile` 把文件截到区间长度。
  - 这和网络响应、和 416 都无关。416 在 IDM 里怎么处理没有定位。
- 服务器不支持续传（`Cannot download this file, maybe this server doesn't support download resume…`）→ 提供「从头下载」的决策；
- `Cannot rebuild downloaded file chunks.` / `Not enough disk space to rebuild downloaded chunks.` → 组装阶段失败是**任务级**错误。

### 4.7 磁盘与容量

三条文案都出现在「新增/重建分片」的路径，说明是**前置检查**而非事后错误：

- `Disk is full. Cannot add next chunks.`
- `Not enough disk space to store files chunks, canceling download.`
- `Not enough disk space to rebuild downloaded chunks.`

`CheckAlternative`/`Insert` 都会建临时文件，因此每次「加片」都会评估磁盘。

### 4.8 任务级 vs 连接级：失败的边界

从文案与调用关系可以整理出 IDM 的失败分层（推断）：

| 事件 | 动作 | 是否算失败 |
| --- | --- | --- |
| 空闲超时（30 或 120 s，特定态 300 s；b16 复核） | 提交位点 → 立即重连（无延时） | 否 |
| 对端关闭 / RST | 重连（同分片、同位点） | 否 |
| 连接失败（DNS/TCP/TLS） | 重连；队列级 30 s 重试 | 否（有限次） |
| 吞吐异常低 | restart（`y_sc`） | 否 |
| 分片卡住 | 改派给其它连接 / 加备选对冲 | 否 |
| 服务器不吃 Range | 单连接回退 / 从头下载 | 是（降级） |
| 数据不一致（指纹不符） | 删分片重建 | 是（重建，但可继续） |
| ~~416~~ 续传前分片文件超长（b16 复核） | 截断到区间长度 | 否（修复后继续） |
| 磁盘不足 | 中止 | **是（终止）** |
| 用户取消 / 限额超限 | 中止 | 是 |

配合 `tips.txt`：`IDM scheduler will retry downloading the file every 30 seconds.` —— **任务级重试在调度器层，30 s 一次，无限次**（除非设置了重拨次数）。这与「连接级零退避重连」形成两层：近端快速修复，远端慢速兜底。

> **b16 复核（IDA 确认）**：重启路径 `sub_574590`（在 `sub_574360` 退役和 `sub_5743F0` 重启之间二选一）以及 `sub_5842E0` 里**没有任何延时，也没有调用 `Sleep`**。“连接级零退避”成立。

---

## 5. 重建的策略伪码（便于对照实现）

```rust
// ---- 连接扩张（每 ~2 s 一次机会） ----
fn try_grow(session: &mut Session) {
    if session.allowed >= session.slots { return; }
    if session.response_state == Done { return; }
    if session.paused && !session.resumed { return; }
    if let Some(limit) = session.speed_limit {
        if session.speed >= limit * 85 / 100 { return; }     // 85% 闸门
    }
    let (started, active) = session.count_started_and_active();
    if now_secs() - session.last_grow <= 1 { return; }        // 2 s 冷却
    if started == session.slots { return; }
    if started > active { return; }                          // 有连接不活跃 → 不扩
    let target = if session.accelerated {
        match session.allowed { 2 => 3, 4 => 6, 6 => 8, n => n * 2 }
    } else { session.allowed * 2 };
    let target = target.min(session.slots);
    for i in session.allowed..target { session.start_connection(i); }
    session.allowed = target;
    session.last_grow = now_secs();
}

// ---- 新连接拿什么 ----
fn assign_chunk(session: &mut Session, conn: &mut Conn) {
    let need = session.need_new_connection();      // 最大剩余 >= 2Q+3
    if let Some(alt) = session.check_alternative(need) {
        session.add_alternative(alt, conn);        // 对冲：同区间第二份
    } else {
        session.insert_split(conn);                // 切走最大块剩余量的一半
    }
}

// ---- 切分 ----
fn insert_split(session: &mut Session, conn: &mut Conn) {
    let (block, free) = session.find_max_free_chunk(packet);   // 剩余最多的块
    if free < 23_363 || (packet != 0 && free < 3 * packet) { return; }
    let half = free / 2;
    let split = align_down(block.pos + half, packet);          // 包对齐
    if split <= block.pos || split >= block.end { return; }
    let item = session.new_item(split..block.end);
    block.end = split;                                          // 原块截短
    session.link_after(block, item);
    conn.bind(item);
}

// ---- 重连（所有超时分支的统一收尾） ----
fn restart_connection(conn: &mut Conn, reason: Reason) {
    conn.pos   += conn.pending;      // 先记账
    conn.pos_hi = conn.pos_hi + conn.pending_hi + carry;
    conn.pending = 0; conn.pending_hi = 0;
    log!(reason);
    conn.close_and_begin_new_range(); // 用 conn.pos 作为新 Range 起点
}
```

---

## 6. 与 bolt-load 的逐项差异

| 维度 | IDM（观测/推断） | bolt-load 现状（文件:行） | 判断 |
| --- | --- | --- | --- |
| 连接数来源 | 全局 `MaxConnectionsNumber`（默认 8）+ 按站点上限 | `available_parallelism()`（CPU 核数），不可配（`concurrent_task/mod.rs:495`） | **应改** |
| 扩张曲线 | 每 ≥2 s **翻倍**（2→4→8），加速档 3/6/8 | 1.5 s tick；`abs(speed-2*prev)>1MB/s` 才 `SplitAllTask`，且无「全部连接活跃」前提（`dynamic_strategy.rs:74-101`） | **应改** |
| 扩张前提 | `started == active`（没有卡住的连接） | 无 | **应改** |
| 限速协同 | 速率 ≥85% 限速则停止扩张 | 无限速概念 | 可选 |
| 切分对象 | 「剩余最多的块」，切走其剩余量的一半 | 只按 `runner_id` 切它自己的 `incomplete_range` 的一半（`mod.rs:726-733`） | 等价度较高，但 bolt-load 缺「全局最大块」视角 |
| 切分闸门 | 最大剩余 ≥ `2Q+3`；且 ≥23 KB / ≥3×packet；且插入后不得为空 | 仅 `remaining >= 2*min_chunk_size`（`chunk_planner.rs:400-422`），无包对齐、无「插入后为空」防护 | **应补** |
| 量子 Q | ~~自适应（AIMD 式增长）~~ b16 复核：固定 11680；切分点按**被切连接的速率**推出 | 固定 1 MiB（`chunk_planner.rs:11`）；切分尺寸 = **任务总速率** × 2（`strategy/mod.rs:80`） | 不引入 Q；切分尺寸改用单连接速率（见改进方案 P1-5） |
| 并发上限的落地校验 | 扩张前统一算 `target`，逐个启动，绝不越界 | `SplitAllTask` 逐个 split，**不校验 `active < max`**，一个 tick 可翻倍越界（`mod.rs:690-697`） | ~~必修 bug~~ 当前闸门下不会越界；真正的风险是 `split()` 先 resize 再 spawn 不是原子操作，外加 `expect`（改进方案 C2） |
| 冗余对冲 | 同区间第二份（备选文件 + registry `<id>a`） | 无 | 可选（成本高） |
| 尾部处理 | `JoinWithNext` 合并（合并的是**已完成**的项） | 只切不合，但尾片仍有连接在下，不白占槽 | ~~应补 merge~~ 不做（改进方案 §2.3） |
| 分片存储 | 每片一个临时文件 + `JoinFile` | 单文件区间写（mmap/pool） | 保持现状 |
| 连接级超时 | 30 s（握手）；body 30 或 120 s，特定态 300 s（b16 复核）；「重启不计失败、无延时」 | `connect_timeout` + `stream_idle_timeout`，超时计为一次 retryable 失败（`failure_policy.rs`） | **应改语义**（重启不计失败，但要有退避） |
| 失败预算 | 连接级几乎无预算（改派/重连/对冲），任务级交给调度器 30 s 无限重试 | 全局 `max_consecutive_failures`，一次成功写就清零 | **应改维度** |
| 分片改派 | 有（4 个调用点） | 失败分片回到 available，靠调度器重新领取；但会与「全局预算」耦合 | 部分等价 |
| Range 校验 | `Content-Range`、`multipart/byteranges`、`If-Range`、首 2 KiB 指纹 | 只 `error_for_status()`；200 全量响应会被当区间数据写入（`reqwest.rs:301-315`） | **必修（正确性）** |
| 4xx 语义 | 416→回滚尺寸；不支持续传→降级单连接；其余分级 | 除 404/401/403/429/503 外全归 `RetryableError::Io`（`reqwest.rs:108-114`） | **应改** |
| `Retry-After` | 头名只在静态头表里，**不读取**（b16 IDA 确认） | 未解析 | 应补（bolt-load 没有调度器兜底） |
| 限速整形 | 按单个下载做延时整形，单次最长 12 s，`select()` 等待；不分摊（b16 IDA 确认） | 无 | 可选（做成跨任务共享） |
| 磁盘前置检查 | 加片前检查 | 无 | 应补 |
| 崩溃恢复 | 注册表逐片记录 + 内容指纹 | `Progress.downloaded_chunks`，无内容校验 | 可选（建议用文件而非注册表） |

---

## 7. 若要移植到 bolt-load：参数与不变量（不写代码）

**参数**（都可进 `TaskOptions`）：

| 参数 | IDM 对应 | 建议默认 |
| --- | --- | --- |
| `max_connections` | `MaxConnectionsNumber` | 8 |
| `max_connections_per_host` | `Max. connections number for a server` | 8 |
| `grow_interval` | 2 s 冷却 | 2 s |
| `grow_factor` | ×2（加速档 3/6/8） | 2 |
| `grow_requires_all_active` | `started == active` | true |
| `speed_limit_headroom` | 85% | 0.85（若有限速） |
| `chunk_quantum` | ~~自适应 Q~~ 固定 Q（b16 复核） | 保持 1 MiB 下限；切分尺寸按单连接速率推算 |
| `min_split_remain` | `2Q+3` | `2*quantum` |
| `split_align` | packet（MSS 倍数） | 64 KiB（或 4 KiB 文件块） |
| `idle_timeout_handshake` | 30 s | 30 s |
| `idle_timeout_body` | 30 或 120 s，特定态 300 s（b16 复核） | 保持现有 `stream_idle_timeout` |
| `reconnect_backoff` | 无（调度器兜底 30 s） | ~~0~~ 指数退避加抖动（bolt-load 没有调度器兜底，见改进方案 §2.6、P1-3） |

**不变量**（实现时必须成立）：

1. `Σ chunk 区间` 覆盖 `[0, size)` 且互不重叠；
2. 任何连接的位置只增不减；重连前必须把在途字节计入位置；
3. `active_connections <= max_connections`，且**切分动作自身**必须做这个校验（不能只在策略入口校验）；
4. 任何分片的起始位置变更都必须与「已下载位点」一致（IDM 用指纹兜底）；
5. 「服务器忽略 Range」必须被识别为**不可重试的降级信号**，而不是写入成功。

---

## 8. 未知与边界

> **b16 复核后的状态**：第 1 条已解决（初值 11680，没有自适应）；第 5 条已解决（不读取）；第 6 条大部分解决（`&2`/`&4` 的含义仍未知）；第 7 条有补充（存在两个状态机）。其余各条仍未解决。

1. `Q` 的初值、更新触发点与单位；`0x2DA0/0x5B40/0xB680` 虽与 `1460×{8,16,32}` 完全吻合，但业务口径仍是推断。
2. `NeedNewConnection` 中 `FindMaxFreeChunk(&size, packet)` 的第二参数（packet）的来源与取值。
3. 加速档标志 `+600` 的开启条件（推测与「服务器类型/网络类型」或用户档位有关）。
4. `CheckAlternative` 里 `fVar6 < local_20` 的精确含义（吞吐 vs 空闲秒数的混用，反编译有歧义）。
5. `Retry-After` 仅确认存在于已知头名表，是否真的用于调度未验证。
6. 按站点连接上限（`sites_sf` 的数值语义、`MCN` 的结构）未逐条验证。
7. 连接状态机的完整状态枚举（只覆盖了 `0/1/4/6/0x10/0x1C..0x20/0x26/0x27`）。
8. 反编译的 `ChunksListItem` 字段偏移在不同版本间可能变化；本文只使用**语义**，未把偏移当契约。

---

## 9. 附：观测证据到结论的映射

| 观测 | 直接结论 |
| --- | --- |
| `local_18 = local_1c * 2`，`2→3/4→6/6→8` | 扩张是倍增，且有加速档 |
| `now - last <= 1` | 扩张有 ≥2 s 冷却（秒级时钟） |
| `started > active → return` | 有连接不活跃时停止扩张 |
| `speed >= limit*0x55/100 → return` | 限速是扩张的硬约束（85%） |
| `size >= 2*Q + 3` | 切分需要「两倍量子」的剩余空间 |
| `free < 0x5B43`、`free < 3*packet` | 切分有绝对/相对下限 |
| `__alldiv(S, 2)` | 新块拿最大块剩余量的一半 |
| `%  packet` 对齐 | 切分边界按包大小对齐 |
| `pos += pending; pending = 0`（每个 timeout 分支） | 重连前提交在途字节，位置只增 |
| `0x1E` / `0x12C` + `_ftime32_s` 秒差 | 30 s / 300 s 两级空闲超时（b16 复核：另一个状态机是 120 s / 300 s） |
| `If-Range:` 唯一 xref | 请求侧做资源一致性校验 |
| 读前 `0x800` 字节 + `Strart data … matched with start of the file` | 落盘侧做内容指纹校验 |
| `MoveFileW(alt.tmp, item.tmp)` | 备选是「同区间第二份」；b16 复核（仅 dsh）：保留已提交字节更多的一份 |
| `<id>` / `<id>a` 两条 16 字节注册表值 | 分片与备选各自持久化位点 |
| `DwnlData` 下 19 个分片文件 vs 8 连接上限 | chunk 数 ≠ 连接数，分片按会话累积 |
| `sites_sf` + `MCN` + `CMaxConnNumbers` | 存在按站点的连接上限（b16 复核：上限在 `MCN\…\number`；`sites_sf` 是标志位） |
| `tips.txt`: queue retry every 30 s | 任务级重试在调度器，30 s 兜底 |
