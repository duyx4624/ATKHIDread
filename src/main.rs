// 双击运行时不要弹出黑窗口；从终端启动时再把父控制台挂回来。
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod atk;
mod autostart;
mod config;
mod cycle;
mod hid;
mod history;
mod http;
mod session;
mod notice;
mod tray;
mod util;

use std::path::Path;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::Local;
use serde_json::Value;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    AttachConsole, GetConsoleWindow, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS,
    STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::Threading::CreateMutexW;

use crate::app::App;
use crate::config::{Config, WIRED_PID};

fn main() {
    setup_console();

    if let Err(e) = run() {
        eprintln!("错误：{e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("serve");
    let rest = if args.is_empty() { &args[..] } else { &args[1..] };

    match cmd {
        "serve" => cmd_serve(rest),
        "once" => cmd_once(rest),
        "list" => cmd_list(rest),
        "probe" => cmd_probe(rest),
        "sessions" => cmd_sessions(rest),
        "cycles" => cmd_cycles(rest),
        "daily" => cmd_daily(rest),
        "history" => cmd_history(rest),
        "autostart" => cmd_autostart(rest),
        "--install-autostart" => cmd_autostart(&[String::from("install")]),
        "--uninstall-autostart" => cmd_autostart(&[String::from("uninstall")]),
        "--help" | "-h" | "help" => {
            println!("{}", config::HELP);
            Ok(())
        }
        other => bail!("未知子命令 `{other}`（--help 查看用法）"),
    }
}

fn cmd_serve(args: &[String]) -> Result<()> {
    // 自启注册的命令带 --autostart：开机启动时不打扰，只在手动双击时提示
    let notify = !args.iter().any(|a| a == "--autostart");
    let filtered: Vec<String> = args.iter().filter(|a| *a != "--autostart").cloned().collect();
    let cfg = Config::load(&filtered)?;

    // 无控制台（双击启动）→ 日志写文件，避免日志凭空消失
    if !has_console() {
        let log = cfg
            .log_file
            .clone()
            .unwrap_or_else(|| cfg.data_dir.join("hidread.log"));
        redirect_output(&log);
    }

    if !acquire_single_instance(restart_wait_ms())? {
        eprintln!("hidread 已经在运行，本次启动直接退出");
        return Ok(());
    }

    history::History::purge(&cfg);

    let app = Arc::new(App::new(cfg.clone()));
    app.warmup();

    // 先绑定端口：端口被占用时立即失败，而不是静默跑在后台
    let server = http::bind(&app)?;

    let ticker = Arc::clone(&app);
    // 节拍是动态的：读取失败后进入"快速探测"，确认离线或恢复后回到常规节拍
    std::thread::Builder::new()
        .name("hidread-tick".into())
        .spawn(move || {
            // 数据上限是长期运行的约束：每天跨日时清一次，而不是只在启动时清
            let mut purge_day = Local::now().date_naive();
            loop {
                let wait = ticker.next_interval_s();
                std::thread::sleep(Duration::from_secs(wait));
                ticker.tick();

                let today = Local::now().date_naive();
                if today != purge_day {
                    purge_day = today;
                    history::History::purge(&ticker.cfg);
                }
            }
        })?;

    let http_app = Arc::clone(&app);
    std::thread::Builder::new()
        .name("hidread-http".into())
        .spawn(move || {
            if let Err(e) = http::run(server, http_app) {
                eprintln!("HTTP 服务退出：{e:#}");
            }
        })?;

    let url = format!("http://{}:{}", cfg.bind, cfg.port);
    if cfg.tray {
        if let Err(e) = tray::run(url, notify) {
            eprintln!("托盘不可用（{e:#}），转为纯后台运行");
            park();
        }
    } else {
        park();
    }
    Ok(())
}

/// 主线程停住（没有托盘时）。
fn park() {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn cmd_once(args: &[String]) -> Result<()> {
    let cfg = Config::load(args)?;
    let (iface, mut dev) = open_target(&cfg)?;
    let started = std::time::Instant::now();
    let reply = dev.exchange(
        cfg.retries,
        Duration::from_millis(cfg.read_timeout_ms),
        Duration::from_millis(cfg.read_deadline_ms),
    )?;
    let ms = started.elapsed().as_millis();
    let raw = match reply {
        hid::Exchange::Valid(raw) => raw,
        hid::Exchange::Invalid(raw) => bail!("帧校验失败：{}", atk::hex(&raw)),
        hid::Exchange::NoReply => {
            bail!("设备没有给出有效回复（鼠标可能已休眠，动一下鼠标再试）")
        }
    };
    let info = atk::parse(&raw).with_context(|| format!("帧校验失败：{}", atk::hex(&raw)))?;

    println!(
        "{} 电量 {}%（{}，{}）  {}ms",
        iface.product,
        info.level,
        if info.wired { "有线" } else { "无线" },
        iface.vid_pid(),
        ms
    );
    println!("raw = {}", atk::hex(&raw));
    Ok(())
}

fn cmd_list(args: &[String]) -> Result<()> {
    let cfg = Config::load(args)?;
    let ifaces = hid::enumerate()?;
    println!("共 {} 个 HID 接口", ifaces.len());
    for i in ifaces {
        let mark = if i.vid == cfg.vid { " *" } else { "" };
        println!("{}{}", i.label(), mark);
    }
    Ok(())
}

fn cmd_probe(args: &[String]) -> Result<()> {
    let cfg = Config::load(args)?;
    let ifaces = hid::enumerate()?;
    let targets: Vec<_> = ifaces
        .iter()
        .filter(|i| i.vid == cfg.vid && cfg.pids.contains(&i.pid))
        .collect();

    if targets.is_empty() {
        println!("没有找到 VID={:04X} 的目标设备", cfg.vid);
        return Ok(());
    }

    for iface in targets {
        println!("{}", iface.label());
        if iface.usage_page != cfg.usage_page || iface.output_len < atk::FRAME_LEN {
            continue;
        }
        match hid::Device::open(iface) {
            Ok(mut dev) => {
                match dev.exchange(
                    cfg.retries,
                    Duration::from_millis(cfg.read_timeout_ms),
                    Duration::from_millis(cfg.read_deadline_ms),
                ) {
                    Ok(hid::Exchange::Valid(raw)) | Ok(hid::Exchange::Invalid(raw)) => {
                        let parsed = atk::parse(&raw);
                        println!(
                            "    raw = {}\n    → {:?}{}",
                            atk::hex(&raw),
                            parsed,
                            if parsed.is_none() { "（校验未通过）" } else { "" }
                        );
                    }
                    Ok(hid::Exchange::NoReply) => {
                        println!("    没有回复（鼠标可能已休眠，动一下鼠标再试）")
                    }
                    Err(e) => println!("    读取失败：{e:#}"),
                }
            }
            Err(e) => println!("    打开失败：{e:#}"),
        }
    }
    Ok(())
}

fn cmd_sessions(args: &[String]) -> Result<()> {
    let cfg = Config::load(args)?;
    let day = arg_value(args, "--day")
        .unwrap_or_else(|| Local::now().format("%Y-%m-%d").to_string());

    let app = App::new(cfg);
    let v = app.sessions_json(&day);

    println!(
        "{} 共 {} 段，在线 {}",
        day,
        v["count"].as_u64().unwrap_or(0),
        v["online_text"].as_str().unwrap_or("-")
    );
    for s in v["sessions"].as_array().cloned().unwrap_or_default() {
        println!(
            "  {} → {}  {:>7}  {}",
            short_hm(&s["wake_at"]),
            short_hm(&s["sleep_at"]),
            s["duration_text"].as_str().unwrap_or("-"),
            s["end_reason"].as_str().unwrap_or("-")
        );
    }
    Ok(())
}

fn cmd_cycles(args: &[String]) -> Result<()> {
    let cfg = Config::load(args)?;
    let limit = arg_value(args, "--limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10);
    let app = App::new(cfg);
    let v = app.cycles_json(limit);

    if let Some(cur) = v.get("current").filter(|c| !c.is_null()) {
        println!(
            "进行中：{} 起，已放电 {}（起点 {}%）",
            short_dt(&cur["start_at"]),
            cur["elapsed_text"].as_str().unwrap_or("-"),
            cur["start_level"]
        );
    } else {
        println!("当前没有进行中的放电周期");
    }

    for c in v["history"].as_array().cloned().unwrap_or_default() {
        println!(
            "  #{} {} → {}  {:>7}  {}%→{}%  {}",
            c["cycle_id"],
            short_dt(&c["start_at"]),
            short_dt(&c["end_at"]),
            c["duration_text"].as_str().unwrap_or("-"),
            c["start_level"],
            c["end_level"],
            c["quality"].as_str().unwrap_or("-")
        );
    }

    if let Some(st) = v.get("stats").filter(|s| !s.is_null()) {
        println!(
            "统计：{} 个合格周期，平均 {}，最佳 {}，最差 {}",
            st["count"],
            st["avg_duration_text"].as_str().unwrap_or("-"),
            st["best"].as_str().unwrap_or("-"),
            st["worst"].as_str().unwrap_or("-")
        );
    } else {
        println!("统计：还没有合格的满充周期");
    }
    Ok(())
}

fn cmd_daily(args: &[String]) -> Result<()> {
    let cfg = Config::load(args)?;
    let days = arg_value(args, "--days")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(14);
    let app = App::new(cfg);
    let v = app.daily_json(days);

    println!("日期        唤醒  在线    首次   末次   电量 首/末/低/高  有线");
    for d in v["days"].as_array().cloned().unwrap_or_default() {
        println!(
            "{}  {:>3}  {:>6}  {:>5}  {:>5}   {}/{}/{}/{}   {}",
            d["date"].as_str().unwrap_or("-"),
            d["sessions"],
            d["online_text"].as_str().unwrap_or("-"),
            d["first_wake"].as_str().unwrap_or("--:--"),
            d["last_sleep"].as_str().unwrap_or("--:--"),
            num(&d["level"]["first"]),
            num(&d["level"]["last"]),
            num(&d["level"]["min"]),
            num(&d["level"]["max"]),
            crate::util::dur_text(d["wired_s"].as_i64().unwrap_or(0)),
        );
    }
    Ok(())
}

fn cmd_history(args: &[String]) -> Result<()> {
    let cfg = Config::load(args)?;
    let days = arg_value(args, "--days")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(7);
    let now = Local::now();
    let points = history::History::load_range(&cfg, now - chrono::Duration::days(days), now);

    println!("ts,ts_unix,level,wired,link,read_ms");
    for p in points {
        println!(
            "{},{},{},{},{},{}",
            util::rfc3339(p.at),
            util::unix(p.at),
            p.level.map(|v| v.to_string()).unwrap_or_default(),
            if p.wired { 1 } else { 0 },
            if p.online { "online" } else { "offline" },
            p.read_ms
        );
    }
    Ok(())
}

fn cmd_autostart(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str).unwrap_or("status") {
        "install" | "on" => {
            let cfg = Config::load(&[])?;
            let cmd = autostart::install(&cfg.data_dir)?;
            println!("已写入开机自启（仅当前用户）：");
            println!("  {cmd}");
            println!("撤销：hidread autostart uninstall");
        }
        "uninstall" | "off" => {
            if autostart::uninstall()? {
                println!("已移除开机自启");
            } else {
                println!("本来就没有自启项");
            }
        }
        "status" => match autostart::status()? {
            Some(cmd) => {
                println!("已启用：");
                println!("  {cmd}");
            }
            None => println!("未启用（hidread autostart install 可开启）"),
        },
        other => bail!("未知动作 `{other}`，可用：install | uninstall | status"),
    }
    Ok(())
}

fn open_target(cfg: &Config) -> Result<(hid::HidIface, hid::Device)> {
    let ifaces = hid::enumerate()?;
    let mut cands: Vec<hid::HidIface> = ifaces
        .into_iter()
        .filter(|i| {
            i.vid == cfg.vid
                && cfg.pids.contains(&i.pid)
                && i.usage_page == cfg.usage_page
                && i.output_len >= atk::FRAME_LEN
        })
        .collect();

    if cands.is_empty() {
        bail!(
            "没有找到电量接口（VID={:04X}, UsagePage=0x{:04X}），dongle/线缆插好了吗？",
            cfg.vid,
            cfg.usage_page
        );
    }

    cands.sort_by_key(|i| if i.pid == WIRED_PID { 0 } else { 1 });
    let iface = cands.remove(0);
    let dev = hid::Device::open(&iface)?;
    Ok((iface, dev))
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn short_hm(v: &Value) -> String {
    v.as_str()
        .and_then(|s| s.get(11..16))
        .unwrap_or("--")
        .to_string()
}

fn short_dt(v: &Value) -> String {
    match v.as_str() {
        Some(s) if s.len() >= 16 => format!("{} {}", &s[5..10], &s[11..16]),
        _ => "--".into(),
    }
}

fn num(v: &Value) -> String {
    if v.is_null() {
        "--".into()
    } else {
        v.to_string()
    }
}

// ---------------------------------------------------------------------------
// Windows 外壳：控制台、单实例
// ---------------------------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 从终端启动时把父控制台挂回来（双击启动时这里会失败，属预期）。
///
/// 只在标准句柄**无效**时才回挂：`hidread history > out.csv` 这类重定向
/// 已经给了有效句柄，无条件 SetStdHandle(CONOUT$) 会让输出写进控制台、
/// 文件恒为空（管道同理）。
fn setup_console() {
    unsafe {
        let out_ok = GetStdHandle(STD_OUTPUT_HANDLE)
            .map(|h| !h.is_invalid())
            .unwrap_or(false);
        let err_ok = GetStdHandle(STD_ERROR_HANDLE)
            .map(|h| !h.is_invalid())
            .unwrap_or(false);
        if out_ok && err_ok {
            return;
        }

        if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
            let name = wide("CONOUT$");
            if let Ok(h) = CreateFileW(
                PCWSTR(name.as_ptr()),
                (windows::Win32::Foundation::GENERIC_READ
                    | windows::Win32::Foundation::GENERIC_WRITE)
                    .0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            ) {
                if !out_ok {
                    let _ = SetStdHandle(STD_OUTPUT_HANDLE, h);
                }
                if !err_ok {
                    let _ = SetStdHandle(STD_ERROR_HANDLE, h);
                }
            }
        }
    }
}

fn has_console() -> bool {
    unsafe { !GetConsoleWindow().0.is_null() }
}

/// 无控制台时把 stdout/stderr 重定向到日志文件；超过 2 MB 就重开一个。
fn redirect_output(path: &Path) {
    use std::os::windows::io::AsRawHandle;

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::metadata(path)
        .map(|m| m.len() > 2 * 1024 * 1024)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(path);
    }

    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let raw = f.as_raw_handle();
        unsafe {
            let _ = SetStdHandle(STD_ERROR_HANDLE, HANDLE(raw));
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, HANDLE(raw));
        }
        // 句柄交给标准输出，进程生命周期内不关闭
        std::mem::forget(f);
    }
}

