//! Pool membership persistence and pure data helpers.
//!
//! This module owns `pool.json` I/O and pure transformations over `PoolState` /
//! `PoolMembership`. Helpers here take already-probed data; they must NOT
//! import `CommandRunner` or call `probe_pool` internally. Each caller (e.g.
//! `add.rs`, `replace.rs`) keeps the best-effort `probe_pool` call local and
//! passes the resulting `PoolState` into membership helpers. This separation
//! keeps persistence decoupled from command execution.

use crate::state_io;
use crate::state_paths::StatePaths;
use crate::types::{ByIdPath, DiskName, LuksUuid, PoolState};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::btree_map;
use std::fmt;
use std::path::{Path, PathBuf};
use thiserror::Error;

// ---------------------------------------------------------------------------
// MembershipError
// ---------------------------------------------------------------------------

/// Errors raised by the membership module. The variant inventory is pinned
/// by the LUKS-UUID-identity migration plan: every error path that bubbles
/// up to `Display` lands in exactly one of these five shapes so operator
/// remediation wording is enumerable.
#[derive(Debug, Error)]
pub enum MembershipError {
    /// `pool.json` exists but cannot be parsed -- bad UUID key, stale
    /// value-side field, unknown top-level key, etc. The display string is
    /// pinned so the cutover and recovery docs can quote it verbatim.
    #[error(
        "pool membership file corrupt at {path}: {detail} -- run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/luks-unlock.md)"
    )]
    Corrupt { path: PathBuf, detail: String },

    /// Uniqueness violation surfaced from `PoolMembership::insert` or the
    /// load-time uniqueness sweep. The inner string follows the
    /// field/value/colliding-UUID patterns pinned in the plan.
    #[error("{0}")]
    Conflict(String),

    /// `by_devid` lookup hit a corrupt membership where multiple UUIDs
    /// carry the same persisted devid. Display enumerates every colliding
    /// UUID in canonical lexicographic order.
    #[error("duplicate devid {devid} in pool membership across UUIDs {}", format_uuid_list(.members))]
    DuplicateDevid { devid: u64, members: Vec<LuksUuid> },

    /// Read-side I/O failure that is NOT a parse error (file missing
    /// where required, EACCES, EIO).
    #[error("failed to read pool membership file at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Write-side I/O failure on `save_membership`.
    #[error("failed to write pool membership file at {path}: {source}")]
    Save {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn format_uuid_list(uuids: &[LuksUuid]) -> String {
    let mut sorted: Vec<&LuksUuid> = uuids.iter().collect();
    sorted.sort();
    sorted
        .into_iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// LuksUuidMap -- canonicalizing, fail-closed UUID-keyed map
// ---------------------------------------------------------------------------

/// UUID-keyed wrapper around `BTreeMap<LuksUuid, V>` with a custom
/// `Deserialize` that canonicalizes keys and rejects duplicates before
/// insertion. The fail-closed `insert` keeps in-process construction
/// agreeing with the Deserialize contract -- callers must `remove`
/// explicitly before re-inserting under the same UUID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct LuksUuidMap<V>(BTreeMap<LuksUuid, V>);

impl<V> LuksUuidMap<V> {
    /// Construct an empty UUID-keyed map. Production callers build maps
    /// explicitly through fail-closed `insert?` rather than collecting
    /// from iterators (see Plan: "No `FromIterator` impl").
    pub fn new() -> Self {
        LuksUuidMap(BTreeMap::new())
    }

    /// Borrow the value under `uuid`, if any. Lookups go through the
    /// canonicalized key already stored in the map.
    pub fn get(&self, uuid: &LuksUuid) -> Option<&V> {
        self.0.get(uuid)
    }

    /// Mutably borrow the value under `uuid`, if any.
    pub fn get_mut(&mut self, uuid: &LuksUuid) -> Option<&mut V> {
        self.0.get_mut(uuid)
    }

    /// Membership test against the canonicalized UUID key set.
    pub fn contains_key(&self, uuid: &LuksUuid) -> bool {
        self.0.contains_key(uuid)
    }

    /// Insert `value` under `uuid`. Errors closed if `uuid` already maps
    /// to a value; callers must `remove` explicitly before re-inserting.
    /// This mirrors the Deserialize duplicate-key contract so the
    /// in-process build path agrees with the on-disk shape.
    pub fn insert(&mut self, uuid: LuksUuid, value: V) -> Result<(), LuksUuidMapConflict> {
        if self.0.contains_key(&uuid) {
            return Err(LuksUuidMapConflict { uuid });
        }
        self.0.insert(uuid, value);
        Ok(())
    }

    /// Remove and return the value under `uuid`, if any.
    pub fn remove(&mut self, uuid: &LuksUuid) -> Option<V> {
        self.0.remove(uuid)
    }

    /// Iterate `(UUID, &value)` pairs in UUID-sorted order.
    pub fn iter(&self) -> btree_map::Iter<'_, LuksUuid, V> {
        self.0.iter()
    }

    /// Iterate UUID keys in sorted order.
    pub fn keys(&self) -> btree_map::Keys<'_, LuksUuid, V> {
        self.0.keys()
    }

    /// Number of entries in the map.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True iff the map has no entries.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<V> Default for LuksUuidMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, V> IntoIterator for &'a LuksUuidMap<V> {
    type Item = (&'a LuksUuid, &'a V);
    type IntoIter = btree_map::Iter<'a, LuksUuid, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Single-shape conflict error raised by `LuksUuidMap::insert` when the
/// caller attempts to insert under a UUID already present in the map.
/// `PoolMembership::insert` flattens this into a `Conflict(String)`;
/// `OpKind::Add` planning surfaces it through `AddError::DuplicateUuid`.
#[derive(Debug, Error)]
#[error("duplicate LUKS UUID: {uuid} already in LuksUuidMap")]
pub struct LuksUuidMapConflict {
    pub uuid: LuksUuid,
}

impl<'de, V> Deserialize<'de> for LuksUuidMap<V>
where
    V: Deserialize<'de>,
{
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor<V> {
            _marker: std::marker::PhantomData<V>,
        }

        impl<'de, V> serde::de::Visitor<'de> for Visitor<V>
        where
            V: Deserialize<'de>,
        {
            type Value = LuksUuidMap<V>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object keyed by canonical LUKS UUID strings")
            }

            fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let mut out: BTreeMap<LuksUuid, V> = BTreeMap::new();
                while let Some(raw_key) = access.next_key::<String>()? {
                    let uuid = LuksUuid::parse(&raw_key).map_err(serde::de::Error::custom)?;
                    let value: V = access.next_value()?;
                    if out.contains_key(&uuid) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate LUKS UUID key after canonicalization: {uuid}"
                        )));
                    }
                    out.insert(uuid, value);
                }
                Ok(LuksUuidMap(out))
            }
        }

        de.deserialize_map(Visitor {
            _marker: std::marker::PhantomData,
        })
    }
}

