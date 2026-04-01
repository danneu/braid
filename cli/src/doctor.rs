use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cmd::{CmdRequest, CommandRunner, RealRunner};
use crate::config::Config;
use crate::membership;
use crate::parse::parse_btrfs_df_json;
use crate::parse::types::{BtrfsBgType, BtrfsDfOutput, BtrfsProfile};
use crate::preflight;
use crate::state_paths::StatePaths;
use crate::status::format_bytes;

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
    pub status: CheckStatus,
    pub checks: Vec<CheckResult>,
}

enum DfSnapshot {
    NotMounted,
    QueryFailed(String),
    ParseFailed(String),
    Ok(BtrfsDfOutput),
}

struct DoctorContext<'a, R: CommandRunner> {
    config_path: PathBuf,
    config_value: Option<serde_json::Value>,
    config: Option<Config>,
    runner: &'a R,
    paths: &'a StatePaths,
    df_snapshot: Option<DfSnapshot>,
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

fn check_config_file<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
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

fn check_config_schema<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
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

    ctx.config = Some(cfg);
    CheckResult {
        name: "config_schema".into(),
        status: CheckStatus::Ok,
        message: "required fields present and valid".into(),
    }
}

fn check_config_permissions<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    if ctx.config_value.is_none() {
        return CheckResult {
            name: "config_permissions".into(),
            status: CheckStatus::Skip,
            message: "skipped (config file not available)".into(),
        };
    }

    let meta = match std::fs::metadata(&ctx.config_path) {
        Ok(m) => m,
        Err(e) => {
            return CheckResult {
                name: "config_permissions".into(),
                status: CheckStatus::Warn,
                message: format!("could not stat {}: {e}", ctx.config_path.display()),
            };
        }
    };

    let mode = meta.mode();
    let uid = meta.uid();
    let mut warnings: Vec<String> = Vec::new();

    // World-writable (o+w)
    if mode & 0o002 != 0 {
        warnings.push("world-writable".into());
    }
    // Group-writable (g+w)
    if mode & 0o020 != 0 {
        warnings.push("group-writable".into());
    }
    // Owner is not root
    if uid != 0 {
        warnings.push(format!("owned by uid {uid}, expected root (0)"));
    }

    if warnings.is_empty() {
        CheckResult {
            name: "config_permissions".into(),
            status: CheckStatus::Ok,
            message: format!("{} permissions ok", ctx.config_path.display()),
        }
    } else {
        CheckResult {
            name: "config_permissions".into(),
            status: CheckStatus::Warn,
            message: format!("{}: {}", ctx.config_path.display(), warnings.join(", ")),
        }
    }
}

fn check_declared_disks<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    let pool_membership = match membership::load_membership(ctx.paths) {
        Ok(m) => m,
        Err(membership::MembershipError::NotFound(_)) => {
            return CheckResult {
                name: "declared_disks".into(),
                status: CheckStatus::Skip,
                message: "skipped (no pool membership file)".into(),
            };
        }
        Err(e) => {
            return CheckResult {
                name: "declared_disks".into(),
                status: CheckStatus::Warn,
                message: format!("could not load pool membership: {e}"),
            };
        }
    };

    let mut missing: Vec<String> = Vec::new();
    let mut not_block: Vec<String> = Vec::new();
    let total = pool_membership.disks.len();
    for (name, member) in &pool_membership.disks {
        let path = Path::new(member.by_id.0.as_str());
        match std::fs::metadata(path) {
            Ok(meta) if meta.file_type().is_block_device() => {}
            Ok(_) => not_block.push(format!("{name} ({})", member.by_id)),
            Err(_) => missing.push(format!("{name} ({})", member.by_id)),
        }
    }

    if missing.is_empty() && not_block.is_empty() {
        CheckResult {
            name: "declared_disks".into(),
            status: CheckStatus::Ok,
            message: format!("all {total} declared disk(s) present"),
        }
    } else {
        let mut parts: Vec<String> = Vec::new();
        if !missing.is_empty() {
            parts.push(format!(
                "{} not found: {}",
                missing.len(),
                missing.join(", ")
            ));
        }
        if !not_block.is_empty() {
            parts.push(format!(
                "{} not a block device: {}",
                not_block.len(),
                not_block.join(", ")
            ));
        }
        CheckResult {
            name: "declared_disks".into(),
            status: CheckStatus::Warn,
            message: format!(
                "{}/{} disk(s) have problems: {}",
                missing.len() + not_block.len(),
                total,
                parts.join("; ")
            ),
        }
    }
}

