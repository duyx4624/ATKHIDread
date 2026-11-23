# hidread

**English summary**

`hidread` is a small Windows tray utility that reads the battery level of ATK-solution 2.4G mice and exposes it through a local HTTP dashboard. It polls the device on a configurable tick, records battery samples and online sessions as CSV files, and derives charge/discharge cycles for battery-life comparison.

- Single-file Windows executable (~800 KB), no runtime dependencies.
- Battery curve, daily online time, weekly heatmap, and charge/discharge cycles.
- Self-contained settings page at `http://127.0.0.1:8731` with no external assets.
- Local-only by default; write APIs reject non-loopback origins.
- Portable layout: `config.json` and `data/` live next to the executable.

Quick start: unzip a release package, run `hidread.exe`, then right-click the tray icon for settings. Build from source with `build.cmd` (or `pwsh tools/build.ps1`); the script produces `dist/hidread-<version>-win64.zip`.

Details: [design (Chinese)](docs/hidread-设计方案.md) · [protocol & measurements (Chinese)](docs/hidread-设计方案-详细版.md) · [usage (Chinese)](使用说明.txt). Licensed under [MIT](LICENSE).

---

## 项目简介

hidread 读取 ATK 方案 2.4G 鼠标（VID `373B`，PID `11D9` / `11E7`，HID usage page `FF02`）的电量，把每次「设备在线」记成会话，按天落盘，再用本地网页查看曲线与使用时段。

它不追求精确到秒的行为分析，只求长期趋势可信、数据可自己复算。程序为单文件 Windows 可执行文件，默认只监听回环地址。

## 功能特性

- 按节拍采样电量与有线状态，形成可回看的曲线。
- 识别使用时段：每日时长、日历周视图、使用时段热力图。
- 推导充放电周期：满充拔线到电量降到阈值算一次续航。
- 曲线弹窗支持 1 / 3 / 7 / 30 天切换，时间轴按活跃区间自学习。
- 业务日口径为 04:00–次日 03:59，凌晨使用归入前一天。
- 设置页可在线改配置并重启服务，支持 HKCU 开机自启。
- 数据目录按总量上限清理，最近 7 天始终保留。

## 运行环境

- Windows 10 / 11。
- ATK 方案的 2.4G 鼠标（接收器或数据线均可；插线时读数因端电压换算会偏高，属正常现象）。
- 源码构建需要 Rust 工具链（项目在 1.98.1 下验证过）。
- 浏览器访问 `http://127.0.0.1:8731`。

## 快速开始

1. 从发布包中解压出 `hidread.exe`、`config.example.json` 和 `plugins/`。
2. 双击 `hidread.exe`：无控制台窗口，托盘区出现电池图标。
3. 右键托盘图标 → **打开设置页** / **重启服务** / **退出**。
4. 若托盘图标被系统收进「隐藏的图标」，点任务栏 `^` 后拖到常显区域即可。

首次运行请确认鼠标接收器已插好；设置页电量应有数值。无窗口运行时日志写在 `data\hidread.log`。

## 从源码构建

```bat
build.cmd
```

或显式调用 PowerShell 脚本：

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File tools/build.ps1
```

脚本会执行 `cargo build --release`，读取 `Cargo.toml` 中的版本号，把 `hidread.exe`、`config.example.json`、`plugins/` 和按当前版本重写过的 `使用说明.txt` 收进 `dist\hidread-<版本>\`，压缩为 `dist\hidread-<版本>-win64.zip`，并打印包大小与 SHA256。

也可以只编译不打包：

```bash
cargo build --release
```

## 配置

把 `config.example.json` 复制为 `config.json` 后修改，重启生效；也可以直接在设置页改完点「保存并重启」。未知键会被忽略，缺项走内置默认值。

| 配置项 | 默认值 | 说明 |
|---|---|---|
| `bind` / `port` | `127.0.0.1` / `8731` | 监听地址与端口 |
| `tick_s` | `60` | 采样节拍，实际间隔带 ±10% 抖动 |
| `sleep_s` | `60` | 驱动休眠档位，决定快探间隔 |
| `offline_after_fails` | `2` | 判离线前至少确认几次 |
| `history_interval_s` | `300` | 稳态写盘间隔 |
| `data_max_mb` | `200` | 数据目录总量上限 |
| `cycle_*` | 见示例配置 | 满充阈值、最短充电、终止电量、周期超时 |
| `pids` | `["11D9","11E7"]` | 兼容的 PID 列表 |

开机自启默认关闭，需到设置页「服务」分组手动勾选；只写当前用户注册表，不需要管理员权限。

## 数据文件

数据都在 exe 同级的 `data\` 目录，整个目录可随程序一起搬走。

| 文件 | 内容 |
|---|---|
| `history-YYYY-MM-DD.csv` | 电量采样曲线：电量、有线状态、链路、读取耗时 |
| `sessions.csv` | 每次会话一行：起止时间、时长、结束原因 |
| `cycles.csv` | 充放电周期：满充 → 耗尽 |
| `events-YYYY-MM-DD.csv` | 状态跳变：上线、离线、重启 |
| `hidread.log` | 无窗口运行时的日志，超过 2 MB 自动重开 |

清理按目录总量控制（默认 200 MB），超限从最旧的按天文件删起，最近 7 天硬保留；`sessions.csv` 与 `cycles.csv` 是累积记录，不参与清理。

## HTTP 接口

| 路径 | 作用 |
|---|---|
| `/` | 设置页（内嵌 HTML，无外部资源） |
| `/battery` | 当前电量快照与链路状态 |
| `/history?hours=&max=` | 曲线数据，极值在降采样前计算 |
| `/daily?days=` | 每日在线时长与电量跨度 |
| `/week?days=&start=` | 周视图：会话段与活跃时间轴 |
| `/api/config` | 读写配置（POST 需 `application/json` 与回环来源） |
| `/health` | 存活与设备状态 |

## LiteMonitor 插件

把 `plugins\MouseBattery.json` 复制到 `LiteMonitor\resources\plugins\`，然后在 LiteMonitor 里重载插件。任务栏会显示「鼠标 70%」，低于 30% 变红。

## 文档

- [设计方案（精简版）](docs/hidread-设计方案.md)
- [设计方案（详细版）](docs/hidread-设计方案-详细版.md)——协议字节级推导、实测数据与源码索引
- [使用说明](使用说明.txt)
- [AGENT.md](AGENT.md)——开发约定

## 升级与卸载

升级只需替换 `hidread.exe`，保留 `data\` 与 `config.json`；新版对旧配置的未知键宽容、缺项走默认。

卸载：设置页取消「开机自启」→ 托盘右键「退出」→ 删除本目录。

## 许可

[MIT](LICENSE) © 2026 duyx4624
