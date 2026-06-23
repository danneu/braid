//! Golden-file parser tests for nixos-unstable tool output.
//!
//! Tracked forecast lane: these tests parse tool output captured from a
//! nixos-unstable VM to foresee parser breakage before it hits stable.
//! Missing fixtures fail (not skip) — run capture commands first.

use braid_cli::cmd::RawCommandOutput;
use braid_cli::parse;

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nixos-unstable");
const REQUIRE_FIXTURES: bool = true;
// The unstable capture lane still carries the pre-refresh unlabeled dump.
// Keep that expectation explicit until the unstable VM startup hang is fixed
// and the fixture can be regenerated from capture-tool-fixtures.py.
const EXPECTED_LUKS_LABEL: Option<&str> = None;

include!("support/golden_common.rs");
