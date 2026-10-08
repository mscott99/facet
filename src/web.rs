// The web route: Facet's main face. Server-rendered HTML, HTMX for the three dynamic needs
// (append new messages, re-render a changed note, post a message), ~20 lines of JS for KaTeX
// and the Enter key. All markdown goes through one renderer; math is extracted by the parser,
// never by a regex.
use crate::cfg::Cfg;
use crate::{cards, diag, doc, log, md, tell};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tiny_http::{Header, Request, Response, Server};

/// How long a waiting request (`?wait=1`) holds before it answers "nothing yet". Short enough
/// that a dead connection frees its thread soon; the page just asks again.
const LONG: Duration = Duration::from_secs(25);

/// Every POST takes this for its whole run: they read-modify-write small files (cards.json,
/// diagnostics.json) that two simultaneous requests could otherwise clobber.
static POSTING: Mutex<()> = Mutex::new(());

const SHELL: &str = r#"<!DOCTYPE html><html><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1,viewport-fit=cover">
<title>{{TITLE}}</title>
<link rel=stylesheet href="https://cdn.jsdelivr.net/npm/katex@0.16.11/dist/katex.min.css">
<script defer src="https://cdn.jsdelivr.net/npm/katex@0.16.11/dist/katex.min.js"></script>
<script src="{{TOK}}/static/htmx.js"></script>
<style>
/* Dark, low contrast, one accent. Typography does the work: a serif at a reading measure,
   room between the lines, a shallow heading scale. Rules and panels are the exception —
   where a border or a background could be whitespace instead, it is. */
:root{--bg:#15161a;--fg:#bdbcb8;--dim:#75767a;--line:#272930;--acc:#8fa8c8;--warn:#b59566;--err:#c08276;
 --measure:37rem;--serif:ui-serif,"New York",Charter,"Iowan Old Style",Palatino,Georgia,serif;
 --mono:ui-monospace,SFMono-Regular,Menlo,monospace;color-scheme:dark}
*{box-sizing:border-box}
html{-webkit-text-size-adjust:100%}
body{margin:0;background:var(--bg);color:var(--fg);font:17px/1.7 var(--serif)}
header{position:sticky;top:0;z-index:5;display:flex;flex-wrap:wrap;gap:.3rem 1.1rem;align-items:baseline;
 padding:.7rem 1.1rem;background:var(--bg);font:12.5px/1.4 var(--mono)}
