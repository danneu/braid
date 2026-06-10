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
    /// Original text supplied by the user, parser, or fixture before
    /// canonicalization failed.
    pub raw: String,
    /// Parser-specific reason from `uuid::Uuid`, kept so CLI errors do not
    /// collapse all malformed UUIDs into the same opaque message.
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
// Fsid -- canonical, validated btrfs filesystem UUID identity
// ---------------------------------------------------------------------------

/// Persistent btrfs filesystem UUID identity. Inner string is canonicalized to
/// lowercase hyphenated form via `Fsid::parse`. The type is the single source of
/// truth for "which btrfs pool" across `pending-op.json`, planner code, and live
/// probes; raw-string FSID comparison across the plan->recover boundary is the
/// mix-up this type makes a compile error.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fsid(String);

/// Error returned when constructing an `Fsid` from text that is not a
/// recognized UUID form. Routed through `Fsid::parse` so call sites surface
/// the offending input verbatim, mirroring `LuksUuidParseError`.
#[derive(Debug, Error)]
#[error("invalid btrfs FSID '{raw}': {detail}")]
pub struct FsidParseError {
    /// Original text supplied by the parser or a fixture before
    /// canonicalization failed.
    pub raw: String,
    /// Parser-specific reason from `uuid::Uuid`, kept so CLI errors do not
    /// collapse all malformed FSIDs into the same opaque message.
    pub detail: String,
}

impl Fsid {
    /// Parse and canonicalize any UUID form accepted by `uuid::Uuid` to
    /// lowercase hyphenated text. The only validating constructor; production
    /// (the btrfs-show parser) and tests share it so a hand-edited
    /// `pending-op.json` FSID canonicalizes the same way probed btrfs output
    /// does. No `new_v4`: braid never mints an FSID -- btrfs owns it.
    pub fn parse(raw: &str) -> Result<Self, FsidParseError> {
        match uuid::Uuid::parse_str(raw) {
            Ok(u) => Ok(Fsid(u.hyphenated().to_string())),
            Err(e) => Err(FsidParseError {
                raw: raw.to_owned(),
                detail: e.to_string(),
            }),
        }
    }

