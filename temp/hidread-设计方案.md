# hidread — ATK X1 Ultimate 2.0 电量桥接服务 · 设计方案

| 项 | 值 |
|---|---|
| 文档版本 | v1.3 |
| 日期 | 2026-11-18 |
| 状态 | 已定案（形态 B+），待实现 |
| 变更 | v1.3 插线实测回填（双 PID + `wired` 确认）、新增会话与每日表（§3.8、§4.10–4.11）；v1.2 放电周期；v1.1 历史与在线状态；v1.0 初版 |
| 目标产物 | 单文件 `hidread.exe` + 1 个 LiteMonitor 插件 JSON |
| 参考实现 | `my_devices.py`（仓库根目录，189,819 字节 / 5010 行） |

---

## 0. 摘要

**要解决的问题**：LiteMonitor 的插件 DSL 只能发 HTTP 请求、不能执行本地命令，而这只鼠标的电量只能通过厂商私有 HID 协议读出（Windows 侧无任何标准电量字段）。两者之间必须有一个进程做翻译。

**定案的形态**：**B+ —— 单进程、60 s 低频节拍、请求永远走缓存、采样顺带落盘**。

| 参数 | 取值 | 说明 |
|---|---|---|
| 监听 | `127.0.0.1:8731` | 仅本机回环，不暴露局域网 |
| 缓存 TTL | **60 s** | 用户指定 |
| 采样节拍 | **60 s** | 与 TTL 同源，请求几乎总命中新鲜值 |
| 历史基线 | 300 s（值变化时立即写） | 曲线既密又不冗余 |
| 历史保留 | 30 天 | 启动时清理过期文件 |
| LiteMonitor 拉取间隔 | 30 s | 与 TTL 配合，最坏显示滞后 90 s |
| 放电周期 | 拔线 → 电量 ≤ 10% | 回答"一次充满能用多久"（§3.7） |
| 会话记录 | 唤醒 / 休眠时刻，±60 s | 每日使用时间表的原料（§3.8） |
| 设备 | `373B:11D9` / 接口 `0xFF02` / 17 B | 现场实测确认（§2.1） |
| 读取超时预算 | 单轮 ≤ 800 ms × 3 次重试 = 2.4 s | 只在节拍线程里发生 |

**明确不做**：不做数值平滑/滤波、不做剩余可用时长预测（电压法非线性，预测必然失真）、不改动鼠标任何配置、不写驱动、不碰系统电池接口、不把"在线窗口"说成"开机时间"（§3.6）。

---

## 1. 约束与依据

### 1.1 为什么必须有桥接件

| 事实 | 依据 |
|---|---|
| 插件只能发 HTTP，不能执行命令 | 官方 `PLUGIN_DEV_GUIDE.md` 中 `execution.type` 仅有 `api_json` / `chain`；`native://` 为宿主内置协议（`Holiday.json` 使用 `native://newyear`），用户不可扩展 |
| 同类需求也是这么做的 | LiteMonitor PR #593（蓝牙电量插件）作者原话：「插件 execution 只能发 HTTP 请求、不能执行本地命令」，因此另起本地 HTTP 服务 |
| 系统侧拿不到这份电量 | 现场实测：接收器全部 10 个接口跑 `read_standard_battery()`，**无任何** `0x0006/0x20`、`0x000D/0x3B`、`0x0085/0x65` 电量字段；设备非 BLE GATT 设备 |

### 1.2 读数精度的现实（重要设计输入）

用户实测：**插上充电线后，鼠标官方网页端电量瞬间从 15% 跳到 40%**。本项目实测同一现象：插线后协议读数 `rep[6]` 由 `0x0F`（15）直接变成 `0x37`（55），**+40 点**。

这基本可以确定固件用的是**端电压反查表**（推断，非文档证实）：充电时端电压被抬高，映射出来的百分比虚高；拔线后回落。由此推出四条设计纪律：

1. **不做平滑、不做滤波、不做插值**——平滑会把"充电虚高"和"真实掉电"一起抹平，反而制造假象。
2. **原样透传**，同时暴露 `wired` 标志与原始帧，把解释权交给使用者与 `/raw`。
3. **阈值必须粗**（60 / 30 两档即可）。电压法在低电量段非线性、塌得极快，任何"精确到 1%"的展示都是虚假精度。
4. **曲线按 `wired` 分段**：有线区段的读数单独标注（不同颜色/虚线/断点）。否则那条"插线暴涨 40 点"会被将来翻历史的你误读成"电池自己回血了"。

> 结论：这个服务是「读数搬运工」，不是「电量估计器」。任何试图让它变聪明的改动都属于范围外。

### 1.3 形态选择：为什么最终是 B+ 而不是纯 B

上一版把"零定时器"当成了优点。要曲线的需求一进来，这个前提就不成立了——**历史需要节奏**，否则采样点完全由外部请求决定：LiteMonitor 关掉、你出差几天，那段曲线就是空的。

| 维度 | A 高频轮询 20 s | B 纯按需 | **B+ 选定** |
|---|---|---|---|
| 定时器 | 20 s | 无 | **60 s** |
| 曲线密度 | 密 | 取决于外部请求，会断档 | 每 60 s 一点，值变化立即补点 |
| 休眠判定 | 最灵敏 | 只能靠请求时刻 | ≤ 120 s 内确认 |
| 请求延迟 | 微秒 | 微秒 | 微秒（仍走缓存） |
| HID 读取量 | 4320 次/天 | 不确定 | ≈1440 次/天 |

B+ 保留了纯 B 的核心性质：**请求路径上零 I/O，永远毫秒返回**。新增的只是一个 60 s 的轻节拍，它同时干三件事——刷新缓存、判定在线状态、按需落盘。这比"定时器 + 请求时再读一次"的两套逻辑更简单，而不是更复杂。

---

## 2. 设备与协议（全部为现场实测）

### 2.1 设备标识（两种模式，两个 PID）

**插线实测（2026-11-18 12:53）**：这只鼠标在无线与有线模式下是**两个不同的 USB 设备**。

| 模式 | VID:PID | 产品串 | 枚举结果 |
|---|---|---|---|
| 无线（2.4G dongle） | `0x373B:0x11D9` | `Wireless mouse 8k dongle-L` | 10 个接口 |
| 有线（USB-C 直连） | `0x373B:0x11E7` | `ATK X1 ULTIMATE 2.0` | 10 个接口 |

插线后 `11D9` 从系统里消失，只剩 `11E7`（枚举总数保持 48，未新增设备）。两种模式的接口集**一一对应**，`0xFF02` 均为 17 B 收发，协议完全通用。

> 这条发现把"是否在充电"从"靠一个字节猜"变成了"看哪个设备在"——两个信号还互相独立，可以交叉验证。

### 2.2 该接收器的接口清单（`list` 子命令的输出基准）

| UsagePage/Usage | in | out | feat | 推断用途 |
|---|---|---|---|---|
| `0x0001/0x0002` | 8 | 0 | 0 | 鼠标主接口 |
| `0x0001/0x0006` | 9 | 2 | 0 | 键盘（多媒体/宏） |
| `0x0001/0x0080` | 2 | 0 | 0 | 系统控制 |
| `0x000C/0x0001` | 3 | 0 | 0 | 消费类按键 |
| **`0xFF02/0x0002`** | **17** | **17** | 0 | **ATK1 电量通道（本方案目标）** |
| `0xFF03/0x0000` | 8 | 0 | 0 | 未知 |
| `0xFF04/0x0002` | 0 | 0 | 8 | 未知（feature 8 B） |
| `0xFF05/0x0000` | 8 | 0 | 0 | 期望 64 B 输出 → **ATK2 不适用** |
| `0xFF06/0x0002` | 49 | 49 | 0 | 未知（疑 8k dongle 配置通道，见 §10） |

有线模式（`0x11E7`）的接口集与上表逐项对应，仅产品串与 PID 不同；`0xFF02` 仍是 17/17。因此 `hid.rs` 按 **(VID, PID 集合, UsagePage)** 匹配，不写死单个 PID。

### 2.3 请求帧（17 字节，Report ID `0x08`）

