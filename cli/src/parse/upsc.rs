//! `upsc <name>` output parser.
//!
//! NUT's upsc client emits one `key: value` pair per line (see
//! `reference/nut/clients/upsc.c:141`). Plan 1 only consumes
//! `ups.status` (a space-separated list of flags, per
//! `reference/nut/clients/upsmon.c:1404`) plus a verbatim passthrough
//! of every other line for `braid ups status`. The richer data model
//! (battery, input voltage, test result, load/watts) is plan 2's
//! responsibility.
//!
//! Daemon-down handling: we treat a non-zero `upsc` exit with an empty
//! or status-less stdout as an error. Callers (`cmd_ups_status`,
//! `check_ups_not_on_battery`) fail-closed on that condition.

use crate::cmd::RawCommandOutput;
use crate::parse::types::{UpscOutput, UpsStatusFlag};
use crate::parse::ParseError;

impl UpsStatusFlag {
    fn from_token(tok: &str) -> Self {
        match tok {
            "OL" => Self::Ol,
            "OB" => Self::Ob,
            "LB" => Self::Lb,
            "RB" => Self::Rb,
            "HB" => Self::Hb,
            "CHRG" => Self::Chrg,
            "DISCHRG" => Self::Dischrg,
            "CAL" => Self::Cal,
            "BYPASS" => Self::Bypass,
            "OFF" => Self::Off,
            "OVER" => Self::Over,
            "TRIM" => Self::Trim,
            "BOOST" => Self::Boost,
            "FSD" => Self::Fsd,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

/// Parse `upsc <name>` output.
///
/// Fails (`ParseError::CommandFailed`) if the subprocess reported a
/// non-zero exit status. On success, splits `ups.status` into
/// `status_flags` and copies every other `key: value` line verbatim
/// into `extra`.
pub fn parse_upsc(raw: &RawCommandOutput) -> Result<UpscOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: "upsc".to_owned(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.trim().to_owned(),
        });
    }

    let mut status_flags = std::collections::HashSet::new();
    let mut extra = std::collections::BTreeMap::new();

