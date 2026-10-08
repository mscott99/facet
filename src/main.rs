// Facet — the agent, seen whole: a memory (the OptChat log and the engine that writes it),
// and the input and output through which it meets the world. This binary is the second half;
// the memory lives in its own process today, which is an implementation detail and not a
// division in the thing itself.
//
//   log   the memory: everything that happened, input and output, in order
//   doc   output addressed by a slug: a note of the vault, published
//   diag  output addressed by a code and anchored to a live line: a comment to be triaged
//   cards a line-comment's id, and the deliberate answer addressed to it
//   tell  input: one funnel, from any route, into the memory
//   web   the route with a screen            tg   the route with push
//
// Anything that is not one of those is an adapter.
mod cards;
mod cfg;
mod diag;
mod doc;
mod log;
mod md;
mod optchat;
mod tell;
mod tui;
mod tg;
mod web;

use cfg::Cfg;
use std::path::PathBuf;

fn sh(args: &[&str]) -> String {
    std::process::Command::new(args[0]).args(&args[1..]).output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}
/// Dates are the one thing not worth a dependency: `date` knows the local zone.
pub fn when(epoch: u64) -> String {
    if epoch == 0 { return "—".into() }
    // GNU date takes `-d @epoch`; BSD (macOS) does not, and wants `-r epoch`.
    let gnu = sh(&["/bin/date", "-d", &format!("@{}", epoch), "+%d %b %H:%M"]);
    if !gnu.is_empty() { gnu } else { sh(&["/bin/date", "-r", &epoch.to_string(), "+%d %b %H:%M"]) }
}
pub fn today() -> String { sh(&["/bin/date", "+%Y-%m-%d"]) }
pub fn stamp() -> String { sh(&["/bin/date", "+%Y%m%d-%H%M%S"]) }

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");
    let rest = &args[1..];
    let cfg = Cfg::load();

    match cmd {
        "serve" => web::serve(cfg),

        // the memory: the engine, and the terminal route into it
        "chat" => tui::run(),
        "engine" => optchat::engine::serve(),
        "view" => {
            let dir = optchat::engine::dir();
            match optchat::engine::request(&dir, serde_json::json!({"op": "view"})) {
                Ok(v) => println!("{}", v["view"].as_str().unwrap_or("")),
                Err(_) => { // no engine: the view is a fold of the files
                    let s = optchat::store::Store::open(&dir);
                    println!("{}", optchat::view::View::fold(&s, optchat::VIEW).render(&s));
                }
            }
        }
        "browse" => {
            let out = PathBuf::from(rest.first().map(|s| s.as_str()).unwrap_or("memory.html"));
            let dir = optchat::engine::dir();
            let s = optchat::store::Store::open(&dir);
            let v = optchat::view::View::fold(&s, optchat::VIEW);
            match std::fs::write(&out, optchat::browse::html(&s, &v, optchat::VIEW, None)) {
                Ok(()) => println!("{}", out.display()), Err(e) => die(&e.to_string()) }
        }
        "import" => {
            if rest.is_empty() { die("facet import <file>...") }
            for l in optchat::import::files(rest) { println!("{}", l); }
        }
        "stats" => print!("{}", optchat::usage::table(&optchat::engine::dir())),
        "cancel" => match optchat::engine::request(&optchat::engine::dir(), serde_json::json!({"op": "cancel"})) {
            Ok(_) => println!("cancelled"), Err(e) => die(&e) },

        "init" => init(),

        "post" => {
            if rest.is_empty() { die("facet post <file> [slug]") }
            match doc::post(&cfg, &PathBuf::from(&rest[0]), rest.get(1).map(|s| s.as_str())) {
                Ok(slug) => println!("{}\n{}", slug, cfg.url(&format!("/m/{}", slug))),
                Err(e) => die(&e),
            }
        }
        "unpost" => match doc::unpost(&cfg, rest.first().map(|s| s.as_str()).unwrap_or("")) {
            Ok(()) => println!("unposted"),
            Err(e) => die(&e),
        },
        "docs" => for (slug, path) in doc::table(&cfg) {
            println!("{:<16} {:<60} {}", slug, cfg.url(&format!("/m/{}", slug)), path.display());
        },

        "send" => match tell::tell(&cfg, &rest.iter().filter(|a| *a != "--later").cloned().collect::<Vec<_>>().join(" "),
                                   "cli", rest.iter().any(|a| a == "--later")) {
            Ok(line) => println!("{}", line),
            Err(e) => die(&e),
        },
        // to Telegram only; with --log it also lands in the chat log as a `note`, which starts no turn
        // (queued like an import while a turn runs). A log failure after a good push is a warning, not an
        // error, so a caller never pushes the same message twice.
        "push" => {
            let log = rest.iter().any(|a| a == "--log");
            let text = rest.iter().filter(|a| *a != "--log").cloned().collect::<Vec<_>>().join(" ");
            match tg::push(&cfg, &text) {
                Ok(()) => println!("pushed"),
                Err(e) => die(&e),
            }
            if log {
                match optchat::engine::request(&optchat::engine::dir(), serde_json::json!({"op": "note", "text": text})) {
                    Ok(v) if v["ok"].as_bool() == Some(true) => println!("logged"),
                    Ok(v) => eprintln!("pushed, NOT logged: {}", v["error"].as_str().unwrap_or("refused")),
                    Err(e) => eprintln!("pushed, NOT logged: {}", e),
                }
            }
        }

        // a subagent that outlives this turn (§9, detached): returns at once; its report
        // arrives later, as a message of its own
        "spawn" => {
            let (mut model, mut kind, mut desc) = (String::new(), String::new(), String::new());
            let mut words = Vec::new();
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--model" => { model = rest.get(i + 1).cloned().unwrap_or_default(); i += 2; }
                    "--kind" => { kind = rest.get(i + 1).cloned().unwrap_or_default(); i += 2; }
                    "--desc" => { desc = rest.get(i + 1).cloned().unwrap_or_default(); i += 2; }
                    w => { words.push(w.to_string()); i += 1; }
                }
            }
            let task = words.join(" ");
            match optchat::engine::request(&optchat::engine::dir(),
                serde_json::json!({"op": "spawn", "model": model, "kind": kind, "desc": desc, "task": task})) {
                Ok(v) if v["ok"].as_bool() == Some(true) => println!("{}", v["id"].as_str().unwrap_or("")),
                Ok(v) => die(v["error"].as_str().unwrap_or("refused")),
                Err(e) => die(&e),
            }
        }

        // a deliberate reply to a line-comment card (§ the viewer's cards), never a talk
        // reply the card's poll happens to catch: the id comes from the card's own message
        "answer" => {
            if rest.len() < 2 { die("facet answer <id> <text> [--apply <replacement>]") }
            let id = rest[0].clone();
            let (mut apply, mut words, mut i) = (String::new(), Vec::new(), 1);
            while i < rest.len() {
                match rest[i].as_str() {
                    "--apply" => { apply = rest.get(i + 1).cloned().unwrap_or_default(); i += 2; }
                    w => { words.push(w.to_string()); i += 1; }
                }
            }
            let mut req = serde_json::json!({"op": "answer", "id": id, "text": words.join(" ")});
            if !apply.trim().is_empty() { req["apply"] = apply.into(); }
            match optchat::engine::request(&optchat::engine::dir(), req) {
                Ok(v) if v["ok"].as_bool() == Some(true) => println!("{}", match v["code"].as_str() {
                    Some(c) => format!("answered; fix {} ready to apply", c), None => "answered".into() }),
                Ok(v) => die(v["error"].as_str().unwrap_or("refused")),
                Err(e) => die(&e),
            }
        }

        // queue a restart of the engine (re-exec) for when the reply ends and no spawn is alive
        "restart" => match optchat::engine::request(&optchat::engine::dir(),
            serde_json::json!({"op": "restart", "serve": rest.iter().any(|a| a == "--serve")})) {
            Ok(v) if v["ok"].as_bool() == Some(true) => println!("restart queued"),
            Ok(v) => die(v["error"].as_str().unwrap_or("refused")),
            Err(e) => die(&e),
        },

        "diag" => print!("{}", diag::brief(&cfg)),
        // one call, N comments: anchors are verbatim text, the binary finds the lines
        "review" => {
            let replace = rest.iter().any(|a| a == "--replace");
            let note = rest.iter().find(|a| !a.starts_with("--")).cloned().unwrap_or_default();
            let mut spec = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut spec).ok();
            match diag::review(&cfg, &note, &spec, replace) {
                Ok(m) => println!("{}", m), Err(e) => die(&e),
            }
        }
        "apply" => match diag::apply(&cfg, rest.first().map(|s| s.as_str()).unwrap_or("")) {
            Ok(m) => println!("{}", m), Err(e) => die(&e) },
        "dismiss" => match diag::dismiss(&cfg, rest.first().map(|s| s.as_str()).unwrap_or(""), &rest[1..].join(" ")) {
            Ok(m) => println!("{}", m), Err(e) => die(&e) },

        "url" => println!("{}", cfg.url(&rest.first().map(|s| format!("/m/{}", s)).unwrap_or("/".into()))),

        "status" => {
            println!("home       {}", cfg.url("/"));
            println!("memory     {} · {} messages", cfg.store().display(), log::last(&cfg) + 1);
            println!("vault      {}", cfg.vault().display());
            match optchat::engine::request(&optchat::engine::dir(), serde_json::json!({"op": "status"})) {
                Ok(v) => println!("engine     {}", v),
                Err(e) => println!("engine     DOWN ({})", e),
            }
            println!("telegram   {}", match cfg.num("telegram.chat_id", 0) {
                0 => "unpaired".to_string(), c => format!("chat {}", c) });
            println!("notes      {}", doc::table(&cfg).len());
            println!("comments   {}", diag::all(&cfg).len());
            println!("queued     {} command(s) riding along", tg::queued());
        }

        _ => println!("{}", HELP),
    }
}

