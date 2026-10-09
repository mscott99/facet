// §7.1 zoom and date, served from the engine process over MCP (streamable HTTP, JSON replies)
// on a loopback port with a random secret in the path (§9: "MCP over HTTP on a local port").
// The tool list is a constant: it is part of every cached prefix.
//
// The master also gets the output tools (README, "Stream and venues"): send_chat, its only way
// to speak in the chat venue, and the card tools (new_card, answer_card, fix_card, close_card;
// list_cards only reads), its way to speak on a line of a note.
// A detached subagent (`facet spawn`) is pointed at a second path of the same server, which
// lists and serves zoom and date only: a subagent reports, it never speaks to the user. Task
// subagents share the master's connection; their definitions (prompts::agents) leave the two
// output tools out of their tool list.
use super::engine::Engine;
use super::{flat, prompts};
use serde_json::{json, Value};
use std::io::Read;
use std::sync::Arc;

pub fn tools(full: bool) -> Value {
    let mut t = json!([
        {"name": "zoom", "description": prompts::ZOOM_DOC,
         "inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}, "n": {"type": "integer"}}, "required": ["id", "n"]}},
        {"name": "date", "description": prompts::DATE_DOC,
         "inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}}
    ]);
    if full {
        let a = t.as_array_mut().unwrap();
        a.push(json!({"name": "send_chat", "description": prompts::SEND_DOC,
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}));
        let st = json!({"type": "string"});
        a.push(json!({"name": "new_card", "description": prompts::NEW_CARD_DOC,
            "inputSchema": {"type": "object", "properties": {"note": st, "anchor": st, "line": {"type": "integer"},
                "text": st, "kind": {"type": "string", "enum": ["comment", "info", "warn", "error"]}, "fix": st},
                "required": ["note", "text"]}}));
        a.push(json!({"name": "answer_card", "description": prompts::ANSWER_DOC,
            "inputSchema": {"type": "object", "properties": {"id": st, "text": st, "fix": st}, "required": ["id", "text"]}}));
        a.push(json!({"name": "fix_card", "description": prompts::FIX_CARD_DOC,
            "inputSchema": {"type": "object", "properties": {"id": st, "fix": st}, "required": ["id", "fix"]}}));
        a.push(json!({"name": "close_card", "description": prompts::CLOSE_CARD_DOC,
            "inputSchema": {"type": "object", "properties": {"id": st, "delete": {"type": "boolean"}}, "required": ["id"]}}));
        a.push(json!({"name": "list_cards", "description": prompts::LIST_CARDS_DOC,
            "inputSchema": {"type": "object", "properties": {"note": st}}}));
    }
    t
}

/// What a successful output-tool call returns (or starts with: a new card's result goes on to
/// give its id). The turn loop leaves the call and this result out of the log (the `chat` or
/// `answer` message already says it); anything else, an error, is logged as an echo so a
/// failed send is remembered.
pub const SENT: &str = "sent";
pub const ANSWERED: &str = "answered";

/// The card tools that speak (and so log themselves); `list_cards` only reads.
pub fn card_action(tool: &str) -> Option<&'static str> {
    match tool {
        "new_card" => Some("new"), "answer_card" => Some("reply"), "fix_card" => Some("fix"),
        "close_card" => Some("close"), "list_cards" => Some("list"), _ => None,
    }
}

/// The path a detached subagent is given: the same server, read-only tools.
pub fn agent_url(master: &str) -> String {
    match master.strip_suffix("/mcp") { Some(b) => format!("{}/agent", b), None => master.to_string() }
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
    let sec = secret();
    let path = format!("/{}/mcp", sec);
    let agent = format!("/{}/agent", sec);
    let url = format!("http://127.0.0.1:{}{}", port, path);
    std::thread::spawn(move || {
        for mut req in server.incoming_requests() {
            let full = req.url() == path;
            if !full && req.url() != agent {
                let _ = req.respond(tiny_http::Response::empty(404));
                continue;
            }
            if *req.method() != tiny_http::Method::Post {
                let _ = req.respond(tiny_http::Response::empty(405));
                continue;
            }
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            let reply = serde_json::from_str::<Value>(&body).ok().and_then(|v| handle(&e, &v, full));
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
fn handle(e: &Arc<Engine>, v: &Value, full: bool) -> Option<Value> {
    let id = v.get("id")?.clone();
    let method = v["method"].as_str().unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": v["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "optchat", "version": "1"}
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools(full)}),
        "tools/call" => {
            let a = &v["params"]["arguments"];
            let num = |k: &str| a[k].as_i64().or_else(|| a[k].as_str().and_then(|s| s.trim().parse().ok()));
            let st = |k: &str| a[k].as_str().map(String::from).or_else(|| a[k].as_i64().map(|x| x.to_string())).unwrap_or_default();
            let (text, err) = match v["params"]["name"].as_str().unwrap_or("") {
                "zoom" => (match (num("id"), num("n")) { (Some(i), Some(n)) => zoom(&e.mem.lock().unwrap().store, i, n), _ => "zoom needs id and n.".into() }, false),
                "date" => (match num("id") { Some(i) => date(&e.mem.lock().unwrap().store, i), None => "date needs id.".into() }, false),
                // the mem lock is not held here: logging takes it
                "send_chat" if full => match super::engine::chat(e, &st("text")) {
                    Ok(()) => (SENT.into(), false),
                    Err(x) => (format!("send_chat failed: {}", x), true),
                },
                t if full && card_action(t).is_some() => {
                    let mut q = a.clone();
                    if !q.is_object() { q = json!({}); }
                    q["do"] = card_action(t).unwrap().into();
                    if t == "close_card" && a["delete"] == true { q["do"] = "delete".into(); }
                    match super::engine::card(e, &q) {
                        Ok(r) if t == "list_cards" => (r["text"].as_str().unwrap_or("").to_string(), false),
                        Ok(r) if t == "new_card" => (format!("{}: card {} at L{}", ANSWERED, r["id"].as_str().unwrap_or(""), r["line"]), false),
                        Ok(_) => (ANSWERED.into(), false),
                        Err(x) => (format!("{} failed: {}", t, x), true),
                    }
                }
                other => (format!("No tool {}.", other), true),
            };
            json!({"content": [{"type": "text", "text": text}], "isError": err})
        }
        _ => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_detached_agent_gets_the_reading_tools_only() {
        assert_eq!(agent_url("http://127.0.0.1:5/abc/mcp"), "http://127.0.0.1:5/abc/agent");
        let names = |t: Value| t.as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(names(tools(false)), ["zoom", "date"]);
        assert_eq!(names(tools(true)), ["zoom", "date", "send_chat", "new_card", "answer_card", "fix_card", "close_card", "list_cards"]);
        let a = crate::optchat::prompts::agents("X", "sonnet", "<chat></chat>");
        assert!(!a.contains("send_chat") && !a.contains("answer_card") && !a.contains("new_card"));
    }

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