    for line in raw.stdout.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key == "ups.status" {
            for tok in value.split_ascii_whitespace() {
                status_flags.insert(UpsStatusFlag::from_token(tok));
            }
        } else {
            extra.insert(key.to_owned(), value.to_owned());
        }
    }

    Ok(UpscOutput {
        status_flags,
        extra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "upsc ups".to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    // Intent: parse_upsc recognizes OL as the sole flag on a healthy UPS.
    // Why: preflight treats "status set contains OB or LB" as refuse; "status
    // set equals {OL}" must therefore be recognized, not treated as unknown.
    // Scenario: typical `upsc ups` output with a UPS on utility power.
    #[test]
    fn parses_ol_flag() {
        let out = parse_upsc(&ok("ups.status: OL\nbattery.charge: 100\n")).unwrap();
        assert!(out.status_flags.contains(&UpsStatusFlag::Ol));
        assert_eq!(out.status_flags.len(), 1);
        assert_eq!(out.extra.get("battery.charge"), Some(&"100".to_owned()));
    }

    // Intent: parse_upsc splits multi-token ups.status into separate flags.
    // Why: upsmon declares critical only when ST_ONBATT and ST_LOWBATT are
    // both set (reference/nut/clients/upsmon.c:1404). Any parser that treated
    // "OB LB" as one opaque flag would miss either condition for preflight.
    // Scenario: UPS has been running on battery long enough that the LB
    // threshold fired -- both flags are present in the same status string.
    #[test]
    fn parses_ob_lb_combined() {
        let out = parse_upsc(&ok("ups.status: OB LB\n")).unwrap();
        assert!(out.status_flags.contains(&UpsStatusFlag::Ob));
        assert!(out.status_flags.contains(&UpsStatusFlag::Lb));
    }

    // Intent: parse_upsc preserves unknown flag tokens via Unknown(String).
    // Why: NUT adds flag tokens over time (e.g. newer firmwares surface novel
    // states). Dropping unknowns silently would hide them from `braid ups
    // status`; fail-closed behavior lives in the caller, not the parser.
    // Scenario: `upsc` emits a token braid has not shipped support for yet.
    #[test]
    fn preserves_unknown_flag_verbatim() {
        let out = parse_upsc(&ok("ups.status: OL NEWFLAG\n")).unwrap();
        assert!(out
            .status_flags
            .contains(&UpsStatusFlag::Unknown("NEWFLAG".to_owned())));
    }

    // Intent: parse_upsc returns empty status_flags for absent or empty
    // ups.status. Preflight treats empty-set as fail-closed, so the parser
    // does not need to invent a sentinel.
    // Why: a real dummy-ups fixture with a blank `.dev` file emits no
    // ups.status line until the driver fills one in. Parser must not panic
    // or synthesize flags.
    // Scenario: operator started dummy-ups against a stub file before the
    // first status write arrived.
    #[test]
    fn empty_status_produces_no_flags() {
        let out = parse_upsc(&ok("battery.charge: 100\ndriver.name: usbhid-ups\n")).unwrap();
        assert!(out.status_flags.is_empty());
        assert_eq!(out.extra.len(), 2);
    }

    // Intent: parse_upsc returns CommandFailed when the subprocess exited
    // non-zero -- upsc's behavior when the daemon is unreachable.
    // Why: caller-side fail-closed logic in check_ups_not_on_battery and
    // cmd_ups_status distinguishes daemon-down from malformed output.
    // Scenario: upsd.service is inactive when operator runs `upsc ups`.
    #[test]
    fn daemon_down_is_command_failed() {
        let raw = RawCommandOutput {
            cmd: "upsc ups".to_owned(),
            stdout: String::new(),
            stderr: "Error: Connection failure: Connection refused".to_owned(),
            exit_status: 1,
        };
        let err = parse_upsc(&raw).unwrap_err();
        assert!(
            matches!(err, ParseError::CommandFailed { .. }),
            "expected CommandFailed, got {err:?}"
        );
    }

    // Intent: parse_upsc accepts the minimal hand-written fixture for the
    // on-utility-power state and produces {OL} plus the extra tail.
    // Why: freezing the fixture contract here guards against later refactors
    // that would silently change what preflight and `braid ups status` see.
    // Scenario: smoke test of the `upsc-online.txt` committed fixture.
    #[test]
    fn parses_online_fixture() {
        let fixture = include_str!("../../tests/fixtures/nut/upsc-online.txt");
        let out = parse_upsc(&ok(fixture)).unwrap();
        assert!(out.status_flags.contains(&UpsStatusFlag::Ol));
        assert!(!out.status_flags.contains(&UpsStatusFlag::Ob));
        assert!(out.extra.contains_key("battery.charge"));
    }

    // Intent: the `upsc-onbattery-low.txt` fixture produces {OB, LB}.
    // Why: this is the preflight refuse case and the upsmon critical-state
    // trigger. The whole safety core depends on parsing that combination.
    // Scenario: operator captures `upsc` output during a simulated outage.
    #[test]
    fn parses_onbattery_low_fixture() {
        let fixture = include_str!("../../tests/fixtures/nut/upsc-onbattery-low.txt");
        let out = parse_upsc(&ok(fixture)).unwrap();
        assert!(out.status_flags.contains(&UpsStatusFlag::Ob));
        assert!(out.status_flags.contains(&UpsStatusFlag::Lb));
    }

    // Intent: the `upsc-daemon-down.stderr` fixture surfaces as an error.
    // Why: even at the fixture layer, daemon-down must stay CommandFailed --
    // a silent Ok({}) would let preflight pass when upsd is unreachable.
    // Scenario: operator runs `braid ups status` while upsd is stopped.
    #[test]
    fn parses_daemon_down_fixture() {
        let stderr = include_str!("../../tests/fixtures/nut/upsc-daemon-down.stderr");
        let raw = RawCommandOutput {
            cmd: "upsc ups".to_owned(),
            stdout: String::new(),
            stderr: stderr.to_owned(),
            exit_status: 1,
        };
        let err = parse_upsc(&raw).unwrap_err();
        assert!(matches!(err, ParseError::CommandFailed { .. }));
    }
}
