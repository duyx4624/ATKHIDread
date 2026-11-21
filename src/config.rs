//! 配置：CLI > 环境变量 `HIDREAD_*` > `config.json` > 内置默认。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

pub const DEFAULT_VID: u16 = 0x373B;
pub const WIRELESS_PID: u16 = 0x11D9;
pub const WIRED_PID: u16 = 0x11E7;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub port: u16,
    pub tick_s: u64,
    pub offline_after_fails: u32,
    /// 鼠标驱动里的休眠档位（秒）。判离线时会放宽到该时长 + 2 个节拍。
    pub sleep_s: u64,
    pub history_interval_s: u64,
    /// 数据目录上限（MB）：超过后从最旧的按天文件开始清理
    pub data_max_mb: u64,
    pub cycle_full_level: u8,
    pub cycle_min_charge_min: u64,
    pub cycle_end_level: u8,
    pub cycle_dead_hours: u64,
    pub vid: u16,
    pub pids: Vec<u16>,
    pub usage_page: u16,
    pub read_timeout_ms: u64,
    pub read_deadline_ms: u64,
    pub retries: u32,
    pub data_dir: PathBuf,
    pub log_file: Option<PathBuf>,
    /// 是否显示托盘图标（`--no-tray` 关闭，用于无桌面环境）
    pub tray: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".into(),
            port: 8731,
            tick_s: 60,
            offline_after_fails: 2,
            sleep_s: 60,
            history_interval_s: 300,
            data_max_mb: 200,
            cycle_full_level: 100,
            cycle_min_charge_min: 20,
            cycle_end_level: 10,
            cycle_dead_hours: 24,
            vid: DEFAULT_VID,
            pids: vec![WIRELESS_PID, WIRED_PID],
            usage_page: 0xFF02,
            read_timeout_ms: 250,
            read_deadline_ms: 800,
            retries: 3,
            data_dir: PathBuf::from("data"),
            log_file: None,
            tray: true,
        }
    }
}

