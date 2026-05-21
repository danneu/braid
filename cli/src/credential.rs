use crate::credential_verify::Credential;
use crate::luks::{self, LuksError};
use crate::secret::Passphrase;
use std::path::{Path, PathBuf};

/// Owned, fully-resolved credential ready to drive `cryptsetup open`.
/// Passphrase plaintext is scrubbed on drop by the `Passphrase` owner.
///
/// Owned (no lifetime parameter) because callers hold the resolved value
/// across multiple operations, including recover's post-resume relock cycle.
pub enum OpenCredential {
    Passphrase(Passphrase),
    KeyFile(PathBuf),
}

impl OpenCredential {
    /// Borrowed view for callers that take a `Credential<'_>` without
    /// taking ownership of the resolved secret or keyfile path.
    pub fn as_borrowed(&self) -> Credential<'_> {
        match self {
            OpenCredential::Passphrase(pp) => Credential::Passphrase(pp),
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
/// - `cmd_recover` calls this eagerly only for `Replace::PoolMutation` (the
///   relock cycle closes every mapper and must reopen with the same
///   credential); other op kinds defer to the existing seams -- the inline
///   resolve in the unlock-and-mount branch for the bootstrap case, and the
///   lazy `recover_passphrase` helper for closed-mapper / replay-verify cases
///   discovered after mount.
///
/// Resolution order: `key_file` (if provided) -> passphrase
/// (file/stdin/TTY).
pub fn resolve_credential(
    passphrase_stdin: bool,
    passphrase_file: Option<&Path>,
    key_file: Option<&Path>,
) -> Result<OpenCredential, LuksError> {
    if let Some(kf) = key_file {
        luks::validate_user_keyfile_path(kf)?;
        return Ok(OpenCredential::KeyFile(kf.to_path_buf()));
    }
    let pp = luks::read_passphrase(passphrase_file, passphrase_stdin)?;
    Ok(OpenCredential::Passphrase(pp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroize::Zeroizing;

    fn passphrase(s: &str) -> Passphrase {
        Passphrase::from_zeroizing(Zeroizing::new(s.to_owned()))
    }

    // Intent: `--key-file` credential resolution rejects wrong-size keyfiles.
    // Why it exists: unlock resolves the keyfile path here, so this boundary
    //   must fail before any cryptsetup open attempt can use an invalid file.
    // Scenario: an admin runs `braid unlock --key-file` with a short file.
    #[test]
    fn resolve_credential_rejects_wrong_size_key_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let key_file = dir.path().join("braid.key");
        std::fs::write(&key_file, b"short").unwrap();

        let err = resolve_credential(false, None, Some(&key_file))
            .expect_err("wrong-size keyfile must fail");

        match err {
            LuksError::Validation(msg) => {
                assert!(msg.contains("4096"), "expected 4096 in: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn open_credential_debug_redacts_passphrase() {
        let rendered = format!(
            "{:?}",
            OpenCredential::Passphrase(passphrase("debug-redaction-secret"))
        );

        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("debug-redaction-secret"));

        let key_file = OpenCredential::KeyFile(PathBuf::from("/run/braid/braid.key"));
        let key_file_rendered = format!("{key_file:?}");
        assert!(key_file_rendered.contains("/run/braid/braid.key"));
    }
}