| 偏移 | 值 | 含义 |
|---|---|---|
| 0 | `0x08` | Report ID |
| 1 | `0x04` | 命令码（读电量） |
| 2–15 | `0x00` | 保留 |
| 16 | `0x49` | 校验 |

校验算法（`my_devices.py:528-531`）：

```
total    = 0x08 + sum(cmd[0..14])          # cmd[0..14] 即帧偏移 1..15
checksum = (0x55 - (total & 0xFF)) & 0xFF
```

本帧：`0x08 + 0x04 = 0x0C` → `0x55 - 0x0C = 0x49`。

完整请求字节：`08 04 00 00 00 00 00 00 00 00 00 00 00 00 00 00 49`

### 2.4 应答帧（17 字节）

实测样本（无线连续 5 次字节完全一致；有线连续 4 次亦一致）：

```
无线 11D9： 08 04 00 00 00  02  0F  00  0E  8A  00 00 00 00 00 00  A0
有线 11E7： 08 04 00 00 00  02  37  01  0F  5B  00 00 00 00 00 00  A5
```

两帧都通过 §2.4 的校验。**插线瞬间电量从 15% 跳到 55%（+40 点）**，与用户此前观察到的"跳变"一致，属端电压虚高。

| 偏移 | 样本 | 含义 | 置信度 |
|---|---|---|---|
| 0 | `0x08` | Report ID | 实测 |
| 1 | `0x04` | 命令回显 | 实测 |
| 2–4 | `0x00` | 保留 | 实测 |
| 5 | `0x02` | 未知，多次采样恒定 | 未证实 |
| **6** | **`0x0F`** | **电量百分比（15）** | 实测，与官方页面一致 |
| 7 | `0x00` / `0x01` | 有线标志：无线 `0x00`，插线 `0x01` | **实测确认** |
| 8 | `0x0E` / `0x0F` | 未知；有线时 `level=55` 而它仍是 `0x0F`，**与电量无关** | 实测否定早期"次级电量"猜测 |
| 9 | `0x8A` / `0x5B` | 未知，两种模式下都随采样漂移 | 未证实 |
| 10–15 | `0x00` | 保留 | 实测 |
| **16** | **`0xA0`** | **校验** | 实测 |

校验（`my_devices.py:546-549`）：

```
len(rep) >= 17 且 rep[0]==0x08 且 rep[1]==0x04
且 (sum(rep[0..15]) + rep[16]) & 0xFF == 0x55
```

验算：`0x08+0x04+0x02+0x0F+0x0E+0x8A = 0xB5`，`0xB5+0xA0 = 0x155`，低 8 位 `0x55` ✓

### 2.5 读取时序与实测时延

参考实现的读法（`my_devices.py:613-656`）——**必须照抄，不要"优化"**：

```
for attempt in 1..=3:
    drain()                              # 清掉残留输入，防止读到上一次的回复
    write_output(17B)                    # 同步句柄
    deadline = now + 800ms
    while now < deadline:
        rep = read_input(timeout=250ms)  # 异步句柄 + WaitForSingleObject
        if valid(rep): return parse(rep)
return Err("设备没有给出有效回复（鼠标可能在休眠）")
```

实测时延（5 次连续读）：

| # | 1 | 2 | 3 | 4 | 5 |
|---|---|---|---|---|---|
| ms | 17.2 | 28.8 | 25.6 | 26.9 | 25.5 |

即热态下约 **25 ms**；设计预算按最坏 2.4 s 计（休眠 + 3 次重试）。

### 2.6 与参考实现的差异（避免照抄踩坑）

| 差异点 | 说明 |
|---|---|
| **设备表不含本机** | `DEVICES[0]` 是 "ATK Dragonfly A9 MINI"，PID 为 `0x1278/0x128E/0x1216`；本机是 `0x11D9`（无线）与 `0x11E7`（有线）。即使无 bug 也匹配不到 |
| **`read_mouse()` 有调试残留** | `my_devices.py:927-929` 直接 `return Reading(ok=False, ..., "已临时忘记该设备")`，导致该函数永远不读设备 |
| ATK2 分支不适用 | 本机 `0xFF05` 接口 `out=0`，不满足 64 B 输出前置条件；`atk_read_at` 会直接报「不是 64 字节接口」 |
| 协议实现本身可用 | 绕过上述两处、直接调 `atk_read_at(iface, "atk1")` 即可稳定读到 15% |

> 因此本项目的协议常量来自参考实现的**算法**，而不是它的**设备表**。

---

## 3. 架构（形态 B+）

### 3.1 运行时结构

```
  浏览器 / curl ──┐
                  ├──►  [ HTTP 线程 · 只读快照，不做 I/O ]
  LiteMonitor ────┘              ▲
                                 │  Mutex<Snapshot>
                    [ 节拍线程 · 每 60 s ]
                     采集 → 状态机 → 落盘
                                 │  独占设备句柄（全进程唯一）
                                 ▼
                    [ 0xFF02 接口 · 17 B / 17 B ]
                                 │  WriteFile / ReadFile
                                 │  2.4G 无线
                                 ▼
                    [ ATK X1 Ultimate 2.0 ]
```

### 3.2 一次请求的语义（stale-while-revalidate）

```
GET /battery
├─ 从未成功读过 → 同步读一次（冷启动兜底，≤2.4s）→ 返回 200 / 503
├─ age <= 60s  → 直接返回缓存，0 次 HID 调用
└─ age >  60s  → 立刻返回旧值（stale=true）
                 同时后台刷新（若已有刷新在跑则跳过，不排队、不并发）
```

**关键性质**：请求路径上不存在阻塞 I/O。鼠标休眠、dongle 被拔，都只会让 `age_s` 变大、`present` 变 false，HTTP 依然毫秒级返回。

> B+ 之后，60 s 节拍与 60 s TTL 同源，正常运行时请求几乎总能命中新鲜缓存；SWR 退化成"节拍失败时不让请求卡住"的保险。

### 3.3 三条铁律

1. **句柄独占**：只有节拍线程持有设备句柄，单线程串行，不需要任何锁或 CAS。参考实现注释明确指出同一句柄上并发阻塞读会与写/IOCTL 互锁（`my_devices.py:327-330`），B+ 从结构上就消灭了这个前提。
2. **读写分句柄**：读句柄 `FILE_FLAG_OVERLAPPED` + `WaitForSingleObject(250ms)` + 超时 `CancelIoEx`；写句柄保持同步。与参考实现一一对应。
3. **请求路径零 HID**：唯一例外是冷启动预热——它在开始 `accept` **之前**完成，因此不构成请求路径上的阻塞。

### 3.4 失败与恢复矩阵

| 触发 | 检测 | 行为 |
|---|---|---|
| 鼠标休眠，无回复 | 3 次重试全超时 | 保留旧值，`age_s` 增长，`error="no reply"`；不重置 `present` |
| dongle 被拔 | `ERROR_DEVICE_NOT_CONNECTED` / `ERROR_FILE_NOT_FOUND` / `ERROR_INVALID_HANDLE` | 关句柄，`present=false`，`level_text="--"` |
| dongle 重插 | 下次刷新时句柄为空 → 重新枚举 | 自动恢复，无需重启服务 |
| 插/拔 USB 线（PID 切换） | 句柄报 `ERROR_DEVICE_NOT_CONNECTED`，或枚举结果由 `11D9` 变 `11E7` | 关句柄 → 重枚举 → 按 `UsagePage=0xFF02` 重开；记 `mode_change`；**不**重置 session 与 cycle |
| 校验失败（乱码回复） | `valid()` 为假 | 记为 `error="bad frame: <hex>"`，不使用该值，重试 |
| PC 睡眠后唤醒 | `now − last_tick > 3 × tick_s` | 记 `host_resume` 事件，**不**累计失败、**不**判离线（§3.5） |
| 端口被占用 | `bind` 失败 | 打印明确错误并退出码 2，不静默换端口 |
| 服务被开两份 | 命名互斥量 `Global\hidread-singleton` | 第二份拒绝启动（避免两个进程抢同一设备的输入队列） |
| 数值越界（如 `0xFF`） | 解析后 clamp 0..100 | 取 100 并写日志；`error` 仍为 null（`error` 只表示"没读到"） |

### 3.5 节拍、采样与落盘

