//! 本地 HTTP 接口。所有处理器只读内存/磁盘，不触发 HID 读取。

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::{Local, NaiveDate};
use serde_json::{json, Value};
use tiny_http::{Header, Method, Response, Server};

use crate::app::App;
use crate::config::Config;
use crate::history::History;
use crate::util;

type Body = Response<Cursor<Vec<u8>>>;

pub fn serve(app: Arc<App>) -> Result<()> {
    let addr = format!("{}:{}", app.cfg.bind, app.cfg.port);
    let server = Server::http(&addr).map_err(|e| anyhow!("监听 {addr} 失败：{e}"))?;

    println!(
        "hidread 已启动：http://{addr}  tick={}s  vid={:04X}  pids={:?}  data={}",
        app.cfg.tick_s,
        app.cfg.vid,
        app.cfg.pids.iter().map(|p| format!("{p:04X}")).collect::<Vec<_>>(),
        app.cfg.data_dir.display()
    );

    for request in server.incoming_requests() {
        let path = request.url().to_string();
        let (route, q) = split_query(&path);

        if request.method() != &Method::Get {
            let _ = request.respond(json_resp(405, &json!({"error": "只支持 GET"})));
            continue;
        }

        let response = match route {
            "/" => html_resp(index_html()),
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
            "/chart" => {
                let hours = int_param(&q, "hours", 24, 1, 24 * 30);
                html_resp(chart_html(hours))
            }
            _ => json_resp(
                404,
                &json!({"error": "未知路径", "paths": [
                    "/battery", "/health", "/raw", "/session", "/sessions", "/daily",
                    "/history", "/history.csv", "/daily.csv", "/events", "/cycles", "/chart"
                ]}),
            ),
        };

        if let Err(e) = request.respond(response) {
            eprintln!("响应失败：{e}");
        }
    }

    Ok(())
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

fn index_html() -> String {
    let mut rows = String::new();
    for (path, desc) in [
        ("/battery", "当前电量（主接口）"),
        ("/health", "存活状态"),
        ("/session", "本次在线状态"),
        ("/sessions", "当日会话明细"),
        ("/daily", "每日电量/在线表"),
        ("/history", "历史曲线数据"),
        ("/history.csv", "导出历史 CSV"),
        ("/daily.csv", "导出每日表 CSV"),
        ("/events", "唤醒/休眠事件"),
        ("/cycles", "放电周期与续航"),
        ("/raw", "原始帧（排障）"),
        ("/chart", "折线图"),
    ] {
        rows.push_str(&format!(
            "<tr><td><a href=\"{path}\">{path}</a></td><td>{desc}</td></tr>"
        ));
    }

    format!(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\">\
         <title>hidread</title>\
         <style>body{{font:14px/1.6 system-ui,sans-serif;max-width:720px;margin:40px auto;padding:0 16px;color:#111}}\
         table{{border-collapse:collapse;width:100%}}td{{padding:6px 8px;border-bottom:1px solid #eee}}\
         a{{color:#185fa5;text-decoration:none}}code{{background:#f5f5f5;padding:2px 4px;border-radius:4px}}</style>\
         <h2>hidread</h2><p>ATK 2.4G 鼠标电量桥接服务</p>\
         <table>{rows}</table></body></html>"
    )
}

fn chart_html(hours: i64) -> String {
    format!(
        r#"<!doctype html><html lang="zh-CN"><meta charset="utf-8"><title>hidread 电量</title>
<style>
body{{font:13px/1.5 system-ui,sans-serif;margin:24px;color:#111;background:#fff}}
canvas{{width:100%;height:320px;border:1px solid #dde3ea;border-radius:8px}}
.legend{{color:#5b6b7c;margin:8px 0}}
</style>
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

#[allow(dead_code)]
fn today_str() -> String {
    NaiveDate::from(Local::now().date_naive())
        .format("%Y-%m-%d")
        .to_string()
}
