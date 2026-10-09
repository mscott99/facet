// The web route: Facet's main face. Server-rendered HTML, HTMX for the three dynamic needs
// (append new messages, re-render a changed note, post a message), ~20 lines of JS for KaTeX
// and the Enter key. All markdown goes through one renderer; math is extracted by the parser,
// never by a regex.
use crate::cfg::Cfg;
use crate::{cards, doc, log, md, tell};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tiny_http::{Header, Request, Response, Server};

/// How long a waiting request (`?wait=1`) holds before it answers "nothing yet". Short enough
/// that a dead connection frees its thread soon; the page just asks again.
const LONG: Duration = Duration::from_secs(25);

/// Every POST takes this for its whole run, so two from the same page land in the order sent
/// (cards.json itself is also locked, across processes, by cards.rs).
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
button{background:none;color:var(--dim);border:0;padding:0;cursor:pointer;font:12.5px var(--mono)}
button:hover{color:var(--acc)}
footer{position:fixed;bottom:0;left:0;right:0;background:var(--bg);
 padding:.6rem 1.1rem env(safe-area-inset-bottom)}
footer form{max-width:var(--measure);margin:0 auto;display:flex;gap:.8rem;align-items:center}
textarea{flex:1;resize:none;background:#101115;color:var(--fg);border:0;border-radius:4px;
 padding:.6rem .8rem;font:16px/1.5 var(--serif);max-height:40vh}
textarea:focus{outline:1px solid var(--line)}
/* A card reads like an aside: a coloured edge under the line (the colour is its kind, and
   nothing else about it differs), the quote dim at the top, the thread under it in order,
   the fix if it has one, and the composer always last. No frame. */
.say{box-sizing:border-box;max-width:100%;margin:.5rem 0 1.2rem;padding-left:1.1rem;border-left:2px solid #8fa8c880;font-size:.95em}
.say.info{border-left-color:#75767a99}.say.warn{border-left-color:var(--warn)}.say.error{border-left-color:var(--err)}
.say .hd{display:flex;gap:.6rem;align-items:baseline;margin-bottom:.35rem}
.say .hd .q{flex:1;font:11.5px/1.5 var(--mono);color:var(--dim);
 overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.say .hd .x{flex:none;font:16px/1 var(--mono);background:none;border:0;cursor:pointer;padding:0 4px;color:var(--dim)}
.say .hd .x:hover{color:var(--err)}
.say .th .t{white-space:pre-wrap;margin:.5rem 0 0}
.say .th>:first-child{margin-top:0}
.say .th .msg.talk{margin:.6rem 0 0;padding-left:.8rem;border-left:1px solid var(--line)}
.say .th .msg.talk p:last-child{margin-bottom:0}
.say .fx{margin-top:.6rem}
.say .fx pre{margin:0 0 .2rem;white-space:pre-wrap}
.say .fx .ap{color:var(--acc)}
.say .cmp{display:flex;gap:.7rem;align-items:flex-end;margin-top:.6rem}
.say .cmp textarea{flex:1;min-width:0;max-height:40vh;overflow-y:auto}
.say .cmp .go{flex:none;padding:.5rem .1rem;font:14px var(--mono);color:var(--acc)}
.say .cmp.bad textarea{outline:1px solid #c88}
.say .t.pend{opacity:.4}
.ctx{margin:2.2rem 0 .3rem}.ctx pre{margin:.3rem 0 0;white-space:pre-wrap}
form.busy textarea,form.busy button{opacity:.45}
#older{min-height:1px}
body{overflow-anchor:none}
#toast{max-width:var(--measure);margin:.3rem auto 0;font:12px var(--mono);color:var(--dim);min-height:1em}
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
// Math is rendered all at once, when the page (or a piece swapped into it) arrives: a note
// comes whole and shows whole. `data-r` marks a formula that is done.
function mrender(s){
  if(s.dataset.r)return;
  s.dataset.r=1;
  try{katex.render(s.textContent,s,{displayMode:s.dataset.mathStyle=='display',throwOnError:false})}
  catch(e){}}
function mathify(r){
  if(typeof katex=='undefined')return;
  r.querySelectorAll('span[data-math-style]').forEach(mrender);}
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
        if(h&&typing())return setTimeout(go,3000);
        if(h){w.outerHTML=h;var nw=document.getElementById('docwrap');mathify(nw);htmx.process(nw);CV=0;cardLive()}
        go();
      },function(e){if(gen==DL&&!(e&&e.name=='AbortError'))setTimeout(go,5000)});
  })();
}
function start(){
  if(document.hidden)return;
  if(document.getElementById('tail'))live();
  if(document.getElementById('docwrap'))docLive();
  cardLive();
}
document.addEventListener('visibilitychange',function(){
  if(document.hidden){LS++;DL++;CG++;if(LC)LC.abort();if(DC)DC.abort();if(CC)CC.abort()}
  else start()});
