//! Golden-file parser tests for nixos-unstable tool output.
//!
//! Tracked forecast lane: these tests parse tool output captured from a
//! nixos-unstable VM to foresee parser breakage before it hits stable.
//! Missing fixtures fail (not skip) — run capture commands first.

use braid_cli::cmd::RawCommandOutput;
use braid_cli::parse;

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nixos-unstable");
const REQUIRE_FIXTURES: bool = true;

include!("support/golden_common.rs");
