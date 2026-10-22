//! `braid ups status` -- operator inspection of live NUT state.
//!
//! Missing `ups` block in config.json prints a helpful enable-hint and exits 0.
//! `braid ups status` on a pool without UPS is not an error. Query failure
//! (non-zero `upsc` exit) is a hard error with `upsc`'s own stderr surfaced.
//!
//! `--json` emits the serialized `UpscOutput` plus braid's `severity`
//! verdict, or a top-level `error` sentinel in its place (see `JsonReport`),
//! so scripts can key off the parsed model and braid's judgment without
//! re-parsing `upsc` output themselves.

use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::{ConfigError, config_read};
use crate::parse::parse_upsc;
use crate::parse::types::{UpsSeverity, UpsStatusFlag, UpscOutput};
use crate::status_tag::{self, StatusTag};
use crate::util::detail_suffix;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum UpsError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("upsc query failed: {detail}")]
    QueryFailed { detail: String },
    /// Runner-level `upsc` failure stays distinct from non-zero UPS query
    /// exits so wrapper/package breakage is not read as a daemon fault. The
    /// PATH hint for a missing `upsc` arrives inside `detail`, from
    /// `CmdError::NotFound`.
    #[error("upsc invocation failed: {detail}")]
    InvocationFailed { detail: String },
    #[error("failed to serialize ups status: {0}")]
    Serialize(#[source] serde_json::Error),
}

/// Outcome of `cmd_ups_status` so the JSON-reported case stays an
/// `Ok` variant instead of a typed-`Err` control-flow sentinel. The
/// CLI shell uses `JsonErrorReported` to exit 1 silently after the
/// `--json` branch has already emitted its sentinel on stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsStatusOutcome {
    Done,
    JsonErrorReported,
}