    /// Observation accessor for log lines, sysfs-path interpolation, and any
    /// non-Display formatter site that needs the raw canonical form.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Fsid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for Fsid {
    fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ser.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Fsid {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Re-parse through Fsid::parse so a hand-edited pending-op.json FSID is
        // re-validated and re-canonicalized on load -- the operator-editable
        // journal defense, identical to LuksUuid.
        let s = String::deserialize(de)?;
        Fsid::parse(&s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// DiskName -- presentation identity, validated to the braid disk-name contract
// ---------------------------------------------------------------------------

/// Operator-facing disk identifier used as the mapper-name and LUKS-label
/// suffix (`braid-<DiskName>`). Not a persistent identity -- `LuksUuid` is
/// -- but every label/mapper construction site goes through this type via
/// `config::mapper_name` and `config::luks_label_for` so the disk-name
/// character contract is enforced once.
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
    /// Original text supplied at a disk-name boundary.
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
    /// Original path supplied at a by-id boundary.
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
/// The constructor rejects tokens that target braid-managed identity
/// or storage-model-breaking cryptsetup options.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LuksFormatExtraOpts(Vec<String>);

/// Error returned when `LuksFormatExtraOpts::parse` rejects a token that
/// targets a braid-managed identity or storage-model-breaking cryptsetup
/// option.
#[derive(Debug, Error)]
pub enum LuksFormatExtraOptsError {
    #[error(
        "--luks-format-arg '{token}' targets a braid-managed identity or storage-model-breaking cryptsetup option"
    )]
    ManagedFormatFlag { token: String },
}

impl LuksFormatExtraOpts {
    /// Validate the supplied argv extras. Empty input is valid. Each token
    /// is checked against the braid-managed identity and storage-model
    /// reject list so unsafe extras fail before any
    /// `CryptsetupLuksFormat` request reaches the executor.
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

const MANAGED_LUKS_FORMAT_LONG_FLAGS: &[&str] = &[
    "--uuid",
    "--label",
    "--header",
    "--key-file",
    "--master-key-file",
    "--volume-key-file",
    "--key-slot",
    "--type",
    "--integrity",
    "--integrity-key-size",
    "--integrity-inline",
    "--integrity-no-journal",
    "--integrity-no-wipe",
    "--integrity-legacy-padding",
    "--keyfile-offset",
    "--keyfile-size",
    "--offset",
    "--align-payload",
    "--luks2-metadata-size",
    "--luks2-keyslots-size",
    "--sector-size",
];

const MANAGED_LUKS_FORMAT_SHORT_FLAGS: &[char] = &['d', 'S', 'M', 'I', 'l', 'o'];

fn is_managed_format_flag(token: &str) -> bool {
    // These flags either overlap identity fields braid writes itself
    // (`--uuid`, `--label`) or change the on-disk/passphrase model braid
    // assumes after format. Names and short aliases come from
    // reference/cryptsetup/src/cryptsetup_arg_list.h.
    //
    // popt matches long options by full name only -- no getopt_long-style
    // abbreviation (`longOptionStrcmp` requires equal length) -- so exact and
    // `--flag=value` are the only spellings that reach a managed flag. A
    // prefix like `--uui` is rejected by cryptsetup as unknown, never read
    // as `--uuid`.
    if MANAGED_LUKS_FORMAT_LONG_FLAGS.iter().any(|flag| {
        token == *flag
            || token
                .strip_prefix(flag)
                .is_some_and(|rest| rest.starts_with('='))
    }) {
        return true;
    }

    // popt allows short-option clusters, so a toggle can lead a
    // value-taking short. Scan the whole single-hyphen cluster before
    // `=` so `-qMluks1` is treated as `-q -M luks1`.
    if let Some(shorts) = token.strip_prefix('-').filter(|_| !token.starts_with("--")) {
        let cluster = shorts
            .split_once('=')
            .map_or(shorts, |(cluster, _)| cluster);
        return cluster
            .chars()
            .any(|short| MANAGED_LUKS_FORMAT_SHORT_FLAGS.contains(&short));
    }

    false
}

// ---------------------------------------------------------------------------
// MapperName / MountPoint
// ---------------------------------------------------------------------------

/// Wraps a `/dev/mapper/<name>` basename so callers can pass mapper identity
/// without re-parsing strings; LUKS UUID stays the persistent identity, this
/// type is for presentation and command argv construction only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MapperName(pub String);

impl MapperName {
    /// Borrow the mapper basename at command argv and filesystem-path
    /// boundaries without exposing mutable access to the identity text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Wraps a LUKS2 label braid writes into the cryptsetup header so callers
/// cannot accidentally pass an unvalidated string in its place. Observed
/// probe labels stay `Option<String>` because cryptsetup may report
/// non-braid text.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LuksLabel(String);

impl LuksLabel {
    /// The sole constructor: derive `braid-<DiskName>` from a validated
    /// disk name and keep arbitrary label bytes outside braid-owned calls.
    pub fn for_disk(name: &DiskName) -> Self {
        LuksLabel(format!("braid-{}", name.as_str()))
    }