// ---------------------------------------------------------------------------
// PoolMembership / DiskMember
// ---------------------------------------------------------------------------

/// Pool membership snapshot persisted to `pool.json`. The inner `disks`
/// map is private so every read/write goes through the typed helpers
/// below, which enforce the four-axis uniqueness invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolMembership {
    disks: LuksUuidMap<DiskMember>,
}

/// Per-disk membership entry. Identity is the UUID this struct is stored
/// under in `PoolMembership.disks` -- the value itself does NOT carry the
/// LUKS UUID, so journal and on-disk shapes have a single source of
/// truth. `deny_unknown_fields` rejects stale value-side fields
/// (e.g. a resurrected `luks_uuid`) at load time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskMember {
    pub name: DiskName,
    pub by_id: ByIdPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devid: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
}

impl PoolMembership {
    /// Empty membership. The only constructor besides Deserialize.
    pub fn empty() -> Self {
        PoolMembership {
            disks: LuksUuidMap::new(),
        }
    }

    /// Look up a member by UUID -- the primary identity axis.
    pub fn by_uuid(&self, uuid: &LuksUuid) -> Option<&DiskMember> {
        self.disks.get(uuid)
    }

    /// Mutable UUID lookup for in-process enrichment paths.
    pub fn by_uuid_mut(&mut self, uuid: &LuksUuid) -> Option<&mut DiskMember> {
        self.disks.get_mut(uuid)
    }

    /// Resolve a presentation `DiskName` back to its UUID-keyed entry.
    /// O(n) over the small pool cardinality; see the plan for why no
    /// secondary index is maintained.
    pub fn by_name(&self, name: &DiskName) -> Option<(&LuksUuid, &DiskMember)> {
        self.disks.iter().find(|(_, m)| m.name == *name)
    }

    /// Resolve a `/dev/disk/by-id/...` path to its UUID-keyed entry.
    pub fn by_by_id(&self, by_id: &ByIdPath) -> Option<(&LuksUuid, &DiskMember)> {
        self.disks.iter().find(|(_, m)| m.by_id == *by_id)
    }

