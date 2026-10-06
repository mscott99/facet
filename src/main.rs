// Facet — the agent, seen whole: a memory (the OptChat log and the engine that writes it),
// and the input and output through which it meets the world. This binary is the second half;
// the memory lives in its own process today, which is an implementation detail and not a
// division in the thing itself.
//
//   log   the memory: everything that happened, input and output, in order
//   doc   output addressed by a slug: a note of the vault, published
//   diag  output addressed by a code and anchored to a live line: a comment to be triaged
//   tell  input: one funnel, from any route, into the memory
//   web   the route with a screen            tg   the route with push
//
// Anything that is not one of those is an adapter.
mod cfg;
mod diag;
mod doc;
mod log;
mod md;
mod tell;
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
    sh(&["/bin/date", "-r", &epoch.to_string(), "+%d %b %H:%M"])
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

        "send" => match tell::tell(&cfg, &rest.join(" "), "cli") {
            Ok(line) => println!("{}", line),
            Err(e) => die(&e),
        },
        "push" => match tg::push(&cfg, &rest.join(" ")) {
            Ok(()) => println!("pushed"),
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
            println!("reader     {}", cfg.url("/"));
            println!("memory     {} · {} messages", cfg.store().display(), log::last(&cfg) + 1);
            println!("vault      {}", cfg.vault().display());
            println!("input      {} {} ({})", cfg.str("input.mode", "tmux"),
                cfg.str("input.tmux_target", "TMUX:1.0"),
                if tell::healthy(&cfg) { "alive" } else { "DOWN" });
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
facet serve                 the web route, plus Telegram if configured
facet init                  write ~/.config/facet/facet.json from what is already here
facet post <file> [slug]    publish a note (writes `facet: slug` into its frontmatter)
facet unpost <slug>         unpublish
facet docs                  what is published
facet send <text>           put a message into the conversation
facet push <text>           push a message to Telegram
facet review <note> [--replace] < spec
                            write comments on a note; the spec is
                              @ verbatim anchor text (must be unique in the note)
                              ! [severity] message
                              ? long explanation, markdown + math (optional, multi-line)
                              + replacement for the anchored line (optional, multi-line)
facet diag                  open comments
facet apply|dismiss <code>  triage one
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
                   ("input.tmux_target", "TMUX:1.0"), ("input.tmux", "/opt/homebrew/bin/tmux"),
                   ("life", "~/.local/bin/life"), ("review_memory", "LLM/Review memory.md")] {
        if c.opt(k).is_none() { c.set(k, v.into()); }
    }
    if c.0.get("port").is_none() { c.set("port", 8730.into()); }
    c.save();
    println!("wrote {}", cfg::dir().join("facet.json").display());
}
