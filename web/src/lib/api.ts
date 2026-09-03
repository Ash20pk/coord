import { RELAY_WS } from './relay';
import { supabase } from './supabase';

/**
 * The relay is the source of truth for anything an agent needs when the
 * network is bad: the event log, live claims, and agent-token hashes. It
 * accepts either an agent token or a signed-in person's Supabase access
 * token, so the console authenticates as the person, never as a machine.
 */
export async function accessToken(): Promise<string> {
  if (!supabase) throw new Error('Sign-in is not configured on this deployment.');
  const { data } = await supabase.auth.getSession();
  const t = data.session?.access_token;
  if (!t) throw new Error('Your session has expired. Sign in again.');
  return t;
}

export async function api<T>(path: string, opts: RequestInit = {}): Promise<T> {
  const token = await accessToken();
  const headers: Record<string, string> = { Authorization: `Bearer ${token}` };
  if (opts.body) headers['Content-Type'] = 'application/json';
  const r = await fetch(path, { ...opts, headers: { ...headers, ...(opts.headers as object) } });
  const body = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error((body as { error?: string }).error || `${r.status} ${r.statusText}`);
  return body as T;
}

export async function relaySocket(): Promise<WebSocket> {
  const token = await accessToken();
  return new WebSocket(`${RELAY_WS}?token=${encodeURIComponent(token)}`);
}

export type RelayEvent = {
  type: string;
  ts?: number;
  seq?: number;
  session?: string;
  user?: string;
  path?: string;
  text?: string;
  to?: string;
  branch?: string;
  holder_user?: string;
  intent?: string;
  lease_until?: number;
};

export type ClaimRow = { session: string; user?: string; path: string; lease_until?: number; intent?: string };
export type SessionRow = { session: string; user?: string; branch?: string; intent?: string };

export type TeamPayload = {
  team_id: string;
  team: string;
  tokens: Array<{ id: string; label: string; created_ts: number; last_seen_ts: number | null; revoked: boolean }>;
  repos: Array<{ repo: string; last_seen_ts?: number | null }>;
  token_id?: string;
};
