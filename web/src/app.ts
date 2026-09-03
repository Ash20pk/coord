import { RELAY_WS, esc, wireCopyButtons } from './lib/relay';
import { api, type TeamPayload, type RelayEvent } from './lib/api';
import { LiveRepo, EVENT_CLASS, eventDetail, ago } from './lib/live';
import { configured, supabase, loadTeam, createTeam, type Team } from './lib/supabase';

const $ = <T extends Element = HTMLElement>(sel: string, root: ParentNode = document): T | null =>
  root.querySelector<T>(sel);

const authEl = $('#auth')!;
const shellEl = $('#shell')!;
const bootEl = $('#booting')!;
const viewEl = $('#view')!;

let team: Team | null = null;
let role = 'member';
let relayTeam: TeamPayload | null = null;
let live: LiveRepo | null = null;
let currentRepo: string | null = null;

/* ------------------------------------------------------------------ *
 * Auth
 * ------------------------------------------------------------------ */
type Mode = 'signin' | 'signup';
let mode: Mode = location.hash === '#signup' ? 'signup' : 'signin';

function paintAuthMode(): void {
  const signup = mode === 'signup';
  $('#auth-title')!.textContent = signup ? 'Create your account' : 'Sign in';
  $('#auth-sub')!.textContent = signup
    ? 'A team, an agent token, and a live log of every session. No card needed.'
    : 'Manage your team, agent tokens and live sessions.';
  $('#auth-go')!.textContent = signup ? 'Create account' : 'Sign in';
  $('#auth-switch')!.textContent = signup ? 'I already have an account' : 'Create an account';
  ($('#team-field') as HTMLElement).hidden = !signup;
  ($('#auth-team') as HTMLInputElement).required = signup;
  ($('#auth-password') as HTMLInputElement).autocomplete = signup ? 'new-password' : 'current-password';
}

function authMessage(kind: 'err' | 'ok' | 'clear', text = ''): void {
  const err = $('#auth-err') as HTMLElement;
  const ok = $('#auth-ok') as HTMLElement;
  err.hidden = kind !== 'err';
  ok.hidden = kind !== 'ok';
  if (kind === 'err') err.textContent = text;
  if (kind === 'ok') ok.textContent = text;
}

function showAuth(): void {
  bootEl.hidden = true;
  shellEl.hidden = true;
  authEl.hidden = false;
  paintAuthMode();
  if (!configured) {
    authMessage('err', 'Sign-in is not configured on this deployment. Set VITE_SUPABASE_URL and VITE_SUPABASE_PUBLISHABLE_KEY at build time, or run your own relay and use an agent token.');
    ($('#auth-go') as HTMLButtonElement).disabled = true;
  }
}

$('#auth-switch')!.addEventListener('click', () => {
  mode = mode === 'signup' ? 'signin' : 'signup';
  location.hash = mode === 'signup' ? '#signup' : '';
  authMessage('clear');
  paintAuthMode();
});

$('#auth-reset')!.addEventListener('click', async () => {
  const email = ($('#auth-email') as HTMLInputElement).value.trim();
  if (!email) { authMessage('err', 'Enter your email address first, then choose Forgot password.'); return; }
  try {
    const { error } = await supabase!.auth.resetPasswordForEmail(email, {
      redirectTo: `${location.origin}/app/`,
    });
    if (error) throw new Error(error.message);
    authMessage('ok', `Check ${email} for a link to set a new password.`);
  } catch (e) {
    authMessage('err', (e as Error).message);
  }
});

$('#auth-form')!.addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const btn = $('#auth-go') as HTMLButtonElement;
  const email = ($('#auth-email') as HTMLInputElement).value.trim();
  const password = ($('#auth-password') as HTMLInputElement).value;
  const teamName = ($('#auth-team') as HTMLInputElement).value.trim();
  authMessage('clear');
  btn.disabled = true;
  btn.textContent = mode === 'signup' ? 'Creating account' : 'Signing in';
  try {
    const sb = supabase!;
    if (mode === 'signup') {
      const { data, error } = await sb.auth.signUp({ email, password });
      if (error) throw new Error(error.message);
      if (!data.session) {
        authMessage('ok', `Check ${email} to confirm your address, then sign in.`);
        mode = 'signin';
        paintAuthMode();
        return;
      }
      await createTeam(teamName || `${email.split('@')[0]}'s team`);
    } else {
      const { error } = await sb.auth.signInWithPassword({ email, password });
      if (error) throw new Error(error.message);
    }
    await boot();
  } catch (e) {
    authMessage('err', (e as Error).message);
  } finally {
    btn.disabled = false;
    paintAuthMode();
  }
});

