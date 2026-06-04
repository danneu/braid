use crate::by_id::{ByIdResolver, RealByIdResolver, by_id_priority, is_partition_entry};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::membership::{DiskMember, PoolMembership, save_membership};
use crate::parse::{
    ParseError, parse_cryptsetup_luks_label, parse_cryptsetup_luks_uuid_from_dump,
    parse_cryptsetup_luks_version,
};
use crate::state_paths::StatePaths;
use crate::types::{ByIdPath, DiskName, LuksUuid};
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fmt;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum DiscoverError {
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("failed to read /dev/disk/by-id: {0}")]
    ReadDir(#[source] std::io::Error),
    /// Defensive post-discover-precheck insert backstop for logic bugs,
    /// separate from reachable duplicate-disk paths (`DuplicateUuid` and
    /// `LabelCollision`) that discover reports before membership insertion.
    #[error("membership insert failed after discover pre-checks: {0}")]
    MembershipInsert(#[source] crate::membership::MembershipError),
    #[error(
        "label collision: braid-{name} found on two distinct devices ({path1}, {path2}) -- relabel or detach one before retrying"
    )]
    LabelCollision {
        name: String,
        path1: String,
        path2: String,
    },
    /// Two physically distinct disks share one LUKS UUID -- typically the
    /// dd-cloned-disk case. Discover names both by-id paths and both
    /// labels so the operator can pick which one to detach.
    /// Raised explicitly in the discover code path before delegating to
    /// `PoolMembership::insert` so the friendly wording reaches the
    /// operator instead of the generic `MembershipError::Conflict`.
    #[error(
        "duplicate LUKS UUID: braid-{name1} ({path1}) and braid-{name2} ({path2}) share UUID {uuid} -- detach the cloned or unintended disk before retrying (this typically indicates a dd-cloned disk)"
    )]
    DuplicateUuid {
        uuid: LuksUuid,
        name1: DiskName,
        path1: String,
        name2: DiskName,
        path2: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum DiscoverWarning {
    LuksDumpFailed {
        path: String,
        exit_code: i32,
        stderr: String,
    },
    LuksDumpUnparseable {
        path: String,
        detail: String,
    },
    UnsupportedLuksVersion {
        path: String,
        version: u32,
    },
    CannotCanonicalize {
        path: String,
        detail: String,
    },
    InvalidDiskName {
        path: String,
        label: String,
    },
    /// Discovery read a braid-labeled LUKS2 disk whose `luksDump` text
    /// body carried a `UUID:` value that `LuksUuid::parse` rejected.
    /// The disk is skipped; the warning carries the raw offending text
    /// so operators can correlate against `cryptsetup luksDump` output.
    InvalidLuksUuid {
        path: String,
        raw: String,
        detail: String,
    },
}

impl fmt::Display for DiscoverWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiscoverWarning::LuksDumpFailed {
                path,
                exit_code,
                stderr,
            } => write!(
                f,
                "skipping {path}: luksDump failed (exit {exit_code}) -- {}",
                stderr.trim()
            ),
            DiscoverWarning::LuksDumpUnparseable { path, detail } => {
                write!(
                    f,
                    "skipping {path}: luksDump output unparseable -- {detail}"
                )
            }
            DiscoverWarning::UnsupportedLuksVersion { path, version } => {
                write!(f, "skipping {path}: LUKS{version} (braid requires LUKS2)")
            }
            DiscoverWarning::CannotCanonicalize { path, detail } => {
                write!(f, "skipping {path}: cannot canonicalize -- {detail}")
            }
            DiscoverWarning::InvalidDiskName { path, label } => write!(
                f,
                "skipping {path}: label \"{}\" has an invalid disk name",
                label.escape_default(),
            ),
            DiscoverWarning::InvalidLuksUuid { path, raw, detail } => write!(
                f,
                "skipping {path}: invalid LUKS UUID \"{raw}\" -- {detail}"
            ),
        }
    }
}

/// Outcome of a discover scan. Warnings are populated even when the
/// structural result is an error so command wrappers can render sibling
/// hazards before propagating the failure.
#[derive(Debug)]
pub struct DiscoverScan {
    /// Non-fatal scan findings for disks skipped before membership use.
    pub warnings: Vec<DiscoverWarning>,
    /// UUID-keyed membership reconstructed from attached braid-labeled disks.
    pub result: Result<PoolMembership, DiscoverError>,
}

/// Format operator-visible discover preview rows in DiskName order.
/// Returned as lines so the binary entry point stays easy to test.
pub fn render_preview_lines(members: &PoolMembership) -> Vec<String> {
    members
        .iter_by_name()
        .into_iter()
        .map(|(_, member)| format!("  {} = {}", member.name, member.by_id))
        .collect()
}

/// Drain scan warnings before yielding the structural result, matching
/// decision 022's report shape for plans that can accumulate notes and
/// fail later.
pub fn drain_warnings<W: std::io::Write>(
    scan: DiscoverScan,
    out: &mut W,
) -> Result<PoolMembership, DiscoverError> {
    for warning in &scan.warnings {
        let _ = writeln!(out, "warning: {warning}");
    }
    scan.result
}

/// Discover-side fail-closed errors that fire from the `--write` path
/// before any `save_membership` call. Separate from `DiscoverError`
/// (which collects pre-write failures from the scan itself) because
/// each variant pins an operator-facing remediation message that
/// downstream tests assert against.
#[derive(Debug, thiserror::Error)]
pub enum DiscoverWriteError {
    /// `pending-op.json` exists at the journal path -- the discover
    /// `--write` precondition fails closed instead of overwriting
    /// `pool.json` mid-recovery (see `docs/internals/luks-unlock.md`).
    #[error(
        "discover refusing to write pool.json: pending-op.json exists at {path} -- run 'braid recover' first (see docs/internals/luks-unlock.md)"
    )]
    PendingOpExists { path: String },
    /// Existing `pool.json` on disk is already a healthy UUID-keyed
    /// membership. `discover --write` would clobber persisted
    /// `DiskMember.devid` bindings, which are decision 024's authorized
    /// fallback identity for `null_underlying` mappers and btrfs
    /// `missing_devids`. The operator must move the file aside;
    /// `discover` is not the surface for mutating an established pool.
    #[error(
        "discover refusing to write pool.json: existing file at {path} is already a healthy UUID-keyed membership -- back it up and move it aside before retrying, or use 'braid add' / 'braid remove' / 'braid replace' to mutate membership (see docs/internals/luks-unlock.md)"
    )]
    ValidUuidKeyed { path: String },
    /// Existing `pool.json` is corrupt and would normally be rebuilt in
    /// place, but its forensic sidecar could not be written. Refuse the
    /// destructive save so prior-binding bytes in the corrupt file survive.
    #[error(
        "discover refusing to write pool.json: failed to snapshot existing corrupt file to {sidecar}: {source} -- refusing to overwrite the corrupt original without a forensic copy; free disk space or fix permissions on the state directory and retry"
    )]
    CorruptSidecarFailed {
        sidecar: String,
        #[source]
        source: std::io::Error,
    },
    /// `--expect-count <N>` was set and discovery produced a member
    /// count other than `N`. Catches partial-attach and unintended
    /// extra-disk hazards during rebuilds.
    #[error(
        "discover refusing to write pool.json: expected exactly {expected} members, found {actual} -- check that all intended pool members are attached and readable, and that no unrelated braid-labeled disks are attached, then retry"
    )]
    ExpectCountUnmet { expected: usize, actual: usize },
    /// `save_membership` failed at the I/O / serialization layer.
    /// Forwards the underlying `MembershipError` so the test surface
    /// still pins the message wording from `membership.rs`.
    #[error("failed to write pool membership: {0}")]
    Save(#[from] crate::membership::MembershipError),
    /// A `DuplicateUuid` (or other structural) discover error fired
    /// during the scan itself. The variant lifts it into the
    /// `--write` error space so callers handle one error type.
    #[error(transparent)]
    Discover(#[from] DiscoverError),
}

/// Bare `braid discover` (no --write) preflight refusals. Mirrors
/// `DiscoverWriteError` so both gating paths have a single thiserror
/// surface and matching unit-test coverage in this module.
#[derive(Debug, thiserror::Error)]
pub enum BareDiscoverError {
    #[error(
        "pool.json already exists at {path} -- live discovery is not authoritative once pool.json exists; discover is for rebuilding missing or corrupt pool state"
    )]
    ValidUuidKeyed { path: String },
    #[error(
        "pool.json at {path} is corrupt or unreadable -- run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/internals/luks-unlock.md)"
    )]
    Corrupt { path: String },
}

/// Shared post-scan refusal when discover finds zero braid-labeled LUKS2
/// members. Typed (not a bare `eprintln!`) so both the bare preview and
/// `--write` paths surface one remediation-bearing message through
/// `print_cli_error`, matching the other discover refusals.
#[derive(Debug, thiserror::Error)]
#[error(
    "no braid-labeled LUKS2 devices found -- check that pool members are attached and readable, and labeled braid-<name> as LUKS2 (LUKS1 or unreadable disks, if any, are skipped with a warning above)"
)]
pub struct NoMembersDiscovered;

/// Scan /dev/disk/by-id/ for LUKS devices with `braid-<name>` labels.
/// Returns a report so callers can print warnings on success or error.
pub fn discover_pool_members<R: CommandRunner>(runner: &R) -> DiscoverScan {
    discover_from_dir(runner, &RealByIdResolver, Path::new("/dev/disk/by-id"))
}

/// Classifies an existing pool.json by shape for discover's gating.
/// A valid UUID-keyed file is only recognized by a successful
/// `PoolMembership` load; everything else that is not missing is
/// treated as corrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolJsonShape {
    Missing,
    ValidUuidKeyed,
    Corrupt,
}

/// Classifies pool.json through the canonical loader before routing
/// loader failures into the generic corrupt-state rebuild path.
pub fn classify_pool_json(path: &Path) -> PoolJsonShape {
    match crate::membership::load_membership_from(path) {
        Ok(_) => PoolJsonShape::ValidUuidKeyed,
        Err(crate::membership::MembershipError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            PoolJsonShape::Missing
        }
        Err(_) => PoolJsonShape::Corrupt,
    }
}

/// Build a `LabelCollision` error from two colliding by-id paths.
/// Sorts the paths lexicographically so the error is deterministic
/// regardless of read_dir ordering.
fn label_collision(name: &str, a: String, b: String) -> DiscoverError {
    let mut paths = [a, b];
    paths.sort();
    let [path1, path2] = paths;
    DiscoverError::LabelCollision {
        name: name.to_owned(),
        path1,
        path2,
    }
}

/// Internal accumulator entry for the alias-dedup pass. Keeps the
/// best-priority by-id, the canonical target path (for label-collision
/// detection), and the probed `LuksUuid` so the post-dedup duplicate
/// check has both UUID and path in scope.
struct AliasCandidate {
    priority: u8,
    filename: String,
    by_id: ByIdPath,
    canonical: String,
    luks_uuid: LuksUuid,
}

/// Testable discover wrapper that preserves warnings collected before
/// any structural error returned by the inner scanner.
fn discover_from_dir<R: CommandRunner>(
    runner: &R,
    resolver: &dyn ByIdResolver,
    by_id_dir: &Path,
) -> DiscoverScan {
    let mut warnings = Vec::new();
    let result = discover_from_dir_inner(runner, resolver, by_id_dir, &mut warnings);
    DiscoverScan { warnings, result }
}

