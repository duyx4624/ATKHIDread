# AGENT.md — 本仓库的开发约定

写给未来的维护者（人或 AI）。读完这一页，应当能独立完成一次改动、一次验证、一次发版。

## 这是什么

一个单文件的 Windows 托盘小工具：读取 ATK 方案鼠标（VID `373B`）的电量，按节拍采样，落盘成按天 CSV，并通过本地 HTTP 提供设置页、曲线与使用时段视图。

它有三个刻意的设计取向：

- **便携**：单个 exe，无运行时依赖，数据全部放在 exe 同级的 `data/`。
- **只读设备**：只发 HID 输出报告请求电量，绝不写设备。
- **可核对**：所有统计都能从 `data/*.csv` 复算出来，没有隐藏状态。

## 目录约定

| 路径 | 作用 | 能否删除 |
|---|---|---|
| `src/` | 全部 Rust 源码 | 否 |
| `docs/hidread-设计方案.md` | 设计与协议溯源，改动前先读 | 否 |
| `tools/my_devices.py` | 外部参考实现，协议字段的出处 | 可，但会失去溯源 |
| `plugins/` | LiteMonitor 插件（可选） | 可 |
| `dist/` | 发布包，git 忽略 | 可，重新打包即可 |
| `data/` | 运行期数据（exe 同级） | 否，删了丢历史 |
| `target/` | 构建产物 | 可，`cargo clean` |

## 构建与验证

```bash
cargo test                      # 25 项，3–5 秒
cargo build --release           # 约 11 秒，链接阶段可能再等 1 分钟
```

链接阶段卡在 `hidread(bin)` 是正常的：rustc 生成 811 KB exe + PDB，杀软要完整扫一遍，通常 1 分钟内结束。**不要因为这一步慢就去清缓存**，清完反而要全量重编。

打包发布：把 `target/release/hidread.exe` 与 `使用说明.txt`、`config.example.json`、`plugins/` 放进 `dist/hidread-<版本>/`，压缩并记录 SHA256。

## 运行与调试

| 场景 | 命令 |
|---|---|
| 正常启动（托盘） | 双击 exe |
| 无托盘（排查用） | `hidread serve --no-tray --port 8740 --data-dir <临时目录>` |
| 与生产实例并存 | 设环境变量 `HIDREAD_INSTANCE=test`（单实例锁会加后缀） |
| 单次读取 | `hidread once` |
| 看状态 | `curl 127.0.0.1:8731/health` |

单实例锁是 `Local\hidread-singleton`。调试时**不要**用 `taskkill /F /IM hidread.exe`，会把用户正在跑的实例一起杀掉；按 PID 精确结束，或用上面的环境变量并存。

## 时间口径（最容易搞错的地方）

| 概念 | 定义 | 出处 |
|---|---|---|
| 业务日 | 04:00 到次日 03:59，凌晨算前一天 | `session.rs::daily_until`、`app.rs::week_json` |
| 会话开始 | 首次读到数据；从离线恢复时再前移半拍，但不早于上次判离线 | `app.rs` tick 成功分支 |
| 会话结束 | 末次成功读取 + 半拍（半拍 = 上次实际间隔的一半） | `app.rs::mark_failure`、`half_gap` |
| 判离线 | 静默 ≥ `max(2×快探, 30s)` 且失败次数 ≥ 配置值 | `app.rs::mark_failure` |
| 快探 | `sleep_s/3`，夹在 10 秒与节拍之间 | `app.rs::probe_s` |
| 采样抖动 | 每次间隔 ±10%，防相位锁定 | `app.rs::jitter` |
| 图与统计 | 图用合并区间（视觉连续），统计用原始区间 | `session.rs::intervals_between(merge)` |

改动这些口径前，先想清楚会不会让 `/daily` 与 `/week` 给出不同的数字——它们必须一致。

## 数据格式

- `sessions.csv`：每次会话一行，`session_id,start_at,start_unix,end_at,end_unix,duration_s,end_reason`。
- `history-YYYY-MM-DD.csv`：按天采样点，`ts,ts_unix,level,wired,link,read_ms`；只在值变化或基线间隔时写。
- `cycles.csv`：充放电周期。
- `events-YYYY-MM-DD.csv`：状态跳变（online/offline/restart）。
- `config.json`：只放配置；未知键会被忽略，缺项走默认值。

`history_interval_s` 决定稳态写盘密度，`data_max_mb` 控制目录总量（超限从最旧按天文件删，最近 7 天硬保留）。

## 代码规范

- 模块边界：`hid`（设备）→ `app`（节拍与状态机）→ `http`（只读快照）。HTTP 线程**永远不碰设备**。
- 文件 I/O 不持 `inner` 锁：先取快照，`drop(g)` 之后再读盘。
- 时间一律用 `chrono::DateTime<Local>`，落盘同时写 RFC3339 与 unix 秒。
- 新增配置项必须三处齐全：`Config` 字段、`to_json`、`check_raw` 校验；只加一处会静默失效。
- 新增接口要有单测或可复现的 curl 命令，写进本文件或设计文档。

## 提交规范

```
type(scope): 中文简述
```

- `type` 只用 `feat` / `fix` / `style`，禁止 `bugfix`。
- 一次提交只做一件事，正文用 `-` 列要点，说清「改了什么、为什么、怎么验证」。
- 版本遵循语义化：patch 修 bug、文案、样式；minor 是行为或统计口径变更；major 是数据格式不兼容。

## AI 协作约定

- **先读再答**：涉及行为的问题，先看源码与文档，给出文件行号。
- **最小变更**：只改被要求的部分，范围外的问题记成 follow-up，不顺手改。
- **不代劳敏感操作**：git 提交、推送、合并冲突由本人执行；准备命令即可。
- **不杀生产进程**：需要重启用户实例时，明确告知命令，不要自己执行 `taskkill /IM`。
- **验证优先于声称**：说「修好了」之前，跑测试或给出 curl 结果。

## 已知边界

- 鼠标休眠 ≠ 人离开：程序只能看到设备是否上报，无法判断你是否在敲键盘。
- 采样精度受节拍限制，两端各用半拍近似真实边界；节拍越小越准。
- `tiny_http` 没有 socket 读超时，半开连接最多占住一个 worker。
- 取消读用 `INFINITE` 等待完成，是为了保证 `OVERLAPPED` 生命周期安全。
- 蓝牙等非 HID 通道的设备（如部分散热器）不在当前枚举范围内。

## 扩展方向

见设计文档末尾「展望」——多设备（键盘、耳机、散热器）需要把「设备身份」从写死的 VID/PID 提升为一等公民，并为每个设备维护独立的会话与历史。