$('#signout')!.addEventListener('click', async () => {
  live?.close();
  await supabase?.auth.signOut();
  team = null;
  location.hash = '';
  showAuth();
});

/* ------------------------------------------------------------------ *
 * Views
 * ------------------------------------------------------------------ */
const ROUTES = ['sessions', 'repositories', 'tokens', 'team', 'settings'] as const;
type Route = (typeof ROUTES)[number];

function route(): Route {
  const h = location.hash.replace('#', '') as Route;
  return ROUTES.includes(h) ? h : 'sessions';
}

function paintTabs(): void {
  const r = route();
  for (const a of document.querySelectorAll<HTMLAnchorElement>('.tabs a')) {
    a.classList.toggle('on', a.getAttribute('href') === `#${r}`);
  }
}

function render(): void {
  paintTabs();
  live?.close();
  live = null;
  switch (route()) {
    case 'sessions': return viewSessions();
    case 'repositories': return viewRepositories();
    case 'tokens': return viewTokens();
    case 'team': return viewTeam();
    case 'settings': return viewSettings();
  }
}

/* ---- sessions: the live instrument ---- */
function viewSessions(): void {
  const repos = relayTeam?.repos ?? [];
  viewEl.innerHTML = `
    <div class="page">
      <div class="page-head">
        <div>
          <h1>Sessions</h1>
          <p>Every agent currently working a repository your team has connected, and the event log behind them.</p>
        </div>
      </div>
      <div class="panel">
        <div class="panel-head">
          <h2>Live</h2>
          ${repos.length
            ? `<select class="picker" id="repo-pick" aria-label="Repository">${repos
                .map((r) => `<option value="${esc(r.repo)}">${esc(r.repo)}</option>`).join('')}</select>`
            : ''}
          <span class="state" id="conn">idle</span>
          <div class="right"><div class="counts" id="counts"></div></div>
        </div>
        <div id="presence"></div>
        <div class="log" id="log">
          <div class="row h"><span>time</span><span>agent</span><span>event</span><span>detail</span></div>
          <div id="log-rows"></div>
        </div>
      </div>
    </div>`;

  if (!repos.length) {
    $('#log-rows')!.innerHTML = emptyRepoHtml();
    wireCopyButtons(viewEl);
    return;
  }

  const pick = $('#repo-pick') as HTMLSelectElement;
  if (currentRepo && repos.some((r) => r.repo === currentRepo)) pick.value = currentRepo;
  currentRepo = pick.value;
  pick.addEventListener('change', () => { currentRepo = pick.value; startLive(); });
  startLive();
}

function emptyRepoHtml(): string {
  return `<div class="empty">
    No repository has connected yet. Enrol one where your agents run, then start the daemon.
    <div class="cmd-row"><code>knoot init --relay ${esc(RELAY_WS)}</code><button class="copy" type="button">Copy</button></div>
    <div class="cmd-row"><code>knoot daemon</code><button class="copy" type="button">Copy</button></div>
  </div>`;
}

function startLive(): void {
  const rowsEl = $('#log-rows')!;
  const logEl = $('#log')!;
  live?.close();
  live = new LiveRepo(
    (e) => appendRow(e, rowsEl, logEl),
    () => { drawPresence(); if (!rowsEl.dataset.seeded) drawLog(rowsEl, logEl); },
    (up) => {
      const el = $('#conn')!;
      el.textContent = up === true ? 'live' : up === false ? 'reconnecting' : 'idle';
      el.className = 'state' + (up === true ? ' live' : up === false ? ' off' : '');
    },
  );
  void live.open(currentRepo!);
}

function rowHtml(e: RelayEvent, entering: boolean): string {
  const k = EVENT_CLASS[e.type] ?? 'plain';
  const t = e.ts ? new Date(e.ts).toLocaleTimeString([], { hour12: false }).slice(0, 8) : '';
  return `<div class="row${k === 'blocked' ? ' is-blocked' : ''}${entering ? ' enter' : ''}">
    <span class="t">${esc(t)}</span><span class="u">${esc(e.user ?? '')}</span>
    <span class="k ${k}">${esc(e.type)}</span><span class="d">${esc(eventDetail(e))}</span></div>`;
}