header a{color:var(--dim);text-decoration:none;border:0}
header a:hover{color:var(--fg)}header a.on{color:var(--acc)}
header .sp{flex:1}
main{max-width:var(--measure);margin:0 auto;padding:.6rem 1.1rem 8rem}
h1,h2,h3,h4{font-weight:600;line-height:1.3}
h1{font-size:1.35rem;margin:2rem 0 .7rem}
h2{font-size:1.1rem;margin:1.8rem 0 .5rem}
h3{font-size:1rem;margin:1.5rem 0 .4rem}
h4{font-size:.95rem;margin:1.3rem 0 .3rem;color:var(--dim)}
main>h1:first-child,#docwrap>h1:first-child{margin-top:.2rem}
p{margin:0 0 1.05em}
hr{border:0;border-top:1px solid var(--line);margin:2.2rem 0}
.at{color:var(--dim);font:12.5px/1.6 var(--mono)}
.msg{margin:1.5rem 0}
.pend{opacity:.4}
.pend.bad{opacity:.7;border-left-color:#c88}
.pend.bad::after{content:' ✕';color:#c88}
.user{border-left:2px solid #8fa8c84d;padding-left:1.1rem}
details.step{margin:.5rem 0;font:12.5px/1.6 var(--mono);color:var(--dim)}
details.step summary{cursor:pointer;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
details.step pre{white-space:pre-wrap;margin:.4rem 0}
pre{overflow-x:auto;background:#101115;padding:.75rem .9rem;border-radius:3px;
 font:12.5px/1.6 var(--mono);color:var(--dim)}
code{font:.85em/1.5 var(--mono)}
pre code{font-size:inherit;color:inherit}
a{color:var(--acc);text-decoration:none;border-bottom:1px solid #8fa8c840}
a:hover{border-bottom-color:var(--acc)}
a.wl{color:inherit;border-bottom:1px solid #4d4f57}
a.wl:hover{color:var(--acc);border-bottom-color:currentColor}
.cite{color:var(--dim);cursor:help}
/* An inlined lemma, proof or definition reads as its own block, not a run of paragraphs
   lost in the surrounding prose: a hairline rule and a little inset, nothing boxed in.
   Nesting is just more of the same rule, indented inside the parent's by the same amount. */
.embed{margin:1.3rem 0;padding-left:1rem;border-left:1px solid var(--line)}
.embed .envlabel{margin:0 0 .3rem;font:11px var(--mono);letter-spacing:.08em;
 text-transform:uppercase;color:var(--dim)}
.envclose{color:var(--dim)}
table{border-collapse:collapse;width:100%;font-size:.9em;margin:1.2em 0}
th,td{text-align:left;padding:.3rem 1.2rem .3rem 0;vertical-align:top}
th{color:var(--dim);font-weight:600;border-bottom:1px solid var(--line)}
blockquote{border-left:1px solid var(--line);margin:1.2em 0;padding-left:1.1rem;color:var(--dim)}
/* A comment is an aside, not a dialog: a coloured edge where it belongs, and controls that
   read as text until you want them. */
.diag{margin:1.2rem 0 1.5rem;padding-left:1.1rem;border-left:2px solid var(--warn);font-size:.92em}
.diag.error{border-left-color:var(--err)}.diag.info,.diag.hint{border-left-color:var(--line)}
.diag .sev{font:11px var(--mono);text-transform:uppercase;letter-spacing:.1em;color:var(--warn)}
.diag.error .sev{color:var(--err)}.diag.info .sev,.diag.hint .sev{color:var(--dim)}
.diag p:last-child{margin-bottom:0}
.diag form{display:flex;gap:1rem;margin-top:.6rem;flex-wrap:wrap;align-items:baseline}
.diag input[type=text]{flex:1;min-width:9rem;background:none;border:0;border-bottom:1px solid var(--line);
 color:var(--fg);padding:.2rem 0;font:12.5px var(--mono)}
.diag input[type=text]:focus{outline:0;border-bottom-color:var(--acc)}
button{background:none;color:var(--dim);border:0;padding:0;cursor:pointer;font:12.5px var(--mono)}
button:hover{color:var(--acc)}
footer{position:fixed;bottom:0;left:0;right:0;background:var(--bg);
 padding:.6rem 1.1rem env(safe-area-inset-bottom)}
footer form{max-width:var(--measure);margin:0 auto;display:flex;gap:.8rem;align-items:center}
textarea{flex:1;resize:none;background:#101115;color:var(--fg);border:0;border-radius:4px;
 padding:.6rem .8rem;font:15px/1.5 var(--serif);max-height:40vh}
textarea:focus{outline:1px solid var(--line)}
/* Saying something about a line reads like the comments do: a coloured edge under the line,
   the quote dim above the box, no frame around either. A card is always removable, even
   unsent — the `x` sits beside the quote rather than floating free of it. */
.say{margin:.5rem 0 1.2rem;padding-left:1.1rem;border-left:2px solid #8fa8c880}
.say .hd{display:flex;gap:.6rem;align-items:baseline;margin-bottom:.35rem}
.say .hd .q{flex:1;font:11.5px/1.5 var(--mono);color:var(--dim);
 overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.say .hd .x{flex:none;font:11px var(--mono);color:var(--dim)}
.say .hd .x:hover{color:var(--err)}
.say textarea{width:100%;display:block;flex:none;max-height:30vh}
.say textarea.r{margin-top:.6rem}
.say .st{font:11px var(--mono);text-transform:uppercase;letter-spacing:.1em;color:var(--dim);margin-top:.3rem}
/* The answer sits under the comment, in the same card, so the two read together; a fix it
   offered gets the same quiet apply button a diagnostic card's does. */
.say .msg.talk{margin:.7rem 0 0;font-size:.95em}
.say .msg.talk p:last-child{margin-bottom:0}
.say form{margin:.3rem 0 0}
/* A sent card keeps its coloured edge: it is waiting its turn, not cancelled. What was typed
   stays as plain text, since a greyed-out textarea reads as discarded. */
.say.done textarea.say{display:none}
.say.done .t{white-space:pre-wrap}
form.busy textarea,form.busy button{opacity:.45}
#older{min-height:1px}
body{overflow-anchor:none}
#toast{max-width:var(--measure);margin:.3rem auto 0;font:12px var(--mono);color:var(--dim);min-height:1em}
.kx span[data-math-style]:not([data-r]){opacity:0}
.kx span[data-math-style=display]:not([data-r]){display:block;min-height:2.6em}
.katex{font-size:1.03em}.katex-display{overflow-x:auto;overflow-y:hidden;margin:1.3em 0}
/* The line you can comment on says so only under the pointer, and only on the block a click
   would land on — never on touch, where there is no hover and every tap would light up. */
@media (hover:hover){p[data-line]:hover,li[data-line]:hover,blockquote[data-line]:hover,
 h1[data-line]:hover,h2[data-line]:hover,h3[data-line]:hover,h4[data-line]:hover{background:#ffffff08}}
@media (max-width:30rem){body{font-size:16px}main{padding:.6rem .9rem 8rem}
 header{gap:.3rem .85rem;padding:.6rem .9rem}footer{padding:.6rem .9rem env(safe-area-inset-bottom)}}
</style></head><body>
<header>{{NAV}}<span class=sp></span><span class=at>{{STATUS}}</span></header>
<main>{{BODY}}</main>
{{FOOT}}
<script>
var TOK="{{TOK}}";
// Math is rendered where the reader is, not all at once: whatever is within a few screens of
// the viewport first (an IntersectionObserver), the rest in idle moments, top to bottom. A
// formula not yet rendered is hidden rather than shown as raw TeX, and holds the room it will
// need; `data-r` marks one that is done.
var MO=null,MQ=[],MI=0;
function mrender(s){
  if(s.dataset.r||typeof katex=='undefined')return;
  s.dataset.r=1;
  try{katex.render(s.textContent,s,{displayMode:s.dataset.mathStyle=='display',throwOnError:false})}
  catch(e){}}
function idle(){
  if(MI)return;MI=1;
  var ric=window.requestIdleCallback||function(f){return setTimeout(function(){f({timeRemaining:function(){return 8}})},80)};
  ric(function pump(dl){
    var n=0;
    while(MQ.length&&n<12&&dl.timeRemaining()>3){var s=MQ.shift();if(!s.dataset.r){if(MO)MO.unobserve(s);mrender(s)}n++}
    if(MQ.length)ric(pump);else MI=0;});}
function mathify(r){
  if(typeof katex=='undefined')return;
  document.documentElement.classList.add('kx');
  var ss=r.querySelectorAll('span[data-math-style]:not([data-q])');
  if(!ss.length)return;
  if(!window.IntersectionObserver){ss.forEach(mrender);return}
  if(!MO)MO=new IntersectionObserver(function(es){es.forEach(function(e){
    if(e.isIntersecting){MO.unobserve(e.target);mrender(e.target)}})},{rootMargin:'1500px 0px'});
  ss.forEach(function(s){s.dataset.q=1;MQ.push(s);MO.observe(s)});
  idle();}
function atEnd(){return innerHeight+scrollY>document.body.scrollHeight-120}
var stick=true;
addEventListener('scroll',function(){stick=atEnd()});
document.addEventListener('htmx:afterSwap',function(e){mathify(e.target);if(stick&&!window._h)scrollTo(0,1e7)});
addEventListener('load',function(){mathify(document);if(location.hash=='')scrollTo(0,1e7);start()});
// A page keeps itself current by asking the server for news and being answered when there is
// some (the request is held up to ~25s): a chat message shows as it lands, an edited note
// refreshes, with no poll every few seconds. A hidden tab stops asking and catches up when
// it is shown again; a dropped connection is retried slowly.
var LS=0,LC=null,DL=0,DC=null;
function live(){
  var gen=++LS;if(LC)LC.abort();
  (function go(){
    var tail=document.getElementById('tail');
    if(gen!=LS||!tail)return;
    LC=new AbortController();
    fetch(TOK+'/f/log?wait=1&since='+tail.dataset.high,{signal:LC.signal})
      .then(function(r){return r.ok?r.text():Promise.reject(r.status)}).then(function(h){
        if(gen!=LS)return;
        var w=document.createElement('div');w.innerHTML=h;
        var nt=w.querySelector('#tail');
        if(nt){nt.remove();tail.dataset.high=nt.dataset.high;tail.dataset.up=nt.dataset.up;
          var dn=document.getElementById('down');if(dn)dn.hidden=nt.dataset.up!='0'}
        var n=w.firstChild,any=false;
        while(n){var nx=n.nextSibling;
          if(n.nodeType==1&&n.dataset&&n.dataset.t){var ps=document.querySelectorAll('.pend'),q=null;
            for(var i=0;i<ps.length;i++)if(ps[i].dataset.t==n.dataset.t){q=ps[i];break}
            if(q)q.remove()}
          tail.parentNode.insertBefore(n,tail);
          if(n.nodeType==1){any=true;mathify(n);htmx.process(n)}n=nx}
        if(any&&stick&&!window._h)scrollTo(0,1e7);
        go();
      },function(e){if(gen==LS&&!(e&&e.name=='AbortError'))setTimeout(go,5000)});
  })();
}
function docLive(){
  var gen=++DL;if(DC)DC.abort();
  (function go(){
    var w=document.getElementById('docwrap');
    if(gen!=DL||!w||!w.dataset.live)return;
    var u=w.dataset.live;
    DC=new AbortController();
    fetch(u+(u.indexOf('?')<0?'?':'&')+'wait=1&v='+w.dataset.v,{signal:DC.signal})
      .then(function(r){return r.status==200?r.text():r.status==204?'':Promise.reject(r.status)}).then(function(h){
        if(gen!=DL)return;
        // a box being typed in would be lost to the swap: wait it out
        if(h&&document.querySelector('.say:not(.done)'))return setTimeout(go,3000);
        if(h){w.outerHTML=h;var nw=document.getElementById('docwrap');mathify(nw);htmx.process(nw);cards()}
        go();
      },function(e){if(gen==DL&&!(e&&e.name=='AbortError'))setTimeout(go,5000)});
  })();
}
function start(){
  if(document.hidden)return;
  if(document.getElementById('tail'))live();
  if(document.getElementById('docwrap')){docLive();cards()}
}
document.addEventListener('visibilitychange',function(){
  if(!document.hidden)wait();
  if(document.hidden){LS++;DL++;if(LC)LC.abort();if(DC)DC.abort()}else start()});
// Cards (the comment boxes) outlive the page: a sent one comes back from the server - the
// words from the log, the answers from cards.json - under the line it was about.
function cards(){
  var ns={};
  document.querySelectorAll('[data-note]').forEach(function(e){if(!e.closest('.say'))ns[e.dataset.note]=1});
  var k=Object.keys(ns);if(!k.length)return;
  fetch(TOK+'/f/cards?notes='+encodeURIComponent(JSON.stringify(k)))
    .then(function(r){return r.json()}).then(function(cs){
      var last=null;
      cs.forEach(function(c){var d=restore(c);if(d)last=d});
      if(last&&(!RCARD||!document.body.contains(RCARD.d||RCARD)))listen(last.d,last.n);
    },function(){});
}
function restore(c){
  if(document.querySelector('.say[data-id="'+c.id+'"]'))return null;
  var b=document.querySelector('[data-note="'+CSS.escape(c.note)+'"][data-line="'+c.line+'"]:not(.say)');
  if(!b)return null;
  var d=document.createElement('div');d.className='say done';
  d.dataset.id=c.id;d.dataset.note=c.note;d.dataset.line=c.line;d.dataset.where=c.where;
  d.innerHTML='<div class=hd><div class=q></div><button class=x type=button>remove</button></div><div class=st>queued</div>';
  d.querySelector('.q').textContent=c.where;
  d.querySelector('.x').addEventListener('click',function(){del(d)});
  var st=d.querySelector('.st');
  c.said.forEach(function(t){var k=document.createElement('div');k.className='t';k.textContent=t;d.insertBefore(k,st)});
  var w=document.createElement('div');w.innerHTML=c.reply;
  var rp=w.firstElementChild,n=rp?parseInt(rp.dataset.high,10):0;
  while(rp&&rp.firstChild){var x=rp.firstChild;d.insertBefore(x,st);if(x.nodeType==1){mathify(x);htmx.process(x)}}
  b.parentNode.insertBefore(d,b.nextSibling);
  if(n)reply(d);
  return {d:d,n:n};
}
// Enter sends (into the running turn, at its next tool call); Shift-Enter sends for a turn
// of its own, after the running one; Alt-Enter is a new line
document.addEventListener('keydown',function(e){
  if(e.key!='Enter'||e.target.tagName!='TEXTAREA')return;
  if(e.target.closest('.say'))return; // the line-comment box keeps its own keys
  e.preventDefault();
  if(e.altKey){e.target.setRangeText('\n',e.target.selectionStart,e.target.selectionEnd,'end');return}
  var f=e.target.form;f.elements.later.value=e.shiftKey?'1':'0';
  htmx.trigger(f,'submit');});
document.addEventListener('htmx:afterRequest',function(e){var l=e.target.elements&&e.target.elements.later;if(l)l.value='0'});
// The compose box: dim while the server has it, clear on success, say plainly when it failed.
// A `/model` reply is not a message, so it goes to the toast and refreshes the header's model.
document.addEventListener('htmx:beforeRequest',function(e){
  var f=e.target;if(f.id!='compose')return;
  f.classList.add('busy');document.getElementById('toast').textContent='';
  var ta=f.querySelector('textarea'),tx=ta.value.trim(),tl=document.getElementById('tail');
  if(!tx||!tl)return;
  var p=document.createElement('div');p.className='msg user pend';p.dataset.t=tx;p.textContent=tx;
  tl.parentNode.insertBefore(p,tl);f._p=p;ta.value='';if(stick)scrollTo(0,1e7);});
document.addEventListener('htmx:afterRequest',function(e){
  var f=e.target;if(f.id!='compose')return;
  f.classList.remove('busy');
  var r=e.detail.xhr.responseText||'',t=document.getElementById('toast');
  var p=f._p;f._p=null;
  if(!e.detail.successful||/^not sent/.test(r)){t.textContent=r||'not sent';
    if(p){p.classList.add('bad');var ta=f.querySelector('textarea');if(!ta.value)ta.value=p.dataset.t;p.title='not sent'}
    return}
  var m=/^model: (\S+)/.exec(r);
  if(m||(p&&r!='sent'&&r!='queued')){if(p)p.remove()}
  if(m){t.textContent=r;var h=document.getElementById('mdl');if(h)h.textContent=m[1]}
  else if(r!='sent')t.textContent=r;});
// older messages load above, keeping the reader where they were
document.addEventListener('htmx:beforeRequest',function(e){
  if(e.target.id=='older')window._h=document.body.scrollHeight});
document.addEventListener('htmx:afterSwap',function(e){
  if(window._h){scrollBy(0,document.body.scrollHeight-window._h);window._h=0;setTimeout(nearTop,50)}});
// fetch the next page early: once the reader is in the top quarter, not only at the very top
function nearTop(){var o=document.getElementById('older');
  if(o&&!window._h&&scrollY<document.body.scrollHeight/4)htmx.trigger(o,'more')}
addEventListener('scroll',nearTop,{passive:true});
// Vim keys for reading: d/u a half page, j/k a few lines, gg and G the ends. Every jump is
// instant — no animation to sit through — and none of them fire while typing somewhere.
var gg=0;
document.addEventListener('keydown',function(e){
  if(e.ctrlKey||e.metaKey||e.altKey)return;
  var t=e.target;
  if(t.tagName=='TEXTAREA'||t.tagName=='INPUT'||t.isContentEditable)return;
  var h=innerHeight,by=0;
  if(e.key=='d')by=h/2; else if(e.key=='u')by=-h/2;
  else if(e.key=='j')by=h/10; else if(e.key=='k')by=-h/10;
  else if(e.key=='G')by=document.body.scrollHeight;
  else if(e.key=='g'){
    if(gg){gg=0;e.preventDefault();scrollTo({top:0,behavior:'instant'});return}
    gg=1;setTimeout(function(){gg=0},400);return;
  } else return;
  gg=0;e.preventDefault();
  scrollBy({top:by,behavior:'instant'});});
// A double-click (double-tap) on any rendered line of a note — comment cards, chat and the
// compose box excluded — opens a box under that line, and what is typed there goes through the
// same `/x/send` a message typed by hand would, shaped the way a reply quoting a line always is.
// KaTeX leaves three copies of every formula in the DOM (the visual one, a MathML one and the
// TeX annotation), so reading `textContent` off a line would repeat each formula three times.
// A quote therefore comes off a clone whose rendered math is put back as its own TeX source.
function quoted(b){
  var c=b.cloneNode(true);
  // a formula not rendered yet is still its own TeX source
  Array.prototype.forEach.call(c.querySelectorAll('span[data-math-style]:not([data-r])'),function(k){
    k.parentNode.replaceChild(document.createTextNode('$'+k.textContent+'$'),k);
  });
  Array.prototype.forEach.call(c.querySelectorAll('.katex'),function(k){
    var a=k.querySelector('annotation');
    k.parentNode.replaceChild(document.createTextNode(a?'$'+a.textContent+'$':''),k);
  });
  return (c.textContent||'').trim().replace(/\s+/g,' ').slice(0,160);
}
function comment(e){
  if(e.target.closest('a,form,button,textarea,.diag'))return;
  var b=e.target.closest('[data-line]');if(!b||!b.dataset.note)return;
  var quote=quoted(b);
  // A short id of its own, right here in the quote, is what lets a deliberate `facet answer`
  // find its way back to this card — never a talk reply its poll merely happens to catch.
  var id='c'+Date.now().toString(36)+Math.random().toString(36).slice(2,5);
  var where='[['+b.dataset.note+']] L'+b.dataset.line+' #'+id+(quote?': "'+quote+'"':'');
  say(b,where,id,b.dataset.note,b.dataset.line);
}
// Nothing pops up: the box opens in the page, under the line it is about, already focused, with
// a dim echo of what is being quoted above it. Enter sends, Shift-Enter is a new line, Escape
// leaves no trace when the box is still empty. Only one unsent box is open at a time, but every
// card, sent or not, keeps its own `x` to take it away.
function say(b,where,id,note,line){
  var old=document.querySelector('.say:not(.done)');if(old)old.remove();
  var d=document.createElement('div');d.className='say';
  d.dataset.id=id;d.dataset.note=note;d.dataset.line=line;d.dataset.where=where;
  d.innerHTML='<div class=hd><div class=q></div><button class=x type=button>remove</button></div>\
    <textarea class=say rows=2 placeholder="say what to change"></textarea><div class=st></div>';
  d.querySelector('.q').textContent=where;
  d.querySelector('.x').addEventListener('click',function(){del(d)});
  b.parentNode.insertBefore(d,b.nextSibling);
  var t=d.querySelector('textarea.say');
  t.addEventListener('blur',function(){
    if(!d.classList.contains('done')&&!t.value.trim())d.remove();
  });
  t.addEventListener('keydown',function(ev){
    if(ev.key=='Escape'){ev.preventDefault();d.remove();return}
    if(ev.key!='Enter'||ev.shiftKey||ev.altKey)return;
    ev.preventDefault();ev.stopPropagation();
    var said=t.value.trim();if(!said){d.remove();return}
    t.readOnly=true;
    send(d,where,said,function(ok){if(!ok)t.readOnly=false});
  });
  t.focus();
}
// A card's own remove: a sent one only has to leave the page, not be unsent — the answer it
// may already carry stays exactly where `facet answer` put it.
function del(d){
  if(d===RCARD)RCARD=null;if(RT){clearTimeout(RT);RT=null}
  if(d.classList.contains('done'))
    fetch(TOK+'/x/hide',{method:'POST',headers:{'Content-Type':'application/x-www-form-urlencoded'},body:'id='+d.dataset.id});
  d.remove()}
// The one POST every card's textarea sends through, first message or a later reply alike: the
// id travels with it every time, so the whole thread stays one card no matter how it grows.
function send(d,where,said,after){
  var st=d.querySelector('.st');
  var mark=function(s){d.classList.add('done');st.textContent=s};
  var kept=document.createElement('div');kept.className='t';kept.textContent=said;
  d.insertBefore(kept,st);
  mark('sending');
  var body='id='+d.dataset.id+'&note='+encodeURIComponent(d.dataset.note)+'&line='+d.dataset.line+
    '&text='+encodeURIComponent(where+'\n'+said)+'&later=1';
  fetch(TOK+'/x/send',{method:'POST',headers:{'Content-Type':'application/x-www-form-urlencoded'},body:body})
    .then(function(r){return r.ok?r.text():Promise.reject(r.status)})
    .then(function(){mark('queued');listen(d);if(after)after(true)},
          function(err){kept.remove();mark('not sent ('+err+')');if(after)after(false)});
}
// An answer belongs where the comment was made, not only in the chat: once a card is sent it
// watches for the reply addressed to it (by id, never a generic one) and puts it underneath
// what was said; a reply box then opens so answering back stays inside the same card. The
// newest card is the one being watched — an older one keeps what it already has.
var RCARD=null,RCOUNT=0,RT=null;
function listen(d,n){RCARD=d;RCOUNT=n||0;wait()}
function wait(){if(!RT&&RCARD&&!document.hidden)RT=setTimeout(poll,0)}
function later(){setTimeout(wait,5000)}
function poll(){
  RT=null;if(!RCARD)return;
  var d=RCARD;
  // held by the server until the card has a new answer (or ~25s): no poll every few seconds
  fetch(TOK+'/f/reply?wait=1&id='+d.dataset.id+'&since='+RCOUNT).then(function(r){return r.text()}).then(function(h){
    var w=document.createElement('div');w.innerHTML=h;
    var rp=w.firstElementChild;
    if(rp&&rp.children.length){
      RCOUNT=parseInt(rp.dataset.high,10);
      var st=d.querySelector('.st');
      // htmx's own swap would wire up an answer's `apply` form; inserted by hand, it needs
      // telling the same way mathify is: once, right after it lands.
      while(rp.firstChild){var n=rp.firstChild;d.insertBefore(n,st);mathify(n);htmx.process(n)}
      reply(d);
    }
    wait();
  },later);
}
// Once a card has an answer in it, a further reply goes out the same way the comment did,
// still carrying the same id, so the thread stays attached to it.
function reply(d){
  if(d.querySelector('.r'))return;
  var t=document.createElement('textarea');t.className='r';t.rows=1;t.placeholder='reply';
  d.insertBefore(t,d.querySelector('.st'));
  t.addEventListener('keydown',function(ev){
    if(ev.key!='Enter'||ev.shiftKey||ev.altKey)return;
    ev.preventDefault();
    var said=t.value.trim();if(!said)return;
    t.readOnly=true;
    send(d,d.dataset.where,said,function(ok){t.readOnly=false;if(ok)t.value=''});
  });
}
// iOS Safari does not fire `dblclick` reliably on a touch, so a coarse (touch) pointer gets
// its own double-tap detector instead, ported from vault-phone's `pick()`.
if(matchMedia('(pointer: coarse)').matches){
  var lastTap=null;
  document.addEventListener('click',function(e){
    var now={t:e.timeStamp,x:e.clientX,y:e.clientY};
    var isDouble=lastTap&&now.t-lastTap.t<350&&Math.hypot(now.x-lastTap.x,now.y-lastTap.y)<30;
    lastTap=isDouble?null:now;
    if(isDouble)comment(e);
  });
}else{
  document.addEventListener('dblclick',comment);
}
</script></body></html>"#;

fn page(cfg: &Cfg, title: &str, nav_on: &str, body: &str, compose: bool) -> String {
    let t = cfg.token_path();
    let item = |href: &str, label: &str, key: &str| {
        format!("<a href=\"{}{}\"{}>{}</a>", t, href,
            if key == nav_on { " class=on" } else { "" }, label)
    };
    let mut nav = String::new();
    nav.push_str(&item("/", "home", "home"));
    nav.push_str(&item("/chat", "chat", "chat"));
    nav.push_str(&item("/m/", "notes", "notes"));
    nav.push_str(&item("/d/", "comments", "diag"));
    nav.push_str(&item("/tree", "memory", "tree"));
    if !cfg.terminal().is_empty() { nav.push_str(&format!("<a href=\"{}\">term</a>", cfg.terminal())); }
    let n = diag::all(cfg).len();
    let model = if compose {
        crate::optchat::engine::request(&crate::optchat::engine::dir(), serde_json::json!({"op": "status"}))
            .ok().and_then(|v| v["model"].as_str().map(String::from)).unwrap_or_default()
    } else { String::new() };
    let status = format!("{}{}{}",
        if model.is_empty() { String::new() } else { format!("<span id=mdl>{}</span> · ", md::esc(&model)) },
        if tell::healthy(cfg) { "" } else { "input down · " },
        if n > 0 { format!("{} comments", n) } else { String::new() });
    // Only the chat has a box standing ready at the bottom. Reading a note, what you want to say
    // is always about a line of it, so the box comes to the line you double-click instead. The
    // toast line stays either way — the comment cards post through it.
    let foot = format!("<footer>{}<div id=toast></div></footer>", if compose {
        format!("<form hx-post=\"{}/x/send\" hx-swap=none id=compose>\
            <textarea name=text rows=1 placeholder=\"message\"></textarea><input type=hidden name=later value=0>\
            <button>send</button></form>", t)
    } else { String::new() });
    SHELL.replace("{{TITLE}}", &md::esc(title))
        .replace("{{TOK}}", &t)
        .replace("{{NAV}}", &nav)
        .replace("{{STATUS}}", &status)
        .replace("{{BODY}}", body)
        .replace("{{FOOT}}", &foot)
}

// ---- rendering the three views ----------------------------------------------------------

/// Where a `[[wikilink]]` goes: facet's own `/n/` route, against the vault facet is
/// configured for. It used to go to the vault-phone service, which serves whatever vault its
/// own script was pointed at, so a link out of a doc answered 404 whenever the two differed.
fn note_base(cfg: &Cfg) -> String { format!("{}/n/", cfg.token_path()) }

/// The name a wikilink or an embed uses for this note — its file stem, the same key `doc::find`
/// matches against — so a block rendered from it tags itself the way a click handler expects.
fn home_of(d: &doc::Doc) -> String { d.path.file_stem().unwrap_or_default().to_string_lossy().to_string() }

fn msg_html(cfg: &Cfg, m: &log::Msg) -> String {
    let base = note_base(cfg);
    match m.kind.as_str() {
        "user" => format!("<div class=\"msg user\" data-t=\"{}\">{}</div>", md::esc(m.text.trim()), md::render(&m.text, &base)),
        "chat" | "talk" | "note" | "work" => format!("<div class=\"msg talk\">{}</div>", md::render(&m.text, &base)),
        _ => {
            let head: String = m.text.lines().next().unwrap_or("").chars().take(110).collect();
            format!("<details class=step><summary>{} · {}</summary><pre>{}</pre></details>",
                m.kind, md::esc(&head), md::esc(&m.text.chars().take(4000).collect::<String>()))
        }
    }
}

/// The chat fragment: new messages, then the `#tail` marker carrying the new cursor (and whether
/// the engine is up). The cursor lives in the DOM; there is no client-side state to get out of
/// step. With `wait` the request holds until the chat has something new, or `LONG` is up: the
/// page asks again at once, so a message shows as it lands without a poll every few seconds.
/// Only the chat venue is shown (`Msg::in_chat`): the user's chat messages and the agent's
/// `send_chat`s. The rest of the stream (plain talk, steps, card traffic) is the memory's, and
/// the tree view (/tree) is where to read it.
fn log_fragment(cfg: &Cfg, since: i64, wait: bool) -> String {
    let t0 = Instant::now();
    let mut cur = since;
    let (msgs, high) = loop {
        let msgs = log::since(cfg, cur);
        let high = msgs.last().map(|m| m.i).unwrap_or(cur);
        // a step or a card comment only moves the cursor: it is not worth waking the page for
        if !wait || msgs.iter().any(|m| m.in_chat()) || t0.elapsed() >= LONG { break (msgs, high) }
        cur = high;
        std::thread::sleep(Duration::from_millis(250));
    };
    let mut out: String = msgs.iter().filter(|m| m.in_chat()).map(|m| msg_html(cfg, m)).collect();
    out.push_str(&format!("<div id=tail data-high=\"{}\" data-up={} hidden></div>",
        high, if tell::healthy(cfg) { 1 } else { 0 }));
    out
}

/// The answers addressed to one card, by its id — never a talk reply the poll merely happened
/// to catch (the bug this and `facet answer` replace). `since` is how many of them the page
/// has already shown; an answer with a fix attached gets the same apply button a diagnostic
/// card does, through the same `/x/diag` route.
fn reply_fragment(cfg: &Cfg, id: &str, since: usize) -> String {
    let sd = crate::optchat::engine::state_dir(&crate::optchat::engine::dir());
    let card = cards::get(&sd, id).unwrap_or(serde_json::Value::Null);
    let answers = card["answers"].as_array().cloned().unwrap_or_default();
    let base = note_base(cfg);
    let said: String = answers.iter().skip(since).map(|a| {
        let mut s = format!("<div class=\"msg talk\">{}</div>",
            md::render(a["text"].as_str().unwrap_or(""), &base));
        if let Some(code) = a["code"].as_str() {
            s.push_str(&format!("<form hx-post=\"{}/x/diag\" hx-target=\"#toast\" hx-swap=innerHTML>\
                <input type=hidden name=code value=\"{}\"><button name=do value=apply>apply</button></form>",
                cfg.token_path(), md::esc(code)));
        }
        s
    }).collect();
    format!("<div class=rp data-high=\"{}\">{}</div>", answers.len(), said)
}

const PAGE: usize = 40;

/// Older chat messages: the PAGE before `before`, and a sentinel that fetches the PAGE before
/// those when it scrolls into view, if there are any.
fn older_fragment(cfg: &Cfg, before: i64) -> String {
    let all: Vec<log::Msg> = log::since_by(cfg, -1, |m| m.in_chat() && m.i < before);
    let from = all.len().saturating_sub(PAGE);
    let mut out = older_sentinel(cfg, &all[from..], from > 0);
    out.push_str(&all[from..].iter().map(|m| msg_html(cfg, m)).collect::<String>());
    out
}
fn older_sentinel(cfg: &Cfg, shown: &[log::Msg], more: bool) -> String {
    match (more, shown.first()) {
        (true, Some(m)) => format!("<div id=older hx-get=\"{}/f/older?before={}\" hx-trigger=\"revealed, more\" hx-swap=outerHTML></div>",
            cfg.token_path(), m.i),
        _ => String::new(),
    }
}

fn chat_page(cfg: &Cfg) -> String {
    // the last PAGE messages of the chat venue, not of the stream; earlier ones load on scroll
    let all: Vec<log::Msg> = log::since_by(cfg, -1, |m| m.in_chat());
    let k = all.len().saturating_sub(PAGE);
    let since = all.get(k).map(|m| m.i - 1).unwrap_or(-1);
    page(cfg, "Facet", "chat", &format!("<p id=down class=at style=\"color:var(--err)\"{}>the engine is not running: \
        nothing will answer until it is restarted</p><div id=log>{}{}</div>",
        if tell::healthy(cfg) { " hidden" } else { "" },
        older_sentinel(cfg, &all[k..], k > 0), log_fragment(cfg, since, false)), true)
}

/// A diagnostic as a card: the message, the explanation with real math, and the three things
/// you can do about it. `where_` is shown when the card is away from its note.
fn diag_card(cfg: &Cfg, d: &diag::Diag, show_where: bool, quote: bool) -> String {
    let t = cfg.token_path();
    let base = note_base(cfg);
    let mut s = format!("<div class=\"diag {}\" id=\"d-{}\"><div><span class=sev>{}</span> \
        <span class=at>{}L{}</span></div><div>{}</div>",
        d.severity, md::esc(&d.code), md::esc(&d.severity),
        if show_where { format!("{} · ", md::esc(d.note.trim_end_matches(".md"))) } else { String::new() },
        d.line, md::render(&d.message, &base));
    if quote {
        s.push_str(&format!("<pre>{}</pre>", md::esc(&diag::context(cfg, d, 1))));
    }
    if let Some(det) = &d.detail {
        s.push_str(&format!("<details class=step><summary>why</summary>{}</details>",
            md::render(det, &base)));
    }
    s.push_str(&format!("<form hx-post=\"{}/x/diag\" hx-target=\"#toast\" hx-swap=innerHTML>\
        <input type=hidden name=code value=\"{}\">\
        <input type=text name=note placeholder=\"reason / question\">{}\
        <button name=do value=dismiss>dismiss</button>\
        <button name=do value=discuss>discuss</button></form></div>",
        t, md::esc(&d.code),
        if d.fixes > 0 { format!("<button name=do value=apply>apply fix ({})</button>", d.fixes) }
        else { String::new() }));
    s
}

/// A note, with its diagnostics anchored in place. The text is cut only at blank lines that
/// are not inside a fence or a display-math block, so a card never lands mid-block.
fn note_html(cfg: &Cfg, d: &doc::Doc) -> String {
    let ds = diag::for_note(cfg, &d.path);
    let base = note_base(cfg);
    let home = home_of(d);
    let lines: Vec<&str> = d.text.split('\n').collect();
    let skip = doc::front_len(&d.text);     // frontmatter is metadata, not prose
    if ds.is_empty() {
        let (text, srcs) = doc::assemble(cfg, &home, &lines[skip.min(lines.len())..], skip);
        return md::render_at(&text, &base, &srcs);
    }
    let mut out = String::new();
    let (mut start, mut fence, mut math) = (skip, false, false);
    let emit = |out: &mut String, a: usize, b: usize| {
        if a >= b { return }
        let (text, srcs) = doc::assemble(cfg, &home, &lines[a..b], a);
        out.push_str(&md::render_at(&text, &base, &srcs));
        for g in ds.iter().filter(|g| g.line - 1 >= a as i64 && g.line - 1 < b as i64) {
            out.push_str(&diag_card(cfg, g, false, false));
        }
    };
    for (i, l) in lines.iter().enumerate().skip(skip) {
        let tl = l.trim_start();
        if tl.starts_with("```") { fence = !fence }
        if tl == "$$" { math = !math }
        if fence || math { continue }
        // Cut at blank lines, and at the start of a top-level list item: lists have no blank
        // lines inside them, and without this a comment on one bullet lands under the last.
        if l.trim().is_empty() {
            emit(&mut out, start, i + 1);
            start = i + 1;
        } else if (l.starts_with("- ") || l.starts_with("* ")) && i > start {
            emit(&mut out, start, i);
            start = i;
        }
    }
    emit(&mut out, start, lines.len());
    // anything anchored past the end of the note
    for g in ds.iter().filter(|g| g.line as usize > lines.len()) {
        out.push_str(&diag_card(cfg, g, false, true));
    }
    out
}

/// One `#`-section of a note: what `[[Note#Section]]` asks for, as `?h=`. No comment cards
/// here — their line numbers are the whole file's, and a section does not start where it does.
fn section_html(cfg: &Cfg, d: &doc::Doc, h: &str) -> String {
    let Some((s, start)) = doc::section_at(&d.text, h) else { return note_html(cfg, d) };
    let lines: Vec<&str> = s.split('\n').collect();
    let (text, srcs) = doc::assemble(cfg, &home_of(d), &lines, start - 1);
    format!("<h2>{}</h2>{}", md::esc(h), md::render_at(&text, &note_base(cfg), &srcs))
}

/// Any note of the vault, read-only, on the same page as a published one. Wikilinks in docs,
/// notes and messages all land here, so a name that is not in the vault must say so plainly.
fn note_page(cfg: &Cfg, name: &str, h: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let Some(d) = doc::note(cfg, name) else {
        return html(page(cfg, "no such note", "notes",
            &format!("<h1>no such note</h1><p class=at>{} is not in {}</p>",
                md::esc(name), md::esc(&cfg.vault().to_string_lossy())), false), 404);
    };
    html(page(cfg, &d.title, "notes", &note_fragment(cfg, &d, name, h), false), 200)
}


fn mtime_us(p: &Path) -> u64 {
    std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_micros() as u64).unwrap_or(0)
}

/// Every file a page is made of: the note, and each note it embeds, however deep (the embeds
/// of an embed count too).
fn doc_files(cfg: &Cfg, d: &doc::Doc) -> Vec<PathBuf> {
    let mut seen = vec![d.path.clone()];
    let mut todo = vec![d.text.clone()];
    while let Some(t) = todo.pop() {
        for l in t.lines() {
            let Some(e) = doc::embed(l) else { continue };
            let Some(p) = doc::find(cfg, &e.note) else { continue };
            if seen.contains(&p) { continue }
            if let Ok(tx) = std::fs::read_to_string(&p) { todo.push(tx) }
            seen.push(p);
        }
    }
    seen
}

/// Changes when anything on the page does: the note, a note it embeds, or a comment on it.
fn doc_version(cfg: &Cfg, d: &doc::Doc) -> u64 {
    doc_files(cfg, d).iter().map(|p| mtime_us(p)).chain(std::iter::once(mtime_us(&diag::file(cfg)))).max().unwrap_or(0)
}

/// A page that keeps itself current: `live` is the route that answers for it, `v` the version
/// the markup was made from. The page asks `live?v=..&wait=1`, which holds until the version
/// differs (then answers with the new markup) or `LONG` is up (204, ask again).
fn live_wrap(live: &str, v: u64, inner: String) -> String {
    format!("<div id=docwrap data-live=\"{}\" data-v=\"{}\">{}</div>", md::esc(live), v, inner)
}

fn doc_fragment(cfg: &Cfg, d: &doc::Doc) -> String {
    let v = doc_version(cfg, d);
    live_wrap(&format!("{}/f/doc/{}", cfg.token_path(), md::urlenc(&d.slug)), v,
        format!("<h1>{}</h1>{}", md::esc(&d.title), note_html(cfg, d)))
}

fn note_fragment(cfg: &Cfg, d: &doc::Doc, name: &str, h: &str) -> String {
    let v = doc_version(cfg, d);
    let body = if h.is_empty() { note_html(cfg, d) } else { section_html(cfg, d, h) };
    let live = format!("{}/f/note/{}{}", cfg.token_path(), md::urlenc(name),
        if h.is_empty() { String::new() } else { format!("?h={}", md::urlenc(h)) });
    live_wrap(&live, v, format!("<h1>{}</h1>{}", md::esc(&d.title), body))
}

/// The answer to a live page's question: the new markup if its version moved on, else nothing
/// (204) once `LONG` has passed, or at once when not asked to wait.
fn live_doc(cfg: &Cfg, get: impl Fn() -> Option<doc::Doc>, v: i64, wait: bool,
            render: impl Fn(&doc::Doc) -> String) -> Response<std::io::Cursor<Vec<u8>>> {
    let t0 = Instant::now();
    loop {
        let Some(d) = get() else { return html(String::new(), 204) };
        if doc_version(cfg, &d) as i64 != v { return html(render(&d), 200) }
        if !wait || t0.elapsed() >= LONG { return html(String::new(), 204) }
        std::thread::sleep(Duration::from_millis(1500));
    }
}

fn diag_page(cfg: &Cfg) -> String {
    let ds = diag::all(cfg);
    let mut body = format!("<h1>Comments</h1><p class=at>{} open</p>", ds.len());
    if ds.is_empty() { body.push_str("<p class=at>Nothing open. Reviews land in <code>.claude/diagnostics.json</code>.</p>"); }
    let mut last = String::new();
    for d in &ds {
        if d.note != last {
            body.push_str(&format!("<h2>{}</h2>", md::esc(d.note.trim_end_matches(".md"))));
            last = d.note.clone();
        }
        body.push_str(&diag_card(cfg, d, false, true));
    }
    page(cfg, "Comments", "diag", &body, false)
}

// ---- the server -------------------------------------------------------------------------

/// One page that links every other one, each with a line of live state.
fn home(cfg: &Cfg) -> String {
    let t = cfg.token_path();
    let st = crate::optchat::engine::request(&crate::optchat::engine::dir(), serde_json::json!({"op": "status"}));
    let (engine, usage) = match &st {
        Ok(v) => (format!("{} · {} messages{}", if v["busy"] == true { "working" } else { "idle" }, v["messages"],
                          v["paused"].as_str().map(|p| format!(" · compactor paused: {}", p)).unwrap_or_default()),
                  v["limits"].as_str().unwrap_or("").to_string()),
        Err(e) => (format!("engine DOWN: {}", e), String::new()),
    };
    let notes = doc::table(cfg).len();
    let comments = diag::all(cfg).len();
    let mut rows: Vec<(String, &str, String)> = vec![
        (format!("{}/chat", t), "Chat", engine),
        (format!("{}/tree", t), "Memory", "the whole tree: summaries down to every message, searchable".into()),
        (format!("{}/m/", t), "Notes", format!("{} published", notes)),
        (format!("{}/d/", t), "Comments", format!("{} open", comments)),
    ];
    if !cfg.terminal().is_empty() { rows.push((cfg.terminal(), "Terminal", "the chat in a terminal (facet chat)".into())); }
    if let Some(u) = cfg.opt("telegram.username") { rows.push((format!("https://t.me/{}", u), "Telegram", format!("@{} · /ping, /last, /help", u))); }
    // No cards, despite the class name: a list of names, each with its line of state under it.
    let mut b = String::from("<style>.home a.card{display:block;padding:.8rem 0;border:0;color:inherit}\
        .home .n{font-size:1.05rem}.home a.card:hover .n{color:var(--acc)}\
        .home .d{font:12.5px/1.6 var(--mono);color:var(--dim)}\
        .home .u{font:12.5px/1.6 var(--mono);color:var(--dim);margin:0 0 1.4rem}</style><div class=home>");
    if !usage.is_empty() { b.push_str(&format!("<div class=u>{}</div>", md::esc(&usage))); }
    for (href, name, desc) in rows {
        b.push_str(&format!("<a class=card href=\"{}\"><div class=n>{}</div><div class=d>{}</div></a>", md::esc(&href), name, md::esc(&desc)));
    }
    b.push_str("</div>");
    b
}

fn html(body: String, code: u16) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_status_code(code)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap())
        .with_header(Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap())
}

fn form(body: &str) -> Vec<(String, String)> {
    body.split('&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (md::urldec(k), md::urldec(v))
    }).collect()
}
fn field(f: &[(String, String)], k: &str) -> String {
    f.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone()).unwrap_or_default()
}

pub fn serve(cfg: Cfg) {
    let addr = format!("{}:{}", cfg.host(), cfg.port());
    let server = Server::http(&addr).unwrap_or_else(|e| { eprintln!("bind {}: {}", addr, e); std::process::exit(1) });
    println!("facet on {} ({})", cfg.url("/"), addr);
    crate::tg::spawn(&cfg);
    // A thread to a request: a page waiting for news (`?wait=1`) holds its thread for up to
    // `LONG`, and nothing else should queue behind it. A panic in a handler ends its own
    // connection and nothing more.
    for mut rq in server.incoming_requests() {
        std::thread::spawn(move || {
            let cfg = Cfg::load();
            let res = route(&cfg, &mut rq);
            let _ = rq.respond(res);
        });
    }
}

// ---- the memory tree, folded once per change ---------------------------------------------

struct Tree { s: crate::optchat::store::Store, v: crate::optchat::view::View, page: Mutex<Option<String>> }

/// The store as it is now, folded into a view, shared by every request until a file under
/// `chat/` grows or changes. What changes it is the signature of those files (name, size,
/// mtime) - a `stat` each, not a parse of the whole store.
fn tree(cfg: &Cfg) -> Arc<Tree> {
    static C: OnceLock<Mutex<Option<(String, Arc<Tree>)>>> = OnceLock::new();
    let mut sig = format!("{}|", cfg.store().display());
    for sub in ["chat/main", "chat/tree"] {
        let mut fs: Vec<_> = std::fs::read_dir(cfg.store().join(sub)).into_iter().flatten().flatten().collect();
        fs.sort_by_key(|e| e.file_name());
        for e in fs {
            let m = e.metadata().ok();
            sig.push_str(&format!("{:?}:{}:{};", e.file_name(), m.as_ref().map(|m| m.len()).unwrap_or(0),
                m.map(|m| mtime_us_meta(&m)).unwrap_or(0)));
        }
    }
    let mut g = C.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner());
    if let Some((k, t)) = g.as_ref() { if k == &sig { return t.clone() } }
    let s = crate::optchat::store::Store::open(&cfg.store());
    let v = crate::optchat::view::View::fold(&s, crate::optchat::VIEW);
    let t = Arc::new(Tree { s, v, page: Mutex::new(None) });
    *g = Some((sig, t.clone()));
    t
}
fn mtime_us_meta(m: &std::fs::Metadata) -> u64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_micros() as u64).unwrap_or(0)
}

/// Every card of these notes still on the page: who it was about, what was said (from the log,
/// where the message already is), and the answers it has - the same markup a live card shows.
fn cards_json(cfg: &Cfg, notes: &[String]) -> String {
    let sd = crate::optchat::engine::state_dir(&crate::optchat::engine::dir());
    let all = cards::all(&sd);
    let msgs = log::since_by(cfg, -1, |m| m.kind == "user" && cards::from_card(&m.text).is_some());
    let mut out: Vec<(i64, serde_json::Value)> = Vec::new();
    for (id, c) in all.as_object().into_iter().flatten() {
        if c["hidden"] == true || !notes.iter().any(|n| Some(n.as_str()) == c["note"].as_str()) { continue }
        let mine: Vec<&log::Msg> = msgs.iter().filter(|m| cards::from_card(&m.text) == Some(id.as_str())).collect();
        let Some(first) = mine.first() else { continue };
        let body = |m: &log::Msg| -> (String, String) {
            let t = m.text.find("[end prior context]\n\n").map(|k| &m.text[k + 21..]).unwrap_or(&m.text).trim_start();
            let (w, r) = t.split_once('\n').unwrap_or((t, ""));
            (w.to_string(), r.trim().to_string())
        };
        out.push((first.i, serde_json::json!({
            "id": id, "note": c["note"], "line": c["line"], "where": body(first).0,
            "said": mine.iter().map(|m| body(m).1).collect::<Vec<_>>(),
            "reply": reply_fragment(cfg, id, 0),
        })));
    }
    out.sort_by_key(|(i, _)| *i);
    serde_json::Value::Array(out.into_iter().map(|(_, v)| v).collect()).to_string()
}

/// A page for something that is not there, in the same dark look as everything else.
fn gone(cfg: &Cfg, what: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    html(page(cfg, "Not found", "", &format!("<h1>Not found</h1><p class=at>{}</p>", md::esc(what)), false), 404)
}

fn route(cfg: &Cfg, rq: &mut Request) -> Response<std::io::Cursor<Vec<u8>>> {
    let url = rq.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let segs: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(md::urldec).collect();

    // one token, in the path, for every route — no headers, no cookies, nothing a
    // WebSocket upgrade can decline to carry.
    let tok = cfg.token();
    if tok.is_empty() || segs.first().map(|s| s != &tok).unwrap_or(true) {
        return html("<!DOCTYPE html><meta charset=utf-8><body style=\"background:#15161a;color:#75767a;font:14px monospace;padding:2rem\">not found</body>".into(), 404);
    }
    let rest: Vec<&str> = segs[1..].iter().map(|s| s.as_str()).collect();
    let qnum = |k: &str| -> i64 {
        query.split('&').find_map(|p| p.strip_prefix(&format!("{}=", k)))
            .and_then(|v| v.parse().ok()).unwrap_or(-1)
    };
    let qstr = |k: &str| -> String {
        query.split('&').find_map(|p| p.strip_prefix(&format!("{}=", k)))
            .map(md::urldec).unwrap_or_default()
    };
    let post = rq.method() == &tiny_http::Method::Post;
    let _posting = if post { Some(POSTING.lock().unwrap_or_else(|e| e.into_inner())) } else { None };
    let mut body = String::new();
    if post { let _ = rq.as_reader().read_to_string(&mut body); }
    let f = form(&body);

    match rest.as_slice() {
        // the root is the home page; /home stays as an alias for old links
        [] | ["home"] => html(page(cfg, "Facet", "home", &home(cfg), false), 200),
        ["chat"] => html(chat_page(cfg), 200),

        ["f", "older"] => html(older_fragment(cfg, qnum("before")), 200),
        ["f", "log"] => html(log_fragment(cfg, qnum("since"), qnum("wait") > 0), 200),

        // the answers addressed to one card, for it to show them where it was sent
        ["f", "reply"] => {
            // with `wait`, held until the card has more answers than the page has shown
            let (id, seen) = (qstr("id"), qnum("since").max(0) as usize);
            let t0 = Instant::now();
            let sd = crate::optchat::engine::state_dir(&crate::optchat::engine::dir());
            while qnum("wait") > 0 && t0.elapsed() < LONG
                && cards::get(&sd, &id).map(|c| c["answers"].as_array().map(|a| a.len()).unwrap_or(0)).unwrap_or(0) <= seen {
                std::thread::sleep(Duration::from_millis(500));
            }
            html(reply_fragment(cfg, &id, seen), 200)
        }

        // the cards of the notes on a page, so a reload (or a refresh) puts them back
        ["f", "cards"] => {
            let notes: Vec<String> = serde_json::from_str(&qstr("notes")).unwrap_or_default();
            html(cards_json(cfg, &notes), 200)
        }

        // the memory tree (folded from the files: the engine's view is the same fold), the
        // scaffold down to the view-line frontier only - a stub past that is `/f/node`'s to fetch
        ["tree"] => {
            let t = tree(cfg);
            let mut pg = t.page.lock().unwrap_or_else(|e| e.into_inner());
            let body = pg.get_or_insert_with(|| crate::optchat::browse::web(&t.s, &t.v, crate::optchat::VIEW, &cfg.token_path())).clone();
            html(body, 200)
        }

        // a stub's first open: the immediate children of (l, i), themselves stubbed one level
        // further wherever they still have halves of their own
        ["f", "node"] => {
            let (l, i) = (qnum("l").max(0) as usize, qnum("i").max(0) as usize);
            let t = tree(cfg);
            html(crate::optchat::browse::node(&t.s, &t.v, l, i), 200)
        }

        // the memory tree's own search: past what the page ever loaded, since it was never
        // all shipped up front to begin with
        ["f", "find"] => {
            html(crate::optchat::browse::find(&tree(cfg).s, &qstr("q")), 200)
        }

        ["m"] => html(page(cfg, "Notes", "notes", &format!("<div id=docwrap>{}</div>",
            md::render(&doc::index(cfg).text, &note_base(cfg))), false), 200),

        ["m", slug] => match doc::get(cfg, slug) {
            Some(d) => { let t = d.title.clone();
                         html(page(cfg, &t, "notes", &doc_fragment(cfg, &d), false), 200) }
            None => gone(cfg, &format!("no published note called {}", slug)),
        },

        // a vault note by its own name, which is what a wikilink carries; `?h=` is one section
        ["n", name] => note_page(cfg, name, &qstr("h")),

        // unchanged -> 204 (after holding, when asked to wait): the page keeps its DOM and its place
        ["f", "doc", slug] => live_doc(cfg, || doc::get(cfg, slug), qnum("v"), qnum("wait") > 0,
            |d| doc_fragment(cfg, d)),
        ["f", "note", name] => { let h = qstr("h");
            live_doc(cfg, || doc::note(cfg, name), qnum("v"), qnum("wait") > 0, |d| note_fragment(cfg, d, name, &h)) }

        ["d"] => html(diag_page(cfg), 200),

        ["x", "send"] if post => {
            // a card's first send registers it: who it is about, so a later `facet answer`
            // (and a fix it attaches) knows where to land, without the id having to carry that
            let id = field(&f, "id");
            if !id.is_empty() {
                let sd = crate::optchat::engine::state_dir(&crate::optchat::engine::dir());
                cards::register(&sd, &id, &field(&f, "note"), field(&f, "line").parse().unwrap_or(0));
            }
            let text = field(&f, "text");
            let t = text.trim();
            if t == "/model" || t.starts_with("/model ") {
                return html(md::esc(&crate::tg::model_cmd(&t[6..])), 200);
            }
            match tell::tell(cfg, &text, "reader", field(&f, "later") == "1") {
                Ok(_) => html("sent".into(), 200),
                Err(e) => html(format!("not sent: {}", md::esc(&e)), 200),
            }
        }

        // taking a card off the page for good (its answers stay where `facet answer` put them)
        ["x", "hide"] if post => {
            cards::hide(&crate::optchat::engine::state_dir(&crate::optchat::engine::dir()), &field(&f, "id"));
            html(String::new(), 204)
        }

        ["x", "diag"] if post => {
            let (code, note) = (field(&f, "code"), field(&f, "note"));
            let r = match field(&f, "do").as_str() {
                "apply" => diag::apply(cfg, &code),
                "dismiss" => diag::dismiss(cfg, &code, &note),
                "discuss" => match diag::find(cfg, &code) {
                    Some(d) => {
                        let text = format!("About my comment on [[{}]] L{} ({}): {}\n\nThe text there now:\n\n{}\n\n{}",
                            d.note.trim_end_matches(".md"), d.line, d.code, d.message,
                            diag::context(cfg, &d, 2), note);
                        tell::tell(cfg, &text, "comment", false).map(|_| "sent to the conversation".into())
                    }
                    None => Err("that diagnostic is gone".into()),
                },
                _ => Err("?".into()),
            };
            html(match r { Ok(m) => md::esc(&m), Err(e) => format!("no: {}", md::esc(&e)) }, 200)
        }

        ["static", "htmx.js"] => Response::from_string(include_str!("static/htmx.min.js"))
            .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/javascript"[..]).unwrap())
            .with_header(Header::from_bytes(&b"Cache-Control"[..], &b"max-age=86400"[..]).unwrap()),

        _ => gone(cfg, &format!("nothing at /{}", rest.join("/"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_changes_version_when_a_note_it_embeds_does() {
        let d = std::env::temp_dir().join(format!("facet-docver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.md"), "---\nfacet: t\n---\ntext\n\n![[b]]\n").unwrap();
        std::fs::write(d.join("b.md"), "lemma\n\n![[c]]\n").unwrap();
        std::fs::write(d.join("c.md"), "deep\n").unwrap();
        let cfg = Cfg(serde_json::json!({"vault": d.to_string_lossy(), "store": d.join("s").to_string_lossy()}));
        doc::forget();
        let page = doc::get(&cfg, "t").unwrap();
        let files: Vec<_> = doc_files(&cfg, &page).iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(files, ["a.md", "b.md", "c.md"], "the embeds of an embed count");
        let v0 = doc_version(&cfg, &page);
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(d.join("c.md"), "deeper\n").unwrap();
        assert!(doc_version(&cfg, &page) > v0);
        let _ = std::fs::remove_dir_all(&d);
    }
}
