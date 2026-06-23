//! Golden-file parser tests for nixos-26.05 tool output.
//!
//! These tests parse actual tool output captured from a nixos-26.05 VM
//! (via `just capture-fixtures`) and verify the parsers handle it correctly.
//! If fixtures haven't been captured yet, tests are skipped.

use braid_cli::cmd::RawCommandOutput;
use braid_cli::parse;

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nixos-26.05");
const REQUIRE_FIXTURES: bool = false;
const EXPECTED_LUKS_LABEL: Option<&str> = Some("braid-vdb");

include!("support/golden_common.rs");

// Stable-only physical-drive capture (not from the VM fixture pipeline).
// Lives here instead of golden_common.rs so the unstable lane doesn't
// require a matching capture. Panics on missing — this fixture is part
// of the stable contract.
#[test]
fn golden_smartctl_sata_with_temperature() {
    let path = format!("{FIXTURE_DIR}/smartctl-sata-with-temperature.json");
    let stdout = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("required fixture missing: {path} ({e})"));
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