function drawLog(rowsEl: Element, logEl: Element): void {
  const evs = live?.events ?? [];
  rowsEl.innerHTML = evs.length
    ? evs.slice(-300).map((e) => rowHtml(e, false)).join('')
    : `<div class="empty">Connected. Nothing has happened in this repository yet.</div>`;
  (rowsEl as HTMLElement).dataset.seeded = '1';
  logEl.scrollTop = logEl.scrollHeight;
}

function appendRow(e: RelayEvent, rowsEl: Element, logEl: Element): void {
  const atBottom = logEl.scrollTop + logEl.clientHeight >= logEl.scrollHeight - 30;
  if (rowsEl.querySelector('.empty')) rowsEl.innerHTML = '';
  rowsEl.insertAdjacentHTML('beforeend', rowHtml(e, true));
  while (rowsEl.children.length > 300) rowsEl.removeChild(rowsEl.firstChild!);
  if (atBottom) logEl.scrollTop = logEl.scrollHeight;
}

function drawPresence(): void {
  const box = $('#presence');
  if (!box || !live) return;
  const rows = [...live.sessions.values()];
  const blocked = new Set(
    live.events.slice(-60).filter((e) => e.type === 'claim_denied').map((e) => e.session),
  );
  box.innerHTML = rows.length
    ? `<table class="rows">
        <thead><tr><th>Agent</th><th>Working on</th><th>Holds</th></tr></thead>
        <tbody>${rows.map((s) => {
          const holds = live!.claims.filter((c) => c.session === s.session).map((c) => c.path);
          const isBlocked = blocked.has(s.session) && !holds.length;
          const cls = holds.length ? 'holds' : isBlocked ? 'holds blocked' : 'holds none';
          const txt = holds.length ? holds.join('  ') : isBlocked ? 'blocked, waiting' : 'nothing';
          return `<tr><td class="mono">${esc(s.user ?? s.session.slice(0, 8))}</td>
            <td class="dim">${esc(s.intent || 'no stated intent yet')}</td>
            <td class="${cls}">${esc(txt)}</td></tr>`;
        }).join('')}</tbody></table>`
    : '';

  const n = rows.length, c = live.claims.length, b = blocked.size;
  const counts = $('#counts');
  if (counts) {
    counts.innerHTML =
      `<span><b>${n}</b>session${n === 1 ? '' : 's'}</span><span><b>${c}</b>claim${c === 1 ? '' : 's'}</span>` +
      (b ? `<span class="blocked"><b>${b}</b>blocked</span>` : '');
  }
}

/* ---- repositories ---- */
function viewRepositories(): void {
  const repos = relayTeam?.repos ?? [];
  viewEl.innerHTML = `
    <div class="page">
      <div class="page-head">
        <div>
          <h1>Repositories</h1>
          <p>A repository appears here the first time an agent on it reaches the relay. Nothing to create by hand.</p>
        </div>
      </div>
      <div class="panel">
        ${repos.length ? `<table class="rows">
          <thead><tr><th>Repository</th><th>Last activity</th><th></th></tr></thead>
          <tbody>${repos.map((r) => `<tr>
            <td class="mono">${esc(r.repo)}</td>
            <td class="dim">${esc(ago(r.last_seen_ts ?? null))}</td>
            <td class="right"><a class="btn quiet sm" href="#sessions" data-repo="${esc(r.repo)}">Open log</a></td>
          </tr>`).join('')}</tbody></table>` : emptyRepoHtml()}
      </div>
    </div>`;
  for (const a of viewEl.querySelectorAll<HTMLAnchorElement>('[data-repo]')) {
    a.addEventListener('click', () => { currentRepo = a.dataset.repo!; });
  }
  wireCopyButtons(viewEl);
}