**一个 60 s 的节拍线程**，每轮依次做四件事，全部在同一个线程内串行：

```
tick():
  1. 读一次（3 次重试 / 800ms 预算）
  2. 成功 → 刷新快照；失败 → consecutive_fails += 1
  3. 更新 link 状态机（§3.6），状态变化时写 event
  4. 满足条件才写一行 history（见下），然后 sleep 到下一个 tick
```

**历史写入条件（满足任一即写）**

| 条件 | 目的 |
|---|---|
| `level` 与上一条不同 | 抓住跳变点（充电、掉电台阶） |
| 距上一条基线 ≥ `history_interval_s`（300 s） | 保证曲线有连续底噪，不至于只剩跳变点 |
| `link` 状态变化（online ↔ offline） | 曲线上能看见断链区段 |

按此策略，一天约 **288–400 行**，单文件 15–20 KB。

**文件格式**（`data/` 下按天分文件，纯 CSV，无第三方依赖）

`history-2026-11-18.csv`

| 列 | 示例 | 说明 |
|---|---|---|
| `ts` | `2026-11-18T12:41:00+08:00` | RFC3339，给人看 |
| `ts_unix` | `1763436060` | 秒级时间戳，给画图和排序 |
| `level` | `15` | 0–100；本轮没读到时留空 |
| `wired` | `0` | 0 / 1 |
| `link` | `online` | online / offline |
| `read_ms` | `25` | 读成功才填 |

`events-2026-11-18.csv`

| 列 | 示例 | 说明 |
|---|---|---|
| `ts` / `ts_unix` | 同上 | |
| `event` | `offline` | `online` / `offline` / `device_removed` / `device_added` / `mode_change` / `host_resume` |
| `detail` | `fails=2` | 触发原因，排障用 |

**系统挂起处理（重要）**

PC 睡眠时进程被冻结，恢复后 `now - last_tick` 会远大于 60 s。此时**不得**把这段空档判成"鼠标离线"：

```
if now - last_tick > 3 * tick_s:
    write_event("host_resume", gap_s=now-last_tick)
    consecutive_fails = 0        # 不作失败累计
    本轮只做一次读取，按结果更新状态
```

曲线里这段就是一截空白，而不是假造的直线。

**其它落盘规则**

- 写文件用「打开 → 追加一行 → 关闭」，不长期持有句柄，避免你随时想导出时被锁。
- 保留期 `history_days`（默认 30），启动时清理过期文件；不删非本项目文件。
- `data/` 不可写（例如 exe 放在 `Program Files`）→ 自动回退到 `%LOCALAPPDATA%\hidread\data`，只告警一次，功能不中断。
- 内存里维护最近 24 h 的环形缓冲（≈1440 点），`/history?hours=24` 直接内存应答；更长时间跨度才读磁盘。

### 3.6 在线状态与"开机时间"的语义边界

这一节专门解决"别把推断说成事实"。

| 问题 | 能否知道 | 依据 |
|---|---|---|
| dongle 是否插在机器上 | **能** | SetupAPI 能否枚举到 `373B:11D9` |
| 鼠标是否响应查询 | **能** | 写帧后 800 ms 内是否收到合法应答 |
| 本次在线窗口从何时开始 | **能**（精度 = 节拍） | 第一次成功应答的时刻 |
| 鼠标当前是休眠、关机、还是超出 2.4G 范围 | **不能** | 三者表现完全一致：dongle 在、无应答 |
| 鼠标真实通电时刻（"开机时间"） | **不能** | 协议里没有 uptime 字段；且实测 4 s 内 0 条主动上报，接收器不会告诉我们链路何时建立 |
| 鼠标是否在充电 | **能**（两个独立信号） | 插线实测：`rep[7]` 0→1，且 PID 由 `11D9` 变 `11E7`；充电电流/是否真的在充仍然未知 |

实测定论：向 `0xFF02` 只读句柄连续监听 4 s，主动上报 **0 条**，说明这条通道是纯请求-应答，没有任何心跳或链路事件可利用。

**因此命名必须诚实**：

- 对外字段叫 `link.state`（`online` / `offline`），不叫 `sleeping` / `awake`。
- 会话边界叫 `wake_at` / `sleep_at`（观察到的首次/末次响应时刻），**不叫 `power_on_at`**。
- 界面上写「本次在线 3h07m」，不写「开机 3h07m」——前者是观测事实，后者是它无法支撑的结论。你真正想要的"一次充满用多久"由 §3.7 的放电周期回答，那条路径不需要猜开机时刻。

> 如果你要的其实是"它什么时候断过、断了多久"，这套事件日志完全够用；如果你要"这只鼠标累计通电多少小时"，那得靠厂商固件，本项目给不了。
>
> 而"一次充满电能用多久"是另一回事——它不需要知道通电时刻，只需要两个可观测的边界：**拔线** 和 **掉到阈值**。见 §3.7。

### 3.7 放电周期：回答"一次充满能用多久"

**指标定义**：一次满充续航 = 从**拔线**（`wired` 由 1 变 0，且此前已确认充满）到**电量首次 ≤ `cycle_end_level`** 的墙钟时长。

这个定义的好处是绕开了所有不可观测的量：不猜开机时刻、不猜真实剩余容量，只认两个协议层能看见的边界事件。

**充电判定：不能只看电量**

因为充电时读数虚高（§1.2），真实 75% 也可能显示成 100%。若只凭 `level ≥ full_level` 就认为"充满"，会把短充错记成满充，把续航均值拉短。因此加一道**充电时长交叉校验**：

| 规则 | 含义 |
|---|---|
| `wired` 由 0 变 1 时记 `charge_started_at` | 充电区间起点 |
| 首次出现 `level ≥ cycle_full_level`（默认 100）时记 `charged_at` | 候选充满点 |
| `charged_at − charge_started_at ≥ cycle_min_charge_min`（默认 20 min） | 才认定真正充满，开周期记 `quality=charged` |
| 不满足时长条件的拔线 | 记 `quality=partial`，附 `note="charge_too_short"`，不进平均 |
| 服务启动时已在充电 | 起点记为首次观测，`quality=charged_estimated` |

**状态机**

| 当前状态 | 事件 | 动作 |
|---|---|---|
| IDLE | `wired` 1→0，且此前已判定充满 | 开新周期，`quality=charged` |
| IDLE | `wired` 1→0，未判定充满 | `quality=partial`，`note` 区分 `not_full` / `charge_too_short` |
| IDLE | 服务首次启动时已是 `wired=0` 且 `level ≥ full_level` | 开周期，`quality=charged_estimated`，起点记为首次观测时刻 |
| CHARGING（`wired=1`） | 首次出现 `level ≥ full_level` | 记 `charged_at`；充电时长 ≥ `cycle_min_charge_min` 时置 `full_confirmed=true` |
| RUNNING | `level ≤ end_level` 且 `wired=0` | 关周期，落盘 |
| RUNNING | `wired` 0→1 | 关周期，`quality=interrupted`（没跑完又充上了） |
| RUNNING | 失联持续 > `cycle_dead_hours`（默认 24） | 关周期，`quality=unknown_end`（可能被关机收纳，不一定是没电） |
| RUNNING | 服务重启 | 从 `cycle-state.json` 恢复，不中断周期 |

**质量标记与统计口径**

| quality | 含义 | 纳入平均 |
|---|---|---|
| `charged` | 充满后拔线，跑到阈值 | 是 |
| `charged_estimated` | 同上，但起点是推测（服务晚于拔线启动） | 是，单独标注 |
| `partial` | 没充满就拔线 | 否 |
| `interrupted` | 没跑完又插上线 | 否 |
| `unknown_end` | 长期失联且未达阈值 | 否 |

平均/最佳/最差只对 `charged` 与 `charged_estimated` 计算，且**只取最近 20 个周期**——锂电池容量会衰减，把半年前的数据混进均值没有意义。

**时长口径**：用**墙钟**（`end_at − start_at`），包含 PC 关机与鼠标休眠时段。因为电池在这期间照常放电，若只累计"在线秒数"会系统性低估续航。同时单独记录 `offline_s` 供参考，但不从时长里扣除。

**落盘**

