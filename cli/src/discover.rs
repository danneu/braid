use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::{ParseError, parse_cryptsetup_luks_label, parse_cryptsetup_luks_version};
use crate::types::ByIdPath;
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
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct DiscoverOutcome {
    pub members: BTreeMap<String, ByIdPath>,
    pub warnings: Vec<DiscoverWarning>,
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

fn discover_from_dir<R: CommandRunner>(
    runner: &R,
    resolver: &dyn crate::recover::ByIdResolver,
    by_id_dir: &Path,
) -> Result<DiscoverOutcome, DiscoverError> {
    let entries = match std::fs::read_dir(by_id_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DiscoverOutcome {
                members: BTreeMap::new(),
                warnings: Vec::new(),
            });
        }
        Err(e) => return Err(DiscoverError::ReadDir(e)),
    };

    // (priority, filename, by_id, canonical) for the best candidate per disk
    // name. Caching priority + filename at insertion lets the Occupied arm
    // tie-break without re-extracting the basename or recomputing the priority
    // class for the stored entry.
    let mut members: BTreeMap<String, (u8, String, ByIdPath, String)> = BTreeMap::new();
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

        // Check if LUKS
        let raw = runner.run(&CmdRequest::CryptsetupIsLuks {
            device: path_str.clone(),
        })?;
        if raw.exit_status != 0 {
            continue;
        }

        // Read LUKS label + version via luksDump text output. One luksDump
        // call, two parses on the same RawCommandOutput. The version check
        // enforces braid's LUKS2-only invariant at this gateway so a
        // braid-labeled LUKS1 disk never reaches pool.json via
        // `braid discover --write`.
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

        // Check if label matches braid-<name>
        let Some(label) = label else {
            continue;
        };
        let Some(disk_name) = crate::config::name_from_mapper(&label) else {
            continue;
        };
        if !crate::membership::is_valid_disk_name(disk_name) {
            warnings.push(DiscoverWarning::InvalidDiskName {
                path: path_str.clone(),
                label: label.clone(),
            });
            continue;
        }

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

        let priority = by_id_priority(&name_str);
        let filename = name_str.into_owned();

        match members.entry(disk_name.to_owned()) {
            Entry::Vacant(e) => {
                e.insert((priority, filename, ByIdPath(path_str), canonical));
            }
            Entry::Occupied(mut e) => {
                let (existing_priority, existing_filename, existing_by_id, existing_canonical) =
                    e.get();
                if *existing_canonical != canonical {
                    return Err(label_collision(
                        disk_name,
                        existing_by_id.0.clone(),
                        path_str,
                    ));
                }

                // Same physical disk via two aliases -- keep the candidate
                // with the best (priority, filename) key so selection is
                // deterministic regardless of read_dir order.
                let candidate_better = (priority, filename.as_str())
                    < (*existing_priority, existing_filename.as_str());
                if candidate_better {
                    e.insert((priority, filename, ByIdPath(path_str), canonical));
                }
            }
        }
    }

    Ok(DiscoverOutcome {
        members: members
            .into_iter()
            .map(|(name, (_, _, by_id, _))| (name, by_id))
            .collect(),
        warnings,
    })
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
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn mock_output(cmd: &str, stdout: &str, exit_status: i32) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status,
        }
    }

    /// Test runner that maps full by-id paths to LUKS labels and versions.
    /// Returns realistic Ok(RawCommandOutput) for all commands — uses non-zero
    /// exit status for unknown devices, never Err (matching RealRunner behavior).
    /// Tracks which (command, device) pairs were called. Default version
    /// for known devices is LUKS2 (matching what braid actually formats).
    struct LabelMap {
        labels: HashMap<String, String>,
        versions: HashMap<String, u32>,
        dump_responses: HashMap<String, RawCommandOutput>,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl LabelMap {
        fn new(entries: &[(&str, &str)]) -> Self {
            LabelMap {
                labels: entries
                    .iter()
                    .map(|(path, label)| (path.to_string(), label.to_string()))
                    .collect(),
                versions: HashMap::new(),
                dump_responses: HashMap::new(),
                calls: Mutex::new(Vec::new()),
            }
        }

        /// Override the LUKS version reported for a specific path. Defaults
        /// to 2 (LUKS2) for any path not explicitly set.
        fn with_version(mut self, path: &str, version: u32) -> Self {
            self.versions.insert(path.to_string(), version);
            self
        }

        /// Override the entire luksDump response for a path. Used to inject
        /// realistic failure modes (non-zero exit + stderr) or unparseable
        /// stdout. Takes precedence over the synthesized default.
        fn with_dump_response(mut self, path: &str, response: RawCommandOutput) -> Self {
            self.dump_responses.insert(path.to_string(), response);
            self
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for LabelMap {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::CryptsetupIsLuks { device } => {
                    self.calls
                        .lock()
                        .unwrap()
                        .push(("isLuks".into(), device.clone()));
                    if self.labels.contains_key(device.as_str()) {
                        Ok(mock_output("cryptsetup", "", 0))
                    } else {
                        Ok(mock_output("cryptsetup", "", 1))
                    }
                }
                CmdRequest::CryptsetupLuksDumpText { device } => {
                    self.calls
                        .lock()
                        .unwrap()
                        .push(("luksDump".into(), device.clone()));
                    if let Some(response) = self.dump_responses.get(device.as_str()) {
                        Ok(response.clone())
                    } else if let Some(label) = self.labels.get(device.as_str()) {
                        let version = self.versions.get(device.as_str()).copied().unwrap_or(2);
                        Ok(mock_output(
                            "cryptsetup",
                            &format!(
                                "LUKS header information\nVersion:\t{version}\nLabel:\t{label}\n"
                            ),
                            0,
                        ))
                    } else {
                        Ok(mock_output(
                            "cryptsetup",
                            "Device /dev/foo is not a valid LUKS device.\n",
                            1,
                        ))
                    }
                }
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.run(request)
        }
    }

    /// Create a real placeholder file in `dir` representing a physical
    /// device. Symlinks pointing at this file canonicalize to its path.
    fn create_target(dir: &Path, name: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, b"").unwrap();
        path.to_string_lossy().into_owned()
    }

    /// Create a by-id symlink in `dir` pointing at `target`. Returns the
    /// symlink's full path, which is what discover_from_dir sees at runtime.
    fn create_by_id_symlink(dir: &Path, name: &str, target: &str) -> String {
        let path = dir.join(name);
        std::os::unix::fs::symlink(target, &path).unwrap();
        path.to_string_lossy().into_owned()
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
        let target = create_target(target_dir.path(), "fake-disk");
        create_by_id_symlink(by_id_dir.path(), "ata-SOMEDISK", &target);

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
        let target = create_target(target_dir.path(), "fake-disk");
        create_by_id_symlink(by_id_dir.path(), "ata-SOMEDISK", &target);

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
        let luks_target = create_target(dir.path(), "fake-sda");
        let usb_target = create_target(dir.path(), "fake-usb");
        let luks_path = create_by_id_symlink(dir.path(), "ata-TOSHIBA_BRAID", &luks_target);
        create_by_id_symlink(dir.path(), "ata-USB_STICK", &usb_target);

        // Only the LUKS device is in the label map; the USB stick is unknown.
        let runner = LabelMap::new(&[(&luks_path, "braid-sda")]);
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
        let modern_target = create_target(dir.path(), "fake-sda");
        let broken_target = create_target(dir.path(), "fake-sdb");
        let modern_path = create_by_id_symlink(dir.path(), "ata-MODERN_DISK", &modern_target);
        let broken_path = create_by_id_symlink(dir.path(), "ata-BROKEN_DISK", &broken_target);
        let runner = LabelMap::new(&[
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
            outcome.members.contains_key("modern"),
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
        let target = create_target(dir.path(), "fake-sda");
        let path = create_by_id_symlink(dir.path(), "ata-ODD_DISK", &target);
        let runner = LabelMap::new(&[(&path, "braid-odd")]).with_dump_response(
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
        let target = create_target(dir.path(), "fake-sda");
        let ata_path = create_by_id_symlink(dir.path(), "ata-SEAGATE_ST500", &target);
        let wwn_path = create_by_id_symlink(dir.path(), "wwn-0x50014ee606704442", &target);
        let runner = LabelMap::new(&[(&ata_path, "braid-sda"), (&wwn_path, "braid-sda")]);
        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();
        assert!(
            outcome.warnings.is_empty(),
            "unexpected warnings: {:?}",
            outcome.warnings
        );
        let members = &outcome.members;
        assert_eq!(members.len(), 1);
        assert!(
            members["sda"].0.ends_with("wwn-0x50014ee606704442"),
            "expected wwn path, got: {}",
            members["sda"].0
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
        let target = create_target(dir.path(), "fake-sda");
        let ata_z = create_by_id_symlink(dir.path(), "ata-ZZZZZ_DISK", &target);
        let ata_a = create_by_id_symlink(dir.path(), "ata-AAAAA_DISK", &target);
        let runner = LabelMap::new(&[(&ata_z, "braid-sda"), (&ata_a, "braid-sda")]);
        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();
        assert!(
            outcome.warnings.is_empty(),
            "unexpected warnings: {:?}",
            outcome.warnings
        );
        let members = &outcome.members;
        assert_eq!(members.len(), 1);
        assert!(
            members["sda"].0.ends_with("ata-AAAAA_DISK"),
            "expected lexicographically earlier path, got: {}",
            members["sda"].0
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
        let luks1_target = create_target(dir.path(), "fake-sda");
        let luks2_target = create_target(dir.path(), "fake-sdb");
        let luks1_path = create_by_id_symlink(dir.path(), "ata-LEGACY_DISK", &luks1_target);
        let luks2_path = create_by_id_symlink(dir.path(), "ata-MODERN_DISK", &luks2_target);
        let runner = LabelMap::new(&[(&luks1_path, "braid-legacy"), (&luks2_path, "braid-modern")])
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
            members.contains_key("modern"),
            "modern (LUKS2) disk should be present: {members:?}"
        );
        assert!(
            !members.contains_key("legacy"),
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
        let bad_target = create_target(dir.path(), "fake-bad");
        let good_target = create_target(dir.path(), "fake-good");
        let bad_path = create_by_id_symlink(dir.path(), "ata-BAD_LABEL", &bad_target);
        let good_path = create_by_id_symlink(dir.path(), "ata-GOOD_LABEL", &good_target);
        let runner = LabelMap::new(&[(&bad_path, "braid-é"), (&good_path, "braid-good")]);

        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();

        assert_eq!(
            outcome.members.len(),
            1,
            "expected only the valid disk: {:?}",
            outcome.members,
        );
        assert!(
            outcome.members.contains_key("good"),
            "good disk should be discovered: {:?}",
            outcome.members,
        );
        assert!(
            !outcome.members.keys().any(|k| k.contains("é")),
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
        let alpha_target = create_target(dir.path(), "fake-disk1");
        let beta_target = create_target(dir.path(), "fake-disk2");
        let ata_alpha = create_by_id_symlink(dir.path(), "ata-DISK1_ALPHA", &alpha_target);
        let wwn_alpha = create_by_id_symlink(dir.path(), "wwn-0x0001", &alpha_target);
        let ata_beta = create_by_id_symlink(dir.path(), "ata-DISK2_BETA", &beta_target);
        let wwn_beta = create_by_id_symlink(dir.path(), "wwn-0x0002", &beta_target);
        let runner = LabelMap::new(&[
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
        assert!(
            members["alpha"].0.ends_with("wwn-0x0001"),
            "expected wwn for alpha, got: {}",
            members["alpha"].0
        );
        assert!(
            members["beta"].0.ends_with("wwn-0x0002"),
            "expected wwn for beta, got: {}",
            members["beta"].0
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
        let target_a = create_target(dir.path(), "fake-sda");
        let target_b = create_target(dir.path(), "fake-sdb");
        let alias_a = create_by_id_symlink(dir.path(), "ata-CLONE_A", &target_a);
        let alias_b = create_by_id_symlink(dir.path(), "ata-CLONE_B", &target_b);
        let runner = LabelMap::new(&[(&alias_a, "braid-foo"), (&alias_b, "braid-foo")]);

        let err =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap_err();

        match &err {
            DiscoverError::LabelCollision { name, path1, path2 } => {
                assert_eq!(name, "foo");
                let pair = [path1.as_str(), path2.as_str()];
                assert!(
                    pair.contains(&alias_a.as_str()) && pair.contains(&alias_b.as_str()),
                    "collision must reference both aliases: {pair:?}",
                );
            }
            other => panic!("expected LabelCollision, got {other:?}"),
        }

        let msg = err.to_string();
        assert!(msg.contains("braid-foo"), "missing label name: {msg}");
        assert!(msg.contains(&alias_a), "missing alias_a: {msg}");
        assert!(msg.contains(&alias_b), "missing alias_b: {msg}");
    }

    #[test]
    fn discover_skips_entry_when_canonicalize_fails() {
        /*
         * Intent: a by-id symlink whose canonicalize fails is skipped with a
         *   warning instead of aborting discovery.
         * Why it exists: collision detection only applies after both entries
         *   resolve to canonical targets. A broken symlink should not become
         *   either a hard collision or an accepted pool member.
         * Scenario: udev leaves a stale by-id symlink after a transient disk
         *   detach; discover still records the remaining valid member.
         */
        let dir = tempfile::tempdir().unwrap();
        let target = create_target(dir.path(), "fake-sda");
        let dangling =
            create_by_id_symlink(dir.path(), "ata-DANGLING", "/nonexistent/dangling/target");
        let valid = create_by_id_symlink(dir.path(), "wwn-VALID", &target);
        let runner = LabelMap::new(&[(&dangling, "braid-foo"), (&valid, "braid-foo")]);

        let outcome =
            discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path()).unwrap();
        let members = &outcome.members;

        assert_eq!(members.len(), 1, "expected only the canonicalizable entry");
        assert!(
            members["foo"].0.ends_with("wwn-VALID"),
            "expected the valid symlink to win, got: {}",
            members["foo"].0
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
}
