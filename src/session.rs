//! 会话（在线窗口）记录与每日聚合。
//!
//! 会话终点取「最后一次成功应答」的时刻，而不是「判定离线」的时刻：
//! 判离线需要连续 N 次失败，若用失败时刻收尾，每段会凭空多出 1–2 分钟。

use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Local, Timelike};
use serde_json::{json, Value};

use crate::config::Config;
use crate::util;

#[derive(Debug, Clone)]
pub struct Session {
    pub id: u64,
    pub wake_at: DateTime<Local>,
    pub sleep_at: Option<DateTime<Local>>,
    pub end_reason: String,
}

impl Session {
    pub fn duration_s(&self, now: DateTime<Local>) -> i64 {
        (self.sleep_at.unwrap_or(now) - self.wake_at).num_seconds().max(0)
    }

    pub fn to_json(&self, now: DateTime<Local>) -> Value {
        let d = self.duration_s(now);
        json!({
            "wake_at": util::rfc3339(self.wake_at),
            "sleep_at": self.sleep_at.map(util::rfc3339),
            "duration_s": d,
            "duration_text": util::dur_text(d),
            "end_reason": self.end_reason,
        })
    }
}

#[derive(Clone)]
pub struct SessionStore {
    open: Option<Session>,
    next_id: u64,
}

impl SessionStore {
    /// 从状态文件恢复未结束的会话。
    ///
    /// 若上次观测距今超过 3 个节拍（最小 180 s），说明服务/机器停过，
    /// 这段空档不能算进"在线"——按最后观测时间收尾为 `service_stop`。
    pub fn load(cfg: &Config) -> Self {
        let mut store = Self {
            open: None,
            // 状态文件丢了也不能和 sessions.csv 里已有的 id 重号
            next_id: Self::max_id(cfg) + 1,
        };
        let Ok(text) = std::fs::read_to_string(cfg.session_state_path()) else {
            return store;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return store;
        };

        let Some(wake_unix) = v.get("wake_unix").and_then(Value::as_i64) else {
            return store;
        };
        let last_seen = v
            .get("last_seen_unix")
            .and_then(Value::as_i64)
            .unwrap_or(wake_unix);
        let id = v.get("id").and_then(Value::as_u64).unwrap_or(1);

        let Some(wake_at) = DateTime::from_timestamp(wake_unix, 0).map(|t| t.with_timezone(&Local))
        else {
            return store;
        };
        let Some(last_at) = DateTime::from_timestamp(last_seen, 0).map(|t| t.with_timezone(&Local))
        else {
            return store;
        };

        let stale_after = (cfg.tick_s as i64 * 3).max(180);
        if (Local::now() - last_at).num_seconds() > stale_after {
            let duration = (last_at - wake_at).num_seconds().max(0);
            let line = format!(
                "{},{},{},{},{},{},{}",
                id,
                util::rfc3339(wake_at),
                util::unix(wake_at),
                util::rfc3339(last_at),
                util::unix(last_at),
                duration,
                "service_stop"
            );
            let _ = util::append_line(&cfg.sessions_path(), &line);
            let _ = std::fs::remove_file(cfg.session_state_path());
            store.next_id = store.next_id.max(id + 1);
            return store;
        }

        store.open = Some(Session {
            id,
            wake_at,
            sleep_at: None,
            end_reason: "ongoing".into(),
        });
        store.next_id = store.next_id.max(id + 1);
        store
    }

    fn max_id(cfg: &Config) -> u64 {
        util::read_rows(&cfg.sessions_path())
            .iter()
            .filter(|r| !r.is_empty() && r[0] != "session_id")
            .filter_map(|r| r[0].parse::<u64>().ok())
            .max()
            .unwrap_or(0)
    }

    /// 已结束的会话（按文件顺序，即开始时间顺序）。
    fn closed(cfg: &Config) -> Vec<Session> {
        util::read_rows(&cfg.sessions_path())
            .into_iter()
            .filter(|r| r.len() >= 7 && r[0] != "session_id")
            .filter_map(|r| {
                let wake_unix = r[2].parse::<i64>().ok()?;
                let end_unix = r[4].parse::<i64>().ok()?;
                Some(Session {
                    id: r[0].parse().unwrap_or(0),
                    wake_at: DateTime::from_timestamp(wake_unix, 0)?.with_timezone(&Local),
                    sleep_at: Some(DateTime::from_timestamp(end_unix, 0)?.with_timezone(&Local)),
                    end_reason: r[6].clone(),
                })
            })
            .collect()
    }

    /// 全部会话（含进行中的那条）。
    fn all(&self, cfg: &Config) -> Vec<Session> {
        let mut v = Self::closed(cfg);
        if let Some(s) = &self.open {
            v.push(s.clone());
        }
        v
    }

    pub fn current(&self) -> Option<&Session> {
        self.open.as_ref()
    }

