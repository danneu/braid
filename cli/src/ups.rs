//! `braid ups status` -- operator inspection of live NUT state.
//!
//! Missing or disabled config prints a helpful enable-hint and exits 0 --
//! `braid ups status` on a pool without UPS is not an error. Query failure
//! (non-zero `upsc` exit) is a hard error with `upsc`'s own stderr surfaced.
//!
//! `--json` emits a stable serialized `UpscOutput` (plus distinct error
//! shapes for query-failed, invocation-failed, and not-enabled cases) so
//! scripts can key off the parsed model without re-parsing `upsc` output
//! themselves.

use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::{ConfigError, config_read};
use crate::parse::parse_upsc;
use crate::parse::types::{UpsStatusFlag, UpscOutput};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum UpsError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("upsc query failed: {detail}")]
    QueryFailed { detail: String },
    #[error("internal: ups query failed (json sentinel already on stdout)")]
    QueryFailedJsonReported,
    #[error("failed to serialize ups status: {0}")]
    Serialize(#[source] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum UpsQueryError {
    /// Runner-level failure: spawn error, signal-killed child, stdin IO
    /// failure, or request/mode mismatch.
    #[error("upsc invocation failed: {0}")]
    InvocationFailed(#[from] CmdError),
    /// `upsc` exited non-zero. This covers an unreachable upsd daemon, an
    /// unknown UPS name, or another fatal NUT path.
    #[error("upsc query failed (exit {exit_code}): {stderr}")]
    QueryFailed { exit_code: i32, stderr: String },
}

pub fn query_ups<R: CommandRunner>(runner: &R, name: &str) -> Result<UpscOutput, UpsQueryError> {
    let raw = runner.run(&CmdRequest::UpscQuery {
        name: name.to_owned(),
    })?;
    if raw.exit_status != 0 {
        return Err(UpsQueryError::QueryFailed {
            exit_code: raw.exit_status,
            stderr: raw.stderr.trim().to_owned(),
        });
    }
    Ok(parse_upsc(&raw.stdout))
}

/// `--json` output mode for `braid ups status`. Separate from
/// `UpscOutput` so the "not enabled" and "query failed" branches have a
/// stable surface for scripting without piggy-backing on the parse model.
#[derive(Debug, serde::Serialize)]
#[serde(untagged)]
enum JsonReport<'a> {
    Error(ErrorReport<'a>),
    Ok(&'a UpscOutput),
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "error")]
enum ErrorReport<'a> {
    #[serde(rename = "ups_not_enabled")]
    NotEnabled,
    #[serde(rename = "query_failed")]
    QueryFailed { detail: &'a str },
    #[serde(rename = "invocation_failed")]
    InvocationFailed { detail: &'a str },
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
    let parsed = match query_ups(runner, &ups_cfg.name) {
        Ok(p) => p,
        Err(UpsQueryError::InvocationFailed(e)) => {
            return emit_invocation_failed(json, format!("invocation failed: {e}"));
        }
        Err(UpsQueryError::QueryFailed { exit_code, stderr }) => {
            return emit_query_failed(json, format!("exit {exit_code}: {stderr}"));
        }
    };
    if json {
        emit_json(&JsonReport::Ok(&parsed))?;
    } else {
        print!("{}", format_human(&ups_cfg.name, &parsed));
    }
    Ok(())
}

fn print_not_enabled(json: bool) -> Result<(), UpsError> {
    if json {
        let payload = JsonReport::Error(ErrorReport::NotEnabled);
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

fn emit_query_failed(json: bool, detail: String) -> Result<(), UpsError> {
    if json {
        emit_json(&JsonReport::Error(ErrorReport::QueryFailed {
            detail: &detail,
        }))?;
        return Err(UpsError::QueryFailedJsonReported);
    }
    Err(UpsError::QueryFailed { detail })
}

/// Keep invocation failures distinct in `--json` while preserving the
/// existing human error shape and shared exit-code wiring.
fn emit_invocation_failed(json: bool, detail: String) -> Result<(), UpsError> {
    if json {
        emit_json(&JsonReport::Error(ErrorReport::InvocationFailed {
            detail: &detail,
        }))?;
        return Err(UpsError::QueryFailedJsonReported);
    }
    Err(UpsError::QueryFailed { detail })
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
    line(
        &mut out,
        "Battery",
        parsed.battery.charge_pct.map(|pct| format!("{pct}%")),
    );
    line(
        &mut out,
        "Runtime",
        parsed.battery.runtime_secs.map(format_runtime),
    );
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

fn line<W: std::fmt::Write>(out: &mut W, label: &str, value: Option<impl std::fmt::Display>) {
    match value {
        Some(v) => {
            let _ = writeln!(out, "{label}: {v}");
        }
        None => {
            let _ = writeln!(out, "{label}: --");
        }
    }
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
    let mut tokens: Vec<&str> = flags.iter().map(UpsStatusFlag::as_token).collect();
    tokens.sort();
    tokens.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::MockRunner;
    use crate::parse::types::{BatteryFields, DeviceFields, InputFields};
    use crate::test_fixtures::{
        ups_query_connection_refused_no_newline, ups_query_connection_refused_with_newline,
        ups_query_healthy_minimal, ups_write_config,
    };

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
                s.insert(UpsStatusFlag::Unknown("NEWFLAG".into()));
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
        assert!(
            text.contains("\"NEWFLAG\""),
            "unknown flag token appears verbatim: {text}"
        );
    }

    // Intent: format_human emits exactly `Battery: --`, `Runtime: --`,
    // and `Load: --` when charge, runtime, and load are missing.
    // Why it exists: captured fixtures populate these fields, so snapshots
    // only pin the Some arm; this catches dash sentinel drift.
    // Scenario: a UPS driver has surfaced ups.status but no numeric telemetry.
    #[test]
    fn format_human_renders_dash_for_missing_optional_fields() {
        let parsed = UpscOutput {
            status_flags: {
                let mut s = std::collections::HashSet::new();
                s.insert(UpsStatusFlag::Ol);
                s
            },
            battery: BatteryFields::default(),
            load_pct: None,
            realpower_nominal_watts: None,
            input: InputFields::default(),
            test_result: None,
            device: DeviceFields::default(),
            extra: std::collections::BTreeMap::new(),
        };
        let rendered = format_human("ups", &parsed);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines.contains(&"Battery: --"), "got: {rendered}");
        assert!(lines.contains(&"Runtime: --"), "got: {rendered}");
        assert!(lines.contains(&"Load: --"), "got: {rendered}");
    }

    // Intent: format_human emits exactly `Load: 50%` when load is present
    // but nominal real power is missing.
    // Why it exists: snapshots only pin the `Some(load) + Some(watts)` shape;
    // this catches regressions in the asymmetric no-watts branch.
    // Scenario: a consumer UPS reports ups.load but not ups.realpower.nominal.
    #[test]
    fn format_human_load_omits_estimated_when_nominal_watts_missing() {
        let parsed = UpscOutput {
            status_flags: {
                let mut s = std::collections::HashSet::new();
                s.insert(UpsStatusFlag::Ol);
                s
            },
            battery: BatteryFields::default(),
            load_pct: Some(50),
            realpower_nominal_watts: None,
            input: InputFields::default(),
            test_result: None,
            device: DeviceFields::default(),
            extra: std::collections::BTreeMap::new(),
        };
        let rendered = format_human("ups", &parsed);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines.contains(&"Load: 50%"), "got: {rendered}");
        assert!(!rendered.contains("estimated"), "got: {rendered}");
    }

    // Intent: not-enabled --json surfaces the stable error sentinel.
    // Why: scripts wrapping `braid ups status --json` key off
    // `.error == "ups_not_enabled"` without needing stderr parsing.
    // Scenario: unit test of the branch triggered when `braid.ups.enable`
    // is false or the config block is absent.
    #[test]
    fn json_not_enabled_has_sentinel_error() {
        let payload = JsonReport::Error(ErrorReport::NotEnabled);
        let text = serde_json::to_string_pretty(&payload).unwrap();
        assert!(text.contains("\"error\""));
        assert!(text.contains("\"ups_not_enabled\""));
    }

    // Intent: query-failed --json surfaces a distinct sentinel with detail.
    // Why: script callers distinguish "unreachable UPS" from "no UPS
    // configured" and can display upsc's own stderr.
    // Scenario: unit test of `emit_query_failed(true)` via JsonReport.
    #[test]
    fn json_query_failed_has_sentinel_error_and_detail() {
        let payload = JsonReport::Error(ErrorReport::QueryFailed {
            detail: "exit 1: Error: Connection failure: Connection refused",
        });
        let text = serde_json::to_string_pretty(&payload).unwrap();
        assert!(text.contains("\"query_failed\""));
        assert!(text.contains("Connection failure"));
    }

    // Intent: the `invocation_failed` JSON payload carries both the
    // sentinel and the captured detail.
    // Why it exists: cheap structural guard against a future refactor
    // that drops the `detail` field or renames the sentinel without
    // updating the doc.
    // Scenario: unit-level mirror of the snapshot test, so a `detail`
    // shape regression fails loudly without an insta accept.
    #[test]
    fn json_invocation_failed_has_sentinel_error_and_detail() {
        let payload = JsonReport::Error(ErrorReport::InvocationFailed {
            detail: "invocation failed: upsc: No such file or directory",
        });
        let text = serde_json::to_string_pretty(&payload).unwrap();
        assert!(text.contains("\"invocation_failed\""));
        assert!(text.contains("No such file or directory"));
    }

    /*
     * Intent: query_ups returns QueryFailed when upsc exits non-zero.
     * Why it exists: non-zero exit is runner-integration state, not parser
     * state; callers need the captured stderr to diagnose daemon vs name
     * problems.
     * Scenario: upsd.service is stopped and upsc reports a connection
     * failure on stderr.
     */
    #[test]
    fn query_ups_returns_query_failed_on_non_zero_exit() {
        let (request, output) = ups_query_connection_refused_with_newline();
        let runner = MockRunner::default().with_output(request, output);

        let err = query_ups(&runner, "ups").expect_err("query failure expected");

        match err {
            UpsQueryError::QueryFailed { exit_code, stderr } => {
                assert_eq!(exit_code, 1);
                assert_eq!(stderr, "Error: Connection failure: Connection refused");
            }
            other => panic!("expected QueryFailed, got {other:?}"),
        }
    }

    /*
     * Intent: query_ups returns InvocationFailed for runner-level failures.
     * Why it exists: spawn failures, signal-killed children, stdin errors,
     * and request/mode mistakes are not the same as upsc's own non-zero
     * exit.
     * Scenario: MockRunner has no UpscQuery response seeded, producing
     * CmdError::MissingMock.
     */
    #[test]
    fn query_ups_returns_invocation_failed_on_missing_mock() {
        let runner = MockRunner::default();

        let err = query_ups(&runner, "ups").expect_err("invocation failure expected");

        assert!(
            matches!(err, UpsQueryError::InvocationFailed(_)),
            "expected InvocationFailed, got {err:?}"
        );
    }

    /*
     * Intent: query_ups returns parsed output when upsc exits zero.
     * Why it exists: the helper is now the production boundary between the
     * command runner and the infallible parser; healthy output must still
     * produce the same model as direct parsing.
     * Scenario: upsd is reachable and reports OL with a full battery.
     */
    #[test]
    fn query_ups_returns_ok_on_healthy_output() {
        let (request, output) = ups_query_healthy_minimal();
        let runner = MockRunner::default().with_output(request, output);

        let out = query_ups(&runner, "ups").expect("healthy upsc output parses");

        assert!(out.status_flags.contains(&UpsStatusFlag::Ol));
        assert_eq!(out.battery.charge_pct, Some(100));
    }

    // Intent: cmd_ups_status routes invocation failure to UpsError::QueryFailed
    // with an "invocation failed: ..." detail prefix.
    // Why it exists: CLI shell at main.rs prints e.to_string() to stderr for
    // non-JSON mode; the prefix tells operators that upsc could not even run.
    // Scenario: MockRunner with no UpscQuery mock seeded simulates a spawn
    // failure (CmdError::MissingMock).
    #[test]
    fn cmd_ups_status_invocation_failure_surfaces_typed_error() {
        let runner = MockRunner::default();
        let dir = tempfile::tempdir().unwrap();
        let cfg = ups_write_config(&dir, "ups");
        let err = cmd_ups_status(&runner, &cfg, false).expect_err("query failure expected");
        match &err {
            UpsError::QueryFailed { detail } => {
                assert!(detail.starts_with("invocation failed: "), "got: {detail}");
            }
            other => panic!("expected QueryFailed, got {other:?}"),
        }
        assert!(
            err.to_string()
                .starts_with("upsc query failed: invocation failed: "),
            "unexpected display: {err}"
        );
    }

    // Intent: cmd_ups_status under --json routes invocation failure
    // through QueryFailedJsonReported.
    // Why it exists: pins the contract main.rs depends on -- the
    // JSON-reported sentinel tells the CLI shell to exit 1 without
    // printing a duplicate human stderr line.
    // Scenario: MockRunner with no UpscQuery mock seeded simulates a
    // spawn failure (CmdError::MissingMock) under --json.
    #[test]
    fn cmd_ups_status_invocation_failure_json_returns_already_reported() {
        let runner = MockRunner::default();
        let dir = tempfile::tempdir().unwrap();
        let cfg = ups_write_config(&dir, "ups");
        let err = cmd_ups_status(&runner, &cfg, true).expect_err("query failure expected");
        assert!(
            matches!(err, UpsError::QueryFailedJsonReported),
            "got {err:?}"
        );
    }

    // Intent: cmd_ups_status returns QueryFailed with detail "exit N: <stderr>"
    // when upsc exits non-zero.
    // Why it exists: non-zero upsc exits carry stderr that diagnoses daemon vs
    // name problems; the rendered detail must surface that stderr verbatim.
    // Scenario: stubbed upsc returning exit 1 with a connection-refused stderr
    // (the realistic shape when upsd.service is down).
    #[test]
    fn cmd_ups_status_non_zero_exit_is_query_failed() {
        let (request, output) = ups_query_connection_refused_no_newline();
        let runner = MockRunner::default().with_output(request, output);
        let dir = tempfile::tempdir().unwrap();
        let cfg = ups_write_config(&dir, "ups");
        let err = cmd_ups_status(&runner, &cfg, false).expect_err("query failure expected");
        match &err {
            UpsError::QueryFailed { detail } => {
                assert!(detail.starts_with("exit 1: "), "got: {detail}");
                assert!(detail.contains("Connection failure"), "got: {detail}");
            }
            other => panic!("expected QueryFailed, got {other:?}"),
        }
        assert_eq!(
            err.to_string(),
            "upsc query failed: exit 1: Error: Connection failure: Connection refused"
        );
    }

    // Intent: emit_query_failed in --json mode returns
    // QueryFailedJsonReported so the CLI shell skips the human-readable error
    // line on stderr.
    // Why it exists: stdout-quiet contract -- scripts wrapping --json must see
    // exactly one document on stdout and nothing on stderr.
    // Scenario: any query failure under --json; the branch under test is the
    // variant choice, not detail formatting.
    #[test]
    fn emit_query_failed_json_returns_already_reported() {
        let err = emit_query_failed(true, "exit 1: dummy".into())
            .expect_err("err expected from emit_query_failed");
        assert!(
            matches!(err, UpsError::QueryFailedJsonReported),
            "got {err:?}"
        );
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
        crate::parse::parse_upsc(stdout)
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

    // Intent: query-failed --json serializes to the sentinel error plus detail.
    // Why: scripts key off `.error == "query_failed"` and can surface the
    // captured upsc stderr without scraping CLI stderr.
    // Scenario: `braid ups status --json` while upsd.service is stopped.
    #[test]
    fn snapshot_json_query_failed() {
        let payload = JsonReport::Error(ErrorReport::QueryFailed {
            detail: "exit 1: Error: Connection failure: Connection refused",
        });
        snap_json!(&payload);
    }

    // Intent: invocation-failed --json serializes to the
    // `invocation_failed` sentinel with detail.
    // Why it exists: scripts key off `.error == "invocation_failed"` to
    // distinguish a broken braid wrapper / missing nut package from
    // `query_failed` (live NUT state); a snapshot pins the exact JSON
    // shape against accidental sentinel renames.
    // Scenario: `braid ups status --json` when `upsc` cannot be spawned
    // (e.g. wrapper PATH bug or nut packaging error).
    #[test]
    fn snapshot_json_invocation_failed() {
        let payload = JsonReport::Error(ErrorReport::InvocationFailed {
            detail: "invocation failed: upsc: No such file or directory",
        });
        snap_json!(&payload);
    }

    // Intent: not-enabled --json serializes to the not-enabled sentinel.
    // Why: distinguishes "UPS unreachable" from "UPS intentionally
    // disabled" so scripts can stay quiet in the latter case.
    // Scenario: host without `braid.ups.enable = true` -- `braid ups
    // status --json` still exits 0 with the ups_not_enabled sentinel.
    #[test]
    fn snapshot_json_not_enabled() {
        let payload = JsonReport::Error(ErrorReport::NotEnabled);
        snap_json!(&payload);
    }
}
