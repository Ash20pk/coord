// A hosted relay requires a token. A browser cannot set headers on these,
// so it travels as ?token=, taken from this page's own URL and remembered
// so a reload does not need it again.
const TOKEN = (() => {
  const q = new URLSearchParams(location.search).get('token');
  try {
    if (q) { sessionStorage.setItem('knootToken', q); return q; }
    return sessionStorage.getItem('knootToken') || '';
  } catch (_) { return q || ''; }
})();
const withTok = (u) => TOKEN ? u + (u.includes('?') ? '&' : '?') + 'token=' + encodeURIComponent(TOKEN) : u;

let ws, repo = new URLSearchParams(location.search).get('repo');
let sessions = new Map(), claims = [], lastWrite = new Map();
let stats = { writes: 0, blocked: 0, ungated: 0 };

const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const hhmm = (ts) => new Date(ts).toLocaleTimeString([], { hour12: false });
const short = (s) => String(s).slice(0, 8);

async function boot() {
  const repos = await fetch(withTok('/api/repos')).then((r) => r.json()).catch(() => []);
  const sel = $('repo');
  sel.innerHTML = repos.map((r) => `<option>${esc(r)}</option>`).join('') || '<option>no repositories yet</option>';
  if (!repo || !repos.includes(repo)) repo = repos[0];
  if (repo) sel.value = repo;
  sel.onchange = () => { repo = sel.value; resetView(); connect(); };
  if (repo) { await history(); connect(); }
  else $('foot').textContent = 'no repositories yet. run knoot init in a repository and start a session';
}
function resetView() {
  sessions = new Map(); claims = []; lastWrite = new Map();
  stats = { writes: 0, blocked: 0, ungated: 0 };
  $('feed').innerHTML = ''; render();
  if (ws) { ws.onclose = null; ws.close(); }
}
async function history() {
  const evs = await fetch(withTok('/api/events?repo=' + encodeURIComponent(repo))).then((r) => r.json()).catch(() => []);
  for (const e of evs) apply(e, false);
  render();
}
function connect() {
  ws = new WebSocket(withTok((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws'));
  ws.onopen = () => {
    $('dot').classList.add('on'); $('dot').textContent = 'live';
    $('live-tag').textContent = 'live'; $('live-tag').className = 'tag live';
    $('foot').textContent = 'live  ' + repo;
    ws.send(JSON.stringify({ type: 'hello', repo, daemon: 'web' }));
  };
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.type === 'welcome') {
      sessions = new Map((msg.sessions || []).map((s) => [s.session, s]));
      claims = msg.claims || [];
      render();
    } else if (msg.type === 'event') { apply(msg.event, true); render(); }
  };
  ws.onclose = () => {
    $('dot').classList.remove('on'); $('dot').textContent = 'reconnecting';
    $('live-tag').textContent = 'reconnecting'; $('live-tag').className = 'tag';
    $('foot').textContent = 'disconnected, retrying';
    setTimeout(connect, 2000);
  };
}
function apply(e, live) {
  const t = e.type;
  if (t === 'session_started') sessions.set(e.session, { session: e.session, user: e.user, branch: e.branch, intent: '', last_seen: e.ts });
  else if (t === 'intent_declared') { const s = sessions.get(e.session); if (s) { s.intent = e.text; s.last_seen = e.ts; } }
  else if (t === 'claim_acquired') {
    const c = claims.find((c) => c.session === e.session && c.path === e.path);
    if (c) c.lease_until = e.lease_until;
    else claims.push({ session: e.session, user: e.user, path: e.path, lease_until: e.lease_until, intent: e.intent });
  } else if (t === 'claim_released') claims = claims.filter((c) => !(c.session === e.session && c.path === e.path));
  else if (t === 'file_written') {
    stats.writes++; lastWrite.set(e.path, e.session);
    if (e.user) { const s = sessions.get(e.session); if (s && !s.user) s.user = e.user; }
  } else if (t === 'claim_denied') stats.blocked++;
  else if (t === 'ungated_write') stats.ungated++;
  else if (t === 'session_ended') { sessions.delete(e.session); claims = claims.filter((c) => c.session !== e.session); }
  feed(e, live);
}
const KIND = {
  session_started: ['joined', 'session_started'], intent_declared: ['intent', 'intent_declared'],
  claim_acquired: ['claim', 'claim_acquired'], claim_released: ['released', 'claim_released'],
  path_freed: ['freed', 'path_freed'], message: ['freed', 'message'],
  file_written: ['wrote', 'file_written'], claim_denied: ['blocked', 'claim_denied'],
  cross_branch_overlap: ['merge', 'cross_branch_overlap'],
  ungated_write: ['ungated', 'ungated_write'], session_ended: ['left', 'session_ended'],
};
function feed(e, live) {
  const [cls, label] = KIND[e.type] || ['intent', e.type];
  const who = e.user || (sessions.get(e.session)?.user) || short(e.session);
  let detail = '';
  if (e.type === 'intent_declared') detail = `“${e.text}”`;
  else if (e.type === 'claim_denied') detail = `${e.path}, held by ${e.holder_user}`;
  else if (e.type === 'ungated_write') detail = `${e.path}, wrote over ${e.holder_user}’s claim`;
  else if (e.type === 'message') detail = `to ${e.to || 'all'}: ${e.text || ''}`;
  else if (e.path) detail = e.path;
  else if (e.type === 'session_started') detail = 'joined on ' + e.branch;
  const row = document.createElement('div');
  row.className = 'ev ' + (e.type === 'claim_denied' ? 'blocked' : e.type === 'ungated_write' ? 'ungated' : '') + (live ? ' new' : '');
  row.innerHTML = `<time>${hhmm(e.ts || Date.now())}</time><span class="u">${esc(who)}</span><span class="k ${cls}">${label}</span><span class="d">${esc(detail)}</span>`;
  const f = $('feed');
  if (f.querySelector('.empty')) f.innerHTML = '';
  f.appendChild(row);
  while (f.children.length > 400) f.removeChild(f.firstChild);
  f.scrollTop = f.scrollHeight;
}
function render() {
  const now = Date.now();
  claims = claims.filter((c) => c.lease_until > now);
  $('s-sessions').textContent = sessions.size;
  $('s-claims').textContent = claims.length;
  $('s-writes').textContent = stats.writes;
  $('s-blocked').textContent = stats.blocked;
  $('s-ungated').textContent = stats.ungated;
  $('w-blocked').className = stats.blocked ? 'hot' : '';
  $('w-ungated').className = stats.ungated ? 'warn' : '';
  const box = $('sessions');
  if (!sessions.size) { box.innerHTML = '<div class="empty">No active sessions.</div>'; return; }
  box.innerHTML = [...sessions.values()]
    .sort((a, b) => (a.user || '').localeCompare(b.user || ''))
    .map((s) => {
      const held = claims.filter((c) => c.session === s.session);
      const chips = held.length
        ? held.map((c) => `<span class="chip">${esc(c.path)}<span class="t">${Math.max(0, Math.round((c.lease_until - now) / 60000))} min</span></span>`).join('')
        : '<span class="chip idle">holding nothing</span>';
      return `<div class="sess">
        <div class="sess-top"><span class="who">${esc(s.user)}</span><span class="sid">${short(s.session)}</span><span class="branch">${esc(s.branch)}</span></div>
        <div class="intent ${s.intent ? '' : 'none'}">${esc(s.intent || 'No stated intent yet.')}</div>
        <div class="holds">${chips}</div></div>`;
    }).join('');
}
setInterval(render, 1000);
boot();
