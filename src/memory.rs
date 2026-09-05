//! Shared memory: facts a team's agents publish into an area, and read back
//! without asking.
//!
//! The spec here is a failure taxonomy, not a feature list. MemClaw ran
//! multi-tenant shared memory for agent fleets in production and published
//! what broke; every primitive below answers one of its four failure modes,
//! and two of its production bugs are load-bearing comments:
//!
//! | Failure mode | Primitive | Here |
//! |---|---|---|
//! | Unauthorized leakage | scoped retrieval | every access path checks the scope, **fetch-by-id included** — that was MemClaw's leak |
//! | Stale propagation | temporal supersession | a fact names the paths it is about; a write to one marks it possibly stale |
//! | Contradiction persistence | supersession chains | append-only with `supersedes`; **never dedupe before supersession** — that was MemClaw's other bug |
//! | Provenance collapse | provenance | the author comes from the key, is bound into the seal, and is checked at the relay |
//!
//! Everything fails open. A relay that is down, a provider that is missing, a
//! shard that will not open: the result is no memory injected, and never a
//! write that is refused for want of it.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::proto::Ts;

/// How long a fact lives unless superseded. Long enough that a convention
/// written in one sprint is still there the next; short enough that a repo
/// which has moved on is not being told about the code it used to have.
pub const FACTS_RETAIN_DAYS: u64 = 90;

/// How long derived knowledge lives. Shorter than a fact because it is
/// *derived*: cheap to work out again, and wrong the moment the code moves.
pub const REPO_CACHE_RETAIN_DAYS: u64 = 14;

/// A session's context is meant to live as long as the session, and is deleted
/// when it ends. This is only the backstop for a session that never says so —
/// a machine that slept, a terminal that was closed — and matches the point at
/// which a session is treated as gone anyway.
pub const SESSION_CONTEXT_TTL_MS: u64 = crate::proto::SESSION_STALE_MS;

/// The largest thing anyone may publish. A fact is a sentence or a paragraph;
/// something that needs more than this is a document, and documents belong in
/// the repo where they can be reviewed.
pub const MAX_SHARD_BYTES: usize = 64 * 1024;

/// The default per-room budget, enforced on write.
pub const DEFAULT_BUDGET_BYTES: i64 = 8 * 1024 * 1024;

/// Where a shard lives: the same three-part key the log is divided by, so a
/// fact is scoped by exactly the thing that bounds who can collide with whom.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Scope {
    pub team: String,
    pub repo: String,
    pub area: String,
}

impl Scope {
    pub fn key(&self) -> String {
        format!("{}/{}/{}", self.team, self.repo, self.area)
    }
}

/// The kinds of memory. Only `facts` is written today; the other two are named
/// here because the retention and policy columns are keyed by kind and a table
/// that learns a new kind later is a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Facts,
    RepoCache,
    SessionContext,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Facts => "facts",
            Kind::RepoCache => "repo_cache",
            Kind::SessionContext => "session_context",
        }
    }

    /// How long a shard of this kind lives, unless a room's policy says
    /// otherwise. `None` for a kind whose lifetime is not a duration.
    pub fn ttl_ms(&self) -> Option<u64> {
        match self {
            Kind::Facts => Some(FACTS_RETAIN_DAYS * 24 * 60 * 60 * 1000),
            Kind::RepoCache => Some(REPO_CACHE_RETAIN_DAYS * 24 * 60 * 60 * 1000),
            Kind::SessionContext => Some(SESSION_CONTEXT_TTL_MS),
        }
    }

    /// Whether a write to a path this shard names *destroys* it rather than
    /// casting doubt on it.
    ///
    /// A fact is flagged and still shown: a human wrote it on purpose, and
    /// "priya changed this since" is exactly what the reader needs. Derived
    /// knowledge is not worth showing once the ground under it moved — it was
    /// mechanical, it is now wrong, and it is cheap to work out again.
    pub fn invalidated_by_writes(&self) -> bool {
        matches!(self, Kind::RepoCache)
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "facts" => Some(Kind::Facts),
            "repo_cache" => Some(Kind::RepoCache),
            "session_context" => Some(Kind::SessionContext),
            _ => None,
        }
    }
}

/// What is inside a shard once opened — of any kind.
///
/// One payload for all three kinds, and the name stayed `Fact` because that is
/// what every caller had already called it. The kinds differ in retention, in
/// what invalidates them and in how they are shown, not in shape:
///
/// * `facts` — `name` is the handle, `text` the statement.
/// * `repo_cache` — `name` is what was worked out ("how tests run"), `text`
///   the answer, `paths` what it was derived from.
/// * `session_context` — `name` is the session id, `text` the plan,
///   `paths` what the session is touching, `decisions` what it has settled.
///
/// `paths` is what makes any of this more than a note: a shard that names the
/// code it is about can be invalidated by a write to that code, and can be
/// surfaced exactly when an agent touches it. `hashes` is each named file as
/// it stood at authoring, so a peer who wrote a file back identically does not
/// make everything about it look stale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    /// The handle a later shard supersedes by. Stable across rewrites, which
    /// is what turns two contradicting statements into a chain instead of two
    /// standing claims.
    pub name: String,
    pub text: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub hashes: std::collections::BTreeMap<String, String>,
    /// What a session has settled, for `session_context`. Empty for the other
    /// kinds. A peer that knows a decision has been made does not re-open it.
    #[serde(default)]
    pub decisions: Vec<String>,
    /// True when the daemon composed this rather than an agent writing it on
    /// purpose. Only `session_context` is ever composed, and only from
    /// declarations the session had already published — its intent and the
    /// paths it holds.
    ///
    /// It is carried so a reader is never told a guess in the voice of a
    /// plan: a composed context says what a session *appears* to be doing,
    /// because that is all its intent sentence supports.
    #[serde(default)]
    pub derived: bool,
}

