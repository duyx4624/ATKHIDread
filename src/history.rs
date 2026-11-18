//! 历史采样：内存环形缓冲（24h）+ 按天 CSV 落盘。

use std::collections::VecDeque;

use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveDate};

use crate::config::Config;
use crate::util;

/// 一个采样点。
#[derive(Debug, Clone)]
pub struct Point {
    pub at: DateTime<Local>,
    pub level: Option<u8>,
    pub wired: bool,
    pub online: bool,
    pub read_ms: u32,
}

pub struct History {
    ring: VecDeque<Point>,
    last_level: Option<u8>,
    last_online: Option<bool>,
    last_write: Option<DateTime<Local>>,
}

impl History {
    pub fn new() -> Self {
        Self {
            ring: VecDeque::with_capacity(1500),
            last_level: None,
            last_online: None,
            last_write: None,
        }
    }

    /// 决定是否落盘：值变化、连接状态变化、或距上次基线超过 `history_interval_s`。
    pub fn record(&mut self, cfg: &Config, p: Point, force: bool) -> Result<bool> {
        let changed = self.last_level != p.level;
        let link_changed = self.last_online != Some(p.online);
        let baseline_due = self
            .last_write
            .map(|t| (p.at - t).num_seconds() >= cfg.history_interval_s as i64)
            .unwrap_or(true);

        let should_write = force || changed || link_changed || (baseline_due && p.online);
        if !should_write {
            return Ok(false);
        }

        if !p.online && !link_changed && !force {
            return Ok(false);
        }

        let date = p.at.format("%Y-%m-%d").to_string();
        let line = format!(
            "{},{},{},{},{},{}",
            util::rfc3339(p.at),
            util::unix(p.at),
            p.level.map(|v| v.to_string()).unwrap_or_default(),
            if p.wired { 1 } else { 0 },
            if p.online { "online" } else { "offline" },
            p.read_ms
        );
        util::append_line(&cfg.history_path(&date), &line)?;

        self.last_level = p.level;
        self.last_online = Some(p.online);
        self.last_write = Some(p.at);

        self.ring.push_back(p);
        while self.ring.len() > 1500 {
            self.ring.pop_front();
        }
        Ok(true)
    }

    /// 启动时把最近 24h 的磁盘数据灌回内存环形缓冲。
    ///
    /// 否则进程一重启，`/history?hours<=24` 就读内存、返回空，
    /// 曲线要等重新攒够采样点才出现。
    pub fn preload(&mut self, cfg: &Config) -> Result<()> {
        let now = Local::now();
        let from = now - ChronoDuration::hours(24);
        let points = Self::load_range(cfg, from, now);

        if let Some(last) = points.last() {
            self.last_level = last.level;
            self.last_online = Some(last.online);
            self.last_write = Some(last.at);
        }
        for p in points {
            self.ring.push_back(p);
        }
        while self.ring.len() > 1500 {
            self.ring.pop_front();
        }
        Ok(())
    }

    /// 最近 24h 的内存数据。
    pub fn ring(&self) -> &VecDeque<Point> {
        &self.ring
    }

    /// 从磁盘读指定区间（用于超过 24h 的查询）。
    pub fn load_range(
        cfg: &Config,
        from: DateTime<Local>,
        to: DateTime<Local>,
    ) -> Vec<Point> {
        let mut out = Vec::new();
        let mut day: NaiveDate = from.date_naive();
        let last = to.date_naive();

        while day <= last {
            let path = cfg.history_path(&day.format("%Y-%m-%d").to_string());
            for row in util::read_rows(&path) {
                if row.len() < 6 {
                    continue;
                }
                let Ok(ts) = row[1].parse::<i64>() else {
                    continue;
                };
                if ts < from.timestamp() || ts > to.timestamp() {
                    continue;
                }
                let Some(at) = DateTime::from_timestamp(ts, 0).map(|t| t.with_timezone(&Local))
                else {
                    continue;
                };
                out.push(Point {
                    at,
                    level: row[2].parse::<u8>().ok(),
                    wired: row[3] == "1",
                    online: row[4] == "online",
                    read_ms: row[5].parse::<u32>().unwrap_or(0),
                });
            }
            day = match day.succ_opt() {
                Some(d) => d,
                None => break,
            };
        }

        out.sort_by_key(|p| p.at);
        out
    }

    /// 清理超过保留期的按天文件。
    pub fn purge(cfg: &Config) {
        let keep_from = Local::now().date_naive() - ChronoDuration::days(cfg.history_days);
        let Ok(entries) = std::fs::read_dir(&cfg.data_dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(rest) = name
                .strip_prefix("history-")
                .or_else(|| name.strip_prefix("events-"))
            else {
                continue;
            };
            let Some(date_str) = rest.strip_suffix(".csv") else {
                continue;
            };
            if let Ok(date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                if date < keep_from {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}
