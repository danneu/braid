//! Golden-file parser tests for nixos-25.11 tool output.
//!
//! These tests parse actual tool output captured from a nixos-25.11 VM
//! (via `make capture-fixtures`) and verify the parsers handle it correctly.
//! If fixtures haven't been captured yet, tests are skipped.

use braid_cli::cmd::RawCommandOutput;
use braid_cli::parse;

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nixos-25.11");

/// btrfs-progs resolves device paths via path_canonicalize(), which reads
/// /sys/block/dm-N/dm/name to map kernel dm-N names to /dev/mapper/<name>.
/// This sysfs lookup succeeds on the macOS aarch64 linux-builder VM but fails
/// on the x86_64 NixOS machine, so fixtures contain either format:
///   NixOS machine (x86_64):        /dev/dm-N
///   macOS linux-builder (aarch64): /dev/mapper/braid-vXX
fn is_dm_or_mapper_path(s: &str) -> bool {
    s.starts_with("/dev/dm-") || s.starts_with("/dev/mapper/braid-")
}

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
        golden_test!($name, $fixture, $cmd, $parse_fn, $assert_fn, exit_status: 0);
    };
    ($name:ident, $fixture:expr, $cmd:expr, $parse_fn:expr, $assert_fn:expr, exit_status: $exit:expr) => {
        #[test]
        fn $name() {
            let Some(content) = fixture($fixture) else {
                eprintln!(
                    "SKIP: fixture {} not captured yet (run `make capture-fixtures`)",
                    $fixture
                );
                return;
            };
            let raw = RawCommandOutput {
                cmd: $cmd.into(),
                stdout: content,
                stderr: String::new(),
                exit_status: $exit,
            };
            let out =
                $parse_fn(&raw).expect(concat!("parser failed on golden fixture: ", $fixture));
            $assert_fn(out);
        }
    };
}

// --- JSON parsers ---

golden_test!(
    golden_lsblk_json,
    "lsblk-2disk.json",
    "lsblk",
    parse::lsblk::parse_lsblk_json,
    |out: parse::types::LsblkOutput| {
        assert_eq!(out.blockdevices.len(), 2, "expected 2 blockdevices");
        // Each disk should have a crypt child (LUKS)
        for dev in &out.blockdevices {
            assert_eq!(dev.device_type, "disk");
            assert!(
                !dev.children.is_empty(),
                "disk {} has no children",
                dev.name
            );
            assert_eq!(dev.children[0].device_type, "crypt");
        }
    }
);

golden_test!(
    golden_findmnt_json,
    "findmnt-btrfs.json",
    "findmnt",
    parse::findmnt::parse_findmnt_json,
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
    parse::btrfs_filesystem_df::parse_btrfs_df_json,
    |out: parse::types::BtrfsDfOutput| {
        assert!(!out.entries.is_empty(), "expected at least one df entry");
        // RAID1 setup should have Data with RAID1 profile
        let data = out
            .entries
            .iter()
            .find(|e| e.bg_type == parse::types::BtrfsBgType::Data);
        assert!(data.is_some(), "expected a Data entry");
        assert_eq!(data.unwrap().bg_profile, parse::types::BtrfsProfile::Raid1);
    }
);

// --- Text parsers ---

golden_test!(
    golden_btrfs_show,
    "btrfs-show-2disk.txt",
    "btrfs filesystem show",
    parse::btrfs_filesystem_show::parse_btrfs_filesystem_show,
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
    parse::btrfs_filesystem_usage::parse_btrfs_filesystem_usage,
    |out: parse::types::BtrfsFilesystemUsageOutput| {
        assert!(out.device_size_bytes > 0, "device_size should be positive");
        assert!(
            out.used_bytes > 0,
            "used should be positive (we wrote test data)"
        );
    }
);

golden_test!(
    golden_btrfs_device_stats,
    "btrfs-device-stats-2disk.json",
    "btrfs device stats",
    parse::btrfs_device_stats::parse_btrfs_device_stats,
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
    parse::btrfs_scrub_status::parse_btrfs_scrub_status,
    |out: parse::types::BtrfsScrubStatusOutput| {
        assert_eq!(out.state, parse::types::ScrubState::Never);
    }
);