/// A shard as it travels and as the relay stores it. The relay holds
/// `ciphertext` and the metadata it needs to route and collect, and nothing
/// else — under the `Plaintext` provider the ciphertext is readable, which is
/// the point of that provider and is said plainly in the docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shard {
    pub id: String,
    pub scope: String,
    pub kind: String,
    /// Member id, verified by the relay against the key that published it.
    pub author: String,
    /// The same person as a name a brief can print. Bound into the seal like
    /// the member id, and verified the same way — a display string the relay
    /// could rewrite freely would put an unverified name on every fact, which
    /// is the provenance collapse this design exists to avoid.
    pub author_email: String,
    pub device: String,
    /// `HMAC(epoch_secret, name)`. Uniqueness only — there is no blinded tag
    /// index, because retrieval happens on the client against plaintext.
    pub name_blind: String,
    pub supersedes: Option<String>,
    pub epoch: u64,
    #[serde(with = "hex_bytes")]
    pub nonce: Vec<u8>,
    #[serde(with = "hex_bytes")]
    pub ciphertext: Vec<u8>,
    pub bytes: i64,
    #[serde(default)]
    pub seq: i64,
    pub created_ts: Ts,
    pub expires_ts: Option<Ts>,
}

/// Bytes as hex on the wire. JSON is what every other message here speaks, and
/// a shard is small enough that the encoding does not matter.
pub mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::hex(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        Ok(super::unhex(&s))
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Vec<u8> {
    s.as_bytes()
        .chunks(2)
        .filter_map(|c| u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok())
        .collect()
}

// ---------------------------------------------------------------- provider

/// An epoch secret. 32 bytes, whatever produced it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub [u8; 32]);

impl std::fmt::Debug for Secret {
    /// Never printed. A secret in a log line is a secret that has left the
    /// process.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// A sealed payload: what the relay stores of a shard's content.
pub struct Sealed {
    pub epoch: u64,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

/// Sealing is a property of the deployment, not of the protocol. The daemon
/// seals through this interface and the relay code is identical whichever
/// provider is behind it.
pub trait KeyProvider: Send + Sync {
    /// The current epoch and its secret for an area's memory.
    fn epoch(&self, scope: &Scope) -> (u64, Secret);

    /// A past epoch's secret, if this device still holds it — for re-reading
    /// older shards until they are rewrapped or expire.
    fn epoch_secret(&self, scope: &Scope, epoch: u64) -> Option<Secret>;

    /// Whether this provider's shards are confidential. `false` says so out
    /// loud rather than letting a deployment assume otherwise.
    fn confidential(&self) -> bool {
        true
    }

    /// A name for `knoot status`, so a human can tell which deployment they
    /// are in without reading the config.
    fn label(&self) -> &'static str;

    fn seal(&self, scope: &Scope, aad: &str, plaintext: &[u8]) -> Sealed {
        let (epoch, secret) = self.epoch(scope);
        let nonce: Vec<u8> = random_bytes(12);
        let ciphertext = if self.confidential() {
            encrypt(&secret, aad, &nonce, plaintext)
        } else {
            // Readable by design: the relay is on a box the org runs, and
            // "can the vendor read it" was answered by where that box is. The
            // tag still binds the metadata.
            let mut out = plaintext.to_vec();
            out.extend_from_slice(&tag(&secret, aad, &nonce, plaintext));
            out
        };
        Sealed { epoch, nonce, ciphertext }
    }

    fn open(&self, scope: &Scope, epoch: u64, aad: &str, nonce: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
        let secret = self.epoch_secret(scope, epoch)?;
        if self.confidential() {
            return decrypt(&secret, aad, nonce, ct);
        }
        if ct.len() < 32 {
            return None;
        }
        let (plaintext, got) = ct.split_at(ct.len() - 32);
        let want = tag(&secret, aad, nonce, plaintext);
        // Constant time: the tag is the only thing standing between a
        // tampered row and an agent believing it.
        let mut diff = 0u8;
        for (a, b) in want.iter().zip(got) {
            diff |= a ^ b;
        }
        (diff == 0).then(|| plaintext.to_vec())
    }
}

/// ChaCha20-Poly1305 under the epoch secret, with the shard's routing metadata
/// as associated data.
///
/// The AEAD is ours, not MLS's: MLS agrees the *key* — that is the hard part,
/// and the part with a standard — and the epoch secret it exports is then used
/// the way any symmetric key is. Doing it this way means the relay stores
/// ordinary sealed bytes and knows nothing about MLS beyond forwarding
/// handshake messages it cannot read.
fn encrypt(secret: &Secret, aad: &str, nonce: &[u8], plaintext: &[u8]) -> Vec<u8> {
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    let cipher = chacha20poly1305::ChaCha20Poly1305::new(&secret.0.into());
    let mut n = [0u8; 12];
    n.copy_from_slice(&nonce[..12.min(nonce.len())]);
    cipher
        .encrypt(&n.into(), Payload { msg: plaintext, aad: aad.as_bytes() })
        .unwrap_or_default()
}

fn decrypt(secret: &Secret, aad: &str, nonce: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    if nonce.len() < 12 {
        return None;
    }
    let cipher = chacha20poly1305::ChaCha20Poly1305::new(&secret.0.into());
    let mut n = [0u8; 12];
    n.copy_from_slice(&nonce[..12]);
    cipher.decrypt(&n.into(), Payload { msg: ct, aad: aad.as_bytes() }).ok()
}

/// `HMAC-SHA256(secret, aad ‖ nonce ‖ plaintext)`, binding a shard's content
/// to the metadata the relay routes it by.
///
/// AAD is `id ‖ scope ‖ kind ‖ author ‖ author_email ‖ epoch`, so a relay that swaps two
/// shards' metadata — or rewrites who wrote one — produces a shard that will
/// not open, rather than a silent lie. The relay is the one component the room
/// cannot audit; the client has to be able to catch it misbehaving.
fn tag(secret: &Secret, aad: &str, nonce: &[u8], plaintext: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 64];
    key[..32].copy_from_slice(&secret.0);
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= key[i];
        opad[i] ^= key[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(aad.as_bytes());
    inner.update(nonce);
    inner.update(plaintext);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

/// The AAD a shard's seal is bound to.
pub fn aad(id: &str, scope: &str, kind: &str, author: &str, email: &str, epoch: u64) -> String {
    format!("{id}\u{1f}{scope}\u{1f}{kind}\u{1f}{author}\u{1f}{email}\u{1f}{epoch}")
}

/// `HMAC(epoch_secret, name)` — the only thing the relay learns about a fact's
/// handle, and only enough of it to enforce uniqueness.
pub fn name_blind(secret: &Secret, name: &str) -> String {
    hex(&tag(secret, "name", b"", name.as_bytes()))
}

/// The provider for a relay in the customer's own VPC, where the org is the
/// trust boundary and "can the vendor read it" was answered by where the box
/// is. Simpler than any cryptography, and the enterprise answer.
///
/// The secret is fixed zeros, so the integrity tag catches a relay that
/// swaps rows or loses a byte, and does **not** catch one that deliberately
/// forges — anyone can recompute a tag under a public key. Confidentiality is
/// not claimed here and never has been; that is what phase 5's `Mls` provider
/// is for.
#[derive(Debug, Default, Clone)]
pub struct Plaintext;

impl KeyProvider for Plaintext {
    fn confidential(&self) -> bool {
        false
    }

    fn label(&self) -> &'static str {
        "plaintext"
    }

    fn epoch(&self, _scope: &Scope) -> (u64, Secret) {
        (0, Secret([0u8; 32]))
    }

    fn epoch_secret(&self, _scope: &Scope, epoch: u64) -> Option<Secret> {
        (epoch == 0).then_some(Secret([0u8; 32]))
    }
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        out.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    out.truncate(n);
    out
}

// ---------------------------------------------------------------- refusals

/// Why a publish was refused. Refused, not warned about: a warning on this
/// path is a secret in a shared store with a note attached.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    Ignored(String),
    SensitivePath(String),
    Secret(&'static str),
    TooBig(usize),
    OverBudget,
    KindDisabled(&'static str),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Ignored(p) => write!(f, "{p} is gitignored — it is not shared code"),
            Refusal::SensitivePath(p) => write!(f, "{p} is the kind of file that holds credentials"),
            Refusal::Secret(what) => write!(f, "the text looks like it contains {what}"),
            Refusal::TooBig(n) => {
                write!(f, "{n} bytes is more than a fact — put a document in the repo")
            }
            Refusal::OverBudget => write!(f, "this room's memory budget is full"),
            Refusal::KindDisabled(k) => write!(f, "this room has {k} turned off"),
        }
    }
}

/// Paths whose *contents* must never become a shard, whatever the caller
/// intended. Matched on the file name, so a path in any directory is caught.
const SENSITIVE: &[&str] = &[".env", "id_", "credentials"];
const SENSITIVE_EXT: &[&str] = &["pem", "key", "tfvars", "p12", "pfx", "jks"];

/// Prefixes of credentials this project or its neighbours issue. A fact
/// carrying one of these is an exfiltration, whether or not anyone meant it.
const TOKEN_PREFIXES: &[(&str, &str)] = &[
    ("knt_", "a knoot device key"),
    ("sb_secret_", "a Supabase secret key"),
    ("sbp_", "a Supabase access token"),
    ("ghp_", "a GitHub token"),
    ("gho_", "a GitHub token"),
    ("ghs_", "a GitHub token"),
    ("github_pat_", "a GitHub token"),
    ("xoxb-", "a Slack token"),
    ("xoxp-", "a Slack token"),
    ("AKIA", "an AWS access key id"),
    ("ASIA", "an AWS access key id"),
    ("sk-", "an API key"),
    ("-----BEGIN", "a private key"),
];

/// Is this path one whose contents may be published at all?
///
/// `git check-ignore` rather than a parser: the repo's own answer is the only
/// one that cannot disagree with the repo. A repo that is not a git checkout,
/// or a git that is not installed, means no ignore rule is enforced — which
/// fails open on purpose, and the other three checks still apply.
pub fn refuse_path(repo_root: &std::path::Path, path: &str) -> Option<Refusal> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if SENSITIVE.iter().any(|p| name.starts_with(p)) {
        return Some(Refusal::SensitivePath(path.to_string()));
    }
    if path
        .rsplit('.')
        .next()
        .is_some_and(|ext| ext != path && SENSITIVE_EXT.contains(&ext))
    {
        return Some(Refusal::SensitivePath(path.to_string()));
    }
    let ignored = std::process::Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "check-ignore", "-q", path])
        // git talks about a missing directory on stderr, which would otherwise
        // land in an agent's terminal for a check it never asked for.
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ignored.then(|| Refusal::Ignored(path.to_string()))
}

