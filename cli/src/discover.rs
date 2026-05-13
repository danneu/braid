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
    /// labels so the operator can pick which one to relabel or detach.
    /// Raised explicitly in the discover code path before delegating to
    /// `PoolMembership::insert` so the friendly wording reaches the
    /// operator instead of the generic `MembershipError::Conflict`.
    #[error(
        "duplicate LUKS UUID: braid-{name1} ({path1}) and braid-{name2} ({path2}) share UUID {uuid} -- relabel or detach one before retrying (this typically indicates a dd-cloned disk)"
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
    /// body had no `UUID:` line. The disk is skipped (it cannot be a
    /// pool member without identity) and surfaced as a structured
    /// warning so operators know to inspect the header.
    MissingLuksUuid {
        path: String,
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
            DiscoverWarning::MissingLuksUuid { path } => {
                write!(f, "skipping {path}: luksDump output missing UUID")
            }
            DiscoverWarning::InvalidLuksUuid { path, raw, detail } => write!(
                f,
                "skipping {path}: invalid LUKS UUID \"{raw}\" -- {detail}"
            ),
        }
    }
}

/// Outcome of `discover_pool_members`. `members` is a `PoolMembership`
/// keyed by UUID so the same value type flows through to
/// `save_membership` without a second collection step on the
/// `--write` path.
#[derive(Debug, PartialEq, Eq)]
pub struct DiscoverOutcome {
    /// UUID-keyed membership reconstructed from attached braid-labeled disks.
    pub members: PoolMembership,
    /// Non-fatal scan findings for disks skipped before membership write.
    pub warnings: Vec<DiscoverWarning>,
}

/// Discover-side fail-closed errors that fire from the `--write` path
/// before any `save_membership` call. Separate from `DiscoverError`
/// (which collects pre-write failures from the scan itself) because
/// each variant pins an operator-facing remediation message that
/// downstream tests assert against.
#[derive(Debug, thiserror::Error)]
pub enum DiscoverWriteError {
    /// `pending-op.json` exists at the journal path -- the discover
    /// `--write` cutover precondition fails closed instead of
    /// overwriting `pool.json` mid-recovery (see
    /// `docs/luks-unlock.md`).
    #[error(
        "discover refusing to write pool.json: pending-op.json exists at {path} -- run 'braid recover' first (see docs/luks-unlock.md)"
    )]
    PendingOpExists { path: String },
    /// Existing `pool.json` on disk is in the old name-keyed shape.
    /// The cutover runbook tells the operator to move it aside; the
    /// gate enforces that instead of silently overwriting it.
    #[error(
        "discover refusing to write pool.json: existing file at {path} is not in UUID-keyed format -- back it up and move it aside before retrying (see docs/luks-unlock.md)"
    )]
    NameKeyedPoolJson { path: String },
    /// `--expect-count <N>` was set and discovery produced fewer than
    /// `N` members. Catches the partial-attach hazard during the
    /// cutover runbook.
    #[error(
        "discover refusing to write pool.json: expected at least {expected} members, found {actual} -- check that all intended pool members are attached and readable, then retry"
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

