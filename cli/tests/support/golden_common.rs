// Shared golden-file parser test harness.
//
// Included (via include!) by golden_nixos_25_11.rs and golden_nixos_unstable.rs.
// Expects the including file to define:
//   const FIXTURE_DIR: &str = "...";
//   const REQUIRE_FIXTURES: bool = true | false;
//
// When REQUIRE_FIXTURES is true, missing fixtures panic (unstable lane).
// When false, missing fixtures skip the test (stable lane).

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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if REQUIRE_FIXTURES {
                panic!("required fixture missing: {name} (run capture commands first)");
            }
            None
        }
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
                    "SKIP: fixture {} not captured yet (run `just capture-fixtures`)",
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
    golden_btrfs_device_stats_degraded,
    "btrfs-device-stats-degraded.json",
    "btrfs device stats",
    parse::btrfs_device_stats::parse_btrfs_device_stats,
    |out: parse::types::BtrfsDeviceStatsOutput| {
        let mut devids: Vec<u64> = out.devices.iter().map(|d| d.devid).collect();
        devids.sort_unstable();
        assert_eq!(devids, vec![1, 2], "expected present and missing devids");
        for dev in &out.devices {
            assert_eq!(dev.read_io_errs, 0);
            assert_eq!(dev.write_io_errs, 0);
            assert_eq!(dev.flush_io_errs, 0);
            assert_eq!(dev.corruption_errs, 0);
            assert_eq!(dev.generation_errs, 0);
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
            matches!(out.state, parse::types::ScrubState::Finished { .. }),
            "expected Finished state after scrub"
        );
    }
);

golden_test!(
    golden_cryptsetup_status,
    "cryptsetup-status-active.txt",
    "cryptsetup status",
    parse::cryptsetup_status::parse_cryptsetup_status,
    |out: parse::types::CryptsetupStatusOutput| {
        assert!(
            matches!(
                out,
                parse::types::CryptsetupStatusOutput::Active {
                    backing: parse::types::BackingDevice::Path(_),
                }
            ),
            "active status should carry a backing path"
        );
    }
);

golden_test!(
    golden_cryptsetup_luks_uuid,
    "cryptsetup-luks-uuid.txt",
    "cryptsetup luksUUID",
    parse::cryptsetup_luks_uuid::parse_cryptsetup_luks_uuid,
    |out: parse::types::CryptsetupLuksUuidOutput| {
        // UUID should be valid (parser already validates via uuid crate)
        assert!(!out.uuid.as_str().is_empty());
    }
);

golden_test!(
    golden_cryptsetup_luks_version,
    "cryptsetup-luks-dump.txt",
    "cryptsetup luksDump",
    parse::cryptsetup_luks_version::parse_cryptsetup_luks_version,
    |out: parse::types::CryptsetupLuksVersionOutput| {
        // braid only formats LUKS2; the captured fixture must be LUKS2
        // because capture-tool-fixtures.py uses `cryptsetup luksFormat`
        // (which defaults to LUKS2 on every supported nixpkgs version).
        assert_eq!(out.version, 2);
    }
);

