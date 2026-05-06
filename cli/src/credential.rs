use crate::credential_verify::Credential;
use crate::luks::{self, LuksError};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Owned, fully-resolved credential ready to drive `cryptsetup open`.
/// Plaintext is scrubbed on drop via `Zeroizing`.
///
/// Owned (no lifetime parameter) because callers hold the resolved value
/// across multiple operations, including recover's post-resume relock cycle.
pub enum OpenCredential {
    Passphrase(Zeroizing<String>),
    KeyFile(PathBuf),
}

impl OpenCredential {
    /// Borrowed view for callers that take a `Credential<'_>` without
    /// taking ownership of the resolved secret or keyfile path.
    pub fn as_borrowed(&self) -> Credential<'_> {
        match self {
            OpenCredential::Passphrase(pp) => Credential::Passphrase(pp.as_str()),
            OpenCredential::KeyFile(kf) => Credential::KeyFile(kf.as_path()),
        }
    }
}

impl std::fmt::Debug for OpenCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenCredential::Passphrase(_) => f.write_str("Passphrase(<redacted>)"),
            OpenCredential::KeyFile(path) => f.debug_tuple("KeyFile").field(path).finish(),
        }
    }
}

/// Resolve credential flag inputs into an owned, fully-resolved
/// `OpenCredential`. ALWAYS reads -- callers decide whether to invoke this,
/// because the "should we prompt now?" rule differs by command:
///
/// - `cmd_unlock` skips this call entirely when `plan.to_unlock` is empty
///   (the no-prompt-when-all-mappers-open UX rule).
/// - `cmd_recover` calls this whenever the pool is not yet mounted, even
///   if the initial plan's `to_unlock` is empty, because the post-mount
///   relock cycle will close every mapper and need to reopen them.
///
/// Resolution order: `key_file` (if provided) -> passphrase
/// (file/stdin/TTY).
pub fn resolve_credential(
    passphrase_stdin: bool,
    passphrase_file: Option<&Path>,
    key_file: Option<&Path>,
) -> Result<OpenCredential, LuksError> {
    if let Some(kf) = key_file {
        return Ok(OpenCredential::KeyFile(kf.to_path_buf()));
    }
    let pp = luks::read_passphrase(passphrase_file, passphrase_stdin)?;
    Ok(OpenCredential::Passphrase(pp))
}