// ---- Cards ----
// Everything said about a line — a comment typed here, a review's warning, the agent's answer
// — is one card, and one component shows it wherever it appears: under its line on a note, or
// in the list on /d/.
//   .say.<kind>[data-id,note,line]   kind (comment|info|warn|error) is the edge's colour only
//     .hd   the quote, and the X (close)
//     .th   the thread, in order: what the user said (.t) and the server's (.msg.talk), each
//           with data-k, its index in the card's thread on the server
//     .fx   the fix, if the card has one: what would replace its lines, and `apply`
//     .cmp  the composer — always last: a one-row box that grows, and `send`
// Enter or `send` sends, Shift-Enter is a new line. A card nothing was said in leaves no
// trace on Escape or a click away. What is sent shows grey at once and firms up when the
// server has it; a failure takes it back out and returns the words to the box.
// Cards are kept by the server (cards.rs) and come to the page through one held request
// (`/f/cards?...&wait=1&v=`), answered when any card changes: a reply, a new card the agent
// opened, a card closed or applied elsewhere all show without a reload.
var CV=0,CC=null,CL=[],CG=0,CLOSED={};
function card(b,o){
  var d=document.createElement('div');d.className='say '+(o.kind||'comment');
  d.dataset.id=o.id;d.dataset.note=o.note;d.dataset.line=o.line;
  d.innerHTML='<div class=hd><div class=q></div><button class=x type=button title=close aria-label=close>&times;</button></div>'+
    '<div class=th></div><div class=fx hidden></div><div class=cmp><textarea rows=1></textarea><button class=go type=button>send</button></div>';
  d._n=0;d._known=0;head(d,o);
  var t=d.querySelector('textarea'),go=d.querySelector('.go');
  d.querySelector('.x').addEventListener('click',function(){drop(d)});
  // the button must not take the focus from the box (on a phone that would close the keyboard)
  go.addEventListener('pointerdown',function(e){e.preventDefault()});
  go.addEventListener('click',function(){submit(d)});
  t.addEventListener('input',function(){fit(t)});
  t.addEventListener('keydown',function(e){
    if(e.key=='Escape'){e.preventDefault();if(fresh(d))drop(d);else{t.value='';t.blur()}return}
    if(e.key!='Enter'||e.shiftKey||e.altKey||e.isComposing)return;
    e.preventDefault();e.stopPropagation();submit(d);});
  t.addEventListener('blur',function(){if(fresh(d)&&!t.value.trim())drop(d)});
  // after the line, and after any cards already under it, so a line's cards read top to bottom
  var at=b;while(at.nextElementSibling&&at.nextElementSibling.classList.contains('say'))at=at.nextElementSibling;
  at.parentNode.insertBefore(d,at.nextSibling);
  return d;
}
function head(d,o){d._quote=o.quote||'';d.querySelector('.q').textContent='L'+o.line+(o.quote?' · '+o.quote:'')}
// nothing has been said in it yet: such a card is only a box, and goes as easily as it came
function fresh(d){return !d._known&&!d.querySelector('.th').children.length}
function fit(t){t.style.height='auto';t.style.height=t.scrollHeight+'px'}
// Whatever grows a card keeps the reader where they were: if the card is above the screen,
// the page moves by as much as it grew instead of the text jumping under the reader.
function grow(d,f){
  var top=d.getBoundingClientRect().top,h=d.offsetHeight;f();
  if(top<0)scrollBy(0,d.offsetHeight-h)}
