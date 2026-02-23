pub mod btrfs_device_stats;
pub mod btrfs_filesystem_df;
pub mod btrfs_filesystem_show;
pub mod btrfs_filesystem_usage;
pub mod btrfs_scrub_status;
pub mod cryptsetup_luks_uuid;
pub mod cryptsetup_status;
pub mod findmnt;
pub mod lsblk;
pub mod mount;
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
pub use btrfs_device_stats::parse_btrfs_device_stats;
pub use btrfs_filesystem_df::parse_btrfs_df_json;
pub use btrfs_filesystem_show::parse_btrfs_filesystem_show;
pub use btrfs_filesystem_usage::parse_btrfs_filesystem_usage;
pub use btrfs_scrub_status::parse_btrfs_scrub_status;
pub use cryptsetup_luks_uuid::parse_cryptsetup_luks_uuid;
pub use cryptsetup_status::parse_cryptsetup_status;
pub use findmnt::parse_findmnt_json;
pub use lsblk::{parse_lsblk_field, parse_lsblk_json};
