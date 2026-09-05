//! Members, devices and rooms: who a key belongs to, and what it may enter.
//!
//! `teams.rs` answers "which team does this token speak for". That was enough
//! while a team was one coordination space and authorship was a display
//! string. It stops being enough the moment an access decision or a memory
//! shard's provenance depends on *who* wrote something: a team-wide bearer
//! token names nobody.
//!
//! So a token becomes a **device**, one row per machine per person, and it
//! names a **member**. A **room** is an access group: a set of people and the
//! `(repo, area)` pairs they work in. A member's key grants the union of their
//! rooms' areas, which is the only authorisation question on the hot path.
//!
//! Three properties this module must not lose, all load-bearing today:
//!
//! * **Old keys keep working.** A `tokens` row with no device is migrated to a
//!   device of a synthetic, unassigned member in the team's `general` room. No
//!   deployment needs a flag day.
//! * **It works with no Supabase.** Rooms are enforced here, so they live
//!   here. A self-hosted relay gets members and rooms with no cloud at all;
//!   Supabase, when present, only tells us a person's email and role.
//! * **It works unconfigured.** The `root` and `local` identities keep their
//!   shape and land in a `general` room over every repo.

use anyhow::{Context, Result};

/// A person, as the relay knows them. `email` is the authorship string on
/// every event and the provenance on every memory shard, and unlike
/// `session_user()` it comes from the key rather than from the client.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Member {
    pub id: String,
    pub email: String,
    /// Team-level role: `owner` | `admin` | `member`. Mirrored from Supabase
    /// when there is a Supabase; authoritative here when there is not.
    pub role: String,
    /// A member invented by the migration to carry a pre-member token. The
    /// console offers these to an admin to attach to a real person.
    pub unassigned: bool,
}

impl Member {
    /// The identity a relay with a configured shared secret, or no secret at
    /// all, speaks for. Named rather than empty so provenance is never blank.
    pub fn legacy(id: &str) -> Self {
        Self { id: id.into(), email: id.into(), role: "owner".into(), unassigned: false }
    }
}

/// One `(repo, area)` a key may enter. `repo == "*"` is every repo in the
/// team, which is what the `general` room holds; `area == "/"` is the whole
/// repo, which is every area until phase 3 declares more.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Area {
    pub repo: String,
    pub area: String,
}

impl Area {
    pub fn everything() -> Self {
        Self { repo: "*".into(), area: "/".into() }
    }

    /// Does this grant cover a path in `repo` under `area`?
    pub fn covers(&self, repo: &str, area: &str) -> bool {
        let repo_ok = self.repo == "*" || self.repo == repo;
        let area_ok = self.area == "/" || area == self.area || area.starts_with(&format!("{}/", self.area.trim_end_matches('/')));
        repo_ok && area_ok
    }
}