function post(u,body){
  return fetch(TOK+u,{method:'POST',headers:{'Content-Type':'application/x-www-form-urlencoded'},body:body})
    .then(function(r){return r.ok?r.json():Promise.reject('HTTP '+r.status)})
    .then(function(j){return j.ok?j:Promise.reject(j.error||'refused')})}
function toast(m){var t=document.getElementById('toast');if(t)t.textContent=m}
// The one way anything is sent from a card, the first comment or a later reply alike: the id
// travels every time, so the whole thread stays one card.
function submit(d){
  var t=d.querySelector('textarea'),said=t.value.trim(),cmp=d.querySelector('.cmp');
  if(!said){if(fresh(d))drop(d);return}
  var k=document.createElement('div');k.className='t pend';k.textContent=said;
  grow(d,function(){d.querySelector('.th').appendChild(k);t.value='';fit(t)});
  cmp.classList.remove('bad');cmp.title='';
  post('/x/card','do=say&id='+d.dataset.id+'&note='+encodeURIComponent(d.dataset.note)+'&line='+d.dataset.line+
    '&quote='+encodeURIComponent(d._quote)+'&text='+encodeURIComponent(said))
    .then(function(r){
      d._known=d._known||Date.now();
      // the held request may have brought the server's copy first: keep one
      var dup=d.querySelector('.th [data-k="'+r.k+'"]');
      if(dup&&dup!==k)k.remove();else{k.dataset.k=r.k;k.classList.remove('pend')}},
    function(err){k.remove();if(!t.value)t.value=said;fit(t);cmp.classList.add('bad');cmp.title='not sent: '+err});
}
// The X: closed on the server (off every page; kept on file), and gone from this one at once.
function drop(d){
  var id=d.dataset.id;CLOSED[id]=1;
  if(d._known)post('/x/card','do=close&id='+id).then(null,function(e){delete CLOSED[id];toast('not closed: '+e)});
  var x=document.querySelector('.ctx[data-for="'+id+'"]');if(x)x.remove();
  d.remove()}
function applyFix(d){
  post('/x/card','do=apply&id='+d.dataset.id).then(function(r){toast(r.text||'applied')},function(e){toast('not applied: '+e)})}
// What the page asks cards for: the notes whose lines it shows, or (on /d/) all of them.
function scope(){
  if(document.getElementById('cardlist'))return 'all=1';
  var ns={};
  document.querySelectorAll('[data-note]').forEach(function(e){if(!e.closest('.say'))ns[e.dataset.note]=1});
  var k=Object.keys(ns);return k.length?'notes='+encodeURIComponent(JSON.stringify(k)):null}
function cardLive(){
  var gen=++CG;if(CC)CC.abort();
  (function go(){
    var sc=scope();if(gen!=CG||!sc||document.hidden)return;
    CC=new AbortController();var t0=Date.now();
    fetch(TOK+'/f/cards?'+sc+'&wait=1&v='+CV,{signal:CC.signal})
      .then(function(r){return r.status==204?null:r.ok?r.json():Promise.reject(r.status)}).then(function(j){
        if(gen!=CG)return;
        if(j){CV=j.v;CL=j.cards;place(j.cards,t0)}
        go();
      },function(e){if(gen==CG&&!(e&&e.name=='AbortError'))setTimeout(go,5000)});
  })();
}
// The line a card belongs under: the last block of its note starting at or before its line.
function anchor(c){
  var best=null,bl=-1,first=null;
  document.querySelectorAll('[data-note="'+CSS.escape(c.note)+'"][data-line]').forEach(function(e){
    if(e.closest('.say'))return;if(!first)first=e;
    var l=+e.dataset.line;if(l<=c.line&&l>=bl){best=e;bl=l}});
  return best||first}