golden_test!(
    golden_btrfs_scrub_completed,
    "btrfs-scrub-completed.txt",
    "btrfs scrub status",
    parse::btrfs_scrub_status::parse_btrfs_scrub_status,
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
    parse::cryptsetup_status::parse_cryptsetup_status,
    |out: parse::types::CryptsetupStatusOutput| {
        assert!(out.is_active);
        assert!(out.device.is_some(), "active status should have a device");
    }
);

golden_test!(
    golden_cryptsetup_luks_uuid,
    "cryptsetup-luks-uuid.txt",
    "cryptsetup luksUUID",
    parse::cryptsetup_luks_uuid::parse_cryptsetup_luks_uuid,
    |out: parse::types::CryptsetupLuksUuidOutput| {
        // UUID should be valid (parser already validates via uuid crate)
        assert!(!out.uuid.0.is_empty());
    }
);

// --- Progress-monitoring parsers ---

golden_test!(
    golden_btrfs_balance_status_none,
    "btrfs-balance-status-none.txt",
    "btrfs balance status",
    parse::btrfs_balance_status::parse_btrfs_balance_status,
    |out: parse::types::BtrfsBalanceStatusOutput| {
        assert_eq!(out.state, parse::types::BalanceState::None);
    }
);

golden_test!(
    golden_btrfs_device_usage,
    "btrfs-device-usage-2disk.txt",
    "btrfs device usage",
    parse::btrfs_device_usage::parse_btrfs_device_usage,
    |out: parse::types::BtrfsDeviceUsageOutput| {
        assert_eq!(out.devices.len(), 2, "expected 2 devices");
        // Exact devid/path mapping
        assert_eq!(out.devices[0].devid, 1);
        assert!(
            is_dm_or_mapper_path(&out.devices[0].path),
            "devid 1 path should be dm or mapper, got: {}",
            out.devices[0].path
        );
        assert_eq!(out.devices[1].devid, 2);
        assert!(
            is_dm_or_mapper_path(&out.devices[1].path),
            "devid 2 path should be dm or mapper, got: {}",
            out.devices[1].path
        );
        // At least one Data,RAID1 allocation with bytes > 0
        let has_data_raid1 = out.devices[0]
            .allocations
            .iter()
            .any(|a| a.alloc_type == "Data" && a.profile == "RAID1" && a.bytes > 0);
        assert!(
            has_data_raid1,
            "expected Data,RAID1 allocation with bytes > 0 on first device"
        );
        // Sanity: sizes are positive
        assert!(
            out.devices[0].device_size > 0,
            "device_size should be positive"
        );
        assert!(
            out.devices[0].unallocated > 0,
            "unallocated should be positive"
        );
    }
);

// --- SMART fixtures ---

#[test]
fn golden_smartctl_nvme_healthy() {
    let Some(content) = fixture("smartctl-nvme-healthy.json") else {
        eprintln!("SKIP: fixture not captured yet");
        return;
    };
    let raw = RawCommandOutput {
        cmd: "smartctl".into(),
        stdout: content,
        stderr: String::new(),
        exit_status: 0,
    };
    assert_eq!(
        parse::smartctl::parse_smartctl_health(&raw),
        parse::types::SmartHealth::Healthy
    );
}

// --- TUI-only parsers (not exercised by CLI commands in VM tests) ---

golden_test!(
    golden_cryptsetup_luks_dump,
    "cryptsetup-luks-dump.json",
    "cryptsetup luksDump --dump-json-metadata",
    parse::cryptsetup_luks_dump::parse_cryptsetup_luks_dump,
    |out: parse::types::CryptsetupLuksDumpOutput| {
        assert_eq!(out.cipher, "aes-xts-plain64");
        assert_eq!(out.key_size_bits, 512);
        assert_eq!(out.keyslot_count, 1);
    }
);

