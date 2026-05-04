//! `braid ups status` -- operator inspection of live NUT state.
//!
//! Missing or disabled config prints a helpful enable-hint and exits 0 --
//! `braid ups status` on a pool without UPS is not an error. Daemon-down
//! (non-zero `upsc` exit) is a hard error with a pointer at the upsd unit.
//!
//! `--json` emits a stable serialized `UpscOutput` (plus distinct error
//! shapes for the daemon-down and not-enabled cases) so scripts can key
//! off the parsed model without re-parsing `upsc` output themselves.

use crate::cmd::{CmdRequest, CommandRunner};
use crate::config::{ConfigError, Ups, config_read};
use crate::parse::parse_upsc;
use crate::parse::types::{UpsStatusFlag, UpscOutput};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum UpsError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("ups daemon not running -- check 'systemctl status upsd.service'")]
    DaemonDown,
    #[error("failed to serialize ups status: {0}")]
    Serialize(#[source] serde_json::Error),
}

/// `--json` output mode for `braid ups status`. Separate from
/// `UpscOutput` so the "not enabled" and "daemon down" branches have a
/// stable surface for scripting without piggy-backing on the parse model.
#[derive(Debug, serde::Serialize)]
#[serde(untagged)]
enum JsonReport<'a> {
    NotEnabled { error: &'static str },
    DaemonDown { error: &'static str },
    Ok(&'a UpscOutput),
}

pub fn cmd_ups_status<R: CommandRunner>(
    runner: &R,
    config_path: &Path,
    json: bool,
) -> Result<(), UpsError> {
    let config = config_read(config_path)?;
    let Some(ups_cfg) = config.ups() else {
        return print_not_enabled(json);
    };
    if !ups_cfg.enable {
        return print_not_enabled(json);
    }
    render_live(runner, ups_cfg, json)
}

fn print_not_enabled(json: bool) -> Result<(), UpsError> {
    if json {
        let payload = JsonReport::NotEnabled {
            error: "ups_not_enabled",
        };
        emit_json(&payload)?;
    } else {
        println!(
            "UPS support is not enabled. Set `braid.ups.enable = true` in\n\
             your NixOS configuration and rebuild to enable preflight\n\
             safety and low-battery shutdown."
        );
    }
    Ok(())
}

fn render_live<R: CommandRunner>(runner: &R, ups_cfg: &Ups, json: bool) -> Result<(), UpsError> {
    let raw = match runner.run(&CmdRequest::UpscQuery {
        name: ups_cfg.name.clone(),
    }) {
        Ok(r) => r,
        Err(_) => return emit_daemon_down(json),
    };
    let parsed = match parse_upsc(&raw) {
        Ok(p) => p,
        Err(_) => return emit_daemon_down(json),
    };
    if json {
        emit_json(&JsonReport::Ok(&parsed))?;
    } else {
        print!("{}", format_human(&ups_cfg.name, &parsed));
    }
    Ok(())
}

fn emit_daemon_down(json: bool) -> Result<(), UpsError> {
    if json {
        emit_json(&JsonReport::DaemonDown {
            error: "daemon_down",
        })?;
    }
    Err(UpsError::DaemonDown)
}

fn emit_json(payload: &JsonReport<'_>) -> Result<(), UpsError> {
    let text = serde_json::to_string_pretty(payload).map_err(UpsError::Serialize)?;
    println!("{}", text);
    Ok(())
}

/// Human render for `braid ups status`. Curated summary only -- raw
/// extras passthrough lives in `--json`.
///
/// Returns the rendered text with a trailing newline so the caller can
/// `print!` it without trimming; the string shape is stable so tests
/// can snapshot it without touching stdout.
pub fn format_human(name: &str, parsed: &UpscOutput) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "UPS: {}", name);
    let _ = writeln!(out, "Status: {}", format_status(&parsed.status_flags));
    match parsed.battery.charge_pct {
        Some(pct) => {
            let _ = writeln!(out, "Battery: {}%", pct);
        }
        None => {
            let _ = writeln!(out, "Battery: --");
        }
    }
    match parsed.battery.runtime_secs {
        Some(secs) => {
            let _ = writeln!(out, "Runtime: {}", format_runtime(secs));
        }
        None => {
            let _ = writeln!(out, "Runtime: --");
        }
    }
    match parsed.load_pct {
        Some(load) => match parsed.watts_estimated() {
            Some(w) => {
                let _ = writeln!(out, "Load: {}% ({} W estimated)", load, w);
            }
            None => {
                let _ = writeln!(out, "Load: {}%", load);
            }
        },
        None => {
            let _ = writeln!(out, "Load: --");
        }
    }
    if let Some(v) = parsed.input.voltage.as_deref() {
        let context = match (
            parsed.input.transfer_low.as_deref(),
            parsed.input.transfer_high.as_deref(),
        ) {
            (Some(low), Some(high)) => format!(" (transfer {}-{} V)", low, high),
            _ => String::new(),
        };
        let _ = writeln!(out, "Input: {} V{}", v, context);
    }
    if let Some(line) = format_device_line(parsed) {
        let _ = writeln!(out, "Device: {}", line);
    }
    if let Some(date) = parsed.battery.mfr_date.as_deref() {
        let _ = writeln!(out, "Battery manufactured: {}", date);
    }
    if let Some(result) = parsed.test_result.as_deref() {
        let _ = writeln!(out, "Last test: {}", result);
    }
    out
}

