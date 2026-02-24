use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{validate, Config};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    pub status: CheckStatus,
    pub checks: Vec<CheckResult>,
}

struct DoctorContext {
    config_path: PathBuf,
    config_value: Option<serde_json::Value>,
    #[allow(dead_code)]
    config: Option<Config>,
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

fn check_config_file(ctx: &mut DoctorContext) -> CheckResult {
    let path = &ctx.config_path;

    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) => {
            return CheckResult {
                name: "config_file".into(),
                status: CheckStatus::Fail,
                message: format!("{}: {e}", path.display()),
            };
        }
    };

    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(v) => {
            ctx.config_value = Some(v);
            CheckResult {
                name: "config_file".into(),
                status: CheckStatus::Ok,
                message: format!("{} exists and is valid JSON", path.display()),
            }
        }
        Err(e) => CheckResult {
            name: "config_file".into(),
            status: CheckStatus::Fail,
            message: format!("{}: invalid JSON: {e}", path.display()),
        },
    }
}

fn check_config_schema(ctx: &mut DoctorContext) -> CheckResult {
    let value = match &ctx.config_value {
        Some(v) => v.clone(),
        None => {
            return CheckResult {
                name: "config_schema".into(),
                status: CheckStatus::Skip,
                message: "skipped (config file not available)".into(),
            };
        }
    };

    let cfg: Config = match serde_json::from_value(value) {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                name: "config_schema".into(),
                status: CheckStatus::Fail,
                message: format!("failed to deserialize config: {e}"),
            };
        }
    };

    if let Err(e) = validate(&cfg) {
        return CheckResult {
            name: "config_schema".into(),
            status: CheckStatus::Fail,
            message: format!("{e}"),
        };
    }

    ctx.config = Some(cfg);
    CheckResult {
        name: "config_schema".into(),
        status: CheckStatus::Ok,
        message: "required fields present and valid".into(),
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

fn overall_status(checks: &[CheckResult]) -> CheckStatus {
    let mut worst = CheckStatus::Ok;
    for c in checks {
        worst = match (worst, c.status) {
            (_, CheckStatus::Skip) => worst,
            (CheckStatus::Fail, _) => CheckStatus::Fail,
            (_, CheckStatus::Fail) => CheckStatus::Fail,
            (CheckStatus::Warn, _) => CheckStatus::Warn,
            (_, CheckStatus::Warn) => CheckStatus::Warn,
            _ => worst,
        };
    }
    worst
}

pub fn run_doctor(config_path: &Path) -> DoctorReport {
    let mut ctx = DoctorContext {
        config_path: config_path.to_owned(),
        config_value: None,
        config: None,
    };

    let mut checks = Vec::new();
    checks.push(check_config_file(&mut ctx));
    checks.push(check_config_schema(&mut ctx));

    let status = overall_status(&checks);

    DoctorReport {
        schema_version: 1,
        status,
        checks,
    }
}

// ---------------------------------------------------------------------------
// Formatters
// ---------------------------------------------------------------------------

pub fn format_doctor_human(report: &DoctorReport) -> String {
    let mut out = String::new();
    for c in &report.checks {
        let tag = match c.status {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "FAIL",
            CheckStatus::Skip => "skip",
        };
        let label = match c.name.as_str() {
            "config_file" => "config file",
            "config_schema" => "config schema",
            other => other,
        };
        out.push_str(&format!("[{tag:<4}]  {label:<14}  {}\n", c.message));
    }
    out
}

// ---------------------------------------------------------------------------
// Command entry point
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct DoctorError;

impl std::fmt::Display for DoctorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "doctor found failures")
    }
}

