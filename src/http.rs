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
    let addr = app.cfg.socket_addr();
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

/// 并发处理请求：单个慢请求（30 天曲线要读几十个 CSV）不该把 `/battery` 一起堵住。
const WORKERS: usize = 4;

pub fn run(server: Server, app: Arc<App>) -> Result<()> {
    let server = Arc::new(server);
    let mut workers = Vec::with_capacity(WORKERS);
    for i in 0..WORKERS {
        let server = Arc::clone(&server);
        let app = Arc::clone(&app);
        workers.push(
            std::thread::Builder::new()
                .name(format!("hidread-http-{i}"))
                .spawn(move || loop {
                    match server.recv() {
                        Ok(request) => handle(request, &app),
                        Err(_) => break, // 服务端关闭
                    }
                })?,
        );
    }
    for w in workers {
        let _ = w.join();
    }
    Ok(())
}

fn handle(mut request: Request, app: &App) {
    let path = request.url().to_string();
    let (route, q) = split_query(&path);
    let method = request.method().clone();

    let response = if method == Method::Post && route == "/api/config" {
        if !remote_is_local(&request) {
            json_resp(403, &json!({"ok": false, "error": "只允许本机修改配置"}))
        } else if !origin_ok(&request) {
            json_resp(403, &json!({"ok": false, "error": "拒绝跨站请求"}))
        } else if !is_json_content_type(&request) {
            // 必须是 application/json：跨站表单发不出这个类型，
            // 也就无法用 simple request 绕过浏览器预检
            json_resp(
                415,
                &json!({"ok": false, "error": "Content-Type 必须是 application/json"}),
            )
        } else {
            let mut body = String::new();
            let _ = request
                .as_reader()
                .take(64 * 1024)
                .read_to_string(&mut body);
            save_config(app, &body)
        }
    } else if method != Method::Get {
        json_resp(405, &json!({"error": "只支持 GET"}))
    } else {
        match route {
            "/" => html_resp(SETTINGS_HTML.replace("{{VER}}", env!("CARGO_PKG_VERSION"))),
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
                let max = int_param(&q, "max", 1200, 100, 5000) as usize;
                json_resp(200, &app.history_json(hours, max))
            }
            "/history.csv" => {
                let days = int_param(&q, "days", 7, 1, 30);
                csv_resp(history_csv(&app.cfg, days), "hidread-history.csv")
            }
            "/daily.csv" => {
                let days = int_param(&q, "days", 14, 1, 90);
                csv_resp(daily_csv(app, days), "hidread-daily.csv")
            }
            "/events" => {
                let days = int_param(&q, "days", 7, 1, 30);
                json_resp(200, &events_json(&app.cfg, days))
            }
            "/cycles" => {
                let limit = int_param(&q, "limit", 10, 1, 100) as usize;
                json_resp(200, &app.cycles_json(limit))
            }
            "/api/config" => json_resp(200, &config_json(app)),
            "/usage" => {
                let days = int_param(&q, "days", 14, 1, 90);
                json_resp(200, &app.usage_json(days))
            }
            "/week" => {
                let start = q.get("start").map(String::as_str);
                let days = int_param(&q, "days", 7, 1, 60);
                json_resp(200, &app.week_json(start, days))
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
                    "/usage", "/week", "/api/config", "/chart"
                ]}),
            ),
        }
    };

    if let Err(e) = request.respond(response) {
        eprintln!("响应失败：{e}");
    }
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
    // 不能用 contains：`127.0.0.1.nip.io` / `localhost.evil.com` 都含关键字
    match origin_host(h.value.as_str()) {
        Some(host) => is_loopback_host(host),
        None => false,
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "[::1]" | "::1"
    )
}

/// 从 `scheme://host[:port]` 里取 host（IPv6 保留方括号）。
fn origin_host(origin: &str) -> Option<&str> {
    let rest = origin.split_once("://")?.1;
    let authority = rest
        .split(|c| c == '/' || c == '?' || c == '#')
        .next()?;
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    if let Some(after) = authority.strip_prefix('[') {
        let end = after.find(']')? + 1;
        return authority.get(..=end);
    }
    Some(match authority.rsplit_once(':') {
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => authority,
    })
}

fn is_json_content_type(req: &Request) -> bool {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("content-type"))
        .map(|h| {
            h.value
                .as_str()
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        })
        .unwrap_or(false)
}

fn remote_is_local(req: &Request) -> bool {
    req.remote_addr()
        .map(|a| a.ip().is_loopback())
        .unwrap_or(false)
}