    /// Resolve a btrfs `devid` to its UUID-keyed entry. Returns
    /// `Err(DuplicateDevid)` if corrupt membership contains the same
    /// devid twice -- the variant lists every colliding UUID so operator
    /// diagnostics name all of them rather than only two.
    pub fn by_devid(
        &self,
        devid: u64,
    ) -> Result<Option<(&LuksUuid, &DiskMember)>, MembershipError> {
        let mut matches: Vec<(&LuksUuid, &DiskMember)> = Vec::new();
        for (uuid, m) in self.disks.iter() {
            if m.devid == Some(devid) {
                matches.push((uuid, m));
            }
        }
        match matches.len() {
            0 => Ok(None),
            1 => Ok(Some(matches[0])),
            _ => Err(MembershipError::DuplicateDevid {
                devid,
                members: matches.iter().map(|(u, _)| (*u).clone()).collect(),
            }),
        }
    }

    /// Iterate `(UUID, &DiskMember)` pairs in UUID-sorted order.
    pub fn iter(&self) -> btree_map::Iter<'_, LuksUuid, DiskMember> {
        self.disks.iter()
    }

    /// Iterate disk names in UUID-sorted order (callers that need name
    /// order sort explicitly).
    pub fn names(&self) -> impl Iterator<Item = &DiskName> {
        self.disks.iter().map(|(_, m)| &m.name)
    }

    /// Number of members in the pool.
    pub fn len(&self) -> usize {
        self.disks.len()
    }

    /// True iff the pool has no members.
    pub fn is_empty(&self) -> bool {
        self.disks.is_empty()
    }

    /// Insert `member` under `uuid`. Enforces the four-axis uniqueness
    /// invariant (UUID, name, by-id, non-None devid) in pinned check
    /// order. The first failing check is the one returned; subsequent
    /// checks do not run.
    pub fn insert(&mut self, uuid: LuksUuid, member: DiskMember) -> Result<(), MembershipError> {
        // Axis 1: UUID.
        if self.disks.contains_key(&uuid) {
            return Err(MembershipError::Conflict(format!(
                "uuid '{uuid}' already in use under UUID {uuid}"
            )));
        }
        // Axis 2: disk-name.
        if let Some((other_uuid, _)) = self.by_name(&member.name) {
            return Err(MembershipError::Conflict(format!(
                "name '{name}' already in use under UUID {other_uuid} while inserting UUID {uuid}",
                name = member.name,
                other_uuid = other_uuid,
                uuid = uuid,
            )));
        }
        // Axis 3: by-id.
        if let Some((other_uuid, _)) = self.by_by_id(&member.by_id) {
            return Err(MembershipError::Conflict(format!(
                "by_id '{by_id}' already in use under UUID {other_uuid} while inserting UUID {uuid}",
                by_id = member.by_id,
                other_uuid = other_uuid,
                uuid = uuid,
            )));
        }
        // Axis 4: non-None devid.
        if let Some(devid) = member.devid {
            for (other_uuid, other_member) in self.disks.iter() {
                if other_member.devid == Some(devid) {
                    return Err(MembershipError::Conflict(format!(
                        "devid '{devid}' already in use under UUID {other_uuid} while inserting UUID {uuid}"
                    )));
                }
            }
        }
        // LuksUuidMap::insert is fail-closed on UUID; we already
        // axis-1-checked above so this can only fail in a logic-bug case.
        self.disks
            .insert(uuid.clone(), member)
            .map_err(|LuksUuidMapConflict { uuid }| {
                MembershipError::Conflict(format!("uuid '{uuid}' already in use under UUID {uuid}"))
            })?;
        Ok(())
    }

    /// Remove the member under `uuid`, if any.
    pub fn remove_by_uuid(&mut self, uuid: &LuksUuid) -> Option<DiskMember> {
        self.disks.remove(uuid)
    }
}