fn ensure_df_snapshot<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) {
    if ctx.df_snapshot.is_some() {
        return;
    }

    let config = match &ctx.config {
        Some(c) => c,
        None => return,
    };

    let mount_point = config.mount_point().to_owned();

    // Check if pool is mounted
    match ctx.runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.clone(),
    }) {
        Ok(out) if out.exit_status == 0 => {}
        _ => {
            ctx.df_snapshot = Some(DfSnapshot::NotMounted);
            return;
        }
    }

    // Query btrfs filesystem df
    let raw = match ctx.runner.run(&CmdRequest::BtrfsFilesystemDfJson {
        mount_point: mount_point.clone(),
    }) {
        Ok(raw) => raw,
        Err(e) => {
            ctx.df_snapshot = Some(DfSnapshot::QueryFailed(e.to_string()));
            return;
        }
    };

    match parse_btrfs_df_json(&raw) {
        Ok(df) => ctx.df_snapshot = Some(DfSnapshot::Ok(df)),
        Err(e) => ctx.df_snapshot = Some(DfSnapshot::ParseFailed(e.to_string())),
    }
}

fn check_profile_mismatch<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
    bg_type: BtrfsBgType,
    check_name: &str,
    type_label: &str,
) -> CheckResult {
    if ctx.config.is_none() {
        return CheckResult {
            name: check_name.into(),
            status: CheckStatus::Skip,
            message: "skipped (config not available)".into(),
        };
    }

    ensure_df_snapshot(ctx);

    let mount_point = ctx.config.as_ref().unwrap().mount_point().to_owned();

    match &ctx.df_snapshot {
        None => CheckResult {
            name: check_name.into(),
            status: CheckStatus::Skip,
            message: "skipped (config not available)".into(),
        },
        Some(DfSnapshot::NotMounted) => CheckResult {
            name: check_name.into(),
            status: CheckStatus::Skip,
            message: "skipped (pool not mounted)".into(),
        },
        Some(DfSnapshot::QueryFailed(e)) => CheckResult {
            name: check_name.into(),
            status: CheckStatus::Warn,
            message: format!("could not query {type_label} profiles: {e}"),
        },
        Some(DfSnapshot::ParseFailed(e)) => CheckResult {
            name: check_name.into(),
            status: CheckStatus::Warn,
            message: format!("could not parse {type_label} profiles: {e}"),
        },
        Some(DfSnapshot::Ok(df)) => {
            let entries: Vec<_> = df.entries.iter().filter(|e| e.bg_type == bg_type).collect();

            let profiles: std::collections::BTreeSet<&BtrfsProfile> =
                entries.iter().map(|e| &e.bg_profile).collect();

            if profiles.len() <= 1 {
                let profile_name = profiles
                    .into_iter()
                    .next()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "unknown".into());
                CheckResult {
                    name: check_name.into(),
                    status: CheckStatus::Ok,
                    message: format!("{type_label} profile: {profile_name}"),
                }
            } else {
                let mut parts: Vec<String> = Vec::new();
                for entry in &entries {
                    parts.push(format!(
                        "{}: {} used / {} total",
                        entry.bg_profile,
                        format_bytes(entry.bg_used),
                        format_bytes(entry.bg_total),
                    ));
                }
                CheckResult {
                    name: check_name.into(),
                    status: CheckStatus::Warn,
                    message: format!(
                        "mixed {type_label} profiles ({}); run: btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft {mount_point}",
                        parts.join(", "),
                    ),
                }
            }
        }
    }
}