/// 设置页里的 `data_dir` 可能是相对路径；注册自启必须写绝对路径，
/// 否则登录时 CWD 不同会把数据写进 System32。
fn effective_data_dir(raw: &std::path::Path) -> std::path::PathBuf {
    if raw.is_absolute() {
        return raw.to_path_buf();
    }
    crate::config::config_path()
        .and_then(|p| p.parent().map(|d| d.join(raw)))
        .unwrap_or_else(|| raw.to_path_buf())
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
                // 用这次保存后的数据目录：同一次保存里改了 data_dir 又开了自启时，
                // 注册表不能还指向旧路径
                autostart::install(&effective_data_dir(&next.data_dir)).map(|_| ())
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
label input[type=text],label input[type=number],label select{font:inherit;color:var(--text);padding:7px 9px;border:1px solid var(--line);border-radius:8px;background:#fff;width:100%}
label select{appearance:none;-webkit-appearance:none;padding-right:30px;background-image:url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='10' height='6'%3E%3Cpath d='M1 1l4 4 4-4' stroke='%238b98a5' fill='none' stroke-width='1.5'/%3E%3C/svg%3E");background-repeat:no-repeat;background-position:right 10px center}
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
.panels{display:grid;grid-template-columns:1.55fr 1fr;gap:12px;margin-bottom:22px}
.panel{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:12px 14px;cursor:zoom-in}
.panel.week{grid-column:1/-1}
.panel[data-zoom="curve"]{display:flex;flex-direction:column}
#curve-thumb{width:100%;flex:1;min-height:120px}
.ph{display:flex;justify-content:space-between;align-items:center;font-size:12px;color:var(--sub);margin-bottom:8px}
.ph .more{color:var(--accent);text-decoration:none;font-size:11px}
#curve-thumb{width:100%;height:150px;display:block}
#week-thumb{width:100%;display:block}
.week-bar{display:flex;align-items:center;gap:10px;margin-bottom:10px;font-size:12px;color:var(--sub)}
.week-bar button{all:unset;cursor:pointer;padding:3px 10px;border:1px solid var(--line);border-radius:8px;color:var(--sub)}
.week-bar button:hover{border-color:var(--accent);color:var(--accent)}
.week-bar .wk-sum{margin-left:auto}
.week-body{display:flex;position:relative;align-items:flex-start}
#week-axis{width:42px;flex:0 0 42px;display:block}
.week-scroll{flex:1;overflow-x:auto;overflow-y:hidden;cursor:grab}
.week-scroll.dragging{cursor:grabbing}
.week-scroll::-webkit-scrollbar{height:8px}
.week-scroll::-webkit-scrollbar-thumb{background:#d5dde6;border-radius:4px}
.week-scroll canvas{display:block}
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
#modal-body{position:relative}
.range-tabs{display:flex;gap:6px;margin-bottom:10px}
.range-tabs button{all:unset;cursor:pointer;font-size:12px;padding:4px 14px;border-radius:999px;border:1px solid var(--line);color:var(--sub)}
.range-tabs button.on{background:var(--accent);border-color:var(--accent);color:#fff}
.stats-line{font-size:12px;color:var(--sub);margin-bottom:10px}
.heat-tip{position:absolute;transform:translate(-50%,-115%);background:#111827;color:#fff;font-size:12px;padding:6px 10px;border-radius:8px;white-space:nowrap;pointer-events:none;display:none;z-index:10;box-shadow:0 6px 16px rgba(0,0,0,.25)}
.heat-tip b{color:#7fb2e5}
@media(max-width:760px){.panels{grid-template-columns:1fr}}
#toast{position:fixed;left:50%;bottom:26px;transform:translateX(-50%) translateY(20px);background:#111827;color:#fff;padding:10px 16px;border-radius:10px;font-size:13px;opacity:0;transition:.25s;pointer-events:none}
#toast.show{opacity:1;transform:translateX(-50%) translateY(0)}
#toast.err{background:var(--bad)}
</style></head><body><div class="wrap">
<header><h1>hidread <small class="ver">v{{VER}}</small></h1><span id="badge" class="badge">连接中…</span></header>

<section class="cards">
  <div class="card"><div class="k">电量</div><div class="v"><span id="level">--</span><small>%</small></div>
    <div class="bar"><i id="bar"></i></div><div class="s" id="bstate">--</div></div>
  <div class="card"><div class="k">模式</div><div class="v" id="mode" style="font-size:20px">--</div><div class="s" id="dev">--</div></div>
  <div class="card"><div class="k">本次在线</div><div class="v" id="uptime" style="font-size:20px">--</div><div class="s" id="since">--</div></div>
  <div class="card"><div class="k">放电周期</div><div class="v" id="cycle" style="font-size:20px">--</div><div class="s" id="cyc">--</div></div>
</section>

<section class="panels">
  <div class="panel week" data-zoom="week">
    <div class="ph"><span>使用时段 · 本周（时间轴按近 30 天活跃区间自动裁剪）</span><a class="more" href="#" data-zoom="week">双击放大</a></div>
    <canvas id="week-thumb"></canvas>
    <div class="heat-sum" id="week-sum"></div>
  </div>
  <div class="panel" data-zoom="curve">
    <div class="ph"><span>电量曲线 · 预览 1 天</span><a class="more" href="#" data-zoom="curve">双击放大</a></div>
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
    <label>鼠标休眠档位
      <select data-key="sleep_s">
        <option value="30">30 秒</option>
        <option value="60">1 分钟</option>
        <option value="120">2 分钟</option>
        <option value="180">3 分钟</option>
        <option value="300">5 分钟</option>
        <option value="1200">20 分钟</option>
        <option value="1500">25 分钟</option>
        <option value="1800">30 分钟</option>
      </select>
      <span class="hint">与驱动里设置一致；判离线时长由它决定，改节拍不会缩短</span></label>
    <label>判离线失败次数（下限）<input type="number" data-key="offline_after_fails" min="1" max="10">
      <span class="hint" id="offline-hint">与静默阈值同时满足才判离线；调大只会更慢</span></label>
  </div></fieldset>

  <fieldset><legend>记录</legend><div class="grid">
    <label>历史基线间隔（秒）<input type="number" data-key="history_interval_s" min="10" max="86400">
      <span class="hint">数值变化时会立即补点</span></label>
    <label>数据上限（MB）<input type="number" data-key="data_max_mb" min="16" max="10240">
      <span class="hint">超过上限时从最旧的曲线/事件文件开始清理，最近 7 天始终保留</span></label>
  </div></fieldset>

  <fieldset><legend>放电周期</legend><div class="grid">
    <label>满充判定电量（%）<input type="number" data-key="cycle_full_level" min="1" max="100"></label>
    <label>最短充电时长（分钟）<input type="number" data-key="cycle_min_charge_min" min="0" max="600">
      <span class="hint">防电压虚高把短充当满充</span></label>
    <label>周期结束电量（%）<input type="number" data-key="cycle_end_level" min="0" max="99"></label>
    <label>周期超时（小时）<input type="number" data-key="cycle_dead_hours" min="1" max="720">
      <span class="hint">放电周期内超过这么久没有数据就作废（记 unknown_end），避免把跨天的间隔算成一个周期</span></label>
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
const numKeys=new Set(['port','sleep_s','tick_s','offline_after_fails','history_interval_s','data_max_mb','cycle_full_level','cycle_min_charge_min','cycle_end_level','cycle_dead_hours','read_timeout_ms','read_deadline_ms','retries']);
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
    else if(el.tagName==='SELECT'){
      // 配置文件里可能是下拉框没有预设的值（如 45s），补一个选项再选中，
      // 否则浏览器会落到第一项，保存时被静默改成 30s
      const v=String(c[k]??'');
      if(v&&![...el.options].some(o=>o.value===v)) el.add(new Option(v+'（当前值）',v));
      el.value=v;
    }
    else el.value=c[k]??'';
  }
  const eff=$('dd-effective');
  if(eff&&c.data_dir_effective) eff.textContent='当前实际：'+c.data_dir_effective;
  refreshOfflineHint();
}
function refreshOfflineHint(){
  const q=k=>document.querySelector('[data-key="'+k+'"]');
  const tick=Number(q('tick_s')&&q('tick_s').value)||60;
  const sleep=Number(q('sleep_s')&&q('sleep_s').value)||60;
  const fails=Math.max(1,Number(q('offline_after_fails')&&q('offline_after_fails').value)||2);
  const probe=Math.min(Math.max(Math.floor(sleep/3),10),Math.max(tick,5));
  const quiet=Math.max(probe*2,30);
  // 从最后一次成功读取算起：首次失败最多要等一个节拍，之后每 probe 秒确认一次
  const est=Math.max(quiet,tick)+(fails-1)*probe;
  const el=$('offline-hint');
  if(el)el.textContent='最后一次读取后约 '+est+'s 判离线（静默 ≥'+quiet+'s 且失败 ≥'+fails+' 次；平时 '+tick+'s / 快探 '+probe+'s）';
}
['input','change'].forEach(ev=>document.addEventListener(ev,e=>{
  const k=e.target&&e.target.dataset&&e.target.dataset.key;
  if(k==='tick_s'||k==='sleep_s'||k==='offline_after_fails')refreshOfflineHint();
}));
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
let dailyCache=[];
let curveHours=7;
let weekCur=null, lastWeek=null;
const CURVE_RANGES=[1,3,7,30];
const WK=['一','二','三','四','五','六','日'];
function fitCanvas(cv,h){
  const d=window.devicePixelRatio||1;
  const w=cv.clientWidth||400, hh=h||cv.clientHeight||150;
  cv.width=Math.round(w*d);cv.height=Math.round(hh*d);
  if(h)cv.style.height=hh+'px';   // 不传高度时交给 CSS 撑满
  const ctx=cv.getContext('2d');ctx.setTransform(d,0,0,d,0,0);
  return {ctx,w,hh};
}
function drawCurve(cv,points,axes){
  const {ctx,w:W,hh:H}=fitCanvas(cv,axes?380:0);
  ctx.clearRect(0,0,W,H);
  const padL=axes?42:4,padB=axes?26:4,padT=axes?14:6,padR=axes?14:8;
  const pts=points.filter(p=>p.level!==null);
  if(!pts.length){ctx.fillStyle='#8b98a5';ctx.font='12px system-ui';ctx.textAlign='left';ctx.fillText('暂无数据',padL+4,H/2);return;}
  const t0=points[0].t,t1=points[points.length-1].t||t0+1;
  const x=t=>padL+(t-t0)/((t1-t0)||1)*(W-padL-padR);
  const y=v=>H-padB-(v/100)*(H-padT-padB);

  // 离线底带：先把碎片合并，再过滤过窄的段——
  // 鼠标休眠时采样点是"离线-在线-离线"交替的，逐点画会变成满屏竖条
  if(axes){
    const bands=[];
    let start=null;
    for(const p of points){
      const off=(p.level===null||p.link==='offline');
      if(off&&start===null)start=p.t;
      if(!off&&start!==null){bands.push([start,p.t]);start=null;}
    }
    if(start!==null)bands.push([start,t1]);

    // 相邻离线段的间隔不到 2.5 个采样间隔就并成一段
    const gapTol=Math.max(1,(t1-t0)/Math.max(1,points.length))*2.5;
    const merged=[];
    for(const b of bands){
      const last=merged[merged.length-1];
      if(last&&(b[0]-last[1])<=gapTol)last[1]=b[1];
      else merged.push([b[0],b[1]]);
    }

    ctx.fillStyle='rgba(148,163,184,.13)';
    for(const band of merged){
      const xa=x(band[0]),xb=x(band[1]);
      if(xb-xa<8)continue;   // 太窄的碎片不画，纯噪音
      ctx.fillRect(xa,padT,xb-xa,H-padT-padB);
    }
  }

  if(axes){
    ctx.strokeStyle='#eef2f5';ctx.lineWidth=1;ctx.font='10px system-ui';ctx.textAlign='right';
    for(let v=0;v<=100;v+=25){ctx.beginPath();ctx.moveTo(padL,y(v));ctx.lineTo(W-padR,y(v));ctx.stroke();ctx.fillStyle='#8b98a5';ctx.fillText(v+'%',padL-6,y(v)+3);}
    const span=(t1-t0)/3600;
    let stepH=6, dateLabel=false;
    if(span>48){ stepH=Math.max(24,Math.ceil(span/8/24)*24); dateLabel=true; }
    else if(span>12){ stepH=12; }
    ctx.textAlign='center';
    for(let t=t0;t<=t1;t+=stepH*3600){
      const dd=new Date(t*1000);
      ctx.strokeStyle='#f3f6f9';ctx.beginPath();ctx.moveTo(x(t),padT);ctx.lineTo(x(t),H-padB);ctx.stroke();
      ctx.fillStyle='#8b98a5';
      const lbl=dateLabel
        ?String(dd.getMonth()+1).padStart(2,'0')+'-'+String(dd.getDate()).padStart(2,'0')
        :String(dd.getHours()).padStart(2,'0')+':00';
      ctx.fillText(lbl,x(t),H-6);
    }
  }

  // 渐变面积（缩略图铺淡色，放大图更明显）
  if(pts.length>1){
    const g=ctx.createLinearGradient(0,padT,0,H-padB);
    g.addColorStop(0,'rgba(24,95,165,'+(axes?.16:.08)+')');
    g.addColorStop(1,'rgba(24,95,165,.02)');
    let run=[];
    const flush=()=>{
      if(run.length>1){
        ctx.beginPath();ctx.moveTo(x(run[0].t),H-padB);
        run.forEach(p=>ctx.lineTo(x(p.t),y(p.level)));
        ctx.lineTo(x(run[run.length-1].t),H-padB);ctx.closePath();
        ctx.fillStyle=g;ctx.fill();
      }
      run=[];
    };
    for(const p of points){ if(p.level===null){flush();continue;} run.push(p); }
    flush();
  }

  let prev=null;
  // 点太密时不再逐个画圆点，否则 24h/461 点会糊成一条色带
  const dense=points.length>140;
  // 间隔超过约 3 个采样周期就断开：离线期间零星读到的点不该连成直线
  const gapLimit=Math.max(1,(t1-t0)/Math.max(1,points.length))*3;
  for(const p of points){
    if(p.level===null){prev=null;continue;}
    const px=x(p.t),py=y(p.level);
    const adjacent=prev&&(p.t-prev.t)<=gapLimit;
    if(adjacent){
      ctx.beginPath();ctx.moveTo(x(prev.t),y(prev.level));ctx.lineTo(px,py);
      ctx.setLineDash(p.wired?[4,3]:[]);
      ctx.strokeStyle=p.wired?'#ba7517':'#185fa5';ctx.lineWidth=axes?2:1.6;ctx.stroke();
    }else if(axes){
      // 孤立点：画淡一点的小圆点，表明"这段只有零星读数"
      ctx.globalAlpha=.55;
      ctx.beginPath();ctx.arc(px,py,1.6,0,Math.PI*2);
      ctx.fillStyle=p.wired?'#ba7517':'#185fa5';ctx.fill();
      ctx.globalAlpha=1;
    }
    // 点数很少时补圆点：进程刚启动只有 1 个采样点，否则整张图是空的
    if(!dense){
      ctx.beginPath();ctx.arc(px,py,axes?2:1.6,0,Math.PI*2);
      ctx.fillStyle=p.wired?'#ba7517':'#185fa5';ctx.fill();
    }
    prev=p;
  }
  ctx.setLineDash([]);
  cv._curve={points,axes,x,y,W,H};
  if(axes)mountCurveHover(cv);
}
function mountCurveHover(cv){
  const host=cv.parentElement;
  if(getComputedStyle(host).position==='static')host.style.position='relative';
  // 关键：每次挂载先清掉旧的覆盖层与浮层，否则重绘一次就多叠一个
  host.querySelectorAll('.curve-ov,.curve-tip').forEach(n=>n.remove());

  const ov=document.createElement('canvas');
  ov.className='curve-ov';
  ov.style.cssText='position:absolute;pointer-events:none;z-index:4';
  host.appendChild(ov);
  const tip=document.createElement('div');
  tip.className='heat-tip curve-tip';
  tip.style.display='none';
  host.appendChild(tip);

  const dpr=window.devicePixelRatio||1;
  const w=cv.offsetWidth,h=cv.offsetHeight;
  ov.style.left=cv.offsetLeft+'px';
  ov.style.top=cv.offsetTop+'px';
  ov.style.width=w+'px';
  ov.style.height=h+'px';
  ov.width=Math.round(w*dpr);
  ov.height=Math.round(h*dpr);
  const octx=ov.getContext('2d');
  octx.setTransform(dpr,0,0,dpr,0,0);

  cv.onmousemove=e=>{
    const d=cv._curve;
    if(!d)return;
    const r=cv.getBoundingClientRect();
    const mx=e.clientX-r.left;
    let best=null,bd=1e9;
    for(const p of d.points){
      if(p.level===null)continue;
      const dd=Math.abs(d.x(p.t)-mx);
      if(dd<bd){bd=dd;best=p;}
    }
    octx.clearRect(0,0,w,h);
    if(!best||bd>60){tip.style.display='none';return;}
    const px=d.x(best.t),py=d.y(best.level);
    octx.setLineDash([3,3]);octx.strokeStyle='rgba(17,24,39,.35)';octx.lineWidth=1;
    octx.beginPath();octx.moveTo(px,10);octx.lineTo(px,h-24);octx.stroke();octx.setLineDash([]);
    octx.beginPath();octx.arc(px,py,4,0,Math.PI*2);octx.fillStyle='#111827';octx.fill();
    tip.innerHTML=best.at.replace('T',' ').slice(5,16)+' · <b>'+best.level+'%</b> '+(best.wired?'有线':'无线');
    placeTip(tip,host,cv.offsetLeft+px,cv.offsetTop+py-8);
  };
  cv.onmouseleave=()=>{
    octx.clearRect(0,0,w,h);
    tip.style.display='none';
  };
}
function rrect(ctx,x,y,w,h,r){
  ctx.beginPath();
  if(ctx.roundRect)ctx.roundRect(x,y,w,h,r);
  else{ctx.moveTo(x+r,y);ctx.arcTo(x+w,y,x+w,y+h,r);ctx.arcTo(x+w,y+h,x,y+h,r);ctx.arcTo(x,y+h,x,y,r);ctx.arcTo(x,y,x+w,y,r);}
  ctx.fill();
}
function fmtDate(d){return d.getFullYear()+'-'+String(d.getMonth()+1).padStart(2,'0')+'-'+String(d.getDate()).padStart(2,'0');}
function shiftDate(s,n){const d=s?new Date(s+'T00:00:00'):new Date();d.setDate(d.getDate()+n);return fmtDate(d);}
function placeTip(tip,host,x,y){
  tip.style.display='block';
  const tw=tip.offsetWidth,th=tip.offsetHeight;
  const hw=host.clientWidth||600;
  // 以鼠标为中心，但不能让左/右半边出界
  const left=Math.max(tw/2+4,Math.min(hw-tw/2-4,x));
  const top=Math.max(th*1.15+2,y);   // 浮层以底边定位，别让它顶出画布
  tip.style.left=left+'px';
  tip.style.top=top+'px';
}
function hourOff(ts){const d=new Date(ts*1000);return ((d.getHours()+20)%24)+d.getMinutes()/60+d.getSeconds()/3600;}
function hourLabel(off){return String((4+off)%24).padStart(2,'0')+':00';}
function hhmm(t){const d=new Date(t*1000);return String(d.getHours()).padStart(2,'0')+':'+String(d.getMinutes()).padStart(2,'0');}

// 日历周视图：7 列＝周一到周日，纵轴＝自学习出来的活跃时段，色块＝一次在线区间
function drawWeek(cv,week,height,opts){
  opts=opts||{};
  const dpr=window.devicePixelRatio||1;
  const W=cv.clientWidth||700,H=height;
  cv.width=Math.round(W*dpr);cv.height=Math.round(H*dpr);cv.style.height=H+'px';
  const ctx=cv.getContext('2d');ctx.setTransform(dpr,0,0,dpr,0,0);
  ctx.clearRect(0,0,W,H);

  const padL=42,padR=10,padT=opts.big?46:34,padB=8;
  const from=week.range.from,to=week.range.to,span=Math.max(1,to-from);
  const colW=(W-padL-padR)/7,plotH=H-padT-padB;
  const yOf=t=>padT+(hourOff(t)-from)/span*plotH;
  const todayIdx=week.days.findIndex(d=>d.date===week.today);

  // 今天列底色
  if(todayIdx>=0){
    ctx.fillStyle='rgba(24,95,165,.045)';
    ctx.fillRect(padL+todayIdx*colW,padT,colW,plotH);
  }

  // 横向网格 + 右侧时间刻度
  const step=span>14?2:1;
  ctx.font='10px system-ui';ctx.textBaseline='middle';ctx.textAlign='right';
  for(let h=from;h<=to;h+=step){
    const yy=padT+(h-from)/span*plotH;
    ctx.strokeStyle=h%6===0?'#e6ecf3':'#f2f6fa';
    ctx.beginPath();ctx.moveTo(padL,yy);ctx.lineTo(W-padR,yy);ctx.stroke();
    ctx.fillStyle='#8b98a5';
    ctx.fillText(hourLabel(h),padL-7,yy);
  }
  // 每天分隔线
  for(let i=0;i<=7;i++){
    ctx.strokeStyle='#e9eef4';
    ctx.beginPath();ctx.moveTo(padL+i*colW,padT);ctx.lineTo(padL+i*colW,H-padB);ctx.stroke();
  }

  // 表头：星期 + 日期，今天用蓝底胶囊
  week.days.forEach((d,i)=>{
    const cx=padL+i*colW+colW/2, dayNum=Number(d.date.slice(8,10));
    ctx.textAlign='center';
    if(i===todayIdx){
      const bw=opts.big?36:26, bh=opts.big?42:32, by=6;
      ctx.fillStyle='#185fa5';
      rrect(ctx,cx-bw/2,by,bw,bh,9);
      ctx.fillStyle='#fff';
      ctx.font=(opts.big?'11px':'10px')+' system-ui';
      ctx.fillText(WK[i],cx,by+(opts.big?12:10));
      ctx.font=(opts.big?'13px':'11px')+' 600 system-ui';
      ctx.fillText(String(dayNum),cx,by+(opts.big?29:23));
    }else{
      ctx.fillStyle='#8b98a5';ctx.font=(opts.big?'11px':'10px')+' system-ui';
      ctx.fillText(WK[i],cx,opts.big?16:12);
      ctx.fillStyle='#5b6b7c';ctx.font=(opts.big?'13px':'11px')+' system-ui';
      ctx.fillText(String(dayNum),cx,opts.big?32:25);
    }
  });

  // 事件块
  const blocks=[];
  week.days.forEach((d,i)=>{
    const x=padL+i*colW+3, w=Math.max(2,colW-6);
    for(const seg of d.segments){
      const y1=yOf(seg[0]),y2=yOf(seg[1]),bh=Math.max(3,y2-y1);
      ctx.fillStyle=i===todayIdx?'rgba(24,95,165,.5)':'rgba(24,95,165,.28)';
      rrect(ctx,x,y1,w,bh,Math.min(3,w/4));
      blocks.push({x:x,y:y1,w:w,h:bh,day:d.date,s:seg[0],e:seg[1]});
    }
  });

  // 当前时间线
  if(todayIdx>=0){
    const nh=hourOff(Date.now()/1000);   // 必须用业务日偏移，不能用自然小时
    if(nh>=from&&nh<=to){
      const yy=padT+(nh-from)/span*plotH;
      ctx.strokeStyle='#e5533d';ctx.lineWidth=1.5;
      ctx.beginPath();ctx.moveTo(padL+todayIdx*colW,yy);ctx.lineTo(padL+(todayIdx+1)*colW,yy);ctx.stroke();
      ctx.fillStyle='#e5533d';
      ctx.beginPath();ctx.arc(padL+todayIdx*colW-1,yy,2.5,0,Math.PI*2);ctx.fill();
    }
  }
  ctx.lineWidth=1;
  cv._wkBlocks=blocks;
  if(opts.hover)mountWeekHover(cv);
}
// 30 天宽视图：左侧固定时间轴 + 右侧横向拖动
function drawWeekWide(host,week){
  const axis=host.querySelector('#week-axis');
  const cv=host.querySelector('#week-big');
  const height=520,padT=46,padB=10,colW=34;
  const dpr=window.devicePixelRatio||1;
  const cols=week.days.length;
  const from=week.range.from,to=week.range.to,span=Math.max(1,to-from);
  const plotH=height-padT-padB;

  axis.width=Math.round(42*dpr);axis.height=Math.round(height*dpr);
  axis.style.height=height+'px';
  const ax=axis.getContext('2d');ax.setTransform(dpr,0,0,dpr,0,0);ax.clearRect(0,0,42,height);
  ax.font='10px system-ui';ax.textBaseline='middle';ax.textAlign='right';
  const step=span>14?2:1;
  for(let h=from;h<=to;h+=step){
    ax.fillStyle='#8b98a5';
    ax.fillText(hourLabel(h),36,padT+(h-from)/span*plotH);
  }

  const W=cols*colW;
  cv.width=Math.round(W*dpr);cv.height=Math.round(height*dpr);
  cv.style.width=W+'px';cv.style.height=height+'px';
  const ctx=cv.getContext('2d');ctx.setTransform(dpr,0,0,dpr,0,0);
  ctx.clearRect(0,0,W,height);

  const todayIdx=week.days.findIndex(d=>d.date===week.today);
  if(todayIdx>=0){ctx.fillStyle='rgba(24,95,165,.06)';ctx.fillRect(todayIdx*colW,padT,colW,plotH);}

  for(let h=from;h<=to;h+=step){
    const yy=padT+(h-from)/span*plotH;
    ctx.strokeStyle=h%6===0?'#e6ecf3':'#f4f7fa';
    ctx.beginPath();ctx.moveTo(0,yy);ctx.lineTo(W,yy);ctx.stroke();
  }

  week.days.forEach((d,i)=>{
    const x=i*colW;
    ctx.strokeStyle='#edf1f6';
    ctx.beginPath();ctx.moveTo(x,padT);ctx.lineTo(x,height-padB);ctx.stroke();
    const cx=x+colW/2, dayNum=Number(d.date.slice(8,10));
    const wd=new Date(d.date+'T00:00:00').getDay();
    const wk='日一二三四五六'[wd];
    ctx.textAlign='center';
    if(i===todayIdx){
      ctx.fillStyle='#185fa5';rrect(ctx,cx-13,6,26,34,9);
      ctx.fillStyle='#fff';ctx.font='10px system-ui';ctx.fillText(wk,cx,19);
      ctx.font='12px 600 system-ui';ctx.fillText(String(dayNum),cx,33);
    }else{
      ctx.fillStyle='#8b98a5';ctx.font='10px system-ui';ctx.fillText(wk,cx,18);
      ctx.fillStyle='#5b6b7c';ctx.font='12px system-ui';ctx.fillText(String(dayNum),cx,32);
    }
  });

  const blocks=[];
  week.days.forEach((d,i)=>{
    const x=i*colW+2,w=colW-4;
    for(const seg of d.segments){
      const y1=padT+(hourOff(seg[0])-from)/span*plotH;
      const y2=padT+(hourOff(seg[1])-from)/span*plotH;
      const bh=Math.max(3,y2-y1);
      ctx.fillStyle=i===todayIdx?'rgba(24,95,165,.5)':'rgba(24,95,165,.3)';
      rrect(ctx,x,y1,w,bh,2);
      blocks.push({x:x,y:y1,w:w,h:bh,day:d.date,s:seg[0],e:seg[1]});
    }
  });

  if(todayIdx>=0){
    const nh=hourOff(Date.now()/1000);
    if(nh>=from&&nh<=to){
      const yy=padT+(nh-from)/span*plotH;
      ctx.strokeStyle='#e5533d';ctx.lineWidth=1.5;
      ctx.beginPath();ctx.moveTo(todayIdx*colW,yy);ctx.lineTo((todayIdx+1)*colW,yy);ctx.stroke();
      ctx.fillStyle='#e5533d';ctx.beginPath();ctx.arc(todayIdx*colW-2,yy,2.5,0,Math.PI*2);ctx.fill();
    }
  }
  ctx.lineWidth=1;
  cv._wkBlocks=blocks;
  mountWeekWideHover(host,cv);
  // 默认停在最右：打开就看到最近几天
  const sc=host.querySelector('#week-scroll');
  requestAnimationFrame(()=>{sc.scrollLeft=sc.scrollWidth;});
}
function mountWeekWideHover(host,cv){
  const sc=host.querySelector('#week-scroll');
  host.querySelectorAll('.curve-tip').forEach(n=>n.remove());
  const tip=document.createElement('div');tip.className='heat-tip';tip.style.display='none';
  host.appendChild(tip);

  cv.onmousemove=e=>{
    const r=cv.getBoundingClientRect(),hr=host.getBoundingClientRect();
    const mx=e.clientX-r.left,my=e.clientY-r.top;
    const b=(cv._wkBlocks||[]).find(b=>mx>=b.x&&mx<=b.x+b.w&&my>=b.y-2&&my<=b.y+b.h+2);
    if(!b){tip.style.display='none';return;}
    tip.innerHTML=b.day.slice(5)+' '+hhmm(b.s)+'–'+hhmm(b.e)+' · <b>'+fmtDur(b.e-b.s)+'</b>';
    placeTip(tip,host,r.left-hr.left+mx,r.top-hr.top+my-10);
  };
  cv.onmouseleave=()=>{tip.style.display='none';};

  // 按住拖动 / 滚轮横向滚动
  let down=false,sx=0,sl=0;
  sc.onmousedown=e=>{down=true;sx=e.clientX;sl=sc.scrollLeft;sc.classList.add('dragging');e.preventDefault();};
  document.onmouseup=()=>{down=false;sc.classList.remove('dragging');};
  document.onmousemove=e=>{if(down)sc.scrollLeft=sl-(e.clientX-sx);};
  sc.onwheel=e=>{if(e.deltaY&&!e.deltaX){sc.scrollLeft+=e.deltaY;e.preventDefault();}};
}
function fitOn(cv,fn){
  if(cv._ro)cv._ro.disconnect();
  let timer=null;
  cv._ro=new ResizeObserver(()=>{clearTimeout(timer);timer=setTimeout(()=>{try{fn()}catch(_){}},120);});
  cv._ro.observe(cv);
}

function mountWeekHover(cv){
  const host=cv.parentElement;
  if(getComputedStyle(host).position==='static')host.style.position='relative';
  host.querySelectorAll('.curve-tip').forEach(n=>n.remove());
  const tip=document.createElement('div');tip.className='heat-tip';tip.style.display='none';
  host.appendChild(tip);
  cv.onmousemove=e=>{
    const r=cv.getBoundingClientRect();
    const mx=e.clientX-r.left,my=e.clientY-r.top;
    const b=(cv._wkBlocks||[]).find(b=>mx>=b.x-3&&mx<=b.x+b.w+3&&my>=b.y-2&&my<=b.y+b.h+2);
    if(!b){tip.style.display='none';return;}
    tip.innerHTML=b.day.slice(5)+' '+hhmm(b.s)+'–'+hhmm(b.e)+' · <b>'+fmtDur(b.e-b.s)+'</b>';
    placeTip(tip,host,cv.offsetLeft+b.x+b.w/2,cv.offsetTop+b.y-6);
  };
  cv.onmouseleave=()=>{tip.style.display='none';};
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
function fmtDur(s){s=Math.round(s);let h=Math.floor(s/3600),m=Math.round((s%3600)/60);if(m===60){h++;m=0;}return h?h+'h'+String(m).padStart(2,'0')+'m':m+'m';}
async function loadPanels(){
  try{
    const [h,d,w]=await Promise.all([
      fetch('/history?hours=24',{cache:'no-store'}).then(r=>r.json()),
      fetch('/daily?days=7',{cache:'no-store'}).then(r=>r.json()),
      fetch('/week',{cache:'no-store'}).then(r=>r.json())
    ]);
    dailyCache=d.days.slice().reverse();
    drawCurve($('curve-thumb'),h.points,false);
    renderDaily($('daily-body'),dailyCache,false);
    weekCur=w.start;lastWeek=w;
    drawWeek($('week-thumb'),w,280,{});
    fitOn($('week-thumb'),()=>{if(lastWeek)drawWeek($('week-thumb'),lastWeek,280,{});});
    $('week-sum').textContent='本周合计 '+fmtDur(w.total_s)+' · 每天平均 '+fmtDur(Math.round(w.total_s/7))
      +' · 时间轴 '+hourLabel(w.range.from)+'–'+hourLabel(w.range.to)+'（近 30 天学习）';
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
async function zoom(kind,range){
  if(kind==='curve'){
    if(range&&CURVE_RANGES.includes(Number(range)))curveHours=Number(range);
    openModal('电量曲线',body=>{
      body.innerHTML='<div class="range-tabs" id="curve-tabs"></div>'
        +'<div class="stats-line" id="curve-stats">加载中…</div>'
        +'<canvas id="curve-big" style="width:100%"></canvas>'
        +'<div class="legend-heat" style="margin-top:10px">'
        +'<i style="background:#185fa5"></i>无线（实线）'
        +'<i style="background:#ba7517"></i>有线（虚线·读数虚高）'
        +'<i style="background:rgba(148,163,184,.4)"></i>离线区间'
        +'<i style="background:#185fa5;border-radius:50%;width:5px;height:5px"></i>零星读数'
        +'</div>';
      const bar=body.querySelector('#curve-tabs');
      bar.innerHTML=CURVE_RANGES.map(d=>'<button data-h="'+d+'"'+(d===curveHours?' class="on"':'')+'>'+d+' 天</button>').join('');
      bar.querySelectorAll('button').forEach(b=>b.onclick=()=>{
        curveHours=Number(b.dataset.h);
        bar.querySelectorAll('button').forEach(x=>x.classList.toggle('on',x===b));
        loadCurveRange(body.querySelector('#curve-big'),curveHours);
      });
      loadCurveRange(body.querySelector('#curve-big'),curveHours);
    });
  }else if(kind==='daily'){
    const d=await fetch('/daily?days=14',{cache:'no-store'}).then(r=>r.json());
    openModal('每日使用表 · 最近 14 天',body=>{
      body.innerHTML='<table><thead><tr><th>日期</th><th>唤醒</th><th>在线</th><th>首次</th><th>末次</th><th>最长</th><th>电量</th><th>有线</th></tr></thead><tbody id="m-daily"></tbody></table>';
      renderDaily(body.querySelector('#m-daily'),d.days.slice().reverse(),true);
    });
  }else if(kind==='week'){
    openModal('使用时段 · 最近 30 天（按住拖动可左右查看）',body=>{
      body.innerHTML='<div class="week-bar"><span id="wk-label">加载中…</span>'
        +'<span class="wk-sum" id="wk-sum2"></span></div>'
        +'<div class="week-body" id="week-body">'
        +'<canvas id="week-axis"></canvas>'
        +'<div class="week-scroll" id="week-scroll"><canvas id="week-big"></canvas></div>'
        +'</div>'
        +'<div class="legend-heat" style="margin-top:10px">'
        +'<i style="background:rgba(24,95,165,.35)"></i>在线时段'
        +'<i style="background:rgba(24,95,165,.08)"></i>今天'
        +'<i style="background:#e5533d;height:2px;border-radius:1px"></i>当前时间'
        +'</div>';
      fetch('/week?days=30',{cache:'no-store'}).then(r=>r.json()).then(w=>{
        weekCur=w.start;
        $('wk-label').textContent=w.start+' ~ '+w.end;
        $('wk-sum2').textContent='合计 '+fmtDur(w.total_s)
          +' · 活跃 '+hourLabel(w.range.from)+'–'+hourLabel(w.range.to);
        const host=body.querySelector('#week-body');
        drawWeekWide(host,w);
        fitOn(host.querySelector('#week-axis'),()=>drawWeekWide(host,w));
      });
    });
  }
}
async function loadCurveRange(cv,hours){
  const st=$('curve-stats');
  if(st)st.textContent='加载中…';
  const h=await fetch('/history?hours='+hours*24+'&max=1500',{cache:'no-store'}).then(r=>r.json());
  cv._pts=h.points;
  drawCurve(cv,h.points,true);
  fitOn(cv,()=>{if(cv._pts)drawCurve(cv,cv._pts,true);});
  // 弹窗刚插入时容器还没定宽，等一帧再按真实宽度重绘
  requestAnimationFrame(()=>requestAnimationFrame(()=>{if(cv._pts)drawCurve(cv,cv._pts,true);}));
  const lv=h.stats||{};
  if(st)st.textContent='最低 '+(lv.min??'--')+'% · 最高 '+(lv.max??'--')+'% · '+h.count+' 个采样点（已降采样）';
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
const _q=new URLSearchParams(location.search);
const _zoomRaw=_q.get('zoom');
const _zoom=_zoomRaw==='usage'?'week':_zoomRaw;   // 兼容旧链接
const _zoomR=_q.get('range');
if(_zoom)setTimeout(()=>zoom(_zoom,_zoomR),400);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_host_parses_scheme_and_port() {
        assert_eq!(origin_host("http://127.0.0.1:8731"), Some("127.0.0.1"));
        assert_eq!(origin_host("http://localhost"), Some("localhost"));
        assert_eq!(origin_host("https://[::1]:8731"), Some("[::1]"));
        assert_eq!(origin_host("null"), None);
    }

    #[test]
    fn origin_host_keeps_attacker_suffix() {
        // 这俩就是旧 contains 校验的绕过样本：host 不等于回环地址
        assert!(!is_loopback_host(
            origin_host("http://127.0.0.1.nip.io").unwrap()
        ));
        assert!(!is_loopback_host(
            origin_host("http://localhost.evil.com").unwrap()
        ));
    }

    #[test]
    fn loopback_hosts_are_accepted() {
        for h in ["127.0.0.1", "localhost", "LOCALHOST", "[::1]", "::1"] {
            assert!(is_loopback_host(h), "{h} 应视为回环");
        }
    }
}
