import { createClient, type SupabaseClient, type Session } from '@supabase/supabase-js';

/**
 * Supabase holds identity and the team records: who you are, which team you
 * belong to, and the metadata for each agent token. The event log stays on the
 * relay, because that is the thing that has to survive without a network.
 *
 * The key is injected at build time. A build without one still serves every
 * page; the console just explains that sign-in is not configured rather than
 * throwing on load.
 *
 * `sb_publishable_…` is the current browser-safe key. The legacy `anon` name
 * is still read, because Supabase keeps both working until it retires the old
 * keys at the end of 2026. Either way this is a public value: everything it
 * can reach is behind row-level security.
 */
const URL_ = import.meta.env.VITE_SUPABASE_URL as string | undefined;
const PUBLISHABLE =
  (import.meta.env.VITE_SUPABASE_PUBLISHABLE_KEY as string | undefined) ??
  (import.meta.env.VITE_SUPABASE_ANON_KEY as string | undefined);

export const configured = Boolean(URL_ && PUBLISHABLE);

export const supabase: SupabaseClient | null = configured
  ? createClient(URL_!, PUBLISHABLE!, {
      auth: { persistSession: true, autoRefreshToken: true, detectSessionInUrl: true },
    })
  : null;

export type Team = {
  id: string;
  name: string;
  slug: string;
  created_at: string;
};

export type Member = {
  user_id: string;
  email: string;
  role: 'owner' | 'admin' | 'member';
  created_at: string;
};

export type AgentToken = {
  id: string;
  team_id: string;
  label: string;
  created_at: string;
  last_seen_at: string | null;
  revoked_at: string | null;
};

export type Repo = {
  id: string;
  team_id: string;
  repo_key: string;
  last_seen_at: string | null;
};

export function requireClient(): SupabaseClient {
  if (!supabase) throw new Error('Sign-in is not configured on this deployment.');
  return supabase;
}

export async function currentSession(): Promise<Session | null> {
  if (!supabase) return null;
  const { data } = await supabase.auth.getSession();
  return data.session;
}

/** The team this user belongs to, creating one on first sign-in. */
export async function loadTeam(): Promise<{ team: Team; role: Member['role'] } | null> {
  const sb = requireClient();
  const { data, error } = await sb
    .from('team_members')
    .select('role, teams(id, name, slug, created_at)')
    .limit(1)
    .maybeSingle();
  if (error) throw new Error(error.message);
  if (!data) return null;
  const team = (data as unknown as { teams: Team }).teams;
  return { team, role: (data as unknown as { role: Member['role'] }).role };
}

/** Called once after sign-up: makes the team and the owner row in one step. */
export async function createTeam(name: string): Promise<Team> {
  const sb = requireClient();
  const { data, error } = await sb.rpc('create_team', { team_name: name });
  if (error) throw new Error(error.message);
  return data as Team;
}

export type Invite = {
  id: string;
  email: string;
  role: 'admin' | 'member';
  created_at: string;
  expires_at: string;
};

/**
 * Invite someone by email. Returns the secret once — only its hash is stored,
 * so there is no way to read an outstanding invitation back out afterwards.
 * Whoever invites has to pass the link on themselves; nothing here sends mail.
 */
export async function inviteMember(email: string, role: Invite['role'] = 'member'): Promise<string> {
  const sb = requireClient();
  const { data, error } = await sb.rpc('invite_member', { invite_email: email, invite_role: role });
  if (error) throw new Error(error.message);
  return data as string;
}

export async function listInvites(): Promise<Invite[]> {
  const sb = requireClient();
  const { data, error } = await sb
    .from('invites')
    .select('id, email, role, created_at, expires_at')
    .is('accepted_at', null)
    .order('created_at');
  if (error) throw new Error(error.message);
  return (data ?? []) as Invite[];
}

export async function revokeInvite(id: string): Promise<void> {
  const sb = requireClient();
  const { error } = await sb.rpc('revoke_invite', { invite_id: id });
  if (error) throw new Error(error.message);
}

/**
 * Join the team an invitation was for. The join and the invitation's closure
 * are one transaction in Postgres, so a failure cannot leave someone signed in
 * with a team they cannot see and a secret they cannot use again.
 */
export async function acceptInvite(token: string): Promise<Team> {
  const sb = requireClient();
  const { data, error } = await sb.rpc('accept_invite', { invite_token: token });
  if (error) throw new Error(error.message);
  return data as Team;
}

/**
 * Take a person out of the team. This is the Supabase half; their device keys
 * live on the relay and are revoked through `/api/members/:id/remove`, which
 * the console calls straight afterwards. Two steps, because a relay that
 * accepted a webhook from anywhere would be a worse trade.
 */
export async function removeTeamMember(userId: string): Promise<void> {
  const sb = requireClient();
  const { error } = await sb.rpc('remove_member', { member_user: userId });
  if (error) throw new Error(error.message);
}
