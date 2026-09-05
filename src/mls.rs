//! The MLS half of the key provider: a room is an MLS group, a device is a
//! leaf, and the epoch secret a shard is sealed under is exported from the
//! group.
//!
//! Why RFC 9420 rather than a scheme of our own. The problem is: several
//! people's machines must agree on a key for an area's memory, a new member
//! must be able to read what is current, and a removed member must not be
//! able to read what comes next. That is continuous group key agreement, it
//! scales to thousands with O(log n) work per change, forward secrecy and
//! post-compromise security fall out of the protocol rather than out of a
//! rotation ceremony we would have to design and then get wrong, and there is
//! a Rust implementation. Anything we invented here would be worse and cost
//! more.
//!
//! What MLS is and is not used for. It agrees the *key*. The sealing is
//! ordinary AEAD under the exported secret (`memory::encrypt`), which is why
//! the relay stores plain sealed bytes and knows nothing about MLS beyond
//! forwarding handshake messages it cannot read. The relay is the Delivery
//! Service: it holds key packages, assigns a total order to commits, and fans
//! them out. RFC 9750 §5 is explicit that a DS need not be trusted with
//! content, and this one is not.
//!
//! Failing open is preserved. Every entry point returns a `Result` and every
//! caller treats an error as "no memory", never as "no write".

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use openmls::prelude::{tls_codec::*, *};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use openmls_traits::OpenMlsProvider;

use crate::memory::{KeyProvider, Scope, Secret};

/// One ciphersuite, named once. MLS negotiates these, but a product that
/// offers a choice nobody can evaluate has added a decision, not a feature.
const SUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// The label the memory epoch secret is exported under. Fixed, and distinct
/// from anything else the group derives, so a secret used for memory can never
/// be a secret used for something else.
const EXPORTER_LABEL: &str = "knoot memory";

/// This device's MLS identity and the groups it is in.
///
/// One per machine per person, matching the `devices` row the key was minted
/// against — which is what makes "a removed device cannot derive the next
/// epoch" a statement about a laptop rather than about a person.
pub struct Device {
    provider: OpenMlsRustCrypto,
    signer: SignatureKeyPair,
    credential: CredentialWithKey,
    /// Device id, as the relay knows it. Also the MLS credential identity, so
    /// a `Remove` can name a leaf from a row in the console.
    pub device_id: String,
    groups: HashMap<String, MlsGroup>,
    /// Epoch secrets this device has held, per `(room, scope, epoch)`.
    ///
    /// Kept because a shard sealed two epochs ago must stay readable until it
    /// is rewrapped or expires. Forward secrecy is about what a *removed*
    /// device can derive going forward; it does not require a current member
    /// to forget what it has already read.
    held: HashMap<(String, String, u64), Secret>,
    dir: PathBuf,
}

