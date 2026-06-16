//! `upsc <name>` output parser.
//!
//! NUT's `upsc` client emits one `key: value` pair per line. Source,
//! nut 2.8.4, clients/upsc.c (fn `list_vars`):
//! ```c
//! printf("%s: %s\n", answer[2], answer[3]);
//! ```
//!
//! This parser splits the familiar keys (`ups.status`, `battery.*`,
//! `input.*`, `ups.load`, `ups.realpower.nominal`, `ups.test.result`,
//! `device.*` / `ups.mfr` / `ups.model` / `ups.serial`) into the typed
//! `UpscOutput` shape, and keeps every other line verbatim in `extra` so
//! unfamiliar driver keys are still observable via `braid ups status --json`.
//!
//! This parser is infallible by design: malformed or unknown fields are
//! omitted from typed fields or preserved in `extra`. Subprocess invocation
//! and non-zero `upsc` exits are classified by `crate::ups::query_ups`.

use crate::parse::types::{BatteryFields, DeviceFields, InputFields, UpsStatusFlag, UpscOutput};

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
            "TESTFAIL" => Self::TestFail,
            "COMMBAD" => Self::CommBad,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

/// Parse `upsc <name>` output.
///
/// Walks `key: value` lines and routes each known key to a typed field;
/// unrecognised keys land in `extra`.
pub fn parse_upsc(stdout: &str) -> UpscOutput {
    let mut status_flags = Vec::new();
    let mut battery = BatteryFields::default();
    let mut load_pct: Option<u8> = None;
    let mut realpower_nominal_watts: Option<u32> = None;
    let mut input = InputFields::default();
    let mut test_result: Option<String> = None;
    let mut device = DeviceFields::default();
    // Fallbacks from `ups.mfr` / `ups.model` / `ups.serial` -- only used
    // when no corresponding `device.*` key is present.
    let mut ups_mfr: Option<String> = None;
    let mut ups_model: Option<String> = None;
    let mut ups_serial: Option<String> = None;
    let mut extra = std::collections::BTreeMap::new();

    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "ups.status" => {
                for tok in value.split_ascii_whitespace() {
                    let flag = UpsStatusFlag::from_token(tok);
                    if !status_flags.contains(&flag) {
                        status_flags.push(flag);
                    }
                }
            }
            "battery.charge" => battery.charge_pct = parse_pct(value),
            "battery.runtime" => battery.runtime_secs = value.parse().ok(),
            "battery.runtime.low" => battery.runtime_low_secs = value.parse().ok(),
            "battery.voltage" => battery.voltage = some_non_empty(value),
            "battery.type" => battery.type_ = some_non_empty(value),
            "battery.mfr.date" => battery.mfr_date = some_non_empty(value),
            "ups.load" => load_pct = parse_pct(value),
            "ups.realpower.nominal" => realpower_nominal_watts = value.parse().ok(),
            "input.voltage" => input.voltage = some_non_empty(value),
            "input.transfer.low" => input.transfer_low = some_non_empty(value),
            "input.transfer.high" => input.transfer_high = some_non_empty(value),
            "input.sensitivity" => input.sensitivity = some_non_empty(value),
            "ups.test.result" => test_result = some_non_empty(value),
            "device.model" => device.model = some_non_empty(value),
            "device.mfr" => device.mfr = some_non_empty(value),
            "device.serial" => device.serial = some_non_empty(value),
            "device.type" => device.type_ = some_non_empty(value),
            "ups.model" => ups_model = some_non_empty(value),
            "ups.mfr" => ups_mfr = some_non_empty(value),
            "ups.serial" => ups_serial = some_non_empty(value),
            _ => {
                extra.insert(key.to_owned(), value.to_owned());
            }
        }
    }

    // Fold `ups.mfr/model/serial` into `device` only when the `device.*`
    // variant was absent. Drivers that emit both prefer the `device.*`
    // spelling; we preserve that preference.
    if device.model.is_none() {
        device.model = ups_model;
    }
    if device.mfr.is_none() {
        device.mfr = ups_mfr;
    }
    if device.serial.is_none() {
        device.serial = ups_serial;
    }

    UpscOutput {
        status_flags,
        battery,
        load_pct,
        realpower_nominal_watts,
        input,
        test_result,
        device,
        extra,
    }
}

