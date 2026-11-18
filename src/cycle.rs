//! 放电周期：从「拔线且已确认充满」到「电量 ≤ 阈值」的墙钟时长。
//!
//! 关键防坑：充电时读数虚高（实测插线瞬间 15% → 55%），
//! 所以「充满」不能只看电量，还要看充电持续了多久（`cycle_min_charge_min`）。

use anyhow::Result;
use chrono::{DateTime, Local};
use serde_json::{json, Value};

use crate::config::Config;
use crate::util;

#[derive(Debug, Clone)]
struct Current {
    id: u64,
    start_at: DateTime<Local>,
    start_level: u8,
    quality: String,
    offline_s: i64,
}

pub struct CycleStore {
    current: Option<Current>,
    charge_started: Option<DateTime<Local>>,
    saw_full: bool,
    full_confirmed: bool,
    last_wired: Option<bool>,
    last_online: Option<DateTime<Local>>,
    offline_since: Option<DateTime<Local>>,
    next_id: u64,
}

impl CycleStore {
    pub fn load(cfg: &Config) -> Self {
        let mut store = Self {
            current: None,
            charge_started: None,
            saw_full: false,
            full_confirmed: false,
            last_wired: None,
            last_online: None,
            offline_since: None,
            next_id: 1,
        };

        if let Ok(text) = std::fs::read_to_string(cfg.cycle_state_path()) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(start_unix) = v.get("start_unix").and_then(Value::as_i64) {
                    if let Some(at) =
                        DateTime::from_timestamp(start_unix, 0).map(|t| t.with_timezone(&Local))
                    {
                        store.current = Some(Current {
                            id: v.get("id").and_then(Value::as_u64).unwrap_or(1),
                            start_at: at,
                            start_level: v
                                .get("start_level")
                                .and_then(Value::as_u64)
                                .unwrap_or(0) as u8,
                            quality: v
                                .get("quality")
                                .and_then(Value::as_str)
                                .unwrap_or("charged")
                                .to_string(),
                            offline_s: v.get("offline_s").and_then(Value::as_i64).unwrap_or(0),
                        });
                        store.next_id = v.get("id").and_then(Value::as_u64).unwrap_or(1) + 1;
                    }
                }
            }
        }

        store
    }

    pub fn current_json(&self, now: DateTime<Local>, last_level: Option<u8>) -> Value {
        match &self.current {
            Some(c) => {
                let d = (now - c.start_at).num_seconds().max(0);
                json!({
                    "start_at": util::rfc3339(c.start_at),
                    "elapsed_s": d,
                    "elapsed_text": util::dur_text(d),
                    "start_level": c.start_level,
                    "last_level": last_level,
                    "quality": c.quality,
                    "offline_s": c.offline_s,
                })
            }
            None => Value::Null,
        }
    }

    /// 每个成功采样调用一次。
    pub fn on_sample(
        &mut self,
        cfg: &Config,
        at: DateTime<Local>,
        level: u8,
        wired: bool,
    ) -> Result<()> {
        self.last_online = Some(at);
        if let Some(t) = self.offline_since.take() {
            if let Some(c) = self.current.as_mut() {
                c.offline_s += (at - t).num_seconds().max(0);
            }
        }

        let prev = self.last_wired.replace(wired);
        match (prev, wired) {
            (Some(false), true) => {
                // 插线：开始充电计时；未跑完的周期记 interrupted
                self.charge_started = Some(at);
                self.saw_full = false;
                self.full_confirmed = false;
                if self.current.is_some() {
                    self.close(cfg, at, level, "interrupted", "plugged_in")?;
                }
            }
            (Some(true), false) => {
                // 拔线：判定是否算「充满」
                let (quality, note) = if self.full_confirmed {
                    ("charged", "ok")
                } else if self.saw_full {
                    ("partial", "charge_too_short")
                } else {
                    ("partial", "not_full")
                };
                self.open(cfg, at, level, quality, note)?;
                self.charge_started = None;
                self.saw_full = false;
                self.full_confirmed = false;
            }
            (None, true) => {
                // 服务启动时已经在充电：从现在开始计充电时长
                self.charge_started = Some(at);
            }
            (None, false) if self.current.is_none() && level >= cfg.cycle_full_level => {
                // 服务晚于拔线启动：只能记估计起点
                self.open(cfg, at, level, "charged_estimated", "estimated_start")?;
            }
            _ => {}
        }

        if wired && level >= cfg.cycle_full_level {
            self.saw_full = true;
            if let Some(started) = self.charge_started {
                if (at - started).num_minutes() >= cfg.cycle_min_charge_min as i64 {
                    self.full_confirmed = true;
                }
            }
        }

        if !wired && level <= cfg.cycle_end_level && self.current.is_some() {
            self.close(cfg, at, level, "", "")?;
        }

        Ok(())
    }

    /// 无响应时调用：长时间失联则收尾为 unknown_end（可能被关机收纳）。
    pub fn on_offline(&mut self, cfg: &Config, at: DateTime<Local>) -> Result<()> {
        if self.offline_since.is_none() {
            self.offline_since = Some(at);
        }
        let Some(c) = &self.current else {
            return Ok(());
        };
        let dead_s = cfg.cycle_dead_hours as i64 * 3600;
        if let Some(last) = self.last_online {
            if (at - last).num_seconds() > dead_s {
                self.close(cfg, last, 0, "unknown_end", "no_reply")?;
            }
        } else {
            let _ = c;
        }
        Ok(())
    }

    fn open(
        &mut self,
        cfg: &Config,
        at: DateTime<Local>,
        level: u8,
        quality: &str,
        note: &str,
    ) -> Result<()> {
        let c = Current {
            id: self.next_id,
            start_at: at,
            start_level: level,
            quality: quality.to_string(),
            offline_s: 0,
        };
        self.next_id += 1;
        let _ = note;
        util::write_json_atomic(
            &cfg.cycle_state_path(),
            &json!({
                "id": c.id,
                "start_at": util::rfc3339(c.start_at),
                "start_unix": util::unix(c.start_at),
                "start_level": c.start_level,
                "quality": c.quality,
                "offline_s": c.offline_s,
            }),
        )?;
        self.current = Some(c);
        Ok(())
    }

    fn close(
        &mut self,
        cfg: &Config,
        at: DateTime<Local>,
        end_level: u8,
        quality: &str,
        note: &str,
    ) -> Result<()> {
        let Some(c) = self.current.take() else {
            return Ok(());
        };
        let end = at.max(c.start_at);
        let duration = (end - c.start_at).num_seconds().max(0);
        let quality = if quality.is_empty() {
            c.quality.clone()
        } else {
            quality.to_string()
        };
        let charge_min = self
            .charge_started
            .map(|s| (end - s).num_minutes().max(0))
            .unwrap_or(0);

        let line = format!(
            "{},{},{},{},{},{},{},{},{},{},{},{}",
            c.id,
            util::rfc3339(c.start_at),
            util::unix(c.start_at),
            c.start_level,
            util::rfc3339(end),
            util::unix(end),
            end_level,
            duration,
            c.offline_s,
            charge_min,
            quality,
            note
        );
        util::append_line(&cfg.cycles_path(), &line)?;
        let _ = std::fs::remove_file(cfg.cycle_state_path());
        Ok(())
    }

    /// 历史周期（按结束时间倒序）。
    pub fn history(cfg: &Config, limit: usize) -> Vec<Value> {
        let mut rows: Vec<Value> = util::read_rows(&cfg.cycles_path())
            .into_iter()
            .filter(|r| r.len() >= 12 && r[0] != "cycle_id")
            .filter_map(|r| {
                let duration = r[7].parse::<i64>().ok()?;
                Some(json!({
                    "cycle_id": r[0].parse::<u64>().ok()?,
                    "start_at": r[1],
                    "end_at": r[4],
                    "start_level": r[3].parse::<u8>().ok(),
                    "end_level": r[6].parse::<u8>().ok(),
                    "duration_s": duration,
                    "duration_text": util::dur_text(duration),
                    "offline_s": r[8].parse::<i64>().unwrap_or(0),
                    "quality": r[10],
                }))
            })
            .collect();

        rows.reverse();
        rows.truncate(limit.max(1));
        rows
    }

    /// 只统计合格的「满充」周期，且只看最近 20 个。
    pub fn stats(cfg: &Config) -> Value {
        let mut durations: Vec<i64> = util::read_rows(&cfg.cycles_path())
            .into_iter()
            .filter(|r| r.len() >= 12 && r[0] != "cycle_id")
            .filter(|r| r[10] == "charged" || r[10] == "charged_estimated")
            .filter_map(|r| r[7].parse::<i64>().ok())
            .collect();

        durations.truncate(durations.len());
        let start = durations.len().saturating_sub(20);
        let window = &durations[start..];

        if window.is_empty() {
            return Value::Null;
        }
        let sum: i64 = window.iter().sum();
        let avg = sum / window.len() as i64;
        json!({
            "count": window.len(),
            "avg_duration_s": avg,
            "avg_duration_text": util::dur_text(avg),
            "best": util::dur_text(*window.iter().max().unwrap()),
            "worst": util::dur_text(*window.iter().min().unwrap()),
            "window": "最近 20 个合格周期",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn cfg(name: &str) -> Config {
        let mut c = Config::default();
        c.data_dir = std::env::temp_dir().join(format!("hidread-cycle-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&c.data_dir);
        std::fs::create_dir_all(&c.data_dir).unwrap();
        c
    }

    fn t(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 11, 18, h, m, 0).unwrap()
    }

    #[test]
    fn full_charge_then_drain_closes_cycle() {
        let c = cfg("full");
        let mut s = CycleStore::load(&c);

        s.on_sample(&c, t(9, 0), 15, false).unwrap(); // 无线放电
        s.on_sample(&c, t(10, 0), 15, true).unwrap(); // 插线
        s.on_sample(&c, t(10, 5), 100, true).unwrap(); // 显示 100%，但只充了 5 分钟
        s.on_sample(&c, t(10, 30), 100, true).unwrap(); // 充够 30 分钟 → 确认充满
        s.on_sample(&c, t(11, 0), 95, false).unwrap(); // 拔线

        let cur = s.current_json(t(12, 0), Some(95));
        assert_eq!(cur["quality"], "charged", "满充拔线应开 charged 周期");
        assert_eq!(cur["start_level"], 95);

        s.on_sample(&c, t(20, 0), 9, false).unwrap(); // 掉到阈值
        assert!(s.current_json(t(20, 1), None).is_null(), "到阈值后周期应结束");

        let rows = util::read_rows(&c.cycles_path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][10], "charged");
        assert_eq!(rows[0][7], "32400", "11:00→20:00 应为 9 小时");
    }

    #[test]
    fn short_charge_is_rejected_as_partial() {
        let c = cfg("short");
        let mut s = CycleStore::load(&c);

        s.on_sample(&c, t(9, 0), 15, false).unwrap();
        s.on_sample(&c, t(10, 0), 15, true).unwrap();
        s.on_sample(&c, t(10, 5), 100, true).unwrap(); // 虚高：5 分钟就"满"
        s.on_sample(&c, t(10, 10), 100, true).unwrap(); // 仍不足 20 分钟
        s.on_sample(&c, t(10, 11), 100, false).unwrap(); // 拔线

        let cur = s.current_json(t(10, 12), Some(100));
        assert_eq!(cur["quality"], "partial", "充电过短不能算满充");
    }

    #[test]
    fn plugging_back_marks_interrupted() {
        let c = cfg("interrupted");
        let mut s = CycleStore::load(&c);

        s.on_sample(&c, t(9, 0), 100, true).unwrap();
        s.on_sample(&c, t(10, 0), 100, true).unwrap();
        s.on_sample(&c, t(11, 0), 95, false).unwrap(); // 开周期
        assert!(!s.current_json(t(11, 1), Some(95)).is_null());

        s.on_sample(&c, t(12, 0), 90, true).unwrap(); // 没跑完又插线
        assert!(s.current_json(t(12, 1), Some(90)).is_null());

        let rows = util::read_rows(&c.cycles_path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][10], "interrupted");
    }

    #[test]
    fn stats_only_count_qualified_cycles() {
        let c = cfg("stats");
        let header = "cycle_id,start_at,start_unix,start_level,end_at,end_unix,end_level,duration_s,offline_s,charge_min,quality,note\n";
        util::append_line(
            &c.cycles_path(),
            header.trim_end(),
        )
        .unwrap();
        for (i, (dur, q)) in [(100_000, "charged"), (50_000, "partial"), (80_000, "charged_estimated")]
            .iter()
            .enumerate()
        {
            util::append_line(
                &c.cycles_path(),
                &format!("{i},x,0,100,y,0,10,{dur},0,30,{q},ok"),
            )
            .unwrap();
        }

        let st = CycleStore::stats(&c);
        assert_eq!(st["count"], 2, "partial 不应参与统计");
        assert_eq!(st["avg_duration_text"], util::dur_text(90_000));
    }
}