/// Inner scanner keeps the existing Result-shaped control flow while
/// borrowing the outer warning accumulator for error-path reporting.
fn discover_from_dir_inner<R: CommandRunner>(
    runner: &R,
    resolver: &dyn ByIdResolver,
    by_id_dir: &Path,
    warnings: &mut Vec<DiscoverWarning>,
) -> Result<PoolMembership, DiscoverError> {
    let entries = match std::fs::read_dir(by_id_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PoolMembership::empty());
        }
        Err(e) => return Err(DiscoverError::ReadDir(e)),
    };
    let mut entries: Vec<std::fs::DirEntry> = entries
        .collect::<Result<Vec<_>, _>>()
        .map_err(DiscoverError::ReadDir)?;
    entries.sort_by_key(|entry| entry.file_name());

    // Per-disk-name best candidate, keyed by the validated DiskName.
    // The Occupied arm tie-breaks within an alias set without
    // re-extracting the basename or re-probing the LUKS UUID.
    let mut members: BTreeMap<DiskName, AliasCandidate> = BTreeMap::new();
    let mut first_collision: Option<DiscoverError> = None;

    for entry in entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip partition entries (e.g., ata-TOSHIBA-part1)
        if is_partition_entry(&name_str) {
            continue;
        }

        let path = entry.path();
        let path_str = path.to_string_lossy().to_string();

        // Catch stale udev by-id symlinks before the LUKS probe. A dangling
        // symlink is a structural by-id problem independent of LUKS state.
        let canonical = match resolver.canonicalize(&path_str) {
            Ok(c) => c,
            Err(e) => {
                warnings.push(DiscoverWarning::CannotCanonicalize {
                    path: path_str.clone(),
                    detail: e.to_string(),
                });
                continue;
            }
        };

        // isLuks is the silent bulk filter: discover probes every
        // non-partition by-id entry and most are legitimately not LUKS
        // (boot disk, USB sticks), so a nonzero exit is the common case
        // and must not warn. The header-classification failure is
        // unclassifiable in-band: after crypt_init succeeds,
        // action_isLuks installs quiet_log before crypt_load
        // (reference/cryptsetup/src/cryptsetup.c:2475-2479;
        // src/utils_tools.c:84-91), crypt_load calls
        // _crypt_load_luks(..., true, false)
        // (reference/cryptsetup/lib/setup.c:1121), and the quiet LUKS
        // path suppresses the normal "not a valid LUKS device" error
        // (reference/cryptsetup/lib/setup.c:892-893). translate_errno
        // collapses both -EINVAL ("not a LUKS device") and default -EIO
        // to exit 1 (src/utils_tools.c:219-235), so a transient read
        // error is indistinguishable from a non-member. We skip silently
        // and let `discover --write --expect-count <N>` fail closed if a
        // member is momentarily unreadable. (probe_luks_header in
        // luks.rs maps the same nonzero exit to `Unreadable` because
        // its caller already knows the device is a pool member; the
        // luksDump path below warns because isLuks has by then confirmed
        // this is LUKS.)
        let raw = runner.run(&CmdRequest::CryptsetupIsLuks {
            device: path_str.clone(),
        })?;
        if raw.exit_status != 0 {
            continue;
        }

        // Read LUKS label + version + UUID via the same luksDump text
        // output. One luksDump call, three parses on the same
        // RawCommandOutput. The version check enforces braid's
        // LUKS2-only invariant at this gateway so a braid-labeled
        // LUKS1 disk never reaches pool.json via `braid discover
        // --write`.
        let dump_raw = runner.run(&CmdRequest::CryptsetupLuksDumpText {
            device: path_str.clone(),
        })?;

        let version = match parse_cryptsetup_luks_version(&dump_raw) {
            Ok(out) => out.version,
            Err(ParseError::CommandFailed {
                exit_code, stderr, ..
            }) => {
                warnings.push(DiscoverWarning::LuksDumpFailed {
                    path: path_str.clone(),
                    exit_code,
                    stderr,
                });
                continue;
            }
            Err(e) => {
                warnings.push(DiscoverWarning::LuksDumpUnparseable {
                    path: path_str.clone(),
                    detail: e.to_string(),
                });
                continue;
            }
        };
        if version != 2 {
            warnings.push(DiscoverWarning::UnsupportedLuksVersion {
                path: path_str.clone(),
                version,
            });
            continue;
        }

        let label = parse_cryptsetup_luks_label(&dump_raw)
            .ok()
            .and_then(|out| out.label);

        // Require label = braid-<valid-name>. An invalid braid label warns so the
        // user can relabel; any other miss is a silent skip.
        let Some(label) = label else {
            continue;
        };
        let Some(disk_name_raw) = crate::config::name_from_mapper(&label) else {
            continue;
        };
        let disk_name = match DiskName::parse(disk_name_raw) {
            Ok(n) => n,
            Err(_) => {
                warnings.push(DiscoverWarning::InvalidDiskName {
                    path: path_str.clone(),
                    label: label.clone(),
                });
                continue;
            }
        };

        // Parse the LUKS UUID from the shared dump body. Missing /
        // invalid UUID surfaces as a structured warning and the disk
        // is skipped -- it cannot be a pool member without identity.
        let luks_uuid = match parse_cryptsetup_luks_uuid_from_dump(&dump_raw) {
            Ok(u) => u,
            Err(ParseError::InvalidValue { raw, detail, .. }) => {
                warnings.push(DiscoverWarning::InvalidLuksUuid {
                    path: path_str.clone(),
                    raw,
                    detail,
                });
                continue;
            }
            Err(e) => {
                warnings.push(DiscoverWarning::LuksDumpUnparseable {
                    path: path_str.clone(),
                    detail: e.to_string(),
                });
                continue;
            }
        };

        let filename = name_str.into_owned();
        let priority = by_id_priority(&filename);
        let by_id_path = Path::new("/dev/disk/by-id")
            .join(&filename)
            .to_string_lossy()
            .into_owned();
        let by_id = ByIdPath::parse(&by_id_path)
            .expect("by-id path comes from /dev/disk/by-id/ enumeration");

        let candidate = AliasCandidate {
            priority,
            filename,
            by_id,
            canonical,
            luks_uuid,
        };

        match members.entry(disk_name) {
            Entry::Vacant(e) => {
                e.insert(candidate);
            }
            Entry::Occupied(mut e) => {
                let existing = e.get();
                if existing.canonical != candidate.canonical {
                    if first_collision.is_none() {
                        first_collision = Some(label_collision(
                            e.key().as_str(),
                            existing.by_id.as_str().to_owned(),
                            candidate.by_id.as_str().to_owned(),
                        ));
                    }
                    continue;
                }

                // Same physical disk via two aliases -- keep the candidate
                // with the best (priority, filename) key so selection is
                // deterministic regardless of read_dir order.
                let candidate_better = (candidate.priority, candidate.filename.as_str())
                    < (existing.priority, existing.filename.as_str());
                if candidate_better {
                    e.insert(candidate);
                }
            }
        }
    }

    if let Some(err) = first_collision {
        return Err(err);
    }

    // After alias-dedup, surface duplicate UUIDs (the cloned-disk
    // hazard) as a structured DiscoverError::DuplicateUuid before
    // delegating to PoolMembership::insert. Both by-id paths and both
    // disk names are in scope here so the operator-facing message can
    // name them; PoolMembership::insert's generic Conflict cannot.
    let mut seen_uuids: BTreeMap<&LuksUuid, (&DiskName, &str)> = BTreeMap::new();
    for (name, cand) in &members {
        if let Some((prev_name, prev_path)) =
            seen_uuids.insert(&cand.luks_uuid, (name, cand.by_id.as_str()))
        {
            // Sort (name, path) pairs lexicographically by path for
            // determinism, matching the label_collision helper.
            let a = (prev_name.clone(), prev_path.to_owned());
            let b = (name.clone(), cand.by_id.as_str().to_owned());
            let (first, second) = if a.1 <= b.1 { (a, b) } else { (b, a) };
            return Err(DiscoverError::DuplicateUuid {
                uuid: cand.luks_uuid.clone(),
                name1: first.0,
                path1: first.1,
                name2: second.0,
                path2: second.1,
            });
        }
    }

    // Build a UUID-keyed PoolMembership from the deduped set.
    // PoolMembership::insert acts as a defense-in-depth backstop
    // (axis-1 UUID + axis-2 name + axis-3 by-id) even though the
    // pre-pass above has just narrowed the duplicate-UUID case.
    let mut membership = PoolMembership::empty();
    // Unreachable: insert's four axes are all pre-satisfied here. Axis 1
    // (UUID) by the seen_uuids pre-pass above; axis 2 (name) by members
    // being keyed on DiskName; axis 3 (by-id) by read_dir yielding unique
    // directory entries; axis 4 (devid) by DiskMember::new starting with
    // devid: None. Wrap defensively so any future regression surfaces the
    // MembershipError text verbatim.
    for (name, cand) in members {
        let member = DiskMember::new(name, cand.by_id);
        membership
            .insert(cand.luks_uuid, member)
            .map_err(DiscoverError::MembershipInsert)?;
    }

    Ok(membership)
}

/// Centralizes bare `braid discover` state-file gating so CLI dispatch
/// and wording-pinning unit tests share one refusal-message surface.
pub fn check_pool_json_for_bare_discover(path: &Path) -> Result<(), BareDiscoverError> {
    match classify_pool_json(path) {
        PoolJsonShape::Missing => Ok(()),
        PoolJsonShape::ValidUuidKeyed => Err(BareDiscoverError::ValidUuidKeyed {
            path: path.display().to_string(),
        }),
        PoolJsonShape::Corrupt => Err(BareDiscoverError::Corrupt {
            path: path.display().to_string(),
        }),
    }
}

