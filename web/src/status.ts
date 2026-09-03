import { RELAY_WS, esc, WS_SCHEME } from './lib/relay';

type Verdict = 'up' | 'down' | 'part';
type Check = { name: string; note: string; run: () => Promise<{ verdict: Verdict; detail: string }> };

const line = document.querySelector('#relay-line');
if (line) line.textContent = `relay ${RELAY_WS}`;

const timed = async <T>(f: () => Promise<T>): Promise<[T, number]> => {
  const t0 = performance.now();
  const v = await f();
  return [v, Math.round(performance.now() - t0)];
};

/** A 401 is a healthy answer here: it proves the surface is up and guarded. */
const guarded = (status: number): boolean => status === 200 || status === 401 || status === 403;

const CHECKS: Check[] = [
  {
    name: 'Web',
    note: 'The site, docs and console are served by the relay binary itself.',
    run: async () => {
      const r = await fetch('/', { method: 'HEAD', cache: 'no-store' });
      return { verdict: r.ok ? 'up' : 'down', detail: r.ok ? 'serving' : `HTTP ${r.status}` };
    },
  },
  {
    name: 'Event log',
    note: 'Where claims, denials and messages are read back from.',
    run: async () => {
      const r = await fetch('/api/events?repo=__status__&limit=1', { cache: 'no-store' });
      return guarded(r.status)
        ? { verdict: 'up', detail: r.status === 200 ? 'responding' : 'responding, authenticated' }
        : { verdict: 'down', detail: `HTTP ${r.status}` };
    },
  },
  {
    name: 'Team API',
    note: 'Token minting and revocation for your machines.',
    run: async () => {
      const r = await fetch('/api/team', { cache: 'no-store' });
      return guarded(r.status)
        ? { verdict: 'up', detail: 'responding, authenticated' }
        : { verdict: 'down', detail: `HTTP ${r.status}` };
    },
  },
  {
    name: 'Realtime',
    note: 'The websocket every daemon holds open for claims and briefs.',
    run: () => new Promise((resolve) => {
      let done = false;
      const finish = (verdict: Verdict, detail: string) => {
        if (done) return;
        done = true;
        try { sock.close(); } catch { /* already closing */ }
        resolve({ verdict, detail });
      };
      const sock = new WebSocket(`${WS_SCHEME}://${location.host}/ws`);
      // An immediate close is what an unauthenticated probe should get from a
      // relay that requires a token; it still proves the socket is listening.
      sock.onopen = () => finish('up', 'accepting connections');
      sock.onerror = () => finish('down', 'refused');
      sock.onclose = (e) => finish(e.code === 1000 || e.code === 1008 ? 'up' : 'down',
        e.code === 1008 ? 'listening, authenticated' : e.code === 1000 ? 'accepting connections' : 'refused');
      setTimeout(() => finish('down', 'timed out'), 5000);
    }),
  },
];

const box = document.querySelector('#checks')!;

async function runAll(): Promise<void> {
  box.innerHTML = CHECKS.map((c) => `<div class="check" data-name="${esc(c.name)}">
    <span class="mark"></span>
    <div><h3>${esc(c.name)}</h3><p>${esc(c.note)}</p></div>
    <span class="verdict">checking</span><span class="ms"></span></div>`).join('');

  const verdicts: Verdict[] = [];
  await Promise.all(CHECKS.map(async (c) => {
    const el = box.querySelector(`[data-name="${CSS.escape(c.name)}"]`)!;
    let result: { verdict: Verdict; detail: string };
    let ms = 0;
    try {
      [result, ms] = await timed(c.run);
    } catch (e) {
      result = { verdict: 'down', detail: (e as Error).message };
    }
    verdicts.push(result.verdict);
    el.querySelector('.mark')!.className = `mark ${result.verdict}`;
    const v = el.querySelector('.verdict')!;
    v.className = `verdict ${result.verdict}`;
    v.textContent = result.detail;
    el.querySelector('.ms')!.textContent = ms ? `${ms} ms` : '';
  }));

  const down = verdicts.filter((v) => v === 'down').length;
  const dot = document.querySelector('#banner-dot')!;
  const text = document.querySelector('#banner-text')!;
  if (!down) { dot.className = 'dot up'; text.textContent = 'All systems operational'; }
  else if (down === verdicts.length) { dot.className = 'dot down'; text.textContent = 'The relay is unreachable'; }
  else { dot.className = 'dot part'; text.textContent = `${down} of ${verdicts.length} checks failing`; }
  document.querySelector('#checked')!.textContent =
    `checked ${new Date().toLocaleTimeString([], { hour12: false })}`;
}

void runAll();
setInterval(() => { void runAll(); }, 30000);
