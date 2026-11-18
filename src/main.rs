mod app;
mod atk;
mod config;
mod cycle;
mod hid;
mod history;
mod http;
mod session;
mod util;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::Local;
use serde_json::Value;

use crate::app::App;
use crate::config::{Config, WIRED_PID};

fn main() {
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
        "--help" | "-h" | "help" => {
            println!("{}", config::HELP);
            Ok(())
        }
        other => bail!("未知子命令 `{other}`（--help 查看用法）"),
    }
}

fn cmd_serve(args: &[String]) -> Result<()> {
    let cfg = Config::load(args)?;
    history::History::purge(&cfg);

    let app = Arc::new(App::new(cfg.clone()));

    // 冷启动预热：先读一次，避免首个请求拿到空值
    app.warmup();

    let ticker = Arc::clone(&app);
    let tick_s = cfg.tick_s.max(5);
    std::thread::Builder::new()
        .name("hidread-tick".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(tick_s));
            ticker.tick();
        })?;

    http::serve(app)
}

fn cmd_once(args: &[String]) -> Result<()> {
    let cfg = Config::load(args)?;
    let (iface, mut dev) = open_target(&cfg)?;
    let started = std::time::Instant::now();
    let raw = dev.exchange(
        cfg.retries,
        Duration::from_millis(cfg.read_timeout_ms),
        Duration::from_millis(cfg.read_deadline_ms),
    )?;
    let ms = started.elapsed().as_millis();
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
                    Ok(raw) => {
                        let parsed = atk::parse(&raw);
                        println!(
                            "    raw = {}\n    → {:?}{}",
                            atk::hex(&raw),
                            parsed,
                            if parsed.is_none() { "（校验未通过）" } else { "" }
                        );
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
    let points = history::History::load_range(
        &cfg,
        now - chrono::Duration::days(days),
        now,
    );

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