pub fn cmd_doctor(config_path: &Path, json: bool) -> Result<(), DoctorError> {
    let report = run_doctor(config_path);

    if json {
        // serde_json::to_string_pretty won't fail on our types
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("serialize doctor report")
        );
    } else {
        print!("{}", format_doctor_human(&report));
    }

    match report.status {
        CheckStatus::Fail => Err(DoctorError),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn valid_config_json() -> &'static str {
        r#"{"disks":["/dev/disk/by-id/a"],"mountPoint":"/mnt/storage"}"#
    }

    fn write_temp(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn valid_config_both_ok() {
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path());
        assert_eq!(report.status, CheckStatus::Ok);
        assert_eq!(report.checks.len(), 2);
        assert_eq!(report.checks[0].status, CheckStatus::Ok);
        assert_eq!(report.checks[0].name, "config_file");
        assert_eq!(report.checks[1].status, CheckStatus::Ok);
        assert_eq!(report.checks[1].name, "config_schema");
    }

    #[test]
    fn missing_file_fail_skip() {
        let report = run_doctor(Path::new("/tmp/nonexistent-braid-doctor-test.json"));
        assert_eq!(report.status, CheckStatus::Fail);
        assert_eq!(report.checks[0].status, CheckStatus::Fail);
        assert_eq!(report.checks[0].name, "config_file");
        assert_eq!(report.checks[1].status, CheckStatus::Skip);
        assert_eq!(report.checks[1].name, "config_schema");
    }

    #[test]
    fn invalid_json_fail_skip() {
        let f = write_temp("not json at all {{{");
        let report = run_doctor(f.path());
        assert_eq!(report.status, CheckStatus::Fail);
        assert_eq!(report.checks[0].status, CheckStatus::Fail);
        assert_eq!(report.checks[1].status, CheckStatus::Skip);
    }

    #[test]
    fn valid_json_bad_schema_empty_disks() {
        let f = write_temp(r#"{"disks":[],"mountPoint":"/mnt/storage"}"#);
        let report = run_doctor(f.path());
        assert_eq!(report.status, CheckStatus::Fail);
        assert_eq!(report.checks[0].status, CheckStatus::Ok);
        assert_eq!(report.checks[0].name, "config_file");
        assert_eq!(report.checks[1].status, CheckStatus::Fail);
        assert_eq!(report.checks[1].name, "config_schema");
        assert!(report.checks[1].message.contains("disks must not be empty"));
    }

    #[test]
    fn valid_json_bad_schema_empty_mount() {
        let f = write_temp(r#"{"disks":["/dev/disk/by-id/a"],"mountPoint":""}"#);
        let report = run_doctor(f.path());
        assert_eq!(report.checks[0].status, CheckStatus::Ok);
        assert_eq!(report.checks[1].status, CheckStatus::Fail);
        assert!(report.checks[1].message.contains("mountPoint must not be empty"));
    }

    #[test]
    fn overall_status_worst_wins() {
        let checks = vec![
            CheckResult {
                name: "a".into(),
                status: CheckStatus::Ok,
                message: "".into(),
            },
            CheckResult {
                name: "b".into(),
                status: CheckStatus::Warn,
                message: "".into(),
            },
        ];
        assert_eq!(overall_status(&checks), CheckStatus::Warn);

        let checks = vec![
            CheckResult {
                name: "a".into(),
                status: CheckStatus::Warn,
                message: "".into(),
            },
            CheckResult {
                name: "b".into(),
                status: CheckStatus::Fail,
                message: "".into(),
            },
        ];
        assert_eq!(overall_status(&checks), CheckStatus::Fail);
    }

    #[test]
    fn skip_does_not_affect_overall() {
        let checks = vec![
            CheckResult {
                name: "a".into(),
                status: CheckStatus::Ok,
                message: "".into(),
            },
            CheckResult {
                name: "b".into(),
                status: CheckStatus::Skip,
                message: "".into(),
            },
        ];
        assert_eq!(overall_status(&checks), CheckStatus::Ok);
    }

    #[test]
    fn json_serialization_lowercase() {
        let report = DoctorReport {
            schema_version: 1,
            status: CheckStatus::Ok,
            checks: vec![CheckResult {
                name: "test".into(),
                status: CheckStatus::Fail,
                message: "msg".into(),
            }],
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(r#""status":"ok""#), "overall: {json}");
        assert!(json.contains(r#""status":"fail""#), "check: {json}");
        assert!(!json.contains("Ok"));
        assert!(!json.contains("Fail"));
    }

    #[test]
    fn human_format_contains_tags() {
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path());
        let human = format_doctor_human(&report);
        assert!(human.contains("[ok  ]"), "expected [ok  ] tag:\n{human}");
        assert!(human.contains("config file"), "expected 'config file':\n{human}");
        assert!(human.contains("config schema"), "expected 'config schema':\n{human}");
    }

    #[test]
    fn human_format_fail_tag() {
        let report = run_doctor(Path::new("/tmp/nonexistent-braid-doctor-test.json"));
        let human = format_doctor_human(&report);
        assert!(human.contains("[FAIL]"), "expected [FAIL] tag:\n{human}");
        assert!(human.contains("[skip]"), "expected [skip] tag:\n{human}");
    }

    #[test]
    fn schema_version_is_1() {
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path());
        assert_eq!(report.schema_version, 1);
    }
}