/// Apply discover's `--write` pre-save fail-closed gates and persist
/// the discovered membership. The three gates pinned in the plan must
/// fire BEFORE any `save_membership` call:
/// 1. `pending-op.json` must not exist (covered by `PendingOpExists`).
/// 2. Existing `pool.json` must not be a healthy UUID-keyed membership
///    (covered by `ValidUuidKeyed`). `Corrupt` is intentionally allowed
///    -- it is the documented rebuild remediation per decision 017.
/// 3. When `pool.json` is `Corrupt`, the forensic snapshot to
///    `pool.json.corrupt-<RFC3339-UTC>` must succeed before rebuild
///    proceeds (covered by `CorruptSidecarFailed`). The sidecar is
///    written after `expected_count` validates, so a count-mismatch
///    refusal does not leave behind a sidecar of a file that was not
///    going to be overwritten.
///
/// When `expected_count` is set, the gate refuses if the produced
/// membership count is not exactly `expected_count`, catching a
/// momentarily detached disk or stray braid-labeled disk during any
/// `discover --write` rebuild.
///
/// On success returns the saved `PoolMembership`. The accepting
/// `Commands::Discover` CLI arm consumes both `warnings` (printed before
/// this call) and the saved membership.
pub fn write_discovered_membership(
    members: PoolMembership,
    paths: &StatePaths,
    expected_count: Option<usize>,
) -> Result<PoolMembership, DiscoverWriteError> {
    let journal_path = paths.pending_op_json();
    if journal_path.exists() {
        return Err(DiscoverWriteError::PendingOpExists {
            path: journal_path.display().to_string(),
        });
    }

    let pool_json_path = paths.pool_json();
    let needs_corrupt_sidecar = match classify_pool_json(&pool_json_path) {
        PoolJsonShape::ValidUuidKeyed => {
            return Err(DiscoverWriteError::ValidUuidKeyed {
                path: pool_json_path.display().to_string(),
            });
        }
        // `Missing` is the normal first-write path. `Corrupt` is the
        // documented rebuild remediation per decision 017, but the
        // corrupt file may still carry prior-binding bytes. Defer the
        // sidecar write until every other gate has passed.
        PoolJsonShape::Corrupt => true,
        PoolJsonShape::Missing => false,
    };

    if let Some(expected) = expected_count {
        let actual = members.len();
        if actual != expected {
            return Err(DiscoverWriteError::ExpectCountUnmet { expected, actual });
        }
    }

    if needs_corrupt_sidecar {
        crate::membership::write_corrupt_sidecar(&pool_json_path).map_err(|e| {
            DiscoverWriteError::CorruptSidecarFailed {
                sidecar: e.target().display().to_string(),
                source: e.into_source(),
            }
        })?;
    }

    save_membership(&members, paths)?;
    Ok(members)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
    use crate::test_fixtures::{
        DiscoverLabelMap, discover_create_by_id_symlink, discover_create_target,
    };

    /// Test-local helper: resolve a disk name to its discovered by-id
    /// string via the post-migration PoolMembership API. Replaces the
    /// pre-migration `members["sda"].0` BTreeMap indexing pattern.
    fn by_id_for(members: &PoolMembership, name: &str) -> String {
        let disk_name = DiskName::parse(name).expect("valid test disk name");
        let (_, m) = members
            .by_name(&disk_name)
            .unwrap_or_else(|| panic!("disk '{name}' should be discovered: {members:?}"));
        m.by_id.as_str().to_owned()
    }

    /// Test-local helper: does the discovered membership contain a
    /// member under the given disk name?
    fn contains_name(members: &PoolMembership, name: &str) -> bool {
        let disk_name = match DiskName::parse(name) {
            Ok(n) => n,
            Err(_) => return false,
        };
        members.by_name(&disk_name).is_some()
    }

    fn member(name: &str, by_id: &str) -> DiskMember {
        DiskMember::new(
            DiskName::parse(name).expect("valid test disk name"),
            ByIdPath::parse(by_id).expect("valid test by-id path"),
        )
    }

    fn stale_luks_uuid_corrupt_pool_json() -> String {
        r#"{"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/ata-old","luks_uuid":"22222222-2222-2222-2222-222222222222","devid":1}}}"#
            .to_owned()
    }

    fn discovered_members(names: &[&str]) -> PoolMembership {
        let mut members = PoolMembership::empty();
        for (i, name) in names.iter().enumerate() {
            members
                .insert(
                    LuksUuid::parse(&format!("aaaaaaaa-bbbb-cccc-dddd-{:012x}", i as u64 + 1))
                        .unwrap(),
                    member(name, &format!("/dev/disk/by-id/ata-{name}")),
                )
                .unwrap();
        }
        members
    }

    fn corrupt_sidecars(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                let name = path.file_name()?.to_str()?;
                name.starts_with("pool.json.corrupt-").then_some(path)
            })
            .collect();
        entries.sort();
        entries
    }

    fn is_pool_json_corrupt_rfc3339_sidecar(path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        let Some(ts) = name.strip_prefix("pool.json.corrupt-") else {
            return false;
        };
        let bytes = ts.as_bytes();
        bytes.len() == 20
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes[10] == b'T'
            && bytes[13] == b':'
            && bytes[16] == b':'
            && bytes[19] == b'Z'
            && bytes
                .iter()
                .enumerate()
                .all(|(i, b)| matches!(i, 4 | 7 | 10 | 13 | 16 | 19) || b.is_ascii_digit())
    }

    // Intent: discover preview lines are returned in DiskName order
    //   regardless of underlying UUID order.
    // Why it exists: a previous regression printed the preview in UUID
    //   order, contradicting decision 024. This pins the call-site helper.
    // Scenario: two discovered members whose UUID order is opposite name
    //   order; operator expects alphabetical preview rows.
    #[test]
    fn render_preview_lines_returns_name_sorted_independent_of_uuid_order() {
        let mut members = PoolMembership::empty();
        members
            .insert(
                LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                member("zeta", "/dev/disk/by-id/ata-Z"),
            )
            .unwrap();
        members
            .insert(
                LuksUuid::parse("99999999-9999-9999-9999-999999999999").unwrap(),
                member("alpha", "/dev/disk/by-id/ata-A"),
            )
            .unwrap();
        assert_eq!(
            render_preview_lines(&members),
            vec![
                "  alpha = /dev/disk/by-id/ata-A",
                "  zeta = /dev/disk/by-id/ata-Z"
            ]
        );
    }

    #[test]
    fn discover_propagates_runner_error_at_isluks() {
        /*
         * Intent: a CmdError from the isLuks runner call bubbles up as
         *   DiscoverError::Cmd, not silently swallowed as "no labeled disks found".
         * Why it exists: runner-level failures and the legitimate per-entry
         *   "not LUKS" signal must not be conflated. This pins propagation
         *   at the first command site.
         * Scenario: a developer runs the CLI outside its NixOS context and
         *   cryptsetup is not on PATH; discover should fail loudly with a
         *   spawn-error message, not silently report no disks.
         */
        struct IsLuksFailRunner;

        impl CommandRunner for IsLuksFailRunner {
            fn run(&self, _req: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
                Err(CmdError::Failed(
                    "cryptsetup: No such file or directory (os error 2)".into(),
                ))
            }

            fn run_with_stdin(
                &self,
                req: &CmdRequest,
                _stdin: &[u8],
            ) -> Result<RawCommandOutput, CmdError> {
                self.run(req)
            }
        }

        let by_id_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let target = discover_create_target(target_dir.path(), "fake-disk");
        discover_create_by_id_symlink(by_id_dir.path(), "ata-SOMEDISK", &target);

        let scan = discover_from_dir(&IsLuksFailRunner, &RealByIdResolver, by_id_dir.path());
        let err = scan.result.unwrap_err();

        assert!(
            matches!(err, DiscoverError::Cmd(_)),
            "expected DiscoverError::Cmd from isLuks failure, got {err:?}",
        );
    }

    #[test]
    fn discover_propagates_runner_error_at_luksdump() {
        /*
         * Intent: a CmdError from the luksDump runner call bubbles up as
         *   DiscoverError::Cmd after isLuks succeeds.
         * Why it exists: a fail-at-first-call test does not exercise the
         *   second command site, so this separately pins the luksDump
         *   propagation path.
         * Scenario: cryptsetup spawns successfully for isLuks but fails for
         *   luksDump, such as a transient I/O error on the second invocation.
         */
        struct LuksDumpFailRunner;

        impl CommandRunner for LuksDumpFailRunner {
            fn run(&self, req: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
                match req {
                    CmdRequest::CryptsetupIsLuks { .. } => Ok(RawCommandOutput {
                        cmd: "cryptsetup".into(),
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_status: 0,
                    }),
                    CmdRequest::CryptsetupLuksDumpText { .. } => {
                        Err(CmdError::Failed("cryptsetup luksDump: I/O error".into()))
                    }
                    _ => Err(CmdError::MissingMock),
                }
            }

            fn run_with_stdin(
                &self,
                req: &CmdRequest,
                _stdin: &[u8],
            ) -> Result<RawCommandOutput, CmdError> {
                self.run(req)
            }
        }

        let by_id_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let target = discover_create_target(target_dir.path(), "fake-disk");
        discover_create_by_id_symlink(by_id_dir.path(), "ata-SOMEDISK", &target);

        let scan = discover_from_dir(&LuksDumpFailRunner, &RealByIdResolver, by_id_dir.path());
        let err = scan.result.unwrap_err();

        assert!(
            matches!(err, DiscoverError::Cmd(_)),
            "expected DiscoverError::Cmd from luksDump failure, got {err:?}",
        );
    }

    #[test]
    fn non_luks_device_never_reaches_luks_dump() {
        /*
         * Intent: the isLuks gate must prevent non-LUKS devices from reaching luksDump.
         * Why it exists: the gate checked .is_err() instead of exit status, making it
         *   a no-op — non-LUKS devices leaked through to luksDump and were only caught
         *   downstream by the parser.
         * Scenario: a NAS has both LUKS-encrypted braid drives and a non-LUKS device
         *   (e.g. a USB stick) in /dev/disk/by-id/. Discovery should never call
         *   luksDump on the non-LUKS device.
         */
        let dir = tempfile::tempdir().unwrap();
        let luks_target = discover_create_target(dir.path(), "fake-sda");
        let usb_target = discover_create_target(dir.path(), "fake-usb");
        let luks_path =
            discover_create_by_id_symlink(dir.path(), "ata-TOSHIBA_BRAID", &luks_target);
        discover_create_by_id_symlink(dir.path(), "ata-USB_STICK", &usb_target);

        // Only the LUKS device is in the label map; the USB stick is unknown.
        let runner = DiscoverLabelMap::new(&[(&luks_path, "braid-sda")]);
        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        scan.result.unwrap();
        assert!(
            scan.warnings.is_empty(),
            "unexpected warnings: {:?}",
            scan.warnings
        );

        let luks_dump_calls: Vec<_> = runner
            .calls()
            .into_iter()
            .filter(|(cmd, _)| cmd == "luksDump")
            .collect();

        assert!(
            luks_dump_calls.iter().all(|(_, dev)| dev == &luks_path),
            "luksDump was called for a non-LUKS device: {:?}",
            luks_dump_calls,
        );
    }

    #[test]
    fn discover_warns_when_labeled_disk_fails_luksdump() {
        /*
         * Intent: a braid-labeled disk whose luksDump command reports a
         *   device/header failure is surfaced as a structured warning.
         * Why it exists: discover used to silently drop the device, leaving
         *   recovery users with only the misleading "no labeled disks" summary
         *   when the broken device was the only candidate.
         * Scenario: one healthy disk and one present-but-broken disk are both
         *   visible under /dev/disk/by-id during pool recovery.
         */
        let dir = tempfile::tempdir().unwrap();
        let modern_target = discover_create_target(dir.path(), "fake-sda");
        let broken_target = discover_create_target(dir.path(), "fake-sdb");
        let modern_path =
            discover_create_by_id_symlink(dir.path(), "ata-MODERN_DISK", &modern_target);
        let broken_path =
            discover_create_by_id_symlink(dir.path(), "ata-BROKEN_DISK", &broken_target);
        let runner = DiscoverLabelMap::new(&[
            (&modern_path, "braid-modern"),
            (&broken_path, "braid-broken"),
        ])
        .with_dump_response(
            &broken_path,
            RawCommandOutput {
                cmd: "cryptsetup".into(),
                stdout: String::new(),
                stderr: "Device /dev/foo is not a valid LUKS device.\n".into(),
                exit_status: 1,
            },
        );

        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let members = scan.result.unwrap();

        assert_eq!(members.len(), 1);
        assert!(
            contains_name(&members, "modern"),
            "modern disk should be discovered: {:?}",
            members
        );
        assert_eq!(scan.warnings.len(), 1);
        let warning = &scan.warnings[0];
        assert!(matches!(
            warning,
            DiscoverWarning::LuksDumpFailed { exit_code: 1, .. }
        ));
        let DiscoverWarning::LuksDumpFailed { path, stderr, .. } = warning else {
            unreachable!();
        };
        assert!(path.ends_with("ata-BROKEN_DISK"), "path was {path}");
        assert!(
            stderr.contains("not a valid LUKS device"),
            "stderr was {stderr:?}"
        );
    }

    #[test]
    fn discover_warns_on_unparseable_luksdump_output() {
        /*
         * Intent: a successful luksDump command whose output does not match
         *   braid's parser contract is reported separately from command
         *   failure.
         * Why it exists: parser drift and cryptsetup rejecting a header point
         *   to different fixes, so discover must not collapse them into one
         *   warning kind.
         * Scenario: cryptsetup exits successfully but omits the Version field
         *   that discover requires to enforce the LUKS2-only invariant.
         */
        let dir = tempfile::tempdir().unwrap();
        let target = discover_create_target(dir.path(), "fake-sda");
        let path = discover_create_by_id_symlink(dir.path(), "ata-ODD_DISK", &target);
        let runner = DiscoverLabelMap::new(&[(&path, "braid-odd")]).with_dump_response(
            &path,
            RawCommandOutput {
                cmd: "cryptsetup".into(),
                stdout: "LUKS header information\nUUID: foo\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );

        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let members = scan.result.unwrap();

        assert!(members.is_empty());
        assert_eq!(scan.warnings.len(), 1);
        let DiscoverWarning::LuksDumpUnparseable { path, detail } = &scan.warnings[0] else {
            panic!("expected LuksDumpUnparseable, got {:?}", scan.warnings[0]);
        };
        assert!(path.ends_with("ata-ODD_DISK"), "path was {path}");
        assert!(detail.contains("Version"), "detail was {detail}");
    }

    #[test]
    fn discover_prefers_wwn_over_ata() {
        /*
         * Intent: verify that discover selects the wwn- symlink when both wwn- and ata-
         *   symlinks exist for the same disk.
         * Why it exists: discover previously used last-wins BTreeMap insertion, so
         *   read_dir() order determined which symlink was stored; this was non-deterministic
         *   across reboots and caused pool.json to desync with discover output.
         * Scenario: a SATA drive has both wwn-0xABCD and ata-SEAGATE_XXXXX in
         *   /dev/disk/by-id/; `braid discover --write` should always record the wwn- path,
         *   not whichever the filesystem happened to return last.
         */
        let dir = tempfile::tempdir().unwrap();
        let target = discover_create_target(dir.path(), "fake-sda");
        let ata_path = discover_create_by_id_symlink(dir.path(), "ata-SEAGATE_ST500", &target);
        let wwn_path = discover_create_by_id_symlink(dir.path(), "wwn-0x50014ee606704442", &target);
        let runner = DiscoverLabelMap::new(&[(&ata_path, "braid-sda"), (&wwn_path, "braid-sda")]);
        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let members = scan.result.unwrap();
        assert!(
            scan.warnings.is_empty(),
            "unexpected warnings: {:?}",
            scan.warnings
        );
        assert_eq!(members.len(), 1);
        let sda = by_id_for(&members, "sda");
        assert!(
            sda.ends_with("wwn-0x50014ee606704442"),
            "expected wwn path, got: {sda}"
        );
    }

    #[test]
    fn discover_same_priority_breaks_ties_lexicographically() {
        /*
         * Intent: verify that when two symlinks share the same priority class, discover
         *   picks the lexicographically earlier filename rather than the last one seen.
         * Why it exists: read_dir() order is unspecified even within the same prefix class;
         *   without tie-breaking, two ata- aliases for the same drive would still flap
         *   across reboots.
         * Scenario: after a kernel upgrade that reformats the ata- name slightly, a drive
         *   transiently has two ata- symlinks; discover should consistently return the
         *   alphabetically earlier one.
         */
        let dir = tempfile::tempdir().unwrap();
        let target = discover_create_target(dir.path(), "fake-sda");
        let ata_z = discover_create_by_id_symlink(dir.path(), "ata-ZZZZZ_DISK", &target);
        let ata_a = discover_create_by_id_symlink(dir.path(), "ata-AAAAA_DISK", &target);
        let runner = DiscoverLabelMap::new(&[(&ata_z, "braid-sda"), (&ata_a, "braid-sda")]);
        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let members = scan.result.unwrap();
        assert!(
            scan.warnings.is_empty(),
            "unexpected warnings: {:?}",
            scan.warnings
        );
        assert_eq!(members.len(), 1);
        let sda = by_id_for(&members, "sda");
        assert!(
            sda.ends_with("ata-AAAAA_DISK"),
            "expected lexicographically earlier path, got: {sda}"
        );
    }

    #[test]
    fn discover_skips_luks1_disk() {
        /*
         * Intent: a braid-labeled LUKS1 disk must NOT be written into the
         *   discovered membership map. The version check at this gateway
         *   prevents `braid discover --write` from persisting an
         *   unsupported disk into pool.json.
         * Why it exists: this is the discovery-side counterpart to the
         *   probe_config_disk gateway check. Without it, dropping
         *   `--type luks2` from CryptsetupIsLuks (which is necessary to
         *   stop probe_luks_header from misclassifying LUKS1 as
         *   "Unreadable") would silently allow LUKS1 disks into pool.json
         *   instead of being filtered upstream.
         * Scenario: a user has a single braid-labeled LUKS1 disk
         *   (perhaps externally formatted) plugged in alongside a normal
         *   LUKS2 braid disk; only the LUKS2 disk should be discovered.
         */
        let dir = tempfile::tempdir().unwrap();
        let luks1_target = discover_create_target(dir.path(), "fake-sda");
        let luks2_target = discover_create_target(dir.path(), "fake-sdb");
        let luks1_path =
            discover_create_by_id_symlink(dir.path(), "ata-LEGACY_DISK", &luks1_target);
        let luks2_path =
            discover_create_by_id_symlink(dir.path(), "ata-MODERN_DISK", &luks2_target);
        let runner =
            DiscoverLabelMap::new(&[(&luks1_path, "braid-legacy"), (&luks2_path, "braid-modern")])
                .with_version(&luks1_path, 1);
        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let members = scan.result.unwrap();
        assert_eq!(
            members.len(),
            1,
            "expected only the LUKS2 disk: {members:?}"
        );
        assert!(
            contains_name(&members, "modern"),
            "modern (LUKS2) disk should be present: {members:?}"
        );
        assert!(
            !contains_name(&members, "legacy"),
            "legacy (LUKS1) disk should be skipped: {members:?}"
        );
        assert_eq!(scan.warnings.len(), 1);
        assert!(matches!(
            &scan.warnings[0],
            DiscoverWarning::UnsupportedLuksVersion { path, version: 1 }
                if path.ends_with("ata-LEGACY_DISK")
        ));
    }

    #[test]
    fn discover_warns_on_invalid_disk_name_in_braid_label() {
        /*
         * Intent: a `braid-<NAME>` label whose <NAME> fails
         *   is_valid_disk_name (leading digit, embedded space, > 32 chars,
         *   non-ASCII) must produce a structured InvalidDiskName warning
         *   AND be absent from the discovered members. Either alone is
         *   insufficient: absence already happens via short-circuit in the
         *   if-let chain (the prior, broken behavior), so the warning push
         *   is what the test pins.
         * Why it exists: the rejection used to be silent and
         *   indistinguishable from "this isn't a braid disk." A user who
         *   externally formats a drive with a malformed name would see
         *   nothing in `braid discover` output. The new warning routes
         *   through main.rs -> Display impl, surfacing the label and path
         *   so the user can relabel.
         * Scenario: an admin runs `cryptsetup luksFormat --label braid-é`
         *   on a new disk (non-ASCII name -- cryptsetup accepts any UTF-8
         *   string up to 47 bytes), plugs it in alongside a properly
         *   formatted braid disk, and runs `braid discover`. Only the
         *   valid disk should appear in members; the malformed one must
         *   produce one InvalidDiskName warning whose rendered Display
         *   form escapes the non-ASCII byte.
         */
        let dir = tempfile::tempdir().unwrap();
        let bad_target = discover_create_target(dir.path(), "fake-bad");
        let good_target = discover_create_target(dir.path(), "fake-good");
        let bad_path = discover_create_by_id_symlink(dir.path(), "ata-BAD_LABEL", &bad_target);
        let good_path = discover_create_by_id_symlink(dir.path(), "ata-GOOD_LABEL", &good_target);
        let runner = DiscoverLabelMap::new(&[(&bad_path, "braid-é"), (&good_path, "braid-good")]);

        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let members = scan.result.unwrap();

        assert_eq!(
            members.len(),
            1,
            "expected only the valid disk: {:?}",
            members,
        );
        assert!(
            contains_name(&members, "good"),
            "good disk should be discovered: {:?}",
            members,
        );
        assert!(
            !members.iter().any(|(_, m)| m.name.as_str().contains("é")),
            "invalid name must not be recorded: {:?}",
            members,
        );

        assert_eq!(scan.warnings.len(), 1);
        let DiscoverWarning::InvalidDiskName { path, label } = &scan.warnings[0] else {
            panic!("expected InvalidDiskName, got {:?}", scan.warnings[0]);
        };
        assert!(path.ends_with("ata-BAD_LABEL"), "path was {path}");
        assert_eq!(label, "braid-é");

        // Pin the escape_default() choice: non-ASCII characters in the
        // label must be rendered as \u{...} escapes in the user-facing
        // warning. If someone replaces escape_default() with {:?} (Debug)
        // or {} (raw), the printable non-ASCII byte survives verbatim and
        // this assertion fails.
        let rendered = scan.warnings[0].to_string();
        assert!(
            rendered.contains("\"braid-\\u{e9}\""),
            "expected escape_default rendering of non-ASCII label, got: {rendered:?}",
        );
    }

    #[test]
    fn discover_selects_best_symlink_per_disk_independently() {
        /*
         * Intent: verify that each disk in a multi-disk pool independently gets its
         *   best-priority symlink.
         * Why it exists: the preference logic operates per disk-name key; a bug could
         *   incorrectly share state across disks or only apply the preference to the first
         *   disk seen.
         * Scenario: a three-drive NAS where every drive has both a wwn- and an ata- entry;
         *   braid discover should return wwn- for every disk, not a mix.
         */
        let dir = tempfile::tempdir().unwrap();
        let alpha_target = discover_create_target(dir.path(), "fake-disk1");
        let beta_target = discover_create_target(dir.path(), "fake-disk2");
        let ata_alpha = discover_create_by_id_symlink(dir.path(), "ata-DISK1_ALPHA", &alpha_target);
        let wwn_alpha = discover_create_by_id_symlink(dir.path(), "wwn-0x0001", &alpha_target);
        let ata_beta = discover_create_by_id_symlink(dir.path(), "ata-DISK2_BETA", &beta_target);
        let wwn_beta = discover_create_by_id_symlink(dir.path(), "wwn-0x0002", &beta_target);
        let runner = DiscoverLabelMap::new(&[
            (&ata_alpha, "braid-alpha"),
            (&wwn_alpha, "braid-alpha"),
            (&ata_beta, "braid-beta"),
            (&wwn_beta, "braid-beta"),
        ]);
        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let members = scan.result.unwrap();
        assert!(
            scan.warnings.is_empty(),
            "unexpected warnings: {:?}",
            scan.warnings
        );
        assert_eq!(members.len(), 2);
        let alpha = by_id_for(&members, "alpha");
        assert!(
            alpha.ends_with("wwn-0x0001"),
            "expected wwn for alpha, got: {alpha}"
        );
        let beta = by_id_for(&members, "beta");
        assert!(
            beta.ends_with("wwn-0x0002"),
            "expected wwn for beta, got: {beta}"
        );
    }

    #[test]
    fn discover_fails_on_label_collision_across_disks() {
        /*
         * Intent: two distinct physical devices that both carry the same
         *   braid-<name> LUKS label must produce a hard discovery error.
         * Why it exists: the priority tie-break only applies to aliases for
         *   one disk. After a dd clone or manual mislabel, silently dropping
         *   one distinct device would write incomplete pool membership.
         * Scenario: admin clones a working braid disk to a spare and forgets
         *   to relabel it before the next `braid discover` run.
         */
        let dir = tempfile::tempdir().unwrap();
        let target_a = discover_create_target(dir.path(), "fake-sda");
        let target_b = discover_create_target(dir.path(), "fake-sdb");
        let alias_a = discover_create_by_id_symlink(dir.path(), "ata-CLONE_A", &target_a);
        let alias_b = discover_create_by_id_symlink(dir.path(), "ata-CLONE_B", &target_b);
        let runner = DiscoverLabelMap::new(&[(&alias_a, "braid-foo"), (&alias_b, "braid-foo")]);

        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let err = scan.result.unwrap_err();

        match &err {
            DiscoverError::LabelCollision { name, path1, path2 } => {
                assert_eq!(name, "foo");
                let pair = [path1.as_str(), path2.as_str()];
                assert!(
                    pair.iter().any(|path| path.ends_with("ata-CLONE_A"))
                        && pair.iter().any(|path| path.ends_with("ata-CLONE_B")),
                    "collision must reference both aliases: {pair:?}",
                );
            }
            other => panic!("expected LabelCollision, got {other:?}"),
        }

        let msg = err.to_string();
        assert!(msg.contains("braid-foo"), "missing label name: {msg}");
        assert!(msg.contains("ata-CLONE_A"), "missing alias_a: {msg}");
        assert!(msg.contains("ata-CLONE_B"), "missing alias_b: {msg}");
    }

    // Intent: warnings accumulated during the scan survive a structural
    //   error return so the operator sees all sibling hazards in one pass.
    // Why it exists: every `return Err(...)` inside discover used to drop
    //   the warning vec, and the LabelCollision early-return inside the
    //   entry loop additionally skipped warnings that later entries would
    //   have produced. Fixing both paths requires a test that pins both
    //   guarantees: warnings survive the error return, and warnings from
    //   entries scanned after the collision still appear.
    // Scenario: multi-disk recovery -- the operator has a dangling by-id
    //   symlink, a LUKS1 leftover, and two distinct disks sharing
    //   `braid-foo`; `braid discover` must report all three hazards so they
    //   can be addressed before retry.
    #[test]
    fn discover_surfaces_warnings_alongside_structural_error() {
        let dir = tempfile::tempdir().unwrap();
        discover_create_by_id_symlink(dir.path(), "ata-DANGLING", "/nonexistent/dangling");

        let luks1_target = discover_create_target(dir.path(), "fake-luks1");
        let luks1_alias = discover_create_by_id_symlink(dir.path(), "ata-LUKS1", &luks1_target);

        let target_a = discover_create_target(dir.path(), "fake-sda");
        let target_b = discover_create_target(dir.path(), "fake-sdb");
        let alias_a = discover_create_by_id_symlink(dir.path(), "ata-CLONE_A", &target_a);
        let alias_b = discover_create_by_id_symlink(dir.path(), "ata-CLONE_B", &target_b);
        let runner = DiscoverLabelMap::new(&[
            (&luks1_alias, "braid-legacy"),
            (&alias_a, "braid-foo"),
            (&alias_b, "braid-foo"),
        ])
        .with_version(&luks1_alias, 1);

        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());

        assert!(
            matches!(&scan.result, Err(DiscoverError::LabelCollision { .. })),
            "expected LabelCollision, got {:?}",
            &scan.result,
        );
        assert!(
            scan.warnings.iter().any(|warning| matches!(
                warning,
                DiscoverWarning::CannotCanonicalize { path, .. }
                    if path.ends_with("ata-DANGLING")
            )),
            "expected CannotCanonicalize warning, got: {:?}",
            scan.warnings,
        );
        assert!(
            scan.warnings.iter().any(|warning| matches!(
                warning,
                DiscoverWarning::UnsupportedLuksVersion { path, version: 1 }
                    if path.ends_with("ata-LUKS1")
            )),
            "expected UnsupportedLuksVersion warning, got: {:?}",
            scan.warnings,
        );
    }

    // Intent: drain_warnings writes every warning to `out` before
    //   returning, even when `scan.result` is `Err`.
    // Why it exists: pins the CLI's "warnings before error" stderr ordering
    //   at the helper boundary so it cannot silently regress to printing
    //   the structural error first and the warnings never.
    // Scenario: any structural error surfaced after warnings accumulated
    //   (label collision, duplicate uuid, ...). The unit test passes a
    //   synthetic DiscoverScan rather than driving discover_from_dir so it
    //   stays a contract test of the helper, not of the scan.
    #[test]
    fn drain_warnings_writes_warnings_before_returning_error() {
        let scan = DiscoverScan {
            warnings: vec![
                DiscoverWarning::CannotCanonicalize {
                    path: "/dev/disk/by-id/ata-DANGLING".into(),
                    detail: "no such file".into(),
                },
                DiscoverWarning::UnsupportedLuksVersion {
                    path: "/dev/disk/by-id/ata-LEGACY".into(),
                    version: 1,
                },
            ],
            result: Err(DiscoverError::LabelCollision {
                name: "foo".into(),
                path1: "/dev/disk/by-id/ata-A".into(),
                path2: "/dev/disk/by-id/ata-B".into(),
            }),
        };

        let mut buf: Vec<u8> = Vec::new();
        let err = drain_warnings(scan, &mut buf).expect_err("expected Err");
        let out = String::from_utf8(buf).unwrap();

        assert!(
            out.contains("ata-DANGLING"),
            "missing dangling warning: {out}"
        );
        assert!(out.contains("ata-LEGACY"), "missing legacy warning: {out}");
        assert!(matches!(err, DiscoverError::LabelCollision { .. }));
    }

    #[test]
    fn discover_warns_on_dangling_symlink_with_no_luks_device() {
        /*
         * Intent: a dangling by-id symlink with no underlying LUKS device
         *   produces a single CannotCanonicalize warning and no member.
         * Why it exists: the LUKS probe used to fail silently on dangling
         *   symlinks, so operators saw no diagnostic when udev left a stale
         *   alias behind. Pinning canonicalize ahead of the probe makes the
         *   warning fire on structural by-id problems regardless of LUKS
         *   state.
         * Scenario: after a disk swap, udev failed to clean up the prior
         *   drive's /dev/disk/by-id/ata-OLD_DRIVE symlink; the operator runs
         *   `braid discover` and expects to see why the entry is being skipped.
         */
        let dir = tempfile::tempdir().unwrap();
        discover_create_by_id_symlink(
            dir.path(),
            "ata-DANGLING_OLD",
            "/nonexistent/dangling/target",
        );
        let runner = DiscoverLabelMap::new(&[]);

        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let members = scan.result.unwrap();

        assert!(members.is_empty());
        assert_eq!(scan.warnings.len(), 1);
        assert!(matches!(
            &scan.warnings[0],
            DiscoverWarning::CannotCanonicalize { path, .. }
                if path.ends_with("ata-DANGLING_OLD")
        ));
    }

    #[test]
    fn discover_skips_entry_when_canonicalize_fails() {
        /*
         * Intent: a by-id symlink whose canonicalize fails is skipped with a
         *   warning instead of aborting discovery.
         * Why it exists: the by-id structural gate must reject a broken
         *   symlink before any LUKS probing or alias collision detection can
         *   treat it as a usable pool candidate.
         * Scenario: udev leaves a stale by-id symlink after a transient disk
         *   detach; discover still records the remaining valid member.
         */
        let dir = tempfile::tempdir().unwrap();
        let target = discover_create_target(dir.path(), "fake-sda");
        let dangling = discover_create_by_id_symlink(
            dir.path(),
            "ata-DANGLING",
            "/nonexistent/dangling/target",
        );
        let valid = discover_create_by_id_symlink(dir.path(), "wwn-VALID", &target);
        let runner = DiscoverLabelMap::new(&[(&dangling, "braid-foo"), (&valid, "braid-foo")]);

        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let members = scan.result.unwrap();

        assert_eq!(members.len(), 1, "expected only the canonicalizable entry");
        let foo = by_id_for(&members, "foo");
        assert!(
            foo.ends_with("wwn-VALID"),
            "expected the valid symlink to win, got: {foo}"
        );
        assert_eq!(scan.warnings.len(), 1);
        assert!(matches!(
            &scan.warnings[0],
            DiscoverWarning::CannotCanonicalize { path, .. }
                if path.ends_with("ata-DANGLING")
        ));
    }

    #[test]
    fn label_collision_sorts_paths_lexicographically() {
        /*
         * Intent: LabelCollision reports path1/path2 in lexicographic order
         *   regardless of which path was encountered first.
         * Why it exists: read_dir ordering is unspecified, so the helper owns
         *   deterministic error ordering independently of integration tests.
         * Scenario: repeated scans of the same collision produce stable
         *   output between runs and reboots.
         */
        let a = "/dev/disk/by-id/ata-AAA".to_owned();
        let z = "/dev/disk/by-id/ata-ZZZ".to_owned();

        for (incumbent, candidate) in [(a.clone(), z.clone()), (z.clone(), a.clone())] {
            let err = label_collision("foo", incumbent.clone(), candidate.clone());
            match err {
                DiscoverError::LabelCollision { name, path1, path2 } => {
                    assert_eq!(name, "foo");
                    assert_eq!(path1, a, "(incumbent={incumbent}, candidate={candidate})");
                    assert_eq!(path2, z, "(incumbent={incumbent}, candidate={candidate})");
                }
                other => panic!("expected LabelCollision, got {other:?}"),
            }
        }
    }

    // -- Migration-Phase-4 tests ----------------------------------------

    /// Build a synthetic luksDump body for negative-UUID tests. The
    /// version and label fields are present so the parser reaches the
    /// UUID-extraction step; the UUID line is controlled by the caller.
    fn luksdump_body(label: &str, uuid_line: Option<&str>) -> String {
        let mut body = String::from("LUKS header information\nVersion:\t2\n");
        if let Some(line) = uuid_line {
            body.push_str(line);
            if !line.ends_with('\n') {
                body.push('\n');
            }
        }
        body.push_str("Label:\t");
        body.push_str(label);
        body.push('\n');
        body
    }

    // Intent: a braid-labeled LUKS2 disk whose luksDump body has no
    //   `UUID:` line at all is skipped and surfaced as
    //   DiscoverWarning::LuksDumpUnparseable (the parser-drift bucket),
    //   never silently admitted to membership.
    // Why it exists: real LUKS2 output always carries the `UUID:` line
    //   (reference/cryptsetup/lib/luks2/luks2_json_metadata.c#LUKS2_hdr_dump
    //   emits it unconditionally), so an absent line means upstream format
    //   drift -- a future cryptsetup that renames or removes the field.
    //   This guards against silently admitting an identity-less disk on
    //   that drift, routing the absent `UUID:` line exactly as the absent
    //   `Version:` line already routes (into LuksDumpUnparseable).
    // Scenario: a hypothetical cryptsetup release drops the `UUID:` line
    //   from luksDump; discover must skip the disk with a structured
    //   unparseable warning rather than reconstruct membership without an
    //   identity.
    #[test]
    fn discover_treats_absent_uuid_line_as_unparseable() {
        let dir = tempfile::tempdir().unwrap();
        let target = discover_create_target(dir.path(), "fake-bad");
        let path = discover_create_by_id_symlink(dir.path(), "ata-MISSING_UUID", &target);
        let runner = DiscoverLabelMap::new(&[(&path, "braid-baddisk")]).with_dump_response(
            &path,
            RawCommandOutput {
                cmd: "cryptsetup".into(),
                stdout: luksdump_body("braid-baddisk", None),
                stderr: String::new(),
                exit_status: 0,
            },
        );

        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let members = scan.result.unwrap();

        assert!(
            !contains_name(&members, "baddisk"),
            "disk with missing UUID must not be a member"
        );
        let warning = scan
            .warnings
            .iter()
            .find(|w| matches!(w, DiscoverWarning::LuksDumpUnparseable { .. }))
            .expect("LuksDumpUnparseable warning expected");
        let DiscoverWarning::LuksDumpUnparseable {
            path: warn_path,
            detail,
        } = warning
        else {
            unreachable!();
        };
        assert!(warn_path.ends_with("ata-MISSING_UUID"));
        assert!(detail.contains("UUID"), "detail was {detail}");
    }

    // Intent: a braid-labeled LUKS2 disk whose luksDump prints the literal
    //   `(no UUID)` sentinel surfaces as DiscoverWarning::InvalidLuksUuid
    //   carrying that raw text, and the disk is absent from members.
    // Why it exists: cryptsetup's LUKS2_hdr_dump prints `(no UUID)` when
    //   the in-memory header UUID field is empty
    //   (reference/cryptsetup/lib/luks2/luks2_json_metadata.c#LUKS2_hdr_dump:
    //   `*hdr->uuid ? hdr->uuid : "(no UUID)"`). This is the reachable
    //   empty-UUID case for a loadable LUKS2 header; braid must reject the
    //   sentinel and skip the disk, never admit an identity-less disk to
    //   membership. How the field came to be empty is unproven and not the
    //   point -- only the dump-time print is.
    // Scenario: cryptsetup loads a LUKS2 header whose binary UUID field is
    //   empty, so luksDump emits `UUID:\t(no UUID)`; discover warns and
    //   skips the disk.
    #[test]
    fn discover_warns_when_header_uuid_is_no_uuid_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let target = discover_create_target(dir.path(), "fake-bad");
        let path = discover_create_by_id_symlink(dir.path(), "ata-NO_UUID_SENTINEL", &target);
        let runner = DiscoverLabelMap::new(&[(&path, "braid-baddisk")]).with_dump_response(
            &path,
            RawCommandOutput {
                cmd: "cryptsetup".into(),
                stdout: luksdump_body("braid-baddisk", Some("UUID:\t(no UUID)")),
                stderr: String::new(),
                exit_status: 0,
            },
        );

        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let members = scan.result.unwrap();

        assert!(!contains_name(&members, "baddisk"));
        let warning = scan
            .warnings
            .iter()
            .find(|w| matches!(w, DiscoverWarning::InvalidLuksUuid { .. }))
            .expect("InvalidLuksUuid warning expected");
        let DiscoverWarning::InvalidLuksUuid {
            path: warn_path,
            raw,
            ..
        } = warning
        else {
            unreachable!();
        };
        assert!(warn_path.ends_with("ata-NO_UUID_SENTINEL"));
        assert_eq!(raw, "(no UUID)");
    }

    /// Intent: a braid-labeled LUKS2 disk whose UUID line carries text
    /// LuksUuid::parse rejects surfaces as DiscoverWarning::InvalidLuksUuid
    /// carrying the raw text.
    /// Why: the invalid-UUID warning path must keep the offending raw
    /// value visible for operator diagnostics.
    /// Scenario: a header has been corrupted to an unparseable UUID
    /// string (seed 801).
    #[test]
    fn discover_warns_when_uuid_unparseable() {
        let dir = tempfile::tempdir().unwrap();
        let target = discover_create_target(dir.path(), "fake-bad");
        let path = discover_create_by_id_symlink(dir.path(), "ata-INVALID_UUID", &target);
        let runner = DiscoverLabelMap::new(&[(&path, "braid-baddisk")]).with_dump_response(
            &path,
            RawCommandOutput {
                cmd: "cryptsetup".into(),
                stdout: luksdump_body("braid-baddisk", Some("UUID:\tnot-a-uuid")),
                stderr: String::new(),
                exit_status: 0,
            },
        );

        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let members = scan.result.unwrap();

        assert!(!contains_name(&members, "baddisk"));
        let warning = scan
            .warnings
            .iter()
            .find(|w| matches!(w, DiscoverWarning::InvalidLuksUuid { .. }))
            .expect("InvalidLuksUuid warning expected");
        let DiscoverWarning::InvalidLuksUuid {
            path: warn_path,
            raw,
            ..
        } = warning
        else {
            unreachable!();
        };
        assert!(warn_path.ends_with("ata-INVALID_UUID"));
        assert_eq!(raw, "not-a-uuid");
        let rendered = warning.to_string();
        assert!(
            rendered.starts_with(&format!(
                "skipping {warn_path}: invalid LUKS UUID \"{raw}\" --"
            )),
            "rendered: {rendered}",
        );
    }

    // Intent: discover surfaces an invalid UUID value containing the
    //   literal " (" substring with raw and detail intact in
    //   DiscoverWarning::InvalidLuksUuid.
    // Why it exists: an earlier implementation reverse-split a formatted
    //   "<raw> (<detail>)" string on " (" inside discover; if raw itself
    //   contained " (", the warning showed a truncated raw (for example,
    //   "not" for input "not (a uuid)") and a malformed detail.
    // Scenario: a luksDump whose UUID: line reads "not (a uuid)" -- the " ("
    //   between "not" and "(a uuid)" is the exact delimiter the old split
    //   matched first.
    #[test]
    fn discover_warns_when_uuid_value_contains_split_delimiter() {
        let dir = tempfile::tempdir().unwrap();
        let target = discover_create_target(dir.path(), "fake-bad");
        let path = discover_create_by_id_symlink(dir.path(), "ata-INVALID_UUID_PAREN", &target);
        let runner = DiscoverLabelMap::new(&[(&path, "braid-baddisk")]).with_dump_response(
            &path,
            RawCommandOutput {
                cmd: "cryptsetup".into(),
                stdout: luksdump_body("braid-baddisk", Some("UUID:\tnot (a uuid)")),
                stderr: String::new(),
                exit_status: 0,
            },
        );

        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let members = scan.result.unwrap();

        assert!(!contains_name(&members, "baddisk"));
        let warning = scan
            .warnings
            .iter()
            .find(|w| matches!(w, DiscoverWarning::InvalidLuksUuid { .. }))
            .expect("InvalidLuksUuid warning expected");
        let DiscoverWarning::InvalidLuksUuid {
            path: warn_path,
            raw,
            detail,
        } = warning
        else {
            unreachable!();
        };
        assert!(warn_path.ends_with("ata-INVALID_UUID_PAREN"));
        assert_eq!(raw, "not (a uuid)");
        assert_ne!(raw, "not");
        assert!(!detail.is_empty(), "detail must carry uuid-crate reason");

        let rendered = warning.to_string();
        assert_eq!(
            rendered.matches("not (a uuid)").count(),
            1,
            "rendered: {rendered}"
        );
    }

    /// Intent: two physical disks with distinct names but the same
    /// LUKS UUID (cloned/dd-imaged case) surface as
    /// DiscoverError::DuplicateUuid; both by-id paths and names are
    /// named in the error, and the lexicographic-by-path ordering is
    /// deterministic.
    /// Why: this is the cloned-disk friendly-error pin from plan
    /// lines 4123-4129.
    /// Scenario: dd-cloned disk plugged in alongside the original
    /// with a relabel (seed 802).
    #[test]
    fn discover_duplicate_uuid_surfaces_friendly_error() {
        let dir = tempfile::tempdir().unwrap();
        let target_a = discover_create_target(dir.path(), "fake-original");
        let target_b = discover_create_target(dir.path(), "fake-clone");
        let path_a = discover_create_by_id_symlink(dir.path(), "ata-ORIGINAL", &target_a);
        let path_b = discover_create_by_id_symlink(dir.path(), "ata-CLONE", &target_b);
        let shared_uuid = "11111111-2222-3333-4444-555566667777";
        let runner = DiscoverLabelMap::new(&[(&path_a, "braid-disk1"), (&path_b, "braid-disk2")])
            .with_uuid(&path_a, shared_uuid)
            .with_uuid(&path_b, shared_uuid);

        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let err = scan
            .result
            .expect_err("duplicate UUID must surface as DuplicateUuid");

        let DiscoverError::DuplicateUuid {
            uuid,
            name1,
            path1,
            name2,
            path2,
        } = &err
        else {
            panic!("expected DuplicateUuid, got {err:?}");
        };
        assert_eq!(uuid.as_str(), shared_uuid);
        // (name1, path1) and (name2, path2) sorted lexicographically by path:
        // path_b ends with "ata-CLONE", path_a ends with "ata-ORIGINAL", so
        // path1 must be path_b (CLONE) since CLONE < ORIGINAL lex.
        assert!(
            path1 < path2,
            "expected lex-sorted paths: path1={path1}, path2={path2}",
        );
        assert!(path1.ends_with("ata-CLONE"), "path1 was {path1}");
        assert!(path2.ends_with("ata-ORIGINAL"), "path2 was {path2}");
        assert_eq!(name1.as_str(), "disk2");
        assert_eq!(name2.as_str(), "disk1");
        let msg = err.to_string();
        assert!(msg.contains(shared_uuid), "missing uuid: {msg}");
        assert!(msg.contains("braid-disk1"), "missing disk1 label: {msg}");
        assert!(msg.contains("braid-disk2"), "missing disk2 label: {msg}");
        assert!(
            msg.contains("detach the cloned or unintended disk before retrying"),
            "missing detach remediation clause: {msg}"
        );
        assert!(
            msg.contains("dd-cloned disk"),
            "missing remediation suffix: {msg}"
        );
    }

    /// Intent: a label collision (same braid-<name> on two distinct
    /// physical disks) AND the same LUKS UUID across both disks must
    /// surface as LabelCollision -- not DuplicateUuid. Pins the
    /// precedence rule from plan 2831-2847.
    /// Why: the cloned-disk-under-same-name scenario hits both axes;
    /// the operator should see LabelCollision so the remediation
    /// (relabel) is identical to the regular label-collision path.
    /// Scenario: seed 803.
    #[test]
    fn discover_label_collision_fires_before_duplicate_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let target_a = discover_create_target(dir.path(), "fake-original");
        let target_b = discover_create_target(dir.path(), "fake-clone");
        let path_a = discover_create_by_id_symlink(dir.path(), "ata-A", &target_a);
        let path_b = discover_create_by_id_symlink(dir.path(), "ata-B", &target_b);
        let shared_uuid = "22222222-3333-4444-5555-666677778888";
        let runner = DiscoverLabelMap::new(&[(&path_a, "braid-foo"), (&path_b, "braid-foo")])
            .with_uuid(&path_a, shared_uuid)
            .with_uuid(&path_b, shared_uuid);

        let scan = discover_from_dir(&runner, &RealByIdResolver, dir.path());
        let err = scan.result.expect_err("expected an error");

        assert!(
            matches!(err, DiscoverError::LabelCollision { .. }),
            "expected LabelCollision before DuplicateUuid, got: {err:?}"
        );
    }

    // Intent: bare `braid discover` refuses an existing UUID-keyed
    //   pool.json with the byte-exact authority/principle wording.
    // Why it exists: every byte of the refusal is operator-facing
    //   contract; this is the cheap regression net for wording drift
    //   that decision 017 leaves to code-level pinning.
    // Scenario: an operator reflexively runs `braid discover` to
    //   "refresh" an existing state file and expects to be told why
    //   live discovery is not the authoritative tool anymore.
    #[test]
    fn check_pool_json_for_bare_discover_refuses_valid_uuid_keyed() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        let mut members = PoolMembership::empty();
        members
            .insert(
                LuksUuid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap(),
                member("disk1", "/dev/disk/by-id/ata-X"),
            )
            .unwrap();
        save_membership(&members, &paths).unwrap();

        let err = check_pool_json_for_bare_discover(&paths.pool_json())
            .expect_err("must refuse with ValidUuidKeyed");

        assert!(
            matches!(&err, BareDiscoverError::ValidUuidKeyed { .. }),
            "expected ValidUuidKeyed, got: {err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "pool.json already exists at {} -- live discovery is not authoritative once pool.json exists; discover is for rebuilding missing or corrupt pool state",
                paths.pool_json().display()
            )
        );
    }

    // Intent: corrupt pool.json surfaces the byte-exact rebuild
    //   remediation through the bare path.
    // Why it exists: the same wording-drift net as the ValidUuidKeyed
    //   test, applied to the Corrupt sibling gate so the asymmetry does
    //   not shift by one slot.
    // Scenario: power loss truncates pool.json to non-JSON bytes; the
    //   operator runs bare `braid discover` and must be directed at
    //   `braid discover --write`.
    #[test]
    fn check_pool_json_for_bare_discover_refuses_corrupt() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        std::fs::write(paths.pool_json(), "not-json").unwrap();

        let err = check_pool_json_for_bare_discover(&paths.pool_json())
            .expect_err("must refuse with Corrupt");

        assert!(
            matches!(&err, BareDiscoverError::Corrupt { .. }),
            "expected Corrupt, got: {err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "pool.json at {} is corrupt or unreadable -- run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/internals/luks-unlock.md)",
                paths.pool_json().display()
            )
        );
    }

    // Intent: absent pool.json is not a refusal.
    // Why it exists: pins the Missing -> Ok(()) arm so the gate cannot
    //   silently flip to fail-closed-by-default and break first boot.
    // Scenario: a freshly installed NAS runs `braid discover` for the
    //   first time with no prior state file.
    #[test]
    fn check_pool_json_for_bare_discover_passes_when_missing() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());

        assert!(check_pool_json_for_bare_discover(&paths.pool_json()).is_ok());
    }

    /// Intent: write_discovered_membership refuses when pending-op.json
    /// exists; no save_membership call happens; pool.json is untouched.
    /// Why: the write precondition gate from plan 2849-2887.
    /// Scenario: seed 804.
    #[test]
    fn discover_write_refuses_when_pending_op_exists() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        // Seed pending-op.json with valid-looking JSON; the gate only
        // checks for file existence.
        std::fs::write(paths.pending_op_json(), "{}").unwrap();
        // Seed an existing pool.json (UUID-keyed shape) so we can
        // assert it's unchanged after the refusal.
        let pool_json_pre = "{\"disks\":{}}";
        std::fs::write(paths.pool_json(), pool_json_pre).unwrap();

        let err = write_discovered_membership(PoolMembership::empty(), &paths, None)
            .expect_err("must refuse with PendingOpExists");
        let msg = err.to_string();
        assert!(
            msg.contains("discover refusing to write pool.json: pending-op.json exists at"),
            "got: {msg}"
        );
        let pool_json_post = std::fs::read_to_string(paths.pool_json()).unwrap();
        assert_eq!(
            pool_json_post, pool_json_pre,
            "pool.json must be byte-for-byte unchanged after refusal"
        );
    }

    /// Intent: write_discovered_membership treats a non-UUID-keyed
    /// pool.json as corrupt, rebuilds it, and preserves the original
    /// bytes in an adjacent sidecar.
    /// Why: all non-current membership shapes should use the generic
    /// corrupt rebuild path instead of a special refusal.
    /// Scenario: an obsolete state file is still present when the operator
    /// runs discover --write with all intended pool members attached.
    #[test]
    fn discover_write_rebuilds_and_snapshots_when_pool_json_has_non_uuid_keys() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        let stale = r#"{"disks":{"toshiba1":{"by_id":"/dev/disk/by-id/ata-X","devid":1}}}"#;
        std::fs::write(paths.pool_json(), stale).unwrap();
        let pool_json_pre = std::fs::read(paths.pool_json()).unwrap();

        assert_eq!(
            classify_pool_json(&paths.pool_json()),
            PoolJsonShape::Corrupt
        );

        let saved = write_discovered_membership(discovered_members(&["disk1"]), &paths, None)
            .expect("expected non-UUID-keyed pool.json to be rebuilt");
        assert_eq!(saved.len(), 1);
        assert_eq!(
            classify_pool_json(&paths.pool_json()),
            PoolJsonShape::ValidUuidKeyed
        );

        let sidecars = corrupt_sidecars(root.path());
        assert_eq!(sidecars.len(), 1, "expected exactly one sidecar");
        assert!(
            is_pool_json_corrupt_rfc3339_sidecar(&sidecars[0]),
            "sidecar name should be pool.json.corrupt-<YYYY-MM-DDTHH:MM:SSZ>: {:?}",
            sidecars[0]
        );
        assert_eq!(
            std::fs::read(&sidecars[0]).unwrap(),
            pool_json_pre,
            "sidecar must preserve the original bytes"
        );
    }

    /// Intent: write_discovered_membership refuses when on-disk
    /// pool.json is a healthy UUID-keyed membership; no save happens;
    /// the existing file is byte-for-byte unchanged.
    /// Why: protects persisted DiskMember.devid bindings (decision 024
    /// fallback identity) from a stray `braid discover --write`
    /// against an already-built pool.
    /// Scenario: an operator who knows their pool.json is fine
    /// reflexively runs `braid discover --write` to "refresh"; the gate
    /// refuses instead of clobbering the file and dropping every devid.
    #[test]
    fn discover_write_refuses_when_pool_json_is_valid_uuid_keyed() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        let mut existing_members = PoolMembership::empty();
        let mut existing = DiskMember::new(
            DiskName::parse("disk1").unwrap(),
            ByIdPath::parse("/dev/disk/by-id/ata-X").unwrap(),
        );
        existing.devid = Some(7);
        existing_members
            .insert(
                LuksUuid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap(),
                existing,
            )
            .unwrap();
        save_membership(&existing_members, &paths).unwrap();
        let pool_json_pre = std::fs::read_to_string(paths.pool_json()).unwrap();

        let mut discovered_members = PoolMembership::empty();
        discovered_members
            .insert(
                LuksUuid::parse("11111111-2222-3333-4444-555555555555").unwrap(),
                DiskMember::new(
                    DiskName::parse("other").unwrap(),
                    ByIdPath::parse("/dev/disk/by-id/ata-Y").unwrap(),
                ),
            )
            .unwrap();
        let err = write_discovered_membership(discovered_members, &paths, None)
            .expect_err("must refuse with ValidUuidKeyed");
        assert!(
            matches!(&err, DiscoverWriteError::ValidUuidKeyed { .. }),
            "expected ValidUuidKeyed, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("is already a healthy UUID-keyed membership"),
            "got: {msg}"
        );
        assert_eq!(
            std::fs::read_to_string(paths.pool_json()).unwrap(),
            pool_json_pre,
            "pool.json must be byte-for-byte unchanged after refusal"
        );
    }

    // Intent: write_discovered_membership rebuilds a corrupt pool.json
    //   and preserves the original bytes in an adjacent sidecar.
    // Why it exists: corrupt state can still carry prior-binding data
    //   such as devid for null_underlying recovery; overwriting it
    //   without a snapshot destroys forensic recovery material.
    // Scenario: a stale value-side luks_uuid makes pool.json off-schema,
    //   but the old entry still carries devid:1.
    #[test]
    fn discover_write_rebuilds_and_snapshots_when_pool_json_is_corrupt() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        let corrupt = stale_luks_uuid_corrupt_pool_json();
        std::fs::write(paths.pool_json(), &corrupt).unwrap();
        let pool_json_pre = std::fs::read(paths.pool_json()).unwrap();

        let members = discovered_members(&["disk1"]);
        let saved = write_discovered_membership(members, &paths, None)
            .expect("expected corrupt pool.json to be rebuilt");
        assert_eq!(saved.len(), 1);
        assert_eq!(
            classify_pool_json(&paths.pool_json()),
            PoolJsonShape::ValidUuidKeyed
        );

        let sidecars = corrupt_sidecars(root.path());
        assert_eq!(sidecars.len(), 1, "expected exactly one sidecar");
        assert!(
            is_pool_json_corrupt_rfc3339_sidecar(&sidecars[0]),
            "sidecar name should be pool.json.corrupt-<YYYY-MM-DDTHH:MM:SSZ>: {:?}",
            sidecars[0]
        );
        assert_eq!(
            std::fs::read(&sidecars[0]).unwrap(),
            pool_json_pre,
            "sidecar must preserve the corrupt original bytes"
        );
    }

    // Intent: write_discovered_membership refuses to overwrite corrupt
    //   pool.json when the forensic sidecar cannot be created.
    // Why it exists: sidecar creation is the hard precondition that
    //   prevents destructive rebuild from losing prior-binding bytes.
    // Scenario: the state directory is read-only or otherwise unwritable
    //   during a corrupt-state rebuild.
    #[cfg(target_family = "unix")]
    #[test]
    fn discover_write_refuses_when_corrupt_sidecar_cannot_be_written() {
        use std::os::unix::fs::PermissionsExt;

        struct RestoreMode {
            path: std::path::PathBuf,
            mode: u32,
        }

        impl Drop for RestoreMode {
            fn drop(&mut self) {
                let Ok(metadata) = std::fs::metadata(&self.path) else {
                    return;
                };
                let mut perms = metadata.permissions();
                perms.set_mode(self.mode);
                let _ = std::fs::set_permissions(&self.path, perms);
            }
        }

        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        let corrupt = stale_luks_uuid_corrupt_pool_json();
        std::fs::write(paths.pool_json(), &corrupt).unwrap();
        let pool_json_pre = std::fs::read(paths.pool_json()).unwrap();
        let _restore = RestoreMode {
            path: root.path().to_path_buf(),
            mode: 0o700,
        };
        let mut perms = std::fs::metadata(root.path()).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(root.path(), perms).unwrap();

        let err = write_discovered_membership(discovered_members(&["disk1"]), &paths, None)
            .expect_err("must refuse when sidecar cannot be written");
        assert!(
            matches!(&err, DiscoverWriteError::CorruptSidecarFailed { .. }),
            "expected CorruptSidecarFailed, got: {err:?}"
        );
        assert_eq!(
            std::fs::read(paths.pool_json()).unwrap(),
            pool_json_pre,
            "corrupt pool.json must remain byte-for-byte unchanged"
        );
        assert!(
            corrupt_sidecars(root.path()).is_empty(),
            "failed sidecar attempt must not leave a sidecar behind"
        );
    }

    // Intent: count mismatch refuses before any corrupt-state sidecar is
    //   written.
    // Why it exists: a sidecar is only needed when a destructive save will
    //   follow; count-mismatch refusals should leave no forensic artifact.
    // Scenario: a corrupt pool.json is present, but the operator's
    //   --expect-count guard catches an unintended extra disk.
    #[test]
    fn discover_write_returns_expect_count_before_sidecar_when_corrupt_and_count_mismatches() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        let corrupt = stale_luks_uuid_corrupt_pool_json();
        std::fs::write(paths.pool_json(), &corrupt).unwrap();
        let pool_json_pre = std::fs::read(paths.pool_json()).unwrap();

        let err =
            write_discovered_membership(discovered_members(&["disk1", "disk2"]), &paths, Some(1))
                .expect_err("must refuse on count mismatch before sidecar");
        assert!(
            matches!(
                &err,
                DiscoverWriteError::ExpectCountUnmet {
                    expected: 1,
                    actual: 2
                }
            ),
            "expected ExpectCountUnmet, got: {err:?}"
        );
        assert_eq!(
            std::fs::read(paths.pool_json()).unwrap(),
            pool_json_pre,
            "corrupt pool.json must remain byte-for-byte unchanged"
        );
        assert!(
            corrupt_sidecars(root.path()).is_empty(),
            "count-mismatch refusal must not write a sidecar"
        );
    }

    // Intent: absent pool.json classifies as Missing.
    // Why it exists: read-only discover must keep scanning when no prior
    //   membership file exists.
    // Scenario: first boot before any discover --write run.
    #[test]
    fn classify_pool_json_returns_missing_when_absent() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());

        assert_eq!(
            classify_pool_json(&paths.pool_json()),
            PoolJsonShape::Missing
        );
    }

    // Intent: loadable UUID-keyed pool.json classifies as ValidUuidKeyed.
    // Why it exists: the "use braid add" refusal is correct only when
    //   the canonical membership loader accepts the file.
    // Scenario: a pool has already been discovered and persisted in the
    //   current UUID-keyed format.
    #[test]
    fn classify_pool_json_returns_valid_uuid_keyed_for_loadable_pool_json() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        let mut members = PoolMembership::empty();
        members
            .insert(
                LuksUuid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap(),
                DiskMember::new(
                    DiskName::parse("disk1").unwrap(),
                    ByIdPath::parse("/dev/disk/by-id/ata-X").unwrap(),
                ),
            )
            .unwrap();
        save_membership(&members, &paths).unwrap();

        assert_eq!(
            classify_pool_json(&paths.pool_json()),
            PoolJsonShape::ValidUuidKeyed
        );
    }

    // Intent: unparseable pool.json classifies as Corrupt.
    // Why it exists: bare discover must point operators at the rebuild
    //   remediation instead of suggesting braid add.
    // Scenario: a power loss truncates pool.json into non-JSON bytes.
    #[test]
    fn classify_pool_json_returns_corrupt_for_unparseable() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        std::fs::write(paths.pool_json(), "not-json").unwrap();

        assert_eq!(
            classify_pool_json(&paths.pool_json()),
            PoolJsonShape::Corrupt
        );
    }

    // Intent: parseable but off-schema pool.json classifies as Corrupt.
    // Why it exists: JSON that lacks the membership schema still cannot
    //   be repaired by braid add.
    // Scenario: an operator or old experiment writes unrelated JSON to
    //   /var/lib/braid/pool.json.
    #[test]
    fn classify_pool_json_returns_corrupt_for_off_schema() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        std::fs::write(paths.pool_json(), r#"{"unexpected":true}"#).unwrap();

        assert_eq!(
            classify_pool_json(&paths.pool_json()),
            PoolJsonShape::Corrupt
        );
    }

    // Intent: non-NotFound I/O from pool.json classifies as Corrupt.
    // Why it exists: only an absent file should allow the Missing path;
    //   unreadable present state must fail closed with rebuild guidance.
    // Scenario: the pool.json path exists as a directory, exercising the
    //   same classifier arm as EACCES/EIO without needing root tricks.
    #[test]
    fn classify_pool_json_returns_corrupt_for_non_not_found_io() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        std::fs::create_dir(paths.pool_json()).unwrap();

        assert_eq!(
            classify_pool_json(&paths.pool_json()),
            PoolJsonShape::Corrupt
        );
    }

    // Intent: value-side uniqueness conflicts classify as Corrupt.
    // Why it exists: the classifier must treat any canonical loader
    //   failure as not valid, including MembershipError::Conflict.
    // Scenario: a UUID-keyed pool.json repeats the same disk name under
    //   two members and cannot be loaded safely.
    #[test]
    fn classify_pool_json_returns_corrupt_for_value_side_conflict() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        let u1 = "aaaaaaaa-0000-0000-0000-000000000001";
        let u2 = "aaaaaaaa-0000-0000-0000-000000000002";
        let body = format!(
            "{{\"disks\":{{\"{u1}\":{{\"name\":\"dup\",\"by_id\":\"/dev/disk/by-id/ata-A\"}},\"{u2}\":{{\"name\":\"dup\",\"by_id\":\"/dev/disk/by-id/ata-B\"}}}}}}"
        );
        std::fs::write(paths.pool_json(), body).unwrap();

        assert_eq!(
            classify_pool_json(&paths.pool_json()),
            PoolJsonShape::Corrupt
        );
    }

    /// Intent: write_discovered_membership proceeds normally when
    /// neither fail-closed gate applies. Pins that the gates are
    /// fail-closed-on-condition, not fail-closed-by-default.
    /// Why: plan 2952-2956.
    /// Scenario: seed 806.
    #[test]
    fn discover_write_proceeds_when_no_gates_fire() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        // No pending-op.json, no pool.json -- both gates pass.

        let mut members = PoolMembership::empty();
        members
            .insert(
                LuksUuid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap(),
                DiskMember::new(
                    DiskName::parse("disk1").unwrap(),
                    ByIdPath::parse("/dev/disk/by-id/ata-X").unwrap(),
                ),
            )
            .unwrap();
        let saved = write_discovered_membership(members, &paths, None)
            .expect("expected save to proceed when no gates fire");
        assert_eq!(saved.len(), 1);
        assert!(
            paths.pool_json().exists(),
            "save_membership must have written pool.json"
        );
    }

    /// Intent: write_discovered_membership refuses when
    /// --expect-count exceeds the produced membership size and does
    /// not call save_membership.
    /// Why: the exact-count guard must catch partial attach.
    /// Scenario: rebuild with a momentarily detached disk.
    #[test]
    fn discover_write_refuses_when_count_mismatches_below() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());

        let mut members = PoolMembership::empty();
        for (i, name) in ["disk1", "disk2"].iter().enumerate() {
            members
                .insert(
                    LuksUuid::parse(&format!("00000000-0000-0000-0000-{:012x}", 807 + i as u64))
                        .unwrap(),
                    DiskMember::new(
                        DiskName::parse(name).unwrap(),
                        ByIdPath::parse(&format!("/dev/disk/by-id/ata-{name}")).unwrap(),
                    ),
                )
                .unwrap();
        }
        let err = write_discovered_membership(members, &paths, Some(3))
            .expect_err("must refuse with ExpectCountUnmet");
        let msg = err.to_string();
        assert!(
            msg.contains(
                "discover refusing to write pool.json: expected exactly 3 members, found 2"
            ),
            "got: {msg}"
        );
        assert!(
            !paths.pool_json().exists(),
            "pool.json must not have been written"
        );
    }

    /// Intent: write_discovered_membership refuses when
    /// --expect-count is lower than the produced membership size and
    /// does not call save_membership.
    /// Why: the exact-count guard must catch unrelated
    /// braid-labeled disks that would otherwise be admitted.
    /// Scenario: rebuild with an extra recovery disk attached.
    #[test]
    fn discover_write_refuses_when_count_mismatches_above() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());

        let mut members = PoolMembership::empty();
        for (i, name) in ["disk1", "disk2"].iter().enumerate() {
            members
                .insert(
                    LuksUuid::parse(&format!("00000000-0000-0000-0000-{:012x}", 900 + i as u64))
                        .unwrap(),
                    DiskMember::new(
                        DiskName::parse(name).unwrap(),
                        ByIdPath::parse(&format!("/dev/disk/by-id/ata-{name}")).unwrap(),
                    ),
                )
                .unwrap();
        }
        let err = write_discovered_membership(members, &paths, Some(1))
            .expect_err("must refuse with ExpectCountUnmet");
        let msg = err.to_string();
        assert!(
            msg.contains(
                "discover refusing to write pool.json: expected exactly 1 members, found 2"
            ),
            "got: {msg}"
        );
        assert!(
            !paths.pool_json().exists(),
            "pool.json must not have been written"
        );
    }

    #[test]
    fn no_members_discovered_message_carries_remediation() {
        /*
         * Intent: the empty-scan refusal names the LUKS2 requirement and the
         *   "attached and readable" remediation, in the discover " -- " house
         *   style, so it cannot silently regress to a bare "found nothing".
         * Why it exists: this message was previously a remediation-free
         *   eprintln! in main.rs that an operator rebuilding pool.json with a
         *   detached/mislabeled/LUKS1-only disk could not act on.
         * Scenario: operator runs `braid discover` with the array's disks
         *   momentarily detached and must learn what to check.
         */
        let msg = NoMembersDiscovered.to_string();
        assert!(
            msg.contains("no braid-labeled LUKS2 devices found"),
            "got: {msg}"
        );
        assert!(msg.contains("attached and readable"), "got: {msg}");
        assert!(msg.contains("LUKS1"), "got: {msg}");
        assert!(msg.contains(" -- "), "got: {msg}");
    }
}