/* ---- agent tokens ---- */
function viewTokens(): void {
  const tokens = relayTeam?.tokens ?? [];
  const liveCount = tokens.filter((t) => !t.revoked).length;
  viewEl.innerHTML = `
    <div class="page">
      <div class="page-head">
        <div>
          <h1>Agent tokens</h1>
          <p>Machines authenticate with tokens, not with your password. Give each machine its own so revoking one costs you nothing else. Tokens are stored as hashes and can never be shown again.</p>
        </div>
      </div>

      <div class="panel">
        <div class="panel-head"><h2>Tokens</h2><div class="right"><span class="state">${liveCount} live</span></div></div>
        <div class="panel-body">
          <div class="inline-form">
            <input id="mint-label" maxlength="40" placeholder="Label, such as laptop or ci">
            <button class="btn" id="mint-go">Mint token</button>
          </div>
          <div id="mint-out"></div>
          <div class="err" id="tok-err" hidden></div>
        </div>
        ${tokens.length ? `<table class="rows">
          <thead><tr><th>Label</th><th>Created</th><th>Last used</th><th></th></tr></thead>
          <tbody>${tokens.map((t) => `<tr>
            <td><span class="${t.revoked ? 'strike' : ''}">${esc(t.label || 'unlabelled')}</span>${
              t.id === relayTeam?.token_id ? '<span class="tag mine">this console</span>' : ''}${
              t.revoked ? '<span class="tag dead">revoked</span>' : ''}</td>
            <td class="dim">${esc(ago(t.created_ts))}</td>
            <td class="dim">${t.revoked ? '' : esc(ago(t.last_seen_ts))}</td>
            <td class="right">${t.revoked ? '' : `<button class="btn danger sm" data-revoke="${esc(t.id)}">Revoke</button>`}</td>
          </tr>`).join('')}</tbody></table>` : ''}
      </div>

      <div class="panel">
        <div class="panel-head"><h2>Use a token</h2></div>
        <div class="panel-body steps">
          <div class="step"><p>Install the binary on the machine that runs agents.</p>
            <div class="cmd-row"><code>cargo install --git https://github.com/Ash20pk/knoot</code><button class="copy" type="button">Copy</button></div></div>
          <div class="step"><p>Enrol the repository once, then commit what it writes.</p>
            <div class="cmd-row"><code>knoot init --relay ${esc(RELAY_WS)}</code><button class="copy" type="button">Copy</button></div></div>
          <div class="step"><p>Store the token on that machine and run the daemon.</p>
            <div class="cmd-row"><code>knoot login --relay ${esc(RELAY_WS)} --token &lt;token&gt;</code><button class="copy" type="button">Copy</button></div>
            <div class="cmd-row"><code>knoot daemon</code><button class="copy" type="button">Copy</button></div></div>
        </div>
      </div>
    </div>`;

  wireCopyButtons(viewEl);

  $('#mint-go')!.addEventListener('click', async () => {
    const btn = $('#mint-go') as HTMLButtonElement;
    const err = $('#tok-err') as HTMLElement;
    err.hidden = true;
    btn.disabled = true;
    try {
      const label = ($('#mint-label') as HTMLInputElement).value.trim();
      const j = await api<{ token: string }>('/api/tokens', { method: 'POST', body: JSON.stringify({ label }) });
      $('#mint-out')!.innerHTML = `<div class="reveal">
        <div class="lbl">New token. This is the only time it is readable.</div>
        <div class="val">${esc(j.token)}</div></div>`;
      await refreshRelayTeam();
    } catch (e) {
      err.textContent = (e as Error).message;
      err.hidden = false;
    } finally {
      btn.disabled = false;
    }
  });

  for (const b of viewEl.querySelectorAll<HTMLButtonElement>('[data-revoke]')) {
    b.addEventListener('click', async () => {
      if (!confirm('Revoke this token? Machines using it stop coordinating. They fail open, so their agents keep working alone.')) return;
      b.disabled = true;
      try {
        await api(`/api/tokens/${encodeURIComponent(b.dataset.revoke!)}/revoke`, { method: 'POST' });
        await refreshRelayTeam();
        viewTokens();
      } catch (e) {
        const err = $('#tok-err') as HTMLElement;
        err.textContent = (e as Error).message;
        err.hidden = false;
        b.disabled = false;
      }
    });
  }
}

/* ---- team ---- */
async function viewTeam(): Promise<void> {
  viewEl.innerHTML = `
    <div class="page">
      <div class="page-head">
        <div>
          <h1>Team</h1>
          <p>Everyone here can see the log and manage agent tokens. You are signed in as ${esc(role)}.</p>
        </div>
      </div>
      <div class="panel">
        <div class="panel-head"><h2>Members</h2></div>
        <div id="members"><div class="empty">Loading members.</div></div>
      </div>
      <div class="panel">
        <div class="panel-head"><h2>Invite a teammate</h2></div>
        <div class="panel-body">
          <p>Send them the sign-up link and the team name. They join this team when they create an account with an email on your domain.</p>
          <div class="cmd-row" style="margin-top:14px"><code>${esc(location.origin)}/app/#signup</code><button class="copy" type="button">Copy</button></div>
        </div>
      </div>
    </div>`;
  wireCopyButtons(viewEl);

  try {
    const sb = supabase!;
    const { data, error } = await sb.from('team_members').select('user_id, email, role, created_at');
    if (error) throw new Error(error.message);
    const rows = (data ?? []) as Array<{ email: string; role: string; created_at: string }>;
    $('#members')!.innerHTML = rows.length
      ? `<table class="rows"><thead><tr><th>Email</th><th>Role</th><th>Joined</th></tr></thead>
         <tbody>${rows.map((m) => `<tr><td>${esc(m.email)}</td><td class="dim">${esc(m.role)}</td>
           <td class="dim">${esc(ago(Date.parse(m.created_at)))}</td></tr>`).join('')}</tbody></table>`
      : `<div class="empty">Just you so far.</div>`;
  } catch (e) {
    $('#members')!.innerHTML = `<div class="empty">Could not load members: ${esc((e as Error).message)}</div>`;
  }
}