| 文件 | 内容 |
|---|---|
| `data/cycles.csv` | `cycle_id, start_at, start_unix, start_level, end_at, end_unix, end_level, duration_s, offline_s, charge_min, quality, note` |
| `data/cycle-state.json` | 进行中的周期（每 tick 原子替换；Windows 上须用 `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`，`std::fs::rename` 在目标已存在时会失败） |

**已知限制（写进实现注释，不要美化）**

1. 完全依赖 `wired` 标志，而它的语义**尚未实测**（§10 第 1 项）。插一次线就能确认，确认前该功能只能算设计就绪。若 `wired` 不翻转，**不拿"电量突然跳升"当替代信号**——虚高本身就会制造跳升，用它判充电等于自己骗自己，只会污染统计。
2. `end_level=10%` 是**读数**，不是真实剩余电量；电压法在低段非线性，所以 10% 背后剩多少没人知道。但同一阈值下的周期之间**可比**，这才是"能用多久"要的结论。
3. 若鼠标直接失联（没来得及报 10%），只能记成 `unknown_end`，不进平均——我不拿它凑数。
4. 长期插线使用（一直 `wired=1`）不会产生任何周期，这是预期行为。

### 3.8 会话记录与每日使用表

**会话定义**：一段连续在线窗口。`start` = 首次成功的 tick；`end` = **最后一次成功**的 tick。

为什么 `end` 不取"判定离线的那一刻"：判离线需要连续 2 次失败（最长 120 s），用失败时刻当终点会让每个会话凭空多出 1–2 分钟。用最后一次成功时刻，误差只剩 ±1 个节拍（±60 s）——正好落在你能接受的分钟级。

**精度**：起点与终点各 ±60 s（`tick_s=60`）。想更准就把节拍调到 30 s，代价是每天多 1440 次 HID 读。

**结束原因**

| reason | 触发 | 说明 |
|---|---|---|
| `no_reply` | 连续失败 ≥ 2 | 疑似休眠/关机/超范围，三者不可分（§3.6） |
| `device_removed` | 枚举不到设备 | 拔了 dongle 或拔了线 |
| `host_suspend` | `now − last_tick > 3 × tick` | PC 睡了，用**睡前最后一个 tick** 收尾，不把睡眠时段算进使用时间 |
| `service_stop` | 进程退出 | 优雅退出时收尾 |
| `ongoing` | 仍在进行 | 只在内存与状态文件中，未落 CSV |

**插拔线不算新会话**：`mode_change`（`11D9` ↔ `11E7`）不结束会话——你只是换了连接方式，人还在用。这条很重要，否则每次插线都会把使用时间切碎。重枚举一般只造成 1 次读取失败，而判离线门槛是 2 次，正好不会把换模式误判成休眠。

**服务重启**：从 `sessions-state.json` 恢复。若停机 ≤ 3 个节拍（最小 180 s）且设备仍在线，延续同一会话；否则以 `service_stop` 收尾并开新会话。状态文件每个节拍刷新一次 `last_seen_unix`，这是重启后判断"这段空档该不该算在线"的唯一依据。

**落盘**

`data/sessions.csv`：`session_id, start_at, start_unix, end_at, end_unix, duration_s, samples, end_reason`

**每日聚合不落盘**：`/daily` 每次请求时从 `sessions.csv` + `history-*.csv` 现算。单一真相（raw 数据），避免 `daily.csv` 与源数据打架。数据量很小（一天几十条会话 + 几百个采样点），现算毫秒级。

**每日表字段**

| 列 | 来源 | 说明 |
|---|---|---|
| 唤醒次数 | sessions | 当天 `start` 落在该日的会话数 |
| 在线时长 | sessions | Σ `duration_s` |
| 离线时长 | 24h − 在线 | 含休眠与关机 |
| 首次唤醒 / 末次休眠 | sessions | 时刻 |
| 最长一段 | sessions | 最长的单次会话 |
| 电量 首/末/最低/最高 | history | 当天非空采样 |
| 有线时长 | history | `wired=1` 的采样数 × tick |

> **口径提醒**：这里叫"在线时长"，不叫"使用时长"。鼠标只要没休眠就一直在响应，你人离开但鼠标还没睡的那段时间也会被计入。它是使用时间的**上界**，误差来自休眠超时本身（厂商固件决定，不可见）。这一点会写在 `/daily` 的响应字段名和 HTML 表头里，不玩文字游戏。

---

## 4. HTTP 接口契约

### 4.1 通用约定

- 仅监听 `127.0.0.1`（可配置，但文档不鼓励改）。
- `Content-Type: application/json; charset=utf-8`，UTF-8 无 BOM。
- 无 CORS 需求（LiteMonitor 是原生 HTTP 客户端，不是浏览器）。
- 不做鉴权：回环地址 + 只读数据，威胁模型可忽略。

### 4.2 `GET /battery`（主接口）

```json
{
  "ok": true,
  "stale": false,
  "age_s": 12.4,
  "present": true,
  "link": {
    "state": "online",
    "session_start_at": "2026-11-18T09:12:40+08:00",
    "session_uptime_s": 11245,
    "session_uptime_text": "3h07m",
    "offline_since": null,
    "consecutive_fails": 0
  },
  "cycle": {
    "start_at": "2026-11-15T20:10:00+08:00",
    "elapsed_s": 264000,
    "elapsed_text": "3d01h",
    "start_level": 100,
    "quality": "charged"
  },
  "battery": {
    "level": 15,
    "level_text": "15",
    "status": "discharging",
    "wired": false
  },
  "device": {
    "name": "Wireless mouse 8k dongle-L",
    "vid": "373B",
    "pid": "11D9",
    "mode": "wireless",
    "interface": "0xFF02/17B"
  },
  "source": "atk1",
  "raw": "08 04 00 00 00 02 0f 00 0e 8a 00 00 00 00 00 00 a0",
  "read_ms": 25,
  "updated_at": "2026-11-18T12:31:05+08:00",
  "error": null
}
```

| 字段 | 类型 | 语义 |
|---|---|---|
| `ok` | bool | 是否**曾经**成功读到过值（即使当前陈旧仍为 true） |
| `stale` | bool | `age_s > cache_ttl_s`，表示这次拿到的是刷新前的值 |
| `age_s` | number | 距上次成功读取的秒数，1 位小数 |
| `battery.level` | int \| null | 原始百分比，0–100 |
| `battery.level_text` | string | 显示用字符串；无值时为 `"--"`（避免插件里出现 `"--%"`） |
| `battery.status` | string | `discharging` / `wired` / `unknown`。**不臆造 `charging`** |
| `battery.wired` | bool | `rep[7] != 0` |
| `present` | bool | 设备接口当前是否存在于系统中 |
| `device.mode` | string | `wireless`（`11D9`）/ `wired`（`11E7`）；同时存在时取有线 |
| `link.state` | string | `online` / `offline`。**不区分休眠、关机、超范围**（§3.6） |
| `link.session_start_at` | string \| null | 本次在线窗口起点（首次成功应答），**不是真实开机时间** |
| `link.session_uptime_s` | int | `now − session_start_at`；服务重启后从事件日志恢复 |
| `link.session_uptime_text` | string | `3h07m` / `12m` / `—`，给插件直接用，免去模板算术 |
| `link.offline_since` | string \| null | 判定离线的时刻 |
| `link.consecutive_fails` | int | 连续失败次数，`≥ offline_after_fails` 时转 offline |
| `cycle` | object \| null | 当前放电周期摘要；无进行中周期时为 `null` |
| `cycle.elapsed_text` | string | `3d01h`，插件可直接显示，免去模板算术 |
| `cycle.quality` | string | `charged` / `charged_estimated` / `partial` / `interrupted` / `unknown_end`（§3.7） |
| `raw` | string | 应答帧十六进制（小写、空格分隔），排障用 |
| `read_ms` | int | 本次成功读取耗时 |
| `updated_at` | string | RFC3339 本地时间（带偏移） |
| `error` | string \| null | 最近一次失败原因，成功后置 null |

**状态码**：有任意历史值 → `200`；从未成功过 → `503` + `ok:false`。

### 4.3 `GET /health`

```json
{ "ok": true, "version": "0.1.0", "uptime_s": 3120, "present": true,
  "last_age_s": 42.1, "refreshing": false, "last_error": null }
```

