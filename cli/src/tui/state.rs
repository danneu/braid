use std::collections::VecDeque;
use std::process::ExitStatus;

pub type CmdId = u64;

pub enum CmdStatus {
    Running,
    Finished(ExitStatus),
}

pub enum Stream {
    Stdout,
    Stderr,
}

pub struct CommandState {
    pub cmd: String,
    pub status: CmdStatus,
    pub output: VecDeque<String>,
}

pub const MAX_LINES: usize = 1000;