// On /d/, a card sits under its note's name and the lines it is about.
function ctx(list,c){
  var x=document.createElement('div');x.className='ctx';x.dataset.for=c.id;
  x.innerHTML='<div class=at><a class=wl></a> · L'+c.line+'</div><pre></pre>';
  var a=x.querySelector('a');a.textContent=c.note;a.href=TOK+'/n/'+encodeURIComponent(c.note);
  x.querySelector('pre').textContent=c.ctx||'';
  list.appendChild(x);return x}
function place(cs,t0){
  var seen={},list=document.getElementById('cardlist');
  cs.forEach(function(c){
    if(CLOSED[c.id])return;
    seen[c.id]=1;
    var d=document.querySelector('.say[data-id="'+c.id+'"]');
    if(!d){var b=list?ctx(list,c):anchor(c);if(!b)return;d=card(b,c)}
    fill(d,c,t0);
  });
  // a card closed (or applied) elsewhere goes here too — unless it is being written in, or
  // this answer was asked for before the page knew the card at all
  document.querySelectorAll('.say').forEach(function(d){
    if(!d._known||d._known>=t0||seen[d.dataset.id])return;
    if(d.querySelector('textarea').value.trim())return;
    var x=document.querySelector('.ctx[data-for="'+d.dataset.id+'"]');if(x)x.remove();d.remove()});
  var none=document.getElementById('none');if(none)none.hidden=!!document.querySelector('.say');
}
// Bring one card up to the server's: its colour, its quote, the messages it does not show yet
// (each in its place by index; a grey one of the user's firms up instead of showing twice),
// and its fix.
function fill(d,c,t0){
  if(!d._known)d._known=t0||1;
  d.className='say '+c.kind;d.dataset.line=c.line;head(d,c);
  var th=d.querySelector('.th');
  var add=c.msgs.filter(function(m){return m.k>=d._n&&!th.querySelector('[data-k="'+m.k+'"]')});
  if(add.length)grow(d,function(){add.forEach(function(m){
    var w=document.createElement('div');w.innerHTML=m.html;var n=w.firstElementChild;
    if(m.by=='user'){var ps=th.querySelectorAll('.t.pend');
      for(var i=0;i<ps.length;i++)if(!ps[i].dataset.k&&ps[i].textContent==n.textContent){
        ps[i].dataset.k=m.k;ps[i].classList.remove('pend');return}}
    var at=null;
    th.querySelectorAll('.t,.msg').forEach(function(e){
      if(!at&&(e.dataset.k?+e.dataset.k>m.k:e.classList.contains('pend')))at=e});
    th.insertBefore(n,at);mathify(n);htmx.process(n)})});
  d._n=Math.max(d._n,c.n);
  var fx=d.querySelector('.fx'),sig=c.fix?c.fix.sig:'';
  if(fx.dataset.sig!==sig){
    fx.dataset.sig=sig;fx.hidden=!c.fix;
    fx.innerHTML=c.fix?'<div class=at>fix: replaces L'+c.fix.lines+'</div><pre></pre><button class=ap type=button>apply</button>':'';
    if(c.fix){fx.querySelector('pre').textContent=c.fix.text;
      fx.querySelector('.ap').addEventListener('click',function(){applyFix(d)})}}
}
// a card being typed in would be lost to a refresh of the page under it
function typing(){var ts=document.querySelectorAll('.say textarea');
  for(var i=0;i<ts.length;i++)if(ts[i].value.trim())return true;
  return false}
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
  if(e.target.closest('a,form,button,textarea,.say,.ctx'))return;
  var b=e.target.closest('[data-line]');if(!b||!b.dataset.note)return;
  var quote=quoted(b);
  // A short id of its own, right here in the quote, is what lets a deliberate `facet answer`
  // find its way back to this card — never a talk reply its poll merely happens to catch.
  var id='c'+Date.now().toString(36)+Math.random().toString(36).slice(2,5);
  // only one card nothing has been said in is open at a time
  document.querySelectorAll('.say').forEach(function(o){if(fresh(o))o.remove()});
  var d=card(b,{id:id,note:b.dataset.note,line:+b.dataset.line,quote:quote,kind:'comment'});
  d.querySelector('textarea').focus();
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
    nav.push_str(&item("/d/", "cards", "diag"));
    nav.push_str(&item("/tree", "memory", "tree"));
    if !cfg.terminal().is_empty() { nav.push_str(&format!("<a href=\"{}\">term</a>", cfg.terminal())); }
    let n = cards::open(cfg).len();
    let model = if compose {
        crate::optchat::engine::request(&crate::optchat::engine::dir(), serde_json::json!({"op": "status"}))
            .ok().and_then(|v| v["model"].as_str().map(String::from)).unwrap_or_default()
    } else { String::new() };
    let status = format!("{}{}{}",
        if model.is_empty() { String::new() } else { format!("<span id=mdl>{}</span> · ", md::esc(&model)) },
        if tell::healthy(cfg) { "" } else { "input down · " },
        if n > 0 { format!("{} cards", n) } else { String::new() });
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