const HELP: &str = "\
facet chat                  the conversation in this terminal (starts the engine if needed)
facet engine                run the engine in the foreground (LaunchAgent: launchd/com.facet.engine.plist)
facet view                  print the view the model sees
facet browse [out.html]     the whole memory tree as one page (also served at /tree)
facet import <file>...      add files to the memory, one note each (queued until a running turn is done)
facet stats                 token usage per day and kind
facet cancel                stop the running turn
facet serve                 the web route, plus Telegram if configured
facet init                  write ~/.config/facet/facet.json from what is already here
facet post <file> [slug]    publish a note (writes `facet: slug` into its frontmatter)
facet unpost <slug>         unpublish
facet docs                  what is published
facet send [--later] <text> put a message into the conversation (--later: a turn of its own,
                            after the running one, instead of at its next tool call)
facet push [--log] <text>   push a message to Telegram; --log also adds it to the chat log as a note (no turn)
facet spawn [--model M] [--kind general-purpose|explore] [--desc D] <task>
                            a subagent that outlives this turn: returns its id at once; its
                            report arrives later, as a message of its own starting \"[id] \"
facet review <note> [--replace] < spec
                            write comments on a note; the spec is
                              @ verbatim anchor text (must be unique in the note)
                              ! [severity] message
                              ? long explanation, markdown + math (optional, multi-line)
                              + replacement for the anchored line (optional, multi-line)