/// Is this text publishable? Size and secrets; the caller adds budget and
/// policy, which it is the only one that can know.
pub fn refuse_text(text: &str) -> Option<Refusal> {
    if text.len() > MAX_SHARD_BYTES {
        return Some(Refusal::TooBig(text.len()));
    }
    for (prefix, what) in TOKEN_PREFIXES {
        if text.contains(prefix) {
            return Some(Refusal::Secret(what));
        }
    }
    // A long unbroken run of key-ish characters is what a credential looks
    // like when it has no prefix we know. Prose does not produce these; a
    // base64 blob and a hex digest both do.
    for word in text.split(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '=') {
        if word.len() >= 40 && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '-' || c == '_')
            && word.chars().any(|c| c.is_ascii_digit())
            && word.chars().any(|c| c.is_ascii_uppercase())
        {
            return Some(Refusal::Secret("a credential"));
        }
    }
    None
}

/// The content of a file, if it may be published. `--from <path>` goes through
/// exactly the refusals everything else does; that is why `project_files` is
/// not a kind of its own.
pub fn read_publishable(repo_root: &std::path::Path, rel: &str) -> Result<String, Refusal> {
    if let Some(r) = refuse_path(repo_root, rel) {
        return Err(r);
    }
    let text = std::fs::read_to_string(repo_root.join(rel))
        .map_err(|e| Refusal::SensitivePath(format!("{rel}: {e}")))?;
    match refuse_text(&text) {
        Some(r) => Err(r),
        None => Ok(text),
    }
}

/// The content hash recorded with a fact, so a file written back identically
/// does not make every fact about it look stale.
pub fn hash_file(repo_root: &std::path::Path, rel: &str) -> Option<String> {
    let bytes = std::fs::read(repo_root.join(rel)).ok()?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Some(hex(&h.finalize())[..16].to_string())
}

// ------------------------------------------------------------------- store
//
// The relay's half. It sequences, scopes and collects; it never opens a
// shard, and under `Mls` it will not be able to.