golden_test!(
    golden_cryptsetup_luks_label,
    "cryptsetup-luks-dump.txt",
    "cryptsetup luksDump",
    parse::cryptsetup_luks_label::parse_cryptsetup_luks_label,
    |out: parse::types::CryptsetupLuksLabelOutput| {
        // capture-tool-fixtures.py formats with `cryptsetup luksFormat`
        // and does not pass --label, so the captured fixture has no
        // label set. The parser must convert the cryptsetup-rendered
        // "(no label)" placeholder into None.
        assert!(
            out.label.is_none(),
            "captured fixture has no Label set (got: {:?})",
            out.label
        );
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
    golden_btrfs_replace_status_never_started,
    "btrfs-replace-status-never-started.txt",
    "btrfs replace status",
    parse::btrfs_replace_status::parse_btrfs_replace_status,
    |out: parse::types::ReplaceState| {
        assert_eq!(out, parse::types::ReplaceState::NotStarted);
    }
);

golden_test!(
    golden_btrfs_replace_status_running,
    "btrfs-replace-status-running.txt",
    "btrfs replace status",
    parse::btrfs_replace_status::parse_btrfs_replace_status,
    |out: parse::types::ReplaceState| match out {
        parse::types::ReplaceState::Running { pct } => {
            assert!(pct.is_finite(), "running percent must be finite");
            assert!(
                (0.0..=100.0).contains(&pct),
                "running percent must be in range, got {pct}"
            );
        }
        other => panic!("expected Running, got {other:?}"),
    }
);

golden_test!(
    golden_btrfs_replace_status_finished,
    "btrfs-replace-status-finished.txt",
    "btrfs replace status",
    parse::btrfs_replace_status::parse_btrfs_replace_status,
    |out: parse::types::ReplaceState| {
        assert_eq!(out, parse::types::ReplaceState::Finished);
    }
);

golden_test!(
    golden_btrfs_replace_status_canceled,
    "btrfs-replace-status-canceled.txt",
    "btrfs replace status",
    parse::btrfs_replace_status::parse_btrfs_replace_status,
    |out: parse::types::ReplaceState| {
        assert_eq!(out, parse::types::ReplaceState::Cancelled);
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
        assert_eq!(out.segment_offset_bytes, 16_777_216);
        assert_eq!(out.segment_size, parse::types::Luks2SegmentSize::Dynamic);
    }
);

golden_test!(
    golden_btrfs_subvolume_list,
    "btrfs-subvolume-list.txt",
    "btrfs subvolume list",
    parse::btrfs_subvolume_list::parse_btrfs_subvolume_list,
    |out: parse::types::BtrfsSubvolumeListOutput| {
        assert!(out.subvolumes.len() >= 2, "expected at least 2 subvolumes");
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
        assert!(out.devices.len() >= 2, "expected at least 2 device entries");
        for dev in &out.devices {
            assert!(
                matches!(
                    dev.state,
                    parse::types::DeviceScrubState::Finished
                        | parse::types::DeviceScrubState::Aborted
                ),
                "expected Finished or Aborted, got {:?}",
                dev.state
            );
        }
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
    assert_eq!(out, parse::types::CryptsetupStatusOutput::Inactive);
}

// --- NUT (upsc) parsers ---
//
// Fixtures live under `upsc/` because they were captured by the dedicated
// `capture-ups-fixtures` VM test; the stable (nixos-25.11) fixtures are
// authoritative and the unstable sibling tracks upstream drift. See
// docs/design/decisions/010-toolchain-pinning.md for the pinning contract.

fn upsc_fixture(name: &str) -> Option<String> {
    fixture(&format!("upsc/{name}"))
}

fn upsc_ok(name: &str) -> Option<braid_cli::parse::types::UpscOutput> {
    let stdout = upsc_fixture(name)?;
    Some(braid_cli::parse::parse_upsc(&stdout))
}

// Intent: the online fixture parses with exactly `{OL}` and surfaces the
// full typed model (battery, load, realpower, input, device).
// Why: this is the normal steady state; every field the TUI and `braid ups
// status` curate must round-trip. A regression here silently blanks
// sections of the UI.
// Scenario: UPS on utility power, full charge, no active alerts.
#[test]
fn golden_upsc_online() {
    let Some(out) = upsc_ok("upsc-online.txt") else {
        eprintln!("SKIP: upsc/upsc-online.txt not captured yet");
        return;
    };
    use parse::types::UpsStatusFlag;
    assert!(out.status_flags.contains(&UpsStatusFlag::Ol));
    assert_eq!(out.status_flags.len(), 1, "online state is exactly {{OL}}");
    assert_eq!(out.battery.charge_pct, Some(100));
    assert_eq!(out.battery.runtime_secs, Some(1800));
    assert_eq!(out.load_pct, Some(17));
    assert_eq!(out.realpower_nominal_watts, Some(330));
    assert_eq!(out.input.voltage.as_deref(), Some("120.0"));
    assert_eq!(out.input.transfer_low.as_deref(), Some("88"));
    assert_eq!(out.input.transfer_high.as_deref(), Some("142"));
    assert_eq!(out.device.model.as_deref(), Some("Back-UPS ES 550G"));
    assert_eq!(out.device.mfr.as_deref(), Some("APC"));
    assert!(out.test_result.is_some(), "ups.test.result expected");
}

// Intent: the on-battery fixture produces {OB} (no LB) and a reduced
// charge + runtime.
// Why: the TUI colors OB yellow, LB red. If the parser folded both into
// one state, the user could not tell whether LB had actually fired.
// Scenario: sustained utility outage; battery is discharging but has not
// yet dropped below battery.charge.low.
#[test]
fn golden_upsc_onbattery() {
    let Some(out) = upsc_ok("upsc-onbattery.txt") else {
        eprintln!("SKIP: upsc/upsc-onbattery.txt not captured yet");
        return;
    };
    use parse::types::UpsStatusFlag;
    assert!(out.status_flags.contains(&UpsStatusFlag::Ob));
    assert!(
        !out.status_flags.contains(&UpsStatusFlag::Lb),
        "OB alone must not carry LB"
    );
    assert!(
        matches!(out.battery.charge_pct, Some(pct) if pct < 100),
        "on-battery fixture seeds a partial charge"
    );
}

// Intent: the low-battery fixture produces the full critical pair {OB,LB}.
// Why: upsmon's critical-state check (reference/nut/clients/upsmon.c:1404)
// requires both flags. Every safety contract that depends on the critical
// state (the SHUTDOWNCMD path, preflight refusal, TUI red coloring) reads
// this combination; a parser regression here breaks all three.
// Scenario: outage long enough to cross battery.charge.low -- upsmon will
// fire SHUTDOWNCMD at this point.
#[test]
fn golden_upsc_lowbattery() {
    let Some(out) = upsc_ok("upsc-lowbattery.txt") else {
        eprintln!("SKIP: upsc/upsc-lowbattery.txt not captured yet");
        return;
    };
    use parse::types::UpsStatusFlag;
    assert!(out.status_flags.contains(&UpsStatusFlag::Ob));
    assert!(out.status_flags.contains(&UpsStatusFlag::Lb));
    // Sanity: the fixture seeds battery.charge below the low threshold.
    match out.battery.charge_pct {
        Some(pct) => assert!(
            pct <= 10,
            "lowbattery fixture should seed charge <=10, got {pct}"
        ),
        None => panic!("battery.charge must parse on the lowbattery fixture"),
    }
}

// Intent: the replace-battery fixture produces {OL, RB} and does NOT
// trigger preflight refusal logic.
// Why: RB is advisory -- the battery is aging, but the UPS is still on
// utility power. A parser that misclassified RB as critical would cause
// mutation commands to refuse for a cosmetic condition.
// Scenario: old UPS battery; operator is being reminded to replace, but
// nothing is actually wrong right now.
#[test]
fn golden_upsc_replace_battery() {
    let Some(out) = upsc_ok("upsc-replace-battery.txt") else {
        eprintln!("SKIP: upsc/upsc-replace-battery.txt not captured yet");
        return;
    };
    use parse::types::UpsStatusFlag;
    assert!(out.status_flags.contains(&UpsStatusFlag::Ol));
    assert!(out.status_flags.contains(&UpsStatusFlag::Rb));
    assert!(!out.status_flags.contains(&UpsStatusFlag::Ob));
    assert!(!out.status_flags.contains(&UpsStatusFlag::Lb));
}
