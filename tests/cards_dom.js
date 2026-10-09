// The card component in a real page's DOM, against a running `facet serve` (driven by
// tests/engine_test.py when FACET_JSDOM points at a node_modules with jsdom in it):
//   node tests/cards_dom.js <base url> <token> <note name>
// Loads /n/<note> with its own inline script (htmx and KaTeX stubbed: they come from files and
// a CDN), prints READY once the page's cards are placed, then waits for a line on stdin — the
// test has the agent open a card meanwhile — and checks that the open page shows it without a
// reload, under the right line; then types a comment on a line and closes a card, as a reader.
const { JSDOM, VirtualConsole } = require("jsdom");
const [base, tok, note] = process.argv.slice(2);
let fails = 0;
const check = (c, what) => { console.log((c ? "ok   " : "FAIL ") + what); if (!c) fails++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(f, ms) { const t = Date.now(); while (Date.now() - t < ms) { const v = f(); if (v) return v; await sleep(100); } return null; }

(async () => {
  const html = await (await fetch(`${base}/${tok}/n/${encodeURIComponent(note)}`)).text();
  const errors = [];
  const vc = new VirtualConsole();
  vc.on("jsdomError", (e) => { if (!/Not implemented/.test(e.message)) errors.push(e.message); });
  const dom = new JSDOM(html, {
    url: `${base}/${tok}/n/${encodeURIComponent(note)}`, runScripts: "dangerously", pretendToBeVisual: true, virtualConsole: vc,
    beforeParse(w) {
      w.htmx = { process() {}, trigger() {} };
      w.fetch = (u, o) => fetch(new URL(u, `${base}/`).href, o);
      w.AbortController = AbortController;
      w.CSS = { escape: (s) => String(s).replace(/["\\]/g, "\\$&") };
      w.matchMedia = () => ({ matches: false });
      w.scrollTo = w.scrollBy = () => {};
    },
  });
  const w = dom.window, doc = w.document;
  await until(() => doc.readyState === "complete", 5000);
  await sleep(1500);
  const before = doc.querySelectorAll(".say").length;
  check(before >= 1, `the note's open cards are placed on load (${before})`);
  console.log("READY " + before);
  await new Promise((r) => process.stdin.once("data", r));

  // the agent opens a card on "line seven" while the page is open
  const fresh = await until(() => [...doc.querySelectorAll(".say.warn")].find((d) => d.dataset.line === "7"), 30000);
  check(!!fresh, "a card the agent opened shows on the open page, without a reload");
  if (fresh) {
    const prev = fresh.previousElementSibling;
    let b = prev; while (b && b.classList.contains("say")) b = b.previousElementSibling;
    check(b && b.dataset.line === "7", `it sits under its line (after L${b && b.dataset.line})`);
    check(fresh.querySelector(".th .msg.talk") && /x\^2/.test(fresh.querySelector(".th").textContent), "its message is in its thread");
    check(!fresh.querySelector(".fx").hidden && fresh.querySelector(".fx pre").textContent === "LINE FIVE" && fresh.querySelector(".fx .ap"), "its fix shows, with apply");
    check(fresh.querySelector(".hd .x") && fresh.querySelector(".cmp textarea").getAttribute("rows") === "1", "X at the top, a one-row box at the bottom");
  }

  // a reader double-clicks line three and writes on it
  const line3 = doc.querySelector('[data-note="Other Note"][data-line="3"]');
  check(!!line3, "line three is a block of its own");
  line3.dispatchEvent(new w.MouseEvent("dblclick", { bubbles: true }));
  const mine = doc.querySelector(".say:not([data-line='7'])[data-line='3']:last-of-type") || [...doc.querySelectorAll('.say[data-line="3"]')].pop();
  check(mine && mine.classList.contains("comment"), "a double-click opens a comment card under the line");
  mine.querySelector("textarea").value = "dom says hi";
  mine.querySelector(".go").click();
  check(mine.querySelector(".th .t.pend"), "what was sent shows grey at once");
  const firm = await until(() => { const t = mine.querySelector(".th .t"); return t && !t.classList.contains("pend") && t.dataset.k === "0" ? t : null; }, 10000);
  check(!!firm, "and firms up when the server has it");
  await sleep(2500); // the held request answers with the card the page just opened
  check(mine.querySelectorAll(".th .t").length === 1, "and is not shown twice");
  const cs = await (await fetch(`${base}/${tok}/f/cards?all=1`)).json();
  const kept = cs.cards.find((c) => c.id === mine.dataset.id);
  check(kept && kept.msgs.length === 1 && kept.by === "user", "the server keeps it as the user's card");

  // the X closes the agent's card
  if (fresh) {
    const id = fresh.dataset.id;
    fresh.querySelector(".hd .x").click();
    check(!doc.querySelector(`.say[data-id="${id}"]`), "the X takes the card off the page");
    await sleep(1500);
    const cs2 = await (await fetch(`${base}/${tok}/f/cards?all=1`)).json();
    check(!cs2.cards.find((c) => c.id === id), "and closes it on the server");
  }
  // closed elsewhere: the reader's own card, closed from the server side, leaves the page
  await fetch(`${base}/${tok}/x/card`, { method: "POST", headers: { "Content-Type": "application/x-www-form-urlencoded" }, body: "do=close&id=" + mine.dataset.id });
  check(await until(() => !doc.contains(mine), 30000), "a card closed elsewhere leaves the open page");
  check(errors.length === 0, "no script errors: " + JSON.stringify(errors));
  dom.window.close();
  process.exit(fails ? 1 : 0);
})().catch((e) => { console.log("FAIL exception " + e.stack); process.exit(1); });