pub fn init_schema(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_shards (
            id          TEXT PRIMARY KEY,
            scope       TEXT NOT NULL,
            kind        TEXT NOT NULL,
            author      TEXT NOT NULL,
            author_email TEXT NOT NULL DEFAULT '',
            device      TEXT NOT NULL,
            name_blind  TEXT NOT NULL,
            supersedes  TEXT,
            epoch       INTEGER NOT NULL,
            nonce       BLOB NOT NULL,
            ciphertext  BLOB NOT NULL,
            bytes       INTEGER NOT NULL,
            seq         INTEGER NOT NULL,
            created_ts  INTEGER NOT NULL,
            expires_ts  INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_shards_scope_seq ON memory_shards (scope, seq);
        CREATE INDEX IF NOT EXISTS idx_shards_supersedes ON memory_shards (supersedes);
        -- One shard per (scope, kind, author, name, parent). This is the only
        -- uniqueness there is, and it is deliberately not content-based:
        -- MemClaw's second production bug was a near-duplicate filter that
        -- rejected a contradicting write before contradiction detection could
        -- see it. A contradiction *is* a near-duplicate. Never dedupe before
        -- supersession.
        CREATE UNIQUE INDEX IF NOT EXISTS idx_shards_chain
            ON memory_shards (scope, kind, author, name_blind, COALESCE(supersedes, ''));",
    )?;
    Ok(())
}

fn row_to_shard(r: &rusqlite::Row) -> rusqlite::Result<Shard> {
    Ok(Shard {
        id: r.get(0)?,
        scope: r.get(1)?,
        kind: r.get(2)?,
        author: r.get(3)?,
        author_email: r.get(4)?,
        device: r.get(5)?,
        name_blind: r.get(6)?,
        supersedes: r.get(7)?,
        epoch: r.get::<_, i64>(8)? as u64,
        nonce: r.get(9)?,
        ciphertext: r.get(10)?,
        bytes: r.get(11)?,
        seq: r.get(12)?,
        created_ts: r.get::<_, i64>(13)? as u64,
        expires_ts: r.get::<_, Option<i64>>(14)?.map(|v| v as u64),
    })
}

const COLS: &str = "id, scope, kind, author, author_email, device, name_blind, supersedes, \
                    epoch, nonce, ciphertext, bytes, seq, created_ts, expires_ts";

/// Store a shard, sequence it, and collect what the room no longer has room
/// for. Returns the assigned sequence.
///
/// The caller has already checked that the author owns the key and may enter
/// the scope; this is the part that only the store can do.
pub fn put(conn: &rusqlite::Connection, shard: &Shard, budget_bytes: i64) -> Result<i64> {
    anyhow::ensure!(
        shard.bytes as usize <= MAX_SHARD_BYTES,
        "a shard may not exceed {MAX_SHARD_BYTES} bytes"
    );
    let seq: i64 = conn
        .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM memory_shards", [], |r| r.get(0))
        .unwrap_or(1);
    conn.execute(
        &format!(
            "INSERT INTO memory_shards ({COLS}) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"
        ),
        rusqlite::params![
            shard.id,
            shard.scope,
            shard.kind,
            shard.author,
            shard.author_email,
            shard.device,
            shard.name_blind,
            shard.supersedes,
            shard.epoch as i64,
            shard.nonce,
            shard.ciphertext,
            shard.bytes,
            seq,
            shard.created_ts as i64,
            shard.expires_ts.map(|v| v as i64),
        ],
    )?;
    collect(conn, &shard.scope, budget_bytes);
    Ok(seq)
}

/// Retention and budget. Expired shards go first, then superseded ones oldest
/// first — the room's history is what it can most afford to lose, and the head
/// of every chain is what anything reads.
fn collect(conn: &rusqlite::Connection, scope: &str, budget_bytes: i64) {
    let now = crate::proto::now_ms() as i64;
    let _ = conn.execute(
        "DELETE FROM memory_shards WHERE scope = ?1 AND expires_ts IS NOT NULL AND expires_ts < ?2",
        rusqlite::params![scope, now],
    );
    loop {
        let used: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(bytes), 0) FROM memory_shards WHERE scope = ?1",
                rusqlite::params![scope],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if used <= budget_bytes {
            return;
        }
        let victim: Option<String> = conn
            .query_row(
                "SELECT id FROM memory_shards WHERE scope = ?1 \
                 AND id IN (SELECT supersedes FROM memory_shards WHERE supersedes IS NOT NULL) \
                 ORDER BY seq ASC LIMIT 1",
                rusqlite::params![scope],
                |r| r.get(0),
            )
            .ok();
        let Some(victim) = victim else { return }; // nothing superseded left to drop
        let _ = conn.execute("DELETE FROM memory_shards WHERE id = ?1", rusqlite::params![victim]);
    }
}

/// Every shard in the given scopes since `since`, oldest first.
pub fn since(conn: &rusqlite::Connection, scopes: &[String], since: i64, limit: usize) -> Vec<Shard> {
    if scopes.is_empty() {
        return Vec::new();
    }
    let holes = scopes.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT {COLS} FROM memory_shards WHERE scope IN ({holes}) AND seq > ? \
         ORDER BY seq ASC LIMIT ?"
    );
    let Ok(mut q) = conn.prepare(&sql) else { return Vec::new() };
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    for s in scopes {
        params.push(Box::new(s.clone()));
    }
    params.push(Box::new(since));
    params.push(Box::new(limit as i64));
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    q.query_map(refs.as_slice(), row_to_shard)
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// One shard by id — **and the scope check runs here too**.
///
/// MemClaw's production leak was a GET-by-id that skipped the check every
/// other path performed. The signature is why it cannot happen here: there is
/// no way to ask for a shard without saying which scopes you hold.
pub fn get(conn: &rusqlite::Connection, id: &str, scopes: &[String]) -> Option<Shard> {
    let shard = conn
        .query_row(
            &format!("SELECT {COLS} FROM memory_shards WHERE id = ?1"),
            rusqlite::params![id],
            row_to_shard,
        )
        .ok()?;
    scopes.contains(&shard.scope).then_some(shard)
}

