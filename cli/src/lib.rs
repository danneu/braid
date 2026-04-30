pub mod ack;
pub mod add;
pub mod alert;
pub mod browse;
pub mod cmd;
pub mod config;
pub mod confirm;
pub mod credential_verify;
pub mod discover;
pub mod doctor;
pub mod enroll_key_file;
pub mod hdparm;
pub mod idle;
pub mod inhibit;
pub mod journal;

pub mod lock;
pub mod luks;
pub mod membership;
pub mod monitor;
pub mod mount;
pub mod mount_check;
pub mod parse;
pub mod pool;
pub mod preflight;
pub mod preview;
pub mod probe;
pub mod progress;
pub mod recover;
pub mod remove;
pub mod remove_missing;
pub mod replace;
pub mod scrub_cancel;
pub mod scrub_needs_resume;
pub mod scrub_resume_or_start;
pub mod state_io;
pub mod state_paths;
pub mod status;
pub mod status_tag;
// TUI is stubbed out — suppress unused-code warnings for now.
// TODO: remove #[allow(dead_code)] once the TUI is more developed.
#[allow(dead_code)]
pub mod tui;
pub mod types;
pub mod unlock;
pub mod ups;
pub mod util;
