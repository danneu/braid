use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_cryptsetup_luks_label;
use crate::types::ByIdPath;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum DiscoverError {
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("failed to read /dev/disk/by-id: {0}")]
    ReadDir(#[source] std::io::Error),
}

/// Scan /dev/disk/by-id/ for LUKS devices with braid-<name> labels.
/// Returns a map of discovered pool members: name -> by_id path.
pub fn discover_pool_members<R: CommandRunner>(
    runner: &R,
) -> Result<BTreeMap<String, ByIdPath>, DiscoverError> {
    discover_from_dir(runner, Path::new("/dev/disk/by-id"))
}

fn discover_from_dir<R: CommandRunner>(
    runner: &R,
    by_id_dir: &Path,
) -> Result<BTreeMap<String, ByIdPath>, DiscoverError> {
    let entries = match std::fs::read_dir(by_id_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeMap::new());
        }
        Err(e) => return Err(DiscoverError::ReadDir(e)),
    };

    let mut members = BTreeMap::new();

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
        if runner
            .run(&CmdRequest::CryptsetupIsLuks {
                device: path_str.clone(),
            })
            .is_err()
        {
            continue;
        }

        // Read LUKS label via luksDump text output
        let label = match runner.run(&CmdRequest::CryptsetupLuksDumpText {
            device: path_str.clone(),
        }) {
            Ok(raw) => parse_cryptsetup_luks_label(&raw)
                .ok()
                .and_then(|out| out.label),
            Err(_) => continue,
        };

        // Check if label matches braid-<name>
        if let Some(label) = label
            && let Some(disk_name) = crate::config::name_from_mapper(&label)
                && crate::membership::is_valid_disk_name(disk_name) {
                    match members.entry(disk_name.to_owned()) {
                        Entry::Vacant(e) => {
                            e.insert(ByIdPath(path_str));
                        }
                        Entry::Occupied(mut e) => {
                            // Keep the candidate with the best (priority, filename) key so
                            // selection is fully deterministic regardless of read_dir order.
                            let existing_name =
                                e.get().0.rsplit('/').next().unwrap_or("").to_owned();
                            let candidate_key = (by_id_priority(&name_str), name_str.as_ref());
                            let existing_key =
                                (by_id_priority(&existing_name), existing_name.as_str());
                            if candidate_key < existing_key {
                                e.insert(ByIdPath(path_str));
                            }
                        }
                    }
                }
    }

    Ok(members)
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
fn by_id_priority(filename: &str) -> u8 {
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

fn is_partition_entry(name: &str) -> bool {
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

    fn mock_ok(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    /// Test runner that maps full by-id paths to LUKS labels.
    /// - CryptsetupIsLuks: OK for known paths, Err otherwise.
    /// - CryptsetupLuksDumpText: returns a luksDump stub with the mapped label.
    struct LabelMap(HashMap<String, String>);

    impl LabelMap {
        fn new(entries: &[(&str, &str)]) -> Self {
            LabelMap(
                entries
                    .iter()
                    .map(|(path, label)| (path.to_string(), label.to_string()))
                    .collect(),
            )
        }
    }

    impl CommandRunner for LabelMap {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::CryptsetupIsLuks { device } => {
                    if self.0.contains_key(device.as_str()) {
                        Ok(mock_ok("cryptsetup", ""))
                    } else {
                        Err(CmdError::Failed("not luks".into()))
                    }
                }
                CmdRequest::CryptsetupLuksDumpText { device } => {
                    let label = self.0.get(device.as_str()).ok_or(CmdError::MissingMock)?;
                    Ok(mock_ok(
                        "cryptsetup",
                        &format!("LUKS header information\nLabel:\t{label}\n"),
                    ))
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

    fn create_file(dir: &Path, name: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, b"").unwrap();
        path.to_string_lossy().into_owned()
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
        let ata_path = create_file(dir.path(), "ata-SEAGATE_ST500");
        let wwn_path = create_file(dir.path(), "wwn-0x50014ee606704442");
        let runner = LabelMap::new(&[(&ata_path, "braid-sda"), (&wwn_path, "braid-sda")]);
        let members = discover_from_dir(&runner, dir.path()).unwrap();
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
        let ata_z = create_file(dir.path(), "ata-ZZZZZ_DISK");
        let ata_a = create_file(dir.path(), "ata-AAAAA_DISK");
        let runner = LabelMap::new(&[(&ata_z, "braid-sda"), (&ata_a, "braid-sda")]);
        let members = discover_from_dir(&runner, dir.path()).unwrap();
        assert_eq!(members.len(), 1);
        assert!(
            members["sda"].0.ends_with("ata-AAAAA_DISK"),
            "expected lexicographically earlier path, got: {}",
            members["sda"].0
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
        let ata_alpha = create_file(dir.path(), "ata-DISK1_ALPHA");
        let wwn_alpha = create_file(dir.path(), "wwn-0x0001");
        let ata_beta = create_file(dir.path(), "ata-DISK2_BETA");
        let wwn_beta = create_file(dir.path(), "wwn-0x0002");
        let runner = LabelMap::new(&[
            (&ata_alpha, "braid-alpha"),
            (&wwn_alpha, "braid-alpha"),
            (&ata_beta, "braid-beta"),
            (&wwn_beta, "braid-beta"),
        ]);
        let members = discover_from_dir(&runner, dir.path()).unwrap();
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
}