/// Replace a shard's sealed bytes, keeping everything else.
///
/// The scope check is here as it is everywhere else — a caller must say which
/// scopes it holds — and nothing that provenance rests on is writable: id,
/// scope, kind, author and author_email are exactly what they were, and they
/// are what the new seal is bound to. So a rewrap rotates a key and cannot
/// launder authorship.
pub fn rewrap(
    conn: &rusqlite::Connection,
    id: &str,
    scopes: &[String],
    epoch: u64,
    nonce: &[u8],
    ciphertext: &[u8],
) -> Result<Shard> {
    let mut shard = get(conn, id, scopes).ok_or_else(|| anyhow::anyhow!("no such shard here"))?;
    anyhow::ensure!(epoch > shard.epoch, "a rewrap must move a shard forward, not back");
    anyhow::ensure!(ciphertext.len() <= MAX_SHARD_BYTES + 64, "rewrapped shard is too large");
    conn.execute(
        "UPDATE memory_shards SET epoch = ?1, nonce = ?2, ciphertext = ?3 WHERE id = ?4",
        rusqlite::params![epoch as i64, nonce, ciphertext, id],
    )?;
    // Handed back so the caller can fan it out. A rewrap does not move the
    // sequence — nothing about the shard's place in the room changed — so a
    // client syncing from its high-water mark would never see the new
    // ciphertext, and a device that had just joined would go on holding a
    // shard it cannot open.
    shard.epoch = epoch;
    shard.nonce = nonce.to_vec();
    shard.ciphertext = ciphertext.to_vec();
    Ok(shard)
}

/// Delete shards, if the caller holds their scope.
///
/// Used when a session ends. The scope check is here for the same reason it is
/// on every other path: a caller that could delete by id alone could delete
/// another room's memory without ever being able to read it.
pub fn forget(conn: &rusqlite::Connection, ids: &[String], scopes: &[String]) -> usize {
    let mut n = 0;
    for id in ids {
        if get(conn, id, scopes).is_some() {
            n += conn
                .execute("DELETE FROM memory_shards WHERE id = ?1", rusqlite::params![id])
                .unwrap_or(0);
        }
    }
    n
}

/// What the relay knows, in aggregate, for the console and `knoot status`.
pub fn counts(conn: &rusqlite::Connection, scope: &str) -> (i64, i64) {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM memory_shards WHERE scope = ?1",
        rusqlite::params![scope],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap_or((0, 0))
}

/// Drop everything a departing member wrote. Sharding by author is what makes
/// this one statement instead of a rewrite of the room's history.
pub fn forget_author(conn: &rusqlite::Connection, team: &str, author: &str) -> usize {
    conn.execute(
        "DELETE FROM memory_shards WHERE author = ?1 AND scope LIKE ?2",
        rusqlite::params![author, format!("{team}/%")],
    )
    .unwrap_or(0)
}

// ------------------------------------------------------------------- cache
//
// The client's half. Retrieval is local: a daemon mirrors the ciphertext for
// the areas its member is in, opens it into this cache, and does relevance and
// staleness on plaintext. The relay learns shard counts, sizes, kinds,
// authors, epochs and which shards share a name — and nothing else.

/// A fact this daemon has opened, with the metadata it was routed by.
#[derive(Debug, Clone)]
pub struct Held {
    pub shard: Shard,
    pub fact: Fact,
}

/// Every shard this daemon has mirrored, opened where it could be.
#[derive(Default)]
pub struct Cache {
    held: std::collections::HashMap<String, Held>,
    /// Shards that arrived and would not open — a tampered row, or an epoch
    /// this device does not hold. Reported rather than hidden: `knoot status`
    /// says how many, because silent unreadable memory is indistinguishable
    /// from no memory.
    ///
    /// Kept whole, not counted, so they can be **retried**. A shard arrives
    /// the moment it is published or rewrapped, and a device that is still
    /// finishing its join cannot open it yet — the key it needs is one
    /// handshake message away. Throwing the bytes away would make that a
    /// permanent hole in a room that is about to be perfectly readable, and
    /// nothing would ever re-send them: a rewrap does not move the sequence a
    /// sync is keyed on.
    unreadable: std::collections::HashMap<String, Shard>,
    /// The highest sequence this cache has seen, so a reconnect resumes.
    pub seq: i64,
}

impl Cache {
    /// Take a shard from the relay. Returns false when it would not open.
    pub fn apply(
        &mut self,
        provider: &dyn KeyProvider,
        scope: &Scope,
        shard: Shard,
    ) -> bool {
        self.seq = self.seq.max(shard.seq);
        let aad = aad(
            &shard.id,
            &shard.scope,
            &shard.kind,
            &shard.author,
            &shard.author_email,
            shard.epoch,
        );
        let Some(plain) = provider.open(scope, shard.epoch, &aad, &shard.nonce, &shard.ciphertext)
        else {
            self.unreadable.insert(shard.id.clone(), shard);
            return false;
        };
        let Ok(fact) = serde_json::from_slice::<Fact>(&plain) else {
            self.unreadable.insert(shard.id.clone(), shard);
            return false;
        };
        self.unreadable.remove(&shard.id);
        self.held.insert(shard.id.clone(), Held { shard, fact });
        true
    }

    /// The kind of a held shard, or `Facts` for a kind this build does not
    /// know — a shard from a newer peer must not vanish from a count.
    pub fn kind_of(h: &Held) -> Kind {
        Kind::parse(&h.shard.kind).unwrap_or(Kind::Facts)
    }

