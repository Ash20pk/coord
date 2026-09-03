// xterm and its fit addon are loaded from a CDN as classic scripts, so they
// arrive as globals rather than imports.
declare const Terminal: any;
declare const FitAddon: any;

// A hosted relay requires a token. A browser cannot set headers on these,
// so it travels as ?token= — taken from this page's own URL, and remembered
// so a reload does not need it again.
const TOKEN = (() => {
  const q = new URLSearchParams(location.search).get('token');
  try {
    if (q) { sessionStorage.setItem('knootToken', q); return q; }
    return sessionStorage.getItem('knootToken') || '';
  } catch (_) { return q || ''; }
})();
const withTok = (u) => TOKEN ? u + (u.includes('?') ? '&' : '?') + 'token=' + encodeURIComponent(TOKEN) : u;

const $ = id => document.getElementById(id);
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const hhmm = ts => new Date(ts).toLocaleTimeString([], {hour12:false});
const short = s => String(s).slice(0,8);

let repo = null, sessions = new Map(), claims = [], agents = [];
let stats = {writes:0, blocked:0, ungated:0};
const blockedFlash = new Map();   // agent name -> until timestamp

const THEME = {
  background:'#12161a', foreground:'#e8ecef', cursor:'#3b7bff',
  black:'#12161a', red:'#ff4a1f', green:'#19a974', yellow:'#f0b429',
  blue:'#3b7bff', magenta:'#b48eff', cyan:'#39c5cf', white:'#e8ecef',
  brightBlack:'#7d868f', brightRed:'#ff7b5c', brightGreen:'#3fc98f',
  brightYellow:'#f5c85c', brightBlue:'#6d9cff', brightMagenta:'#cbb0ff',
  brightCyan:'#5ed5dd', brightWhite:'#ffffff',
};

function mountTerm(idx, name){
  const wrap = document.createElement('div');
  wrap.className = 'term';
  wrap.innerHTML = `<div class="term-bar">
      <span class="who">${esc(name)}</span><span class="tag">Claude Code</span>
      <span class="holding" id="hold-${idx}"><span class="chip idle">holding nothing</span></span>
    </div><div class="screen" id="screen-${idx}"></div>`;
  $('terms').appendChild(wrap);

  const term = new Terminal({
    fontFamily:"'Geist Mono', ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
    fontSize:12, lineHeight:1.15, cursorBlink:true, scrollback:5000,
    theme:THEME, allowProposedApi:true,
  });
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open($(`screen-${idx}`));

  const proto = location.protocol === 'https:' ? 'wss://' : 'ws://';
  let ws, dead = false;
  const open = () => {
    ws = new WebSocket(withTok(`${proto}${location.host}/term/ws/${idx}`));
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => { fitNow(); };
    ws.onmessage = e => term.write(new Uint8Array(e.data));
    ws.onclose = () => { if (!dead) setTimeout(open, 1500); };
  };
  const fitNow = () => {
    try { fit.fit(); } catch {}
    if (ws && ws.readyState === 1)
      ws.send(JSON.stringify({cols: term.cols, rows: term.rows}));
  };
  term.onData(d => { if (ws && ws.readyState === 1) ws.send(new TextEncoder().encode(d)); });
  new ResizeObserver(() => fitNow()).observe($(`screen-${idx}`));
  open();
  return {term, fitNow};
}

async function boot(){
  const info = await fetch(withTok('/api/terms')).then(r => r.json()).catch(() => ({agents:[]}));
  agents = info.agents || [];
  if (!agents.length){
    $('terms').innerHTML = '<div class="empty">No terminals. Start the relay with --lab-dir to host agents here.</div>';
  } else {
    const box = $('terms');
    if (agents.length > 2){
      box.classList.add('quad');
      box.style.gridTemplateRows = `repeat(${Math.ceil(agents.length/2)}, 1fr)`;
    } else {
      box.style.gridTemplateRows = `repeat(${agents.length}, 1fr)`;
    }
    agents.forEach((n, i) => mountTerm(i, n));
  }

  const repos = await fetch(withTok('/api/repos')).then(r => r.json()).catch(() => []);
  repo = repos[0];
  $('repo').textContent = info.dir ? `${info.dir}` : (repo || '');
  if (repo){ await history(); connect(); }
}

async function history(){
  const evs = await fetch(withTok('/api/events?repo=' + encodeURIComponent(repo))).then(r => r.json()).catch(() => []);
  for (const e of evs) apply(e, false);
  render();
}