impl Config {
    /// `args` 不含程序名与子命令。
    pub fn load(args: &[String]) -> Result<Self> {
        let mut cfg = Config::default();

        if let Some(v) = read_config_file() {
            cfg.apply_json(&v);
        }
        cfg.apply_env();
        cfg.apply_args(args)?;

        // 相对路径一律基于 exe 所在目录：保证"数据全部在软件文件夹里"，
        // 也不会因为从别处启动而把数据散落到当前工作目录。
        if cfg.data_dir.is_relative() {
            if let Some(dir) = config_path().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
                cfg.data_dir = dir.join(&cfg.data_dir);
            }
        }
        cfg.data_dir = resolve_data_dir(&cfg.data_dir);
        // 统一成绝对路径：自启注册、日志位置都依赖它
        cfg.data_dir = std::path::absolute(&cfg.data_dir).unwrap_or(cfg.data_dir);
        Ok(cfg)
    }

    pub fn apply_json(&mut self, v: &Value) {
        if let Some(s) = v.get("bind").and_then(Value::as_str) {
            self.bind = s.to_string();
        }
        if let Some(n) = v.get("port").and_then(Value::as_u64) {
            self.port = n as u16;
        }
        if let Some(n) = v.get("tick_s").and_then(Value::as_u64) {
            self.tick_s = n.max(5);
        }
        if let Some(n) = v.get("sleep_s").and_then(Value::as_u64) {
            self.sleep_s = n.clamp(5, 3600);
        }
        if let Some(n) = v.get("offline_after_fails").and_then(Value::as_u64) {
            self.offline_after_fails = n.max(1) as u32;
        }
        if let Some(n) = v.get("history_interval_s").and_then(Value::as_u64) {
            self.history_interval_s = n.max(10);
        }
        if let Some(n) = v.get("data_max_mb").and_then(Value::as_u64) {
            self.data_max_mb = n.clamp(16, 10240);
        }
        if let Some(n) = v.get("cycle_full_level").and_then(Value::as_u64) {
            self.cycle_full_level = n.min(100) as u8;
        }
        if let Some(n) = v.get("cycle_min_charge_min").and_then(Value::as_u64) {
            self.cycle_min_charge_min = n;
        }
        if let Some(n) = v.get("cycle_end_level").and_then(Value::as_u64) {
            self.cycle_end_level = n.min(100) as u8;
        }
        if let Some(n) = v.get("cycle_dead_hours").and_then(Value::as_u64) {
            self.cycle_dead_hours = n.max(1);
        }
        if let Some(s) = v.get("vid").and_then(Value::as_str) {
            if let Some(n) = parse_hex_u16(s) {
                self.vid = n;
            }
        }
        if let Some(arr) = v.get("pids").and_then(Value::as_array) {
            let list: Vec<u16> = arr
                .iter()
                .filter_map(|x| match x {
                    Value::String(s) => parse_hex_u16(s),
                    Value::Number(n) => n.as_u64().map(|v| v as u16),
                    _ => None,
                })
                .collect();
            if !list.is_empty() {
                self.pids = list;
            }
        }
        if let Some(s) = v.get("usage_page").and_then(Value::as_str) {
            if let Some(n) = parse_hex_u16(s) {
                self.usage_page = n;
            }
        }
        if let Some(n) = v.get("read_timeout_ms").and_then(Value::as_u64) {
            self.read_timeout_ms = n.max(10);
        }
        if let Some(n) = v.get("read_deadline_ms").and_then(Value::as_u64) {
            self.read_deadline_ms = n.max(50);
        }
        if let Some(n) = v.get("retries").and_then(Value::as_u64) {
            self.retries = n.clamp(1, 10) as u32;
        }
        if let Some(s) = v.get("data_dir").and_then(Value::as_str) {
            self.data_dir = PathBuf::from(s);
        }
        if let Some(s) = v.get("log_file").and_then(Value::as_str) {
            self.log_file = Some(PathBuf::from(s));
        }
        if let Some(b) = v.get("tray").and_then(Value::as_bool) {
            self.tray = b;
        }
    }

    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("HIDREAD_PORT") {
            if let Ok(n) = v.parse() {
                self.port = n;
            }
        }
        if let Ok(v) = std::env::var("HIDREAD_TICK") {
            if let Ok(n) = v.parse::<u64>() {
                self.tick_s = n.max(5);
            }
        }
        if let Ok(v) = std::env::var("HIDREAD_DATA_DIR") {
            // 环境变量按当前工作目录解释
            self.data_dir = std::path::absolute(&v).unwrap_or_else(|_| PathBuf::from(v));
        }
    }

    fn apply_args(&mut self, args: &[String]) -> Result<()> {
        let mut i = 0;
        while i < args.len() {
            let key = args[i].as_str();

            // 无值开关，单独处理（不能按 key+value 跳过两项）
            if key == "--no-tray" {
                self.tray = false;
                i += 1;
                continue;
            }

            let val = args.get(i + 1).map(String::as_str);
            let take = |v: Option<&str>| -> Result<String> {
                v.map(|s| s.to_string())
                    .with_context(|| format!("参数 {key} 缺少取值"))
            };

            match key {
                "--port" => self.port = take(val)?.parse().context("--port 需要数字")?,
                "--bind" => self.bind = take(val)?,
                "--tick" => self.tick_s = take(val)?.parse::<u64>()?.max(5),
                "--sleep" => self.sleep_s = take(val)?.parse::<u64>()?.clamp(5, 3600),
                "--data-dir" => {
                    // 命令行显式指定的路径按当前工作目录解释（和大多数 CLI 一致）
                    let raw = take(val)?;
                    self.data_dir = std::path::absolute(&raw).unwrap_or_else(|_| PathBuf::from(raw));
                }
                "--log" => self.log_file = Some(PathBuf::from(take(val)?)),
                "--vid" => {
                    self.vid = parse_hex_u16(&take(val)?).context("--vid 需要十六进制")?
                }
                "--pids" => {
                    let raw = take(val)?;
                    let list: Vec<u16> = raw
                        .split(',')
                        .filter_map(|s| parse_hex_u16(s.trim()))
                        .collect();
                    if list.is_empty() {
                        anyhow::bail!("--pids 需要至少一个 PID，例如 11D9,11E7");
                    }
                    self.pids = list;
                }
                "--interval" => {
                    self.history_interval_s = take(val)?.parse::<u64>()?.max(10)
                }
                // 子命令自己的参数，配置层不消费（但也不能当成未知参数报错）
                "--day" | "--days" | "--limit" => {}
                "--help" | "-h" => {
                    println!("{}", HELP);
                    std::process::exit(0);
                }
                other => {
                    if other.starts_with("--") {
                        anyhow::bail!("未知参数 `{other}`（--help 查看用法）");
                    }
                }
            }
            i += 2;
        }
        Ok(())
    }

    /// 只读配置文件（忽略 CLI / 环境变量），设置页编辑的就是这一份。
    pub fn load_file_only() -> Self {
        let mut cfg = Config::default();
        if let Some(v) = read_config_file() {
            cfg.apply_json(&v);
        }
        cfg
    }

    /// 校验设置页提交的**原始**值。
    ///
    /// 必须在 `apply_json` 之前调用：`apply_json` 对配置文件是防御性 clamp，
    /// 但用于用户输入时会静默改写（`tick_s=1` 变 5、`port=70000` 截成 4464），
    /// 那等于骗用户"保存成功了"。
    pub fn check_raw(v: &Value) -> Result<()> {
        let num = |key: &str| v.get(key).and_then(Value::as_u64);

        if let Some(n) = num("port") {
            if n == 0 || n > 65535 {
                anyhow::bail!("端口应在 1–65535 之间");
            }
        }
        if let Some(n) = num("tick_s") {
            if !(5..=3600).contains(&n) {
                anyhow::bail!("采样节拍应在 5–3600 秒之间");
            }
        }
        if let Some(n) = num("sleep_s") {
            if !(5..=3600).contains(&n) {
                anyhow::bail!("休眠档位应在 5–3600 秒之间");
            }
        }
        if let Some(n) = num("offline_after_fails") {
            if !(1..=10).contains(&n) {
                anyhow::bail!("判定离线所需失败次数应在 1–10 之间");
            }
        }
        if let Some(n) = num("history_interval_s") {
            if !(10..=86_400).contains(&n) {
                anyhow::bail!("历史基线间隔应在 10–86400 秒之间");
            }
        }
        if let Some(n) = num("data_max_mb") {
            if !(16..=10240).contains(&n) {
                anyhow::bail!("数据上限应在 16–10240 MB 之间");
            }
        }
        if let Some(n) = num("cycle_full_level") {
            if !(1..=100).contains(&n) {
                anyhow::bail!("满充判定电量应在 1–100 之间");
            }
        }
        if let Some(n) = num("cycle_min_charge_min") {
            if n > 600 {
                anyhow::bail!("最短充电时长不应超过 600 分钟");
            }
        }
        if let Some(n) = num("cycle_end_level") {
            if n > 99 {
                anyhow::bail!("周期结束电量应在 0–99 之间");
            }
        }
        if let Some(n) = num("cycle_dead_hours") {
            if !(1..=720).contains(&n) {
                anyhow::bail!("失联判定时长应在 1–720 小时之间");
            }
        }
        if let Some(n) = num("read_timeout_ms") {
            if !(10..=5000).contains(&n) {
                anyhow::bail!("单次读超时应在 10–5000 ms 之间");
            }
        }
        if let Some(n) = num("read_deadline_ms") {
            if !(50..=10_000).contains(&n) {
                anyhow::bail!("单轮预算应在 50–10000 ms 之间");
            }
        }
        if let Some(n) = num("retries") {
            if !(1..=10).contains(&n) {
                anyhow::bail!("重试次数应在 1–10 之间");
            }
        }

        if let Some(arr) = v.get("pids") {
            let Some(list) = arr.as_array() else {
                anyhow::bail!("PID 列表必须是数组");
            };
            if list.is_empty() {
                anyhow::bail!("至少需要一个 PID");
            }
            for item in list {
                let ok = match item {
                    Value::String(s) => parse_hex_u16(s).is_some(),
                    Value::Number(n) => n.as_u64().map(|x| x <= 0xFFFF).unwrap_or(false),
                    _ => false,
                };
                if !ok {
                    anyhow::bail!("PID 列表里有无法解析的值：{item}");
                }
            }
        }

        for key in ["vid", "usage_page"] {
            if let Some(s) = v.get(key).and_then(Value::as_str) {
                if parse_hex_u16(s).is_none() {
                    anyhow::bail!("{key} 必须是十六进制，例如 373B");
                }
            }
        }

        Ok(())
    }

    /// 历史 CSV 的路径。
    pub fn history_path(&self, date: &str) -> PathBuf {
        self.data_dir.join(format!("history-{date}.csv"))
    }

    pub fn events_path(&self, date: &str) -> PathBuf {
        self.data_dir.join(format!("events-{date}.csv"))
    }

    pub fn sessions_path(&self) -> PathBuf {
        self.data_dir.join("sessions.csv")
    }

    pub fn cycles_path(&self) -> PathBuf {
        self.data_dir.join("cycles.csv")
    }

    pub fn session_state_path(&self) -> PathBuf {
        self.data_dir.join("session-state.json")
    }

    pub fn cycle_state_path(&self) -> PathBuf {
        self.data_dir.join("cycle-state.json")
    }

    /// 设置页展示/回传用的 JSON（键名与 config.json 一致）。
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "bind": self.bind,
            "port": self.port,
            "tick_s": self.tick_s,
            "sleep_s": self.sleep_s,
            "offline_after_fails": self.offline_after_fails,
            "history_interval_s": self.history_interval_s,
            "data_max_mb": self.data_max_mb,
            "cycle_full_level": self.cycle_full_level,
            "cycle_min_charge_min": self.cycle_min_charge_min,
            "cycle_end_level": self.cycle_end_level,
            "cycle_dead_hours": self.cycle_dead_hours,
            "vid": format!("{:04X}", self.vid),
            "pids": self.pids.iter().map(|p| format!("{p:04X}")).collect::<Vec<_>>(),
            "usage_page": format!("{:04X}", self.usage_page),
            "read_timeout_ms": self.read_timeout_ms,
            "read_deadline_ms": self.read_deadline_ms,
            "retries": self.retries,
            "data_dir": self.data_dir.display().to_string(),
            "tray": self.tray,
        })
    }

    /// 保存到 exe 同目录的 `config.json`（读取时也是优先这个位置）。
    pub fn save(&self) -> Result<std::path::PathBuf> {
        let path = config_path().context("无法定位 exe 所在目录")?;
        std::fs::write(&path, serde_json::to_vec_pretty(&self.to_json())?)
            .with_context(|| format!("写入 {} 失败", path.display()))?;
        Ok(path)
    }

    /// 校验设置页提交的值；`current_port` 用于判断端口是否真的变了。
    pub fn validate(&self, current_port: u16) -> Result<()> {
        if self.tick_s < 5 || self.tick_s > 3600 {
            anyhow::bail!("采样节拍应在 5–3600 秒之间");
        }
        if !(1..=10).contains(&self.offline_after_fails) {
            anyhow::bail!("判定离线所需失败次数应在 1–10 之间");
        }
        if self.history_interval_s < 10 || self.history_interval_s > 86_400 {
            anyhow::bail!("历史基线间隔应在 10–86400 秒之间");
        }
        if !(16..=10240).contains(&self.data_max_mb) {
            anyhow::bail!("历史保留天数应在 1–365 之间");
        }
        if self.cycle_full_level == 0 || self.cycle_full_level > 100 {
            anyhow::bail!("满充判定电量应在 1–100 之间");
        }
        if self.cycle_end_level >= self.cycle_full_level {
            anyhow::bail!("周期结束阈值必须小于满充阈值");
        }
        if self.cycle_min_charge_min > 600 {
            anyhow::bail!("最短充电时长不应超过 600 分钟");
        }
        if !(1..=720).contains(&self.cycle_dead_hours) {
            anyhow::bail!("失联判定时长应在 1–720 小时之间");
        }
        if self.bind.parse::<std::net::IpAddr>().is_err() {
            anyhow::bail!("监听地址不是合法的 IP（例如 127.0.0.1）");
        }
        if self.port == 0 {
            anyhow::bail!("端口不能为 0");
        }
        if self.port != current_port {
            // 只有端口真的变了才试绑，避免把当前监听中的端口当成占用
            if let Ok(addr) = format!("{}:{}", self.bind, self.port).parse::<std::net::SocketAddr>() {
                if std::net::TcpListener::bind(addr).is_err() {
                    anyhow::bail!("端口 {} 已被占用", self.port);
                }
            }
        }
        if self.pids.is_empty() {
            anyhow::bail!("至少需要一个 PID");
        }
        if !(1..=10).contains(&self.retries) {
            anyhow::bail!("重试次数应在 1–10 之间");
        }
        Ok(())
    }
}