用途：确认服务活着、设备在不在。**不触发刷新**。

### 4.4 `GET /raw`

```json
{ "request": "08 04 00 00 00 00 00 00 00 00 00 00 00 00 00 00 49",
  "raw": "08 04 00 00 00 02 0f 00 0e 8a 00 00 00 00 00 00 a0",
  "read_ms": 25, "at": "2026-11-18T12:31:05+08:00" }
```

排障专用：换固件、换鼠标时用来比对帧结构。

### 4.5 `GET /session` — 在线状态

```json
{ "state": "online", "session_start_at": "2026-11-18T09:12:40+08:00",
  "session_uptime_s": 11245, "session_uptime_text": "3h07m",
  "offline_since": null, "consecutive_fails": 0,
  "today": { "online_s": 21400, "offline_s": 3200, "transitions": 7 } }
```

`today` 由 `events-*.csv` 当日记录现算：在线总时长、离线总时长、切换次数。这是"今天它醒了多少次"的直接答案。

### 4.6 `GET /history?hours=24` — 曲线数据

```json
{ "from": "2026-11-17T12:00:00+08:00", "to": "2026-11-18T12:00:00+08:00",
  "count": 312,
  "points": [
    { "t": 1763436060, "level": 15, "wired": 0, "link": "online" },
    { "t": 1763436120, "level": null, "wired": 0, "link": "offline" }
  ],
  "stats": { "min": 8, "max": 17, "first": 15, "last": 14 } }
```

| 约定 | 说明 |
|---|---|
| `hours` 上限 | 720（= 保留期 30 天），超出按 30 天截断，不报错 |
| 24 h 内 | 走内存环形缓冲，零磁盘 I/O |
| 超过 24 h | 读对应日期的 CSV 文件 |
| `level: null` | **必须保留**这样的点，它代表"当时离线"。丢掉它们等于把断链画成直线，是造假 |

### 4.7 `GET /events?days=7` — 事件流水

```json
{ "count": 12,
  "events": [
    { "t": 1763436000, "event": "offline", "detail": "fails=2" },
    { "t": 1763437200, "event": "online",  "detail": "recovered" }
  ] }
```

### 4.8 导出与看图（可选，P5）

| 端点 | 返回 | 说明 |
|---|---|---|
| `GET /history.csv?days=7` | `text/csv; charset=utf-8` + `Content-Disposition: attachment` | 多天拼接，表头只写一次；直接拖进 Excel |
| `GET /chart?hours=24` | `text/html` | 内嵌 canvas 折线页，**不引任何外部 JS/CDN**，断网可用；断链区段留空 |

以上所有端点（含 `/chart`）**只读内存或磁盘，永不触发 HID 读取**，与 `/battery` 的刷新逻辑完全解耦。

### 4.9 `GET /cycles?limit=10` — 放电周期与续航统计

```json
{ "current": {
    "start_at": "2026-11-15T20:10:00+08:00", "elapsed_s": 264000, "elapsed_text": "3d01h",
    "start_level": 100, "last_level": 15, "quality": "charged", "offline_s": 1200 },
  "history": [
    { "start_at": "2026-11-08T21:00:00+08:00", "end_at": "2026-11-15T19:40:00+08:00",
      "duration_s": 592800, "duration_text": "6d20h", "end_level": 9, "quality": "charged" }
  ],
  "stats": { "count": 3, "avg_duration_text": "6d17h",
             "best": "7d00h", "worst": "6d02h", "window": "最近 20 个合格周期" } }
```

| 约定 | 说明 |
|---|---|
| `history` 排序 | 按 `end_at` 倒序 |
| `limit` | 默认 10，上限 100 |
| `stats.count` | 参与统计的周期数（只含 `charged` / `charged_estimated`） |
| `stats` 为 null | 还没有任何合格周期时返回 `null`，不返回 0 或伪造值 |

### 4.10 `GET /sessions?day=2026-11-18`

```json
{ "day": "2026-11-18", "count": 7, "online_s": 21400, "online_text": "5h57m",
  "sessions": [
    { "wake_at": "2026-11-18T08:12:00+08:00", "sleep_at": "2026-11-18T09:40:00+08:00",
      "duration_s": 5280, "duration_text": "1h28m", "end_reason": "no_reply" }
  ] }
```

不传 `day` 取当天；`ongoing` 的会话以 `sleep_at: null`、`end_reason: "ongoing"` 出现在列表首位。

### 4.11 `GET /daily?days=14` — 每日电量/使用时间表

```json
{ "days": [
  { "date": "2026-11-18", "sessions": 7,
    "online_s": 21400, "online_text": "5h57m", "offline_s": 65000,
    "first_wake": "08:12", "last_sleep": "23:41", "longest_s": 5280,
    "level": { "first": 15, "last": 12, "min": 9, "max": 17 },
    "wired_s": 3600 }
] }
```

| 约定 | 说明 |
|---|---|
| 排序 | 按日期升序，最后一项是今天 |
| `days` | 默认 14，上限 90 |
| 字段命名 | 用 `online_*` 而不是 `usage_*`，理由见 §3.8 结尾 |
| CSV | 同数据可用 `/daily.csv?days=14` 导出，表头一行，Excel 直开 |

### 4.12 `GET/POST /api/config` — 设置页

`GET` 返回**持久化配置**（不含 CLI/环境变量覆盖，避免把 `--no-tray` 这类临时开关写进文件）；
`POST` 接受同名字段的子集，校验通过后写入 exe 同目录 `config.json`，并在 500 ms 后自动重启。

| 要点 | 说明 |
|---|---|
| 校验顺序 | 先 `check_raw` 校验原始值，再 `apply_json`。反过来会被 clamp 静默改写（`tick_s=1` 变 5、`port=70000` 截成 4464） |
| 跨站防护 | 检查 `Origin`，只接受 `127.0.0.1` / `localhost`，否则 403 |
| 自动重启 | 释放单实例互斥量 → 拉起新进程（带 `HIDREAD_RESTART`，最多等 4 s 拿锁）→ 旧进程退出 |
| 失败行为 | 400 返回具体原因，**不写文件、不重启** |
| 生效方式 | 统一"保存即重启"，不做运行时热改，避免半生效的中间态 |

### 4.13 `GET /usage?days=14` — 每小时在线分钟数

```json
{ "days": [ { "date": "2026-11-18",
              "hours": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
              "total_s": 780 } ] }
```

供设置页的"课程表"热力图使用：横轴＝日期，纵轴＝0–23 时，格子深浅＝该小时在线分钟数。

计算方式是**会话区间与小时窗口求交集**，不是数采样点——采样点受节拍与休眠判定影响，会系统性低估在线分钟数。

---

## 5. 配置

优先级：**CLI 参数 > 环境变量 `HIDREAD_*` > `config.json`（exe 同目录）> 内置默认**。

| 键 | 默认 | 说明 |
|---|---|---|
| `bind` | `127.0.0.1` | 改成 `0.0.0.0` 才对外可见，默认不做 |
| `port` | `8731` | |
| `cache_ttl_s` | `60` | 本次定案值 |
| `vid` | `0x373B` | |
| `pids` | `[0x11D9, 0x11E7]` | 无线 / 有线两种模式；两者同时存在时优先取有线（`11E7`） |
| `usage_page` | `0xFF02` | 换鼠标时的第一调整项 |
| `read_timeout_ms` | `250` | 单次读超时 |
| `read_deadline_ms` | `800` | 单轮总预算 |
| `retries` | `3` | |
| `tick_s` | `60` | 节拍周期；采集、状态判定、缓存刷新共用 |
| `offline_after_fails` | `2` | 连续失败几次判定离线（≈120 s 确认） |
| `history_interval_s` | `300` | 基线写入间隔（值未变化时的兜底采样） |
| `history_days` | `30` | 历史保留天数，启动时清理 |
| `cycle_full_level` | `100` | 充电时达到此值才算"候选充满"（虚高问题见 §3.7） |
| `cycle_min_charge_min` | `20` | 充电最短时长，不到则判 `partial`，防止虚高误判 |
| `cycle_end_level` | `10` | 电量 ≤ 此值判定周期结束 |
| `cycle_dead_hours` | `24` | 失联超过此时长判 `unknown_end`，不进平均 |
| `data_dir` | `data` | **相对路径基于 exe 所在目录**（绿色版：数据全在程序文件夹内，不受启动时 CWD 影响）；CLI `--data-dir` / 环境变量例外，按 CWD 解释；不可写时回退 `%LOCALAPPDATA%\hidread\data` |
| `log_file` | `null` | 追加写；不做滚动（避免引入额外依赖） |