    /// Borrow the label text at command argv and probe-comparison
    /// boundaries without exposing the inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Wraps the absolute mount path braid hands to `mount(8)` so it cannot be
/// confused with arbitrary user paths at call sites that mix the two.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MountPoint(pub String);

impl MountPoint {
    /// Borrow the configured mount path for command argv and mountinfo
    /// comparisons without exposing a mutable string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MapperName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for LuksLabel {
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
    pub fsid: Option<Fsid>,
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
    /// Live backing path for a present pool device identified by LUKS UUID.
    /// Hardware queries must prefer this over persisted by-id paths because
    /// those setup/repair handles can drift while the member is still present.
    pub fn underlying_for_uuid(&self, uuid: &LuksUuid) -> Option<&str> {
        self.devices
            .iter()
            .find(|d| d.luks_uuid == *uuid)
            .map(|d| d.underlying.as_str())
    }

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

/// Live btrfs member observed through `probe_pool`; identity comes from
/// the LUKS UUID probed from the mapper's backing device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolDevice {
    /// Observed mapper basename from btrfs/probe output; used for command
    /// argv and diagnostics, not persistent identity.
    pub mapper: MapperName,
    /// Persistent LUKS UUID for this live device.
    pub luks_uuid: LuksUuid,
    /// Live btrfs devid for topology and missing-device correlation.
    pub devid: u64,
    /// Backing block device path reported by `cryptsetup status`.
    pub underlying: String,
}

/// A pool device whose LUKS mapper is open but the underlying block device
/// is gone (hot-unplugged). These are effectively missing for alerting but
/// not yet confirmed by btrfs as MISSING.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NullUnderlyingDevice {
    /// Observed mapper basename whose backing device is currently `(null)`.
    pub mapper: MapperName,
    /// Btrfs devid still associated with the open mapper.
    pub devid: u64,
}

/// Pre-probed state of each config disk (produced by probe, consumed by commands).
/// `name` is the validated `DiskName` resolved at the probe boundary so
/// downstream command code never re-checks the disk-name contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDisk {
    pub name: DiskName,
    pub by_id_path: ByIdPath,
    pub state: ConfigDiskState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigDiskState {
    /// Device file doesn't exist (unplugged / absent).
    Absent,
    /// `luksUUID` failed: either not LUKS-formatted, or a LUKS header that
    /// cryptsetup's `crypt_load` cannot read or validate. Refined into
    /// Unreadable for diagnostics (status/doctor/TUI) while add/replace
    /// keep this coarse state for their destructive-format guards.
    PresentNotLuks,
    /// Device exists, has LUKS header, UUID known.
    /// `label` is the optional LUKS2 label captured from the same luksDump
    /// probe that verifies braid's LUKS2 invariant.
    /// `mapper_open` = true if `/dev/mapper/<name>` is already active (crash recovery skip).
    PresentLuks {
        uuid: LuksUuid,
        label: Option<String>,
        mapper_open: bool,
    },
}

/// Planner-side disk state after the command boundary rejects unplugged
/// disks. Builders consume this narrower shape so absence checks remain
/// centralized in the top-level planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentConfigDiskState {
    PresentNotLuks,
    PresentLuks {
        uuid: LuksUuid,
        label: Option<String>,
        mapper_open: bool,
    },
}

/// `ConfigDisk` after planner-side presence validation, retaining the
/// identity fields needed for diagnostics and downstream command planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentConfigDisk {
    pub name: DiskName,
    pub by_id_path: ByIdPath,
    pub state: PresentConfigDiskState,
}

impl TryFrom<ConfigDisk> for PresentConfigDisk {
    /// Returns the original `ConfigDisk` so the caller can format the
    /// absent-disk diagnostic from the same identity the probe produced.
    type Error = ConfigDisk;