    pub fn ensure_open(&mut self, cfg: &Config, at: DateTime<Local>) -> Result<()> {
        if self.open.is_some() {
            return self.persist(cfg, at);
        }
        let s = Session {
            id: self.next_id,
            wake_at: at,
            sleep_at: None,
            end_reason: "ongoing".into(),
        };
        self.next_id += 1;
        self.open = Some(s);
        self.persist(cfg, at)
    }

    /// 落盘当前会话状态；`last_seen_unix` 用于下次启动判断这段空档是否该计入在线。
    fn persist(&self, cfg: &Config, at: DateTime<Local>) -> Result<()> {
        let Some(s) = &self.open else {
            return Ok(());
        };
        util::write_json_atomic(
            &cfg.session_state_path(),
            &json!({
                "id": s.id,
                "wake_at": util::rfc3339(s.wake_at),
                "wake_unix": util::unix(s.wake_at),
                "last_seen_unix": util::unix(at),
            }),
        )
    }

    pub fn close(&mut self, cfg: &Config, at: DateTime<Local>, reason: &str) -> Result<()> {
        let Some(s) = self.open.take() else {
            return Ok(());
        };
        let end = at.max(s.wake_at);
        let duration = (end - s.wake_at).num_seconds().max(0);
        let line = format!(
            "{},{},{},{},{},{},{}",
            s.id,
            util::rfc3339(s.wake_at),
            util::unix(s.wake_at),
            util::rfc3339(end),
            util::unix(end),
            duration,
            reason
        );
        util::append_line(&cfg.sessions_path(), &line)?;
        let _ = std::fs::remove_file(cfg.session_state_path());
        Ok(())
    }

    /// 某一天内开始的会话（含尚未结束的那条）。
    pub fn for_day(&self, cfg: &Config, date: &str) -> Vec<Session> {
        let Ok(day) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") else {
            return Vec::new();
        };
        let mut out: Vec<Session> = Self::closed(cfg)
            .into_iter()
            .filter(|s| s.wake_at.date_naive() == day)
            .collect();

        if let Some(s) = &self.open {
            if s.wake_at.date_naive() == day {
                let mut s = s.clone();
                s.end_reason = "ongoing".into();
                out.push(s);
            }
        }

