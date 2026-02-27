//! Parser boundary for raw CLI command output.
//!
//! Text parser guidelines:
//! - Prefer structured command output (JSON) when available.
//! - Use parser combinators (`nom`) for real grammars: repeated records,
//!   alternatives, and strict line formats (for example: `btrfs device stats`,
//!   `btrfs filesystem show`, `cryptsetup status`).
//! - Keep simple keyed extraction for trivial outputs (single fields, UUIDs, or
//!   a few labeled lines).
//! - Do not use free-form `str::contains` for command-output classification in
//!   domain code; keep text interpretation in `parse/*` and return typed enums.
//! - File-based fixtures must come from `tests/fixtures/nixos-25.11` only.
//! - Synthetic scenarios (variant happy-paths, negative/malformed inputs) must
//!   be inline string literals in tests.
//! - Compatibility aliases are out of contract unless explicitly documented.
//!
pub mod btrfs_balance_status;
pub mod btrfs_device_stats;
pub mod btrfs_device_usage;
pub mod btrfs_filesystem_df;
pub mod btrfs_filesystem_show;
pub mod btrfs_filesystem_usage;
pub mod btrfs_scrub_status;
pub mod btrfs_scrub_status_per_device;
pub mod cryptsetup_luks_dump;
pub mod cryptsetup_luks_uuid;
pub mod cryptsetup_status;
pub mod findmnt;
pub mod lsblk;
pub mod smartctl;
pub mod types;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("invalid JSON from `{cmd}`: {detail}")]
    InvalidJson { cmd: String, detail: String },

    #[error("invalid text from `{cmd}`: {detail}")]
    InvalidText { cmd: String, detail: String },

    #[error("command `{cmd}` failed (exit {exit_code}): {stderr}")]
    CommandFailed {
        cmd: String,
        exit_code: i32,
        stderr: String,
    },

    #[error("missing field `{field}` in output of `{cmd}`")]
    MissingField { cmd: String, field: String },
}

// Re-export all types for convenient access
pub use types::*;

// Re-export parse functions (same names, new source modules)
pub use btrfs_balance_status::parse_btrfs_balance_status;
pub use btrfs_device_stats::parse_btrfs_device_stats;
pub use btrfs_device_usage::parse_btrfs_device_usage;
pub use btrfs_filesystem_df::parse_btrfs_df_json;
pub use btrfs_filesystem_show::parse_btrfs_filesystem_show;
pub use btrfs_filesystem_usage::parse_btrfs_filesystem_usage;
pub use btrfs_scrub_status::parse_btrfs_scrub_status;
pub use btrfs_scrub_status_per_device::parse_btrfs_scrub_status_per_device;
pub use cryptsetup_luks_dump::parse_cryptsetup_luks_dump;
pub use cryptsetup_luks_uuid::parse_cryptsetup_luks_uuid;
pub use cryptsetup_status::parse_cryptsetup_status;
pub use findmnt::parse_findmnt_json;
pub use lsblk::{parse_lsblk_field, parse_lsblk_json};
pub use smartctl::parse_smartctl_health;
