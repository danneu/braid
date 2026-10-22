//! Golden-file parser tests for nixos-26.05 tool output.
//!
//! These tests parse actual tool output captured from a nixos-26.05 VM
//! (via `just capture-fixtures`) and verify the parsers handle it correctly.
//! The committed stable-contract fixtures are required; missing files fail.

use braid_cli::cmd::RawCommandOutput;
use braid_cli::parse;

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nixos-26.05");
const EXPECTED_LUKS_LABEL: Option<&str> = Some("braid-vdb");

include!("support/golden_common.rs");

// Stable-only physical-drive capture (not from the VM fixture pipeline).
// Lives here instead of golden_common.rs so the unstable lane doesn't
// require a matching capture. Panics on missing — this fixture is part
// of the stable contract.
#[test]
fn golden_smartctl_sata_with_temperature() {
    let stdout = fixture("smartctl-sata-with-temperature.json");
    let raw = RawCommandOutput {
        cmd: "smartctl -H -A --json".into(),
        stdout,
        stderr: String::new(),
        exit_status: 0,
    };
    let probe = parse::smartctl::parse_smartctl(&raw);
    assert_eq!(probe.health, parse::types::SmartHealth::Healthy);
    assert_eq!(probe.celsius, Some(26));
    // The real fixture's attributes 5/197/198 are all nominal, so the parser
    // carries zeroed SATA evidence -- validating sata_evidence against a real
    // capture, which retires the old `// TODO: validate with real SATA fixture`.
    assert_eq!(
        probe.evidence,
        Some(parse::types::SmartEvidence::Sata {
            reallocated_sectors: 0,
            pending_sectors: 0,
            offline_uncorrectable: 0,
        })
    );
}

// Intent: a real kernel dm UUID for an open LUKS2 mapper parses to the same
//   LUKS UUID `cryptsetup luksUUID` reads from that disk's header.
// Why it exists: braid identifies every open mapper by this sysfs value
//   (cli/src/dm_identity.rs), including mappers whose disk is gone; a format
//   drift in cryptsetup's dm UUID would silently turn every mapper into an
//   unverifiable one.
// Scenario: capture-tool-fixtures.py opens /dev/vdb as braid-vdb and records
//   both /sys/block/dm-N/dm/uuid and `cryptsetup luksUUID /dev/vdb` from the
//   same VM run. Stable-only: the unstable lane has no matching capture.
#[test]
fn golden_dm_uuid_luks2_matches_luks_uuid() {
    let parse::DmUuid::Luks(from_dm) = parse::parse_dm_uuid(&fixture("dm-uuid-luks2.txt")) else {
        panic!("captured dm UUID must classify as a LUKS mapping");
    };
    let raw = RawCommandOutput {
        cmd: "cryptsetup luksUUID".into(),
        stdout: fixture("cryptsetup-luks-uuid.txt"),
        stderr: String::new(),
        exit_status: 0,
    };
    let from_header = parse::cryptsetup_luks_uuid::parse_cryptsetup_luks_uuid(&raw)
        .expect("captured luksUUID parses");
    assert_eq!(from_dm, from_header.uuid);
}
