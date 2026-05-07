use zeroize::Zeroizing;

/// In-memory LUKS passphrase boundary.
///
/// Wraps `Zeroizing<String>` so plaintext is scrubbed on drop, avoids
/// accidental `Clone`, and funnels subprocess handoff through
/// `expose_secret()` as the documented egress point.
pub struct Passphrase(Zeroizing<String>);

impl Passphrase {
    /// Construct from an already-zeroizing read buffer without copying the
    /// plaintext into an intermediate unprotected owner.
    pub fn from_zeroizing(z: Zeroizing<String>) -> Self {
        Self(z)
    }

    /// Plaintext access for subprocess stdin handoff and narrow validation
    /// paths where the caller already owns the secret boundary.
    pub fn expose_secret(&self) -> &str {
        self.0.as_str()
    }

    /// Byte length used by validation tests without exposing plaintext.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Empty-state query used by tests and validators without exposing
    /// plaintext.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Passphrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Passphrase(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passphrase(s: &str) -> Passphrase {
        Passphrase::from_zeroizing(Zeroizing::new(s.to_owned()))
    }

    #[test]
    fn passphrase_debug_redacts() {
        for plaintext in ["hunter2", "correct horse battery staple", "short"] {
            let rendered = format!("{:?}", passphrase(plaintext));
            assert!(rendered.contains("<redacted>"));
            assert!(
                !rendered.contains(plaintext),
                "debug output leaked plaintext: {rendered}"
            );
        }
    }
}
