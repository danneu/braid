use time::PrimitiveDateTime;
use time::macros::format_description;

/// Parses a ctime-formatted timestamp string.
///
/// Example: "Tue Feb 24 02:00:07 2026" → PrimitiveDateTime
pub(super) fn parse_ctime(s: &str) -> Option<PrimitiveDateTime> {
    // "Tue Feb 24 02:00:07 2026" — ctime format from btrfs scrub status
    let fmt = format_description!(
        "[weekday repr:short] [month repr:short] [day padding:space] [hour]:[minute]:[second] [year]"
    );
    PrimitiveDateTime::parse(s, &fmt).ok()
}

/// Parses "H:MM:SS" duration string to total seconds.
///
/// Example: "0:03:15" → 195
pub(super) fn parse_duration_hms(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: u64 = parts[0].parse().ok()?;
    let m: u64 = parts[1].parse().ok()?;
    let s: u64 = parts[2].parse().ok()?;
    h.checked_mul(3600)?
        .checked_add(m.checked_mul(60)?)?
        .checked_add(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Intent: parse_duration_hms rejects durations whose seconds total would
    // overflow u64.
    // Why it exists: btrfs scrub status duration text is external tool output,
    // so a malformed but numeric duration must not panic or wrap.
    // Scenario: corrupt scrub status output reports an astronomically large
    // hour count.
    #[test]
    fn parse_duration_hms_overflow_returns_none() {
        assert_eq!(parse_duration_hms("99999999999999999:00:00"), None);
    }
}
