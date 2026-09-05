import { api, relaySocket, type ClaimRow, type RelayEvent, type SessionRow } from './api';

/**
 * A live view of one repository: backfill from the event log, then follow the
 * websocket. Reconnects quietly, because a relay restart is not an error the
 * person needs to act on.
 */
export class LiveRepo {
  claims: ClaimRow[] = [];
  sessions = new Map<string, SessionRow>();
  events: RelayEvent[] = [];

  private ws: WebSocket | null = null;
  private gen = 0;
  private repo: string | null = null;
  private stopped = false;

  constructor(
    private onEvent: (e: RelayEvent) => void,
    private onState: () => void,
    private onConn: (up: boolean | null) => void,
  ) {}

  async open(repo: string): Promise<void> {
    const gen = ++this.gen;
    this.repo = repo;
    this.close(false);
    this.claims = [];
    this.sessions.clear();
    this.events = [];

    try {
      const hist = await api<RelayEvent[]>(`/api/events?repo=${encodeURIComponent(repo)}&limit=300`);
      if (gen !== this.gen) return;
      this.events = hist;
    } catch {
      /* an empty backfill is not fatal; the socket still carries live events */
    }
    this.onState();

    let sock: WebSocket;
    try {
      sock = await relaySocket();
    } catch {
      this.onConn(false);
      return;
    }
    if (gen !== this.gen) { sock.close(); return; }
    this.ws = sock;

    sock.onopen = () => {
      if (gen !== this.gen) { sock.close(); return; }
      this.onConn(true);
      sock.send(JSON.stringify({ type: 'hello', repo, daemon: 'console' }));
    };
    sock.onmessage = (ev) => {
      if (gen !== this.gen) return;
      let m: { type?: string; claims?: ClaimRow[]; sessions?: SessionRow[]; event?: RelayEvent };
      try { m = JSON.parse(ev.data as string); } catch { return; }
      if (m.type === 'welcome') {
        this.claims = m.claims ?? [];
        this.sessions = new Map((m.sessions ?? []).map((s) => [s.session, s]));
        this.onState();
      } else if (m.type === 'event' && m.event) {
        this.apply(m.event);
        this.events.push(m.event);
        if (this.events.length > 500) this.events.shift();
        this.onEvent(m.event);
        this.onState();
      }
    };
    sock.onerror = () => { if (gen === this.gen) this.onConn(false); };
    sock.onclose = () => {
      if (gen !== this.gen || this.stopped) return;
      this.onConn(false);
      setTimeout(() => {
        if (gen === this.gen && this.repo && !this.stopped) void this.open(this.repo);
      }, 3000);
    };
  }

  close(permanent = true): void {
    if (permanent) { this.stopped = true; this.gen++; }
    if (this.ws) { try { this.ws.close(); } catch { /* already gone */ } this.ws = null; }
  }

  /** Mirror just enough of the relay's state to render presence. */
  private apply(e: RelayEvent): void {
    switch (e.type) {
      case 'claim_acquired':
        this.claims = this.claims.filter((c) => c.path !== e.path);
        this.claims.push({ session: e.session!, user: e.user, path: e.path!, intent: e.intent, lease_until: e.lease_until });
        break;
      case 'claim_released':
      case 'path_freed':
        this.claims = this.claims.filter((c) => c.path !== e.path);
        break;
      case 'session_started':
        this.sessions.set(e.session!, { session: e.session!, user: e.user, branch: e.branch, intent: '' });
        break;
      case 'intent_declared': {
        const s = this.sessions.get(e.session!);
        if (s) s.intent = e.text ?? '';
        break;
      }
      case 'session_ended':
        this.sessions.delete(e.session!);
        this.claims = this.claims.filter((c) => c.session !== e.session);
        break;
    }
  }
}

export const EVENT_CLASS: Record<string, string> = {
  claim_acquired: 'held',
  claim_denied: 'blocked',
  claim_released: 'plain',
  path_freed: 'wire',
  message: 'wire',
  ungated_write: 'warn',
  cross_branch_overlap: 'warn',
  // The collisions a claim cannot see. `create_collision` is red because two
  // agents creating one file is work already lost; the rest are warnings
  // because somebody was told in time.
  create_collision: 'blocked',
  duplicate_intent: 'blocked',
  stale_read: 'warn',
  path_removed: 'warn',
};

export function eventDetail(e: RelayEvent): string {
  switch (e.type) {
    case 'claim_denied': return `${e.path}, held by ${e.holder_user ?? 'unknown'}`;
    case 'ungated_write': return `${e.path}, over ${e.holder_user ?? 'a peer'}’s claim`;
    case 'intent_declared': return `“${e.text ?? ''}”`;
    case 'message': return `to ${e.to ?? 'all'}: ${e.text ?? ''}`;
    case 'session_started': return `joined on ${e.branch ?? 'unknown branch'}`;
    case 'session_ended': return 'left';
    case 'stale_read':
      return `${e.path}, read before ${e.peer_user ?? 'a peer'} changed it`;
    case 'create_collision':
      return `${e.path}, already created by ${e.peer_user ?? 'a peer'}`;
    case 'duplicate_intent':
      return `same task as ${e.peer_user ?? 'a peer'}: “${e.peer_text ?? ''}”`;
    case 'path_removed':
      return `${e.path} ${e.moved ? 'moved away' : 'deleted'}`;
    default: return e.path ?? '';
  }
}

export const ago = (ms: number | null | undefined): string => {
  if (!ms) return 'never';
  const d = Date.now() - ms;
  if (d < 60e3) return 'just now';
  if (d < 3600e3) return `${Math.floor(d / 60e3)} min ago`;
  if (d < 86400e3) return `${Math.floor(d / 3600e3)} h ago`;
  return `${Math.floor(d / 86400e3)} d ago`;
};
