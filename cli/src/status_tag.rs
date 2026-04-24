use std::fmt;

/// A 4-char bracket status tag for human CLI status rows.
///
/// Used by `lock`, `mount`, and `doctor` to prefix per-item outcome
/// lines. The bracketed form is always 6 columns wide so consecutive
/// rows align.
///
/// Distinct from the dry-run risk tag in `cmd::Step::print_dry_run`,
/// which uses an 11-wide column for `safe` / `destructive` etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTag {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl StatusTag {
    fn as_label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

impl fmt::Display for StatusTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:<4}]", self.as_label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_tag_pins_four_known_levels() {
        // Byte-pin cross-command contract: lock/mount/doctor all rely
        // on these exact strings for column alignment.
        assert_eq!(StatusTag::Ok.to_string(), "[ok  ]");
        assert_eq!(StatusTag::Warn.to_string(), "[warn]");
        assert_eq!(StatusTag::Fail.to_string(), "[fail]");
        assert_eq!(StatusTag::Skip.to_string(), "[skip]");
    }
}