        out.sort_by_key(|s| s.wake_at);
        out
    }

    /// 从近 `days` 天的使用分布里推断"活跃小时带"：返回 `(from, to)`。
    ///
    /// 口径是**业务日**：每天从 04:00 到次日 03:59，凌晨 0–4 点算前一天的尾巴。
    /// 因此返回值是"相对 04:00 的小时偏移"（0–24），而不是自然小时。
    pub fn active_range(&self, cfg: &Config, days: i64) -> (u8, u8) {
        let now = Local::now();
        let days = days.max(1) as usize;
        let first_day = now.date_naive() - ChronoDuration::days(days as i64 - 1);
        let Some(start) = first_day
            .and_hms_opt(4, 0, 0)
            .and_then(|d| d.and_local_timezone(Local).single())
        else {
            return (0, 24);
        };
        let start_ts = start.timestamp();

        // 每个业务日 × 每个小时，是否有过在线
        let mut hit = vec![[false; 24]; days];
        for (a, b) in self.intervals_between(cfg, start_ts, now.timestamp(), now, true) {
            let mut t = a;
            while t < b {
                let off = t - start_ts;
                let d = (off / 86_400) as usize;
                let h = ((off % 86_400) / 3_600) as usize;
                if d < days {
                    hit[d][h] = true;
                }
                t = start_ts + (off / 3_600 + 1) * 3_600;
            }
        }

        let mut counts = [0usize; 24];
        for day in &hit {
            for (h, on) in day.iter().enumerate() {
                if *on {
                    counts[h] += 1;
                }
            }
        }

        // 阈值基数用"实际有记录的天数"：刚装几天时 30 天的 10% 永远达不到，
        // 会导致自适应永远退回 0–24
        let active_days = hit.iter().filter(|d| d.iter().any(|x| *x)).count().max(1);
        let threshold = (((active_days as f64) * 0.1).ceil() as usize)
            .max(1)
            .min(active_days);
        let mut first: Option<usize> = None;
        let mut last = 0usize;
        for (h, count) in counts.iter().enumerate() {
            if *count >= threshold {
                if first.is_none() {
                    first = Some(h);
                }
                last = h;
            }
        }

        let Some(f) = first else { return (0, 24) };

        // 两端各留余量；若活跃只到 24:00 之前，就不必把次日凌晨也画进来
        let mut from = f.saturating_sub(1);
        let mut to = if last <= 20 {
            (last + 2).min(20)
        } else {
            (last + 2).min(24)
        };
        while to - from < 8 {
            if to < 24 {
                to += 1;
            } else if from > 0 {
                from -= 1;
            } else {
                break;
            }
        }
        (from as u8, to as u8)
    }

    /// 取 `[from, to)` 内所有会话区间，并裁剪到该窗口（跨天会话会被截断）。
    pub fn intervals_between(
        &self,
        cfg: &Config,
        from: i64,
        to: i64,
        now: DateTime<Local>,
        merge: bool,
    ) -> Vec<(i64, i64)> {
        let mut out: Vec<(i64, i64)> = util::read_rows(&cfg.sessions_path())
            .into_iter()
            .filter(|r| r.len() >= 7 && r[0] != "session_id")
            .filter_map(|r| {
                let s = r[2].parse::<i64>().ok()?;
                let e = r[4].parse::<i64>().ok()?;
                if e <= from || s >= to {
                    None
                } else {
                    Some((s.max(from), e.min(to)))
                }
            })
            .collect();

        if let Some(s) = &self.open {
            let a = s.wake_at.timestamp();
            let b = now.timestamp();
            if b > from && a < to {
                out.push((a.max(from), b.min(to)));
            }
        }

        out.sort();
        if !merge {
            return out;
        }

        // 合并只用于"画图好看"；统计时长要用未合并的原始区间，
        // 否则鼠标休眠的那几分钟会被算进使用时间
        let merge_gap = (cfg.sleep_s as i64 * 3).max(300);
        let mut merged: Vec<(i64, i64)> = Vec::new();
        for (a, b) in out {
            if let Some(last) = merged.last_mut() {
                if a - last.1 <= merge_gap {
                    last.1 = last.1.max(b);
                    continue;
                }
            }
            merged.push((a, b));
        }
        merged
    }

    /// 最近 `days` 天，每小时在线多少分钟（0–60）。用于"课程表"热力图。
    ///
    /// 按会话区间与小时窗口求交集，而不是数采样点——采样点受节拍与休眠影响，
    /// 会系统性低估真实在线分钟数。
    pub fn usage(&self, cfg: &Config, days: i64) -> Vec<Value> {
        let now = Local::now();
        let today = now.date_naive();

        // 走合并后的区间：休眠造成的小间隙不该被算成两次使用
        let win_start = (today - ChronoDuration::days(days.max(1) - 1))
            .and_hms_opt(0, 0, 0)
            .and_then(|d| d.and_local_timezone(Local).single())
            .map(|d| d.timestamp())
            .unwrap_or(0);
        let spans = self.intervals_between(cfg, win_start, now.timestamp(), now, true);

        let mut out = Vec::new();
        for offset in (0..days.max(1)).rev() {
            let day = today - ChronoDuration::days(offset);
            let Some(start) = day
                .and_hms_opt(4, 0, 0)
                .and_then(|d| d.and_local_timezone(Local).single())
            else {
                continue;
            };
            let d0 = start.timestamp();
            let d1 = d0 + 86_400;
            let mut hours = [0i64; 24];

            for (s, e) in &spans {
                let a = (*s).max(d0);
                let b = (*e).min(d1);
                if b <= a {
                    continue;
                }
                for (h, slot) in hours.iter_mut().enumerate() {
                    let hs = d0 + h as i64 * 3600;
                    let x = a.max(hs);
                    let y = b.min(hs + 3600);
                    if y > x {
                        *slot += (y - x) / 60;
                    }
                }
            }

            let total_s = hours.iter().sum::<i64>() * 60;
            out.push(json!({
                "date": day.format("%Y-%m-%d").to_string(),
                "hours": hours.to_vec(),
                "total_s": total_s,
            }));
        }

        out
    }

    /// 最近 `days` 天的每日聚合（含今天）。
    ///
    /// 时长口径与 `/week` 一致：按天裁剪，跨午夜的会话不会整段压在前一天。
    /// `sessions` 计数仍按"开始日"归属，所以它与时长不是同一个分母。
    pub fn daily(&self, cfg: &Config, days: i64) -> Vec<Value> {
        self.daily_until(cfg, days, Local::now())
    }

    /// `now` 显式传入，便于测试固定"今天"。`daily` 只是它的当前时间版本。
    fn daily_until(&self, cfg: &Config, days: i64, now: DateTime<Local>) -> Vec<Value> {
        // 业务日：04:00 到次日 03:59，与 /week 保持同一口径
        let today = if now.hour() < 4 {
            now.date_naive() - ChronoDuration::days(1)
        } else {
            now.date_naive()
        };
        let mut out = Vec::new();

        // 只读一次 sessions.csv：之前是每天读一遍，90 天视图要读 90 次
        let all = self.all(cfg);
        let spans: Vec<(i64, i64)> = all
            .iter()
            .map(|s| {
                let a = s.wake_at.timestamp();
                (a, s.sleep_at.unwrap_or(now).timestamp().max(a))
            })
            .collect();

        for offset in (0..days.max(1)).rev() {
            let day = today - ChronoDuration::days(offset);
            let date = day.format("%Y-%m-%d").to_string();
            let Some(start) = day
                .and_hms_opt(4, 0, 0)
                .and_then(|d| d.and_local_timezone(Local).single())
            else {
                continue;
            };
            let d0 = start.timestamp();
            let d1 = d0 + 86_400;

            let online: i64 = spans
                .iter()
                .map(|&(a, b)| (b.min(d1) - a.max(d0)).max(0))
                .sum();

            let sessions: Vec<&Session> = all
                .iter()
                .filter(|s| {
                    let t = s.wake_at.timestamp();
                    t >= d0 && t < d1
                })
                .collect();
            let longest = sessions
                .iter()
                .map(|s| s.duration_s(now))
                .max()
                .unwrap_or(0);
            let first_wake = sessions
                .iter()
                .map(|s| s.wake_at)
                .min()
                .map(|t| t.format("%H:%M").to_string());
            let last_sleep = sessions
                .iter()
                .filter_map(|s| s.sleep_at)
                .max()
                .map(|t| t.format("%H:%M").to_string());

            let points = crate::history::History::load_range(
                cfg,
                start,
                day.and_hms_opt(23, 59, 59)
                    .and_then(|d| d.and_local_timezone(Local).single())
                    .unwrap_or(now),
            );

            let levels: Vec<u8> = points.iter().filter_map(|p| p.level).collect();

            // 有线时长按"相邻采样点的时间差"累计，而不是"行数 × tick"：
            // 历史行只在值变化或基线间隔才写，稳定插线时行数远少于 tick 数。
            let mut wired_s = 0i64;
            for w in points.windows(2) {
                if w[0].wired && w[0].online {
                    let step = (w[1].at - w[0].at).num_seconds();
                    wired_s += step.clamp(0, cfg.history_interval_s as i64);
                }
            }
            if let Some(last) = points.last() {
                if last.wired && last.online {
                    wired_s += cfg.tick_s as i64;
                }
            }

            out.push(json!({
                "date": date,
                "sessions": sessions.len(),
                "online_s": online,
                "online_text": util::dur_text(online),
                "offline_s": (86_400 - online).max(0),
                "first_wake": first_wake,
                "last_sleep": last_sleep,
                "longest_s": longest,
                "level": {
                    "first": levels.first(),
                    "last": levels.last(),
                    "min": levels.iter().min(),
                    "max": levels.iter().max(),
                },
                "wired_s": wired_s,
            }));
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str) -> Config {
        let mut c = Config::default();
        c.data_dir =
            std::env::temp_dir().join(format!("hidread-session-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&c.data_dir);
        std::fs::create_dir_all(&c.data_dir).unwrap();
        c
    }

    fn at(day: chrono::NaiveDate, h: u32, m: u32) -> DateTime<Local> {
        day.and_hms_opt(h, m, 0)
            .unwrap()
            .and_local_timezone(Local)
            .single()
            .unwrap()
    }

    #[test]
    fn daily_clips_cross_midnight_sessions() {
        let c = cfg("clip");
        let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        let w = at(day, 23, 0);
        let e = at(day + ChronoDuration::days(1), 2, 0);
        util::append_line(
            &c.sessions_path(),
            &format!(
                "1,{},{},{},{},10800,no_reply",
                util::rfc3339(w),
                util::unix(w),
                util::rfc3339(e),
                util::unix(e)
            ),
        )
        .unwrap();

        let store = SessionStore::load(&c);
        let days = store.daily_until(&c, 2, at(day + ChronoDuration::days(1), 12, 0));
        assert_eq!(days[0]["date"], "2026-11-20");
        assert_eq!(days[0]["online_s"], 10800, "业务日 04:00 起：23:00→次日02:00 全归 11-20");
        assert_eq!(days[1]["online_s"], 0, "11-21 业务日从 04:00 才开始");
        assert_eq!(days[0]["sessions"], 1);
        assert_eq!(days[1]["sessions"], 0, "会话计数仍按开始日归属");
    }

    #[test]
    fn wired_seconds_follow_sample_spacing() {
        let c = cfg("wired");
        let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        // 300s 一个基线点，全部有线在线：2 个间隔 × 300s + 末点 1 个 tick
        for i in 0..3 {
            let t = at(day, 10, 0) + ChronoDuration::seconds(i * 300);
            util::append_line(
                &c.history_path("2026-11-20"),
                &format!("{},{},80,1,online,10", util::rfc3339(t), util::unix(t)),
            )
            .unwrap();
        }
        let store = SessionStore::load(&c);
        let days = store.daily_until(&c, 1, at(day, 23, 0));
        assert_eq!(days[0]["wired_s"], 660, "2×300s + 末点 60s");
    }
}
