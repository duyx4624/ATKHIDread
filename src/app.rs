//! 应用状态与节拍线程：采集 → 状态机 → 落盘。
//!
//! HTTP 请求只读 `inner` 快照（微秒级），永远不碰 HID；
//! 设备句柄被单独的锁保护，只有节拍线程持有。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration as CDur, Local};
use serde_json::{json, Value};

use crate::atk;
use crate::config::{Config, WIRED_PID};
use crate::cycle::CycleStore;
use crate::hid;
use crate::history::{History, Point};
use crate::session::SessionStore;
use crate::util;

pub struct Snapshot {
    pub level: Option<u8>,
    pub wired: bool,
    pub pid: u16,
    pub product: String,
    pub raw: Option<Vec<u8>>,
    pub read_ms: u64,
    pub updated_at: Option<DateTime<Local>>,
    pub error: Option<String>,
    pub present: bool,
}

pub struct Link {
    pub online: bool,
    pub fails: u32,
    pub last_seen: Option<DateTime<Local>>,
    pub offline_since: Option<DateTime<Local>>,
}

struct Inner {
    snapshot: Snapshot,
    link: Link,
    sessions: SessionStore,
    cycles: CycleStore,
    history: History,
    last_tick: Option<DateTime<Local>>,
    refreshing: bool,
}

struct Opened {
    iface: hid::HidIface,
    dev: hid::Device,
}

enum TickResult {
    Ok {
        iface: hid::HidIface,
        raw: Vec<u8>,
        ms: u64,
    },
    NoDevice(String),
    ReadError(String),
}

pub struct App {
    pub cfg: Config,
    /// 配置文件里的值（不含 CLI/环境变量覆盖），设置页读写这一份
    pub settings: Config,
    pub started: DateTime<Local>,
    inner: Mutex<Inner>,
    device: Mutex<Option<Opened>>,
}

impl App {
    pub fn new(cfg: Config) -> Self {
        let started = Local::now();
        let settings = Config::load_file_only();
        let mut inner = Inner {
            snapshot: Snapshot {
                level: None,
                wired: false,
                pid: 0,
                product: String::new(),
                raw: None,
                read_ms: 0,
                updated_at: None,
                error: None,
                present: false,
            },
            link: Link {
                online: false,
                fails: 0,
                last_seen: None,
                offline_since: None,
            },
            sessions: SessionStore::load(&cfg),
            cycles: CycleStore::load(&cfg),
            history: History::new(),
            last_tick: None,
            refreshing: false,
        };

        // 重启后立刻能画出 24h 曲线，不用等重新攒点
        let _ = inner.history.preload(&cfg);

        Self {
            cfg,
            settings,
            started,
            inner: Mutex::new(inner),
            device: Mutex::new(None),
        }
    }

    /// 冷启动预热：在开始监听之前跑一次，保证首个请求就有值。
    pub fn warmup(&self) {
        self.tick();
    }