#[derive(Debug, thiserror::Error)]
pub enum UpsQueryError {
    /// Runner-level failure: spawn error, signal-killed child, stdin IO
    /// failure, or request/mode mismatch.
    #[error("upsc invocation failed: {0}")]
    InvocationFailed(#[from] CmdError),
    /// `upsc` exited non-zero. This covers an unreachable upsd daemon, an
    /// unknown UPS name, or another fatal NUT path.
    #[error("upsc query failed (exit {exit_code}){}", detail_suffix(.stderr))]
    QueryFailed { exit_code: i32, stderr: String },
}

/// Successful `upsc` query payload so callers share one exit-code gate while
/// preserving raw stdout for the TUI Variables panel and parsed command state.
#[derive(Debug)]
pub struct UpscQueried {
    pub raw_stdout: String,
    pub parsed: UpscOutput,
}

pub fn query_ups<R: CommandRunner>(runner: &R, name: &str) -> Result<UpscQueried, UpsQueryError> {
    let raw = runner.run(&CmdRequest::UpscQuery {
        name: name.to_owned(),
    })?;
    if raw.exit_status != 0 {
        return Err(UpsQueryError::QueryFailed {
            exit_code: raw.exit_status,
            stderr: raw.stderr.trim().to_owned(),
        });
    }
    let parsed = parse_upsc(&raw.stdout);
    Ok(UpscQueried {
        raw_stdout: raw.stdout,
        parsed,
    })
}

/// `--json` output mode for `braid ups status`. Separate from
/// `UpscOutput` so `error` sentinels have a stable surface for scripts.
/// The `--json` help on `cli/src/main.rs#UpsStatusArgs` and
/// `docs/commands/ups-status.md` restate the script rule, so a change to
/// this contract must update both.
#[derive(Debug, serde::Serialize)]
#[serde(untagged)]
enum JsonReport<'a> {
    Error(ErrorReport<'a>),
    Ok(JsonSuccessReport<'a>),
}

impl<'a> JsonReport<'a> {
    fn success(parsed: &'a UpscOutput) -> Self {
        Self::Ok(JsonSuccessReport::new(parsed))
    }
}

/// Success body: the typed UPS model flattened at top level plus braid's
/// `severity` verdict, so scripts read the judgment preflight uses instead of
/// re-deriving it from `status_flags`. The verdict is computed from the model
/// here rather than stored on `UpscOutput`, so the two cannot disagree.
#[derive(Debug, serde::Serialize)]
struct JsonSuccessReport<'a> {
    severity: UpsSeverity,
    #[serde(flatten)]
    parsed: &'a UpscOutput,
}

impl<'a> JsonSuccessReport<'a> {
    fn new(parsed: &'a UpscOutput) -> Self {
        Self {
            severity: parsed.severity(),
            parsed,
        }
    }
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

/// Entry for `braid ups status`. It takes the writer so main.rs owns process
/// stdout and tests capture every byte the command emits in a buffer.
pub fn cmd_ups_status<R: CommandRunner>(
    runner: &R,
    config_path: &Path,
    json: bool,
    out: &mut dyn std::io::Write,
) -> Result<UpsStatusOutcome, UpsError> {
    let config = config_read(config_path)?;
    let Some(ups_cfg) = config.ups() else {
        return print_not_enabled(json, out);
    };
    let parsed = match query_ups(runner, ups_cfg.name.as_str()) {
        Ok(q) => q.parsed,
        Err(UpsQueryError::InvocationFailed(e)) => {
            return emit_invocation_failed(json, e, out);
        }
        Err(UpsQueryError::QueryFailed { exit_code, stderr }) => {
            return emit_query_failed(
                json,
                format!("exit {exit_code}{}", detail_suffix(&stderr)),
                out,
            );
        }
    };
    if json {
        emit_json(&JsonReport::success(&parsed), out)?;
    } else {
        write!(
            out,
            "{}",
            format_human(
                ups_cfg.name.as_str(),
                &parsed,
                status_tag::color_enabled_for_stdout()
            )
        )
        .ok();
    }
    Ok(UpsStatusOutcome::Done)
}

fn print_not_enabled(
    json: bool,
    out: &mut dyn std::io::Write,
) -> Result<UpsStatusOutcome, UpsError> {
    if json {
        let payload = JsonReport::Error(ErrorReport::NotEnabled);
        emit_json(&payload, out)?;
    } else {
        writeln!(
            out,
            "UPS support is not enabled. Set `braid.enable = true` and\n\
             `braid.ups.enable = true` in your NixOS configuration and rebuild\n\
             to enable preflight safety and low-battery shutdown."
        )
        .ok();
    }
    Ok(UpsStatusOutcome::Done)
}

fn emit_query_failed(
    json: bool,
    detail: String,
    out: &mut dyn std::io::Write,
) -> Result<UpsStatusOutcome, UpsError> {
    if json {
        emit_json(
            &JsonReport::Error(ErrorReport::QueryFailed { detail: &detail }),
            out,
        )?;
        return Ok(UpsStatusOutcome::JsonErrorReported);
    }
    Err(UpsError::QueryFailed { detail })
}

/// Keep invocation failures distinct in `--json` and human mode while
/// preserving shared exit-code wiring.
fn emit_invocation_failed(
    json: bool,
    error: CmdError,
    out: &mut dyn std::io::Write,
) -> Result<UpsStatusOutcome, UpsError> {
    let detail = error.to_string();
    if json {
        emit_json(
            &JsonReport::Error(ErrorReport::InvocationFailed { detail: &detail }),
            out,
        )?;
        return Ok(UpsStatusOutcome::JsonErrorReported);
    }
    Err(UpsError::InvocationFailed { detail })
}

fn emit_json(payload: &JsonReport<'_>, out: &mut dyn std::io::Write) -> Result<(), UpsError> {
    let text = serde_json::to_string_pretty(payload).map_err(UpsError::Serialize)?;
    writeln!(out, "{text}").ok();
    Ok(())
}

/// Human render for `braid ups status`. Curated summary only -- raw
/// extras passthrough lives in `--json`.
///
/// Returns the rendered text with a trailing newline so the caller can
/// `print!` it without trimming; the string shape is stable so tests
/// can snapshot it without touching stdout.
fn format_human(name: &str, parsed: &UpscOutput, color_enabled: bool) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "UPS: {}", name);
    let _ = writeln!(out, "Status: {}", format_status_line(parsed, color_enabled));
    line(
        &mut out,
        "Battery",
        parsed.battery.charge_pct.map(|pct| format!("{pct}%")),
    );
    line(
        &mut out,
        "Runtime",
        parsed
            .battery
            .runtime_secs
            .map(|secs| crate::util::format_duration_secs(secs as u64)),
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

/// Human `Status:` value from one exhaustive match on the shared UPS verdict,
/// so the tag, the gloss, and the empty-status sentinel follow `UpsSeverity`.
fn format_status_line(parsed: &UpscOutput, color_enabled: bool) -> String {
    let tagged = |tag, gloss: &str| {
        format!(
            "{}  {} {gloss}",
            format_status(&parsed.status_flags),
            status_tag::render_status_tag(tag, color_enabled)
        )
    };
    match parsed.severity() {
        UpsSeverity::Online => tagged(StatusTag::Ok, "on utility power"),
        UpsSeverity::OnBattery => tagged(StatusTag::Warn, "on battery"),
        UpsSeverity::Critical => tagged(StatusTag::Fail, "critical"),
        UpsSeverity::Indeterminate => tagged(StatusTag::Skip, "utility power not confirmed"),
        UpsSeverity::StatusMissing => "(unknown -- ups.status missing)".to_owned(),
    }
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

fn format_status(flags: &[UpsStatusFlag]) -> String {
    flags
        .iter()
        .map(UpsStatusFlag::as_token)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, MockRunner};
    use crate::parse::types::{BatteryFields, DeviceFields, InputFields};
    use crate::test_fixtures::{
        ups_query_connection_refused_no_newline, ups_query_connection_refused_with_newline,
        ups_query_empty_stderr_exit_1, ups_query_healthy_minimal, ups_write_config,
        ups_write_config_without_ups,
    };

    /// Baseline `UpscOutput` for render-branch tests: status OL, every
    /// optional field absent. Tests override just the field under test via
    /// `UpscOutput { field: ..., ..base_output() }`. `UpscOutput` has no
    /// `Default` because status order is a contract, so this helper keeps
    /// that fact local to the render tests.
    fn base_output() -> UpscOutput {
        UpscOutput {
            status_flags: vec![UpsStatusFlag::Ol],
            battery: BatteryFields::default(),
            load_pct: None,
            realpower_nominal_watts: None,
            input: InputFields::default(),
            test_result: None,
            device: DeviceFields::default(),
            extra: std::collections::BTreeMap::new(),
        }
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

    // Intent: format_status renders OL verbatim when the UPS is on utility power.
    // Why: operators triaging preflight failures need the rendered line to
    // match NUT's own token vocabulary; translating OL to "online" would
    // force them to cross-reference wording.
    // Scenario: `braid ups status` against a healthy UPS.
    #[test]
    fn format_status_ol() {
        let flags = vec![UpsStatusFlag::Ol];
        assert_eq!(format_status(&flags), "OL");
    }

    // Intent: multi-flag status renders tokens in input order.
    // Why it exists: braid mirrors `upsc` emission order instead of
    // synthesizing a static sort order.
    // Scenario: critical state arrives in both OB/LB and LB/OB order.
    #[test]
    fn format_status_preserves_insertion_order() {
        assert_eq!(
            format_status(&[UpsStatusFlag::Ob, UpsStatusFlag::Lb]),
            "OB LB"
        );
        assert_eq!(
            format_status(&[UpsStatusFlag::Lb, UpsStatusFlag::Ob]),
            "LB OB"
        );
    }

    // Intent: JSON output of a healthy parse round-trips to a stable shape,
    // with an unrecognized token beside `OL` kept verbatim and still judged
    // `online`.
    // Why it exists: the --json contract is the top-level typed UPS body
    // plus braid's `severity` verdict. Snapshot-style coverage lives in
    // golden tests; here we lock in the minimum invariants (status_flags
    // present, battery.charge_pct surfaced, unknown tokens neither dropped
    // nor treated as blockers).
    // Scenario: a newer driver adds a status token braid does not know yet
    // while the UPS is on utility power.
    #[test]
    fn json_output_has_severity_status_and_battery_keys() {
        let parsed = UpscOutput {
            status_flags: vec![UpsStatusFlag::Ol, UpsStatusFlag::Unknown("NEWFLAG".into())],
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
        let text = serde_json::to_string_pretty(&JsonReport::success(&parsed))
            .expect("serialize succeeds");
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
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
        assert_eq!(value["severity"], "online", "got: {text}");
    }

    // Intent: --json serializes status_flags in stored Vec order, with no
    // re-sort at the serialization boundary.
    // Why it exists: this boundary once carried a `serialize_with` hook
    // that lex-sorted the flags; the emission-order pivot removed it and
    // made Vec order the contract (see status_flags doc + ADR 020). A
    // re-added serialize sort would pass the parser-order test, which never
    // serializes -- so this guard is not redundant with it. The 17-flag
    // fixture also pins every variant's NUT token verbatim.
    // Scenario: a UPS reporting every known flag at once plus an
    // unrecognized driver-extension token.
    #[test]
    fn json_output_status_flags_preserve_insertion_order() {
        let flags = vec![
            UpsStatusFlag::Ol,
            UpsStatusFlag::Ob,
            UpsStatusFlag::Lb,
            UpsStatusFlag::Rb,
            UpsStatusFlag::Hb,
            UpsStatusFlag::Chrg,
            UpsStatusFlag::Dischrg,
            UpsStatusFlag::Cal,
            UpsStatusFlag::Bypass,
            UpsStatusFlag::Off,
            UpsStatusFlag::Over,
            UpsStatusFlag::Trim,
            UpsStatusFlag::Boost,
            UpsStatusFlag::Fsd,
            UpsStatusFlag::TestFail,
            UpsStatusFlag::CommBad,
            UpsStatusFlag::Unknown("ZZZ".into()),
        ];
        let parsed = UpscOutput {
            status_flags: flags,
            battery: BatteryFields::default(),
            load_pct: None,
            realpower_nominal_watts: None,
            input: InputFields::default(),
            test_result: None,
            device: DeviceFields::default(),
            extra: std::collections::BTreeMap::new(),
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&JsonReport::success(&parsed)).unwrap())
                .unwrap();
        let actual: Vec<&str> = value["status_flags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();

        assert_eq!(
            actual,
            vec![
                "OL", "OB", "LB", "RB", "HB", "CHRG", "DISCHRG", "CAL", "BYPASS", "OFF", "OVER",
                "TRIM", "BOOST", "FSD", "TESTFAIL", "COMMBAD", "ZZZ",
            ],
        );
    }

    // Intent: a `status_missing` success body still carries the typed
    // telemetry upsc reported.
    // Why it exists: the empty-status verdict replaces the old warning
    // sentinel, so it must not cost scripts the battery, load, and device
    // fields that arrive before the driver fills in ups.status.
    // Scenario: upsc exits 0 and reports battery data before the driver
    // has populated ups.status.
    #[test]
    fn json_output_with_empty_status_keeps_body() {
        let parsed = UpscOutput {
            status_flags: Vec::new(),
            battery: BatteryFields {
                charge_pct: Some(55),
                ..Default::default()
            },
            load_pct: Some(12),
            realpower_nominal_watts: None,
            input: InputFields::default(),
            test_result: None,
            device: DeviceFields {
                model: Some("Back-UPS ES 550G".into()),
                ..Default::default()
            },
            extra: std::collections::BTreeMap::new(),
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&JsonReport::success(&parsed)).unwrap())
                .unwrap();

        assert_eq!(value["severity"], "status_missing", "got: {value}");
        assert!(value["status_flags"].as_array().unwrap().is_empty());
        assert_eq!(value["battery"]["charge_pct"], 55);
        assert_eq!(value["load_pct"], 12);
        assert_eq!(value["device"]["model"], "Back-UPS ES 550G");
        assert!(value.get("error").is_none(), "got: {value}");
    }

    // Intent: an all-unrecognized `ups.status` serializes as a success body
    // whose `status_flags` carries the unknown token verbatim.
    // Why it exists: `severity` says only that braid could not judge the
    // UPS (`json_success_carries_severity_verdict` pins `indeterminate`);
    // the token is how a script or operator learns what braid could not
    // judge. Driving the real `parse_upsc` -> `JsonReport::success` path
    // (not a hand-built struct) is deliberate: a parser regression that
    // dropped unknown-only tokens would empty `status_flags`, and only raw
    // input catches that.
    // Scenario: `upsc` emits `ups.status: WEIRD` -- a token from a newer
    // firmware braid has not shipped support for.
    #[test]
    fn json_all_unknown_status_keeps_token_verbatim() {
        let parsed = parse_upsc("ups.status: WEIRD\n");
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&JsonReport::success(&parsed)).unwrap())
                .unwrap();
        assert!(value.get("error").is_none(), "got: {value}");
        assert_eq!(value["status_flags"], serde_json::json!(["WEIRD"]));
    }

    // Intent: format_human emits exactly `Battery: --`, `Runtime: --`, and
    // `Load: --` when charge/runtime/load are missing, while omitting absent
    // provenance lines.
    // Why it exists: captured fixtures populate these fields, so snapshots
    // only pin the Some arms; this catches dash sentinel and omitted-line drift.
    // Scenario: a UPS driver has surfaced ups.status but no numeric telemetry.
    #[test]
    fn format_human_renders_sentinels_and_omits_absent_provenance() {
        let parsed = base_output();
        let rendered = format_human("ups", &parsed, false);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines.contains(&"Battery: --"), "got: {rendered}");
        assert!(lines.contains(&"Runtime: --"), "got: {rendered}");
        assert!(lines.contains(&"Load: --"), "got: {rendered}");
        assert!(
            !rendered.contains("Battery manufactured"),
            "got: {rendered}"
        );
        assert!(!rendered.contains("Last test"), "got: {rendered}");
    }

