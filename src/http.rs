//! 本地 HTTP 接口 + 设置页。
//!
//! 读接口只碰内存/磁盘，不触发 HID；写接口只有 `POST /api/config` 一个，
//! 保存后由服务自己重启，让每一项设置都按同一套语义生效。

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::Local;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::app::App;
use crate::autostart;
use crate::config::Config;
use crate::history::History;
use crate::util;

type Body = Response<Cursor<Vec<u8>>>;

pub fn bind(app: &App) -> Result<Server> {
    let addr = format!("{}:{}", app.cfg.bind, app.cfg.port);
    let server = Server::http(&addr).map_err(|e| anyhow!("监听 {addr} 失败：{e}"))?;

    eprintln!(
        "hidread 已启动：http://{addr}  tick={}s  vid={:04X}  pids={:?}  data={}",
        app.cfg.tick_s,
        app.cfg.vid,
        app.cfg.pids.iter().map(|p| format!("{p:04X}")).collect::<Vec<_>>(),
        app.cfg.data_dir.display()
    );

    Ok(server)
}

pub fn run(server: Server, app: Arc<App>) -> Result<()> {
    for mut request in server.incoming_requests() {
        let path = request.url().to_string();
        let (route, q) = split_query(&path);
        let method = request.method().clone();

        let response = if method == Method::Post && route == "/api/config" {
            if !origin_ok(&request) {
                json_resp(403, &json!({"ok": false, "error": "拒绝跨站请求"}))
            } else {
                let mut body = String::new();
                let _ = request
                    .as_reader()
                    .take(64 * 1024)
                    .read_to_string(&mut body);
                save_config(&app, &body)
            }
        } else if method != Method::Get {
            json_resp(405, &json!({"error": "只支持 GET"}))
        } else {
            match route {
                "/" => html_resp(SETTINGS_HTML.to_string()),
                "/battery" => {
                    let (code, v) = app.battery_json();
                    json_resp(code, &v)
                }
                "/health" => json_resp(200, &app.health_json()),
                "/raw" => json_resp(200, &app.raw_json()),
                "/session" => json_resp(200, &app.session_json()),
                "/sessions" => {
                    let day = q
                        .get("day")
                        .cloned()
                        .unwrap_or_else(|| Local::now().format("%Y-%m-%d").to_string());
                    json_resp(200, &app.sessions_json(&day))
                }
                "/daily" => {
                    let days = int_param(&q, "days", 14, 1, 90);
                    json_resp(200, &app.daily_json(days))
                }
                "/history" => {
                    let hours = int_param(&q, "hours", 24, 1, 24 * 30);
                    json_resp(200, &app.history_json(hours))
                }
                "/history.csv" => {
                    let days = int_param(&q, "days", 7, 1, 30);
                    csv_resp(history_csv(&app.cfg, days), "hidread-history.csv")
                }
                "/daily.csv" => {
                    let days = int_param(&q, "days", 14, 1, 90);
                    csv_resp(daily_csv(&app, days), "hidread-daily.csv")
                }
                "/events" => {
                    let days = int_param(&q, "days", 7, 1, 30);
                    json_resp(200, &events_json(&app.cfg, days))
                }
                "/cycles" => {
                    let limit = int_param(&q, "limit", 10, 1, 100) as usize;
                    json_resp(200, &app.cycles_json(limit))
                }
                "/api/config" => json_resp(200, &config_json(&app)),
                "/usage" => {
                    let days = int_param(&q, "days", 14, 1, 90);
                    json_resp(200, &app.usage_json(days))
                }
                "/chart" => {
                    let hours = int_param(&q, "hours", 24, 1, 24 * 30);
                    html_resp(chart_html(hours))
                }
                _ => json_resp(
                    404,
                    &json!({"error": "未知路径", "paths": [
                        "/", "/battery", "/health", "/raw", "/session", "/sessions", "/daily",
                        "/history", "/history.csv", "/daily.csv", "/events", "/cycles",
                        "/api/config", "/chart"
                    ]}),
                ),
            }
        };

        if let Err(e) = request.respond(response) {
            eprintln!("响应失败：{e}");
        }
    }

    Ok(())
}

/// 只接受同源（或没有 Origin 的本地客户端）的写请求，挡住浏览器跨站表单。
fn origin_ok(req: &Request) -> bool {
    let Some(h) = req
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("origin"))
    else {
        return true;
    };
    let v = h.value.as_str();
    v.contains("127.0.0.1") || v.contains("localhost")
}

fn config_json(app: &App) -> Value {
    let mut v = app.settings.to_json();
    // 自启不在 config.json 里，实时读注册表
    v["autostart"] = json!(autostart::status().map(|s| s.is_some()).unwrap_or(false));
    // 文件里可能是相对路径，这里给出实际生效的绝对路径
    v["data_dir_effective"] = json!(app.cfg.data_dir.display().to_string());
    v
}

