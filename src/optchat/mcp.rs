// §7.1 zoom and date, served from the engine process over MCP (streamable HTTP, JSON replies)
// on a loopback port with a random secret in the path (§9: "MCP over HTTP on a local port").
// The tool list is a constant: it is part of every cached prefix.
use super::engine::Engine;
use super::{flat, prompts};
use serde_json::{json, Value};
use std::io::Read;
use std::sync::Arc;

pub fn tools() -> Value {
    json!([
        {"name": "zoom", "description": prompts::ZOOM_DOC,
         "inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}, "n": {"type": "integer"}}, "required": ["id", "n"]}},
        {"name": "date", "description": prompts::DATE_DOC,
         "inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}}
    ])
}

/// zoom(id, n): n = 1 gives the message whole; otherwise the two lines under id+n.
pub fn zoom(s: &super::store::Store, id: i64, n: i64) -> String {
    let t = s.t() as i64;
    let none = format!("No line {}+{}.", id, n);
    if id < 0 || n < 1 || n & (n - 1) != 0 || id % n != 0 || id + n > t { return none }
    let (id, n) = (id as usize, n as usize);
    if n == 1 {
        let m = &s.msgs[id];
        return format!("{}+0|{}: {}", id, m.kind, m.text);
    }
    let l = n.trailing_zeros() as usize - 1;
    let i = id / (n / 2);
    let line = |k: usize| format!("{}+{}|{}", k << l, 1usize << l,
        flat(s.node(l, k).unwrap_or(super::view::PLACEHOLDER)));
    format!("{}\n{}", line(i), line(i + 1))
}

pub fn date(s: &super::store::Store, id: i64) -> String {
    if id < 0 || id as usize >= s.t() { return format!("No message {}.", id) }
    let d = &s.msgs[id as usize].date;
    match chrono::DateTime::parse_from_rfc3339(d) {
        Ok(x) => x.with_timezone(&chrono::Local).format("%a %Y-%m-%d %H:%M:%S %z").to_string(),
        Err(_) => d.clone(),
    }
}

fn secret() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") { let _ = f.read_exact(&mut b); }
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

/// Start the server; returns the URL to give Claude Code.
pub fn start(e: Arc<Engine>) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("mcp server");
    let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
    let path = format!("/{}/mcp", secret());
    let url = format!("http://127.0.0.1:{}{}", port, path);
    std::thread::spawn(move || {
        for mut req in server.incoming_requests() {
            if req.url() != path {
                let _ = req.respond(tiny_http::Response::empty(404));
                continue;
            }
            if *req.method() != tiny_http::Method::Post {
                let _ = req.respond(tiny_http::Response::empty(405));
                continue;
            }
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            let reply = serde_json::from_str::<Value>(&body).ok().and_then(|v| handle(&e, &v));
            let resp = match reply {
                None => tiny_http::Response::from_string("").with_status_code(202),
                Some(r) => tiny_http::Response::from_string(r.to_string()).with_header(
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()),
            };
            let _ = req.respond(resp);
        }
    });
    url
}

/// One JSON-RPC message; None for a notification.
fn handle(e: &Engine, v: &Value) -> Option<Value> {
    let id = v.get("id")?.clone();
    let method = v["method"].as_str().unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": v["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "optchat", "version": "1"}
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let a = &v["params"]["arguments"];
            let num = |k: &str| a[k].as_i64().or_else(|| a[k].as_str().and_then(|s| s.trim().parse().ok()));
            let m = e.mem.lock().unwrap();
            let text = match v["params"]["name"].as_str().unwrap_or("") {
                "zoom" => match (num("id"), num("n")) { (Some(i), Some(n)) => zoom(&m.store, i, n), _ => "zoom needs id and n.".into() },
                "date" => match num("id") { Some(i) => date(&m.store, i), None => "date needs id.".into() },
                other => format!("No tool {}.", other),
            };
            json!({"content": [{"type": "text", "text": text}]})
        }
        _ => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zoom_addresses() {
        let d = std::env::temp_dir().join(format!("facet-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mut s = crate::optchat::store::Store::open(&d);
        for k in 0..6 { s.log("user", &format!("m{}\nx", k)).unwrap(); }
        for k in 0..6 { s.put(0, k, &format!("user: m{}", k)).unwrap(); }
        s.put(1, 0, "user: m0\nuser: m1").unwrap();
        s.put(1, 1, "pair 2-3").unwrap();
        assert_eq!(zoom(&s, 3, 1), "3+0|user: m3\nx");
        assert_eq!(zoom(&s, 2, 2), "2+1|user: m2\n3+1|user: m3");
        assert_eq!(zoom(&s, 0, 4), "0+2|user: m0 user: m1\n2+2|pair 2-3");
        assert_eq!(zoom(&s, 4, 4), "No line 4+4.");
        assert_eq!(zoom(&s, 1, 2), "No line 1+2.");
        assert_eq!(zoom(&s, 0, 3), "No line 0+3.");
        assert_eq!(zoom(&s, 6, 1), "No line 6+1.");
        assert!(date(&s, 0).contains(" 20"), "{}", date(&s, 0));
    }
}