fn check_pool_missing_devices<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    if ctx.config.is_none() {
        return CheckResult {
            name: "pool_missing_devices".into(),
            status: CheckStatus::Skip,
            message: "skipped (config not available)".into(),
        };
    }

    ensure_df_snapshot(ctx);

    match &ctx.df_snapshot {
        None | Some(DfSnapshot::NotMounted) => {
            return CheckResult {
                name: "pool_missing_devices".into(),
                status: CheckStatus::Skip,
                message: "skipped (pool not mounted)".into(),
            };
        }
        _ => {}
    }

    let mount_point = ctx.config.as_ref().unwrap().mount_point().to_owned();

    match preflight::probe_missing_devids(ctx.runner, mount_point.as_str()) {
        Ok(missing) if missing.is_empty() => CheckResult {
            name: "pool_missing_devices".into(),
            status: CheckStatus::Ok,
            message: "no missing devices".into(),
        },
        Ok(missing) => {
            let devids: Vec<String> = missing.iter().map(|d| d.to_string()).collect();
            CheckResult {
                name: "pool_missing_devices".into(),
                status: CheckStatus::Warn,
                message: format!(
                    "pool has {} missing device{} (devid{}: {}); replace with: braid replace --old <disk> --new <disk> --missing-id <devid>",
                    missing.len(),
                    if missing.len() == 1 { "" } else { "s" },
                    if missing.len() == 1 { "" } else { "s" },
                    devids.join(", "),
                ),
            }
        }
        Err(e) => CheckResult {
            name: "pool_missing_devices".into(),
            status: CheckStatus::Warn,
            message: format!("could not probe for missing devices: {e}"),
        },
    }
}

fn check_data_profile_mismatch<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    check_profile_mismatch(ctx, BtrfsBgType::Data, "data_profile_mismatch", "data")
}