/// Scan /dev/disk/by-id/ for LUKS devices with braid-<name> labels.
/// Returns discovered pool members and per-device warnings.
pub fn discover_pool_members<R: CommandRunner>(
    runner: &R,
) -> Result<DiscoverOutcome, DiscoverError> {
    discover_from_dir(
        runner,
        &crate::recover::RealByIdResolver,
        Path::new("/dev/disk/by-id"),
    )
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

fn discover_from_dir<R: CommandRunner>(
    runner: &R,
    resolver: &dyn crate::recover::ByIdResolver,
    by_id_dir: &Path,
) -> Result<DiscoverOutcome, DiscoverError> {
    let entries = match std::fs::read_dir(by_id_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DiscoverOutcome {
                members: PoolMembership::empty(),
                warnings: Vec::new(),
            });
        }
        Err(e) => return Err(DiscoverError::ReadDir(e)),
    };

    // Per-disk-name best candidate, keyed by the validated DiskName.
    // The Occupied arm tie-breaks within an alias set without
    // re-extracting the basename or re-probing the LUKS UUID.
    let mut members: BTreeMap<DiskName, AliasCandidate> = BTreeMap::new();
    let mut warnings = Vec::new();

    for entry in entries.flatten() {
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

        // Check if LUKS
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
            Err(ParseError::MissingField { .. }) => {
                warnings.push(DiscoverWarning::MissingLuksUuid {
                    path: path_str.clone(),
                });
                continue;
            }
            Err(ParseError::UnexpectedValue { value, .. }) => {
                // value is "<raw> (<detail>)" per the dump parser;
                // split the formatted prefix back out into the raw +
                // detail surface the warning needs.
                let (raw, detail) = match value.find(" (") {
                    Some(idx) => (
                        value[..idx].to_owned(),
                        value[idx + 2..].trim_end_matches(')').to_owned(),
                    ),
                    None => (value.clone(), String::new()),
                };
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
                    return Err(label_collision(
                        e.key().as_str(),
                        existing.by_id.as_str().to_owned(),
                        candidate.by_id.as_str().to_owned(),
                    ));
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
    for (name, cand) in members {
        let member = DiskMember::new(name, cand.by_id);
        membership.insert(cand.luks_uuid, member).map_err(|e| {
            // Discover is the only writer of fresh membership here, so
            // a Conflict at this point indicates a logic bug rather
            // than user-facing state corruption. Surface the message
            // through DiscoverError::Cmd's escape hatch is wrong; use
            // an explicit panic-equivalent by mapping to a synthetic
            // DiscoverError::LabelCollision is also wrong. The
            // pragmatic answer: bubble it through a generic
            // `DiscoverError::ReadDir`-like surface would also be
            // wrong. Stay strict here: log the error and treat as
            // ReadDir error wrapping the I/O-shaped MembershipError
            // body. In practice, the prior DuplicateUuid pass + the
            // four-axis pre-checks make this branch unreachable.
            DiscoverError::ReadDir(std::io::Error::other(format!(
                "membership insert failed after discover pre-checks: {e}"
            )))
        })?;
    }

    Ok(DiscoverOutcome {
        members: membership,
        warnings,
    })
}

/// Apply discover's `--write` pre-save fail-closed gates and persist
/// the discovered membership. The two gates pinned in the plan must
/// fire BEFORE any `save_membership` call:
/// 1. `pending-op.json` must not exist (covered by `PendingOpExists`).
/// 2. Existing `pool.json` must not be in the legacy name-keyed shape
///    (covered by `NameKeyedPoolJson`).
///
/// When `expected_count` is set, the gate refuses if the produced
/// membership has fewer than `expected_count` members (cutover
/// partial-attach guard).
///
/// On success returns the saved `PoolMembership`. The accepting CLI
/// arm at `main.rs:707` consumes both `warnings` (printed before this
/// call) and the saved membership.
pub fn write_discovered_membership(
    outcome: DiscoverOutcome,
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
    if pool_json_path.exists()
        && let Ok(raw) = std::fs::read_to_string(&pool_json_path)
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw)
        && let Some(disks) = value.get("disks").and_then(|v| v.as_object())
        && disks.keys().any(|k| LuksUuid::parse(k).is_err())
    {
        return Err(DiscoverWriteError::NameKeyedPoolJson {
            path: pool_json_path.display().to_string(),
        });
    }

    if let Some(expected) = expected_count {
        let actual = outcome.members.len();
        if actual < expected {
            return Err(DiscoverWriteError::ExpectCountUnmet { expected, actual });
        }
    }

    save_membership(&outcome.members, paths)?;
    Ok(outcome.members)
}

/// Priority for /dev/disk/by-id/ symlink prefixes. Lower = more preferred.
///
/// | Prefix  | Source                                               | Stable?                           |
/// |---------|------------------------------------------------------|-----------------------------------|
/// | wwn-    | World Wide Name from firmware, fully persistent      | Yes                               |
/// | nvme-   | NVMe controller serial + namespace                   | Yes                               |
/// | scsi-   | SCSI Inquiry VPD page (hardware serial/EUI-64)       | Yes                               |
/// | ata-    | Model + serial via kernel ATA driver                 | Yes (format can vary by kernel)   |
/// | usb-    | USB device serial number                             | Yes (absent on cheap drives)      |
/// | other   | Everything else (dm-uuid, etc.)                      | Varies                            |
pub(crate) fn by_id_priority(filename: &str) -> u8 {
    if filename.starts_with("wwn-") {
        return 0;
    }
    if filename.starts_with("nvme-") {
        return 1;
    }
    if filename.starts_with("scsi-") {
        return 2;
    }
    if filename.starts_with("ata-") {
        return 3;
    }
    if filename.starts_with("usb-") {
        return 4;
    }
    5
}

