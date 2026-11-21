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
    /// 拔线前这次充电持续了多少分钟（在 open 时定格，之后 charge_started 会被清掉）
    charge_min: i64,
}

#[derive(Clone)]
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
            // 状态文件可能丢（手工删除/异常退出），用 CSV 里的最大 id 兜底，避免重号
            next_id: Self::next_id_from_csv(cfg),
        };

        if let Ok(text) = std::fs::read_to_string(cfg.cycle_state_path()) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(start_unix) = v.get("start_unix").and_then(Value::as_i64) {
                    if let Some(at) =
                        DateTime::from_timestamp(start_unix, 0).map(|t| t.with_timezone(&Local))
                    {
                        let id = v.get("id").and_then(Value::as_u64).unwrap_or(1);
                        store.current = Some(Current {
                            id,
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
                            charge_min: v.get("charge_min").and_then(Value::as_i64).unwrap_or(0),
                        });
                        store.next_id = store.next_id.max(id + 1);
                    }
                }
                // 这些字段决定"充电中重启后还能不能判出满充"，以及失联多久收尾
                store.last_online = ts_field(&v, "last_seen_unix");
                store.charge_started = ts_field(&v, "charge_started_unix");
                store.saw_full = v.get("saw_full").and_then(Value::as_bool).unwrap_or(false);
                store.full_confirmed = v
                    .get("full_confirmed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
        }

        store
    }

    fn next_id_from_csv(cfg: &Config) -> u64 {
        util::read_rows(&cfg.cycles_path())
            .iter()
            .filter(|r| !r.is_empty() && r[0] != "cycle_id")
            .filter_map(|r| r[0].parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1
    }

    /// 落盘完整状态（周期 + 充电确认进度）。
    ///
    /// 只写周期字段是不够的：充电中重启会丢掉 `charge_started/saw_full`，
    /// 拔线时就把满充误判成 partial；`last_seen` 丢了则 `on_offline` 永不收尾。
    fn persist(&self, cfg: &Config) -> Result<()> {
        if self.current.is_none() && self.charge_started.is_none() {
            let _ = std::fs::remove_file(cfg.cycle_state_path());
            return Ok(());
        }

        let mut v = json!({
            "last_seen_unix": self.last_online.map(util::unix),
            "charge_started_unix": self.charge_started.map(util::unix),
            "saw_full": self.saw_full,
            "full_confirmed": self.full_confirmed,
        });
        if let Some(c) = &self.current {
            v["id"] = json!(c.id);
            v["start_at"] = json!(util::rfc3339(c.start_at));
            v["start_unix"] = json!(util::unix(c.start_at));
            v["start_level"] = json!(c.start_level);
            v["quality"] = json!(c.quality);
            v["offline_s"] = json!(c.offline_s);
            v["charge_min"] = json!(c.charge_min);
        }
        util::write_json_atomic(&cfg.cycle_state_path(), &v)
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
        let mut dirty = false;
        match (prev, wired) {
            (Some(false), true) => {
                // 插线：开始充电计时；未跑完的周期记 interrupted
                self.charge_started = Some(at);
                self.saw_full = false;
                self.full_confirmed = false;
                let had_cycle = self.current.is_some();
                if had_cycle {
                    self.close(cfg, at, level, "interrupted", "plugged_in")?;
                }
                // close 内部已经落盘；没有周期可关时才需要自己写
                dirty = !had_cycle;
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
                // open 已写过一次；清掉充电态后还要再写一次
                dirty = true;
            }
            (None, true) => {
                // 服务启动时已经在充电：从恢复的 charge_started 继续计时，
                // 没有记录才从现在开始（否则重启会把已充时长清零）
                if self.charge_started.is_none() {
                    self.charge_started = Some(at);
                    dirty = true;
                }
            }
            (None, false) if self.current.is_none() && level >= cfg.cycle_full_level => {
                // 服务晚于拔线启动：只能记估计起点
                self.open(cfg, at, level, "charged_estimated", "estimated_start")?;
            }
            _ => {}
        }

        if wired && level >= cfg.cycle_full_level {
            if !self.saw_full {
                self.saw_full = true;
                dirty = true;
            }
            if let Some(started) = self.charge_started {
                if (at - started).num_minutes() >= cfg.cycle_min_charge_min as i64
                    && !self.full_confirmed
                {
                    self.full_confirmed = true;
                    dirty = true;
                }
            }
        }

        if !wired && level <= cfg.cycle_end_level && self.current.is_some() {
            self.close(cfg, at, level, "", "")?;
        }

        if dirty {
            self.persist(cfg)?;
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
            // 必须在清掉 charge_started 之前定格
            charge_min: self
                .charge_started
                .map(|s| (at - s).num_minutes().max(0))
                .unwrap_or(0),
        };
        self.next_id += 1;
        let _ = note;
        self.current = Some(c);
        self.persist(cfg)
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
        let charge_min = c.charge_min;

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
        // 不能直接删状态文件：充电中/刚插线时还有 charge_started 要留着
        self.persist(cfg)
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

fn ts_field(v: &Value, key: &str) -> Option<DateTime<Local>> {
    v.get(key)
        .and_then(Value::as_i64)
        .and_then(|s| DateTime::from_timestamp(s, 0))
        .map(|t| t.with_timezone(&Local))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, TimeZone};

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

    #[test]
    fn charge_state_survives_restart() {
        let c = cfg("restart-charge");
        let mut s = CycleStore::load(&c);
        s.on_sample(&c, t(9, 0), 15, false).unwrap();
        s.on_sample(&c, t(10, 0), 15, true).unwrap(); // 插线
        s.on_sample(&c, t(10, 30), 100, true).unwrap(); // 充够 30 分钟

        // 重启：只靠状态文件重建
        let mut s2 = CycleStore::load(&c);
        s2.on_sample(&c, t(10, 45), 100, true).unwrap(); // 仍在充电
        s2.on_sample(&c, t(11, 0), 95, false).unwrap(); // 拔线

        let cur = s2.current_json(t(11, 1), Some(95));
        assert_eq!(cur["quality"], "charged", "充电中重启后拔线仍应判满充");
        assert_eq!(cur["start_level"], 95);

        s2.on_sample(&c, t(20, 0), 9, false).unwrap(); // 掉到阈值收尾
        let rows = util::read_rows(&c.cycles_path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][9], "60", "充电时长应从重启前的 10:00 起算");
    }

    #[test]
    fn restart_then_device_gone_closes_cycle() {
        let c = cfg("restart-zombie");
        let mut s = CycleStore::load(&c);
        s.on_sample(&c, t(9, 0), 100, true).unwrap();
        s.on_sample(&c, t(10, 0), 95, false).unwrap(); // 开周期
        assert!(!s.current_json(t(10, 1), Some(95)).is_null());

        // 重启后设备再也不回来：失联超过 cycle_dead_hours 必须收尾，不能变僵尸
        let mut s2 = CycleStore::load(&c);
        s2.on_offline(&c, t(10, 0) + ChronoDuration::hours(25))
            .unwrap();
        assert!(
            s2.current_json(t(11, 0), Some(95)).is_null(),
            "超过周期超时应收尾为 unknown_end"
        );
        let rows = util::read_rows(&c.cycles_path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][10], "unknown_end");
    }
}
