use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use thiserror::Error;

// ---------------------------------------------------------------------------
// LuksUuid -- canonical, validated LUKS UUID identity
// ---------------------------------------------------------------------------

/// Persistent LUKS UUID identity. Inner string is canonicalized to
/// lowercase hyphenated form via `LuksUuid::parse`. The type is the
/// migration's single source of truth for "which physical LUKS volume"
/// across `pool.json`, `pending-op.json`, planner code, and live probes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LuksUuid(String);

/// Error returned when constructing a `LuksUuid` from text that is not a
/// recognized UUID form. Routed through `LuksUuid::parse` so call sites
/// surface the offending input verbatim.
#[derive(Debug, Error)]
#[error("invalid LUKS UUID '{raw}': {detail}")]
pub struct LuksUuidParseError {
    pub raw: String,
    pub detail: String,
}

impl LuksUuid {
    /// Parse and canonicalize any UUID form accepted by `uuid::Uuid` to
    /// lowercase hyphenated text. This is the only validating constructor;
    /// production and tests share it so canonicalization never depends on
    /// where the bytes came from.
    pub fn parse(raw: &str) -> Result<Self, LuksUuidParseError> {
        match uuid::Uuid::parse_str(raw) {
            Ok(u) => Ok(LuksUuid(u.hyphenated().to_string())),
            Err(e) => Err(LuksUuidParseError {
                raw: raw.to_owned(),
                detail: e.to_string(),
            }),
        }
    }

    /// Generate a fresh v4 UUID at the point a LUKS format is planned, so
    /// `OpKind::Add` and `OpKind::Replace` journals can record authoritative
    /// identity before `cryptsetup luksFormat` runs.
    pub fn new_v4() -> Self {
        LuksUuid(uuid::Uuid::new_v4().hyphenated().to_string())
    }