    // Intent: format_human emits exactly `Load: 50%` when load is present
    // but nominal real power is missing.
    // Why it exists: snapshots only pin the `Some(load) + Some(watts)` shape;
    // this catches regressions in the asymmetric no-watts branch.
    // Scenario: a consumer UPS reports ups.load but not ups.realpower.nominal.
    #[test]
    fn format_human_load_omits_estimated_when_nominal_watts_missing() {
        let parsed = UpscOutput {
            load_pct: Some(50),
            ..base_output()
        };
        let rendered = format_human("ups", &parsed, false);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines.contains(&"Load: 50%"), "got: {rendered}");
        assert!(!rendered.contains("estimated"), "got: {rendered}");
    }

    // Intent: format_human omits transfer context unless both low and high
    // transfer bounds are present.
    // Why it exists: snapshots only pin the fully-bound fixture shape; a
    // partial render like `(transfer -142 V)` would mislead operators.
    // Scenario: a UPS driver reports line voltage and only one transfer bound.
    #[test]
    fn format_human_omits_transfer_context_when_bounds_incomplete() {
        for input in [
            InputFields {
                voltage: Some("120.0".into()),
                transfer_low: Some("88".into()),
                ..Default::default()
            },
            InputFields {
                voltage: Some("120.0".into()),
                transfer_high: Some("142".into()),
                ..Default::default()
            },
        ] {
            let rendered = format_human(
                "ups",
                &UpscOutput {
                    input,
                    ..base_output()
                },
                false,
            );
            assert!(
                rendered.lines().any(|line| line == "Input: 120.0 V"),
                "got: {rendered}"
            );
            assert!(!rendered.contains("transfer"), "got: {rendered}");
        }
    }

    // Intent: format_human's device line collapses to whichever device field is
    // present and disappears when both fields are absent.
    // Why it exists: fixtures publish both mfr and model, so snapshots cannot
    // catch swapped single-field arms or a stray blank `Device:` line.
    // Scenario: sparse UPS drivers report model only, mfr only, or neither.
    #[test]
    fn format_human_device_line_collapses_to_present_field() {
        let model_only = UpscOutput {
            device: DeviceFields {
                model: Some("Back-UPS ES 550G".into()),
                ..Default::default()
            },
            ..base_output()
        };
        assert!(
            format_human("ups", &model_only, false)
                .lines()
                .any(|line| line == "Device: Back-UPS ES 550G")
        );

        let mfr_only = UpscOutput {
            device: DeviceFields {
                mfr: Some("APC".into()),
                ..Default::default()
            },
            ..base_output()
        };
        assert!(
            format_human("ups", &mfr_only, false)
                .lines()
                .any(|line| line == "Device: APC")
        );

        assert!(
            !format_human("ups", &base_output(), false).contains("Device:"),
            "neither field should omit Device line"
        );
    }

    // Intent: format_human emits exactly the line
    // `Status: (unknown -- ups.status missing)` when status_flags is empty.
    // Why it exists: this is the line an operator lands on after preflight
    // refuses an empty-ups.status mutation and refers them to `braid ups
    // status` (preflight.rs). The referral itself is a fixed string and
    // does not depend on this wording; what the exact-match pin protects is
    // the landing spot staying self-explaining instead of degrading to a
    // bare `(unknown)`. Snapshots otherwise cover only non-empty flag sets.
    // Scenario: dummy-ups driver published telemetry before populating ups.status.
    #[test]
    fn format_human_empty_status_renders_sentinel() {
        let parsed = UpscOutput {
            status_flags: Vec::new(),
            ..base_output()
        };
        let rendered = format_human("ups", &parsed, false);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(
            lines.contains(&"Status: (unknown -- ups.status missing)"),
            "got: {rendered}"
        );
    }

    // Intent: a non-empty but unproven UPS status gets the skip tag and
    // utility-power-not-confirmed gloss.
    // Why it exists: fixture snapshots cover OL/OB/RB/LB states only, so
    // unknown-only status needs a direct guard on the indeterminate arm.
    // Scenario: a newer UPS driver emits a token braid has not classified yet.
    #[test]
    fn format_human_indeterminate_status_renders_skip_gloss() {
        let parsed = UpscOutput {
            status_flags: vec![UpsStatusFlag::Unknown("WEIRD".into())],
            ..base_output()
        };
        let rendered = format_human("ups", &parsed, false);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(
            lines.contains(&"Status: WEIRD  [skip] utility power not confirmed"),
            "got: {rendered}"
        );
    }

    // Intent: color-enabled human status wraps only the severity tag in ANSI.
    // Why it exists: `cmd_ups_status` enables color at the stdout boundary; this
    // pins the rendered bytes without relying on a TTY in the test process.
    // Scenario: an interactive terminal renders a critical UPS status.
    #[test]
    fn format_human_colored_critical_status_wraps_tag() {
        let parsed = UpscOutput {
            status_flags: vec![UpsStatusFlag::Ob, UpsStatusFlag::Lb],
            ..base_output()
        };
        let rendered = format_human("ups", &parsed, true);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(
            lines.contains(&"Status: OB LB  \x1b[31m[fail]\x1b[0m critical"),
            "got: {rendered:?}"
        );
    }

    // Intent: not-enabled --json surfaces the stable error sentinel.
    // Why: scripts wrapping `braid ups status --json` key off
    // `.error == "ups_not_enabled"` without needing stderr parsing.
    // Scenario: unit test of the branch triggered when the `ups` block is
    // absent from config.json.
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
            detail: "command failed: upsc ups: No such file or directory",
        });
        let text = serde_json::to_string_pretty(&payload).unwrap();
        assert!(text.contains("\"invocation_failed\""));
        assert!(text.contains("command failed: upsc "));
        assert!(!text.contains("invocation failed"));
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

        assert!(out.parsed.status_flags.contains(&UpsStatusFlag::Ol));
        assert_eq!(out.parsed.battery.charge_pct, Some(100));
    }

    // Intent: UpsQueryError::QueryFailed Display drops the stderr suffix only
    // when stderr is empty.
    // Why it exists: the inner query error has a distinct Display surface from
    // cmd_ups_status' outer UpsError wrapping, so the shared suffix rule needs
    // a direct pin here.
    // Scenario: upsc exits 1 with no stderr in a future NUT or wrapper path,
    // then with the current daemon-down diagnostic.
    #[test]
    fn ups_query_error_display_omits_empty_stderr_tail() {
        assert_eq!(
            UpsQueryError::QueryFailed {
                exit_code: 1,
                stderr: String::new(),
            }
            .to_string(),
            "upsc query failed (exit 1)"
        );
        assert_eq!(
            UpsQueryError::QueryFailed {
                exit_code: 1,
                stderr: "boom".into(),
            }
            .to_string(),
            "upsc query failed (exit 1): boom"
        );
    }

    // Intent: cmd_ups_status selects the human or JSON success body from
    // the caller's `json` flag and returns Done in both modes.
    // Why it exists: the `--json` contract lives at the command boundary, so
    // helper-only tests would miss a swapped or ignored flag.
    // Scenario: `braid ups status` against a healthy configured UPS.
    #[test]
    fn cmd_ups_status_healthy_selects_mode_and_returns_done() {
        let (request, output) = ups_query_healthy_minimal();
        let runner = MockRunner::default().with_output(request, output);
        let dir = tempfile::tempdir().unwrap();
        let cfg = ups_write_config(&dir, "ups");
        let parsed = query_ups(&runner, "ups").unwrap().parsed;

        let mut buf = Vec::new();
        let mut outcome = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            outcome = Some(cmd_ups_status(&runner, &cfg, false, &mut buf));
        });
        assert_eq!(captured, "");
        let outcome = outcome.unwrap().unwrap();
        assert_eq!(outcome, UpsStatusOutcome::Done);
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            format_human("ups", &parsed, false)
        );

        let mut buf = Vec::new();
        let outcome = cmd_ups_status(&runner, &cfg, true, &mut buf).unwrap();
        assert_eq!(outcome, UpsStatusOutcome::Done);
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&JsonReport::success(&parsed)).unwrap()
            )
        );
    }

    // Intent: cmd_ups_status selects the human or JSON not-enabled body
    // from the caller's `json` flag and returns Done in both modes.
    // Why it exists: hosts without a UPS still have a user-facing status
    // contract, and the fast lane should pin both exact bodies.
    // Scenario: `braid ups status` on a config with no `ups` block.
    #[test]
    fn cmd_ups_status_not_enabled_selects_mode_and_returns_done() {
        let runner = MockRunner::default();
        let dir = tempfile::tempdir().unwrap();
        let cfg = ups_write_config_without_ups(&dir);

        let mut buf = Vec::new();
        let outcome = cmd_ups_status(&runner, &cfg, false, &mut buf).unwrap();
        assert_eq!(outcome, UpsStatusOutcome::Done);
        snap!(String::from_utf8(buf).unwrap());

        let mut buf = Vec::new();
        let outcome = cmd_ups_status(&runner, &cfg, true, &mut buf).unwrap();
        assert_eq!(outcome, UpsStatusOutcome::Done);
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&JsonReport::Error(ErrorReport::NotEnabled)).unwrap()
            )
        );
    }

    // Intent: each upsc failure, with and without --json, produces exactly the
    // output `braid ups status` promises. Under --json the command writes one
    // `query_failed` or `invocation_failed` document to stdout and returns
    // JsonErrorReported. In human mode it writes nothing to stdout and returns
    // the error main.rs prints to stderr. A missing upsc names
    // braid.packages.nut; any other invocation failure carries no PATH hint.
    // Why it exists: scripts key on `.error` and read upsc's stderr from
    // `.detail`, but only the VM lane (tests/cli/braid-status-ups.py) checked
    // the failure bodies. A wrong sentinel, an empty body, or a detail without
    // the stderr passed every Rust test. The human invocation error also ended
    // every runner failure with a hint naming `pkgs.nut`, not the
    // braid.packages.nut override, so a SIGKILLed upsc sent the operator to
    // check package wiring. Rows compare text, not variants,
    // because the text is the contract; the distinct `upsc query failed:` and
    // `upsc invocation failed:` prefixes still pin which failure each row hit.
    // Scenario: upsd.service is stopped (connection refused), upsc exits 1
    // with no stderr, upsc is killed by SIGKILL during a power event, upsc is
    // missing from the wrapper PATH, or the runner fails with no mock seeded,
    // each with and without --json.
    #[test]
    fn cmd_ups_status_failure_output_matrix() {
        /// What one row expects; the variant also picks the `json` flag.
        enum Expect {
            /// `--json`: Ok(JsonErrorReported) and exactly these stdout bytes.
            Stdout(String),
            /// Human mode: an error with exactly this Display, empty stdout.
            Error(String),
        }

        let seeded = |(request, output)| MockRunner::default().with_output(request, output);
        let killed = MockRunner::default().with_handler(|request| {
            matches!(request, CmdRequest::UpscQuery { .. }).then(|| {
                Err(CmdError::Failed(
                    "upsc ups: killed by signal 9 (SIGKILL)".into(),
                ))
            })
        });
        let not_found = MockRunner::default().with_handler(|request| {
            matches!(request, CmdRequest::UpscQuery { .. }).then(|| {
                Err(CmdError::NotFound {
                    cmd: "upsc ups".into(),
                    program: "upsc".into(),
                    source: std::io::Error::from_raw_os_error(libc::ENOENT),
                })
            })
        });
        let enoent = std::io::Error::from_raw_os_error(libc::ENOENT);
        let missing = CmdError::MissingMock.to_string();
        let rows = [
            (
                "connection refused --json",
                seeded(ups_query_connection_refused_no_newline()),
                Expect::Stdout(
                    "{\n  \"error\": \"query_failed\",\n  \"detail\": \
                     \"exit 1: Error: Connection failure: Connection refused\"\n}\n"
                        .into(),
                ),
            ),
            (
                "connection refused",
                seeded(ups_query_connection_refused_no_newline()),
                Expect::Error(
                    "upsc query failed: exit 1: Error: Connection failure: Connection refused"
                        .into(),
                ),
            ),
            (
                "empty stderr --json",
                seeded(ups_query_empty_stderr_exit_1()),
                Expect::Stdout(
                    "{\n  \"error\": \"query_failed\",\n  \"detail\": \"exit 1\"\n}\n".into(),
                ),
            ),
            (
                "empty stderr",
                seeded(ups_query_empty_stderr_exit_1()),
                Expect::Error("upsc query failed: exit 1".into()),
            ),
            (
                "killed by signal --json",
                killed.clone(),
                Expect::Stdout(
                    "{\n  \"error\": \"invocation_failed\",\n  \"detail\": \
                     \"command failed: upsc ups: killed by signal 9 (SIGKILL)\"\n}\n"
                        .into(),
                ),
            ),
            (
                "killed by signal",
                killed,
                Expect::Error(
                    "upsc invocation failed: command failed: upsc ups: killed by signal 9 (SIGKILL)"
                        .into(),
                ),
            ),
            (
                "upsc not found --json",
                not_found.clone(),
                Expect::Stdout(format!(
                    "{{\n  \"error\": \"invocation_failed\",\n  \"detail\": \
                     \"command failed: upsc ups: {enoent} -- is braid.packages.nut on PATH?\"\n}}\n"
                )),
            ),
            (
                "upsc not found",
                not_found,
                Expect::Error(format!(
                    "upsc invocation failed: command failed: upsc ups: {enoent} \
                     -- is braid.packages.nut on PATH?"
                )),
            ),
            (
                "missing mock --json",
                MockRunner::default(),
                Expect::Stdout(format!(
                    "{{\n  \"error\": \"invocation_failed\",\n  \"detail\": \"{missing}\"\n}}\n"
                )),
            ),
            (
                "missing mock",
                MockRunner::default(),
                Expect::Error(format!("upsc invocation failed: {missing}")),
            ),
        ];

        let dir = tempfile::tempdir().unwrap();
        let cfg = ups_write_config(&dir, "ups");
        for (row, runner, expect) in rows {
            let json = matches!(expect, Expect::Stdout(_));
            let mut buf = Vec::new();
            let result = cmd_ups_status(&runner, &cfg, json, &mut buf);
            let stdout = String::from_utf8(buf).unwrap();
            match expect {
                Expect::Stdout(want) => {
                    match result {
                        Ok(outcome) => {
                            assert_eq!(outcome, UpsStatusOutcome::JsonErrorReported, "{row}")
                        }
                        Err(e) => panic!("{row}: expected Ok(JsonErrorReported), got Err({e})"),
                    }
                    assert_eq!(stdout, want, "{row}");
                }
                Expect::Error(want) => {
                    let err = result.expect_err(row);
                    assert_eq!(err.to_string(), want, "{row}");
                    assert_eq!(stdout, "", "{row}");
                }
            }
        }
    }

    // --- Fixture-backed render snapshots ---
    //
    // The plan mandates a curated human summary against each captured
    // fixture. Insta snapshots lock the full wording; any edit to
    // format_human that changes a label, ordering, or sentinel
    // produces a visible diff the reviewer must accept.
    //
    // The online fixture additionally JSON-serializes its parsed model
    // (json_online_fixture_has_expected_shape), anchoring the `--json`
    // contract against a real fixture. Every fixture's `--json` verdict is
    // pinned by json_success_carries_severity_verdict; beyond that, the
    // other states keep only their human snapshot -- per-state JSON shape
    // is not re-checked here, because status-token serialization is
    // covered by json_output_status_flags_preserve_insertion_order and
    // sparse field presence by snapshot_json_success_keeps_sparse_typed_keys.

    fn parse_fixture(stdout: &str) -> UpscOutput {
        crate::parse::parse_upsc(stdout)
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
            "../tests/fixtures/nixos-26.05/upsc/upsc-online.txt"
        ));
        snap!(format_human("ups", &parsed, false));
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
            "../tests/fixtures/nixos-26.05/upsc/upsc-online.txt"
        ));
        let text = serde_json::to_string_pretty(&JsonReport::success(&parsed)).unwrap();
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
        assert_eq!(value["battery"]["type"], "PbAc");
        assert_eq!(value["device"]["type"], "ups");
        assert!(value["battery"].get("type_").is_none());
        assert!(value["device"].get("type_").is_none());
    }

    // Intent: every --json success body carries braid's UPS verdict as a
    //   top-level `severity` string, and none carries a `warning` key.
    // Why it exists: scripts must take the verdict from the one classifier
    //   that preflight, doctor, the TUI, and the human render share, instead
    //   of re-deriving the ladder from `status_flags` and drifting when it
    //   changes. Literal strings pin the wire names, so renaming a variant
    //   fails here instead of silently breaking scripts.
    // Scenario: `braid ups status --json` against each captured fixture
    //   state, a newer firmware's unknown-only status, and a driver that has
    //   not yet populated `ups.status` (absent key, then an empty value).
    #[test]
    fn json_success_carries_severity_verdict() {
        for (label, stdout, want) in [
            (
                "online fixture",
                include_str!("../tests/fixtures/nixos-26.05/upsc/upsc-online.txt"),
                "online",
            ),
            (
                "onbattery fixture",
                include_str!("../tests/fixtures/nixos-26.05/upsc/upsc-onbattery.txt"),
                "on_battery",
            ),
            (
                "lowbattery fixture",
                include_str!("../tests/fixtures/nixos-26.05/upsc/upsc-lowbattery.txt"),
                "critical",
            ),
            (
                "replace-battery fixture",
                include_str!("../tests/fixtures/nixos-26.05/upsc/upsc-replace-battery.txt"),
                "online",
            ),
            (
                "unknown-only status",
                "ups.status: WEIRD\n",
                "indeterminate",
            ),
            ("absent status", "battery.charge: 55\n", "status_missing"),
            (
                "empty status",
                "battery.charge: 55\nups.status:\n",
                "status_missing",
            ),
        ] {
            let parsed = parse_fixture(stdout);
            let value = serde_json::to_value(JsonReport::success(&parsed)).unwrap();
            assert_eq!(value["severity"], want, "{label}: got {value}");
            assert!(value.get("warning").is_none(), "{label}: got {value}");
        }
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
            "../tests/fixtures/nixos-26.05/upsc/upsc-onbattery.txt"
        ));
        snap!(format_human("ups", &parsed, false));
    }

    // Intent: lowbattery fixture renders Status: OB LB.
    // Why: this is the critical pair upsmon fires SHUTDOWNCMD on; the
    // human render must show both flags so the operator understands
    // that the host is about to power off.
    // Scenario: the outage crossed battery.charge.low; upsmon has
    // already triggered systemctl poweroff.
    #[test]
    fn snapshot_human_lowbattery() {
        let parsed = parse_fixture(include_str!(
            "../tests/fixtures/nixos-26.05/upsc/upsc-lowbattery.txt"
        ));
        snap!(format_human("ups", &parsed, false));
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
            "../tests/fixtures/nixos-26.05/upsc/upsc-replace-battery.txt"
        ));
        snap!(format_human("ups", &parsed, false));
    }

    // Intent: sparse success JSON keeps every nullable typed key present
    // with null values and keeps extra present as an empty map.
    // Why it exists: a future `skip_serializing_if = "Option::is_none"`
    // on the parse model would silently switch script-facing absence
    // from null values to omitted keys.
    // Scenario: a thin UPS driver reports ups.status but none of the
    // optional typed telemetry fields.
    #[test]
    fn snapshot_json_success_keeps_sparse_typed_keys() {
        let parsed = UpscOutput {
            status_flags: vec![UpsStatusFlag::Ol],
            battery: BatteryFields::default(),
            load_pct: None,
            realpower_nominal_watts: None,
            input: InputFields::default(),
            test_result: None,
            device: DeviceFields::default(),
            extra: std::collections::BTreeMap::new(),
        };
        snap_json!(&JsonReport::success(&parsed));
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
            detail: "command failed: upsc ups: No such file or directory",
        });
        snap_json!(&payload);
    }

    // Intent: not-enabled --json serializes to the not-enabled sentinel.
    // Why: distinguishes "UPS unreachable" from "UPS intentionally
    // disabled" so scripts can stay quiet in the latter case.
    // Scenario: host with no `ups` block in config.json -- `braid ups status
    // --json` still exits 0 with the ups_not_enabled sentinel.
    #[test]
    fn snapshot_json_not_enabled() {
        let payload = JsonReport::Error(ErrorReport::NotEnabled);
        snap_json!(&payload);
    }
}