`config.json` 示例见 `config.example.json`（实现批次产出）。配置文件缺失时按默认值运行，不报错。

---

## 6. CLI

| 命令 | 行为 | 典型输出 |
|---|---|---|
| `hidread serve` | 启动服务（默认子命令） | `listening on 127.0.0.1:8731, device 373B:11D9, ttl=60s` |
| `hidread once` | 同步读一次并打印 | `鼠标 15%（放电中，25ms）` |
| `hidread once --json` | 同上，输出与 `/battery` 同构的 JSON | — |
| `hidread list` | 列出全部 HID 接口（含 usage page / 三个长度） | 每行一个接口 |
| `hidread probe` | 对本机目标 VID/PID 的每个厂商接口逐一试探，dump 原始帧 | 每个接口一行结论 |
| `hidread history --days 7` | 把历史 CSV 打到 stdout（可重定向存档） | 与 `/history.csv` 同内容 |
| `hidread cycles --limit 10` | 打印放电周期表与续航统计 | 纯文本表格，便于贴到聊天里 |
| `hidread sessions --day today` | 打印当日会话（唤醒/休眠时刻） | 每行一段，含时长与结束原因 |
| `hidread daily --days 14` | 打印每日在线时长/电量表 | 纯文本表格 |

> `once` / `probe` 与服务**互斥**：两者都会抢占同一设备的输入队列，跑之前先停服务（或直接靠 §3.4 的单实例锁拦住 `serve`）。

---

## 7. LiteMonitor 插件

### 7.1 安装步骤

1. 把 `MouseBattery.json` 放到 `<LiteMonitor 安装目录>\resources\plugins\`。
2. 设置页 → 插件 → 「重载插件」。
3. 添加「鼠标电量」监控项，端口保持 `8731`。
4. 任务栏/面板即显示 `鼠标 15%`。

### 7.2 完整插件 JSON

```json
{
  "id": "atk_mouse_battery",
  "meta": {
    "name": "鼠标电量",
    "version": "1.0",
    "author": "dauroar",
    "description": "通过本地 hidread 服务读取 ATK X1 Ultimate 2.0（2.4G）电量"
  },
  "inputs": [
    { "key": "port", "label": "服务端口", "type": "text", "default": "8731", "scope": "target" }
  ],
  "execution": {
    "type": "chain",
    "interval": 30,
    "steps": [
      {
        "id": "battery",
        "url": "http://127.0.0.1:{{port}}/battery",
        "method": "GET",
        "response_format": "json",
        "cache_minutes": 0,
        "extract": {
          "level": "battery.level",
          "display": "battery.level_text"
        },
        "process": [
          {
            "var": "color_state",
            "source": "level",
            "function": "threshold_switch",
            "value_map": { "0": "2", "30": "1", "60": "0" }
          }
        ]
      }
    ]
  },
  "outputs": [
    {
      "key": "main",
      "label": "鼠标",
      "short_label": "M",
      "format_val": "{{display ?? '--'}}",
      "unit": "%",
      "color": "{{color_state}}"
    }
  ]
}
```

### 7.3 设计说明

| 点 | 理由 |
|---|---|
| 用 `chain` 而非 `api_json` | `interval`、`steps[].extract`、`process` 在官方指南中只于 `chain` 下完整定义 |
| `cache_minutes: 0` | 缓存由本地服务负责，插件侧必须关掉，否则会叠加两层缓存 |
| `value_map` 反序 | 该函数是「大于等于」语义：`≥60` 绿、`≥30` 黄、`≥0` 红。**低电量才是红色**，所以键要倒着写 |
| 阈值取 60 / 30 | 电压法非线性，精确阈值无意义（见 §1.2） |
| `unit` 与 `level_text` 分离 | 无值时显示 `--` 而不是 `--%` |
| `interval: 30` | 与 TTL 60 配合：命中新鲜缓存 → 秒回；命中陈旧缓存 → 秒回旧值 + 后台刷新 |

### 7.4 显示滞后

| 时刻 | 服务端行为 | 插件看到 |
|---|---|---|
| t=0 | 预热读到 15% | 15%（age 0） |
| t=30 | 命中缓存 | 15%（age 30） |
| t=60 | 过期 → 返回旧值并后台刷新 | 15%（stale，age 60） |
| t=90 | 刷新已生效 | 新值（age ≈30） |

最坏情况下显示值滞后约 **90 秒**。对电量无实际影响。

### 7.5 已知限制

- 插件的 HTTP 超时/重试不可配置，因此**必须**靠服务端保证"立即返回"。这也正是形态 B+ 的核心约束。
- 插件只显示错误/未知状态，拿不到具体原因；排障请看 `/raw`。
- 想额外显示「充电中」，在 `steps[0].extract` 里加 `"state": "battery.status"`，再补一个 `outputs` 项即可（一行改动，不影响主项）。
- 想同时显示「本次在线」与「本次放电」，把 `steps[0].extract` 改成下面这个对象，再补两个 output：

```json
{ "level": "battery.level", "display": "battery.level_text",
  "uptime": "link.session_uptime_text", "cycle_elapsed": "cycle.elapsed_text" }
```

```json
{ "key": "session", "label": "在线", "short_label": "UP", "format_val": "{{uptime ?? '—'}}", "unit": "" }
```

```json
{ "key": "cycle", "label": "放电", "short_label": "BAT", "format_val": "{{cycle_elapsed ?? '—'}}", "unit": "" }
```

---

## 8. 代码结构

```
hidread/
├─ Cargo.toml
├─ config.example.json
├─ plugins/
│  └─ MouseBattery.json          # §7.2 的成品
├─ src/
│  ├─ main.rs                    # CLI 分发、单实例锁、日志
│  ├─ config.rs                  # 三级优先级合并
│  ├─ hid.rs                     # SetupAPI 枚举、CreateFileW、双句柄读写
│  ├─ atk.rs                     # ATK1 帧构造/校验/解析（纯函数，可单测）
│  ├─ state.rs                   # Snapshot + Mutex + 节拍状态
│  ├─ link.rs                    # 在线状态机、唤醒/休眠事件
│  ├─ session.rs                 # 会话边界与时长、每日聚合
│  ├─ cycle.rs                   # 放电周期状态机、续航统计
│  ├─ history.rs                 # CSV 落盘/读取、保留期清理、24h 环形缓冲
│  ├─ poller.rs                  # 节拍线程：采集 → 状态 → 落盘
│  └─ http.rs                    # 12 个端点 + JSON 序列化
├─ data/                         # 运行时生成，加入 .gitignore
│  ├─ history-YYYY-MM-DD.csv
│  ├─ events-YYYY-MM-DD.csv
│  ├─ sessions.csv
│  ├─ sessions-state.json
│  ├─ cycles.csv
│  └─ cycle-state.json
└─ temp/hidread-设计方案.md       # 本文档
```

### 8.1 依赖

| crate | 版本 | 用途 |
|---|---|---|
| `windows` | 0.58+ | `Win32_Devices_HumanInterfaceDevice`、`Win32_Devices_DeviceAndDriverInstallation`（SetupAPI）、`Win32_Storage_FileSystem`、`Win32_System_IO`、`Win32_System_Threading`、`Win32_Foundation` |
| `tiny_http` | 0.12 | 同步 HTTP，单依赖 |
| `serde` / `serde_json` | 1 | 序列化 |
| `anyhow` | 1 | 错误上下文 |
| `chrono` | 0.4 | RFC3339 时间戳、按日分文件 |

历史落盘不引入任何数据库或 CSV crate：格式固定、字段全是数字和枚举，手写 `writeln!` 即可，也避免 `rusqlite` 带来的 C 编译链（与"不依赖 `cl.exe`"的选型原则一致）。

不用 `hidapi`：它要经 `cc` 编译 C，构建链更脆；本项目 HID 面很小（枚举 + 4 个 API），纯 Rust 更可控。预计 release 体积 0.6–1 MB。

### 8.2 节拍与请求的交互（伪代码，非实现）

```
# 节拍线程 —— 全进程唯一碰设备的地方
loop:
    gap = now - last_tick
    if gap > 3 * tick_s:                       # PC 睡过一觉
        events.write("host_resume", gap); fails = 0

    ok, resp, ms = read_atk1(handle, 3, 800ms) # 内部失败时关句柄、下次重枚举

    if ok:
        snap.update(level=clamp(resp[6]), wired=resp[7]!=0, raw=resp, ms=ms)
        fails = 0
        if link.state != online:
            link.to_online(); events.write("online")
        if level != last_level or now - last_row >= history_interval_s:
            history.write(level, wired, "online", ms)
    else:
        fails += 1
        if fails >= offline_after_fails and link.state == online:
            link.to_offline(); events.write("offline", f"fails={fails}")
            history.write(level=null, link="offline")

    sleep_until(next_tick)