fn some_non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_owned())
    }
}

/// Parse a percent value like `"100"` or `"47.5"` into `0..=100`. The
/// integer part alone is the gate: `intpart > 100` returns `None`,
/// `intpart <= 100` returns `Some(intpart)`, fractional part discarded.
///
/// Floor-after-split is deliberately conservative at both ends --
/// `99.9 -> Some(99)` so a not-quite-full UPS does not misrepresent,
/// and `100.5 -> Some(100)` so borderline driver overshoot (firmware
/// FP drift) is tolerated. Real driver bugs like `200` still trip the
/// `intpart > 100` gate; see the `pct_out_of_range_is_none` test.
fn parse_pct(s: &str) -> Option<u8> {
    let intpart = s.split_once('.').map(|(a, _)| a).unwrap_or(s);
    let n: u16 = intpart.parse().ok()?;
    if n > 100 { None } else { Some(n as u8) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Intent: parse_upsc recognizes OL as the sole flag on a healthy UPS.
    // Why: TUI severity and UPS preflight both key on the typed Ol flag:
    // the TUI renders Ol green, and preflight requires Ol before starting
    // pool mutations.
    // Scenario: typical `upsc ups` output with a UPS on utility power.
    #[test]
    fn parses_ol_flag() {
        let out = parse_upsc("ups.status: OL\nbattery.charge: 100\n");
        assert!(out.status_flags.contains(&UpsStatusFlag::Ol));
        assert_eq!(out.status_flags.len(), 1);
        assert_eq!(out.battery.charge_pct, Some(100));
    }

    // Intent: parse_upsc splits multi-token ups.status into separate flags.
    // Why: upsmon declares critical only when ST_ONBATT and ST_LOWBATT are
    // both set (reference/nut/clients/upsmon.c:1404). Any parser that treated
    // "OB LB" as one opaque flag would miss either condition for preflight.
    // Scenario: UPS has been running on battery long enough that the LB
    // threshold fired -- both flags are present in the same status string.
    #[test]
    fn parses_ob_lb_combined() {
        let out = parse_upsc("ups.status: OB LB\n");
        assert!(out.status_flags.contains(&UpsStatusFlag::Ob));
        assert!(out.status_flags.contains(&UpsStatusFlag::Lb));
    }

    // Intent: parse_upsc preserves upsc's emission order in status_flags.
    // Why it exists: the human render and --json output both iterate
    // status_flags verbatim, so parser insertion order is the script-facing
    // contract.
    // Scenario: calibration with charging plus replace-batt advisory, which
    // exercises four distinct flags in NUT's canonical emission order.
    #[test]
    fn parse_upsc_preserves_status_flag_order() {
        let out = parse_upsc("ups.status: CAL OL CHRG RB\n");
        assert_eq!(
            out.status_flags,
            vec![
                UpsStatusFlag::Cal,
                UpsStatusFlag::Ol,
                UpsStatusFlag::Chrg,
                UpsStatusFlag::Rb,
            ],
        );
    }

    // Intent: duplicates in ups.status collapse to a single flag in
    // first-seen order.
    // Why it exists: Vec replaces HashSet for ordering reasons, but set
    // semantics for membership must survive a driver that repeats a token.
    // Scenario: hand-crafted `ups.status: OL OB OL`, not produced by a real
    // NUT driver but covered by the infallible parser contract.
    #[test]
    fn parse_upsc_dedupes_repeated_status_tokens() {
        let out = parse_upsc("ups.status: OL OB OL\n");
        assert_eq!(out.status_flags, vec![UpsStatusFlag::Ol, UpsStatusFlag::Ob]);
    }

    // Intent: parse_upsc preserves unknown flag tokens via Unknown(String).
    // Why: NUT adds flag tokens over time (e.g. newer firmwares surface novel
    // states). Dropping unknowns silently would hide them from `braid ups
    // status`; fail-closed behavior lives in the caller, not the parser.
    // Scenario: `upsc` emits a token braid has not shipped support for yet.
    #[test]
    fn preserves_unknown_flag_verbatim() {
        let out = parse_upsc("ups.status: OL NEWFLAG\n");
        assert!(
            out.status_flags
                .contains(&UpsStatusFlag::Unknown("NEWFLAG".to_owned()))
        );
    }

    // Intent: parse_upsc recognises TESTFAIL and COMMBAD as typed variants.
    // Why: the TUI's severity mapping wants these to render red, which is
    // only reliable if the parser classifies them instead of dumping them
    // into Unknown(String) (where string-matching becomes load-bearing).
    // Scenario: driver surfaces TESTFAIL in ups.status alongside OL.
    #[test]
    fn recognises_testfail_and_commbad_as_typed() {
        let out = parse_upsc("ups.status: OL TESTFAIL COMMBAD\n");
        assert!(out.status_flags.contains(&UpsStatusFlag::Ol));
        assert!(out.status_flags.contains(&UpsStatusFlag::TestFail));
        assert!(out.status_flags.contains(&UpsStatusFlag::CommBad));
    }

    // Intent: parse_upsc returns empty status_flags when ups.status is absent.
    // Preflight treats empty-set as fail-closed, so the parser does not need
    // to invent a sentinel.
    // Why: a real dummy-ups fixture with a blank `.dev` file emits no
    // ups.status line until the driver fills one in. Parser must not panic
    // or synthesize flags.
    // Scenario: operator started dummy-ups against a stub file before the
    // first status write arrived.
    #[test]
    fn absent_status_produces_no_flags() {
        let out = parse_upsc("battery.charge: 50\ndriver.name: usbhid-ups\n");
        assert!(out.status_flags.is_empty());
        assert_eq!(out.battery.charge_pct, Some(50));
        // driver.name is not a typed key yet -> lands in `extra`.
        assert_eq!(out.extra.get("driver.name"), Some(&"usbhid-ups".to_owned()));
    }

    // Intent: an explicit-but-empty `ups.status:` value yields zero flags --
    // the same empty set as an absent key, but via a different code path
    // (the match arm runs; the split loop iterates zero times).
    // Why it exists: this empty-set result rides entirely on
    // `split_ascii_whitespace` yielding no tokens for "" -- there is no
    // explicit empty guard. A swap to `split(' ')` would emit a stray
    // `Unknown("")`, silently breaking the `ups_status_empty` JSON warning
    // and the `(unknown -- ups.status missing)` human sentinel. Until now
    // only the `emptyups.dev` VM canary (tests/cli/braid-status-ups) covered
    // this path; this is its cheap unit mirror.
    // Scenario: a driver publishes ups.status with no tokens -- the dummy-ups
    // `emptyups.dev` fixture sets an explicit empty status line, since the
    // driver would otherwise default a missing status to OL.
    #[test]
    fn empty_status_value_produces_no_flags() {
        let out = parse_upsc("battery.charge: 55\nups.status:\n");
        assert!(out.status_flags.is_empty());
        // charge still parses -> the empty status line was consumed, not that
        // the whole parse degraded to empty.
        assert_eq!(out.battery.charge_pct, Some(55));
        // The empty status line routes to the typed arm, not `extra`.
        assert!(out.extra.get("ups.status").is_none());
    }

    // Intent: parse_upsc populates the full typed model when all expected
    // keys are present -- battery, load, realpower, input, test result,
    // device.
    // Why: this is the shape `braid ups status` and the TUI consume;
    // regression here would silently empty the curated summary.
    // Scenario: APC Back-UPS-style output with the full key set.
    #[test]
    fn parses_rich_model_fields() {
        let stdout = "\
battery.charge: 95\n\
battery.charge.low: 10\n\
battery.runtime: 1800\n\
battery.runtime.low: 120\n\
battery.type: PbAc\n\
battery.voltage: 27.0\n\
battery.mfr.date: 2023/04/12\n\
device.mfr: APC\n\
device.model: Back-UPS ES 550G\n\
device.serial: 3B1234X56789\n\
device.type: ups\n\
input.voltage: 120.0\n\
input.transfer.low: 88\n\
input.transfer.high: 142\n\
input.sensitivity: medium\n\
ups.load: 17\n\
ups.realpower.nominal: 330\n\
ups.status: OL\n\
ups.test.result: Done and passed\n\
";
        let out = parse_upsc(stdout);
        assert!(out.status_flags.contains(&UpsStatusFlag::Ol));
        assert_eq!(out.battery.charge_pct, Some(95));
        assert_eq!(out.battery.runtime_secs, Some(1800));
        assert_eq!(out.battery.runtime_low_secs, Some(120));
        assert_eq!(out.battery.type_.as_deref(), Some("PbAc"));
        assert_eq!(out.battery.voltage.as_deref(), Some("27.0"));
        assert_eq!(out.battery.mfr_date.as_deref(), Some("2023/04/12"));
        assert_eq!(out.load_pct, Some(17));
        assert_eq!(out.realpower_nominal_watts, Some(330));
        assert_eq!(out.input.voltage.as_deref(), Some("120.0"));
        assert_eq!(out.input.transfer_low.as_deref(), Some("88"));
        assert_eq!(out.input.transfer_high.as_deref(), Some("142"));
        assert_eq!(out.input.sensitivity.as_deref(), Some("medium"));
        assert_eq!(out.test_result.as_deref(), Some("Done and passed"));
        assert_eq!(out.device.mfr.as_deref(), Some("APC"));
        assert_eq!(out.device.model.as_deref(), Some("Back-UPS ES 550G"));
        assert_eq!(out.device.serial.as_deref(), Some("3B1234X56789"));
        assert_eq!(out.device.type_.as_deref(), Some("ups"));
        // No stray extras: every documented key routed to a typed field.
        assert_eq!(
            out.extra.get("battery.charge.low"),
            Some(&"10".to_owned()),
            "battery.charge.low stays in extras -- no typed home yet"
        );
    }

    // Intent: `ups.mfr` / `ups.model` / `ups.serial` populate device fields
    // only when the `device.*` form is absent.
    // Why: different drivers prefer different keys, and some emit both. If
    // both spellings are present, the `device.*` value wins; if only
    // `ups.*` is present, the parser still surfaces it.
    // Scenario: old APC firmwares emit only `ups.model`; newer firmwares
    // emit both.
    #[test]
    fn ups_keys_are_fallback_for_device_fields() {
        // ups.* only -- populated.
        let only_ups = parse_upsc("ups.mfr: APC\nups.model: Back-UPS\n");
        assert_eq!(only_ups.device.mfr.as_deref(), Some("APC"));
        assert_eq!(only_ups.device.model.as_deref(), Some("Back-UPS"));
        // Both present -- device.* wins.
        let both = parse_upsc("device.mfr: DeviceMfr\nups.mfr: UpsMfr\n");
        assert_eq!(both.device.mfr.as_deref(), Some("DeviceMfr"));
    }

    // Intent: watts_estimated returns None unless both ingredients are set.
    // Why: the plan says the "estimated watts" line is omitted entirely
    // when either load% or realpower.nominal is missing. Centralising that
    // rule on UpscOutput prevents inconsistent render decisions.
    // Scenario: load present but nominal missing; nominal present but load
    // missing; both present.
    #[test]
    fn watts_estimated_requires_both_ingredients() {
        let only_load = parse_upsc("ups.load: 50\n");
        assert_eq!(only_load.watts_estimated(), None);
        let only_nominal = parse_upsc("ups.realpower.nominal: 330\n");
        assert_eq!(only_nominal.watts_estimated(), None);
        let both = parse_upsc("ups.load: 50\nups.realpower.nominal: 330\n");
        // 50 * 330 = 16500, / 100 = 165 (with rounding: +50 before div).
        assert_eq!(both.watts_estimated(), Some(165));
    }

    // Intent: watts_estimated handles maximum parsed nominal wattage without overflow.
    // Why it exists: NUT driver output is an ungated u32, and debug overflow
    // checks must not panic on syntactically valid values.
    // Scenario: buggy driver emits u32::MAX while load is 100%.
    #[test]
    fn watts_estimated_handles_max_nominal_watts() {
        let out = parse_upsc("ups.load: 100\nups.realpower.nominal: 4294967295\n");
        assert_eq!(out.watts_estimated(), Some(u32::MAX));
    }

    // Intent: percent values out of range round-trip to None.
    // Why: a driver bug that emits `battery.charge: 200` should not be
    // quietly clipped; callers render "--" and the typed field reflects
    // that the source was unreliable.
    // Scenario: malformed driver output.
    #[test]
    fn pct_out_of_range_is_none() {
        let out = parse_upsc("battery.charge: 200\nups.load: 999\n");
        assert_eq!(out.battery.charge_pct, None);
        assert_eq!(out.load_pct, None);
    }

    // Intent: percent parsing floors fractional values and rejects only
    // integer parts above 100.
    // Why it exists: this boundary policy is deliberate and documented in
    // parse_pct and commit bcbc435b, but clean-integer fixtures would miss a
    // refactor to rounding or decimal-rejecting parsing.
    // Scenario: a UPS driver reports fractional charge/load values around
    // 100% because of firmware floating-point drift, plus a genuinely
    // out-of-range 101 value.
    #[test]
    fn pct_floors_fraction_and_gates_on_intpart() {
        let near_full = parse_upsc("battery.charge: 99.9\nups.load: 100.5\n");
        assert_eq!(near_full.battery.charge_pct, Some(99));
        assert_eq!(near_full.load_pct, Some(100));

        let over = parse_upsc("battery.charge: 101.0\nups.load: 101\n");
        assert_eq!(over.battery.charge_pct, None);
        assert_eq!(over.load_pct, None);
    }

    // Intent: parse_upsc accepts the captured fixture for the
    // on-utility-power state and produces {OL} plus the typed tail.
    // Why: freezing the fixture contract here guards against later refactors
    // that would silently change what preflight and `braid ups status` see.
    // Scenario: smoke test of the `upsc-online.txt` committed fixture.
    #[test]
    fn parses_online_fixture() {
        let fixture = include_str!("../../tests/fixtures/nixos-26.05/upsc/upsc-online.txt");
        let out = parse_upsc(fixture);
        assert!(out.status_flags.contains(&UpsStatusFlag::Ol));
        assert!(!out.status_flags.contains(&UpsStatusFlag::Ob));
        assert_eq!(out.battery.charge_pct, Some(100));
        assert_eq!(out.device.model.as_deref(), Some("Back-UPS ES 550G"));
    }

    // Intent: the `upsc-lowbattery.txt` fixture produces {OB, LB}.
    // Why: this is the preflight refuse case and the upsmon critical-state
    // trigger. The whole safety core depends on parsing that combination.
    // Scenario: operator captures `upsc` output during a simulated outage.
    #[test]
    fn parses_lowbattery_fixture() {
        let fixture = include_str!("../../tests/fixtures/nixos-26.05/upsc/upsc-lowbattery.txt");
        let out = parse_upsc(fixture);
        assert!(out.status_flags.contains(&UpsStatusFlag::Ob));
        assert!(out.status_flags.contains(&UpsStatusFlag::Lb));
        assert_eq!(out.battery.charge_pct, Some(8));
    }
}
