use crate::cmd::{CmdOutput, RawCommandOutput};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("unsupported parser path for command: {0}")]
    Unsupported(String),
    #[error("invalid json: {0}")]
    InvalidJson(String),
    #[error("invalid text format: {0}")]
    InvalidText(String),
}

pub fn parse_output(raw: RawCommandOutput) -> Result<CmdOutput, ParseError> {
    // Phase 1 stub only. Implement command-specific parsers in Phase 2.
    Err(ParseError::Unsupported(raw.cmd))
}