    /// 一个节拍：读设备 → 更新状态 → 按需落盘。
    pub fn tick(&self) {
        let now = Local::now();

        // 1. 系统挂起检测：PC 睡过一觉，不能用这段空档判定鼠标离线
        {
            let mut g = self.inner.lock().unwrap();
            if let Some(prev) = g.last_tick {
                let gap = (now - prev).num_seconds();
                if gap > 3 * self.cfg.tick_s as i64 {
                    self.event("host_resume", &format!("gap_s={gap}"));
                    if g.link.online {
                        let _ = g.sessions.close(&self.cfg, prev, "host_suspend");
                    }
                    g.link.online = false;
                    g.link.fails = 0;
                    g.link.offline_since = None;
                }
            }
            g.last_tick = Some(now);
        }

        // 2. 读设备（不持 inner 锁，最长约 2.4s）
        let result = self.read_once();

        // 3. 应用结果
        let mut g = self.inner.lock().unwrap();
        let was_online = g.link.online;

        match result {
            TickResult::Ok { iface, raw, ms } => {
                let info = match atk::parse(&raw) {
                    Some(v) => v,
                    None => {
                        g.snapshot.error = Some(format!("帧校验失败：{}", atk::hex(&raw)));
                        drop(g);
                        self.mark_failure(now);
                        return;
                    }
                };

                g.snapshot.level = Some(info.level);
                g.snapshot.wired = info.wired;
                g.snapshot.pid = iface.pid;
                g.snapshot.product = iface.product.clone();
                g.snapshot.raw = Some(raw);
                g.snapshot.read_ms = ms;
                g.snapshot.updated_at = Some(now);
                g.snapshot.error = None;
                g.snapshot.present = true;

                // 开始时间同样有盲区：离线状态下节拍是 tick_s，真实上线可能发生在
                // 上一次采样之后、本次采样之前。从离线恢复时把开始时间前移半拍，
                // 但不能早于上次判定离线的时刻，否则会话会重叠。
                let half = chrono::Duration::milliseconds((self.cfg.tick_s.max(1) * 500) as i64);
                let start_at = match (was_online, g.link.offline_since) {
                    (false, Some(t)) => (now - half).max(t),
                    _ => now, // 首次启动没有"上一次采样"可参考，不凭空补
                };

                g.link.online = true;
                g.link.fails = 0;
                g.link.last_seen = Some(now);
                g.link.offline_since = None;

                if !was_online {
                    self.event("online", &format!("{:04X}:{:04X}", iface.vid, iface.pid));
                }

                if let Err(e) = g.sessions.ensure_open(&self.cfg, start_at) {
                    eprintln!("会话写入失败：{e:#}");
                }
                if let Err(e) = g.cycles.on_sample(&self.cfg, now, info.level, info.wired) {
                    eprintln!("周期写入失败：{e:#}");
                }

                let point = Point {
                    at: now,
                    level: Some(info.level),
                    wired: info.wired,
                    online: true,
                    read_ms: ms as u32,
                };
                if let Err(e) = g.history.record(&self.cfg, point, !was_online) {
                    eprintln!("历史写入失败：{e:#}");
                }
            }
            TickResult::NoDevice(msg) => {
                g.snapshot.present = false;
                g.snapshot.error = Some(msg.clone());
                drop(g);
                self.mark_failure(now);
            }
            TickResult::ReadError(msg) => {
                g.snapshot.present = true;
                g.snapshot.error = Some(msg.clone());
                drop(g);
                self.mark_failure(now);
            }
        }
    }

    /// 快速探测节拍：休眠档位的 1/3，夹在 10 秒与常规节拍之间。
    ///
    /// 平时按 `tick_s` 慢采（省电、够用），一旦读不到数据就提速确认，
    /// 这样"判定延迟"不会因为节拍大而虚长。
    pub fn probe_s(&self) -> u64 {
        // 注意 clamp 的上下界必须合法：tick 很小时 10 秒下限会大于上限而 panic
        let cap = self.cfg.tick_s.max(5);
        (self.cfg.sleep_s / 3).clamp(10.min(cap), cap)
    }

    /// 下一次采样前要等多久。
    pub fn next_interval_s(&self) -> u64 {
        let g = self.inner.lock().unwrap();
        if g.link.online && g.link.fails > 0 {
            self.probe_s()
        } else {
            self.cfg.tick_s.max(5)
        }
    }