# HTTP —— 只读快照
GET /battery:
    s = snap.read()
    if s.last_ok is None:                      # 冷启动兜底，仅此一处同步读
        kick_once_and_wait(2.4s)
    return s
```

---

## 9. 测试计划

### 9.1 单元测试（离线，不依赖硬件）

`src/atk.rs` 全部为纯函数，用现场真实报文断言：

| 用例 | 断言 |
|---|---|
| 校验和 | `checksum(0x08, [0x04,0×14]) == 0x49` |
| 请求帧 | 等于 `08 04 00 … 00 49`（17 B） |
| 真实应答解析 | `08 04 00 00 00 02 0f 00 0e 8a … a0` → `level=15, wired=false` |
| 校验位篡改 | 末位改 `0xA1` → `valid=false` |
| 长度不足 | 16 B 输入 → `valid=false` |
| 越界值 | `rep[6]=0xFF` → `level=100`，`error` 记 clamp |
| JSON golden | 序列化结果字段齐全、`level_text` 与 `level` 一致 |
| 历史行格式 | 给定快照 → CSV 列序、空值、`level=null` 的写法固定 |
| 事件去重 | 连续 5 次失败只产出 **1 条** `offline` 事件 |
| 挂起检测 | `gap=600s, tick=60s` → 产出 `host_resume`，`fails` 不累计 |
| session 恢复 | 从 events 末尾的 `online` 恢复 `session_start_at` |
| 环形缓冲 | 灌 1500 点后容量仍为 1440，最旧被淘汰 |
| 周期：充满拔线 | `wired 1→0` + `level=100` → 开 `charged` 周期 |
| 周期：没充满 | `wired 1→0` + `level=60` → `partial`，不进统计 |
| 周期：中途插线 | RUNNING + `wired 0→1` → `interrupted` |
| 周期：失联超时 | RUNNING + 25 h 无响应 → `unknown_end` |
| 周期：达到阈值 | RUNNING + `level=9` → 关周期，写入 `cycles.csv` |
| 周期：重启恢复 | 存在 `cycle-state.json` → 周期不中断、`cycle_id` 不变 |
| 统计口径 | 5 个周期里混入 `partial`/`interrupted`，只有 3 个进均值 |
| 会话终点 | 最后成功 t=100、判定失败 t=160 → `end=100`（不是 160），时长不含多出的 120 s |
| 会话跨模式 | 产出 `mode_change` 事件 → 会话不切分，`session_id` 不变 |
| PC 睡眠 | `gap=10min` → 会话在最后一个 tick 收尾，`host_suspend`，睡眠时段不计入在线 |
| 每日聚合 | 构造 2 天 sessions + history → 在线/离线/首末/最值全部正确 |

### 9.2 手工验收清单

| # | 场景 | 期望 |
|---|---|---|
| 1 | `hidread once` | 打印当前电量，热态耗时 < 100 ms（实测 ~25 ms） |
| 2 | `curl 127.0.0.1:8731/battery` | 200，`level` 与 `once` 一致 |
| 3 | 连续 100 次请求 | 无句柄泄漏，无单次耗时 > 100 ms |
| 4 | 静置 5 分钟 | 每次请求都返回，`age_s` 随 TTL 在 0–60 间波动 |
| 5 | 拔掉 dongle | `present:false`，有旧值则 200，服务不崩 |
| 6 | 重插 dongle | 60 s 内自动恢复，无需重启 |
| 7 | **插上充电线** | 记录 `rep[7]` 实际值 + 电量跳变幅度，回填 §2.4 与 §10 |
| 8 | 开两个 `serve` | 第二个以退出码 2 拒绝启动 |
| 9 | LiteMonitor 添加插件 | 任务栏显示 `鼠标 15%`，低于 30% 变红 |
| 10 | 静置 1 小时 | 历史每 5 min 一行基线，曲线连续；`/history` 点数与 CSV 行数一致 |
| 11 | 等鼠标休眠（约 5 min 不动） | ≤120 s 内出 `offline` 事件；动一下 ≤120 s 恢复 `online`，`session_start_at` 刷新 |
| 12 | PC 睡眠 10 分钟后唤醒 | 有 `host_resume` 事件，**无** `offline` 事件；曲线留空档而非直线 |
| 13 | `/history?hours=24` | `level:null` 的断链点保留，`stats` 只统计非空值 |
| 14 | 把 `data_dir` 指向只读目录 | 回退 `%LOCALAPPDATA%`，告警一次，服务照常 |
| 15 | `/history.csv?days=7` | Excel 直接打开不乱码，多天数据表头只一行 |
| 16 | 插线充满后拔线 | `/cycles` 出现 `current`，`quality=charged`，`start_level` 为拔线时刻读数（通常 99–100） |
| 17 | 跑到 10% 以下 | `current` 变 null，`history` 多一条，`duration` 与墙钟一致（含关机时段） |
| 18 | 中途插线 | 该周期标 `interrupted`，不进入 `stats` |

### 9.3 不做的测试

不做自动化集成测试：真实设备状态（休眠/拔插/充电）无法在 CI 里可靠复现。用 `probe` 子命令 + 上表人工过一遍即可。

---

## 10. 风险与未决项

| # | 项 | 影响 | 处置 |
|---|---|---|---|
| 1 | `rep[7]` 与 PID 的模式语义 | **已实测确认**（12:53）：插线后 `rep[7]` 由 `0x00` 变 `0x01`，PID 由 `11D9` 变 `11E7` | **风险关闭**；两信号独立，可交叉校验（§2.1、§2.4） |
| 2 | 充电时数值虚高（15% → 40%） | 用户已知，属固件特性 | **不修**。文档明示"插电瞬间读数不可信"；可选 follow-up：`wired=true` 时插件改显示「充电中」 |
| 3 | `rep[8]` / `rep[9]` 语义 | 仅影响 `/raw` 可读性 | 保持未知，不猜测、不映射 |
| 4 | `0xFF06`（49 B 收发）用途 | 可能藏有更细的充电/电压信息 | 范围外，`probe --all` 留作探针 |
| 5 | 电压法曲线非线性 | 低电量段可能断崖 | 阈值粗化到 60/30，不承诺精确 |
| 6 | 双进程抢设备 | 读到对方请求的回复 | 命名互斥量 + 文档声明互斥 |
| 7 | ATK2（`0xFF05/64B`） | 本机不适用 | 不实现；仅在 `probe` 输出里说明为何跳过 |
| 8 | 60 s 轮询是否影响续航 | 未知：若查询帧会经 2.4G 送达鼠标，可能推迟其深度休眠 | 默认 60 s，可调 120–300 s；观察一周续航，异常就调大 |
| 9 | "真实开机时间"不可得 | 与用户预期有落差 | 用 `session_start_at` + 界面文案约束规避（§3.6），不硬编一个假指标 |
| 10 | 跨天切文件 | 00:00 前后一行可能写进相邻日期文件 | 以行内 `ts` 为准，`/history` 按时间戳过滤，不依赖文件名 |
| 11 | 插线区段读数虚高 40 点 | 曲线若不按 `wired` 分段，历史会被误读成"电池回血" | 已定为纪律（§1.2 第 4 条）；`/chart` 有线区段用虚线 + 图例标注 |

---

## 11. 交付批次

| 批次 | 内容 | 验收方式 |
|---|---|---|
| P1 | `atk.rs` + 协议单测 | `cargo test` 全绿，不插鼠标也能跑 |
| P2 | `hid.rs` + `once` / `list` / `probe` | 命令行读到真实电量；插线时用 `probe` 回填 `rep[7]` 语义 |
| P3 | `state.rs` + `poller.rs` + `http.rs` + `config.rs` + 单实例锁 | `/battery` `/health` `/raw` 可用；拔插不崩 |
| P4 | `link.rs` + `session.rs` + `cycle.rs` + `history.rs` + `/session` `/sessions` `/daily` `/history` `/events` `/cycles` | 静置 1 小时有曲线；充满拔线到 10% 能算周期；`/daily` 给出每日在线表；休眠/唤醒、PC 挂起事件均正确 |
| P5 | 插件 JSON（可选：`/chart`、CSV 导出） | LiteMonitor 显示电量与在线时长 |

每批产物可独立运行，不出现"必须等全部写完才能验证"的情况。

---

## 12. 范围外（记录，不实现）

- 托盘菜单里的「立即读取一次 / 显示电量」等高级项
- 数值平滑/滤波、掉电速率预测、电量校准
- 剩余可用时长预测（电压法非线性 + 电池老化，外推必失真）
- 多设备聚合（鼠标 + 耳机）
- ATK2 协议、`0xFF06` 通道探测
- 给 LiteMonitor 提「内置 HID 外设电量」PR（PR #593 同类改动仍未合并）

---

## 附录 A · 参考源码索引

| 符号 | 位置 | 用途 |
|---|---|---|
| `HidIface` / `enumerate_hid` | `my_devices.py:225` / `:251` | SetupAPI 枚举范式、接口字段含义 |
| `HidDevice` | `my_devices.py:327` | 双句柄设计；注释说明同句柄并发阻塞读会互锁 |
| `atk1_checksum` | `my_devices.py:528` | 校验算法 |
| `atk_request1` | `my_devices.py:539` | 17 B 请求帧拼装 |
| `atk1_valid` | `my_devices.py:546` | 应答校验 |
| `atk_read_at` | `my_devices.py:613` | 3 次重试 / 800 ms 预算 / 250 ms 单次读 |
| `read_standard_battery` | `my_devices.py:587` | 证明本机无标准电量字段的检测逻辑 |
| `read_mouse` | `my_devices.py:926` | 调试残留所在（`:927-929`），不要照抄 |
| `DEVICES` | `my_devices.py:917` | 设备表不含 `0x11D9` |

## 附录 B · 实测原始数据

**枚举结果**（48 个 HID 接口，摘取目标设备 10 个）

```
373B:11D9 UP=FF05 U=0000 in=8  out=0  feat=0   | Compx | Wireless mouse 8k dongle-L
373B:11D9 UP=FF03 U=0000 in=8  out=0  feat=0
373B:11D9 UP=0001 U=0002 in=8  out=0  feat=0
373B:11D9 UP=000C U=0001 in=3  out=0  feat=0
373B:11D9 UP=0001 U=0080 in=2  out=0  feat=0
373B:11D9 UP=FF02 U=0002 in=17 out=17 feat=0   <-- 目标
373B:11D9 UP=FF04 U=0002 in=0  out=0  feat=8
373B:11D9 UP=FF06 U=0002 in=49 out=49 feat=0
373B:11D9 UP=0001 U=0002 in=8  out=0  feat=0
373B:11D9 UP=0001 U=0006 in=9  out=2  feat=0
```

**5 次连续读取**

```
1: ok=True level=15 status=discharging 17.2ms raw=08 04 00 00 00 02 0f 00 0e 8a 00 00 00 00 00 00 a0
2: ok=True level=15 status=discharging 28.8ms raw=...（同上）
3: ok=True level=15 status=discharging 25.6ms raw=...（同上）
4: ok=True level=15 status=discharging 26.9ms raw=...（同上）
5: ok=True level=15 status=discharging 25.5ms raw=...（同上）
```

**一次早期采样**（`[9]` 与校验位不同，说明该字节随电压漂移）

```
08 04 00 00 00 02 0f 00 0e 8f 00 00 00 00 00 00 9b
```

**插线后（2026-11-18 12:53，PID 变为 `11E7`，连续 4 次一致）**

```
08 04 00 00 00 02 37 01 0f 5b 00 00 00 00 00 00 a5
  level=0x37(55)  wired=0x01  b8=0x0F  b9=0x5B  校验通过