    pub fn len(&self) -> usize {
        self.heads().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The current statement of every chain: shards nothing else supersedes.
    ///
    /// Contradictions are resolved here and only here. Two facts under one
    /// name are a chain, and the head is what an agent is told; the losers
    /// stay on the record, because "what did we used to believe, and who
    /// changed it" is the question provenance exists to answer.
    pub fn heads(&self) -> Vec<&Held> {
        let superseded: std::collections::HashSet<&str> =
            self.held.values().filter_map(|h| h.shard.supersedes.as_deref()).collect();
        let mut out: Vec<&Held> =
            self.held.values().filter(|h| !superseded.contains(h.shard.id.as_str())).collect();
        out.sort_by_key(|h| std::cmp::Reverse(h.shard.created_ts));
        out
    }

    /// Heads of one kind. The kinds are shown in different sections, with
    /// different rules about staleness, so nothing that reads memory for an
    /// agent wants them mixed.
    pub fn heads_of(&self, kind: Kind) -> Vec<&Held> {
        self.heads().into_iter().filter(|h| Self::kind_of(h) == kind).collect()
    }

    /// The head of the chain a name is on, for the author about to write it.
    /// This is what makes a second `knoot remember` under the same name a
    /// supersession rather than a second standing claim.
    ///
    /// Keyed by kind as well as name: a `repo_cache` entry called `retry` and
    /// a fact called `retry` are two different things, and one superseding the
    /// other would silently delete the fact.
    pub fn head_of(&self, author: &str, kind: Kind, name: &str) -> Option<&Held> {
        self.heads_of(kind)
            .into_iter()
            .find(|h| h.shard.author == author && h.fact.name == name)
    }

    /// Every session_context a peer has published, newest first.
    ///
    /// `mine` is dropped: an agent does not need its own plan read back to it,
    /// and the budget it would spend is the budget a peer's plan needs.
    pub fn peer_context(&self, mine: &str) -> Vec<&Held> {
        self.heads_of(Kind::SessionContext)
            .into_iter()
            .filter(|h| h.fact.name != mine)
            .collect()
    }

    /// How many shards this device holds but cannot open.
    pub fn unreadable(&self) -> usize {
        self.unreadable.len()
    }

    /// The shards this device is holding and cannot open, as
    /// `(id, scope, epoch)`. For diagnostics: "unreadable" with no reason is
    /// the kind of quiet that wastes an afternoon.
    pub fn unreadable_epochs(&self) -> Vec<(String, String, u64)> {
        self.unreadable
            .values()
            .map(|s| (s.id.clone(), s.scope.clone(), s.epoch))
            .collect()
    }

    /// Try the unreadable ones again. Returns how many opened this time.
    ///
    /// Called whenever this device's key material changes — it has joined a
    /// room, or the room has moved to an epoch it can derive. Cheap: the set
    /// is empty in the normal case, and a room's whole memory is kilobytes.
    pub fn retry(&mut self, provider: &dyn KeyProvider, scope_of: &dyn Fn(&str) -> Scope) -> usize {
        if self.unreadable.is_empty() {
            return 0;
        }
        let pending: Vec<Shard> = self.unreadable.values().cloned().collect();
        let mut opened = 0;
        for shard in pending {
            let scope = scope_of(&shard.scope);
            if self.apply(provider, &scope, shard) {
                opened += 1;
            }
        }
        opened
    }

    /// Every shard this cache has opened, superseded ones included.
    ///
    /// Used by rewrap: after a Remove, the whole live set has to be re-sealed
    /// under the new epoch, not only the standing statements — a superseded
    /// fact is still what "what did we used to believe" is answered from.
    pub fn all(&self) -> impl Iterator<Item = &Held> {
        self.held.values()
    }

    pub fn by_id(&self, id: &str) -> Option<&Held> {
        self.held.get(id)
    }

    /// Shards of a kind about any of these paths, most recent first.
    pub fn about(&self, kind: Kind, paths: &[String]) -> Vec<&Held> {
        self.heads_of(kind)
            .into_iter()
            .filter(|h| h.fact.paths.iter().any(|p| paths.iter().any(|q| path_touches(p, q))))
            .collect()
    }

    /// Ids of every shard under a name, for a kind. Used to delete a
    /// session's context when the session ends: the chain, not only its head,
    /// because a session that replanned three times left three shards.
    pub fn ids_named(&self, kind: Kind, name: &str) -> Vec<String> {
        self.held
            .values()
            .filter(|h| Self::kind_of(h) == kind && h.fact.name == name)
            .map(|h| h.shard.id.clone())
            .collect()
    }

    /// Drop shards this cache should no longer hold.
    pub fn forget(&mut self, ids: &[String]) {
        for id in ids {
            self.held.remove(id);
            self.unreadable.remove(id);
        }
    }

    /// Free-text recall for a person at a prompt. Deliberately dumb — word
    /// overlap over plaintext — because the alternative is an embedding model
    /// in a daemon that must answer in milliseconds and fail open.
    pub fn search(&self, query: &str) -> Vec<&Held> {
        let q: Vec<String> = words(query);
        if q.is_empty() {
            return self.heads();
        }
        let mut scored: Vec<(usize, &Held)> = self
            .heads()
            .into_iter()
            .filter_map(|h| {
                let hay = words(&format!("{} {} {}", h.fact.name, h.fact.text, h.fact.paths.join(" ")));
                let n = q.iter().filter(|w| hay.contains(w)).count();
                (n > 0).then_some((n, h))
            })
            .collect();
        scored.sort_by_key(|(n, h)| (std::cmp::Reverse(*n), std::cmp::Reverse(h.shard.created_ts)));
        scored.into_iter().map(|(_, h)| h).collect()
    }
}

/// Does a fact about `fact_path` bear on `touched`? Either is allowed to be
/// the directory of the other, so a fact about `src/http/` surfaces when an
/// agent opens `src/http/retry.rs`.
fn path_touches(fact_path: &str, touched: &str) -> bool {
    let a = fact_path.trim_matches('/');
    let b = touched.trim_matches('/');
    a == b || b.starts_with(&format!("{a}/")) || a.starts_with(&format!("{b}/"))
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(|w| w.to_lowercase())
        .collect()
}

/// Whether a fact's ground has moved, and what moved it.
///
/// Nobody else has this signal. A memory system that only knows when a fact
/// was written can tell you it is old; one that knows which files it is about
/// can tell you it is *wrong*, and name the person who made it so.
///
/// `repo_root` is passed so that a write which restored a file byte for byte
/// can be told from one that changed it — the hash recorded at authoring is
/// what that costs, and it is checked here, off the hot path, rather than at
/// `PreToolUse`.
pub fn staleness(
    held: &Held,
    last_write: &std::collections::HashMap<String, (String, Ts)>,
    users: &dyn Fn(&str) -> String,
    repo_root: Option<&std::path::Path>,
) -> Option<String> {
    let mut worst: Option<(&String, &str, Ts)> = None;
    for p in &held.fact.paths {
        let Some((session, ts)) = last_write.get(p) else { continue };
        if *ts <= held.shard.created_ts {
            continue;
        }
        // Written since — but written back to what it was? A peer who reverted
        // a file has not invalidated anything, and a stale flag that fires on
        // that is the one that teaches agents to ignore the flag.
        if let Some(root) = repo_root {
            if let (Some(now), Some(then)) = (hash_file(root, p), held.fact.hashes.get(p)) {
                if &now == then {
                    continue;
                }
            }
        }
        if worst.is_none_or(|(_, _, w)| *ts > w) {
            worst = Some((p, session, *ts));
        }
    }
    let (path, session, _) = worst?;
    Some(format!("possibly stale: {} changed {} since", users(session), path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        init_schema(&c).unwrap();
        c
    }

    fn scope() -> Scope {
        Scope { team: "t1".into(), repo: "api".into(), area: "auth".into() }
    }

    /// Seal a fact the way `publish_fact` does, so the tests exercise the real
    /// binding rather than a shape of their own.
    fn shard_of(fact: &Fact, author: &str, email: &str, supersedes: Option<&str>) -> Shard {
        let p = Plaintext;
        let scope = scope();
        let id = format!("sh_{}", uuid::Uuid::new_v4().simple());
        let (epoch, secret) = p.epoch(&scope);
        let key = scope.key();
        let plain = serde_json::to_vec(fact).unwrap();
        let a = aad(&id, &key, "facts", author, email, epoch);
        let sealed = p.seal(&scope, &a, &plain);
        Shard {
            id,
            scope: key,
            kind: "facts".into(),
            author: author.into(),
            author_email: email.into(),
            device: "d1".into(),
            name_blind: name_blind(&secret, &fact.name),
            supersedes: supersedes.map(str::to_string),
            epoch,
            nonce: sealed.nonce,
            ciphertext: sealed.ciphertext,
            bytes: plain.len() as i64,
            seq: 0,
            created_ts: crate::proto::now_ms(),
            expires_ts: None,
        }
    }

    fn fact(name: &str, text: &str, paths: &[&str]) -> Fact {
        Fact {
            name: name.into(),
            text: text.into(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
            hashes: Default::default(),
            decisions: Vec::new(),
            derived: false,
        }
    }

    // ---------------------------------------------------------- scoping

    /// MemClaw's production leak: a GET-by-id that skipped the scope check
    /// every other path performed.
    #[test]
    fn fetch_by_id_enforces_scope() {
        let c = db();
        let s = shard_of(&fact("f", "x", &[]), "m1", "a@b.c", None);
        put(&c, &s, DEFAULT_BUDGET_BYTES).unwrap();

        assert!(get(&c, &s.id, &["t1/api/auth".to_string()]).is_some(), "held: readable");
        assert!(
            get(&c, &s.id, &["t1/api/billing".to_string()]).is_none(),
            "a scope this caller does not hold is not readable by id either"
        );
        assert!(get(&c, &s.id, &[]).is_none(), "and holding nothing reads nothing");
    }

    #[test]
    fn a_sync_returns_only_the_scopes_asked_for() {
        let c = db();
        let mine = shard_of(&fact("f", "x", &[]), "m1", "a@b.c", None);
        put(&c, &mine, DEFAULT_BUDGET_BYTES).unwrap();
        let mut theirs = shard_of(&fact("g", "y", &[]), "m2", "d@e.f", None);
        theirs.scope = "t1/api/billing".into();
        put(&c, &theirs, DEFAULT_BUDGET_BYTES).unwrap();

        let got = since(&c, &["t1/api/auth".to_string()], 0, 100);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, mine.id);
        assert!(since(&c, &[], 0, 100).is_empty(), "no scopes, no shards");
    }

    // ---------------------------------------------------- supersession

    /// MemClaw's other production bug: a near-duplicate filter that rejected a
    /// contradicting write before contradiction detection could see it. A
    /// contradiction *is* a near-duplicate.
    #[test]
    fn a_contradicting_fact_is_stored_as_a_supersession_not_rejected_as_a_duplicate() {
        let c = db();
        let first = shard_of(&fact("retry", "the client retries 3 times", &[]), "m1", "a@b.c", None);
        put(&c, &first, DEFAULT_BUDGET_BYTES).unwrap();
        let second = shard_of(
            &fact("retry", "the client retries 5 times, not 3", &[]),
            "m1",
            "a@b.c",
            Some(&first.id),
        );
        put(&c, &second, DEFAULT_BUDGET_BYTES).expect("a contradiction must be storable");

        let mut cache = Cache::default();
        for s in since(&c, &["t1/api/auth".to_string()], 0, 100) {
            assert!(cache.apply(&Plaintext, &scope(), s));
        }
        let heads = cache.heads();
        assert_eq!(heads.len(), 1, "one standing statement, not two");
        assert_eq!(heads[0].fact.text, "the client retries 5 times, not 3");
        assert!(cache.by_id(&first.id).is_some(), "and the superseded one is still on the record");
    }

    #[test]
    fn two_people_may_hold_the_same_name_without_colliding() {
        let c = db();
        put(&c, &shard_of(&fact("style", "tabs", &[]), "m1", "a@b.c", None), DEFAULT_BUDGET_BYTES)
            .unwrap();
        put(&c, &shard_of(&fact("style", "spaces", &[]), "m2", "d@e.f", None), DEFAULT_BUDGET_BYTES)
            .expect("shards are sharded by author; one name per person, not per room");
        assert_eq!(since(&c, &["t1/api/auth".to_string()], 0, 100).len(), 2);
    }

    // -------------------------------------------------------- integrity

    #[test]
    fn a_shard_whose_metadata_was_tampered_with_fails_to_open() {
        let mut s = shard_of(&fact("f", "trust me", &[]), "m1", "a@b.c", None);
        let mut cache = Cache::default();
        assert!(cache.apply(&Plaintext, &scope(), s.clone()), "untouched, it opens");

        // The interesting attack is not on the content — under `Plaintext` the
        // content is readable anyway — it is on the *metadata* the relay
        // routes by. Re-attributing a fact to someone else must not be silent.
        s.author_email = "someone.else@example.com".into();
        let mut c2 = Cache::default();
        assert!(!c2.apply(&Plaintext, &scope(), s.clone()), "a rewritten author does not open");
        assert_eq!(c2.unreadable(), 1, "and is counted rather than hidden");

        let mut s2 = shard_of(&fact("f", "trust me", &[]), "m1", "a@b.c", None);
        s2.scope = "t1/api/billing".into();
        let mut c3 = Cache::default();
        assert!(!c3.apply(&Plaintext, &scope(), s2), "nor does a shard moved to another area");
    }

    // -------------------------------------------------------- staleness

    #[test]
    fn a_fact_about_a_path_is_marked_stale_when_that_path_is_written() {
        let f = fact("retry", "the client retries 3 times", &["src/http/client.rs"]);
        let s = shard_of(&f, "m1", "ash@example.com", None);
        let mut cache = Cache::default();
        cache.apply(&Plaintext, &scope(), s.clone());
        let held = cache.by_id(&s.id).unwrap();
        let users = |x: &str| format!("{x}-user");

        let mut writes = std::collections::HashMap::new();
        assert!(
            staleness(held, &writes, &users, None).is_none(),
            "nothing has been written; nothing is stale"
        );

        // A write to a file the fact is not about says nothing about it.
        writes.insert("src/other.rs".to_string(), ("s2".to_string(), s.created_ts + 1000));
        assert!(staleness(held, &writes, &users, None).is_none());

        // A write to the file it *is* about names who moved the ground.
        writes.insert("src/http/client.rs".to_string(), ("s2".to_string(), s.created_ts + 1000));
        let why = staleness(held, &writes, &users, None).expect("this is the signal");
        assert!(why.contains("s2-user"), "{why}");
        assert!(why.contains("src/http/client.rs"), "{why}");

        // A write that predates the fact does not: the fact already knows.
        let mut older = std::collections::HashMap::new();
        older.insert("src/http/client.rs".to_string(), ("s2".to_string(), s.created_ts - 1000));
        assert!(staleness(held, &older, &users, None).is_none());
    }

    #[test]
    fn a_fact_surfaces_for_the_directory_it_is_about() {
        let s = shard_of(&fact("http", "we use one client", &["src/http"]), "m1", "a@b.c", None);
        let mut cache = Cache::default();
        cache.apply(&Plaintext, &scope(), s);
        assert_eq!(cache.about(Kind::Facts, &["src/http/retry.rs".to_string()]).len(), 1);
        assert_eq!(cache.about(Kind::Facts, &["src/auth/token.rs".to_string()]).len(), 0);
    }

    // --------------------------------------------------------- refusals

    #[test]
    fn a_secret_in_the_text_is_refused() {
        assert!(refuse_text("the api key is knt_2f6c9a1b3d4e5f60718293a4b5c6d7e8").is_some());
        assert!(refuse_text("set GITHUB_TOKEN to ghp_abcdefghijklmnop").is_some());
        assert!(refuse_text("-----BEGIN RSA PRIVATE KEY-----").is_some());
        assert!(refuse_text("AKIAIOSFODNN7EXAMPLE is the key id").is_some());
        assert!(
            refuse_text("token=Zm9vYmFyQmF6MTIzNDU2Nzg5MDEyMzQ1Njc4OTBhYmM").is_some(),
            "a long high-entropy run with no prefix we know is still a credential"
        );
        assert!(refuse_text(&"x".repeat(MAX_SHARD_BYTES + 1)).is_some());

        // And the ordinary case is not refused, or nobody publishes anything.
        assert!(refuse_text("the retry budget is 3 attempts with jittered backoff").is_none());
        assert!(
            refuse_text("see docs/architecture-and-deployment-notes.md for the rest").is_none(),
            "a long word is not a secret"
        );
    }

    #[test]
    fn a_dotenv_is_refused_by_its_path() {
        let root = std::path::Path::new("/nonexistent");
        assert!(matches!(refuse_path(root, ".env"), Some(Refusal::SensitivePath(_))));
        assert!(matches!(refuse_path(root, "app/.env.local"), Some(Refusal::SensitivePath(_))));
        assert!(matches!(refuse_path(root, "certs/server.pem"), Some(Refusal::SensitivePath(_))));
        assert!(matches!(refuse_path(root, "infra/prod.tfvars"), Some(Refusal::SensitivePath(_))));
        assert!(matches!(refuse_path(root, "~/.ssh/id_rsa"), Some(Refusal::SensitivePath(_))));
        assert!(refuse_path(root, "src/http/client.rs").is_none());
    }

    // ------------------------------------------------------ collection

    #[test]
    fn a_full_room_evicts_superseded_history_and_never_a_standing_fact() {
        let c = db();
        let big = "y".repeat(4096);
        let first = shard_of(&fact("a", &big, &[]), "m1", "a@b.c", None);
        put(&c, &first, DEFAULT_BUDGET_BYTES).unwrap();
        let second = shard_of(&fact("a", &big, &[]), "m1", "a@b.c", Some(&first.id));
        put(&c, &second, DEFAULT_BUDGET_BYTES).unwrap();
        let standing = shard_of(&fact("b", &big, &[]), "m1", "a@b.c", None);

        // A budget too small for all three: the superseded one goes.
        put(&c, &standing, 9_000).unwrap();
        let left: Vec<String> =
            since(&c, &["t1/api/auth".to_string()], 0, 100).into_iter().map(|s| s.id).collect();
        assert!(!left.contains(&first.id), "superseded history is what a full room can spare");
        assert!(left.contains(&second.id) && left.contains(&standing.id), "heads survive");
    }

    #[test]
    fn a_shard_larger_than_the_cap_is_refused_by_the_store_too() {
        let c = db();
        let mut s = shard_of(&fact("a", "x", &[]), "m1", "a@b.c", None);
        s.bytes = (MAX_SHARD_BYTES + 1) as i64;
        assert!(
            put(&c, &s, DEFAULT_BUDGET_BYTES).is_err(),
            "the client checks this, and the store does not take the client's word for it"
        );
    }

    #[test]
    fn a_departing_member_takes_their_shards_and_nobody_elses() {
        let c = db();
        put(&c, &shard_of(&fact("a", "x", &[]), "m1", "a@b.c", None), DEFAULT_BUDGET_BYTES).unwrap();
        put(&c, &shard_of(&fact("b", "y", &[]), "m2", "d@e.f", None), DEFAULT_BUDGET_BYTES).unwrap();
        assert_eq!(forget_author(&c, "t1", "m1"), 1);
        let left = since(&c, &["t1/api/auth".to_string()], 0, 100);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].author, "m2");
    }

    // ---------------------------------------------------------- search

    #[test]
    fn recall_finds_a_fact_by_its_words_and_not_by_filler() {
        let c = db();
        let mut cache = Cache::default();
        for f in [
            fact("retry", "the http client retries three times with backoff", &["src/http"]),
            fact("tax", "invoice tax rounds half up", &["src/billing"]),
        ] {
            let s = shard_of(&f, "m1", "a@b.c", None);
            put(&c, &s, DEFAULT_BUDGET_BYTES).unwrap();
            cache.apply(&Plaintext, &scope(), s);
        }
        let hits = cache.search("how does the http client handle retries");
        assert_eq!(hits.first().map(|h| h.fact.name.as_str()), Some("retry"));
        assert!(cache.search("kubernetes").is_empty(), "no match is empty, not everything");
        assert_eq!(cache.search("").len(), 2, "an empty query is `list`");
    }
}
