pub mod ack;
pub mod add;
pub mod alert;
pub mod cmd;
pub mod config;
pub mod confirm;
pub mod credential_verify;

/// Owned, fully-resolved credential values (`OpenCredential`) shared
/// by `unlock` and `recover` plus the flag-router that produces them.
/// Sibling to `credential_verify` (borrowed-credential verification).
pub mod credential;
pub mod discover;
pub mod doctor;
pub mod enroll_key_file;
pub mod hdparm;
pub mod idle;
pub mod inhibit;
pub mod journal;

pub mod lock;
pub mod luks;
pub(crate) mod mapper_close;
pub mod membership;
pub mod monitor;
pub mod mount;
pub mod mount_check;
pub mod online_state;
pub mod parse;
pub mod pool;
pub mod pool_lock;
pub mod preflight;
pub mod preview;
pub mod probe;
/// Shared post-commit UUID drift probe used by replace/remove/recover to verify
/// an observed mapper still maps to the journaled LUKS UUID before close.
pub(crate) mod probe_mapper_uuid;
pub mod progress;
pub mod recover;
pub mod remove;
pub mod remove_missing;
pub mod replace;
pub mod scrub_cancel;
pub mod scrub_needs_resume;
pub mod scrub_resume_or_start;
/// In-memory secret types (currently `Passphrase`) that scrub on drop and
/// gate plaintext egress through `expose_secret()`. Sibling to `credential`
/// and `credential_verify`, which carry resolved and borrowed credentials.
pub mod secret;
pub mod state_io;
pub mod state_paths;
pub mod status;
pub mod status_tag;
#[cfg(test)]
pub(crate) mod test_fixtures;
// TUI is stubbed out — suppress unused-code warnings for now.
// TODO: remove #[allow(dead_code)] once the TUI is more developed.
#[allow(dead_code)]
pub mod tui;
pub mod types;
pub mod unlock;
pub mod ups;
pub mod util;