    fn mark_failure(&self, now: DateTime<Local>) {
        let mut g = self.inner.lock().unwrap();
        g.link.fails += 1;
        // 静默从"最后一次成功读取"起算——那时鼠标已经休眠（休眠期间一直在上报），
        // 所以休眠档位已经隐含在 last_seen 里，这里只需追加确认时间，
        // 再叠加 sleep_s 就等于把休眠算了两遍。
        let probe = self.probe_s();
        let limit = (probe * 2).max(30) as i64;
        let silent = g
            .link
            .last_seen
            .map(|t| (now - t).num_seconds())
            .unwrap_or(0);
        let threshold = self.cfg.offline_after_fails.max(2);

        if g.link.online && silent >= limit && g.link.fails >= threshold {
            g.link.online = false;
            g.link.offline_since = Some(now);
            // 盲区补偿：两次采样之间发生了什么无法知道，取半拍作为期望中点——
            // 有时多记半拍、有时少记半拍，期望误差为零，而不是固定少记半拍
            let half = chrono::Duration::milliseconds((self.cfg.tick_s.max(1) * 500) as i64);
            let end = g
                .link
                .last_seen
                .map(|t| (t + half).min(now))
                .unwrap_or(now);
            self.event("offline", &format!("fails={} silent={}s", g.link.fails, silent));
            if let Err(e) = g.sessions.close(&self.cfg, end, "no_reply") {
                eprintln!("会话收尾失败：{e:#}");
            }
            let point = Point {
                at: now,
                level: None,
                wired: g.snapshot.wired,
                online: false,
                read_ms: 0,
            };
            if let Err(e) = g.history.record(&self.cfg, point, true) {
                eprintln!("历史写入失败：{e:#}");
            }
        }

        if let Err(e) = g.cycles.on_offline(&self.cfg, now) {
            eprintln!("周期收尾失败：{e:#}");
        }
    }

    fn read_once(&self) -> TickResult {
        let mut slot = self.device.lock().unwrap();

        if slot.is_none() {
            match open_target(&self.cfg) {
                Ok(opened) => *slot = Some(opened),
                Err(e) => return TickResult::NoDevice(format!("{e:#}")),
            }
        }

        let opened = slot.as_mut().unwrap();
        let start = Instant::now();
        let res = opened.dev.exchange(
            self.cfg.retries,
            Duration::from_millis(self.cfg.read_timeout_ms),
            Duration::from_millis(self.cfg.read_deadline_ms),
        );
        let ms = start.elapsed().as_millis() as u64;

        match res {
            Ok(raw) => TickResult::Ok {
                iface: opened.iface.clone(),
                raw,
                ms,
            },
            Err(e) => {
                // 句柄可能已失效（拔线/换模式），下次重新枚举
                *slot = None;
                TickResult::ReadError(format!("{e:#}"))
            }
        }
    }

    fn event(&self, kind: &str, detail: &str) {
        let now = Local::now();
        let line = format!(
            "{},{},{},{}",
            util::rfc3339(now),
            util::unix(now),
            kind,
            detail
        );
        let path = self.cfg.events_path(&now.format("%Y-%m-%d").to_string());
        if let Err(e) = util::append_line(&path, &line) {
            eprintln!("事件写入失败：{e:#}");
        }
    }

    pub fn battery_json(&self) -> (u16, Value) {
        let now = Local::now();
        let g = self.inner.lock().unwrap();
        let s = &g.snapshot;

        let age = s
            .updated_at
            .map(|t| (now - t).num_milliseconds() as f64 / 1000.0)
            .unwrap_or(-1.0);
        let age_s: f64 = if age < 0.0 { 0.0 } else { age };
        // 陈旧判定跟着节拍走：允许漏一拍，两拍没更新就算陈旧
        let ttl = (self.cfg.tick_s.max(5) * 2).max(10) as f64;
        let stale = s.updated_at.is_none() || age_s > ttl;
        let ok = s.level.is_some();

        let status = if s.wired {
            "wired"
        } else if g.link.online {
            "discharging"
        } else {
            "unknown"
        };

        let mode = if s.pid == WIRED_PID {
            "wired"
        } else if s.pid == 0 {
            "unknown"
        } else {
            "wireless"
        };

        let uptime = g
            .sessions
            .current()
            .map(|x| (now - x.wake_at).num_seconds().max(0));

        let value = json!({
            "ok": ok,
            "stale": stale,
            "age_s": (age_s * 10.0).round() / 10.0,
            "present": s.present,
            "device": {
                "name": s.product,
                "vid": format!("{:04X}", self.cfg.vid),
                "pid": if s.pid == 0 { Value::Null } else { json!(format!("{:04X}", s.pid)) },
                "mode": mode,
                "interface": format!("0x{:04X}/{}B", self.cfg.usage_page, atk::FRAME_LEN),
            },
            "link": {
                "state": if g.link.online { "online" } else { "offline" },
                "session_start_at": g.sessions.current().map(|x| util::rfc3339(x.wake_at)),
                "session_uptime_s": uptime,
                "session_uptime_text": uptime.map(util::dur_text),
                "offline_since": g.link.offline_since.map(util::rfc3339),
                "consecutive_fails": g.link.fails,
            },
            "battery": {
                "level": s.level,
                "level_text": s.level.map(|v| v.to_string()).unwrap_or_else(|| "--".into()),
                "status": status,
                "wired": s.wired,
            },
            "cycle": g.cycles.current_json(now, s.level),
            "source": "atk1",
            "raw": s.raw.as_ref().map(|r| atk::hex(r)),
            "read_ms": s.read_ms,
            "updated_at": s.updated_at.map(util::rfc3339),
            "error": s.error,
        });

        (if ok { 200 } else { 503 }, value)
    }

