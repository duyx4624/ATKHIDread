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
    pub cache_ttl_s: u64,
    pub offline_after_fails: u32,
    pub history_interval_s: u64,
    pub history_days: i64,
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".into(),
            port: 8731,
            tick_s: 60,
            cache_ttl_s: 60,
            offline_after_fails: 2,
            history_interval_s: 300,
            history_days: 30,
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

        cfg.data_dir = resolve_data_dir(&cfg.data_dir);
        Ok(cfg)
    }

    fn apply_json(&mut self, v: &Value) {
        if let Some(s) = v.get("bind").and_then(Value::as_str) {
            self.bind = s.to_string();
        }
        if let Some(n) = v.get("port").and_then(Value::as_u64) {
            self.port = n as u16;
        }
        if let Some(n) = v.get("tick_s").and_then(Value::as_u64) {
            self.tick_s = n.max(5);
        }
        if let Some(n) = v.get("cache_ttl_s").and_then(Value::as_u64) {
            self.cache_ttl_s = n;
        }
        if let Some(n) = v.get("offline_after_fails").and_then(Value::as_u64) {
            self.offline_after_fails = n.max(1) as u32;
        }
        if let Some(n) = v.get("history_interval_s").and_then(Value::as_u64) {
            self.history_interval_s = n.max(10);
        }
        if let Some(n) = v.get("history_days").and_then(Value::as_i64) {
            self.history_days = n.max(1);
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
            self.data_dir = PathBuf::from(v);
        }
    }

    fn apply_args(&mut self, args: &[String]) -> Result<()> {
        let mut i = 0;
        while i < args.len() {
            let key = args[i].as_str();
            let val = args.get(i + 1).map(String::as_str);
            let take = |v: Option<&str>| -> Result<String> {
                v.map(|s| s.to_string())
                    .with_context(|| format!("参数 {key} 缺少取值"))
            };

            match key {
                "--port" => self.port = take(val)?.parse().context("--port 需要数字")?,
                "--bind" => self.bind = take(val)?,
                "--tick" => self.tick_s = take(val)?.parse::<u64>()?.max(5),
                "--ttl" => self.cache_ttl_s = take(val)?.parse()?,
                "--data-dir" => self.data_dir = PathBuf::from(take(val)?),
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

选项：
  --port <N>        监听端口（默认 8731）
  --bind <ADDR>     监听地址（默认 127.0.0.1）
  --tick <秒>       采样节拍（默认 60）
  --interval <秒>   历史基线间隔（默认 300）
  --data-dir <路径> 数据目录（默认 ./data）
  --vid <HEX>       USB VID（默认 373B）
  --pids <HEX,HEX>  两种模式的 PID（默认 11D9,11E7）
";
