-- knoot identity schema.
--
-- Supabase owns people and teams. The relay owns the event log and the hashed
-- agent tokens, because those have to keep working when the network does not.
-- The only thing crossing between them is a team id.
--
-- Apply with: supabase db push, or paste into the SQL editor.

create extension if not exists "pgcrypto";

create table if not exists public.teams (
  id          uuid primary key default gen_random_uuid(),
  name        text not null check (length(trim(name)) between 1 and 60),
  slug        text unique,
  created_at  timestamptz not null default now()
);

create table if not exists public.team_members (
  team_id     uuid not null references public.teams (id) on delete cascade,
  user_id     uuid not null references auth.users (id) on delete cascade,
  email       text not null,
  role        text not null default 'member' check (role in ('owner', 'admin', 'member')),
  created_at  timestamptz not null default now(),
  primary key (team_id, user_id)
);

create index if not exists team_members_user_idx on public.team_members (user_id);

alter table public.teams enable row level security;
alter table public.team_members enable row level security;

-- Membership is the whole authorisation model, and the check has to run
-- without reading the table the policy is attached to, or Postgres recurses.
-- security definer is what breaks that cycle.
create or replace function public.is_member_of(check_team uuid)
returns boolean
language sql
stable
security definer
set search_path = public
as $$
  select exists (
    select 1 from public.team_members
    where team_id = check_team and user_id = auth.uid()
  );
$$;

drop policy if exists teams_read on public.teams;
create policy teams_read on public.teams
  for select using (public.is_member_of(id));

drop policy if exists teams_update on public.teams;
create policy teams_update on public.teams
  for update using (public.is_member_of(id));

drop policy if exists members_read on public.team_members;
create policy members_read on public.team_members
  for select using (public.is_member_of(team_id));

-- Creating a team and its owner row must happen together, or a failure between
-- the two leaves a person signed in with a team they cannot see. One function,
-- one transaction.
create or replace function public.create_team(team_name text)
returns public.teams
language plpgsql
security definer
set search_path = public
as $$
declare
  uid  uuid := auth.uid();
  mail text;
  t    public.teams;
begin
  if uid is null then
    raise exception 'not signed in';
  end if;

  if exists (select 1 from public.team_members where user_id = uid) then
    raise exception 'you already belong to a team';
  end if;

  select email into mail from auth.users where id = uid;

  insert into public.teams (name, slug)
  values (
    coalesce(nullif(trim(team_name), ''), split_part(mail, '@', 1) || '''s team'),
    lower(regexp_replace(coalesce(nullif(trim(team_name), ''), split_part(mail, '@', 1)), '[^a-zA-Z0-9]+', '-', 'g'))
      || '-' || substr(gen_random_uuid()::text, 1, 6)
  )
  returning * into t;

  insert into public.team_members (team_id, user_id, email, role)
  values (t.id, uid, mail, 'owner');

  return t;
end;
$$;

revoke all on function public.create_team(text) from public;
grant execute on function public.create_team(text) to authenticated;
grant execute on function public.is_member_of(uuid) to authenticated;