impl DiskMember {
    /// Minimal member -- used by discover and initial-add paths that have
    /// just the name and by-id, with enrichment to follow.
    pub fn new(name: DiskName, by_id: ByIdPath) -> Self {
        DiskMember {
            name,
            by_id,
            devid: None,
            added_at: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Load / save
// ---------------------------------------------------------------------------

/// Load authoritative pool membership from disk. Surface errors:
/// `Io` for file missing / EACCES, `Corrupt` for parse/uniqueness failures.
pub fn load_membership(paths: &StatePaths) -> Result<PoolMembership, MembershipError> {
    load_membership_from(&paths.pool_json())
}

/// Lower-level load helper used by tests against arbitrary tempfile paths
/// and by `load_membership` against the standard `StatePaths` location.
pub fn load_membership_from(path: &Path) -> Result<PoolMembership, MembershipError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            return Err(MembershipError::Io {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };

    let parsed: PoolMembership =
        serde_json::from_str(&raw).map_err(|e| MembershipError::Corrupt {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;

    // Secondary uniqueness sweep -- the wrapper Deserialize already enforced
    // UUID uniqueness on the outer keys, but value-side uniqueness (name,
    // by-id, devid) is the load-path's job.
    let mut seen_names: BTreeMap<&DiskName, &LuksUuid> = BTreeMap::new();
    let mut seen_byid: BTreeMap<&ByIdPath, &LuksUuid> = BTreeMap::new();
    let mut seen_devid: BTreeMap<u64, Vec<&LuksUuid>> = BTreeMap::new();
    for (uuid, m) in parsed.disks.iter() {
        if let Some(other) = seen_names.insert(&m.name, uuid) {
            let mut pair = [other, uuid];
            pair.sort();
            return Err(MembershipError::Conflict(format!(
                "name '{name}' already in use under UUID {first} while inserting UUID {second}",
                name = m.name,
                first = pair[0],
                second = pair[1],
            )));
        }
        if let Some(other) = seen_byid.insert(&m.by_id, uuid) {
            let mut pair = [other, uuid];
            pair.sort();
            return Err(MembershipError::Conflict(format!(
                "by_id '{by_id}' already in use under UUID {first} while inserting UUID {second}",
                by_id = m.by_id,
                first = pair[0],
                second = pair[1],
            )));
        }
        if let Some(devid) = m.devid {
            seen_devid.entry(devid).or_default().push(uuid);
        }
    }
    for (devid, uuids) in seen_devid {
        if uuids.len() >= 2 {
            let mut sorted: Vec<&LuksUuid> = uuids;
            sorted.sort();
            let first = sorted[0];
            let second = sorted[1];
            return Err(MembershipError::Conflict(format!(
                "devid '{devid}' already in use under UUID {first} while inserting UUID {second}"
            )));
        }
    }

    Ok(parsed)
}

/// Durably persist pool membership. Fails hard on any I/O error.
pub fn save_membership(m: &PoolMembership, paths: &StatePaths) -> Result<(), MembershipError> {
    save_membership_to(m, &paths.pool_json())
}

/// Lower-level save helper used by tests against arbitrary tempfile paths
/// and by `save_membership` against the standard `StatePaths` location.
pub fn save_membership_to(m: &PoolMembership, path: &Path) -> Result<(), MembershipError> {
    let json = serde_json::to_string_pretty(m).map_err(|e| MembershipError::Save {
        path: path.to_path_buf(),
        source: std::io::Error::other(e.to_string()),
    })?;
    state_io::atomic_write(path, json.as_bytes()).map_err(|e| MembershipError::Save {
        path: path.to_path_buf(),
        source: e,
    })
}

// ---------------------------------------------------------------------------
// parse_disk_spec
// ---------------------------------------------------------------------------

/// Parse a `NAME=/dev/disk/by-id/...` disk spec from CLI arguments,
/// routing each side through the validating value-type constructors so
/// every CLI entry point produces the same shape.
pub fn parse_disk_spec(spec: &str) -> Result<(DiskName, ByIdPath), DiskSpecParseError> {
    let (name_raw, by_id_raw) = spec
        .split_once('=')
        .ok_or_else(|| DiskSpecParseError::Shape {
            spec: spec.to_owned(),
        })?;
    let name = DiskName::parse(name_raw).map_err(|e| DiskSpecParseError::Name(e.to_string()))?;
    let by_id =
        ByIdPath::parse(by_id_raw).map_err(|e| DiskSpecParseError::ByIdPath(e.to_string()))?;
    Ok((name, by_id))
}

/// Error returned by `parse_disk_spec`. Distinct from `MembershipError`
/// because spec parsing is a pre-validation step at the CLI boundary --
/// the membership shape has not yet been touched.
#[derive(Debug, Error)]
pub enum DiskSpecParseError {
    #[error("expected NAME=/dev/disk/by-id/..., got '{spec}'")]
    Shape { spec: String },
    #[error("{0}")]
    Name(String),
    #[error("{0}")]
    ByIdPath(String),
}

// ---------------------------------------------------------------------------
// enrich_from_pool_state -- name-based; retained as-is until Phase 3/4 migrate
// the callers. The migration removes name_from_mapper-driven correlation.
// ---------------------------------------------------------------------------

/// Populate membership entries with `devid` and `added_at` drawn from a
/// freshly probed `PoolState`. After Phase 3 this resolves by UUID; here
/// it is left in a placeholder shape so the production callers compile
/// against `PoolMembership::by_uuid_mut` once they migrate.
pub(crate) fn enrich_from_pool_state(_pool: &PoolState, _membership: &mut PoolMembership) {
    // Phase 3 migrates this to UUID-keyed correlation. The current Phase 1
    // body is intentionally a no-op so the symbol stays callable from the
    // sole production caller (`refresh_pool_metadata`) while name-based
    // correlation is removed from this module.
}

/// Enrich `pool.json` with metadata from the live pool state.
/// Best-effort: logs a warning on failure, never fails the caller.
pub fn refresh_pool_metadata(pool: &PoolState, paths: &StatePaths) {
    let mut membership = match load_membership(paths) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Warning: failed to load membership for metadata refresh: {e}");
            return;
        }
    };
    enrich_from_pool_state(pool, &mut membership);
    if let Err(e) = save_membership(&membership, paths) {
        eprintln!("Warning: failed to save enriched membership: {e}");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_paths::StatePaths;

    // Test-module seed allocation: cli/src/membership.rs uses 100-199.
    fn test_uuid(seed: u64) -> LuksUuid {
        LuksUuid::parse(&format!("00000000-0000-0000-0000-{:012x}", seed))
            .expect("hand-padded UUID is canonical")
    }

    fn disk_name(s: &str) -> DiskName {
        DiskName::parse(s).expect("valid disk name in fixture")
    }

    fn by_id(s: &str) -> ByIdPath {
        ByIdPath::parse(s).expect("valid by-id path in fixture")
    }

    fn member(name: &str, by_id_s: &str) -> DiskMember {
        DiskMember::new(disk_name(name), by_id(by_id_s))
    }

    // ----- LuksUuidMap shape regressions -----------------------------------

    #[test]
    fn luks_uuid_map_serializes_as_flat_object() {
        // Intent: LuksUuidMap serializes as a JSON object keyed by canonical
        //   UUID strings, not as a positional tuple-like structure.
        // Why: accidental removal of `#[serde(transparent)]` would silently
        //   break every pool.json and pending-op.json fixture.
        let mut map: LuksUuidMap<u32> = LuksUuidMap::new();
        let u = test_uuid(100);
        map.insert(u.clone(), 42).unwrap();
        let s = serde_json::to_string(&map).unwrap();
        assert_eq!(
            s,
            format!("{{\"{}\":42}}", u.as_str()),
            "LuksUuidMap serialized as {s}"
        );
        let back: LuksUuidMap<u32> = serde_json::from_str(&s).unwrap();
        assert_eq!(back.get(&u), Some(&42));
    }

    #[test]
    fn luks_uuid_map_insert_fail_closed() {
        // Intent: a second insert under the same UUID returns
        //   LuksUuidMapConflict and leaves the original value intact.
        // Why: the in-process insert path must agree with the
        //   Deserialize duplicate-key contract.
        let mut map: LuksUuidMap<&'static str> = LuksUuidMap::new();
        let u = test_uuid(101);
        map.insert(u.clone(), "first").unwrap();
        let err = map.insert(u.clone(), "second").unwrap_err();
        assert_eq!(err.uuid, u);
        assert_eq!(map.get(&u), Some(&"first"));
    }

    #[test]
    fn luks_uuid_map_deserialize_rejects_duplicate_canonical_keys() {
        // Intent: an uppercase key and the corresponding lowercase key in
        //   the same JSON object fail with the pinned substring.
        // Why: the canonicalizing deserialize must not silently keep the
        //   last value (default BTreeMap behavior).
        let s = "{\"AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA\":1,\"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\":2}";
        let err = serde_json::from_str::<LuksUuidMap<u32>>(s).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(
                "duplicate LUKS UUID key after canonicalization: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
            ),
            "got: {msg}"
        );
    }

    #[test]
    fn luks_uuid_map_deserialize_canonicalizes_uppercase_key() {
        // Intent: an uppercase key in JSON deserializes equal to the
        //   lowercase key form.
        let s = "{\"AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA\":1}";
        let map: LuksUuidMap<u32> = serde_json::from_str(s).unwrap();
        let u = LuksUuid::parse("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
        assert_eq!(map.get(&u), Some(&1));
    }

    #[test]
    fn luks_uuid_map_deserialize_rejects_invalid_key() {
        // Intent: a non-UUID key fails Deserialize.
        let err = serde_json::from_str::<LuksUuidMap<u32>>("{\"not-a-uuid\":1}").unwrap_err();
        assert!(err.to_string().contains("invalid LUKS UUID"));
    }

    // ----- PoolMembership::insert four-axis conflicts ----------------------

    #[test]
    fn insert_rejects_duplicate_uuid() {
        // Intent: re-inserting under the same UUID returns
        //   MembershipError::Conflict with the pinned wording.
        let mut m = PoolMembership::empty();
        let u = test_uuid(110);
        m.insert(u.clone(), member("d1", "/dev/disk/by-id/ata-X1"))
            .unwrap();
        let err = m
            .insert(u.clone(), member("d2", "/dev/disk/by-id/ata-X2"))
            .unwrap_err();
        let s = err.to_string();
        assert!(
            s.contains(&format!("uuid '{u}' already in use under UUID {u}")),
            "got: {s}"
        );
    }

    #[test]
    fn insert_rejects_duplicate_name() {
        // Intent: two UUIDs sharing the same DiskName fail with the
        //   pinned name-axis wording.
        let mut m = PoolMembership::empty();
        let u1 = test_uuid(111);
        let u2 = test_uuid(112);
        m.insert(u1.clone(), member("d1", "/dev/disk/by-id/ata-X1"))
            .unwrap();
        let err = m
            .insert(u2.clone(), member("d1", "/dev/disk/by-id/ata-X2"))
            .unwrap_err();
        let s = err.to_string();
        assert!(
            s.contains(&format!(
                "name 'd1' already in use under UUID {u1} while inserting UUID {u2}"
            )),
            "got: {s}"
        );
    }

    #[test]
    fn insert_rejects_duplicate_by_id() {
        // Intent: two UUIDs sharing the same ByIdPath fail with the
        //   pinned by-id-axis wording.
        let mut m = PoolMembership::empty();
        let u1 = test_uuid(113);
        let u2 = test_uuid(114);
        m.insert(u1.clone(), member("d1", "/dev/disk/by-id/ata-X1"))
            .unwrap();
        let err = m
            .insert(u2.clone(), member("d2", "/dev/disk/by-id/ata-X1"))
            .unwrap_err();
        let s = err.to_string();
        assert!(
            s.contains(&format!(
                "by_id '/dev/disk/by-id/ata-X1' already in use under UUID {u1} while inserting UUID {u2}"
            )),
            "got: {s}"
        );
    }

    #[test]
    fn insert_rejects_duplicate_devid() {
        // Intent: two UUIDs sharing the same non-None devid fail with
        //   the pinned devid-axis wording.
        let mut m = PoolMembership::empty();
        let u1 = test_uuid(115);
        let u2 = test_uuid(116);
        let mut d1 = member("d1", "/dev/disk/by-id/ata-X1");
        d1.devid = Some(7);
        let mut d2 = member("d2", "/dev/disk/by-id/ata-X2");
        d2.devid = Some(7);
        m.insert(u1.clone(), d1).unwrap();
        let err = m.insert(u2.clone(), d2).unwrap_err();
        let s = err.to_string();
        assert!(
            s.contains(&format!(
                "devid '7' already in use under UUID {u1} while inserting UUID {u2}"
            )),
            "got: {s}"
        );
    }

    #[test]
    fn insert_check_order_uuid_first() {
        // Intent: when both UUID and name collide, the UUID-axis error
        //   fires first (pinned check order).
        let mut m = PoolMembership::empty();
        let u = test_uuid(117);
        m.insert(u.clone(), member("d1", "/dev/disk/by-id/ata-X1"))
            .unwrap();
        // Re-insert under same UUID and same name -- UUID axis should win.
        let err = m
            .insert(u.clone(), member("d1", "/dev/disk/by-id/ata-X2"))
            .unwrap_err();
        let s = err.to_string();
        assert!(s.contains("uuid '"), "expected UUID-axis first, got: {s}");
        assert!(
            !s.contains("name '"),
            "name-axis should not fire first, got: {s}"
        );
    }

    // ----- by_devid corruption ---------------------------------------------

    #[test]
    fn by_devid_returns_duplicate_devid_on_corruption() {
        // Intent: by_devid against a manually-corrupted membership returns
        //   DuplicateDevid with all colliding UUIDs in canonical lex order.
        let mut disks: BTreeMap<LuksUuid, DiskMember> = BTreeMap::new();
        let u1 = test_uuid(120);
        let u2 = test_uuid(121);
        let u3 = test_uuid(122);
        for (u, name, by_id_s) in [
            (&u1, "d1", "/dev/disk/by-id/ata-X1"),
            (&u2, "d2", "/dev/disk/by-id/ata-X2"),
            (&u3, "d3", "/dev/disk/by-id/ata-X3"),
        ] {
            let mut dm = member(name, by_id_s);
            dm.devid = Some(7);
            disks.insert(u.clone(), dm);
        }
        let m = PoolMembership {
            disks: LuksUuidMap(disks),
        };
        let err = m.by_devid(7).unwrap_err();
        match &err {
            MembershipError::DuplicateDevid { devid, members } => {
                assert_eq!(*devid, 7);
                // canonical-lex order
                let mut want = vec![u1.clone(), u2.clone(), u3.clone()];
                want.sort();
                assert_eq!(members, &want);
            }
            other => panic!("expected DuplicateDevid, got: {other:?}"),
        }
        let s = err.to_string();
        // The displayed UUID list is canonical-lex.
        assert!(
            s.contains(&format!(
                "duplicate devid 7 in pool membership across UUIDs {u1}, {u2}, {u3}"
            )),
            "got: {s}"
        );
    }

    // ----- Load-time enforcement -------------------------------------------

    #[test]
    fn load_membership_rejects_old_name_keyed_format() {
        // Intent: a pool.json with disk-name top-level keys fails to load
        //   with the pinned Corrupt remediation suffix.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("pool.json");
        std::fs::write(
            &path,
            "{\"disks\":{\"toshiba\":{\"name\":\"toshiba\",\"by_id\":\"/dev/disk/by-id/ata-X\"}}}",
        )
        .unwrap();
        let err = load_membership_from(&path).unwrap_err();
        match err {
            MembershipError::Corrupt { .. } => {}
            other => panic!("expected Corrupt, got: {other:?}"),
        }
        let s = err.to_string();
        assert!(
            s.contains(
                "-- run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/luks-unlock.md)"
            ),
            "got: {s}"
        );
    }

    #[test]
    fn load_membership_rejects_stale_value_side_luks_uuid() {
        // Intent: an entry with a UUID key but a value-side `luks_uuid`
        //   field fails through DiskMember's deny_unknown_fields, not
        //   the outer-key path.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("pool.json");
        let body = format!(
            "{{\"disks\":{{\"{u}\":{{\"name\":\"toshiba1\",\"by_id\":\"/dev/disk/by-id/ata-X\",\"luks_uuid\":\"{u}\",\"devid\":1}}}}}}",
            u = test_uuid(130)
        );
        std::fs::write(&path, body).unwrap();
        let err = load_membership_from(&path).unwrap_err();
        let s = err.to_string();
        assert!(matches!(err, MembershipError::Corrupt { .. }));
        assert!(
            s.contains("luks_uuid"),
            "expected error to name the unknown field, got: {s}"
        );
    }

    #[test]
    fn load_membership_rejects_unknown_top_level_key() {
        // Intent: an unknown top-level key in pool.json fails through
        //   PoolMembership's deny_unknown_fields.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("pool.json");
        std::fs::write(&path, "{\"disks\":{}, \"schema_version\":1}").unwrap();
        let err = load_membership_from(&path).unwrap_err();
        match err {
            MembershipError::Corrupt { .. } => {}
            other => panic!("expected Corrupt, got: {other:?}"),
        }
    }

    #[test]
    fn load_membership_rejects_hybrid_uuid_and_name_keys() {
        // Intent: pool.json with one valid UUID key and one disk-name key
        //   fails -- LuksUuidMap's deserialize must reject the non-UUID
        //   key rather than partially load.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("pool.json");
        let body = format!(
            "{{\"disks\":{{\"{u}\":{{\"name\":\"a\",\"by_id\":\"/dev/disk/by-id/ata-A\"}},\"toshiba\":{{\"name\":\"toshiba\",\"by_id\":\"/dev/disk/by-id/ata-T\"}}}}}}",
            u = test_uuid(131)
        );
        std::fs::write(&path, body).unwrap();
        let err = load_membership_from(&path).unwrap_err();
        assert!(matches!(err, MembershipError::Corrupt { .. }));
    }

    #[test]
    fn load_membership_rejects_duplicate_value_side_name() {
        // Intent: two UUID-keyed entries with the same `name` value fail
        //   at load with the pinned name-axis Conflict wording.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("pool.json");
        let u1 = test_uuid(132);
        let u2 = test_uuid(133);
        // u1 < u2 lex; the load-time sweep is insertion-order over the
        // sorted map so u1 is "first" and u2 is "second".
        let body = format!(
            "{{\"disks\":{{\"{u1}\":{{\"name\":\"dup\",\"by_id\":\"/dev/disk/by-id/ata-A\"}},\"{u2}\":{{\"name\":\"dup\",\"by_id\":\"/dev/disk/by-id/ata-B\"}}}}}}",
            u1 = u1,
            u2 = u2
        );
        std::fs::write(&path, body).unwrap();
        let err = load_membership_from(&path).unwrap_err();
        let s = err.to_string();
        assert!(matches!(err, MembershipError::Conflict(_)));
        assert!(
            s.contains(&format!(
                "name 'dup' already in use under UUID {u1} while inserting UUID {u2}"
            )),
            "got: {s}"
        );
    }

    #[test]
    fn load_membership_rejects_duplicate_value_side_by_id() {
        // Intent: two UUID-keyed entries with the same `by_id` fail at
        //   load with the pinned by-id-axis Conflict wording.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("pool.json");
        let u1 = test_uuid(134);
        let u2 = test_uuid(135);
        let body = format!(
            "{{\"disks\":{{\"{u1}\":{{\"name\":\"a\",\"by_id\":\"/dev/disk/by-id/ata-SAME\"}},\"{u2}\":{{\"name\":\"b\",\"by_id\":\"/dev/disk/by-id/ata-SAME\"}}}}}}",
            u1 = u1,
            u2 = u2
        );
        std::fs::write(&path, body).unwrap();
        let err = load_membership_from(&path).unwrap_err();
        let s = err.to_string();
        assert!(matches!(err, MembershipError::Conflict(_)));
        assert!(
            s.contains(&format!(
                "by_id '/dev/disk/by-id/ata-SAME' already in use under UUID {u1} while inserting UUID {u2}"
            )),
            "got: {s}"
        );
    }

    #[test]
    fn load_membership_rejects_duplicate_value_side_devid() {
        // Intent: two UUID-keyed entries with the same non-None devid
        //   fail at load with the pinned devid-axis Conflict wording.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("pool.json");
        let u1 = test_uuid(136);
        let u2 = test_uuid(137);
        let body = format!(
            "{{\"disks\":{{\"{u1}\":{{\"name\":\"a\",\"by_id\":\"/dev/disk/by-id/ata-A\",\"devid\":5}},\"{u2}\":{{\"name\":\"b\",\"by_id\":\"/dev/disk/by-id/ata-B\",\"devid\":5}}}}}}",
            u1 = u1,
            u2 = u2
        );
        std::fs::write(&path, body).unwrap();
        let err = load_membership_from(&path).unwrap_err();
        let s = err.to_string();
        assert!(matches!(err, MembershipError::Conflict(_)));
        assert!(
            s.contains(&format!(
                "devid '5' already in use under UUID {u1} while inserting UUID {u2}"
            )),
            "got: {s}"
        );
    }

    // ----- Multi-disk round trip -------------------------------------------

    #[test]
    fn multi_disk_round_trip_stable_uuid_order() {
        // Intent: a >=3-member pool.json round-trips through serialize +
        //   atomic_write + load_membership with UUID-sorted key order
        //   independent of insertion order.
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let mut m = PoolMembership::empty();
        // Insert in REVERSE UUID order.
        let mut uuids = vec![test_uuid(140), test_uuid(141), test_uuid(142)];
        uuids.sort();
        for (i, u) in uuids.iter().rev().enumerate() {
            m.insert(
                u.clone(),
                member(&format!("d{i}"), &format!("/dev/disk/by-id/ata-MULTI-{i}")),
            )
            .unwrap();
        }
        save_membership(&m, &paths).unwrap();
        let loaded = load_membership(&paths).unwrap();
        // Iteration order is UUID-sorted regardless of insertion order.
        let observed: Vec<&LuksUuid> = loaded.iter().map(|(u, _)| u).collect();
        let expected: Vec<&LuksUuid> = uuids.iter().collect();
        assert_eq!(observed, expected);
    }

    // ----- parse_disk_spec -------------------------------------------------

    #[test]
    fn parse_disk_spec_valid() {
        let (name, by_id_v) = parse_disk_spec("toshiba=/dev/disk/by-id/ata-TOSHIBA").unwrap();
        assert_eq!(name.as_str(), "toshiba");
        assert_eq!(by_id_v.as_str(), "/dev/disk/by-id/ata-TOSHIBA");
    }

    #[test]
    fn parse_disk_spec_shape_error() {
        let err = parse_disk_spec("toshiba").unwrap_err();
        assert!(matches!(err, DiskSpecParseError::Shape { .. }));
    }

    #[test]
    fn parse_disk_spec_bad_by_id() {
        let err = parse_disk_spec("toshiba=/dev/sda").unwrap_err();
        assert!(matches!(err, DiskSpecParseError::ByIdPath(_)));
    }

    #[test]
    fn parse_disk_spec_bad_name() {
        let err = parse_disk_spec("1bad=/dev/disk/by-id/ata-X").unwrap_err();
        assert!(matches!(err, DiskSpecParseError::Name(_)));
    }
}