/// 命名互斥量保证只跑一份，避免两个进程抢同一个设备的输入队列。
/// 重启时新进程会等旧进程释放互斥量，所以这里允许等待一小段时间。
static SINGLETON: AtomicIsize = AtomicIsize::new(0);

fn restart_wait_ms() -> u64 {
    if std::env::var_os("HIDREAD_RESTART").is_some() {
        4000
    } else {
        0
    }
}

fn acquire_single_instance(wait_ms: u64) -> Result<bool> {
    // 允许通过 HIDREAD_INSTANCE 起多个实例（调试/并行测试用），默认仍是全局单例
    let suffix = std::env::var("HIDREAD_INSTANCE")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| format!("-{v}"))
        .unwrap_or_default();
    let name = wide(&format!(r"Local\hidread-singleton{suffix}"));
    let deadline = std::time::Instant::now() + Duration::from_millis(wait_ms);

    loop {
        unsafe {
            let handle =
                CreateMutexW(None, BOOL(1), PCWSTR(name.as_ptr())).context("创建互斥量失败")?;
            let already = windows::Win32::Foundation::GetLastError() == ERROR_ALREADY_EXISTS;
            if !already {
                SINGLETON.store(handle.0 as isize, Ordering::SeqCst);
                return Ok(true);
            }
            let _ = CloseHandle(handle);
        }

        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(120));
    }
}

/// 重启自身：释放互斥量 → 拉起新进程（带 `HIDREAD_RESTART`）→ 退出。
pub(crate) fn restart() -> Result<()> {
    let exe = std::env::current_exe().context("定位 exe 失败")?;
    let args: Vec<String> = std::env::args().skip(1).collect();

    let handle = SINGLETON.swap(0, Ordering::SeqCst);
    if handle != 0 {
        unsafe {
            let _ = CloseHandle(HANDLE(handle as *mut core::ffi::c_void));
        }
    }
    // 给系统一点时间销毁互斥量对象
    std::thread::sleep(Duration::from_millis(150));

    std::process::Command::new(&exe)
        .args(&args)
        .env("HIDREAD_RESTART", "1")
        .spawn()
        .with_context(|| format!("启动新实例失败：{}", exe.display()))?;

    std::process::exit(0);
}
