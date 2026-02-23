pub mod json;
pub mod text;
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

// Re-export parse functions
pub use json::{parse_btrfs_df_json, parse_findmnt_json, parse_lsblk_json};
pub use text::{
    parse_btrfs_device_stats, parse_btrfs_filesystem_show, parse_btrfs_filesystem_usage,
    parse_btrfs_scrub_status, parse_cryptsetup_luks_uuid, parse_cryptsetup_status,
    parse_lsblk_field,
};