/// A note, rendered whole. Its cards are not in this markup: the page places them under their
/// lines itself (`/f/cards`), so a card can change without the note being rendered again.
fn note_html(cfg: &Cfg, d: &doc::Doc) -> String {
    let lines: Vec<&str> = d.text.split('\n').collect();
    let skip = doc::front_len(&d.text);     // frontmatter is metadata, not prose
    let (text, srcs) = doc::assemble(cfg, &home_of(d), &lines[skip.min(lines.len())..], skip);
    md::render_at(&text, &note_base(cfg), &srcs)
}

/// One `#`-section of a note: what `[[Note#Section]]` asks for, as `?h=`. Its lines carry the
/// whole file's numbers, so the note's cards land in it as they would on the whole note.
fn section_html(cfg: &Cfg, d: &doc::Doc, h: &str) -> String {
    let Some((s, start)) = doc::section_at(&d.text, h) else { return note_html(cfg, d) };
    let lines: Vec<&str> = s.split('\n').collect();
    let (text, srcs) = doc::assemble(cfg, &home_of(d), &lines, start - 1);
    format!("<h2>{}</h2>{}", md::esc(h), md::render_at(&text, &note_base(cfg), &srcs))
}

/// Any note of the vault, read-only, on the same page as a published one. Wikilinks in docs,
/// notes and messages all land here, so a name that is not in the vault must say so plainly.
fn note_page(cfg: &Cfg, name: &str, h: &str, inm: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let Some(d) = doc::note(cfg, name) else {
        return html(page(cfg, "no such note", "notes",
            &format!("<h1>no such note</h1><p class=at>{} is not in {}</p>",
                md::esc(name), md::esc(&cfg.vault().to_string_lossy())), false), 404);
    };
    html_tagged(page(cfg, &d.title, "notes", &note_fragment(cfg, &d, name, h), false), inm)
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

/// Changes when the page's text does: the note, or a note it embeds. (Cards have their own.)
fn doc_version(cfg: &Cfg, d: &doc::Doc) -> u64 {
    doc_files(cfg, d).iter().map(|p| mtime_us(p)).max().unwrap_or(0)
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
    // Woken by the vault watcher the moment any file changes (no action needed from whoever
    // edits); the 1.5s timeout is only the fallback where inotify isn't available.
    let t0 = Instant::now();
    crate::watch::ensure(&cfg.vault());
    let mut seen = crate::watch::now();
    loop {
        let Some(d) = get() else { return html(String::new(), 204) };
        if doc_version(cfg, &d) as i64 != v { return html(render(&d), 200) }
        if !wait || t0.elapsed() >= LONG { return html(String::new(), 204) }
        let n = crate::watch::wait(seen, Duration::from_millis(1500));
        if n != seen { seen = n; std::thread::sleep(Duration::from_millis(40)); } // let a burst of writes settle
    }
}

/// Every open card, under its note's name and the lines it is about — the same component as on
/// a note, placed by the page from `/f/cards?all=1` and kept live the same way.
fn cards_page(cfg: &Cfg) -> String {
    page(cfg, "Cards", "diag", "<h1>Cards</h1><p class=at id=none>Nothing open.</p><div id=cardlist></div>", false)
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
    let comments = cards::open(cfg).len();
    let mut rows: Vec<(String, &str, String)> = vec![
        (format!("{}/chat", t), "Chat", engine),
        (format!("{}/tree", t), "Memory", "the whole tree: summaries down to every message, searchable".into()),
        (format!("{}/m/", t), "Notes", format!("{} published", notes)),
        (format!("{}/d/", t), "Cards", format!("{} open", comments)),
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

/// A page that is a pure function of what it shows: tagged by its own bytes, so a browser that
/// already holds this exact page gets a 304 with no body. The page is still made every time -
/// that is a few milliseconds - so nothing can go stale; only the transfer is saved. Never used
/// for the chat, the log, or anything a POST just changed.
fn html_tagged(body: String, inm: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut h);
    let tag = format!("\"{:x}-{:x}\"", h.finish(), body.len());
    let hdr = |k: &str, v: &str| Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap();
    if inm.split(',').any(|t| t.trim() == tag) {
        return Response::from_string(String::new()).with_status_code(304)
            .with_header(hdr("ETag", &tag)).with_header(hdr("Cache-Control", "no-cache"));
    }
    html(body, 200).with_header(hdr("ETag", &tag)).with_header(hdr("Cache-Control", "no-cache"))
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
    crate::watch::ensure(&cfg.vault());
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

/// The open cards of these notes (or all of them), as the page builds them: each message
/// rendered (the user's as typed, the server's as markdown with math) with its index in the
/// thread, and the fix as the text it would put in. `v` is the version they were read at.
fn cards_json(cfg: &Cfg, notes: Option<&[String]>) -> String {
    let v = cards::version(cfg);
    let base = note_base(cfg);
    let mut cs: Vec<serde_json::Value> = cards::open(cfg).into_iter()
        .filter(|c| notes.is_none_or(|ns| ns.iter().any(|n| *n == cards::stem(c["note"].as_str().unwrap_or("")))))
        .collect();
    cs.sort_by(|a, b| a["note"].as_str().cmp(&b["note"].as_str()).then(a["line"].as_i64().cmp(&b["line"].as_i64()))
        .then(a["at"].as_str().cmp(&b["at"].as_str())));
    let out: Vec<serde_json::Value> = cs.iter().map(|c| {
        let thread = c["thread"].as_array().cloned().unwrap_or_default();
        let msgs: Vec<serde_json::Value> = thread.iter().enumerate().map(|(k, m)| {
            let text = m["text"].as_str().unwrap_or("");
            let by = if m["by"] == "user" { "user" } else { "server" };
            let html = if by == "user" { format!("<div class=t data-k={}>{}</div>", k, md::esc(text)) }
                else { format!("<div class=\"msg talk\" data-k={}>{}</div>", k, md::render(text, &base)) };
            serde_json::json!({"k": k, "by": by, "html": html})
        }).collect();
        let fix = c["fix"].as_array().filter(|f| !f.is_empty()).map(|f| {
            let text = f.iter().map(|e| e["new_text"].as_str().unwrap_or("")).collect::<Vec<_>>().join("\n…\n");
            let lines = f.iter().map(|e| {
                let (a, b) = (e["start_line"].as_i64().unwrap_or(0), e["end_line"].as_i64().unwrap_or(0));
                if a == b { a.to_string() } else { format!("{}-{}", a, b) }
            }).collect::<Vec<_>>().join(", ");
            serde_json::json!({"text": text, "lines": lines, "sig": format!("{}|{}", lines, text)})
        });
        let note = c["note"].as_str().unwrap_or("");
        let mut j = serde_json::json!({
            "id": c["id"], "note": cards::stem(note), "path": note, "line": c["line"], "kind": c["kind"],
            "quote": c["quote"], "by": c["by"], "n": thread.len(), "msgs": msgs, "fix": fix,
        });
        if notes.is_none() {
            let text = std::fs::read_to_string(cfg.vault().join(note)).unwrap_or_default();
            let ls: Vec<&str> = text.split('\n').collect();
            let (a, b) = (c["line"].as_i64().unwrap_or(1), c["end"].as_i64().unwrap_or(1));
            let lo = (a - 2).max(0) as usize;
            let hi = (b.max(a) as usize + 1).min(ls.len());
            j["ctx"] = ls[lo.min(hi)..hi].join("\n").into();
        }
        j
    }).collect();
    serde_json::json!({"v": v, "cards": out}).to_string()
}

fn json_resp(v: serde_json::Value) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(v.to_string())
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap())
        .with_header(Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap())
}

/// A page for something that is not there, in the same dark look as everything else.
fn gone(cfg: &Cfg, what: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    html(page(cfg, "Not found", "", &format!("<h1>Not found</h1><p class=at>{}</p>", md::esc(what)), false), 404)
}

/// What the user writes on a card: kept on the card (opening it, if this is its first word),
/// then told to the conversation as a card message, queued for a turn of its own. A send the
/// conversation refused is taken back off the card, so the card never shows what was not sent.
fn card_say(cfg: &Cfg, f: &[(String, String)]) -> Result<serde_json::Value, String> {
    let (id, text) = (field(f, "id"), field(f, "text"));
    if text.trim().is_empty() { return Err("empty".into()) }
    let k = match cards::get(cfg, &id) {
        Some(_) => cards::say(cfg, &id, "user", &text)?,
        None => {
            let line: i64 = field(f, "line").parse().map_err(|_| "no line".to_string())?;
            let quote = field(f, "quote");
            cards::create(cfg, cards::New { id: Some(&id), note: &field(f, "note"), at: cards::At::Line(line),
                text: &text, kind: "comment", fix: None, by: "user", quote: Some(&quote) })?;
            0
        }
    };
    let c = cards::get(cfg, &id).ok_or("the card vanished")?;
    if let Err(e) = tell::tell(cfg, &cards::message(&c, &text), "reader", true) {
        let _ = cards::unsay(cfg, &id, k);
        return Err(format!("not sent: {}", e));
    }
    Ok(serde_json::json!({"k": k}))
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
    let inm = rq.headers().iter().find(|h| h.field.equiv("If-None-Match"))
        .map(|h| h.value.as_str().to_string()).unwrap_or_default();
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

        // the cards of the notes on a page (or `all`), held with `wait` until any card changes
        // from `v`: a reply, a new card the agent opened, one closed elsewhere
        ["f", "cards"] => {
            let notes: Option<Vec<String>> = if qnum("all") > 0 { None }
                else { Some(serde_json::from_str(&qstr("notes")).unwrap_or_default()) };
            let (v, t0) = (qnum("v"), Instant::now());
            while qnum("wait") > 0 && cards::version(cfg) as i64 == v {
                if t0.elapsed() >= LONG { return html(String::new(), 204) }
                std::thread::sleep(Duration::from_millis(300));
            }
            Response::from_string(cards_json(cfg, notes.as_deref()))
                .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap())
                .with_header(Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap())
        }

        // the memory tree (folded from the files: the engine's view is the same fold), the
        // scaffold down to the view-line frontier only - a stub past that is `/f/node`'s to fetch
        ["tree"] => {
            let t = tree(cfg);
            let mut pg = t.page.lock().unwrap_or_else(|e| e.into_inner());
            let body = pg.get_or_insert_with(|| crate::optchat::browse::web(&t.s, &t.v, crate::optchat::VIEW, &cfg.token_path())).clone();
            html_tagged(body, &inm)
        }

        // a stub's first open: the immediate children of (l, i), themselves stubbed one level
        // further wherever they still have halves of their own
        ["f", "node"] => {
            let (l, i) = (qnum("l").max(0) as usize, qnum("i").max(0) as usize);
            let t = tree(cfg);
            html_tagged(crate::optchat::browse::node(&t.s, &t.v, l, i), &inm)
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
                         html_tagged(page(cfg, &t, "notes", &doc_fragment(cfg, &d), false), &inm) }
            None => gone(cfg, &format!("no published note called {}", slug)),
        },

        // a vault note by its own name, which is what a wikilink carries; `?h=` is one section
        ["n", name] => note_page(cfg, name, &qstr("h"), &inm),

        // unchanged -> 204 (after holding, when asked to wait): the page keeps its DOM and its place
        ["f", "doc", slug] => live_doc(cfg, || doc::get(cfg, slug), qnum("v"), qnum("wait") > 0,
            |d| doc_fragment(cfg, d)),
        ["f", "note", name] => { let h = qstr("h");
            live_doc(cfg, || doc::note(cfg, name), qnum("v"), qnum("wait") > 0, |d| note_fragment(cfg, d, name, &h)) }

        ["d"] => html(cards_page(cfg), 200),

        ["x", "send"] if post => {
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

        // a card, from its page: `say` (the first word opens it), `close` (the X), `apply`
        ["x", "card"] if post => {
            let id = field(&f, "id");
            let r: Result<serde_json::Value, String> = match field(&f, "do").as_str() {
                "say" => card_say(cfg, &f),
                "close" => cards::close(cfg, &id, &field(&f, "reason")).map(|m| serde_json::json!({"text": m})),
                "apply" => cards::apply(cfg, &id).map(|m| serde_json::json!({"text": m})),
                other => Err(format!("unknown action {:?}", other)),
            };
            json_resp(match r {
                Ok(mut v) => { v["ok"] = true.into(); v }
                Err(e) => serde_json::json!({"ok": false, "error": e}),
            })
        }

        // the one bundled script: kept a week, and revalidated by tag after that
        ["static", "htmx.js"] => {
            let js = include_str!("static/htmx.min.js");
            let tag = format!("\"htmx-{}\"", js.len());
            let hdr = |k: &str, v: &str| Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap();
            let r = if inm.split(',').any(|t| t.trim() == tag) { Response::from_string(String::new()).with_status_code(304) }
                    else { Response::from_string(js).with_header(hdr("Content-Type", "text/javascript")) };
            r.with_header(hdr("ETag", &tag)).with_header(hdr("Cache-Control", "max-age=604800"))
        }

        _ => gone(cfg, &format!("nothing at /{}", rest.join("/"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tagged_page_is_not_resent_to_a_browser_that_has_it() {
        let first = html_tagged("<p>hi</p>".into(), "");
        assert_eq!(first.status_code().0, 200);
        let tag = first.headers().iter().find(|h| h.field.equiv("ETag")).unwrap().value.as_str().to_string();
        assert_eq!(html_tagged("<p>hi</p>".into(), &tag).status_code().0, 304);
        assert_eq!(html_tagged("<p>changed</p>".into(), &tag).status_code().0, 200);
    }

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