/* ---- settings ---- */
function viewSettings(): void {
  viewEl.innerHTML = `
    <div class="page">
      <div class="page-head"><div><h1>Settings</h1><p>Account and relay details.</p></div></div>
      <div class="panel">
        <div class="panel-head"><h2>Relay</h2></div>
        <div class="panel-body">
          <p>Your agents connect to this address. It is the same host that served this page.</p>
          <div class="cmd-row" style="margin-top:14px"><code>${esc(RELAY_WS)}</code><button class="copy" type="button">Copy</button></div>
          <p style="margin-top:16px">Team id <code>${esc(relayTeam?.team_id ?? '')}</code></p>
        </div>
      </div>
      <div class="panel">
        <div class="panel-head"><h2>Password</h2></div>
        <div class="panel-body">
          <label class="field" style="max-width:400px;margin-top:0">
            <span>New password</span>
            <input id="new-password" type="password" minlength="8" autocomplete="new-password" placeholder="At least 8 characters">
          </label>
          <button class="btn" id="pw-go" style="margin-top:14px">Change password</button>
          <div class="err" id="pw-err" hidden></div>
          <div class="ok" id="pw-ok" hidden></div>
        </div>
      </div>
    </div>`;
  wireCopyButtons(viewEl);

  $('#pw-go')!.addEventListener('click', async () => {
    const err = $('#pw-err') as HTMLElement;
    const ok = $('#pw-ok') as HTMLElement;
    err.hidden = true; ok.hidden = true;
    const password = ($('#new-password') as HTMLInputElement).value;
    if (password.length < 8) { err.textContent = 'Use at least 8 characters.'; err.hidden = false; return; }
    const { error } = await supabase!.auth.updateUser({ password });
    if (error) { err.textContent = error.message; err.hidden = false; return; }
    ok.textContent = 'Password changed.';
    ok.hidden = false;
    ($('#new-password') as HTMLInputElement).value = '';
  });
}

/* ------------------------------------------------------------------ *
 * Boot
 * ------------------------------------------------------------------ */
async function refreshRelayTeam(): Promise<void> {
  relayTeam = await api<TeamPayload>('/api/team');
}

async function boot(): Promise<void> {
  if (!configured) { showAuth(); return; }
  const { data } = await supabase!.auth.getSession();
  if (!data.session) { showAuth(); return; }

  bootEl.hidden = false;
  authEl.hidden = true;
  try {
    const found = await loadTeam();
    if (!found) {
      const email = data.session.user.email ?? 'your';
      team = await createTeam(`${email.split('@')[0]}'s team`);
      role = 'owner';
    } else {
      team = found.team;
      role = found.role;
    }
    await refreshRelayTeam();
  } catch (e) {
    bootEl.textContent = `Could not load your console: ${(e as Error).message}`;
    return;
  }

  $('#team-name')!.textContent = team!.name;
  $('#who-email')!.textContent = data.session.user.email ?? '';
  bootEl.hidden = true;
  shellEl.hidden = false;
  render();
}

addEventListener('hashchange', () => {
  if (shellEl.hidden) { mode = location.hash === '#signup' ? 'signup' : 'signin'; paintAuthMode(); return; }
  render();
});

// Repositories appear as agents connect, so refresh rather than making
// someone reload to find their first one.
setInterval(async () => {
  if (shellEl.hidden || !team) return;
  try {
    const before = (relayTeam?.repos ?? []).map((r) => r.repo).join();
    await refreshRelayTeam();
    const after = (relayTeam?.repos ?? []).map((r) => r.repo).join();
    if (before !== after && route() !== 'sessions') render();
    else if (before !== after && !currentRepo) render();
  } catch { /* transient; the next tick tries again */ }
}, 20000);

void boot();
