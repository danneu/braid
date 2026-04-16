//! Golden-file parser tests for nixos-25.11 tool output.
//!
//! These tests parse actual tool output captured from a nixos-25.11 VM
//! (via `just capture-fixtures`) and verify the parsers handle it correctly.
//! If fixtures haven't been captured yet, tests are skipped.

use braid_cli::cmd::RawCommandOutput;
use braid_cli::parse;

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nixos-25.11");
const REQUIRE_FIXTURES: bool = false;

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
}