    /// Observation accessor for argv rendering, log lines, and any non-
    /// Display formatter site that needs the raw canonical form.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LuksUuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Render UUIDs in canonical lexicographic order for stable diagnostics.
pub(crate) fn format_uuid_list(uuids: &[LuksUuid]) -> String {
    let mut sorted: Vec<&LuksUuid> = uuids.iter().collect();
    sorted.sort();
    sorted
        .into_iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

impl Serialize for LuksUuid {
    fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ser.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for LuksUuid {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(de)?;
        LuksUuid::parse(&s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// DiskName -- presentation identity, validated to the braid disk-name contract
// ---------------------------------------------------------------------------

/// Operator-facing disk identifier used as the mapper-name and LUKS-label
/// suffix (`braid-<DiskName>`). Not a persistent identity -- `LuksUuid` is
/// -- but every label/mapper construction site goes through this type so
/// the disk-name character contract is enforced once.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DiskName(String);

/// Error returned when constructing a `DiskName` from text that does not
/// satisfy the disk-name contract (leading ASCII letter, ASCII alnum / `-` /
/// `_`, max 32 chars).
#[derive(Debug, Error)]
#[error(
    "invalid disk name '{raw}': must start with a letter, contain only letters, digits, hyphens, or underscores, and be at most 32 characters"
)]
pub struct DiskNameParseError {
    pub raw: String,
}

impl DiskName {
    /// Parse and validate a disk name. The contract is fixed by
    /// `braid-<DiskName>` mapper/label conventions and the cryptsetup label
    /// length limit, and is enforced at every boundary that accepts user
    /// or probe input.
    pub fn parse(raw: &str) -> Result<Self, DiskNameParseError> {
        if !is_valid_disk_name(raw) {
            return Err(DiskNameParseError {
                raw: raw.to_owned(),
            });
        }
        Ok(DiskName(raw.to_owned()))
    }

    /// Observation accessor for argv rendering, log lines, and label
    /// construction sites that interpolate the raw name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_valid_disk_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 32 {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl fmt::Display for DiskName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for DiskName {
    fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ser.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DiskName {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(de)?;
        DiskName::parse(&s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// ByIdPath -- hardware addressing, validated to /dev/disk/by-id/ prefix
// ---------------------------------------------------------------------------

/// Stable hardware-addressing path used to open or format a physical
/// device. Validated at construction so probe and CLI surfaces cannot
/// inject `/dev/sdX`-style paths that would lose their binding across
/// reboots.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ByIdPath(String);

/// Error returned when constructing a `ByIdPath` from a path that does not
/// start with `/dev/disk/by-id/`.
#[derive(Debug, Error)]
#[error("invalid by_id path '{raw}': must start with /dev/disk/by-id/")]
pub struct ByIdPathParseError {
    pub raw: String,
}

impl ByIdPath {
    /// Parse and validate a `/dev/disk/by-id/...` path. The validation
    /// keeps probe data and CLI input shaped identically at the type level
    /// so downstream code does not need to recheck the prefix.
    pub fn parse(raw: &str) -> Result<Self, ByIdPathParseError> {
        if !raw.starts_with("/dev/disk/by-id/") {
            return Err(ByIdPathParseError {
                raw: raw.to_owned(),
            });
        }
        Ok(ByIdPath(raw.to_owned()))
    }

    /// Observation accessor for argv rendering and any site that needs
    /// the raw path text (e.g. cryptsetup invocations).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ByIdPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for ByIdPath {
    fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ser.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ByIdPath {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(de)?;
        ByIdPath::parse(&s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// LuksFormatExtraOpts -- validated argv extras for `cryptsetup luksFormat`
// ---------------------------------------------------------------------------

/// Validated wrapper around user-supplied `cryptsetup luksFormat` extras.
/// The constructor rejects tokens that target braid-managed cryptsetup
/// options (`--uuid`, `--label`) so user input cannot shadow the
/// journaled identity or the braid label.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LuksFormatExtraOpts(Vec<String>);

/// Error returned when `LuksFormatExtraOpts::parse` rejects a token that
/// targets a braid-managed cryptsetup option.
#[derive(Debug, Error)]
pub enum LuksFormatExtraOptsError {
    #[error(
        "--luks-format-arg '{token}' targets a braid-managed cryptsetup option (--uuid, --label); braid sets these itself and rejects user-supplied overrides"
    )]
    ManagedFormatFlag { token: String },
}

impl LuksFormatExtraOpts {
    /// Validate the supplied argv extras. Empty input is valid. Each token
    /// is checked against the braid-managed reject list (`--uuid`,
    /// `--uuid=...`, `--label`, `--label=...`) so user-supplied overrides
    /// for managed flags fail before any `CryptsetupLuksFormat` request
    /// reaches the executor.
    pub fn parse(extras: &[String]) -> Result<Self, LuksFormatExtraOptsError> {
        for token in extras {
            if is_managed_format_flag(token) {
                return Err(LuksFormatExtraOptsError::ManagedFormatFlag {
                    token: token.clone(),
                });
            }
        }
        Ok(LuksFormatExtraOpts(extras.to_vec()))
    }

    /// Borrow the validated extras as a slice for argv assembly. No
    /// mutable accessor: the validation invariant must not be bypassed
    /// after construction.
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

fn is_managed_format_flag(token: &str) -> bool {
    // Long-form only. Per cryptsetup 2.8.4 audit (OPT_UUID at
    // reference/cryptsetup/src/cryptsetup_arg_list.h:217 and OPT_LABEL at
    // line 109, both with popt short name '\0'), no short alias exists
    // for the managed flags on the pinned upstream.
    token == "--uuid"
        || token == "--label"
        || token.starts_with("--uuid=")
        || token.starts_with("--label=")
}

// ---------------------------------------------------------------------------
// MapperName / MountPoint
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MapperName(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MountPoint(pub String);

impl MountPoint {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MapperName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for MountPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// ---------------------------------------------------------------------------
// Planner input types (produced by probe, consumed by commands)
// ---------------------------------------------------------------------------

/// What we know about the live btrfs pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolState {
    pub mounted: bool,
    pub devices: Vec<PoolDevice>,
    pub missing_count: u64,
    pub total_devices: u64,
    /// btrfs filesystem FSID (uuid), populated when pool is mounted.
    pub fsid: Option<String>,
    /// Devids of missing devices (from btrfs filesystem show MISSING sentinels).
    ///
    /// Authoritative to btrfs — does NOT include null-underlying devices.
    /// `remove-missing` uses this to resolve destructive removal targets, so
    /// only devices that btrfs has confirmed as MISSING belong here.
    pub missing_devids: Vec<u64>,
    /// Devices whose LUKS mapper is open but underlying block device is gone
    /// (hot-unplugged). Kept separate from `missing_devids` because
    /// `missing_devids` is used by `remove-missing` to pick destructive
    /// removal targets -- a transient hot-unplug must not look removable.
    ///
    /// Monitor and ack compute an alert-local union (`missing_devids ∪
    /// null_underlying devids`) to fire `MissingDevice` alerts for both cases.
    /// btrfs device stats keeps reporting these devices' mapper paths along
    /// with their devids, so the alert pipeline pairs rows by devid directly
    /// from the parsed stats output -- no path-to-devid map required.
    pub null_underlying: Vec<NullUnderlyingDevice>,
}

impl PoolState {
    /// Devids that must fire `MissingDevice` alert causes: the btrfs-
    /// authoritative MISSING set unioned with null-underlying devids,
    /// deduplicated and sorted. Dedup matters when btrfs has promoted a
    /// hot-unplugged device to MISSING while its LUKS mapper still reports
    /// `(null)` -- without it, the same devid would produce two
    /// `MissingDevice` causes.
    pub fn alert_missing_devids(&self) -> Vec<u64> {
        self.missing_devids
            .iter()
            .copied()
            .chain(self.null_underlying.iter().map(|d| d.devid))
            .collect::<BTreeSet<u64>>()
            .into_iter()
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolDevice {
    pub mapper: MapperName,
    pub luks_uuid: LuksUuid,
    pub devid: u64,
    pub underlying: String,
}

/// A pool device whose LUKS mapper is open but the underlying block device
/// is gone (hot-unplugged). These are effectively missing for alerting but
/// not yet confirmed by btrfs as MISSING.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NullUnderlyingDevice {
    pub mapper: MapperName,
    pub devid: u64,
}

/// Pre-probed state of each config disk (produced by probe, consumed by commands).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDisk {
    pub name: String,
    pub by_id_path: ByIdPath,
    pub state: ConfigDiskState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigDiskState {
    /// Device file doesn't exist (unplugged / absent).
    Absent,
    /// Device exists but is not LUKS-formatted.
    PresentNotLuks,
    /// Device exists, has LUKS header, UUID known.
    /// `label` is the optional LUKS2 label captured from the same luksDump
    /// probe that verifies braid's LUKS2 invariant.
    /// `mapper_open` = true if /dev/mapper/<name> is already active (crash recovery skip).
    PresentLuks {
        uuid: LuksUuid,
        label: Option<String>,
        mapper_open: bool,
    },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- LuksUuid -----------------------------------------------------------

    #[test]
    fn luks_uuid_parse_canonicalizes_uppercase() {
        // Intent: uppercase hyphenated UUID canonicalizes to lowercase.
        // Why: canonicalization gates duplicate-key collapse and equality
        //   with cryptsetup's lowercase output across the codebase.
        // Scenario: an operator hand-edits pool.json with uppercase keys;
        //   the loaded membership equates with the probed lowercase UUID.
        let u = LuksUuid::parse("8C78A966-EF17-4610-B835-5B376EF10B4E").unwrap();
        assert_eq!(u.as_str(), "8c78a966-ef17-4610-b835-5b376ef10b4e");
    }

    #[test]
    fn luks_uuid_parse_canonicalizes_simple_form() {
        // Intent: 32-hex simple form parses to the canonical hyphenated form.
        // Why: cryptsetup accepts both simple and hyphenated forms; the
        //   canonicalizer is the single source of truth for equality.
        // Scenario: a journal value field carries the simple form; on
        //   load it equates with the hyphenated form stored elsewhere.
        let u = LuksUuid::parse("8c78a966ef174610b8355b376ef10b4e").unwrap();
        assert_eq!(u.as_str(), "8c78a966-ef17-4610-b835-5b376ef10b4e");
    }

    #[test]
    fn luks_uuid_parse_canonicalizes_urn() {
        // Intent: URN form (`urn:uuid:...`) parses to the canonical
        //   hyphenated form.
        // Why: uuid::Uuid::parse_str accepts URN; the canonicalizer must
        //   not silently reject a valid alternative form.
        let u = LuksUuid::parse("urn:uuid:8c78a966-ef17-4610-b835-5b376ef10b4e").unwrap();
        assert_eq!(u.as_str(), "8c78a966-ef17-4610-b835-5b376ef10b4e");
    }

    #[test]
    fn luks_uuid_parse_rejects_invalid() {
        // Intent: non-UUID text fails parse with the offending raw input.
        // Why: invalid identity must surface as a parse error so the
        //   deserialize path can route it into MembershipError::Corrupt.
        let err = LuksUuid::parse("not-a-uuid").unwrap_err();
        assert_eq!(err.raw, "not-a-uuid");
    }

    #[test]
    fn luks_uuid_serialize_round_trip() {
        // Intent: serialize emits canonical form; deserialize re-parses
        //   through the canonicalizer.
        // Why: round-trip stability is load-bearing for atomic_write +
        //   load_membership and pending-op.json replay.
        let u = LuksUuid::parse("8C78A966-EF17-4610-B835-5B376EF10B4E").unwrap();
        let s = serde_json::to_string(&u).unwrap();
        assert_eq!(s, "\"8c78a966-ef17-4610-b835-5b376ef10b4e\"");
        let back: LuksUuid = serde_json::from_str(&s).unwrap();
        assert_eq!(back, u);
    }

    #[test]
    fn luks_uuid_deserialize_canonicalizes_uppercase() {
        // Intent: a JSON-source uppercase UUID deserializes equal to its
        //   lowercase form.
        // Why: the canonicalization invariant must hold for every entry
        //   point, not only for `LuksUuid::parse` directly.
        let upper: LuksUuid =
            serde_json::from_str("\"8C78A966-EF17-4610-B835-5B376EF10B4E\"").unwrap();
        let lower: LuksUuid =
            serde_json::from_str("\"8c78a966-ef17-4610-b835-5b376ef10b4e\"").unwrap();
        assert_eq!(upper, lower);
    }

    // -- DiskName -----------------------------------------------------------

    #[test]
    fn disk_name_parse_valid() {
        // Intent: the disk-name contract accepts braid-supported names.
        // Why: discover, parse_disk_spec, and CLI surfaces all route
        //   through this constructor; reject-by-default regressions would
        //   silently lock operators out of legitimate names.
        for name in ["toshiba", "disk1", "my-disk", "my_disk", "A", "Z1-b2-c3"] {
            assert!(DiskName::parse(name).is_ok(), "'{name}' should be valid");
        }
    }

    #[test]
    fn disk_name_parse_rejects_invalid() {
        // Intent: leading non-letter, embedded space, oversized, empty,
        //   and non-ASCII names are rejected.
        // Why: the rejection contract gates discover's InvalidDiskName
        //   warning and label-suffix construction across the codebase.
        for name in ["1bad", "-bad", "_bad", "my disk", "", &"a".repeat(33)] {
            assert!(DiskName::parse(name).is_err(), "'{name}' should be invalid");
        }
    }

    #[test]
    fn disk_name_deserialize_rejects_invalid() {
        // Intent: a JSON-source invalid disk name fails to deserialize.
        // Why: deny_unknown_fields catches stale value-side keys but the
        //   shape-check on a known field requires Deserialize to route
        //   through DiskName::parse.
        let r: Result<DiskName, _> = serde_json::from_str("\"1bad\"");
        assert!(r.is_err());
    }

    // -- ByIdPath -----------------------------------------------------------

    #[test]
    fn by_id_path_parse_requires_prefix() {
        // Intent: any path not under `/dev/disk/by-id/` is rejected.
        // Why: braid's reboot-stable addressing depends on this prefix;
        //   accepting `/dev/sda1` here would silently let pool.json
        //   record an unstable handle.
        assert!(ByIdPath::parse("/dev/disk/by-id/ata-OK").is_ok());
        assert!(ByIdPath::parse("/dev/sda1").is_err());
        assert!(ByIdPath::parse("").is_err());
    }

    #[test]
    fn by_id_path_deserialize_rejects_invalid() {
        // Intent: JSON-source ByIdPath validates through the same gate.
        let r: Result<ByIdPath, _> = serde_json::from_str("\"/dev/sda1\"");
        assert!(r.is_err());
    }

    // -- LuksFormatExtraOpts ------------------------------------------------

    #[test]
    fn luks_format_extra_opts_empty_succeeds() {
        // Intent: empty input parses to an empty value.
        // Why: most add/replace invocations do not pass any --luks-format-arg;
        //   the parser must not require non-empty input.
        let opts = LuksFormatExtraOpts::parse(&[]).unwrap();
        assert!(opts.as_slice().is_empty());
    }

    #[test]
    fn luks_format_extra_opts_rejects_uuid_equals() {
        // Intent: `--uuid=<value>` is rejected with the pinned wording.
        // Why: user-supplied identity must not override the journaled UUID.
        let err = LuksFormatExtraOpts::parse(&["--uuid=foo".to_owned()]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(
                "--luks-format-arg '--uuid=foo' targets a braid-managed cryptsetup option"
            ),
            "got: {msg}"
        );
    }

    #[test]
    fn luks_format_extra_opts_rejects_bare_uuid() {
        // Intent: bare `--uuid` (no `=`) is rejected defensively even
        //   though clap's `require_equals = true` blocks the pair form.
        let err = LuksFormatExtraOpts::parse(&["--uuid".to_owned()]).unwrap_err();
        assert!(
            err.to_string()
                .contains("--luks-format-arg '--uuid' targets a braid-managed cryptsetup option")
        );
    }

    #[test]
    fn luks_format_extra_opts_rejects_label_equals() {
        // Intent: `--label=<value>` is rejected.
        let err = LuksFormatExtraOpts::parse(&["--label=braid-x".to_owned()]).unwrap_err();
        assert!(err.to_string().contains(
            "--luks-format-arg '--label=braid-x' targets a braid-managed cryptsetup option"
        ));
    }

    #[test]
    fn luks_format_extra_opts_rejects_bare_label() {
        // Intent: bare `--label` (no `=`) is rejected.
        let err = LuksFormatExtraOpts::parse(&["--label".to_owned()]).unwrap_err();
        assert!(
            err.to_string()
                .contains("--luks-format-arg '--label' targets a braid-managed cryptsetup option")
        );
    }

    #[test]
    fn luks_format_extra_opts_accepts_non_managed() {
        // Intent: legitimate extras are passed through unchanged.
        // Why: positive-extras forwarding regression -- a regression that
        //   silently drops accepted extras would pass the rejection suite.
        let opts = LuksFormatExtraOpts::parse(&["--use-random".to_owned()]).unwrap();
        assert_eq!(opts.as_slice(), &["--use-random".to_owned()]);
    }
}