facet diag                  open comments
facet apply|dismiss <code>  triage one
facet answer <id> <text> [--apply <replacement>]
                            answer a line-comment card by id (from its message, shaped
                            `[[Note]] L<n> #<id>: \"quote\"`); --apply only when you mean the
                            line itself replaced, which the card can then apply
facet status                where everything stands";

fn die(m: &str) -> ! { eprintln!("{}", m); std::process::exit(1) }

/// First run: take what the old scripts already had rather than asking for it again.
fn init() {
    let mut c = Cfg::load();
    let old = cfg::home().join(".config/optchat-web");
    let get = |f: &str| std::fs::read_to_string(old.join(f)).ok().map(|s| s.trim().to_string());
    if c.opt("token").is_none() {
        if let Some(t) = get("token") { c.set("token", t.into()); }
    }
    if c.opt("telegram.bot_token").is_none() {
        if let Some(v) = get("telegram.json").and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
            if let Some(t) = v["bot_token"].as_str() { c.set("telegram.bot_token", t.into()); }
            if let Some(i) = v["chat_id"].as_i64() { c.set("telegram.chat_id", i.into()); }
        }
    }
    for (k, v) in [("store", "~/.optchat"), ("vault", "~/Obsidian/myVault"),
                   ("host", "127.0.0.1"), ("input.mode", "tmux"),
                   ("input.tmux_target", "TMUX:1.0"), ("input.tmux", "tmux"),
                   ("life", "~/.local/bin/life"), ("review_memory", "LLM/Review memory.md")] {
        if c.opt(k).is_none() { c.set(k, v.into()); }
    }
    if c.0.get("port").is_none() { c.set("port", 8730.into()); }
    c.save();
    println!("wrote {}", cfg::dir().join("facet.json").display());
}