fn format_device_line(parsed: &UpscOutput) -> Option<String> {
    let model = parsed.device.model.as_deref();
    let mfr = parsed.device.mfr.as_deref();
    match (mfr, model) {
        (Some(mfr), Some(model)) => Some(format!("{} {}", mfr, model)),
        (None, Some(model)) => Some(model.to_owned()),
        (Some(mfr), None) => Some(mfr.to_owned()),
        (None, None) => None,
    }
}

/// Format a runtime in seconds as `H:MM` (or `M:SS` for sub-hour
/// durations). The pattern matches what typical UPS dashboards show so
/// operators can compare values across tools.
pub fn format_runtime(secs: u32) -> String {
    if secs >= 3600 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{}:{:02}", h, m)
    } else {
        let m = secs / 60;
        let s = secs % 60;
        format!("{}:{:02}", m, s)
    }
}

fn format_status(flags: &std::collections::HashSet<UpsStatusFlag>) -> String {
    if flags.is_empty() {
        return "(unknown -- ups.status missing)".to_owned();
    }
    let mut tokens: Vec<String> = flags.iter().map(UpsStatusFlag::as_token).collect();
    tokens.sort();
    tokens.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::parse::types::{BatteryFields, DeviceFields, InputFields};

    // Intent: format_status renders OL verbatim when the UPS is on utility power.
    // Why: operators triaging preflight failures need the rendered line to
    // match NUT's own token vocabulary; translating OL to "online" would
    // force them to cross-reference wording.
    // Scenario: `braid ups status` against a healthy UPS.
    #[test]
    fn format_status_ol() {
        let mut flags = std::collections::HashSet::new();
        flags.insert(UpsStatusFlag::Ol);
        assert_eq!(format_status(&flags), "OL");
    }

    // Intent: multi-flag status renders every token, sorted for stability.
    // Why: sorting makes unit tests deterministic and lets operators diff
    // two renders without spurious reordering noise.
    // Scenario: critical state -- UPS on battery, low battery threshold hit.
    #[test]
    fn format_status_ob_lb_sorted() {
        let mut flags = std::collections::HashSet::new();
        flags.insert(UpsStatusFlag::Ob);
        flags.insert(UpsStatusFlag::Lb);
        assert_eq!(format_status(&flags), "LB OB");
    }

    // Intent: empty flag set renders an explicit `unknown` sentinel.
    // Why: preflight fails closed on an empty set; `braid ups status` needs
    // to print something the operator can act on, not a blank line.
    // Scenario: dummy-ups fixture with no ups.status line yet.
    #[test]
    fn format_status_empty_is_unknown() {
        let flags = std::collections::HashSet::new();
        let rendered = format_status(&flags);
        assert!(rendered.contains("unknown"));
    }

    // Intent: format_runtime uses HH:MM for >= 1 hour, MM:SS for shorter.
    // Why: operators expect HH:MM for the typical "15 minutes left on battery"
    // message; sub-minute sprints during self-tests benefit from seconds
    // resolution.
    // Scenario: 1800s (30:00), 7260s (2:01), 45s (0:45).
    #[test]
    fn format_runtime_splits_on_hour_boundary() {
        assert_eq!(format_runtime(1800), "30:00");
        assert_eq!(format_runtime(7260), "2:01");
        assert_eq!(format_runtime(45), "0:45");
    }

    // Intent: JSON output of a healthy parse round-trips to a stable shape.
    // Why: the --json contract is "serde_json::to_string_pretty(UpscOutput)".
    // Snapshot-style coverage lives in golden tests; here we lock in the
    // minimum invariants (status_flags present, battery.charge_pct surfaced).
    // Scenario: unit coverage for the JSON branch without spinning up a VM.
    #[test]
    fn json_output_has_status_and_battery_keys() {
        let parsed = UpscOutput {
            status_flags: {
                let mut s = std::collections::HashSet::new();
                s.insert(UpsStatusFlag::Ol);
                s
            },
            battery: BatteryFields {
                charge_pct: Some(100),
                ..Default::default()
            },
            load_pct: None,
            realpower_nominal_watts: None,
            input: InputFields::default(),
            test_result: None,
            device: DeviceFields::default(),
            extra: std::collections::BTreeMap::new(),
        };
        let text =
            serde_json::to_string_pretty(&JsonReport::Ok(&parsed)).expect("serialize succeeds");
        assert!(text.contains("\"status_flags\""), "got: {text}");
        assert!(text.contains("\"battery\""), "got: {text}");
        assert!(
            text.contains("\"OL\""),
            "flag token appears verbatim: {text}"
        );
    }

    // Intent: not-enabled --json surfaces the stable error sentinel.
    // Why: scripts wrapping `braid ups status --json` key off
    // `.error == "ups_not_enabled"` without needing stderr parsing.
    // Scenario: unit test of the branch triggered when `braid.ups.enable`
    // is false or the config block is absent.
    #[test]
    fn json_not_enabled_has_sentinel_error() {
        let payload = JsonReport::NotEnabled {
            error: "ups_not_enabled",
        };
        let text = serde_json::to_string_pretty(&payload).unwrap();
        assert!(text.contains("\"error\""));
        assert!(text.contains("\"ups_not_enabled\""));
    }

    // Intent: daemon-down --json surfaces a distinct sentinel.
    // Why: script callers distinguish "unreachable UPS" from "no UPS
    // configured"; one is a transient failure, the other is steady state.
    // Scenario: unit test of `emit_daemon_down(true)` via JsonReport.
    #[test]
    fn json_daemon_down_has_sentinel_error() {
        let payload = JsonReport::DaemonDown {
            error: "daemon_down",
        };
        let text = serde_json::to_string_pretty(&payload).unwrap();
        assert!(text.contains("\"daemon_down\""));
    }

    // Intent: render_live routes daemon-down to UpsError::DaemonDown.
    // Why: CLI shell relies on that variant to print the error and
    // exit(1); if MissingMock (simulating a spawn failure) were surfaced
    // as a generic Config error, the wrapper would drop the intended
    // diagnostic.
    // Scenario: MockRunner with no UpscQuery mock seeded.
    #[test]
    fn render_live_daemon_down_surfaces_typed_error() {
        let runner = MockRunner::default();
        let cfg = Ups {
            enable: true,
            name: "ups".into(),
        };
        let err = render_live(&runner, &cfg, false).expect_err("daemon down expected");
        assert!(matches!(err, UpsError::DaemonDown));
    }

    // Intent: render_live returns DaemonDown on a non-zero `upsc` exit.
    // Why: parse_upsc also classifies non-zero exit as CommandFailed; the
    // outer render layer collapses both failure modes into DaemonDown so
    // operators see a single consistent error message.
    // Scenario: stubbed upsc returning exit 1 and an empty stdout.
    #[test]
    fn render_live_non_zero_exit_is_daemon_down() {
        let runner = MockRunner::default().with_output(
            CmdRequest::UpscQuery { name: "ups".into() },
            RawCommandOutput {
                cmd: "upsc ups".into(),
                stdout: String::new(),
                stderr: "Error: Connection refused".into(),
                exit_status: 1,
            },
        );
        let cfg = Ups {
            enable: true,
            name: "ups".into(),
        };
        let err = render_live(&runner, &cfg, false).expect_err("daemon down expected");
        assert!(matches!(err, UpsError::DaemonDown));
    }

    // --- Fixture-backed render snapshots ---
    //
    // The plan mandates a curated human summary against each captured
    // fixture. Insta snapshots lock the full wording; any edit to
    // format_human that changes a label, ordering, or sentinel
    // produces a visible diff the reviewer must accept.
    //
    // Each snapshot test also JSON-serializes the parsed model so the
    // `--json` contract is covered from the same fixture. This double
    // coverage is cheap (one parse, two serializers) and guards the
    // two outputs against drift relative to each other.

    fn parse_fixture(stdout: &str) -> UpscOutput {
        let raw = RawCommandOutput {
            cmd: "upsc ups".into(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        };
        crate::parse::parse_upsc(&raw).expect("fixture parses cleanly")
    }

    macro_rules! snap {
        ($value:expr) => {
            insta::with_settings!({ prepend_module_to_snapshot => false }, {
                insta::assert_snapshot!($value);
            });
        };
    }

    macro_rules! snap_json {
        ($value:expr) => {
            insta::with_settings!({ prepend_module_to_snapshot => false }, {
                insta::assert_json_snapshot!($value);
            });
        };
    }

    // Intent: online fixture renders the steady-state summary with OL,
    // full charge, full runtime, and APC device info.
    // Why: this is the UX operators will stare at 99% of the time. A
    // snapshot is the tightest possible guard against "someone reworded
    // a label and didn't notice the doctor docs still cite the old
    // wording."
    // Scenario: `braid ups status` against an APC Back-UPS on AC.
    #[test]
    fn snapshot_human_online() {
        let parsed = parse_fixture(include_str!(
            "../tests/fixtures/nixos-25.11/upsc/upsc-online.txt"
        ));
        snap!(format_human("ups", &parsed));
    }

    // Intent: online fixture JSON shape round-trips the rich typed
    // model. Asserts top-level keys + a representative nested field
    // per typed subsection, plus status flag membership.
    // Why: `--json` is a documented API surface for scripts. We keep
    // this as structural assertions rather than a byte-exact snapshot
    // because the captured fixture's `extra` map includes driver.*
    // fields that bump with every nixpkgs revision (driver.version,
    // per-state port filename). Checking structure keeps the contract
    // honest without brittle pins on values we do not own.
    // Scenario: `braid ups status --json | jq` against the fixture.
    #[test]
    fn json_online_fixture_has_expected_shape() {
        let parsed = parse_fixture(include_str!(
            "../tests/fixtures/nixos-25.11/upsc/upsc-online.txt"
        ));
        let text = serde_json::to_string_pretty(&JsonReport::Ok(&parsed)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let flags = value["status_flags"].as_array().unwrap();
        assert!(flags.iter().any(|v| v == "OL"), "OL in status_flags");
        assert_eq!(value["battery"]["charge_pct"], 100);
        assert_eq!(value["battery"]["runtime_secs"], 1800);
        assert_eq!(value["load_pct"], 17);
        assert_eq!(value["realpower_nominal_watts"], 330);
        assert_eq!(value["input"]["voltage"], "120.0");
        assert_eq!(value["device"]["model"], "Back-UPS ES 550G");
        assert_eq!(value["test_result"], "Done and passed");
    }

    // Intent: onbattery fixture renders Status: OB with a partial-charge
    // battery line.
    // Why: the yellow-severity state needs distinctive output so
    // operators can confirm at a glance that the UPS is on battery but
    // not yet critical. Regression here would blur the distinction
    // between OB and OL.
    // Scenario: sustained outage before battery.charge.low is crossed.
    #[test]
    fn snapshot_human_onbattery() {
        let parsed = parse_fixture(include_str!(
            "../tests/fixtures/nixos-25.11/upsc/upsc-onbattery.txt"
        ));
        snap!(format_human("ups", &parsed));
    }

    #[test]
    fn json_onbattery_fixture_has_expected_shape() {
        let parsed = parse_fixture(include_str!(
            "../tests/fixtures/nixos-25.11/upsc/upsc-onbattery.txt"
        ));
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&JsonReport::Ok(&parsed)).unwrap())
                .unwrap();
        let flags = value["status_flags"].as_array().unwrap();
        assert!(flags.iter().any(|v| v == "OB"), "OB in status_flags");
        assert!(
            !flags.iter().any(|v| v == "LB"),
            "LB not in onbattery fixture"
        );
        assert_eq!(value["input"]["voltage"], "0.0");
    }

    // Intent: lowbattery fixture renders Status: LB OB.
    // Why: this is the critical pair upsmon fires SHUTDOWNCMD on; the
    // human render must show both flags so the operator understands
    // that the host is about to power off.
    // Scenario: the outage crossed battery.charge.low; upsmon has
    // already triggered systemctl poweroff.
    #[test]
    fn snapshot_human_lowbattery() {
        let parsed = parse_fixture(include_str!(
            "../tests/fixtures/nixos-25.11/upsc/upsc-lowbattery.txt"
        ));
        snap!(format_human("ups", &parsed));
    }

    #[test]
    fn json_lowbattery_fixture_has_expected_shape() {
        let parsed = parse_fixture(include_str!(
            "../tests/fixtures/nixos-25.11/upsc/upsc-lowbattery.txt"
        ));
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&JsonReport::Ok(&parsed)).unwrap())
                .unwrap();
        let flags = value["status_flags"].as_array().unwrap();
        assert!(flags.iter().any(|v| v == "OB"), "OB in status_flags");
        assert!(flags.iter().any(|v| v == "LB"), "LB in status_flags");
        assert_eq!(value["battery"]["charge_pct"], 8);
    }

    // Intent: replace-battery fixture renders OL + RB without triggering
    // any low-battery wording.
    // Why: RB is advisory. If the human render treated RB as critical
    // (e.g. showing "on battery" wording), users would be whiplashed
    // into thinking they are losing power when they are not.
    // Scenario: an old UPS whose battery health has degraded; utility
    // power is fine, the advisory is just a reminder.
    #[test]
    fn snapshot_human_replace_battery() {
        let parsed = parse_fixture(include_str!(
            "../tests/fixtures/nixos-25.11/upsc/upsc-replace-battery.txt"
        ));
        snap!(format_human("ups", &parsed));
    }

    #[test]
    fn json_replace_battery_fixture_has_expected_shape() {
        let parsed = parse_fixture(include_str!(
            "../tests/fixtures/nixos-25.11/upsc/upsc-replace-battery.txt"
        ));
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&JsonReport::Ok(&parsed)).unwrap())
                .unwrap();
        let flags = value["status_flags"].as_array().unwrap();
        assert!(flags.iter().any(|v| v == "OL"));
        assert!(flags.iter().any(|v| v == "RB"));
        assert!(!flags.iter().any(|v| v == "OB"));
    }

    // Intent: daemon-down --json serializes to the sentinel error.
    // Why: scripts key off `.error == "daemon_down"`; snapshot proves
    // the JSON object has exactly that shape and nothing more.
    // Scenario: `braid ups status --json` while upsd.service is stopped.
    #[test]
    fn snapshot_json_daemon_down() {
        let payload = JsonReport::DaemonDown {
            error: "daemon_down",
        };
        snap_json!(&payload);
    }

    // Intent: not-enabled --json serializes to the not-enabled sentinel.
    // Why: distinguishes "UPS unreachable" from "UPS intentionally
    // disabled" so scripts can stay quiet in the latter case.
    // Scenario: host without `braid.ups.enable = true` -- `braid ups
    // status --json` still exits 0 with the ups_not_enabled sentinel.
    #[test]
    fn snapshot_json_not_enabled() {
        let payload = JsonReport::NotEnabled {
            error: "ups_not_enabled",
        };
        snap_json!(&payload);
    }
}
