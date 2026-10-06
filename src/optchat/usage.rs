// One line per API request in <dir>/usage.jsonl: {date, kind, model, usage}. kind is
// compact | prime | turn. The same format the previous engine wrote, so its history counts.
//
// "eq" is the request's cost in input-token equivalents at API price ratios:
// in + 0.1 read + 1.25 write(5m) + 2 write(1h) + 5 out. On a subscription it is a proxy for
// how fast the limit is used, not money; it is comparable within one model only.
use super::claude::Req;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

pub fn eq(u: &Value) -> f64 {
    let g = |k: &str| u[k].as_f64().unwrap_or(0.0);
    let (w5, w1) = match u.get("cache_creation") {
        Some(c) if c.is_object() => (c["ephemeral_5m_input_tokens"].as_f64().unwrap_or(0.0), c["ephemeral_1h_input_tokens"].as_f64().unwrap_or(0.0)),
        _ => (g("cache_creation_input_tokens"), 0.0),
    };
    g("input_tokens") + 0.1 * g("cache_read_input_tokens") + 1.25 * w5 + 2.0 * w1 + 5.0 * g("output_tokens")
}

pub fn record(dir: &Path, kind: &str, r: &Req) {
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let line = format!("{}\n", json!({"date": super::store::now_iso(), "kind": kind, "model": r.model, "usage": r.usage}));
    let _g = ONE.lock().unwrap();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("usage.jsonl")) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Tables: per day (last 14) and per kind/model, from the usage log.
pub fn table(dir: &Path) -> String {
    let body = std::fs::read_to_string(dir.join("usage.jsonl")).unwrap_or_default();
    // (day, kind) -> [requests, in, read, write, out, eq]
    let mut by: BTreeMap<(String, String), [f64; 6]> = BTreeMap::new();
    for line in body.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let u = &v["usage"];
        let day = v["date"].as_str().unwrap_or("").chars().take(10).collect::<String>();
        let model = v["model"].as_str().unwrap_or("?").replace("claude-", "");
        let k = format!("{} {}", v["kind"].as_str().unwrap_or("?"), model);
        let e = by.entry((day, k)).or_default();
        let g = |k: &str| u[k].as_f64().unwrap_or(0.0);
        e[0] += 1.0; e[1] += g("input_tokens"); e[2] += g("cache_read_input_tokens");
        e[3] += g("cache_creation_input_tokens"); e[4] += g("output_tokens"); e[5] += eq(u);
    }
    let days: Vec<String> = { let mut d: Vec<String> = by.keys().map(|k| k.0.clone()).collect(); d.dedup(); d };
    let keep: Vec<String> = days.iter().rev().take(14).cloned().collect();
    let mut out = format!("{:<10} {:<20} {:>6} {:>9} {:>11} {:>10} {:>8} {:>10}\n", "day", "kind model", "reqs", "input", "cache read", "cache wr", "output", "eq");
    let m = |x: f64| if x >= 1e6 { format!("{:.2}M", x / 1e6) } else if x >= 1e3 { format!("{:.1}k", x / 1e3) } else { format!("{:.0}", x) };
    for ((day, k), e) in &by {
        if !keep.contains(day) { continue }
        out.push_str(&format!("{:<10} {:<20} {:>6} {:>9} {:>11} {:>10} {:>8} {:>10}\n", day, k, e[0], m(e[1]), m(e[2]), m(e[3]), m(e[4]), m(e[5])));
    }
    out
}

/// The subscription's limits as Claude Code reports them (`rate_limit_event`), in one line:
/// "session 88% left · resets 15:00 · week 72% left". Empty if nothing is known yet.
pub fn limits_line(info: &Value) -> String {
    let w = &info["unifiedWindows"];
    let when = |t: i64| {
        use chrono::TimeZone;
        let Some(at) = chrono::Local.timestamp_opt(t, 0).single() else { return String::new() };
        let now = chrono::Local::now();
        if at.date_naive() == now.date_naive() { at.format("%H:%M").to_string() }
        else if (at - now).num_days() < 6 { at.format("%a %H:%M").to_string() }
        else { at.format("%b %-d").to_string() }
    };
    let left = |u: f64| format!("{}% left", ((1.0 - u) * 100.0).round().max(0.0));
    let mut parts = Vec::new();
    if let Some(u) = w["five_hour"]["utilization"].as_f64() {
        parts.push(format!("session {}", left(u)));
        if let Some(t) = w["five_hour"]["resetsAt"].as_i64() { parts.push(format!("resets {}", when(t))); }
    }
    if let Some(u) = w["seven_day"]["utilization"].as_f64() { parts.push(format!("week {}", left(u))); }
    match info["status"].as_str() {
        Some("rejected") => {
            let t = info["resetsAt"].as_i64().map(|t| format!(" until {}", when(t))).unwrap_or_default();
            parts.insert(0, format!("LIMIT REACHED{}", t));
        }
        Some("allowed_warning") => parts.insert(0, "near the limit".into()),
        _ => {}
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn limits_line_reads_the_event() {
        let now = chrono::Local::now().timestamp();
        let v: serde_json::Value = serde_json::json!({"status": "allowed", "unifiedWindows": {
            "five_hour": {"utilization": 0.12, "resetsAt": now + 3600},
            "seven_day": {"utilization": 0.28, "resetsAt": now + 3 * 86400}}});
        let l = super::limits_line(&v);
        assert!(l.starts_with("session 88% left · resets "), "{}", l);
        assert!(l.ends_with(" · week 72% left"), "{}", l);
        assert_eq!(super::limits_line(&serde_json::json!({})), "");
    }
}
