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