/// `config.json` 的首选位置（exe 同目录）。
pub fn config_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("config.json")))
}

fn read_config_file() -> Option<Value> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("config.json"));
        }
    }
    candidates.push(PathBuf::from("config.json"));

    for p in candidates {
        if let Ok(text) = std::fs::read_to_string(&p) {
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => return Some(v),
                Err(e) => eprintln!("警告：{} 解析失败（{e}），已忽略", p.display()),
            }
        }
    }
    None
}

/// 支持 `"11D9"` / `"0x11D9"` / `"0x373B"`。
pub fn parse_hex_u16(s: &str) -> Option<u16> {
    let t = s.trim();
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    u16::from_str_radix(t, 16).ok()
}

/// 目录不可写时回退到 `%LOCALAPPDATA%\hidread\data`。
fn resolve_data_dir(preferred: &Path) -> PathBuf {
    if is_writable(preferred) {
        return preferred.to_path_buf();
    }

    let fallback = std::env::var("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("hidread").join("data"))
        .unwrap_or_else(|_| PathBuf::from("data"));

    if is_writable(&fallback) {
        eprintln!(
            "提示：{} 不可写，数据改存到 {}",
            preferred.display(),
            fallback.display()
        );
        fallback
    } else {
        preferred.to_path_buf()
    }
}

fn is_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".write_probe");
    match std::fs::write(&probe, b"1") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

pub const HELP: &str = "\
hidread —— ATK 2.4G 鼠标电量桥接服务

用法：
  hidread serve [选项]        启动本地 HTTP 服务（默认子命令）
  hidread once                读一次并打印
  hidread list                列出 HID 接口
  hidread probe               探测目标设备的原始帧
  hidread sessions [--day D]  打印当日会话
  hidread cycles [--limit N]  打印放电周期
  hidread daily [--days N]    打印每日使用表
  hidread autostart [install|uninstall|status]
                              开机自启（写 HKCU Run，无需管理员）

选项：
  --port <N>        监听端口（默认 8731）
  --bind <ADDR>     监听地址（默认 127.0.0.1）
  --tick <秒>       采样节拍（默认 60）
  --interval <秒>   历史基线间隔（默认 300）
  --data-dir <路径> 数据目录（默认 ./data）
  --no-tray         不显示托盘图标（纯后台）
  --vid <HEX>       USB VID（默认 373B）
  --pids <HEX,HEX>  两种模式的 PID（默认 11D9,11E7）
";