pub fn init_schema(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS members (
            id         TEXT PRIMARY KEY,
            team_id    TEXT NOT NULL,
            email      TEXT NOT NULL,
            user_id    TEXT,
            role       TEXT NOT NULL DEFAULT 'member',
            unassigned INTEGER NOT NULL DEFAULT 0,
            created_ts INTEGER NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_members_team_email ON members (team_id, email);
        CREATE INDEX IF NOT EXISTS idx_members_user ON members (user_id);

        CREATE TABLE IF NOT EXISTS devices (
            id           TEXT PRIMARY KEY,
            team_id      TEXT NOT NULL,
            member_id    TEXT NOT NULL,
            label        TEXT NOT NULL,
            token_hash   TEXT NOT NULL UNIQUE,
            key_package  BLOB,
            created_ts   INTEGER NOT NULL,
            last_seen_ts INTEGER,
            revoked_ts   INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_devices_hash ON devices (token_hash);
        CREATE INDEX IF NOT EXISTS idx_devices_member ON devices (member_id);
        CREATE INDEX IF NOT EXISTS idx_devices_team ON devices (team_id);

        CREATE TABLE IF NOT EXISTS rooms (
            id         TEXT PRIMARY KEY,
            team_id    TEXT NOT NULL,
            name       TEXT NOT NULL,
            policy     TEXT NOT NULL,
            created_ts INTEGER NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_rooms_team_name ON rooms (team_id, name);
        CREATE TABLE IF NOT EXISTS room_areas (
            room_id TEXT NOT NULL, repo TEXT NOT NULL, area TEXT NOT NULL,
            PRIMARY KEY (room_id, repo, area)
        );
        CREATE TABLE IF NOT EXISTS room_members (
            room_id TEXT NOT NULL, member_id TEXT NOT NULL,
            role TEXT NOT NULL DEFAULT 'member',
            PRIMARY KEY (room_id, member_id)
        );",
    )?;
    Ok(())
}

/// The memory policy a new room starts with. Kinds and retention are §4.4 of
/// the multiplayer design. Written since phase 1, read since phase 4: the
/// budget and the per-kind switches are enforced on every publish, through
/// `budget_for_scope` and `kind_enabled_for_scope` below.
pub fn default_policy() -> String {
    serde_json::json!({
        "facts": {"enabled": true, "retain_days": 90},
        "repo_cache": {"enabled": true, "retain_days": 14},
        "session_context": {"enabled": true},
        "budget_bytes": 8_388_608u64,
        "propagate_to": ["same_area"],
    })
    .to_string()
}

/// The policies of every room that grants a memory scope.
///
/// A scope is `team/repo/area`, and a room grants it if it holds `(repo, area)`
/// or anything that covers it — a `/` grant covers every area of a repo, and a
/// `*` repo grant covers every repo, exactly as `Area::covers` decides for
/// events. Phase 4 reads this; phase 1 only wrote it.
fn policies_for_scope(conn: &rusqlite::Connection, scope: &str) -> Vec<serde_json::Value> {
    let mut parts = scope.splitn(3, '/');
    let (Some(team), Some(repo), Some(area)) = (parts.next(), parts.next(), parts.next()) else {
        return Vec::new();
    };
    let Ok(mut q) = conn.prepare(
        "SELECT r.policy, ra.repo, ra.area FROM rooms r          JOIN room_areas ra ON ra.room_id = r.id WHERE r.team_id = ?1",
    ) else {
        return Vec::new();
    };
    let rows = q.query_map(rusqlite::params![team], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    });
    let Ok(rows) = rows else { return Vec::new() };
    rows.flatten()
        .filter(|(_, r, a)| Area { repo: r.clone(), area: a.clone() }.covers(repo, area))
        .filter_map(|(p, _, _)| serde_json::from_str(&p).ok())
        .collect()
}

/// The write budget for a scope, in bytes.
///
/// The **largest** among the rooms that grant it, not the smallest. Two rooms
/// sharing an area share one store by construction, and enforcing the stricter
/// room's budget would let it silently evict the other room's facts — a policy
/// that reaches outside its own room is not a policy, it is a bug. A scope no
/// room grants gets the default, because failing closed here would mean losing
/// memory to a misconfiguration.
pub fn budget_for_scope(conn: &rusqlite::Connection, scope: &str) -> i64 {
    policies_for_scope(conn, scope)
        .iter()
        .filter_map(|p| p.get("budget_bytes").and_then(|v| v.as_i64()))
        .max()
        .unwrap_or(crate::memory::DEFAULT_BUDGET_BYTES)
}

/// Has any room that grants this scope turned this kind off?
///
/// Off wins. A room that disabled a kind said something about the area, and an
/// area shared with a permissive room is not a reason to overrule it — the
/// direction that is safe to get wrong here is the opposite of the budget's.
pub fn kind_enabled_for_scope(conn: &rusqlite::Connection, scope: &str, kind: &str) -> bool {
    let policies = policies_for_scope(conn, scope);
    if policies.is_empty() {
        return true;
    }
    policies.iter().all(|p| {
        p.get(kind)
            .and_then(|k| k.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    })
}

/// How long a kind's shards live in a scope, in days, or `None` for "until the
/// session ends" kinds that carry no retention.
pub fn retain_days_for_scope(conn: &rusqlite::Connection, scope: &str, kind: &str) -> Option<u64> {
    policies_for_scope(conn, scope)
        .iter()
        .filter_map(|p| p.get(kind).and_then(|k| k.get("retain_days")).and_then(|v| v.as_u64()))
        .min()
}

fn rand_id(prefix: &str, bytes: usize) -> String {
    let mut s = String::new();
    while s.len() < bytes * 2 {
        s.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    s.truncate(bytes * 2);
    format!("{prefix}_{s}")
}

/// Every team has a `general` room holding every repo and everyone, so no
/// team is ever a member of nothing and no key ever resolves to no areas.
/// The id is derived from the team, which is what makes this idempotent
/// without a read-modify-write.
pub fn general_room(conn: &rusqlite::Connection, team_id: &str) -> Result<String> {
    let id = format!("rm_general_{team_id}");
    conn.execute(
        "INSERT INTO rooms (id, team_id, name, policy, created_ts) VALUES (?1,?2,'general',?3,?4)
         ON CONFLICT(id) DO NOTHING",
        rusqlite::params![id, team_id, default_policy(), crate::proto::now_ms()],
    )?;
    conn.execute(
        "INSERT INTO room_areas (room_id, repo, area) VALUES (?1,'*','/')
         ON CONFLICT DO NOTHING",
        rusqlite::params![id],
    )?;
    Ok(id)
}

/// Record a person, and put them in `general`. Idempotent on `(team, email)`,
/// because this runs on every console request that resolves a Supabase user.
///
/// An existing row is updated but never *downgraded* out of being real: once a
/// migrated `unassigned` member is attached to an email, it stops being
/// unassigned and stays that way.
pub fn ensure_member(
    conn: &rusqlite::Connection,
    team_id: &str,
    email: &str,
    user_id: Option<&str>,
    role: &str,
) -> Result<Member> {
    let email = email.trim().to_lowercase();
    anyhow::ensure!(!email.is_empty(), "a member needs an email");
    let id = rand_id("m", 8);
    conn.execute(
        "INSERT INTO members (id, team_id, email, user_id, role, unassigned, created_ts)
         VALUES (?1,?2,?3,?4,?5,0,?6)
         ON CONFLICT(team_id, email) DO UPDATE SET
            user_id    = coalesce(excluded.user_id, members.user_id),
            role       = excluded.role,
            unassigned = 0",
        rusqlite::params![id, team_id, email, user_id, role, crate::proto::now_ms()],
    )
    .context("could not record member")?;
    let m = member_by_email(conn, team_id, &email).context("member vanished after insert")?;
    let room = general_room(conn, team_id)?;
    conn.execute(
        "INSERT INTO room_members (room_id, member_id, role) VALUES (?1,?2,?3)
         ON CONFLICT DO NOTHING",
        rusqlite::params![room, m.id, role],
    )?;
    Ok(m)
}

/// The owner of a team created by open registration, where nothing knows the
/// person's email. Unassigned, exactly like a migrated key: real once the
/// console signs someone in, or once an admin attaches it.
pub fn placeholder_owner(conn: &rusqlite::Connection, team_id: &str) -> Result<Member> {
    let id = rand_id("m", 8);
    let email = "owner@unassigned.invalid";
    conn.execute(
        "INSERT INTO members (id, team_id, email, role, unassigned, created_ts)
         VALUES (?1,?2,?3,'owner',1,?4) ON CONFLICT(team_id, email) DO NOTHING",
        rusqlite::params![id, team_id, email, crate::proto::now_ms()],
    )?;
    let m = member_by_email(conn, team_id, email).context("owner vanished after insert")?;
    let room = general_room(conn, team_id)?;
    conn.execute(
        "INSERT INTO room_members (room_id, member_id, role) VALUES (?1,?2,'owner')
         ON CONFLICT DO NOTHING",
        rusqlite::params![room, m.id],
    )?;
    Ok(m)
}

fn row_to_member(r: &rusqlite::Row) -> rusqlite::Result<Member> {
    Ok(Member {
        id: r.get(0)?,
        email: r.get(1)?,
        role: r.get(2)?,
        unassigned: r.get::<_, i64>(3)? != 0,
    })
}

pub fn member_by_email(conn: &rusqlite::Connection, team_id: &str, email: &str) -> Option<Member> {
    conn.query_row(
        "SELECT id, email, role, unassigned FROM members WHERE team_id = ?1 AND email = ?2",
        rusqlite::params![team_id, email.trim().to_lowercase()],
        row_to_member,
    )
    .ok()
}

pub fn list_members(conn: &rusqlite::Connection, team_id: &str) -> Vec<Member> {
    let Ok(mut q) = conn.prepare(
        "SELECT id, email, role, unassigned FROM members WHERE team_id = ?1 ORDER BY created_ts",
    ) else {
        return Vec::new();
    };
    q.query_map(rusqlite::params![team_id], row_to_member)
        .map(|r| r.flatten().collect())
        .unwrap_or_default()
}

/// Every `(repo, area)` this member may enter, as the union of their rooms.
pub fn areas_for_member(conn: &rusqlite::Connection, member_id: &str) -> Vec<Area> {
    let Ok(mut q) = conn.prepare(
        "SELECT DISTINCT room_areas.repo, room_areas.area FROM room_areas
         JOIN room_members ON room_members.room_id = room_areas.room_id
         WHERE room_members.member_id = ?1",
    ) else {
        return Vec::new();
    };
    q.query_map(rusqlite::params![member_id], |r| {
        Ok(Area { repo: r.get(0)?, area: r.get(1)? })
    })
    .map(|r| r.flatten().collect())
    .unwrap_or_default()
}

/// Mint a device key for one member. The secret keeps the shape agent tokens
/// have always had (`knt_…`, hashed at rest), so `token_for` and the login
/// path do not move; what changes is that the row names a person.
pub fn mint_device(
    conn: &rusqlite::Connection,
    team_id: &str,
    member_id: &str,
    label: &str,
) -> Result<crate::teams::IssuedToken> {
    let owner: String = conn
        .query_row(
            "SELECT team_id FROM members WHERE id = ?1",
            rusqlite::params![member_id],
            |r| r.get(0),
        )
        .context("no such member")?;
    anyhow::ensure!(owner == team_id, "that member belongs to another team");

    let label = {
        let l = label.trim();
        if l.is_empty() { "unnamed".to_string() } else { l.chars().take(40).collect() }
    };
    let secret = format!("knt_{}", &rand_id("", 24)[1..]);
    let id = rand_id("k", 6);
    conn.execute(
        "INSERT INTO devices (id, team_id, member_id, label, token_hash, created_ts)
         VALUES (?1,?2,?3,?4,?5,?6)",
        rusqlite::params![
            id,
            team_id,
            member_id,
            label,
            crate::teams::hash_token(&secret),
            crate::proto::now_ms()
        ],
    )?;
    Ok(crate::teams::IssuedToken { id, secret })
}

#[derive(Debug, serde::Serialize)]
pub struct DeviceRow {
    pub id: String,
    pub label: String,
    pub member_id: String,
    pub member_email: String,
    pub unassigned: bool,
    pub created_ts: i64,
    pub last_seen_ts: Option<i64>,
    pub revoked: bool,
}

pub fn list_devices(conn: &rusqlite::Connection, team_id: &str) -> Vec<DeviceRow> {
    let Ok(mut q) = conn.prepare(
        "SELECT devices.id, devices.label, members.id, members.email, members.unassigned,
                devices.created_ts, devices.last_seen_ts, devices.revoked_ts
         FROM devices JOIN members ON members.id = devices.member_id
         WHERE devices.team_id = ?1 ORDER BY devices.created_ts",
    ) else {
        return Vec::new();
    };
    q.query_map(rusqlite::params![team_id], |r| {
        Ok(DeviceRow {
            id: r.get(0)?,
            label: r.get(1)?,
            member_id: r.get(2)?,
            member_email: r.get(3)?,
            unassigned: r.get::<_, i64>(4)? != 0,
            created_ts: r.get(5)?,
            last_seen_ts: r.get(6)?,
            revoked: r.get::<_, Option<i64>>(7)?.is_some(),
        })
    })
    .map(|r| r.flatten().collect())
    .unwrap_or_default()
}

/// Attach a migrated, unassigned member's devices to a real person: the
/// console's answer to "whose key is this?". The synthetic member is removed
/// so it stops appearing in the members list.
pub fn attach_devices(
    conn: &rusqlite::Connection,
    team_id: &str,
    from_member: &str,
    to_member: &str,
) -> Result<usize> {
    anyhow::ensure!(from_member != to_member, "that is already the owner");
    for (id, label) in [(from_member, "source"), (to_member, "target")] {
        let ok: i64 = conn
            .query_row(
                "SELECT count(*) FROM members WHERE id = ?1 AND team_id = ?2",
                rusqlite::params![id, team_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        anyhow::ensure!(ok == 1, "no such {label} member for this team");
    }
    let n = conn.execute(
        "UPDATE devices SET member_id = ?1 WHERE member_id = ?2 AND team_id = ?3",
        rusqlite::params![to_member, from_member, team_id],
    )?;
    // Only ever drop the synthetic placeholder; a real member keeps their row
    // even when their last device moves.
    conn.execute(
        "DELETE FROM members WHERE id = ?1 AND team_id = ?2 AND unassigned = 1",
        rusqlite::params![from_member, team_id],
    )?;
    conn.execute("DELETE FROM room_members WHERE member_id = ?1", rusqlite::params![from_member])?;
    Ok(n)
}

/// Revoke one device. Scoped by team, and refuses the last live device of a
/// *team* for the same reason `teams::revoke` does: there is no account
/// recovery here and no admin to ask.
pub fn revoke_device(conn: &rusqlite::Connection, team_id: &str, device_id: &str) -> Result<()> {
    let n = conn.execute(
        "UPDATE devices SET revoked_ts = ?1
         WHERE id = ?2 AND team_id = ?3 AND revoked_ts IS NULL",
        rusqlite::params![crate::proto::now_ms(), device_id, team_id],
    )?;
    anyhow::ensure!(n == 1, "no such device for this team");
    let live: i64 = conn
        .query_row(
            "SELECT count(*) FROM devices WHERE team_id = ?1 AND revoked_ts IS NULL",
            rusqlite::params![team_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if live == 0 {
        conn.execute(
            "UPDATE devices SET revoked_ts = NULL WHERE id = ?1",
            rusqlite::params![device_id],
        )?;
        anyhow::bail!("that is the team's last live device key — mint a replacement before revoking it");
    }
    Ok(())
}

/// Remove a person: their devices stop resolving and their room memberships
/// go, and nobody else's key is touched. This is the exit criterion of phase
/// 1 and the reason a team-wide bearer token was not good enough.
/// Take a person off the team: their keys stop working, their shards go, and
/// nobody else is touched.
///
/// Returns the rooms they were in, so the caller can rotate those rooms' keys.
/// Dropping the shards is what makes "their memory goes" true — sharding by
/// author is what turns it into one statement instead of a rewrite of the
/// room's history — and it was written in phase 4 and called by nothing until
/// now, which made `knoot member rm`'s own promise false.
pub fn remove_member(
    conn: &rusqlite::Connection,
    team_id: &str,
    member_id: &str,
) -> Result<Vec<String>> {
    let live_elsewhere: i64 = conn
        .query_row(
            "SELECT count(*) FROM devices WHERE team_id = ?1 AND revoked_ts IS NULL AND member_id <> ?2",
            rusqlite::params![team_id, member_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let mine: i64 = conn
        .query_row(
            "SELECT count(*) FROM devices WHERE team_id = ?1 AND revoked_ts IS NULL AND member_id = ?2",
            rusqlite::params![team_id, member_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    anyhow::ensure!(
        live_elsewhere > 0 || mine == 0,
        "that is the only person with a live device key — the team would lock itself out"
    );
    let n = conn.execute(
        "UPDATE devices SET revoked_ts = ?1
         WHERE member_id = ?2 AND team_id = ?3 AND revoked_ts IS NULL",
        rusqlite::params![crate::proto::now_ms(), member_id, team_id],
    )?;
    // Their rooms, before the rows go: the caller needs them to rotate keys.
    let was_in: Vec<String> = conn
        .prepare("SELECT room_id FROM room_members WHERE member_id = ?1")
        .and_then(|mut q| {
            q.query_map(rusqlite::params![member_id], |r| r.get(0))
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    conn.execute("DELETE FROM room_members WHERE member_id = ?1", rusqlite::params![member_id])?;
    crate::memory::forget_author(conn, team_id, member_id);
    let gone = conn.execute(
        "DELETE FROM members WHERE id = ?1 AND team_id = ?2",
        rusqlite::params![member_id, team_id],
    )?;
    anyhow::ensure!(gone == 1 || n > 0, "no such member for this team");
    Ok(was_in)
}

// ---------------------------------------------------------------- rooms

#[derive(Debug, serde::Serialize)]
pub struct RoomRow {
    pub id: String,
    pub name: String,
    pub policy: serde_json::Value,
    pub areas: Vec<Area>,
    pub members: Vec<Member>,
}

pub fn create_room(conn: &rusqlite::Connection, team_id: &str, name: &str) -> Result<String> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "a room needs a name");
    anyhow::ensure!(name.chars().count() <= 60, "room name is too long (60 max)");
    let id = rand_id("rm", 8);
    conn.execute(
        "INSERT INTO rooms (id, team_id, name, policy, created_ts) VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![id, team_id, name, default_policy(), crate::proto::now_ms()],
    )
    .context("a room with that name already exists")?;
    Ok(id)
}

/// Rooms may share an area, and when they do they share its log by
/// construction — there is nothing here to stop it, which is the point: the
/// log is per `(repo, area)`, so two rooms over one area is two groups of
/// people who can see each other, which is what an admin asked for.
pub fn add_area(conn: &rusqlite::Connection, team_id: &str, room: &str, repo: &str, area: &str) -> Result<()> {
    own_room(conn, team_id, room)?;
    let area = normalise_area(area);
    conn.execute(
        "INSERT INTO room_areas (room_id, repo, area) VALUES (?1,?2,?3) ON CONFLICT DO NOTHING",
        rusqlite::params![room, repo.trim(), area],
    )?;
    Ok(())
}

pub fn remove_area(conn: &rusqlite::Connection, team_id: &str, room: &str, repo: &str, area: &str) -> Result<()> {
    own_room(conn, team_id, room)?;
    conn.execute(
        "DELETE FROM room_areas WHERE room_id = ?1 AND repo = ?2 AND area = ?3",
        rusqlite::params![room, repo.trim(), normalise_area(area)],
    )?;
    Ok(())
}

pub fn add_member(conn: &rusqlite::Connection, team_id: &str, room: &str, member_id: &str, role: &str) -> Result<()> {
    own_room(conn, team_id, room)?;
    let ok: i64 = conn
        .query_row(
            "SELECT count(*) FROM members WHERE id = ?1 AND team_id = ?2",
            rusqlite::params![member_id, team_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    anyhow::ensure!(ok == 1, "no such member for this team");
    conn.execute(
        "INSERT INTO room_members (room_id, member_id, role) VALUES (?1,?2,?3)
         ON CONFLICT(room_id, member_id) DO UPDATE SET role = excluded.role",
        rusqlite::params![room, member_id, role],
    )?;
    Ok(())
}

pub fn remove_room_member(conn: &rusqlite::Connection, team_id: &str, room: &str, member_id: &str) -> Result<()> {
    own_room(conn, team_id, room)?;
    conn.execute(
        "DELETE FROM room_members WHERE room_id = ?1 AND member_id = ?2",
        rusqlite::params![room, member_id],
    )?;
    Ok(())
}

/// `general` is what makes "a key always resolves to some area" true, so it
/// is not deletable; anything else is.
pub fn delete_room(conn: &rusqlite::Connection, team_id: &str, room: &str) -> Result<()> {
    own_room(conn, team_id, room)?;
    anyhow::ensure!(
        room != format!("rm_general_{team_id}"),
        "the general room is where every key lands and cannot be deleted"
    );
    conn.execute("DELETE FROM room_areas WHERE room_id = ?1", rusqlite::params![room])?;
    conn.execute("DELETE FROM room_members WHERE room_id = ?1", rusqlite::params![room])?;
    conn.execute("DELETE FROM rooms WHERE id = ?1 AND team_id = ?2", rusqlite::params![room, team_id])?;
    Ok(())
}

pub fn set_policy(conn: &rusqlite::Connection, team_id: &str, room: &str, policy: &serde_json::Value) -> Result<()> {
    own_room(conn, team_id, room)?;
    anyhow::ensure!(policy.is_object(), "a memory policy is a json object");
    conn.execute(
        "UPDATE rooms SET policy = ?1 WHERE id = ?2 AND team_id = ?3",
        rusqlite::params![policy.to_string(), room, team_id],
    )?;
    Ok(())
}

/// Scoped by team on every write, so no room can be edited by guessing an id
/// from another team — the same rule `teams::revoke` follows.
fn own_room(conn: &rusqlite::Connection, team_id: &str, room: &str) -> Result<()> {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM rooms WHERE id = ?1 AND team_id = ?2",
            rusqlite::params![room, team_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    anyhow::ensure!(n == 1, "no such room for this team");
    Ok(())
}

/// Areas are path prefixes with no leading or trailing slash; `/` is the whole
/// repo. Normalising on write means the covers-check never has to guess.
fn normalise_area(area: &str) -> String {
    let a = area.trim().trim_matches('/');
    if a.is_empty() { "/".into() } else { a.to_string() }
}

pub fn list_rooms(conn: &rusqlite::Connection, team_id: &str) -> Vec<RoomRow> {
    let Ok(mut q) = conn
        .prepare("SELECT id, name, policy FROM rooms WHERE team_id = ?1 ORDER BY created_ts")
    else {
        return Vec::new();
    };
    let rooms: Vec<(String, String, String)> = q
        .query_map(rusqlite::params![team_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map(|r| r.flatten().collect())
        .unwrap_or_default();
    rooms
        .into_iter()
        .map(|(id, name, policy)| RoomRow {
            areas: room_areas(conn, &id),
            members: room_members(conn, &id),
            policy: serde_json::from_str(&policy).unwrap_or_else(|_| serde_json::json!({})),
            id,
            name,
        })
        .collect()
}

fn room_areas(conn: &rusqlite::Connection, room: &str) -> Vec<Area> {
    let Ok(mut q) = conn.prepare("SELECT repo, area FROM room_areas WHERE room_id = ?1 ORDER BY repo, area")
    else {
        return Vec::new();
    };
    q.query_map(rusqlite::params![room], |r| Ok(Area { repo: r.get(0)?, area: r.get(1)? }))
        .map(|r| r.flatten().collect())
        .unwrap_or_default()
}

fn room_members(conn: &rusqlite::Connection, room: &str) -> Vec<Member> {
    let Ok(mut q) = conn.prepare(
        "SELECT members.id, members.email, room_members.role, members.unassigned
         FROM room_members JOIN members ON members.id = room_members.member_id
         WHERE room_members.room_id = ?1 ORDER BY members.email",
    ) else {
        return Vec::new();
    };
    q.query_map(rusqlite::params![room], row_to_member)
        .map(|r| r.flatten().collect())
        .unwrap_or_default()
}

pub fn rooms_for_member(conn: &rusqlite::Connection, member_id: &str) -> Vec<String> {
    let Ok(mut q) = conn.prepare(
        "SELECT rooms.name FROM rooms JOIN room_members ON room_members.room_id = rooms.id
         WHERE room_members.member_id = ?1 ORDER BY rooms.name",
    ) else {
        return Vec::new();
    };
    q.query_map(rusqlite::params![member_id], |r| r.get(0))
        .map(|r| r.flatten().collect())
        .unwrap_or_default()
}

/// The ids of the rooms a member is in. Used to rotate their keys when
/// somebody's access changes.
pub fn rooms_of_member(conn: &rusqlite::Connection, member_id: &str) -> Vec<String> {
    conn.prepare("SELECT room_id FROM room_members WHERE member_id = ?1")
        .and_then(|mut q| {
            q.query_map(rusqlite::params![member_id], |r| r.get(0))
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default()
}

/// The rooms a member is in, by id, with each room's grants on one repo.
///
/// The key provider needs this and nothing else: a memory scope is sealed
/// under the group of the room that grants it, and only the relay knows which
/// room that is. Ordered by room id so every device in a team resolves the
/// same room for a scope — two members binding a scope to different rooms
/// would seal shards each other could not open.
pub fn room_grants_for_repo(
    conn: &rusqlite::Connection,
    member_id: &str,
    repo: &str,
) -> Vec<(String, String)> {
    let Ok(mut q) = conn.prepare(
        "SELECT rooms.id, room_areas.repo, room_areas.area FROM rooms \
         JOIN room_members ON room_members.room_id = rooms.id \
         JOIN room_areas ON room_areas.room_id = rooms.id \
         WHERE room_members.member_id = ?1 ORDER BY rooms.id, room_areas.area",
    ) else {
        return Vec::new();
    };
    let rows = q.query_map(rusqlite::params![member_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    });
    let Ok(rows) = rows else { return Vec::new() };
    rows.flatten()
        .filter(|(_, r, _)| r == "*" || r == repo)
        .map(|(id, _, area)| (id, area))
        .collect()
}

/// Every device that belongs in a room: the machines of its members that have
/// not been revoked. This is the roster a current member reconciles the MLS
/// group against.
pub fn devices_in_room(conn: &rusqlite::Connection, team_id: &str, room: &str) -> Vec<String> {
    let Ok(mut q) = conn.prepare(
        "SELECT devices.id FROM devices \
         JOIN room_members ON room_members.member_id = devices.member_id \
         WHERE room_members.room_id = ?1 AND devices.team_id = ?2 \
           AND devices.revoked_ts IS NULL ORDER BY devices.id",
    ) else {
        return Vec::new();
    };
    q.query_map(rusqlite::params![room, team_id], |r| r.get(0))
        .map(|r| r.flatten().collect())
        .unwrap_or_default()
}

/// Is this member in this room? The check every DS call needs, and the reason
/// a relay that forwards blobs it cannot read is still not a free-for-all.
pub fn member_in_room(conn: &rusqlite::Connection, room: &str, member_id: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM room_members WHERE room_id = ?1 AND member_id = ?2",
        rusqlite::params![room, member_id],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

// ------------------------------------------------------------- migration

/// Bring pre-member `tokens` rows forward. Each becomes a device of a
/// synthetic member named after its label, in the team's `general` room, so
/// the old key keeps authenticating and the console can show it as
/// "unassigned" until an admin attaches it to a person.
///
/// Idempotent, and safe to run on every start: matching is by `token_hash`,
/// which is unique in both tables.
/// One row of the pre-member `tokens` table: id, team, label, hash, created,
/// last seen, revoked.
type LegacyToken = (String, String, String, String, i64, Option<i64>, Option<i64>);

pub fn migrate_tokens(conn: &rusqlite::Connection) -> Result<usize> {
    let exists: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='tokens'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if exists == 0 {
        return Ok(0);
    }
    let legacy: Vec<LegacyToken> = {
        let mut q = conn.prepare(
            "SELECT id, team_id, label, token_hash, created_ts, last_seen_ts, revoked_ts
             FROM tokens WHERE token_hash NOT IN (SELECT token_hash FROM devices)",
        )?;
        let rows: Vec<_> = q
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
            })?
            .flatten()
            .collect();
        rows
    };
    let mut n = 0;
    for (id, team_id, label, hash, created, seen, revoked) in legacy {
        let email = synthetic_email(&label);
        // Not `ensure_member`: that marks a member as real and belonging to a
        // signed-in person, and this one is neither.
        let member_id = match member_by_email(conn, &team_id, &email) {
            Some(m) => m.id,
            None => {
                let mid = rand_id("m", 8);
                conn.execute(
                    "INSERT INTO members (id, team_id, email, role, unassigned, created_ts)
                     VALUES (?1,?2,?3,'member',1,?4)",
                    rusqlite::params![mid, team_id, email, created],
                )?;
                let room = general_room(conn, &team_id)?;
                conn.execute(
                    "INSERT INTO room_members (room_id, member_id, role) VALUES (?1,?2,'member')
                     ON CONFLICT DO NOTHING",
                    rusqlite::params![room, mid],
                )?;
                mid
            }
        };
        conn.execute(
            "INSERT INTO devices (id, team_id, member_id, label, token_hash, created_ts, last_seen_ts, revoked_ts)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![id, team_id, member_id, label, hash, created, seen, revoked],
        )?;
        n += 1;
    }
    Ok(n)
}

/// A placeholder that reads as a placeholder. `@unassigned.invalid` is a
/// reserved TLD, so this can never collide with a real address or be mailed
/// to by mistake.
fn synthetic_email(label: &str) -> String {
    let slug: String = label
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    format!("{}@unassigned.invalid", if slug.is_empty() { "key".into() } else { slug })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        crate::teams::init_schema(&c).unwrap();
        init_schema(&c).unwrap();
        // `resolve` joins the team's own row, so the teams these tests talk
        // about have to exist as far as the relay is concerned.
        for t in ["t1", "t2"] {
            crate::teams::ensure_team(&c, t, t);
        }
        c
    }

    #[test]
    fn a_member_lands_in_general_over_every_repo() {
        let c = db();
        let m = ensure_member(&c, "t1", "ash@example.com", Some("u1"), "owner").unwrap();
        let areas = areas_for_member(&c, &m.id);
        assert_eq!(areas, vec![Area::everything()]);
        assert!(areas[0].covers("api", "src/auth"), "general covers any repo and any area");
        assert_eq!(rooms_for_member(&c, &m.id), vec!["general".to_string()]);
    }

    #[test]
    fn recording_the_same_person_twice_is_one_member() {
        let c = db();
        let a = ensure_member(&c, "t1", "Ash@Example.com", None, "owner").unwrap();
        let b = ensure_member(&c, "t1", "ash@example.com", Some("u1"), "admin").unwrap();
        assert_eq!(a.id, b.id, "email is matched case-insensitively");
        assert_eq!(b.role, "admin");
        assert_eq!(list_members(&c, "t1").len(), 1);
    }

    #[test]
    fn a_device_names_the_member_it_was_minted_for() {
        let c = db();
        let m = ensure_member(&c, "t1", "ash@example.com", None, "owner").unwrap();
        let d = mint_device(&c, "t1", &m.id, "laptop").unwrap();
        let rows = list_devices(&c, "t1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, d.id);
        assert_eq!(rows[0].member_email, "ash@example.com");
        assert!(!rows[0].unassigned);
    }

    #[test]
    fn a_device_cannot_be_minted_for_another_teams_member() {
        let c = db();
        let m = ensure_member(&c, "t1", "ash@example.com", None, "owner").unwrap();
        assert!(mint_device(&c, "t2", &m.id, "laptop").is_err());
        assert!(mint_device(&c, "t1", "m_nope", "laptop").is_err());
    }

    /// The exit criterion of phase 1: one person leaves and nobody else's key
    /// changes. A team-wide bearer token could not do this.
    #[test]
    fn removing_one_member_leaves_everyone_elses_keys_working() {
        let c = db();
        let ash = ensure_member(&c, "t1", "ash@example.com", None, "owner").unwrap();
        let priya = ensure_member(&c, "t1", "priya@example.com", None, "member").unwrap();
        let ash_key = mint_device(&c, "t1", &ash.id, "laptop").unwrap();
        let priya_key = mint_device(&c, "t1", &priya.id, "laptop").unwrap();

        remove_member(&c, "t1", &priya.id).unwrap();

        assert!(crate::teams::resolve(&c, &ash_key.secret).is_some(), "ash is unaffected");
        assert!(crate::teams::resolve(&c, &priya_key.secret).is_none(), "priya's key is dead");
        assert!(areas_for_member(&c, &priya.id).is_empty());
        assert_eq!(list_members(&c, "t1").len(), 1);
    }

    #[test]
    fn a_team_cannot_remove_its_way_to_no_access() {
        let c = db();
        let ash = ensure_member(&c, "t1", "ash@example.com", None, "owner").unwrap();
        mint_device(&c, "t1", &ash.id, "laptop").unwrap();
        let err = remove_member(&c, "t1", &ash.id).unwrap_err().to_string();
        assert!(err.contains("lock itself out"), "got: {err}");
        assert_eq!(list_members(&c, "t1").len(), 1);
    }

    #[test]
    fn a_room_grants_the_areas_it_holds_and_nothing_else() {
        let c = db();
        let m = ensure_member(&c, "t1", "ash@example.com", None, "member").unwrap();
        // Out of general, into a room over one subtree only.
        let general = general_room(&c, "t1").unwrap();
        remove_room_member(&c, "t1", &general, &m.id).unwrap();
        let room = create_room(&c, "t1", "auth").unwrap();
        add_area(&c, "t1", &room, "api", "/src/auth/").unwrap();
        add_member(&c, "t1", &room, &m.id, "member").unwrap();

        let areas = areas_for_member(&c, &m.id);
        assert_eq!(areas, vec![Area { repo: "api".into(), area: "src/auth".into() }]);
        assert!(areas[0].covers("api", "src/auth"));
        assert!(areas[0].covers("api", "src/auth/tokens"));
        assert!(!areas[0].covers("api", "src/authz"), "prefix matching is per path segment");
        assert!(!areas[0].covers("api", "src/payments"));
        assert!(!areas[0].covers("web", "src/auth"), "another repo is another area");
    }

    #[test]
    fn two_rooms_may_share_an_area() {
        let c = db();
        let a = create_room(&c, "t1", "platform").unwrap();
        let b = create_room(&c, "t1", "payments").unwrap();
        add_area(&c, "t1", &a, "api", "src/http").unwrap();
        add_area(&c, "t1", &b, "api", "src/http").unwrap();
        let m = ensure_member(&c, "t1", "ash@example.com", None, "member").unwrap();
        add_member(&c, "t1", &a, &m.id, "member").unwrap();
        add_member(&c, "t1", &b, &m.id, "member").unwrap();
        // The union is a set: being in both rooms is not two grants.
        let mut areas = areas_for_member(&c, &m.id);
        areas.retain(|x| x.repo == "api");
        assert_eq!(areas.len(), 1);
    }

    #[test]
    fn one_teams_room_cannot_be_edited_by_another() {
        let c = db();
        let room = create_room(&c, "t1", "auth").unwrap();
        assert!(add_area(&c, "t2", &room, "api", "src").is_err());
        assert!(delete_room(&c, "t2", &room).is_err());
        assert!(set_policy(&c, "t2", &room, &serde_json::json!({})).is_err());
        assert_eq!(list_rooms(&c, "t1").len(), 1);
        assert!(list_rooms(&c, "t2").is_empty());
    }

    #[test]
    fn the_general_room_cannot_be_deleted() {
        let c = db();
        let g = general_room(&c, "t1").unwrap();
        let err = delete_room(&c, "t1", &g).unwrap_err().to_string();
        assert!(err.contains("cannot be deleted"), "got: {err}");
    }

    #[test]
    fn a_policy_must_be_an_object() {
        let c = db();
        let room = create_room(&c, "t1", "auth").unwrap();
        assert!(set_policy(&c, "t1", &room, &serde_json::json!([1, 2])).is_err());
        set_policy(&c, "t1", &room, &serde_json::json!({"facts": {"enabled": false}})).unwrap();
        let got = list_rooms(&c, "t1").into_iter().find(|r| r.id == room).unwrap();
        assert_eq!(got.policy["facts"]["enabled"], serde_json::json!(false));
    }

    #[test]
    fn a_rooms_policy_governs_the_memory_in_the_areas_it_grants() {
        let c = db();
        let m = ensure_member(&c, "t1", "ash@example.com", None, "owner").unwrap();
        let scope = "t1/api//";
        assert_eq!(budget_for_scope(&c, scope), 8_388_608, "general's default");
        assert!(kind_enabled_for_scope(&c, scope, "facts"));
        assert_eq!(retain_days_for_scope(&c, scope, "facts"), Some(90));

        // A second room over the same area, with facts off. Off wins: a room
        // that disabled a kind said something about the area, and a permissive
        // neighbour is not a reason to overrule it.
        let room = create_room(&c, "t1", "strict").unwrap();
        add_area(&c, "t1", &room, "api", "/").unwrap();
        add_member(&c, "t1", &room, &m.id, "member").unwrap();
        set_policy(
            &c,
            "t1",
            &room,
            &serde_json::json!({
                "facts": {"enabled": false, "retain_days": 7},
                "budget_bytes": 1024,
            }),
        )
        .unwrap();
        assert!(!kind_enabled_for_scope(&c, scope, "facts"), "off wins");
        assert_eq!(retain_days_for_scope(&c, scope, "facts"), Some(7), "the shorter wins");
        assert_eq!(
            budget_for_scope(&c, scope),
            8_388_608,
            "but the larger budget wins: a stricter room must not evict a \
             neighbour's facts out of a store they share"
        );

        // A room that grants a different area says nothing about this one.
        assert!(kind_enabled_for_scope(&c, "t1/other-repo//", "facts"));
    }

    #[test]
    fn a_new_room_starts_with_the_default_memory_policy() {
        let c = db();
        create_room(&c, "t1", "auth").unwrap();
        let got = &list_rooms(&c, "t1")[0];
        assert_eq!(got.policy["facts"]["retain_days"], serde_json::json!(90));
        assert_eq!(got.policy["propagate_to"], serde_json::json!(["same_area"]));
    }

    #[test]
    fn a_room_name_is_unique_within_a_team_and_free_across_teams() {
        let c = db();
        create_room(&c, "t1", "auth").unwrap();
        assert!(create_room(&c, "t1", "auth").is_err());
        assert!(create_room(&c, "t2", "auth").is_ok());
        assert!(create_room(&c, "t1", "  ").is_err());
    }

    /// The migration is what lets this ship without a flag day.
    #[test]
    fn a_legacy_token_still_authenticates_and_lands_in_general() {
        let c = db();
        // A team as it exists today: `tokens` rows, no members at all.
        let (id, first) = crate::teams::create_team_legacy(&c, "Acme").unwrap();
        assert_eq!(migrate_tokens(&c).unwrap(), 1);

        let got = crate::teams::resolve(&c, &first.secret).expect("old key must keep working");
        assert_eq!(got.team_id, id.team_id);
        assert!(got.member.unassigned, "and be visibly unattached");
        assert_eq!(got.areas, vec![Area::everything()]);
        assert_eq!(rooms_for_member(&c, &got.member.id), vec!["general".to_string()]);
        // Running it again is not a second device.
        assert_eq!(migrate_tokens(&c).unwrap(), 0);
        assert_eq!(list_devices(&c, &id.team_id).len(), 1);
    }

    #[test]
    fn a_migrated_key_can_be_attached_to_a_real_person() {
        let c = db();
        let (id, key) = crate::teams::create_team_legacy(&c, "Acme").unwrap();
        migrate_tokens(&c).unwrap();
        let orphan = crate::teams::resolve(&c, &key.secret).unwrap().member;
        let ash = ensure_member(&c, &id.team_id, "ash@example.com", None, "owner").unwrap();

        assert_eq!(attach_devices(&c, &id.team_id, &orphan.id, &ash.id).unwrap(), 1);

        let now = crate::teams::resolve(&c, &key.secret).expect("the key still works");
        assert_eq!(now.member.email, "ash@example.com");
        assert!(!now.member.unassigned);
        assert!(
            !list_members(&c, &id.team_id).iter().any(|m| m.unassigned),
            "the placeholder is gone once nothing points at it"
        );
    }

    #[test]
    fn a_synthetic_email_can_never_be_a_real_address() {
        assert_eq!(synthetic_email("first token"), "first-token@unassigned.invalid");
        assert_eq!(synthetic_email("  "), "key@unassigned.invalid");
        assert!(synthetic_email("ci@example.com").ends_with("@unassigned.invalid"));
    }
}