impl Device {
    /// Load this device's MLS state, or create it.
    ///
    /// The state lives beside the credential, in `~/.knoot/mls/<device>`, mode
    /// 0700. It is key material: a copy of it is a copy of this device's
    /// ability to read the room.
    pub fn open(dir: &Path, device_id: &str) -> Result<Self> {
        let dir = dir.join(device_id);
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let provider = OpenMlsRustCrypto::default();
        restore_storage(&provider, &dir.join("storage.json"));

        // The signature key is the one thing that must be the same across
        // restarts: it is what this leaf is known by inside the group.
        let signer = match std::fs::read(dir.join("signer.bin")) {
            Ok(bytes) => SignatureKeyPair::tls_deserialize_exact(bytes.as_slice())
                .context("this device's MLS signing key is corrupt")?,
            Err(_) => {
                let kp = SignatureKeyPair::new(SUITE.signature_algorithm())?;
                let bytes = kp.tls_serialize_detached()?;
                write_private(&dir.join("signer.bin"), &bytes)?;
                kp
            }
        };
        signer.store(provider.storage()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let credential = CredentialWithKey {
            credential: BasicCredential::new(device_id.as_bytes().to_vec()).into(),
            signature_key: signer.to_public_vec().into(),
        };

        let mut me = Self {
            provider,
            signer,
            credential,
            device_id: device_id.to_string(),
            groups: HashMap::new(),
            held: HashMap::new(),
            dir,
        };
        me.reload_groups();
        Ok(me)
    }

    /// Groups whose state survived in storage, brought back into memory.
    fn reload_groups(&mut self) {
        let Ok(rooms) = std::fs::read_to_string(self.dir.join("rooms.json")) else { return };
        let Ok(rooms) = serde_json::from_str::<Vec<String>>(&rooms) else { return };
        for room in rooms {
            let gid = GroupId::from_slice(room.as_bytes());
            if let Ok(Some(g)) = MlsGroup::load(self.provider.storage(), &gid) {
                self.groups.insert(room, g);
            }
        }
    }

    fn persist(&self) {
        let rooms: Vec<&String> = self.groups.keys().collect();
        if let Ok(j) = serde_json::to_vec(&rooms) {
            let _ = write_private(&self.dir.join("rooms.json"), &j);
        }
        dump_storage(&self.provider, &self.dir.join("storage.json"));
    }

    pub fn rooms(&self) -> Vec<String> {
        self.groups.keys().cloned().collect()
    }

    pub fn in_room(&self, room: &str) -> bool {
        self.groups.contains_key(room)
    }

    /// A key package for this device: the credential and one-time keys another
    /// member needs in order to add it. Uploaded once at `knoot join`.
    pub fn key_package(&self) -> Result<Vec<u8>> {
        let bundle = KeyPackage::builder()
            .build(SUITE, &self.provider, &self.signer, self.credential.clone())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let bytes = MlsMessageOut::from(bundle.key_package().clone()).tls_serialize_detached()?;
        // The private half is in storage, so it has to reach disk before the
        // public half reaches the relay — otherwise a restart between the two
        // leaves a key package nobody can use.
        dump_storage(&self.provider, &self.dir.join("storage.json"));
        Ok(bytes)
    }

    /// Create a room's group, with this device as its only member.
    ///
    /// The group id is the room id, so the DS can order a room's commits
    /// without parsing them and a daemon can ask for the right stream.
    pub fn create_room(&mut self, room: &str) -> Result<()> {
        if self.groups.contains_key(room) {
            return Ok(());
        }
        let group = MlsGroup::builder()
            .ciphersuite(SUITE)
            // The tree in the message, so a joining device needs the Welcome
            // and nothing else. A DS that had to serve ratchet trees would be
            // a DS with a second thing to get wrong.
            .use_ratchet_tree_extension(true)
            .with_group_id(GroupId::from_slice(room.as_bytes()))
            .build(&self.provider, &self.signer, self.credential.clone())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        self.groups.insert(room.to_string(), group);
        self.persist();
        Ok(())
    }

    /// Add a device to a room. Returns the commit for everyone and the welcome
    /// for the joiner, both opaque to the relay that carries them.
    pub fn add_device(&mut self, room: &str, key_package: &[u8]) -> Result<Handshake> {
        let body = MlsMessageIn::tls_deserialize_exact(key_package)
            .context("malformed key package")?
            .extract();
        let MlsMessageBodyIn::KeyPackage(kp) = body else {
            anyhow::bail!("that is not a key package");
        };
        // Validated, not merely deserialised: the signature, the lifetime and
        // the version are checked here rather than taken on the word of the
        // relay that carried it. The DS is not trusted with content and it is
        // not trusted with this either.
        let kp = kp
            .validate(self.provider.crypto(), ProtocolVersion::Mls10)
            .map_err(|e| anyhow::anyhow!("that key package does not verify: {e}"))?;
        // Fields borrowed directly rather than through a helper: `group_mut`
        // would borrow all of `self`, and the group, the provider and the
        // signer are three disjoint fields.
        let (provider, signer) = (&self.provider, &self.signer);
        let group = self
            .groups
            .get_mut(room)
            .ok_or_else(|| anyhow::anyhow!("this device is not in {room}"))?;
        let (commit, welcome, _) = group
            .add_members(provider, signer, core::slice::from_ref(&kp))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        // The epoch the commit moves the room *to*. Genesis owns zero, so a
        // commit built on epoch n claims n+1 — and the DS's unique index on
        // (room, epoch) is then exactly "one commit per epoch".
        let epoch = group.epoch().as_u64() + 1;
        Ok(Handshake {
            epoch,
            commit: commit.tls_serialize_detached()?,
            welcome: Some(welcome.tls_serialize_detached()?),
        })
    }

    /// Remove a device from a room.
    ///
    /// After the commit merges the group is in a new epoch that the removed
    /// leaf cannot derive — which is the whole property, and is the protocol's
    /// rather than ours.
    pub fn remove_device(&mut self, room: &str, device_id: &str) -> Result<Handshake> {
        let (provider, signer) = (&self.provider, &self.signer);
        let group = self
            .groups
            .get_mut(room)
            .ok_or_else(|| anyhow::anyhow!("this device is not in {room}"))?;
        let leaf = group
            .members()
            .find(|m| m.credential.serialized_content() == device_id.as_bytes())
            .map(|m| m.index)
            .ok_or_else(|| anyhow::anyhow!("{device_id} is not in this room"))?;
        let (commit, _, _) = group
            .remove_members(provider, signer, &[leaf])
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let epoch = group.epoch().as_u64() + 1;
        Ok(Handshake { epoch, commit: commit.tls_serialize_detached()?, welcome: None })
    }

    /// Commit a change this device proposed, once the DS has accepted it.
    ///
    /// Two-phase on purpose. The DS assigns the total order, and a commit it
    /// rejected — because someone else's landed first — must not be merged
    /// here, or this device would be in an epoch the room is not.
    pub fn merge_own(&mut self, room: &str) -> Result<()> {
        let provider = &self.provider;
        let group = self
            .groups
            .get_mut(room)
            .ok_or_else(|| anyhow::anyhow!("this device is not in {room}"))?;
        group.merge_pending_commit(provider).map_err(|e| anyhow::anyhow!("{e}"))?;
        self.persist();
        Ok(())
    }

    /// Throw away a commit the DS rejected, so this device can re-sync and
    /// try again from the epoch the room is actually in.
    pub fn discard_own(&mut self, room: &str) -> Result<()> {
        let storage = self.provider.storage();
        let group = self
            .groups
            .get_mut(room)
            .ok_or_else(|| anyhow::anyhow!("this device is not in {room}"))?;
        group.clear_pending_commit(storage).map_err(|e| anyhow::anyhow!("{e}"))?;
        self.persist();
        Ok(())
    }

    /// Apply a commit somebody else made, in the order the DS gave it.
    pub fn process(&mut self, room: &str, message: &[u8]) -> Result<()> {
        let msg = MlsMessageIn::tls_deserialize_exact(message).context("malformed handshake")?;
        let protocol = msg
            .try_into_protocol_message()
            .map_err(|e| anyhow::anyhow!("not a protocol message: {e}"))?;
        let provider = &self.provider;
        let group = self
            .groups
            .get_mut(room)
            .ok_or_else(|| anyhow::anyhow!("this device is not in {room}"))?;
        let processed =
            group.process_message(provider, protocol).map_err(|e| anyhow::anyhow!("{e}"))?;
        if let ProcessedMessageContent::StagedCommitMessage(staged) = processed.into_content() {
            group.merge_staged_commit(provider, *staged).map_err(|e| anyhow::anyhow!("{e}"))?;
            self.persist();
        }
        Ok(())
    }

    /// Join a room from a welcome addressed to this device.
    pub fn join(&mut self, room: &str, welcome: &[u8]) -> Result<()> {
        if self.groups.contains_key(room) {
            return Ok(());
        }
        let body =
            MlsMessageIn::tls_deserialize_exact(welcome).context("malformed welcome")?.extract();
        let MlsMessageBodyIn::Welcome(welcome) = body else {
            anyhow::bail!("that is not a welcome");
        };
        let cfg = MlsGroupJoinConfig::builder().use_ratchet_tree_extension(true).build();
        let group = StagedWelcome::new_from_welcome(&self.provider, &cfg, welcome, None)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .into_group(&self.provider)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let id = String::from_utf8_lossy(group.group_id().as_slice()).to_string();
        self.groups.insert(if id.is_empty() { room.to_string() } else { id }, group);
        self.persist();
        Ok(())
    }

    /// Drop a room's group from this device.
    ///
    /// Used when the Delivery Service refuses this device's genesis because
    /// another machine started the room first: two devices that both built a
    /// group for one room have built two rooms, and the one that lost must
    /// forget its own and wait to be welcomed into the real one.
    pub fn forget_room(&mut self, room: &str) {
        if let Some(mut g) = self.groups.remove(room) {
            let _ = g.delete(self.provider.storage());
        }
        self.held.retain(|(r, _, _), _| r != room);
        self.persist();
    }

    /// Devices currently in a room's group, by credential identity.
    pub fn members(&self, room: &str) -> Vec<String> {
        self.groups
            .get(room)
            .map(|g| {
                g.members()
                    .map(|m| String::from_utf8_lossy(m.credential.serialized_content()).to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The epoch this device believes a room is in.
    pub fn epoch(&self, room: &str) -> Option<u64> {
        self.groups.get(room).map(|g| g.epoch().as_u64())
    }

    /// The memory secret for a scope, exported from the room's current epoch.
    ///
    /// The scope is the exporter *context*, so two areas of one repo never
    /// share a key even though they share a group — an area is the unit of who
    /// may read, and a key that spanned areas would make that a fiction.
    pub fn export(&mut self, room: &str, scope: &Scope) -> Result<(u64, Secret)> {
        let group = self.groups.get(room).ok_or_else(|| anyhow::anyhow!("not in {room}"))?;
        let epoch = group.epoch().as_u64();
        let key = scope.key();
        if let Some(s) = self.held.get(&(room.to_string(), key.clone(), epoch)) {
            return Ok((epoch, s.clone()));
        }
        let raw = group
            .export_secret(self.provider.crypto(), EXPORTER_LABEL, key.as_bytes(), 32)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut out = [0u8; 32];
        out.copy_from_slice(&raw[..32]);
        let secret = Secret(out);
        self.held.insert((room.to_string(), key, epoch), secret.clone());
        Ok((epoch, secret))
    }

    /// A past epoch's secret, if this device still holds it.
    pub fn held_secret(&self, room: &str, scope: &Scope, epoch: u64) -> Option<Secret> {
        self.held.get(&(room.to_string(), scope.key(), epoch)).cloned()
    }

}

/// A change to a room's membership, on its way to the Delivery Service.
pub struct Handshake {
    /// The epoch the commit was built on, so the DS can reject one that raced.
    pub epoch: u64,
    pub commit: Vec<u8>,
    /// Present for an Add. Addressed to the joining device; every other member
    /// needs the commit instead.
    pub welcome: Option<Vec<u8>>,
}

// ------------------------------------------------------------- persistence

/// `MemoryStorage` is a byte map with a public field, so persisting a device's
/// group state is dumping and restoring that map. Not elegant, and the honest
/// alternative — openmls's sqlite provider — is a second storage backend in a
/// binary that already has one.
fn dump_storage(provider: &OpenMlsRustCrypto, path: &Path) {
    let Ok(values) = provider.storage().values.read() else { return };
    let flat: Vec<(String, String)> = values
        .iter()
        .map(|(k, v)| (crate::memory::hex(k), crate::memory::hex(v)))
        .collect();
    if let Ok(j) = serde_json::to_vec(&flat) {
        let _ = write_private(path, &j);
    }
}

fn restore_storage(provider: &OpenMlsRustCrypto, path: &Path) {
    let Ok(bytes) = std::fs::read(path) else { return };
    let Ok(flat) = serde_json::from_slice::<Vec<(String, String)>>(&bytes) else { return };
    let Ok(mut values) = provider.storage().values.write() else { return };
    for (k, v) in flat {
        values.insert(crate::memory::unhex(&k), crate::memory::unhex(&v));
    }
}

/// Mode 0600, written whole. Key material never gets a mode-0644 window.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------- provider

/// The hosted tier's key provider: epoch secrets from the room's MLS group.
///
/// Wraps the device behind a lock because the daemon seals from whichever task
/// is handling a request, and exporting a secret touches group state.
pub struct Mls {
    device: std::sync::Arc<std::sync::Mutex<Device>>,
    /// Which room's group covers a scope. A member's key grants the union of
    /// their rooms' areas, so this is the same mapping the relay enforces —
    /// resolved once when the daemon learns its rooms.
    rooms: std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
}

impl Mls {
    pub fn new(device: std::sync::Arc<std::sync::Mutex<Device>>) -> Self {
        Self { device, rooms: Default::default() }
    }

    /// Say that a scope's memory belongs to a room's group.
    pub fn bind(&self, scope_key: &str, room: &str) {
        self.rooms.lock().unwrap().insert(scope_key.to_string(), room.to_string());
    }

    fn room_for(&self, scope: &Scope) -> Option<String> {
        let key = scope.key();
        let rooms = self.rooms.lock().unwrap();
        if let Some(r) = rooms.get(&key) {
            return Some(r.clone());
        }
        // The area's own scope is not bound, but the repo's root scope may be:
        // a room that grants `/` covers every area under it.
        let root = Scope { area: crate::config::ROOT_AREA.to_string(), ..scope.clone() };
        rooms.get(&root.key()).cloned()
    }
}

impl KeyProvider for Mls {
    fn label(&self) -> &'static str {
        "mls"
    }

    /// A scope with no room, a device not yet in that room, or an export that
    /// fails, all end the same way: an epoch of zero and a zero secret, which
    /// seals nothing anyone can read and — because `epoch_secret` refuses
    /// epoch zero — opens nothing either. The caller treats that as "no
    /// memory", which is the only acceptable failure here.
    fn epoch(&self, scope: &Scope) -> (u64, Secret) {
        let Some(room) = self.room_for(scope) else { return (0, Secret([0u8; 32])) };
        let mut dev = self.device.lock().unwrap();
        match dev.export(&room, scope) {
            // Epochs are one-based here so that zero can mean "no key".
            Ok((e, s)) => (e + 1, s),
            Err(_) => (0, Secret([0u8; 32])),
        }
    }

    /// The secret for an epoch, deriving it if it is the one the room is in.
    ///
    /// Deriving matters: a device that has never *published* has never called
    /// `epoch`, so nothing has put the current secret in `held` — and a
    /// read-only member could see every shard in the room and open none of
    /// them. Past epochs are only ever looked up, never derived, because MLS
    /// cannot derive them and that is the property the whole design rests on.
    fn epoch_secret(&self, scope: &Scope, epoch: u64) -> Option<Secret> {
        if epoch == 0 {
            return None;
        }
        let room = self.room_for(scope)?;
        let mut dev = self.device.lock().unwrap();
        if dev.epoch(&room) == Some(epoch - 1) {
            return dev.export(&room, scope).ok().map(|(_, s)| s);
        }
        dev.held_secret(&room, scope, epoch - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("knoot-mls-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn scope() -> Scope {
        Scope { team: "t1".into(), repo: "api".into(), area: "/".into() }
    }

    /// The point of using a group protocol at all: two machines that have
    /// never exchanged a secret directly agree on one.
    #[test]
    fn two_devices_in_a_room_export_the_same_memory_secret() {
        let dir = tmpdir("agree");
        let mut ash = Device::open(&dir, "d-ash").unwrap();
        let mut priya = Device::open(&dir, "d-priya").unwrap();

        ash.create_room("rm1").unwrap();
        let kp = priya.key_package().unwrap();
        let hs = ash.add_device("rm1", &kp).unwrap();
        ash.merge_own("rm1").unwrap();
        priya.join("rm1", hs.welcome.as_ref().unwrap()).unwrap();

        let (ae, asec) = ash.export("rm1", &scope()).unwrap();
        let (pe, psec) = priya.export("rm1", &scope()).unwrap();
        assert_eq!(ae, pe, "same epoch");
        assert_eq!(asec.0, psec.0, "and the same secret, never sent anywhere");
    }

    /// Two areas of one repo share a group and must not share a key: an area
    /// is the unit of who may read, and one key across areas makes that a
    /// fiction.
    #[test]
    fn two_areas_of_one_repo_do_not_share_a_key() {
        let dir = tmpdir("areas");
        let mut ash = Device::open(&dir, "d1").unwrap();
        ash.create_room("rm1").unwrap();
        let auth = Scope { area: "auth".into(), ..scope() };
        let billing = Scope { area: "billing".into(), ..scope() };
        assert_ne!(
            ash.export("rm1", &auth).unwrap().1 .0,
            ash.export("rm1", &billing).unwrap().1 .0
        );
    }

    /// The property the hosted tier is sold on. After a Remove the group is in
    /// an epoch the departed leaf cannot derive — and this is the protocol's
    /// guarantee, not ours.
    #[test]
    fn a_removed_device_cannot_derive_the_next_epoch() {
        let dir = tmpdir("remove");
        let mut ash = Device::open(&dir, "d-ash").unwrap();
        let mut priya = Device::open(&dir, "d-priya").unwrap();
        ash.create_room("rm1").unwrap();
        let hs = ash.add_device("rm1", &priya.key_package().unwrap()).unwrap();
        ash.merge_own("rm1").unwrap();
        priya.join("rm1", hs.welcome.as_ref().unwrap()).unwrap();

        let shared = ash.export("rm1", &scope()).unwrap();
        assert_eq!(shared.1 .0, priya.export("rm1", &scope()).unwrap().1 .0);

        // Ash removes priya's laptop.
        let hs = ash.remove_device("rm1", "d-priya").unwrap();
        ash.merge_own("rm1").unwrap();

        let after = ash.export("rm1", &scope()).unwrap();
        assert_ne!(after.0, shared.0, "the room moved on");
        assert_ne!(after.1 .0, shared.1 .0, "to a secret priya's laptop never held");

        // And priya's laptop, handed the commit that removed it, cannot follow
        // the group into that epoch.
        let _ = priya.process("rm1", &hs.commit);
        match priya.export("rm1", &scope()) {
            Err(_) => {}
            Ok((e, s)) => {
                assert_ne!(s.0, after.1 .0, "a removed device must not hold the new secret");
                assert_ne!(e, after.0, "nor reach the new epoch");
            }
        }
    }

    /// A daemon restarts. If group state did not survive, every fact in the
    /// room would become unreadable on a reboot — which is worse than no
    /// encryption, because it looks like data loss.
    #[test]
    fn a_devices_group_state_survives_a_restart() {
        let dir = tmpdir("restart");
        let before = {
            let mut ash = Device::open(&dir, "d1").unwrap();
            ash.create_room("rm1").unwrap();
            ash.export("rm1", &scope()).unwrap()
        };
        let mut again = Device::open(&dir, "d1").unwrap();
        assert!(again.in_room("rm1"), "the room came back");
        let after = again.export("rm1", &scope()).unwrap();
        assert_eq!(before.0, after.0);
        assert_eq!(before.1 .0, after.1 .0, "and so did the key");
    }

    /// A commit the DS rejected must not be merged locally, or this device
    /// ends up in an epoch the room is not in and everything it seals is
    /// unreadable to everyone else.
    #[test]
    fn a_rejected_commit_leaves_the_device_in_the_rooms_epoch() {
        let dir = tmpdir("reject");
        let mut ash = Device::open(&dir, "d1").unwrap();
        let priya = Device::open(&dir, "d2").unwrap();
        ash.create_room("rm1").unwrap();
        let at = ash.epoch("rm1").unwrap();

        ash.add_device("rm1", &priya.key_package().unwrap()).unwrap();
        ash.discard_own("rm1").unwrap();
        assert_eq!(ash.epoch("rm1"), Some(at), "discarding leaves the epoch where it was");

        // And the device can still propose afterwards.
        let hs = ash.add_device("rm1", &priya.key_package().unwrap()).unwrap();
        ash.merge_own("rm1").unwrap();
        assert_eq!(ash.epoch("rm1"), Some(at + 1));
        assert!(hs.welcome.is_some());
    }

    /// The provider wrapper, which is what `memory.rs` actually seals through.
    #[test]
    fn the_provider_seals_and_opens_through_the_group() {
        let dir = tmpdir("provider");
        let dev = std::sync::Arc::new(std::sync::Mutex::new(Device::open(&dir, "d1").unwrap()));
        dev.lock().unwrap().create_room("rm1").unwrap();
        let p = Mls::new(dev);
        p.bind(&scope().key(), "rm1");

        assert!(p.confidential(), "the hosted tier keeps nothing readable");
        let sealed = p.seal(&scope(), "aad", b"the client retries three times");
        assert!(
            !sealed.ciphertext.windows(6).any(|w| w == b"retrie"),
            "the plaintext must not be in the ciphertext"
        );
        assert_eq!(
            p.open(&scope(), sealed.epoch, "aad", &sealed.nonce, &sealed.ciphertext).as_deref(),
            Some(&b"the client retries three times"[..])
        );
        assert!(
            p.open(&scope(), sealed.epoch, "other-aad", &sealed.nonce, &sealed.ciphertext).is_none(),
            "and the metadata is bound, as under plaintext"
        );
    }

    /// Failing open, at the provider. A scope with no group must produce no
    /// memory — never an error that reaches an agent.
    #[test]
    fn a_scope_with_no_group_yields_no_key_and_no_panic() {
        let dir = tmpdir("nogroup");
        let dev = std::sync::Arc::new(std::sync::Mutex::new(Device::open(&dir, "d1").unwrap()));
        let p = Mls::new(dev);
        let unbound = Scope { team: "t1".into(), repo: "nope".into(), area: "/".into() };
        assert_eq!(p.epoch(&unbound).0, 0, "no room, no epoch");
        assert!(p.epoch_secret(&unbound, 0).is_none(), "and epoch zero opens nothing");
        let sealed = p.seal(&unbound, "aad", b"x");
        assert!(p.open(&unbound, sealed.epoch, "aad", &sealed.nonce, &sealed.ciphertext).is_none());
    }
}

// --------------------------------------------------- the delivery service
//
// The relay's half. RFC 9750 §5: a Delivery Service must order handshake
// messages and fan them out, and need not be trusted with content. This one
// is not — it stores key packages and opaque blobs, assigns each room's
// commits a total order, and cannot read any of it.

/// One entry in a room's handshake log.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Envelope {
    pub seq: i64,
    /// The epoch the commit moves the room *to*. The DS orders by this and
    /// nothing else, which is all the ordering MLS needs from it.
    pub epoch: u64,
    /// `commit` for everyone, `welcome` for one joining device.
    pub kind: String,
    #[serde(with = "crate::memory::hex_bytes")]
    pub blob: Vec<u8>,
    /// Set on a welcome: the device it is addressed to.
    pub for_device: Option<String>,
}

pub fn init_schema(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS mls_key_packages (
            id         TEXT PRIMARY KEY,
            team_id    TEXT NOT NULL,
            device_id  TEXT NOT NULL,
            kp         BLOB NOT NULL,
            created_ts INTEGER NOT NULL,
            consumed_ts INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_kp_device ON mls_key_packages (device_id, consumed_ts);

        CREATE TABLE IF NOT EXISTS mls_log (
            room_id    TEXT NOT NULL,
            seq        INTEGER NOT NULL,
            epoch      INTEGER NOT NULL,
            kind       TEXT NOT NULL,
            blob       BLOB NOT NULL,
            for_device TEXT,
            created_ts INTEGER NOT NULL,
            PRIMARY KEY (room_id, seq)
        );
        -- One commit per epoch per room. This single constraint is the whole
        -- of the DS's ordering job: two daemons that propose from the same
        -- epoch race, and exactly one wins. The loser discards its pending
        -- commit and tries again from where the room actually is.
        CREATE UNIQUE INDEX IF NOT EXISTS idx_mls_log_epoch
            ON mls_log (room_id, epoch) WHERE kind = 'commit';",
    )?;
    Ok(())
}

/// Store a device's key package. A device keeps one on file; uploading again
/// replaces it, because a key package is consumed by being used and a device
/// that has been added and removed must be addable again.
pub fn put_key_package(
    conn: &rusqlite::Connection,
    team: &str,
    device: &str,
    kp: &[u8],
) -> Result<()> {
    conn.execute("DELETE FROM mls_key_packages WHERE device_id = ?1", rusqlite::params![device])?;
    conn.execute(
        "INSERT INTO mls_key_packages (id, team_id, device_id, kp, created_ts) \
         VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![
            format!("kp_{}", uuid::Uuid::new_v4().simple()),
            team,
            device,
            kp,
            crate::proto::now_ms() as i64
        ],
    )?;
    Ok(())
}

/// The key package for a device, so a current member can add it.
pub fn key_package_for(conn: &rusqlite::Connection, team: &str, device: &str) -> Option<Vec<u8>> {
    conn.query_row(
        "SELECT kp FROM mls_key_packages WHERE team_id = ?1 AND device_id = ?2 \
         AND consumed_ts IS NULL ORDER BY created_ts DESC LIMIT 1",
        rusqlite::params![team, device],
        |r| r.get(0),
    )
    .ok()
}

/// Accept a commit, if it is the next one.
///
/// The epoch check is the arbitration, and it is the same shape as the claim
/// arbitration one layer up: the one decision only the server may make,
/// resolved under one lock so two answers cannot disagree.
pub fn append(
    conn: &rusqlite::Connection,
    room: &str,
    env: &Envelope,
) -> Result<i64> {
    let seq: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM mls_log WHERE room_id = ?1",
            rusqlite::params![room],
            |r| r.get(0),
        )
        .unwrap_or(1);
    conn.execute(
        "INSERT INTO mls_log (room_id, seq, epoch, kind, blob, for_device, created_ts) \
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![
            room,
            seq,
            env.epoch as i64,
            env.kind,
            env.blob,
            env.for_device,
            crate::proto::now_ms() as i64
        ],
    )
    .map_err(|e| anyhow::anyhow!("a commit for that epoch already landed ({e})"))?;
    Ok(seq)
}

/// A room's handshake log since `since`, filtered to what this device may act
/// on: every commit, and only welcomes addressed to it.
pub fn log_since(
    conn: &rusqlite::Connection,
    room: &str,
    device: &str,
    since: i64,
) -> Vec<Envelope> {
    let Ok(mut q) = conn.prepare(
        "SELECT seq, epoch, kind, blob, for_device FROM mls_log \
         WHERE room_id = ?1 AND seq > ?2 AND (for_device IS NULL OR for_device = ?3) \
         ORDER BY seq ASC LIMIT 500",
    ) else {
        return Vec::new();
    };
    q.query_map(rusqlite::params![room, since, device], |r| {
        Ok(Envelope {
            seq: r.get(0)?,
            epoch: r.get::<_, i64>(1)? as u64,
            kind: r.get(2)?,
            blob: r.get(3)?,
            for_device: r.get(4)?,
        })
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

/// Whether a room's group has been started at all.
pub fn has_group(conn: &rusqlite::Connection, room: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM mls_log WHERE room_id = ?1 AND kind = 'commit'",
        rusqlite::params![room],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}
