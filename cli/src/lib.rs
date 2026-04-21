pub mod ack;
pub mod add;
pub mod alert;
pub mod browse;
pub mod cmd;
pub mod confirm;
pub mod config;
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
pub mod mount;
pub mod monitor;
pub mod parse;
pub mod pool;
pub mod preflight;
pub mod recover;
pub mod probe;
pub mod progress;
pub mod remove;
pub mod remove_missing;
pub mod replace;
pub mod scrub_cancel;
pub mod state_io;
pub mod state_paths;
pub mod status;
// TUI is stubbed out — suppress unused-code warnings for now.
// TODO: remove #[allow(dead_code)] once the TUI is more developed.
#[allow(dead_code)]
pub mod tui;
pub mod types;
pub mod unlock;
pub mod ups;
pub mod util;