function connect(){
  const proto = location.protocol === 'https:' ? 'wss://' : 'ws://';
  const ws = new WebSocket(withTok(proto + location.host + '/ws'));
  ws.onopen = () => {
    $('dot').classList.add('on'); $('dot').textContent = 'live'; $('live-tag').textContent = 'live'; $('live-tag').className = 'tag live';
    ws.send(JSON.stringify({type:'hello', repo, daemon:'lab-web'}));
  };
  ws.onmessage = m => {
    const msg = JSON.parse(m.data);
    if (msg.type === 'welcome'){
      sessions = new Map((msg.sessions||[]).map(s => [s.session, s]));
      claims = msg.claims || [];
    } else if (msg.type === 'event'){
      apply(msg.event, true);
    }
    render();
  };
  ws.onclose = () => {
    $('dot').classList.remove('on'); $('dot').textContent = 'reconnecting'; $('live-tag').textContent = 'reconnecting'; $('live-tag').className = 'tag';
    setTimeout(connect, 2000);
  };
}

function apply(e, live){
  const t = e.type;
  if (t === 'session_started')
    sessions.set(e.session, {session:e.session, user:e.user, branch:e.branch, intent:'', last_seen:e.ts});
  else if (t === 'intent_declared'){
    const s = sessions.get(e.session); if (s){ s.intent = e.text; s.last_seen = e.ts; }
  } else if (t === 'claim_acquired'){
    const c = claims.find(c => c.session === e.session && c.path === e.path);
    if (c) c.lease_until = e.lease_until;
    else claims.push({session:e.session, user:e.user, path:e.path, lease_until:e.lease_until});
  } else if (t === 'claim_released')
    claims = claims.filter(c => !(c.session === e.session && c.path === e.path));
  else if (t === 'file_written') stats.writes++;
  else if (t === 'claim_denied'){
    stats.blocked++;
    if (live) blockedFlash.set(e.user, Date.now() + 8000);
  }
  else if (t === 'ungated_write') stats.ungated++;
  else if (t === 'session_ended'){
    sessions.delete(e.session);
    claims = claims.filter(c => c.session !== e.session);
  }
  feed(e, live);
}

const KIND = {
  session_started:['joined','session_started'], intent_declared:['intent','intent_declared'],
  claim_acquired:['claim','claim_acquired'], claim_released:['released','claim_released'],
  path_freed:['freed','path_freed'], message:['freed','message'],
  file_written:['wrote','file_written'], claim_denied:['blocked','claim_denied'],
  cross_branch_overlap:['merge','cross_branch_overlap'],
  ungated_write:['ungated','ungated_write'], session_ended:['left','session_ended'],
};

function feed(e, live){
  const [cls, label] = KIND[e.type] || ['intent', e.type];
  const who = e.user || sessions.get(e.session)?.user || short(e.session);
  let detail = '';
  if (e.type === 'intent_declared') detail = e.text;
  else if (e.type === 'claim_denied') detail = `${e.path}, held by ${e.holder_user}`;
  else if (e.type === 'ungated_write') detail = `${e.path}, wrote over ${e.holder_user}`;
  else if (e.type === 'message') detail = `to ${e.to || 'all'}: ${e.text || ''}`;
  else if (e.path) detail = e.path;
  else if (e.type === 'session_started') detail = e.branch;

  const row = document.createElement('div');
  row.className = 'ev ' + (e.type === 'claim_denied' ? 'blocked' : e.type === 'ungated_write' ? 'ungated' : '') + (live ? ' new' : '');
  row.innerHTML = `<time>${hhmm(e.ts || Date.now())}</time><span class="u">${esc(who)}</span>`
    + `<span class="k ${cls}">${label}</span><span class="d">${esc(detail)}</span>`;
  const f = $('feed');
  if (f.querySelector('.empty')) f.innerHTML = '';
  f.appendChild(row);
  while (f.children.length > 400) f.removeChild(f.firstChild);
  f.scrollTop = f.scrollHeight;
}

function render(){
  const now = Date.now();
  claims = claims.filter(c => c.lease_until > now);
  $('s-claims').textContent = claims.length;
  $('s-writes').textContent = stats.writes;
  $('s-blocked').textContent = stats.blocked;
  $('s-ungated').textContent = stats.ungated;
  $('w-blocked').className = stats.blocked ? 'hot' : '';
  $('w-ungated').className = stats.ungated ? 'warn' : '';

  agents.forEach((name, i) => {
    const box = $(`hold-${i}`); if (!box) return;
    const mine = claims.filter(c => c.user === name);
    const flash = (blockedFlash.get(name) || 0) > now;
    let html = mine.length
      ? mine.map(c => `<span class="chip">${esc(c.path)} ${Math.max(0, Math.round((c.lease_until-now)/60000))} min</span>`).join('')
      : '<span class="chip idle">holding nothing</span>';
    if (flash) html = '<span class="chip blocked">blocked</span>' + html;
    box.innerHTML = html;
  });
}

setInterval(render, 1000);
boot();
