import { createClient, type SupabaseClient, type Session } from '@supabase/supabase-js';

/**
 * Supabase holds identity and the team records: who you are, which team you
 * belong to, and the metadata for each agent token. The event log stays on the
 * relay, because that is the thing that has to survive without a network.
 *
 * Keys are injected at build time. A build without them still serves every
 * page; the console just explains that sign-in is not configured rather than
 * throwing on load.
 */
const URL_ = import.meta.env.VITE_SUPABASE_URL as string | undefined;
const ANON = import.meta.env.VITE_SUPABASE_ANON_KEY as string | undefined;

export const configured = Boolean(URL_ && ANON);

export const supabase: SupabaseClient | null = configured
  ? createClient(URL_!, ANON!, {
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