    fn try_from(cd: ConfigDisk) -> Result<Self, ConfigDisk> {
        let ConfigDisk {
            name,
            by_id_path,
            state,
        } = cd;
        match state {
            ConfigDiskState::Absent => Err(ConfigDisk {
                name,
                by_id_path,
                state: ConfigDiskState::Absent,
            }),
            ConfigDiskState::PresentNotLuks => Ok(PresentConfigDisk {
                name,
                by_id_path,
                state: PresentConfigDiskState::PresentNotLuks,
            }),
            ConfigDiskState::PresentLuks {
                uuid,
                label,
                mapper_open,
            } => Ok(PresentConfigDisk {
                name,
                by_id_path,
                state: PresentConfigDiskState::PresentLuks {
                    uuid,
                    label,
                    mapper_open,
                },
            }),
        }
    }
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
    fn try_from_config_disk_absent_preserves_identity() {
        // Intent: the refined present-disk conversion returns the original
        //   identity when a probed config disk is absent.
        // Why it exists: planner-level absent-disk errors format the disk
        //   name and by-id path from the conversion error.
        // Scenario: a configured replacement target is unplugged; the
        //   planner still reports the exact requested disk identity.
        let name = DiskName::parse("disk3").expect("valid disk name");
        let by_id_path = ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap();
        let disk = ConfigDisk {
            name: name.clone(),
            by_id_path: by_id_path.clone(),
            state: ConfigDiskState::Absent,
        };

        let err = PresentConfigDisk::try_from(disk).expect_err("absent disk should not refine");

        assert_eq!(err.name, name);
        assert_eq!(err.by_id_path, by_id_path);
        assert_eq!(err.state, ConfigDiskState::Absent);
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

    // -- Fsid ---------------------------------------------------------------

    #[test]
    fn fsid_parse_canonicalizes_uppercase() {
        // Intent: uppercase hyphenated FSID canonicalizes to lowercase.
        // Why: canonicalization gates equality with btrfs's lowercase output
        //   across the plan->recover boundary.
        // Scenario: an operator hand-edits pending-op.json with an uppercase
        //   verified_pool_fsid; the loaded value equates with probed output.
        let f = Fsid::parse("8C78A966-EF17-4610-B835-5B376EF10B4E").unwrap();
        assert_eq!(f.as_str(), "8c78a966-ef17-4610-b835-5b376ef10b4e");
    }

    #[test]
    fn fsid_parse_canonicalizes_simple_form() {
        // Intent: 32-hex simple form parses to the canonical hyphenated form.
        // Why: the canonicalizer is the single source of truth for FSID
        //   equality regardless of which UUID spelling the input carried.
        let f = Fsid::parse("8c78a966ef174610b8355b376ef10b4e").unwrap();
        assert_eq!(f.as_str(), "8c78a966-ef17-4610-b835-5b376ef10b4e");
    }

    #[test]
    fn fsid_parse_canonicalizes_urn() {
        // Intent: URN form (`urn:uuid:...`) parses to the canonical
        //   hyphenated form.
        // Why: uuid::Uuid::parse_str accepts URN; the canonicalizer must not
        //   silently reject a valid alternative form.
        let f = Fsid::parse("urn:uuid:8c78a966-ef17-4610-b835-5b376ef10b4e").unwrap();
        assert_eq!(f.as_str(), "8c78a966-ef17-4610-b835-5b376ef10b4e");
    }

    #[test]
    fn fsid_parse_rejects_invalid() {
        // Intent: non-UUID text fails parse with the offending raw input.
        // Why: invalid identity must surface as a parse error so the
        //   btrfs-show parser can route it into ParseError::InvalidValue and
        //   the journal deserialize path can reject a corrupt FSID.
        let err = Fsid::parse("not-a-uuid").unwrap_err();
        assert_eq!(err.raw, "not-a-uuid");
        assert!(
            !err.detail.is_empty(),
            "detail must carry uuid-crate reason"
        );
    }

    #[test]
    fn fsid_serialize_round_trip() {
        // Intent: serialize emits canonical form; deserialize re-parses
        //   through the canonicalizer.
        // Why: round-trip stability is load-bearing for pending-op.json
        //   journal write and replay.
        let f = Fsid::parse("8C78A966-EF17-4610-B835-5B376EF10B4E").unwrap();
        let s = serde_json::to_string(&f).unwrap();
        assert_eq!(s, "\"8c78a966-ef17-4610-b835-5b376ef10b4e\"");
        let back: Fsid = serde_json::from_str(&s).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn fsid_deserialize_canonicalizes_uppercase() {
        // Intent: a JSON-source uppercase FSID deserializes equal to its
        //   lowercase form.
        // Why: this is the operator-editable journal defense -- a hand-edited
        //   uppercase verified_pool_fsid in pending-op.json must load as the
        //   canonical lowercase form and equate with probed btrfs output.
        let upper: Fsid = serde_json::from_str("\"8C78A966-EF17-4610-B835-5B376EF10B4E\"").unwrap();
        let lower: Fsid = serde_json::from_str("\"8c78a966-ef17-4610-b835-5b376ef10b4e\"").unwrap();
        assert_eq!(upper, lower);
    }

    #[test]
    fn fsid_deserialize_rejects_invalid() {
        // Intent: a JSON-source non-UUID FSID fails to deserialize.
        // Why: deny_unknown_fields catches stale keys, but a malformed value
        //   on a known field requires Deserialize to route through
        //   Fsid::parse so a corrupt journal FSID is rejected on load.
        let r: Result<Fsid, _> = serde_json::from_str("\"fsid-1\"");
        assert!(r.is_err());
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

    fn assert_luks_format_extra_rejects(token: &str) {
        let err = LuksFormatExtraOpts::parse(&[token.to_owned()]).unwrap_err();
        match err {
            LuksFormatExtraOptsError::ManagedFormatFlag { token: offending } => {
                assert_eq!(offending, token);
            }
        }
    }

    #[test]
    fn luks_format_extra_opts_rejects_uuid_equals() {
        // Intent: `--uuid=<value>` is rejected with the pinned wording.
        // Why: user-supplied identity must not override the journaled UUID.
        let err = LuksFormatExtraOpts::parse(&["--uuid=foo".to_owned()]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(
                "--luks-format-arg '--uuid=foo' targets a braid-managed identity or storage-model-breaking cryptsetup option"
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
                .contains("--luks-format-arg '--uuid' targets a braid-managed identity or storage-model-breaking cryptsetup option")
        );
    }

    #[test]
    fn luks_format_extra_opts_rejects_offset() {
        // Intent: `--offset` is rejected before cryptsetup can change
        //   the LUKS2 payload offset.
        // Why: replace preflight models fresh-LUKS mapper capacity from
        //   braid's fixed default header size.
        // Scenario: operator attempts to pass a raw offset override.
        assert_luks_format_extra_rejects("--offset");
    }

    #[test]
    fn luks_format_extra_opts_rejects_offset_short() {
        // Intent: `-o` is rejected as the short alias for `--offset`.
        // Why: short-option clusters must not bypass the offset guard.
        // Scenario: operator passes cryptsetup's short offset flag.
        assert_luks_format_extra_rejects("-o");
    }

    #[test]
    fn luks_format_extra_opts_rejects_align_payload() {
        // Intent: `--align-payload` is rejected before it can alter
        //   payload placement.
        // Why: fresh-LUKS target capacity must stay derived from the
        //   cryptsetup LUKS2 default, not user-chosen alignment.
        // Scenario: operator tries an alignment override during replace.
        assert_luks_format_extra_rejects("--align-payload");
    }

    #[test]
    fn luks_format_extra_opts_rejects_luks2_metadata_size() {
        // Intent: `--luks2-metadata-size` is rejected.
        // Why: changing metadata area size changes the space reserved
        //   before the data segment.
        // Scenario: operator tries to customize LUKS2 metadata sizing.
        assert_luks_format_extra_rejects("--luks2-metadata-size");
    }

    #[test]
    fn luks_format_extra_opts_rejects_luks2_keyslots_size() {
        // Intent: `--luks2-keyslots-size` is rejected.
        // Why: changing keyslot area size changes header layout and the
        //   payload offset braid assumes for fresh targets.
        // Scenario: operator tries to customize LUKS2 keyslot sizing.
        assert_luks_format_extra_rejects("--luks2-keyslots-size");
    }

    #[test]
    fn luks_format_extra_opts_rejects_sector_size() {
        // Intent: `--sector-size` is rejected conservatively.
        // Why: sector-size overrides can affect cryptsetup alignment and
        //   make braid's fresh-LUKS capacity estimate unsafe.
        // Scenario: operator tries a non-default sector-size override.
        assert_luks_format_extra_rejects("--sector-size");
    }

    #[test]
    fn luks_format_extra_opts_rejects_payload_offset_equals_form() {
        // Intent: offset-affecting flags are rejected in `--flag=value`
        //   form, not only as bare tokens.
        // Why: clap passes each `--luks-format-arg=--offset=...` value
        //   through as one raw token.
        // Scenario: operator supplies a byte offset inline.
        assert_luks_format_extra_rejects("--offset=32768");
    }

    #[test]
    fn luks_format_extra_opts_rejects_label_equals() {
        // Intent: `--label=<value>` is rejected.
        let err = LuksFormatExtraOpts::parse(&["--label=braid-x".to_owned()]).unwrap_err();
        assert!(err.to_string().contains(
            "--luks-format-arg '--label=braid-x' targets a braid-managed identity or storage-model-breaking cryptsetup option"
        ));
    }

    #[test]
    fn luks_format_extra_opts_rejects_bare_label() {
        // Intent: bare `--label` (no `=`) is rejected.
        let err = LuksFormatExtraOpts::parse(&["--label".to_owned()]).unwrap_err();
        assert!(
            err.to_string()
                .contains("--luks-format-arg '--label' targets a braid-managed identity or storage-model-breaking cryptsetup option")
        );
    }

    #[test]
    fn luks_format_extra_opts_rejects_long_form_set() {
        // Intent: every long-form managed/storage-breaking flag is rejected
        //   in bare and `--flag=value` form.
        // Why: `--luks-format-arg` accepts raw argv tokens, so each unsafe
        //   cryptsetup spelling must be stopped at the shared parse gate.
        // Scenario: operator passes a raw luksFormat flag that would alter
        //   braid's identity, header, key material, type, or integrity model.
        for flag in MANAGED_LUKS_FORMAT_LONG_FLAGS {
            for token in [(*flag).to_owned(), format!("{flag}=value")] {
                let err = LuksFormatExtraOpts::parse(std::slice::from_ref(&token)).unwrap_err();
                let msg = err.to_string();
                match err {
                    LuksFormatExtraOptsError::ManagedFormatFlag { token: offending } => {
                        assert_eq!(
                            offending, token,
                            "error must preserve the offending token verbatim"
                        );
                    }
                }
                assert!(
                    msg.contains(&format!("--luks-format-arg '{token}'")),
                    "error must echo {token:?}"
                );
            }
        }
    }

    #[test]
    fn luks_format_extra_opts_rejects_short_aliases() {
        // Intent: managed/storage-breaking short aliases are rejected in
        //   bare, concatenated, equals, and clustered forms.
        // Why: cryptsetup uses popt, which accepts clusters like
        //   `-qMluks1`; a prefix-only check would miss that spelling.
        // Scenario: operator combines a harmless toggle with a forbidden
        //   value-taking short in one raw `--luks-format-arg` token.
        for short in MANAGED_LUKS_FORMAT_SHORT_FLAGS {
            for token in [
                format!("-{short}"),
                format!("-{short}value"),
                format!("-{short}=value"),
            ] {
                let err = LuksFormatExtraOpts::parse(std::slice::from_ref(&token)).unwrap_err();
                let msg = err.to_string();
                match err {
                    LuksFormatExtraOptsError::ManagedFormatFlag { token: offending } => {
                        assert_eq!(
                            offending, token,
                            "error must preserve the offending token verbatim"
                        );
                    }
                }
                assert!(
                    msg.contains(&format!("--luks-format-arg '{token}'")),
                    "error must echo {token:?}"
                );
            }
        }

        for token in ["-qMluks1", "-vIhmac-sha256", "-ql16"] {
            let err = LuksFormatExtraOpts::parse(&[token.to_owned()]).unwrap_err();
            let msg = err.to_string();
            match err {
                LuksFormatExtraOptsError::ManagedFormatFlag { token: offending } => {
                    assert_eq!(
                        offending, token,
                        "error must preserve the offending token verbatim"
                    );
                }
            }
            assert!(
                msg.contains(&format!("--luks-format-arg '{token}'")),
                "error must echo {token:?}"
            );
        }
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