fn check_metadata_profile_mismatch<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
) -> CheckResult {
    check_profile_mismatch(
        ctx,
        BtrfsBgType::Metadata,
        "metadata_profile_mismatch",
        "metadata",
    )
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

pub fn run_doctor<R: CommandRunner>(
    config_path: &Path,
    runner: &R,
    paths: &StatePaths,
) -> DoctorReport {
    let mut ctx = DoctorContext {
        config_path: config_path.to_owned(),
        config_value: None,
        config: None,
        runner,
        paths,
        df_snapshot: None,
    };

    let checks = vec![
        check_config_file(&mut ctx),
        check_config_schema(&mut ctx),
        check_config_permissions(&mut ctx),
        check_declared_disks(&mut ctx),
        check_pool_missing_devices(&mut ctx),
        check_data_profile_mismatch(&mut ctx),
        check_metadata_profile_mismatch(&mut ctx),
    ];

    let status = overall_status(&checks);

    DoctorReport { status, checks }
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
            "config_permissions" => "config perms",
            "declared_disks" => "declared disks",
            "pool_missing_devices" => "missing devs",
            "data_profile_mismatch" => "data profiles",
            "metadata_profile_mismatch" => "meta profiles",
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

pub fn cmd_doctor(config_path: &Path, paths: &StatePaths, json: bool) -> Result<(), DoctorError> {
    let runner = RealRunner;
    let report = run_doctor(config_path, &runner, paths);

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
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::state_paths::StatePaths;
    use crate::types::MountPoint;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    fn isolated_paths() -> (TempDir, StatePaths) {
        let dir = TempDir::new().unwrap();
        let paths = StatePaths::custom(dir.path().to_owned());
        (dir, paths)
    }

    fn valid_config_json() -> &'static str {
        r#"{"disks":{"toshiba":{"by_id":"/dev/disk/by-id/a"}},"mount_point":"/mnt/storage"}"#
    }

    fn mock() -> MockRunner {
        MockRunner::default()
    }

    fn write_temp(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    fn find_check<'a>(report: &'a DoctorReport, name: &str) -> &'a CheckResult {
        report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("check '{name}' not found"))
    }

    #[test]
    fn valid_config_parses_ok_disks_warn() {
        let f = write_temp(valid_config_json());
        let (_dir, paths) = isolated_paths();
        let report = run_doctor(f.path(), &mock(), &paths);
        assert_eq!(report.checks.len(), 7);
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        assert_eq!(find_check(&report, "config_schema").status, CheckStatus::Ok);
        // declared_disks skips since no pool membership file exists in test env
        assert_eq!(
            find_check(&report, "declared_disks").status,
            CheckStatus::Skip
        );
    }

    #[test]
    fn missing_file_fail_skip() {
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &mock(),
            &StatePaths::production(),
        );
        assert_eq!(report.status, CheckStatus::Fail);
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Fail);
        assert_eq!(
            find_check(&report, "config_schema").status,
            CheckStatus::Skip
        );
        assert_eq!(
            find_check(&report, "config_permissions").status,
            CheckStatus::Skip
        );
    }

    #[test]
    fn invalid_json_fail_skip() {
        let f = write_temp("not json at all {{{");
        let report = run_doctor(f.path(), &mock(), &StatePaths::production());
        assert_eq!(report.status, CheckStatus::Fail);
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Fail);
        assert_eq!(
            find_check(&report, "config_schema").status,
            CheckStatus::Skip
        );
        assert_eq!(
            find_check(&report, "config_permissions").status,
            CheckStatus::Skip
        );
    }

    #[test]
    fn valid_json_with_extra_fields_parses_ok() {
        // Config no longer has disks — extra fields are ignored.
        let f = write_temp(r#"{"disks":{},"mount_point":"/mnt/storage"}"#);
        let report = run_doctor(f.path(), &mock(), &StatePaths::production());
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        assert_eq!(find_check(&report, "config_schema").status, CheckStatus::Ok);
    }

    #[test]
    fn valid_json_bad_schema_empty_mount() {
        let f = write_temp(r#"{"disks":{"a":{"by_id":"/dev/disk/by-id/a"}},"mount_point":""}"#);
        let report = run_doctor(f.path(), &mock(), &StatePaths::production());
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        let schema = find_check(&report, "config_schema");
        assert_eq!(schema.status, CheckStatus::Fail);
        assert!(
            schema.message.contains("mount_point must not be empty"),
            "unexpected message: {}",
            schema.message
        );
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
        let report = run_doctor(f.path(), &mock(), &StatePaths::production());
        let human = format_doctor_human(&report);
        assert!(human.contains("[ok  ]"), "expected [ok  ] tag:\n{human}");
        assert!(
            human.contains("config file"),
            "expected 'config file':\n{human}"
        );
        assert!(
            human.contains("config schema"),
            "expected 'config schema':\n{human}"
        );
    }

    #[test]
    fn human_format_fail_tag() {
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &mock(),
            &StatePaths::production(),
        );
        let human = format_doctor_human(&report);
        assert!(human.contains("[FAIL]"), "expected [FAIL] tag:\n{human}");
        assert!(human.contains("[skip]"), "expected [skip] tag:\n{human}");
    }

    #[test]
    fn permissions_world_writable_warns() {
        use std::os::unix::fs::PermissionsExt;
        let f = write_temp(valid_config_json());
        std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(0o666)).unwrap();
        let report = run_doctor(f.path(), &mock(), &StatePaths::production());
        let perm = find_check(&report, "config_permissions");
        assert_eq!(perm.status, CheckStatus::Warn);
        assert!(perm.message.contains("world-writable"), "{}", perm.message);
        assert!(perm.message.contains("group-writable"), "{}", perm.message);
    }

    #[test]
    fn permissions_restrictive_ok() {
        use std::os::unix::fs::PermissionsExt;
        let f = write_temp(valid_config_json());
        std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let report = run_doctor(f.path(), &mock(), &StatePaths::production());
        let perm = find_check(&report, "config_permissions");
        // May still warn about uid (tests don't run as root), but should not
        // warn about world/group bits.
        assert!(
            !perm.message.contains("world-"),
            "unexpected world- warning: {}",
            perm.message
        );
        assert!(
            !perm.message.contains("group-writable"),
            "unexpected group-writable: {}",
            perm.message
        );
    }

    #[test]
    fn permissions_skip_when_no_config() {
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &mock(),
            &StatePaths::production(),
        );
        let perm = find_check(&report, "config_permissions");
        assert_eq!(perm.status, CheckStatus::Skip);
    }

    #[test]
    fn human_format_contains_perms_label() {
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &mock(), &StatePaths::production());
        let human = format_doctor_human(&report);
        assert!(
            human.contains("config perms"),
            "expected 'config perms':\n{human}"
        );
    }

    #[test]
    fn declared_disks_skips_when_no_membership() {
        let f = write_temp(r#"{"mount_point":"/mnt/storage"}"#);
        let (_dir, paths) = isolated_paths();
        let report = run_doctor(f.path(), &mock(), &paths);
        let check = find_check(&report, "declared_disks");
        assert_eq!(check.status, CheckStatus::Skip);
    }

    #[test]
    fn declared_disks_skip_when_no_config() {
        let (_dir, paths) = isolated_paths();
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &mock(),
            &paths,
        );
        let check = find_check(&report, "declared_disks");
        assert_eq!(check.status, CheckStatus::Skip);
    }

    #[test]
    fn declared_disks_skip_when_bad_schema() {
        let f = write_temp(r#"{"disks":{},"mount_point":"/mnt/storage"}"#);
        let (_dir, paths) = isolated_paths();
        let report = run_doctor(f.path(), &mock(), &paths);
        let check = find_check(&report, "declared_disks");
        assert_eq!(check.status, CheckStatus::Skip);
    }

    #[test]
    fn human_format_contains_declared_disks_label() {
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &mock(), &StatePaths::production());
        let human = format_doctor_human(&report);
        assert!(
            human.contains("declared disks"),
            "expected 'declared disks':\n{human}"
        );
    }

    // --- data_profile_mismatch tests ---

    fn mountpoint_ok() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "mountpoint -q /mnt/storage".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn mountpoint_fail() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "mountpoint -q /mnt/storage".into(),
                stdout: String::new(),
                stderr: "/mnt/storage is not a mountpoint\n".into(),
                exit_status: 1,
            },
        )
    }

    fn df_json(json: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsFilesystemDfJson {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "btrfs --format json filesystem df /mnt/storage".into(),
                stdout: json.into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn df_json_fail() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsFilesystemDfJson {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "btrfs --format json filesystem df /mnt/storage".into(),
                stdout: String::new(),
                stderr: "ERROR: not a btrfs filesystem".into(),
                exit_status: 1,
            },
        )
    }

    const DF_RAID1_CLEAN: &str = r#"{
        "filesystem-df": [
            { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
            { "bg-type": "System", "bg-profile": "RAID1", "total": 8388608, "used": 16384 },
            { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 262144 },
            { "bg-type": "GlobalReserve", "bg-profile": "single", "total": 3407872, "used": 0 }
        ]
    }"#;

    const DF_MIXED: &str = r#"{
        "filesystem-df": [
            { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
            { "bg-type": "Data", "bg-profile": "single", "total": 8388608, "used": 4194304 },
            { "bg-type": "System", "bg-profile": "RAID1", "total": 8388608, "used": 16384 },
            { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 262144 },
            { "bg-type": "GlobalReserve", "bg-profile": "single", "total": 3407872, "used": 0 }
        ]
    }"#;

    #[test]
    fn data_profile_clean_raid1_ok() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &StatePaths::production());
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.message.contains("RAID1"),
            "expected RAID1 in message: {}",
            check.message
        );
    }

    #[test]
    fn data_profile_mixed_warns() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_MIXED);
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &StatePaths::production());
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("mixed"),
            "expected 'mixed' in message: {}",
            check.message
        );
        assert!(
            check.message.contains("-dconvert=raid1,soft"),
            "expected soft flag in suggestion: {}",
            check.message
        );
    }

    #[test]
    fn data_profile_global_reserve_single_not_warned() {
        // GlobalReserve is always "single" — must not trigger mismatch
        let json = r#"{
            "filesystem-df": [
                { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
                { "bg-type": "GlobalReserve", "bg-profile": "single", "total": 3407872, "used": 0 }
            ]
        }"#;
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(json);
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &StatePaths::production());
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Ok);
    }

    #[test]
    fn data_profile_skip_when_config_unavailable() {
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &mock(),
            &StatePaths::production(),
        );
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Skip);
        assert!(
            check.message.contains("config not available"),
            "{}",
            check.message
        );
    }

    #[test]
    fn data_profile_skip_when_pool_not_mounted() {
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = mock().with_output(mp_req, mp_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &StatePaths::production());
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Skip);
        assert!(check.message.contains("not mounted"), "{}", check.message);
    }

    #[test]
    fn data_profile_warn_when_df_fails() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json_fail();
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &StatePaths::production());
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("could not"),
            "expected error message: {}",
            check.message
        );
    }

    // --- metadata_profile_mismatch tests ---

    const DF_MIXED_METADATA: &str = r#"{
        "filesystem-df": [
            { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
            { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 262144 },
            { "bg-type": "Metadata", "bg-profile": "single", "total": 8388608, "used": 65536 },
            { "bg-type": "System", "bg-profile": "RAID1", "total": 8388608, "used": 16384 },
            { "bg-type": "GlobalReserve", "bg-profile": "single", "total": 3407872, "used": 0 }
        ]
    }"#;

    // Intent: Verify metadata_profile_mismatch reports Ok for uniform RAID1 metadata.
    // Why: Ensures the check doesn't false-positive on a healthy pool.
    // Scenario: A clean 2-disk RAID1 pool has all metadata block groups as RAID1.
    #[test]
    fn metadata_profile_clean_raid1_ok() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &StatePaths::production());
        let check = find_check(&report, "metadata_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.message.contains("RAID1"),
            "expected RAID1 in message: {}",
            check.message
        );
    }

    // Intent: Verify metadata_profile_mismatch detects mixed metadata profiles.
    // Why: Mixed metadata is more dangerous than mixed data — metadata loss
    //   can make the entire filesystem unrecoverable.
    // Scenario: An interrupted `btrfs balance` leaves some metadata block groups
    //   as single while others remain RAID1. braid doctor should warn.
    #[test]
    fn metadata_profile_mixed_warns() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_MIXED_METADATA);
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &StatePaths::production());
        let check = find_check(&report, "metadata_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("mixed"),
            "expected 'mixed' in message: {}",
            check.message
        );
        assert!(
            check.message.contains("-dconvert=raid1,soft"),
            "expected soft flag in suggestion: {}",
            check.message
        );
    }

    // Intent: Verify metadata_profile_mismatch skips when config is unavailable.
    // Why: Without config, we don't know the mount point to query.
    // Scenario: User runs `braid doctor --config /nonexistent` — profile checks
    //   should skip gracefully.
    #[test]
    fn metadata_profile_skip_when_config_unavailable() {
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &mock(),
            &StatePaths::production(),
        );
        let check = find_check(&report, "metadata_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Skip);
        assert!(
            check.message.contains("config not available"),
            "{}",
            check.message
        );
    }

    // Intent: Verify metadata_profile_mismatch skips when pool is not mounted.
    // Why: Can't query btrfs filesystem df on an unmounted pool.
    // Scenario: User runs `braid doctor` before unlocking the pool.
    #[test]
    fn metadata_profile_skip_when_pool_not_mounted() {
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = mock().with_output(mp_req, mp_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &StatePaths::production());
        let check = find_check(&report, "metadata_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Skip);
        assert!(check.message.contains("not mounted"), "{}", check.message);
    }

    // Intent: Verify human format includes the "meta profiles" label.
    // Why: Ensures the new check has a human-readable label in format_doctor_human.
    // Scenario: Operator reads `braid doctor` output and sees metadata profile status.
    #[test]
    fn human_format_contains_meta_profiles_label() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &StatePaths::production());
        let human = format_doctor_human(&report);
        assert!(
            human.contains("meta profiles"),
            "expected 'meta profiles':\n{human}"
        );
    }

    // --- pool_missing_devices tests ---

    fn device_usage_healthy() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsDeviceUsageRaw {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "btrfs device usage --raw /mnt/storage".into(),
                stdout: "\
/dev/mapper/braid-toshiba, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:            50331648

"
                .into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn device_usage_with_missing() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsDeviceUsageRaw {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "btrfs device usage --raw /mnt/storage".into(),
                stdout: "\
/dev/mapper/braid-toshiba, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:            50331648

<missing disk>, ID: 2
   Device size:                  0
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:                  0

"
                .into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    // Intent: pool_missing_devices reports Ok when no devices are missing.
    // Why: ensures the check doesn't false-positive on a healthy pool.
    // Scenario: healthy 1-disk pool, all present.
    #[test]
    fn pool_missing_devices_ok_when_healthy() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let (du_req, du_out) = device_usage_healthy();
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(du_req, du_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &StatePaths::production());
        let check = find_check(&report, "pool_missing_devices");
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(check.message.contains("no missing"), "{}", check.message);
    }

    // Intent: pool_missing_devices warns when devices are missing and recommends replace.
    // Why: degraded pools need operator action; doctor should guide them to replace.
    // Scenario: one drive died in a 2-disk NAS.
    #[test]
    fn pool_missing_devices_warns_with_replace_recommendation() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let (du_req, du_out) = device_usage_with_missing();
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(du_req, du_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &StatePaths::production());
        let check = find_check(&report, "pool_missing_devices");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("missing device"),
            "{}",
            check.message
        );
        assert!(
            check.message.contains("braid replace"),
            "expected replace recommendation: {}",
            check.message
        );
        assert!(
            check.message.contains("--missing-id"),
            "expected --missing-id in recommendation: {}",
            check.message
        );
        assert!(
            check.message.contains("devid"),
            "expected devid in message: {}",
            check.message
        );
    }

    // Intent: pool_missing_devices skips when pool is not mounted.
    // Why: can't probe device usage on an unmounted pool.
    // Scenario: user runs braid doctor before unlocking.
    #[test]
    fn pool_missing_devices_skip_when_not_mounted() {
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = mock().with_output(mp_req, mp_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &StatePaths::production());
        let check = find_check(&report, "pool_missing_devices");
        assert_eq!(check.status, CheckStatus::Skip);
    }

    // Intent: human format includes the "missing devs" label.
    // Why: ensures the new check has a human-readable label.
    // Scenario: operator reads braid doctor output.
    #[test]
    fn human_format_contains_missing_devs_label() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let (du_req, du_out) = device_usage_healthy();
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(du_req, du_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &StatePaths::production());
        let human = format_doctor_human(&report);
        assert!(
            human.contains("missing devs"),
            "expected 'missing devs':\n{human}"
        );
    }
}
