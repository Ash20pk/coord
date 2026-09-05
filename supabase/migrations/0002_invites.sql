-- knoot invites: how a second person gets into an existing team.
--
-- `create_team` is the only way in today and it refuses a second team per
-- user, so without this a team is permanently a team of one — which is why
-- this function is not optional for multiplayer.
--
-- Supabase owns people, so it owns the invite. The relay owns rooms and device
-- keys, and learns about a new person the first time their console session
-- authenticates. Nothing here reaches across.
--
-- Apply with: supabase db push, or paste into the SQL editor.

create table if not exists public.invites (
  id          uuid primary key default gen_random_uuid(),
  team_id     uuid not null references public.teams (id) on delete cascade,
  email       text not null check (position('@' in email) > 1),
  role        text not null default 'member' check (role in ('admin', 'member')),
  -- Only the hash is stored, for the same reason the relay only stores a
  -- token hash: a database dump must not hand over working invitations.
  token_hash  text not null unique,
  invited_by  uuid references auth.users (id) on delete set null,
  created_at  timestamptz not null default now(),
  expires_at  timestamptz not null default now() + interval '7 days',
  accepted_at timestamptz,
  accepted_by uuid references auth.users (id) on delete set null
);

create index if not exists invites_team_idx on public.invites (team_id);
create index if not exists invites_email_idx on public.invites (lower(email));

-- One live invite per address per team. A resend replaces rather than
-- accumulates, so a revoked invite cannot be resurrected by an older row.
create unique index if not exists invites_one_live_per_email
  on public.invites (team_id, lower(email))
  where accepted_at is null;

alter table public.invites enable row level security;

-- A team can see its own invitations, and nothing else can. The token hash is
-- readable by them and is useless without the secret, which only ever existed
-- in the response to `invite_member`.
drop policy if exists invites_read on public.invites;
create policy invites_read on public.invites
  for select using (public.is_member_of(team_id));

create or replace function public.is_admin_of(check_team uuid)
returns boolean
language sql
stable
security definer
set search_path = public
as $$
  select exists (
    select 1 from public.team_members
    where team_id = check_team and user_id = auth.uid() and role in ('owner', 'admin')
  );
$$;

-- Invite someone. Returns the secret once; only its hash is written down, so
-- there is no way to read an outstanding invitation back out of the database.
create or replace function public.invite_member(invite_email text, invite_role text default 'member')
returns text
language plpgsql
security definer
set search_path = public, extensions
as $$
declare
  uid    uuid := auth.uid();
  team   uuid;
  secret text;
begin
  if uid is null then
    raise exception 'not signed in';
  end if;

  select team_id into team from public.team_members where user_id = uid;
  if team is null then
    raise exception 'you do not belong to a team';
  end if;
  if not public.is_admin_of(team) then
    raise exception 'only an owner or admin can invite';
  end if;
  if coalesce(invite_role, 'member') not in ('admin', 'member') then
    raise exception 'a role is admin or member';
  end if;
  if exists (
    select 1 from public.team_members
    where team_id = team and lower(email) = lower(trim(invite_email))
  ) then
    raise exception 'that person is already in this team';
  end if;

  secret := 'kni_' || replace(gen_random_uuid()::text, '-', '')
                   || replace(gen_random_uuid()::text, '-', '');

  -- A resend supersedes the outstanding invitation rather than sitting beside
  -- it, so exactly one secret is live per address at a time.
  delete from public.invites
   where team_id = team and lower(email) = lower(trim(invite_email)) and accepted_at is null;

  insert into public.invites (team_id, email, role, token_hash, invited_by)
  values (team, lower(trim(invite_email)), coalesce(invite_role, 'member'),
          encode(digest(secret, 'sha256'), 'hex'), uid);

  return secret;
end;
$$;

-- Accept one. The join and the invite's closure happen in one transaction, or
-- a failure between them leaves a person signed in with a team they cannot see
-- and an invitation they cannot use again — the same reason `create_team` is
-- one function.
--
-- The address is checked: an invite is to a person, not a bearer token for
-- whoever holds the link. A signed-in user with a different address is
-- refused even holding a valid secret.
create or replace function public.accept_invite(invite_token text)
returns public.teams
language plpgsql
security definer
set search_path = public, extensions
as $$
declare
  uid  uuid := auth.uid();
  mail text;
  inv  public.invites;
  t    public.teams;
begin
  if uid is null then
    raise exception 'not signed in';
  end if;
  select email into mail from auth.users where id = uid;

  select * into inv from public.invites
   where token_hash = encode(digest(coalesce(invite_token, ''), 'sha256'), 'hex')
   for update;

  if inv.id is null then
    raise exception 'that invitation is not valid';
  end if;
  if inv.accepted_at is not null then
    raise exception 'that invitation has already been used';
  end if;
  if inv.expires_at < now() then
    raise exception 'that invitation has expired — ask for a new one';
  end if;
  if lower(inv.email) <> lower(mail) then
    raise exception 'that invitation was sent to a different address';
  end if;

  if exists (select 1 from public.team_members where user_id = uid) then
    raise exception 'you already belong to a team';
  end if;

  insert into public.team_members (team_id, user_id, email, role)
  values (inv.team_id, uid, mail, inv.role);

  update public.invites
     set accepted_at = now(), accepted_by = uid
   where id = inv.id;

  select * into t from public.teams where id = inv.team_id;
  return t;
end;
$$;

-- Withdraw an outstanding invitation.
create or replace function public.revoke_invite(invite_id uuid)
returns void
language plpgsql
security definer
set search_path = public
as $$
declare
  team uuid;
begin
  select team_id into team from public.invites where id = invite_id;
  if team is null then
    raise exception 'no such invitation';
  end if;
  if not public.is_admin_of(team) then
    raise exception 'only an owner or admin can withdraw an invitation';
  end if;
  delete from public.invites where id = invite_id and accepted_at is null;
end;
$$;

-- Remove a person from a team. The relay's own copy of the member, and their
-- device keys, are revoked separately through `/api/members/:id/remove` —
-- Supabase cannot reach the relay, and a relay that trusted a webhook from
-- anywhere would be a worse trade than two explicit steps.
create or replace function public.remove_member(member_user uuid)
returns void
language plpgsql
security definer
set search_path = public
as $$
declare
  team uuid;
  victim_role text;
begin
  select team_id into team from public.team_members where user_id = auth.uid();
  if team is null or not public.is_admin_of(team) then
    raise exception 'only an owner or admin can remove a member';
  end if;
  select role into victim_role from public.team_members
   where team_id = team and user_id = member_user;
  if victim_role is null then
    raise exception 'that person is not in this team';
  end if;
  -- A team with no owner has nobody who can invite, promote or remove, and
  -- there is no support desk to unstick it.
  if victim_role = 'owner' then
    raise exception 'transfer ownership before removing the owner';
  end if;
  delete from public.team_members where team_id = team and user_id = member_user;
end;
$$;

revoke all on function public.invite_member(text, text) from public;
revoke all on function public.accept_invite(text) from public;
revoke all on function public.revoke_invite(uuid) from public;
revoke all on function public.remove_member(uuid) from public;
grant execute on function public.invite_member(text, text) to authenticated;
grant execute on function public.accept_invite(text) to authenticated;
grant execute on function public.revoke_invite(uuid) to authenticated;
grant execute on function public.remove_member(uuid) to authenticated;
grant execute on function public.is_admin_of(uuid) to authenticated;