golden_test!(
    golden_btrfs_subvolume_list,
    "btrfs-subvolume-list.txt",
    "btrfs subvolume list",
    parse::btrfs_subvolume_list::parse_btrfs_subvolume_list,
    |out: parse::types::BtrfsSubvolumeListOutput| {
        assert_eq!(out.subvolumes.len(), 4, "expected 4 subvolumes");
        assert_eq!(out.subvolumes[0].path, "data");
        assert_eq!(out.subvolumes[1].path, "snapshots");
    }
);

golden_test!(
    golden_btrfs_scrub_per_device_finished,
    "btrfs-scrub-per-device-finished.txt",
    "btrfs scrub status -d -R",
    parse::btrfs_scrub_status_per_device::parse_btrfs_scrub_status_per_device,
    |out: parse::types::BtrfsScrubStatusPerDeviceOutput| {
        assert_eq!(out.devices.len(), 3, "expected 3 device entries");
        assert_eq!(
            out.devices[0].state,
            parse::types::DeviceScrubState::Finished
        );
        assert_eq!(
            out.devices[1].state,
            parse::types::DeviceScrubState::Aborted
        );
        assert_eq!(
            out.devices[2].state,
            parse::types::DeviceScrubState::Finished
        );
    }
);

golden_test!(
    golden_btrfs_scrub_per_device_running,
    "btrfs-scrub-per-device-running.txt",
    "btrfs scrub status -d -R",
    parse::btrfs_scrub_status_per_device::parse_btrfs_scrub_status_per_device,
    |out: parse::types::BtrfsScrubStatusPerDeviceOutput| {
        assert_eq!(out.devices.len(), 3, "expected 3 device entries");
        for dev in &out.devices {
            assert_eq!(dev.state, parse::types::DeviceScrubState::Running);
        }
    }
);

// --- In-progress fixtures (captured from progress-monitoring VM test) ---

golden_test!(
    golden_btrfs_balance_status_running,
    "btrfs-balance-status-running.txt",
    "btrfs balance status",
    parse::btrfs_balance_status::parse_btrfs_balance_status,
    |out: parse::types::BtrfsBalanceStatusOutput| {
        match out.state {
            parse::types::BalanceState::Running {
                estimated_total_chunks,
                pct_left,
                ..
            } => {
                assert!(
                    estimated_total_chunks > 0,
                    "expected estimated_total_chunks > 0"
                );
                assert!(pct_left <= 100, "pct_left should be <= 100, got {pct_left}");
            }
            ref other => panic!("expected Running state, got {other:?}"),
        }
    },
    exit_status: 1
);

golden_test!(
    golden_btrfs_balance_status_paused,
    "btrfs-balance-status-paused-skip-balance.txt",
    "btrfs balance status",
    parse::btrfs_balance_status::parse_btrfs_balance_status,
    |out: parse::types::BtrfsBalanceStatusOutput| {
        assert_eq!(
            out.state,
            parse::types::BalanceState::Paused {
                done_chunks: 0,
                estimated_total_chunks: 0,
                considered_chunks: 0,
                pct_left: 0,
            }
        );
    },
    exit_status: 1
);

golden_test!(
    golden_btrfs_device_usage_removing,
    "btrfs-device-usage-removing.txt",
    "btrfs device usage",
    parse::btrfs_device_usage::parse_btrfs_device_usage,
    |out: parse::types::BtrfsDeviceUsageOutput| {
        assert!(!out.devices.is_empty(), "expected at least one device");
        let has_used = out.devices.iter().any(|d| d.used_bytes() > 0);
        assert!(has_used, "expected at least one device with used_bytes > 0");
    }
);

// --- Manual golden tests (don't fit the macro) ---

#[test]
fn golden_cryptsetup_status_inactive() {
    let Some(stdout) = fixture("cryptsetup-status-inactive.stdout") else {
        eprintln!("SKIP: fixture not captured yet");
        return;
    };
    let stderr = fixture("cryptsetup-status-inactive.stderr").unwrap_or_default();
    let raw = RawCommandOutput {
        cmd: "cryptsetup status".into(),
        stdout,
        stderr,
        exit_status: 4,
    };
    let out = parse::cryptsetup_status::parse_cryptsetup_status(&raw)
        .expect("parser failed on golden fixture: cryptsetup-status-inactive");
    assert!(!out.is_active);
    assert_eq!(out.device, None);
}