fn save_config(app: &App, body: &str) -> Body {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return json_resp(400, &json!({"ok": false, "error": "请求体不是合法 JSON"}));
    };

    // 先校验原始值，再应用到配置（顺序不能反：apply_json 会做防御性 clamp）
    if let Err(e) = Config::check_raw(&v) {
        return json_resp(400, &json!({"ok": false, "error": format!("{e}")}));
    }

    let mut next = app.settings.clone();
    next.apply_json(&v);

    if let Err(e) = next.validate(app.cfg.port) {
        return json_resp(400, &json!({"ok": false, "error": format!("{e}")}));
    }

    // 开机自启写注册表，不属于 config.json，也不需要重启
    let want_auto = v.get("autostart").and_then(Value::as_bool);
    let has_auto = autostart::status().map(|s| s.is_some()).unwrap_or(false);
    let mut auto_changed = false;
    if let Some(want) = want_auto {
        if want != has_auto {
            let res = if want {
                autostart::install(&app.cfg.data_dir).map(|_| ())
            } else {
                autostart::uninstall().map(|_| ())
            };
            if let Err(e) = res {
                return json_resp(
                    500,
                    &json!({"ok": false, "error": format!("设置开机自启失败：{e:#}")}),
                );
            }
            auto_changed = true;
        }
    }

    let changed = next.to_json() != app.settings.to_json();
    if !changed {
        // 只改了自启：不写文件、不重启
        return json_resp(
            200,
            &json!({
                "ok": true,
                "restarting": false,
                "changed": auto_changed,
                "autostart": want_auto.unwrap_or(has_auto),
            }),
        );
    }

    match next.save() {
        Ok(path) => {
            // 先把响应发回去，再重启，避免浏览器拿到连接重置
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(500));
                if let Err(e) = crate::restart() {
                    eprintln!("自动重启失败：{e:#}");
                }
            });
            json_resp(
                200,
                &json!({"ok": true, "restarting": true, "path": path.display().to_string()}),
            )
        }
        Err(e) => json_resp(
            500,
            &json!({"ok": false, "error": format!("写配置文件失败：{e:#}")}),
        ),
    }
}

fn split_query(url: &str) -> (&str, HashMap<String, String>) {
    let mut map = HashMap::new();
    let (route, query) = match url.split_once('?') {
        Some((r, q)) => (r, q),
        None => (url, ""),
    };
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(k.to_string(), v.to_string());
    }
    (route, map)
}

fn int_param(q: &HashMap<String, String>, key: &str, default: i64, min: i64, max: i64) -> i64 {
    q.get(key)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn json_resp(status: u16, value: &Value) -> Body {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(body)
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json; charset=utf-8"))
        .with_header(header("Cache-Control", "no-store"))
}

fn csv_resp(text: String, filename: &str) -> Body {
    Response::from_data(text.into_bytes())
        .with_status_code(200)
        .with_header(header("Content-Type", "text/csv; charset=utf-8"))
        .with_header(header(
            "Content-Disposition",
            &format!("attachment; filename=\"{filename}\""),
        ))
}

fn html_resp(html: String) -> Body {
    Response::from_data(html.into_bytes())
        .with_status_code(200)
        .with_header(header("Content-Type", "text/html; charset=utf-8"))
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("header")
}

fn history_csv(cfg: &Config, days: i64) -> String {
    let now = Local::now();
    let from = now - chrono::Duration::days(days);
    let points = History::load_range(cfg, from, now);

    let mut out = String::from("ts,ts_unix,level,wired,link,read_ms\n");
    for p in points {
        out.push_str(&format!(
            "{},{},{},{},{},{}\n",
            util::rfc3339(p.at),
            util::unix(p.at),
            p.level.map(|v| v.to_string()).unwrap_or_default(),
            if p.wired { 1 } else { 0 },
            if p.online { "online" } else { "offline" },
            p.read_ms
        ));
    }
    out
}

fn daily_csv(app: &App, days: i64) -> String {
    let v = app.daily_json(days);
    let mut out = String::from(
        "date,sessions,online_s,online_text,offline_s,first_wake,last_sleep,longest_s,level_first,level_last,level_min,level_max,wired_s\n",
    );
    if let Some(rows) = v.get("days").and_then(Value::as_array) {
        for r in rows {
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
                r["date"].as_str().unwrap_or(""),
                r["sessions"],
                r["online_s"],
                r["online_text"].as_str().unwrap_or(""),
                r["offline_s"],
                r["first_wake"].as_str().unwrap_or(""),
                r["last_sleep"].as_str().unwrap_or(""),
                r["longest_s"],
                opt(&r["level"]["first"]),
                opt(&r["level"]["last"]),
                opt(&r["level"]["min"]),
                opt(&r["level"]["max"]),
                r["wired_s"],
            ));
        }
    }
    out
}

fn opt(v: &Value) -> String {
    if v.is_null() {
        String::new()
    } else {
        v.to_string()
    }
}

fn events_json(cfg: &Config, days: i64) -> Value {
    let today = Local::now().date_naive();
    let mut items = Vec::new();

    for offset in (0..days.max(1)).rev() {
        let date = today - chrono::Duration::days(offset);
        let path = cfg.events_path(&date.format("%Y-%m-%d").to_string());
        for row in util::read_rows(&path) {
            if row.len() < 4 || row[0] == "ts" {
                continue;
            }
            let Ok(ts) = row[1].parse::<i64>() else {
                continue;
            };
            items.push(json!({"t": ts, "at": row[0], "event": row[2], "detail": row[3]}));
        }
    }

    json!({"count": items.len(), "events": items})
}

