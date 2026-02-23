//! Golden-file parser tests for nixos-25.11 tool output.
//!
//! These tests parse actual tool output captured from a nixos-25.11 VM
//! (via `make capture-fixtures`) and verify the parsers handle it correctly.
//! If fixtures haven't been captured yet, tests are skipped.

use braid_cli::cmd::RawCommandOutput;
use braid_cli::parse;

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nixos-25.11");

fn fixture(name: &str) -> Option<String> {
    let path = format!("{FIXTURE_DIR}/{name}");
    match std::fs::read_to_string(&path) {
        Ok(contents) => Some(contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("reading fixture {name}: {e}"),
    }
}

macro_rules! golden_test {
    ($name:ident, $fixture:expr, $cmd:expr, $parse_fn:expr, $assert_fn:expr) => {
        #[test]
        fn $name() {
            let Some(content) = fixture($fixture) else {
                eprintln!("SKIP: fixture {} not captured yet (run `make capture-fixtures`)", $fixture);
                return;
            };
            let raw = RawCommandOutput {
                cmd: $cmd.into(),
                stdout: content,
                stderr: String::new(),
                exit_status: 0,
            };
            let out = $parse_fn(&raw).expect(concat!("parser failed on golden fixture: ", $fixture));
            $assert_fn(out);
        }
    };
}

// --- JSON parsers ---

golden_test!(
    golden_lsblk_json,
    "lsblk-2disk.json",
    "lsblk",
    parse::json::parse_lsblk_json,
    |out: parse::types::LsblkOutput| {
        assert_eq!(out.blockdevices.len(), 2, "expected 2 blockdevices");
        // Each disk should have a crypt child (LUKS)
        for dev in &out.blockdevices {
            assert_eq!(dev.device_type, "disk");
            assert!(!dev.children.is_empty(), "disk {} has no children", dev.name);
            assert_eq!(dev.children[0].device_type, "crypt");
        }
    }
);

golden_test!(
    golden_findmnt_json,
    "findmnt-btrfs.json",
    "findmnt",
    parse::json::parse_findmnt_json,
    |out: parse::types::FindmntOutput| {
        assert_eq!(out.filesystems.len(), 1, "expected 1 filesystem");
        assert_eq!(out.filesystems[0].target, "/mnt/storage");
        assert_eq!(out.filesystems[0].fstype, "btrfs");
    }
);

golden_test!(
    golden_btrfs_df_json,
    "btrfs-df-raid1.json",
    "btrfs filesystem df",
    parse::json::parse_btrfs_df_json,
    |out: parse::types::BtrfsDfOutput| {
        assert!(!out.entries.is_empty(), "expected at least one df entry");
        // RAID1 setup should have Data with RAID1 profile
        let data = out.entries.iter().find(|e| e.bg_type == "Data");
        assert!(data.is_some(), "expected a Data entry");
        assert_eq!(data.unwrap().bg_profile, "RAID1");
    }
);

// --- Text parsers ---

golden_test!(
    golden_btrfs_show,
    "btrfs-show-2disk.txt",
    "btrfs filesystem show",
    parse::text::parse_btrfs_filesystem_show,
    |out: parse::types::BtrfsFilesystemShowOutput| {
        assert_eq!(out.total_devices, 2);
        assert_eq!(out.devices.len(), 2);
        assert!(!out.has_missing);
    }
);

golden_test!(
    golden_btrfs_usage,
    "btrfs-usage-raw.txt",
    "btrfs filesystem usage",
    parse::text::parse_btrfs_filesystem_usage,
    |out: parse::types::BtrfsFilesystemUsageOutput| {
        assert!(out.device_size_bytes > 0, "device_size should be positive");
        assert!(out.used_bytes > 0, "used should be positive (we wrote test data)");
    }
);

golden_test!(
    golden_btrfs_device_stats,
    "btrfs-device-stats-2disk.txt",
    "btrfs device stats",
    parse::text::parse_btrfs_device_stats,
    |out: parse::types::BtrfsDeviceStatsOutput| {
        assert_eq!(out.devices.len(), 2, "expected stats for 2 devices");
        // Fresh pool — no errors expected
        for dev in &out.devices {
            assert_eq!(dev.read_io_errs, 0);
            assert_eq!(dev.write_io_errs, 0);
            assert_eq!(dev.corruption_errs, 0);
        }
    }
);

golden_test!(
    golden_btrfs_scrub_never,
    "btrfs-scrub-never.txt",
    "btrfs scrub status",
    parse::text::parse_btrfs_scrub_status,
    |out: parse::types::BtrfsScrubStatusOutput| {
        assert_eq!(out.state, parse::types::ScrubState::Never);
    }
);

golden_test!(
    golden_btrfs_scrub_completed,
    "btrfs-scrub-completed.txt",
    "btrfs scrub status",
    parse::text::parse_btrfs_scrub_status,
    |out: parse::types::BtrfsScrubStatusOutput| {
        assert!(
            matches!(out.state, parse::types::ScrubState::Completed { .. }),
            "expected Completed state after scrub"
        );
    }
);

golden_test!(
    golden_cryptsetup_status,
    "cryptsetup-status-active.txt",
    "cryptsetup status",
    parse::text::parse_cryptsetup_status,
    |out: parse::types::CryptsetupStatusOutput| {
        assert!(out.is_active);
        assert!(out.device.is_some(), "active status should have a device");
    }
);

golden_test!(
    golden_cryptsetup_luks_uuid,
    "cryptsetup-luks-uuid.txt",
    "cryptsetup luksUUID",
    parse::text::parse_cryptsetup_luks_uuid,
    |out: parse::types::CryptsetupLuksUuidOutput| {
        // UUID should be valid (parser already validates via uuid crate)
        assert!(!out.uuid.0.is_empty());
    }
);