    pub fn health_json(&self) -> Value {
        let now = Local::now();
        let g = self.inner.lock().unwrap();
        json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_s": (now - self.started).num_seconds().max(0),
            "present": g.snapshot.present,
            "last_age_s": g.snapshot.updated_at.map(|t| (now - t).num_seconds()),
            "refreshing": g.refreshing,
            "last_error": g.snapshot.error,
            "device": format!("{:04X}", self.cfg.vid),
        })
    }

    pub fn raw_json(&self) -> Value {
        let g = self.inner.lock().unwrap();
        json!({
            "request": atk::hex(&atk::request()),
            "raw": g.snapshot.raw.as_ref().map(|r| atk::hex(r)),
            "read_ms": g.snapshot.read_ms,
            "at": g.snapshot.updated_at.map(util::rfc3339),
        })
    }

    pub fn session_json(&self) -> Value {
        let now = Local::now();
        let g = self.inner.lock().unwrap();
        let online_s: i64 = g
            .sessions
            .for_day(&self.cfg, &now.format("%Y-%m-%d").to_string(), now)
            .iter()
            .map(|s| s.duration_s(now))
            .sum();

        json!({
            "state": if g.link.online { "online" } else { "offline" },
            "session_start_at": g.sessions.current().map(|x| util::rfc3339(x.wake_at)),
            "session_uptime_s": g.sessions.current().map(|x| (now - x.wake_at).num_seconds()),
            "session_uptime_text": g.sessions.current().map(|x| util::dur_text((now - x.wake_at).num_seconds())),
            "offline_since": g.link.offline_since.map(util::rfc3339),
            "consecutive_fails": g.link.fails,
            "today": { "online_s": online_s, "online_text": util::dur_text(online_s) },
        })
    }

    pub fn sessions_json(&self, day: &str) -> Value {
        let now = Local::now();
        let g = self.inner.lock().unwrap();
        let list = g.sessions.for_day(&self.cfg, day, now);
        let total: i64 = list.iter().map(|s| s.duration_s(now)).sum();
        json!({
            "day": day,
            "count": list.len(),
            "online_s": total,
            "online_text": util::dur_text(total),
            "sessions": list.iter().map(|s| s.to_json(now)).collect::<Vec<_>>(),
        })
    }

    pub fn daily_json(&self, days: i64) -> Value {
        let g = self.inner.lock().unwrap();
        json!({ "days": g.sessions.daily(&self.cfg, days) })
    }

    /// 周视图：7 天的会话区间 + 自学习出来的时间轴范围。
    pub fn week_json(&self, start: Option<&str>, days: i64) -> Value {
        let now = Local::now();
        let today = now.date_naive();
        let days = days.clamp(1, 60);

        let start_date = start
            .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
            .unwrap_or_else(|| {
                if days <= 7 {
                    // 短跨度默认本周一
                    let wd = chrono::Datelike::weekday(&today).num_days_from_monday() as i64;
                    today - CDur::days(wd)
                } else {
                    // 长跨度默认滚动到今天
                    today - CDur::days(days - 1)
                }
            });

        let g = self.inner.lock().unwrap();
        let (from_h, to_h) = g.sessions.active_range(&self.cfg, 30);

        // 业务日：每天从 04:00 到次日 03:59
        let day0 = start_date
            .and_hms_opt(4, 0, 0)
            .and_then(|d| d.and_local_timezone(Local).single())
            .map(|d| d.timestamp())
            .unwrap_or(0);
        // 画图用合并区间（视觉连续），统计用原始区间（不把休眠算进时长）
        let segs = g
            .sessions
            .intervals_between(&self.cfg, day0, day0 + days * 86_400, now, true);
        let raw = g
            .sessions
            .intervals_between(&self.cfg, day0, day0 + days * 86_400, now, false);

        let mut days_out = Vec::new();
        let mut total = 0i64;
        for i in 0..days {
            let d0 = day0 + i * 86_400;
            let d1 = d0 + 86_400;
            let mut items: Vec<Value> = Vec::new();
            for (a, b) in &segs {
                let x = (*a).max(d0);
                let y = (*b).min(d1);
                if y > x {
                    items.push(json!([x, y]));
                }
            }
            let mut day_total = 0i64;
            for (a, b) in &raw {
                let x = (*a).max(d0);
                let y = (*b).min(d1);
                if y > x {
                    day_total += y - x;
                }
            }
            total += day_total;
            days_out.push(json!({
                "date": (start_date + CDur::days(i)).format("%Y-%m-%d").to_string(),
                "segments": items,
                "total_s": day_total,
            }));
        }

        json!({
            "start": start_date.format("%Y-%m-%d").to_string(),
            "end": (start_date + CDur::days(days - 1)).format("%Y-%m-%d").to_string(),
            "today": today.format("%Y-%m-%d").to_string(),
            "range": {"from": from_h, "to": to_h},
            "days": days_out,
            "total_s": total,
        })
    }

    pub fn usage_json(&self, days: i64) -> Value {
        let g = self.inner.lock().unwrap();
        json!({ "days": g.sessions.usage(&self.cfg, days) })
    }

    pub fn history_json(&self, hours: i64, max_points: usize) -> Value {
        let now = Local::now();
        let from = now - CDur::hours(hours);
        let g = self.inner.lock().unwrap();

        let ring: Vec<Point> = g
            .history
            .ring()
            .iter()
            .filter(|p| p.at >= from)
            .cloned()
            .collect();
        // 磁盘是按天的真源，内存环只补"尚未落盘的最新点"。
        // 之前优先用环，预热不完整时曲线只剩几个点（整张图空白）。
        drop(g);
        let mut points = History::load_range(&self.cfg, from, now);
        match points.last() {
            Some(last) => {
                let last_at = last.at;
                points.extend(ring.into_iter().filter(|p| p.at > last_at));
            }
            None => points = ring,
        }

        let points = History::downsample(points, max_points);
        let levels: Vec<u8> = points.iter().filter_map(|p| p.level).collect();
        json!({
            "from": util::rfc3339(from),
            "to": util::rfc3339(now),
            "count": points.len(),
            "points": points.iter().map(|p| json!({
                "t": util::unix(p.at),
                "at": util::rfc3339(p.at),
                "level": p.level,
                "wired": if p.wired { 1 } else { 0 },
                "link": if p.online { "online" } else { "offline" },
            })).collect::<Vec<_>>(),
            "stats": {
                "min": levels.iter().min(),
                "max": levels.iter().max(),
                "first": levels.first(),
                "last": levels.last(),
            },
        })
    }

    pub fn cycles_json(&self, limit: usize) -> Value {
        let now = Local::now();
        let g = self.inner.lock().unwrap();
        json!({
            "current": g.cycles.current_json(now, g.snapshot.level),
            "history": CycleStore::history(&self.cfg, limit),
            "stats": CycleStore::stats(&self.cfg),
        })
    }
}

fn open_target(cfg: &Config) -> Result<Opened> {
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
        return Err(anyhow!(
            "未找到电量接口（VID={:04X}, UsagePage=0x{:04X}）",
            cfg.vid,
            cfg.usage_page
        ));
    }

    cands.sort_by_key(|i| if i.pid == WIRED_PID { 0 } else { 1 });
    let iface = cands.remove(0);
    let dev = hid::Device::open(&iface)?;
    Ok(Opened { iface, dev })
}