/// 设置页：状态卡片 + 表单，单文件、无外部依赖。
const SETTINGS_HTML: &str = r##"<!doctype html><html lang="zh-CN"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>hidread 设置</title>
<style>
:root{--bg:#f6f8fb;--card:#fff;--line:#e3e9f0;--text:#111827;--sub:#5b6b7c;--accent:#185fa5;--ok:#3b6d11;--bad:#a32d2d}
*{box-sizing:border-box}
body{margin:0;padding:28px 20px 60px;background:var(--bg);color:var(--text);font:14px/1.6 system-ui,"Segoe UI",sans-serif}
.wrap{max-width:860px;margin:0 auto}
header{display:flex;align-items:center;gap:10px;margin-bottom:18px}
h1{font-size:18px;font-weight:600;margin:0}
.badge{font-size:12px;padding:2px 8px;border-radius:999px;border:1px solid var(--line);color:var(--sub);background:#fff}
.badge.on{color:var(--ok);border-color:#c0dd97;background:#eaf3de}
.badge.off{color:var(--bad);border-color:#f7c1c1;background:#fcebeb}
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:12px;margin-bottom:22px}
.card{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:14px 16px}
.card .k{font-size:12px;color:var(--sub);margin-bottom:2px}
.card .v{font-size:26px;font-weight:600;line-height:1.2}
.card .v small{font-size:14px;font-weight:400;color:var(--sub)}
.card .s{font-size:12px;color:var(--sub);margin-top:2px}
.bar{height:6px;background:#eef2f7;border-radius:999px;margin-top:8px;overflow:hidden}
.bar i{display:block;height:100%;width:0;background:var(--accent);border-radius:999px;transition:width .3s}
form{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:6px 18px 18px}
fieldset{border:0;border-top:1px solid var(--line);margin:0;padding:14px 0 6px}
fieldset:first-of-type{border-top:0}
legend{font-size:12px;color:var(--sub);padding:0 6px 0 0;letter-spacing:.02em}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(230px,1fr));gap:12px 18px}
label{display:flex;flex-direction:column;gap:4px;font-size:13px;color:var(--sub)}
label input[type=text],label input[type=number]{font:inherit;color:var(--text);padding:7px 9px;border:1px solid var(--line);border-radius:8px;background:#fff}
label input:focus{outline:2px solid #cfe2f7;border-color:var(--accent)}
label .hint{font-size:11px;color:#8b98a5}
label.check{flex-direction:row;align-items:center;gap:8px;color:var(--text);margin-top:22px}
details{margin:6px 0 4px}
summary{cursor:pointer;font-size:12px;color:var(--sub);padding:10px 0}
.actions{display:flex;align-items:center;gap:12px;margin-top:18px;padding-top:14px;border-top:1px solid var(--line)}
button{font:inherit;padding:8px 18px;border-radius:9px;border:1px solid var(--accent);background:var(--accent);color:#fff;cursor:pointer}
button:disabled{opacity:.55;cursor:default}
.msg{font-size:13px;color:var(--sub)}
.msg.err{color:var(--bad)}
footer{margin-top:18px;font-size:12px;color:var(--sub)}
footer a{color:var(--accent);text-decoration:none;margin-right:10px}
.panels{display:grid;grid-template-columns:minmax(250px,.95fr) 1.55fr;gap:12px;margin-bottom:22px}
.panel{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:12px 14px;cursor:zoom-in}
.panel.usage{grid-row:span 2}
.ph{display:flex;justify-content:space-between;align-items:center;font-size:12px;color:var(--sub);margin-bottom:8px}
.ph .more{color:var(--accent);text-decoration:none;font-size:11px}
#curve-thumb{width:100%;height:150px;display:block}
#heat-thumb{display:block;margin:0 auto}
.mini{width:100%;border-collapse:collapse;font-size:12px}
.mini th{text-align:left;color:#8b98a5;font-weight:500;padding:2px 4px;border-bottom:1px solid var(--line);white-space:nowrap}
.mini td{padding:3px 4px;border-bottom:1px solid #f4f7fa;white-space:nowrap}
.mini td.n{font-variant-numeric:tabular-nums}
.legend-heat{font-size:11px;color:#8b98a5;margin-top:8px;display:flex;align-items:center;gap:2px;flex-wrap:wrap}
.legend-heat i{width:9px;height:9px;border-radius:2px;display:inline-block;margin:0 3px 0 8px}
.heat-sum{font-size:12px;color:var(--sub);margin-top:10px;border-top:1px solid var(--line);padding-top:8px}
#modal{position:fixed;inset:0;background:rgba(17,24,39,.45);display:none;align-items:center;justify-content:center;z-index:50;padding:20px}
#modal.show{display:flex}
.modal-box{background:#fff;border-radius:14px;padding:16px 20px 20px;width:min(94vw,1000px);max-height:88vh;overflow:auto;box-shadow:0 24px 60px rgba(0,0,0,.28)}
.modal-box h3{margin:0 0 14px;font-size:14px;font-weight:600;display:flex;justify-content:space-between;align-items:center;gap:12px}
.modal-box h3 button{all:unset;cursor:pointer;color:var(--sub);font-size:20px;line-height:1;padding:0 6px}
.modal-box table{width:100%;border-collapse:collapse;font-size:12px}
.modal-box th,.modal-box td{padding:4px 6px;border-bottom:1px solid #f1f5f9;text-align:left;white-space:nowrap}
.modal-box th{color:#8b98a5;font-weight:500}
@media(max-width:760px){.panels{grid-template-columns:1fr}.panel.usage{grid-row:auto}}
#toast{position:fixed;left:50%;bottom:26px;transform:translateX(-50%) translateY(20px);background:#111827;color:#fff;padding:10px 16px;border-radius:10px;font-size:13px;opacity:0;transition:.25s;pointer-events:none}
#toast.show{opacity:1;transform:translateX(-50%) translateY(0)}
#toast.err{background:var(--bad)}
</style></head><body><div class="wrap">
<header><h1>hidread</h1><span id="badge" class="badge">连接中…</span></header>

<section class="cards">
  <div class="card"><div class="k">电量</div><div class="v"><span id="level">--</span><small>%</small></div>
    <div class="bar"><i id="bar"></i></div><div class="s" id="bstate">--</div></div>
  <div class="card"><div class="k">模式</div><div class="v" id="mode" style="font-size:20px">--</div><div class="s" id="dev">--</div></div>
  <div class="card"><div class="k">本次在线</div><div class="v" id="uptime" style="font-size:20px">--</div><div class="s" id="since">--</div></div>
  <div class="card"><div class="k">放电周期</div><div class="v" id="cycle" style="font-size:20px">--</div><div class="s" id="cyc">--</div></div>
</section>

<section class="panels">
  <div class="panel usage" data-zoom="usage">
    <div class="ph"><span>使用时段 · 最近 14 天</span><a class="more" href="#" data-zoom="usage">双击放大</a></div>
    <canvas id="heat-thumb"></canvas>
    <div class="legend-heat" id="heat-legend"></div>
    <div class="heat-sum" id="heat-sum"></div>
  </div>
  <div class="panel" data-zoom="curve">
    <div class="ph"><span>电量曲线 · 最近 24 小时</span><a class="more" href="#" data-zoom="curve">双击放大</a></div>
    <canvas id="curve-thumb"></canvas>
  </div>
  <div class="panel" data-zoom="daily">
    <div class="ph"><span>每日使用</span><a class="more" href="#" data-zoom="daily">双击放大</a></div>
    <table class="mini"><thead><tr><th>日期</th><th>在线</th><th>首次</th><th>末次</th></tr></thead>
      <tbody id="daily-body"><tr><td colspan="4" style="color:#8b98a5">加载中…</td></tr></tbody></table>
  </div>
</section>

<form id="f">
  <fieldset><legend>采集</legend><div class="grid">
    <label>采样节拍（秒）<input type="number" data-key="tick_s" min="5" max="3600">
      <span class="hint">越小越灵敏，但 HID 读取更频繁</span></label>
    <label>缓存有效期（秒）<input type="number" data-key="cache_ttl_s" min="1" max="3600">
      <span class="hint">超过该时长标记为 stale</span></label>
    <label>判定离线所需失败次数<input type="number" data-key="offline_after_fails" min="1" max="10">
      <span class="hint">2 次 ≈ 两分钟确认休眠</span></label>
  </div></fieldset>

  <fieldset><legend>记录</legend><div class="grid">
    <label>历史基线间隔（秒）<input type="number" data-key="history_interval_s" min="10" max="86400">
      <span class="hint">数值变化时会立即补点</span></label>
    <label>历史保留天数<input type="number" data-key="history_days" min="1" max="365"></label>
  </div></fieldset>

  <fieldset><legend>放电周期</legend><div class="grid">
    <label>满充判定电量（%）<input type="number" data-key="cycle_full_level" min="1" max="100"></label>
    <label>最短充电时长（分钟）<input type="number" data-key="cycle_min_charge_min" min="0" max="600">
      <span class="hint">防电压虚高把短充当满充</span></label>
    <label>周期结束电量（%）<input type="number" data-key="cycle_end_level" min="0" max="99"></label>
    <label>失联判死时长（小时）<input type="number" data-key="cycle_dead_hours" min="1" max="720">
      <span class="hint">超时未达阈值则记 unknown_end</span></label>
  </div></fieldset>

  <fieldset><legend>服务</legend><div class="grid">
    <label>监听地址<input type="text" data-key="bind" placeholder="127.0.0.1"></label>
    <label>端口<input type="number" data-key="port" min="1" max="65535"></label>
    <label>数据目录<input type="text" data-key="data_dir">
      <span class="hint">相对路径基于程序所在目录</span>
      <span class="hint" id="dd-effective"></span></label>
    <label class="check"><input type="checkbox" data-key="tray"> 显示托盘图标</label>
    <label class="check"><input type="checkbox" data-key="autostart"> 开机自启（登录时静默启动）</label>
  </div></fieldset>

  <details><summary>高级（HID 匹配与超时）</summary><div class="grid">
    <label>VID<input type="text" data-key="vid"></label>
    <label>PID 列表<input type="text" data-key="pids">
      <span class="hint">逗号分隔，无线,有线</span></label>
    <label>UsagePage<input type="text" data-key="usage_page"></label>
    <label>单次读超时（ms）<input type="number" data-key="read_timeout_ms" min="10" max="5000"></label>
    <label>单轮预算（ms）<input type="number" data-key="read_deadline_ms" min="50" max="10000"></label>
    <label>重试次数<input type="number" data-key="retries" min="1" max="10"></label>
  </div></details>

  <div class="actions">
    <button type="submit" id="save">保存并重启</button>
    <span class="msg" id="msg">保存后服务会自动重启，约 1 秒</span>
  </div>
</form>

<footer>
  <a href="/chart?hours=24">电量曲线</a>
  <a href="/daily?days=14">每日表</a>
  <a href="/cycles">放电周期</a>
  <a href="/daily.csv?days=14">导出每日 CSV</a>
  <a href="/history.csv?days=7">导出历史 CSV</a>
  <a href="/raw">原始帧</a>
</footer>
</div>
<div id="modal"><div class="modal-box"><h3><span id="modal-title"></span><button id="modal-close" title="关闭">×</button></h3><div id="modal-body"></div></div></div>
<div id="toast"></div>
<script>
const numKeys=new Set(['port','tick_s','cache_ttl_s','offline_after_fails','history_interval_s','history_days','cycle_full_level','cycle_min_charge_min','cycle_end_level','cycle_dead_hours','read_timeout_ms','read_deadline_ms','retries']);
const fields=[...document.querySelectorAll('[data-key]')];
const $=id=>document.getElementById(id);
function toast(text,err){const t=$('toast');t.textContent=text;t.className='show'+(err?' err':'');clearTimeout(t._t);t._t=setTimeout(()=>t.className='',4000);}
function fmtSince(iso){if(!iso)return '--';const d=new Date(iso);return '自 '+d.toLocaleTimeString('zh-CN',{hour:'2-digit',minute:'2-digit'});}
async function loadConfig(){
  const c=await (await fetch('/api/config',{cache:'no-store'})).json();
  for(const el of fields){
    const k=el.dataset.key;
    if(el.type==='checkbox') el.checked=!!c[k];
    else if(k==='pids') el.value=(c[k]||[]).join(',');
    else el.value=c[k]??'';
  }
  const eff=$('dd-effective');
  if(eff&&c.data_dir_effective) eff.textContent='当前实际：'+c.data_dir_effective;
}
async function loadStatus(){
  try{
    const r=await fetch('/battery',{cache:'no-store'});
    const b=await r.json();
    const lv=b.battery&&b.battery.level;
    $('level').textContent=lv==null?'--':lv;
    $('bar').style.width=(lv||0)+'%';
    $('bar').style.background=lv==null?'#c9d3de':(lv<30?'#a32d2d':lv<60?'#ba7517':'#185fa5');
    const st=b.battery&&b.battery.status;
    $('bstate').textContent=st==='wired'?'有线（读数虚高）':st==='discharging'?'无线放电':'未知';
    $('mode').textContent=b.device&&b.device.mode==='wired'?'有线':b.device&&b.device.mode==='wireless'?'2.4G 无线':'--';
    $('dev').textContent=b.device&&b.device.pid?('PID '+b.device.pid):'--';
    const up=b.link&&b.link.session_uptime_text;
    $('uptime').textContent=up||'--';
    $('since').textContent=fmtSince(b.link&&b.link.session_start_at);
    const badge=$('badge');
    if(b.link&&b.link.state==='online'){badge.className='badge on';badge.textContent='在线';}
    else{badge.className='badge off';badge.textContent='离线';}
    const cy=b.cycle;
    $('cycle').textContent=cy?cy.elapsed_text:'--';
    $('cyc').textContent=cy?('起点 '+cy.start_level+'% · '+cy.quality):'当前无进行中周期';
  }catch(e){ $('badge').className='badge off'; $('badge').textContent='服务不可达'; }
}
$('f').addEventListener('submit',async e=>{
  e.preventDefault();
  const body={};
  for(const el of fields){
    const k=el.dataset.key;
    if(el.type==='checkbox') body[k]=el.checked;
    else if(k==='pids') body[k]=el.value.split(',').map(s=>s.trim()).filter(Boolean);
    else if(numKeys.has(k)) body[k]=Number(el.value);
    else body[k]=el.value.trim();
  }
  $('save').disabled=true; $('msg').textContent='保存中…'; $('msg').className='msg';
  try{
    const r=await fetch('/api/config',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(body)});
    const j=await r.json();
    if(!j.ok){ $('msg').textContent=j.error||'保存失败'; $('msg').className='msg err'; $('save').disabled=false; return; }
    if(!j.restarting){
      // 只改了开机自启：写注册表即生效，不重启
      toast(j.changed?'已保存':'没有需要保存的改动');
      $('msg').textContent='已生效';
      $('msg').className='msg';
      $('save').disabled=false;
      return;
    }
    toast('已保存，正在重启…');
    $('msg').textContent='服务重启中…';
    for(let i=0;i<25;i++){
      await new Promise(r=>setTimeout(r,600));
      try{ const h=await fetch('/health',{cache:'no-store'}); if(h.ok){ toast('已重启，设置生效'); $('msg').textContent='已生效'; $('save').disabled=false; loadStatus(); return; } }catch(_){}
    }
    toast('服务未在预期时间内恢复，请检查 data/hidread.log',true);
    $('save').disabled=false;
  }catch(err){ $('msg').textContent='请求失败：'+err; $('msg').className='msg err'; $('save').disabled=false; }
});
// ---------- 缩略图 / 放大 ----------
let dailyCache=[], usageCache=[];
function heatColor(m){return m<=0?'#eef2f7':m<15?'#cfe2f7':m<35?'#8fbde8':m<50?'#4a86c8':'#185fa5';}
function legendHtml(prefix){
  return (prefix?'每小时在线：':'')
    +'<i style="background:#eef2f7"></i>0'
    +'<i style="background:#cfe2f7"></i>&lt;15m'
    +'<i style="background:#8fbde8"></i>&lt;35m'
    +'<i style="background:#4a86c8"></i>&lt;50m'
    +'<i style="background:#185fa5"></i>≥50m';
}
function fitCanvas(cv,h){
  const d=window.devicePixelRatio||1;
  const w=cv.clientWidth||400, hh=h||cv.clientHeight||150;
  cv.width=Math.round(w*d);cv.height=Math.round(hh*d);
  cv.style.height=hh+'px';
  const ctx=cv.getContext('2d');ctx.setTransform(d,0,0,d,0,0);
  return {ctx,w,hh};
}
function drawCurve(cv,points,axes){
  const {ctx,w:W,hh:H}=fitCanvas(cv,axes?360:150);
  ctx.clearRect(0,0,W,H);
  const padL=axes?38:4,padB=axes?22:4,padT=axes?10:4,padR=8;
  const pts=points.filter(p=>p.level!==null);
  if(!pts.length){ctx.fillStyle='#8b98a5';ctx.font='12px system-ui';ctx.textAlign='left';ctx.fillText('暂无数据',padL+4,H/2);return;}
  const t0=points[0].t,t1=points[points.length-1].t||t0+1;
  const x=t=>padL+(t-t0)/((t1-t0)||1)*(W-padL-padR);
  const y=v=>H-padB-(v/100)*(H-padT-padB);
  if(axes){
    ctx.strokeStyle='#eef2f5';ctx.lineWidth=1;ctx.font='10px system-ui';ctx.textAlign='right';
    for(let v=0;v<=100;v+=25){ctx.beginPath();ctx.moveTo(padL,y(v));ctx.lineTo(W-padR,y(v));ctx.stroke();ctx.fillStyle='#8b98a5';ctx.fillText(v+'%',padL-6,y(v)+3);}
    const hours=(t1-t0)/3600, stepH=hours>48?24:6;
    ctx.textAlign='center';
    for(let t=t0;t<=t1;t+=stepH*3600){const d=new Date(t*1000);ctx.fillText(String(d.getHours()).padStart(2,'0')+':00',x(t),H-6);}
  }
  // 先铺一层面积底色（缩略图才画，放大图保持干净）
  if(!axes){
    let run=[];
    const flush=()=>{
      if(run.length>1){
        ctx.beginPath();ctx.moveTo(x(run[0].t),H-padB);
        run.forEach(p=>ctx.lineTo(x(p.t),y(p.level)));
        ctx.lineTo(x(run[run.length-1].t),H-padB);ctx.closePath();
        ctx.fillStyle='rgba(24,95,165,.07)';ctx.fill();
      }
      run=[];
    };
    for(const p of points){
      if(p.level===null){flush();continue;}
      run.push(p);
    }
    flush();
  }

  let prev=null;
  // 点太密时不再逐个画圆点，否则 24h/461 点会糊成一条色带
  const dense=points.length>120;
  for(const p of points){
    if(p.level===null){prev=null;continue;}
    const px=x(p.t),py=y(p.level);
    if(prev){
      ctx.beginPath();ctx.moveTo(x(prev.t),y(prev.level));ctx.lineTo(px,py);
      ctx.setLineDash(p.wired?[4,3]:[]);
      ctx.strokeStyle=p.wired?'#ba7517':'#185fa5';ctx.lineWidth=axes?2:1.6;ctx.stroke();
    }
    // 点数很少时补圆点：进程刚启动只有 1 个采样点，否则整张图是空的
    if(!dense){
      ctx.beginPath();ctx.arc(px,py,axes?2:1.6,0,Math.PI*2);
      ctx.fillStyle=p.wired?'#ba7517':'#185fa5';ctx.fill();
    }
    prev=p;
  }
  ctx.setLineDash([]);
}
function rrect(ctx,x,y,w,h,r){
  ctx.beginPath();
  if(ctx.roundRect)ctx.roundRect(x,y,w,h,r);
  else{ctx.moveTo(x+r,y);ctx.arcTo(x+w,y,x+w,y+h,r);ctx.arcTo(x+w,y+h,x,y+h,r);ctx.arcTo(x,y+h,x,y,r);ctx.arcTo(x,y,x+w,y,r);}
  ctx.fill();
}
function drawHeat(cv,days,availW,axes){
  const gap=2,left=axes?30:26,top=4,bottom=axes?44:36;
  let cell=Math.floor((availW-left-gap*days.length)/Math.max(1,days.length));
  cell=Math.max(7,Math.min(axes?30:16,cell));
  if(axes){
    // 同时受弹窗可视高度约束，否则底部日期轴会被裁掉
    const byH=Math.floor((620-top-bottom)/24)-gap;
    cell=Math.max(7,Math.min(cell,byH));
  }
  const W=left+days.length*(cell+gap),H=top+24*(cell+gap)+bottom;
  const d=window.devicePixelRatio||1;
  cv.width=Math.round(W*d);cv.height=Math.round(H*d);
  cv.style.width=W+'px';cv.style.height=H+'px';cv.style.margin='0 auto';cv.style.display='block';
  const ctx=cv.getContext('2d');ctx.setTransform(d,0,0,d,0,0);
  ctx.clearRect(0,0,W,H);
  ctx.font=(axes?'10px':'9px')+' system-ui';ctx.textBaseline='middle';ctx.textAlign='right';
  for(let h=0;h<24;h++){
    const y=top+h*(cell+gap);
    if(h%3===0){ctx.fillStyle='#7b8998';ctx.fillText(String(h).padStart(2,'0'),left-7,y+cell/2);}
    for(let i=0;i<days.length;i++){
      const m=days[i].hours[h]||0;
      ctx.fillStyle=heatColor(m);
      rrect(ctx,left+i*(cell+gap),y,cell,cell,Math.min(3,cell/4));
    }
  }
  ctx.textAlign='left';ctx.fillStyle='#7b8998';ctx.font=(axes?'10px':'9px')+' system-ui';
  const step=Math.max(1,Math.ceil(days.length/(axes?8:5)));
  for(let i=0;i<days.length;i+=step){
    const x=left+i*(cell+gap)+cell/2;
    ctx.save();ctx.translate(x,H-bottom+10);ctx.rotate(-Math.PI/4);ctx.fillText(days[i].date.slice(5),0,0);ctx.restore();
  }
}
function renderDaily(tbody,days,full){
  tbody.innerHTML=days.map(d=>{
    if(full){
      return `<tr><td>${d.date}</td><td class="n">${d.sessions}</td><td class="n">${d.online_text}</td>`
        +`<td class="n">${d.first_wake||'--'}</td><td class="n">${d.last_sleep||'--'}</td>`
        +`<td class="n">${d.longest_s?Math.round(d.longest_s/60)+'m':'--'}</td>`
        +`<td class="n">${d.level.min??'--'}–${d.level.max??'--'}%</td>`
        +`<td class="n">${d.wired_s?Math.round(d.wired_s/60)+'m':'--'}</td></tr>`;
    }
    return `<tr><td>${d.date.slice(5)}</td><td class="n">${d.online_text}</td>`
      +`<td class="n">${d.first_wake||'--'}</td><td class="n">${d.last_sleep||'--'}</td></tr>`;
  }).join('');
}
function fmtDur(s){s=Math.round(s);const h=Math.floor(s/3600),m=Math.round((s%3600)/60);return h?h+'h'+String(m).padStart(2,'0')+'m':m+'m';}
async function loadPanels(){
  try{
    const [h,d,u]=await Promise.all([
      fetch('/history?hours=24',{cache:'no-store'}).then(r=>r.json()),
      fetch('/daily?days=7',{cache:'no-store'}).then(r=>r.json()),
      fetch('/usage?days=14',{cache:'no-store'}).then(r=>r.json())
    ]);
    dailyCache=d.days.slice().reverse();
    usageCache=u.days||[];
    drawCurve($('curve-thumb'),h.points,false);
    renderDaily($('daily-body'),dailyCache,false);
    $('heat-legend').innerHTML=legendHtml(false);
    const thumbW=(($('heat-thumb').parentElement)||document.body).clientWidth-30;
    drawHeat($('heat-thumb'),usageCache,thumbW,false);
    const total=usageCache.reduce((a,x)=>a+(x.total_s||0),0);
    const active=usageCache.filter(x=>x.total_s>0).length;
    $('heat-sum').textContent='近 '+usageCache.length+' 天合计在线 '+fmtDur(total)+'，有记录 '+active+' 天';
  }catch(e){}
}
function openModal(title,build){
  $('modal-title').textContent=title;
  const body=$('modal-body');body.innerHTML='';
  $('modal').classList.add('show');
  build(body);
}
function closeModal(){$('modal').classList.remove('show');}
$('modal-close').addEventListener('click',closeModal);
$('modal').addEventListener('click',e=>{if(e.target.id==='modal')closeModal();});
document.addEventListener('keydown',e=>{if(e.key==='Escape')closeModal();});
async function zoom(kind){
  if(kind==='curve'){
    const h=await fetch('/history?hours=72',{cache:'no-store'}).then(r=>r.json());
    openModal('电量曲线 · 最近 72 小时（实线无线 / 虚线有线读数）',body=>{
      const cv=document.createElement('canvas');cv.style.width='100%';body.appendChild(cv);
      requestAnimationFrame(()=>drawCurve(cv,h.points,true));
    });
  }else if(kind==='daily'){
    const d=await fetch('/daily?days=14',{cache:'no-store'}).then(r=>r.json());
    openModal('每日使用表 · 最近 14 天',body=>{
      body.innerHTML='<table><thead><tr><th>日期</th><th>唤醒</th><th>在线</th><th>首次</th><th>末次</th><th>最长</th><th>电量</th><th>有线</th></tr></thead><tbody id="m-daily"></tbody></table>';
      renderDaily(body.querySelector('#m-daily'),d.days.slice().reverse(),true);
    });
  }else if(kind==='usage'){
    const u=await fetch('/usage?days=30',{cache:'no-store'}).then(r=>r.json());
    const total=(u.days||[]).reduce((a,x)=>a+(x.total_s||0),0);
    openModal('使用时段 · 最近 30 天 · 合计在线 '+fmtDur(total),body=>{
      const lg=document.createElement('div');lg.className='legend-heat';lg.style.margin='0 0 12px';lg.innerHTML=legendHtml(true);body.appendChild(lg);
      const cv=document.createElement('canvas');body.appendChild(cv);
      requestAnimationFrame(()=>drawHeat(cv,u.days,body.clientWidth-8,true));
    });
  }
}
document.querySelectorAll('.more[data-zoom]').forEach(el=>{
  el.addEventListener('click',e=>{e.preventDefault();e.stopPropagation();zoom(el.dataset.zoom);});
});
document.querySelectorAll('.panel[data-zoom]').forEach(el=>{
  el.addEventListener('dblclick',e=>{
    if(e.target.closest('.more'))return;
    e.preventDefault();zoom(el.dataset.zoom);
  });
});
const _zoom=new URLSearchParams(location.search).get('zoom');
if(_zoom)setTimeout(()=>zoom(_zoom),400);
loadPanels();
setInterval(loadPanels,60000);
loadConfig().catch(e=>toast('读取配置失败：'+e,true));
loadStatus();
setInterval(loadStatus,5000);
</script></body></html>"##;

fn chart_html(hours: i64) -> String {
    format!(
        r#"<!doctype html><html lang="zh-CN"><meta charset="utf-8"><title>hidread 电量</title>
<style>
body{{font:13px/1.5 system-ui,sans-serif;margin:24px;color:#111;background:#fff}}
canvas{{width:100%;height:320px;border:1px solid #dde3ea;border-radius:8px}}
.legend{{color:#5b6b7c;margin:8px 0}}
a{{color:#185fa5;text-decoration:none}}
</style>
<p><a href="/">← 返回设置</a></p>
<h2 style="font-size:15px;font-weight:500;margin:0 0 4px">最近 {hours} 小时电量</h2>
<div class="legend">实线＝无线（放电）；虚线＝有线（读数虚高，不可当真）</div>
<canvas id="c"></canvas>
<script>
const cv=document.getElementById('c'),ctx=cv.getContext('2d');
function draw(d){{
  const dpr=window.devicePixelRatio||1;
  cv.width=cv.clientWidth*dpr;cv.height=cv.clientHeight*dpr;ctx.scale(dpr,dpr);
  const W=cv.clientWidth,H=cv.clientHeight,pad=32;
  ctx.clearRect(0,0,W,H);
  const pts=d.points.filter(p=>p.level!==null);
  if(!pts.length){{ctx.fillStyle='#888';ctx.fillText('暂无数据',pad,pad);return}}
  const t0=d.points[0].t,t1=d.points[d.points.length-1].t||t0+1;
  const x=t=>pad+(t-t0)/(t1-t0||1)*(W-pad*2);
  const y=v=>H-pad-(v/100)*(H-pad*2);
  ctx.strokeStyle='#eef1f5';ctx.lineWidth=1;
  for(let v=0;v<=100;v+=25){{ctx.beginPath();ctx.moveTo(pad,y(v));ctx.lineTo(W-pad,y(v));ctx.stroke();
    ctx.fillStyle='#8b98a5';ctx.fillText(v+'%',6,y(v)+4)}}
  let prev=null;
  for(const p of d.points){{
    if(p.level===null){{prev=null;continue}}
    if(prev){{
      ctx.beginPath();ctx.moveTo(x(prev.t),y(prev.level));ctx.lineTo(x(p.t),y(p.level));
      ctx.setLineDash(p.wired?'4 3':[]);
      ctx.strokeStyle=p.wired?'#ba7517':'#185fa5';ctx.lineWidth=1.5;ctx.stroke();
    }}
    prev=p;
  }}
  ctx.setLineDash([]);
}}
fetch('/history?hours={hours}').then(r=>r.json()).then(draw).catch(e=>{{
  document.getElementById('c').outerHTML='<pre>加载失败：'+e+'</pre>';
}});
</script></html>"#
    )
}