```

对照结论：`rep[7]` 是可靠的有线标志；`rep[8]` 在电量 15% 与 55% 两种情况下都是 `0x0F`，**与电量无关**。

## 附录 C · 实现对照（v0.1.0，2026-11-18）

| 项 | 设计 | 实际实现 |
|---|---|---|
| 形态 | B+ | 已实现：节拍线程 + 快照 + 12 个端点 |
| 节拍 | 60 s | 已实现（`--tick` 可调，最小 5） |
| 会话恢复 | 停机 ≤ 2 个节拍延续 | 改为 **≤ 3 个节拍且最小 180 s**；状态文件每 tick 写 `last_seen_unix`，重启后据此判断空档 |
| 采集粒度 | 值变化 / 300 s 基线 | 已实现，另外断链时强制补一个 `level=null` 点 |
| 放电周期 | 充满时长交叉校验 | 已实现，4 个单测覆盖（满充 / 短充 / 中途插线 / 统计口径） |
| 端点 | §4.1–4.11 | 另加 `/` 索引页、`/daily.csv`；共 13 个路径 |
| 依赖 | 无 C 工具链 | 已满足：`windows` + `tiny_http`，release 体积 **578 KB** |
| 测试 | P1 单测 | `cargo test` 13 项全绿（协议 9 + 周期 4） |
| 实机验证 | — | 无线 15% / 有线 70% 均读到，`read_ms` ≈ 1 ms |
| 启动形态 | — | 双击不弹控制台：GUI 子系统 + 托盘图标（打开状态页 / 退出）；从终端启动自动回挂父控制台；无控制台时日志写 `data/hidread.log`；命名互斥量防双开 |
| 开机自启 | — | `hidread autostart install/uninstall/status`，写 `HKCU\…\Run`（无需管理员）；注册命令带**绝对** `--data-dir`，避免登录时工作目录不同把数据写进 System32 |
| 设置页 | — | `/` 升级为设置页：状态卡片 + 全部参数表单；`GET/POST /api/config`；保存后自动重启（释放互斥量→拉起新进程）；`Origin` 校验防跨站；原始值校验防静默 clamp |
| 概览面板 | — | 三块缩略图：**使用时段周视图（本周 7 列时间轴）**、电量曲线预览 1 天、每日使用 7 天；曲线弹窗带 1/3/7/30 天切换、离线灰底、渐变面积、hover 十字线；使用时段弹窗为**最近 30 天时间轴**（左轴固定、右侧拖动/滚轮横向滚动、色块 hover 详情）；支持 `?zoom=…&range=…` 深链 |
| 休眠档位适配 | `sleep_s` | 鼠标驱动休眠档位（30s–30min）。判离线阈值 = max(失败次数下限, 档位/节拍+2)，避免「静止即休眠」被当成离开；会话区间按 max(5min, 3×档位) 合并休眠间隙 |
| 时间轴自学习 | `/week` | 取近 30 天每小时在线分钟数，某小时"≥2 天且≥10% 天数"有记录才算活跃；前后各留 1 小时、最少 8 小时跨度。例：9–24 点活跃 → 显示 08:00–24:00 |
| 历史接口 | `/history?hours=N` | 增加 `max` 参数（默认 1200）：桶降采样到 ≤N 点，30 天 4.3 万点压成 1500 点返回，浏览器端才画得动 |
| 便携性 | — | `config.json` 与 `data/` 全部在 exe 同级目录；启动时预热最近 24h 历史，重启后曲线不空白 |

未实现（明确范围外）：剩余时长预测、ATK2 协议、`0xFF06` 通道探测、托盘里的「立即读取一次」。