pub(crate) fn is_partition_entry(name: &str) -> bool {
    // Match -part1, -part2, etc. at end of name
    if let Some(idx) = name.rfind("-part") {
        let rest = &name[idx + 5..];
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
    } else {
        false
    }
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

        let err = discover_from_dir(
            &IsLuksFailRunner,
            &crate::recover::RealByIdResolver,
            by_id_dir.path(),
        )
        .unwrap_err();

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

        let err = discover_from_dir(
            &LuksDumpFailRunner,
            &crate::recover::RealByIdResolver,
            by_id_dir.path(),
        )
        .unwrap_err();

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
        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();
        assert!(
            outcome.warnings.is_empty(),
            "unexpected warnings: {:?}",
            outcome.warnings
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

        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();

        assert_eq!(outcome.members.len(), 1);
        assert!(
            contains_name(&outcome.members, "modern"),
            "modern disk should be discovered: {:?}",
            outcome.members
        );
        assert_eq!(outcome.warnings.len(), 1);
        let warning = &outcome.warnings[0];
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

        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();

        assert!(outcome.members.is_empty());
        assert_eq!(outcome.warnings.len(), 1);
        let DiscoverWarning::LuksDumpUnparseable { path, detail } = &outcome.warnings[0] else {
            panic!(
                "expected LuksDumpUnparseable, got {:?}",
                outcome.warnings[0]
            );
        };
        assert!(path.ends_with("ata-ODD_DISK"), "path was {path}");
        assert!(detail.contains("Version"), "detail was {detail}");
    }

    #[test]
    fn partition_detection() {
        assert!(is_partition_entry("ata-TOSHIBA_MN08-part1"));
        assert!(is_partition_entry("ata-TOSHIBA_MN08-part12"));
        assert!(!is_partition_entry("ata-TOSHIBA_MN08"));
        assert!(!is_partition_entry("ata-TOSHIBA_MN08-part"));
        assert!(!is_partition_entry("ata-TOSHIBA_MN08-partial"));
    }

    #[test]
    fn by_id_priority_ordering() {
        /*
         * Intent: verify the relative priority of all known by-id prefix classes.
         * Why it exists: if the ordering constants are wrong (e.g. ata and scsi swapped),
         *   discover would silently prefer the less stable symlink.
         * Scenario: developer adds a new prefix tier and accidentally misorders the values.
         */
        assert!(by_id_priority("wwn-0x123") < by_id_priority("nvme-SAMSUNG"));
        assert!(by_id_priority("nvme-SAMSUNG") < by_id_priority("scsi-360014"));
        assert!(by_id_priority("scsi-360014") < by_id_priority("ata-SEAGATE"));
        assert!(by_id_priority("ata-WD") < by_id_priority("usb-Kingston"));
        assert!(by_id_priority("usb-Kingston") < by_id_priority("dm-uuid-123"));
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
        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();
        assert!(
            outcome.warnings.is_empty(),
            "unexpected warnings: {:?}",
            outcome.warnings
        );
        let members = &outcome.members;
        assert_eq!(members.len(), 1);
        let sda = by_id_for(members, "sda");
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
        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();
        assert!(
            outcome.warnings.is_empty(),
            "unexpected warnings: {:?}",
            outcome.warnings
        );
        let members = &outcome.members;
        assert_eq!(members.len(), 1);
        let sda = by_id_for(members, "sda");
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
        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();
        let members = &outcome.members;
        assert_eq!(
            members.len(),
            1,
            "expected only the LUKS2 disk: {members:?}"
        );
        assert!(
            contains_name(members, "modern"),
            "modern (LUKS2) disk should be present: {members:?}"
        );
        assert!(
            !contains_name(members, "legacy"),
            "legacy (LUKS1) disk should be skipped: {members:?}"
        );
        assert_eq!(outcome.warnings.len(), 1);
        assert!(matches!(
            &outcome.warnings[0],
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

        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();

        assert_eq!(
            outcome.members.len(),
            1,
            "expected only the valid disk: {:?}",
            outcome.members,
        );
        assert!(
            contains_name(&outcome.members, "good"),
            "good disk should be discovered: {:?}",
            outcome.members,
        );
        assert!(
            !outcome
                .members
                .iter()
                .any(|(_, m)| m.name.as_str().contains("é")),
            "invalid name must not be recorded: {:?}",
            outcome.members,
        );

        assert_eq!(outcome.warnings.len(), 1);
        let DiscoverWarning::InvalidDiskName { path, label } = &outcome.warnings[0] else {
            panic!("expected InvalidDiskName, got {:?}", outcome.warnings[0]);
        };
        assert!(path.ends_with("ata-BAD_LABEL"), "path was {path}");
        assert_eq!(label, "braid-é");

        // Pin the escape_default() choice: non-ASCII characters in the
        // label must be rendered as \u{...} escapes in the user-facing
        // warning. If someone replaces escape_default() with {:?} (Debug)
        // or {} (raw), the printable non-ASCII byte survives verbatim and
        // this assertion fails.
        let rendered = outcome.warnings[0].to_string();
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
        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();
        assert!(
            outcome.warnings.is_empty(),
            "unexpected warnings: {:?}",
            outcome.warnings
        );
        let members = &outcome.members;
        assert_eq!(members.len(), 2);
        let alpha = by_id_for(members, "alpha");
        assert!(
            alpha.ends_with("wwn-0x0001"),
            "expected wwn for alpha, got: {alpha}"
        );
        let beta = by_id_for(members, "beta");
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

        let err =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap_err();

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

        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();

        assert!(outcome.members.is_empty());
        assert_eq!(outcome.warnings.len(), 1);
        assert!(matches!(
            &outcome.warnings[0],
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

        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();
        let members = &outcome.members;

        assert_eq!(members.len(), 1, "expected only the canonicalizable entry");
        let foo = by_id_for(members, "foo");
        assert!(
            foo.ends_with("wwn-VALID"),
            "expected the valid symlink to win, got: {foo}"
        );
        assert_eq!(outcome.warnings.len(), 1);
        assert!(matches!(
            &outcome.warnings[0],
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

    /// Intent: a braid-labeled LUKS2 disk whose luksDump body has no
    /// UUID line surfaces as DiscoverWarning::MissingLuksUuid and is
    /// absent from the discovered members.
    /// Why it exists: the discover -> save_membership path depends on
    /// every member having a UUID. A regression that silently kept
    /// the disk (or dropped it without warning) would either corrupt
    /// pool.json or leave the operator without a diagnostic.
    /// Scenario: a disk's LUKS header was zeroed mid-format leaving a
    /// label but no UUID; discover warns and skips it (seed 800).
    #[test]
    fn discover_warns_when_uuid_line_missing() {
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

        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();

        assert!(
            !contains_name(&outcome.members, "baddisk"),
            "disk with missing UUID must not be a member"
        );
        let warning = outcome
            .warnings
            .iter()
            .find(|w| matches!(w, DiscoverWarning::MissingLuksUuid { .. }))
            .expect("MissingLuksUuid warning expected");
        let DiscoverWarning::MissingLuksUuid { path: warn_path } = warning else {
            unreachable!();
        };
        assert!(warn_path.ends_with("ata-MISSING_UUID"));
        assert_eq!(
            warning.to_string(),
            format!("skipping {warn_path}: luksDump output missing UUID"),
        );
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

        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();

        assert!(!contains_name(&outcome.members, "baddisk"));
        let warning = outcome
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

        let err = discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path())
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

        let err = discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path())
            .expect_err("expected an error");

        assert!(
            matches!(err, DiscoverError::LabelCollision { .. }),
            "expected LabelCollision before DuplicateUuid, got: {err:?}"
        );
    }

    /// Intent: write_discovered_membership refuses when pending-op.json
    /// exists; no save_membership call happens; pool.json is untouched.
    /// Why: the cutover precondition gate from plan 2849-2887.
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

        let outcome = DiscoverOutcome {
            members: PoolMembership::empty(),
            warnings: Vec::new(),
        };

        let err = write_discovered_membership(outcome, &paths, None)
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

    /// Intent: write_discovered_membership refuses when on-disk
    /// pool.json is in old name-keyed shape; no save happens; the
    /// existing file is byte-for-byte unchanged.
    /// Why: the cutover schema-sniff gate from plan 2888-2920.
    /// Scenario: seed 805 -- operator forgot step 4 of the runbook.
    #[test]
    fn discover_write_refuses_when_pool_json_is_name_keyed() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(root.path().to_path_buf());
        // Synthetic old-shape pool.json: top-level keys are disk names,
        // not UUIDs.
        let stale = r#"{"disks":{"toshiba1":{"by_id":"/dev/disk/by-id/ata-X"}}}"#;
        std::fs::write(paths.pool_json(), stale).unwrap();

        let outcome = DiscoverOutcome {
            members: PoolMembership::empty(),
            warnings: Vec::new(),
        };

        let err = write_discovered_membership(outcome, &paths, None)
            .expect_err("must refuse with NameKeyedPoolJson");
        let msg = err.to_string();
        assert!(
            msg.contains("is not in UUID-keyed format -- back it up and move it aside"),
            "got: {msg}"
        );
        let pool_json_post = std::fs::read_to_string(paths.pool_json()).unwrap();
        assert_eq!(
            pool_json_post, stale,
            "name-keyed pool.json must be byte-for-byte unchanged after refusal"
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
        let outcome = DiscoverOutcome {
            members,
            warnings: Vec::new(),
        };

        let saved = write_discovered_membership(outcome, &paths, None)
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
    /// Why: the cutover partial-attach guard from plan 3859-3864.
    /// Scenario: seed 807 -- runbook step 5 with a momentarily
    /// detached disk.
    #[test]
    fn discover_write_refuses_when_below_expected_count() {
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
        let outcome = DiscoverOutcome {
            members,
            warnings: Vec::new(),
        };

        let err = write_discovered_membership(outcome, &paths, Some(3))
            .expect_err("must refuse with ExpectCountUnmet");
        let msg = err.to_string();
        assert!(
            msg.contains(
                "discover refusing to write pool.json: expected at least 3 members, found 2"
            ),
            "got: {msg}"
        );
        assert!(
            !paths.pool_json().exists(),
            "pool.json must not have been written"
        );
    }
}
