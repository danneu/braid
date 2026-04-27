use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::config_read;
use crate::confirm;
use crate::inhibit::AcquireSleepInhibitor;
use crate::journal;
use crate::membership;
use crate::parse::parse_btrfs_device_usage;
use crate::pool::pool_remove_device_using;
use crate::preflight;
use crate::preview::{self, PerDiskStyle, Preview, PreviewCompleteness, PreviewNote};
use crate::probe::{Filesystem, ProbeError, probe_pool};
use crate::progress::{self, ProgressOutput};
use crate::state_paths::StatePaths;
use crate::types::MountPoint;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum RemoveMissingError {
    #[error("{0}")]
    Validation(String),
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("pool error: {0}")]
    Pool(#[from] crate::pool::PoolError),
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
}

/// Resolve the missing-device removal target to a (devid, membership-name) pair.
/// Returns Err if the missing device's identity can't be mapped to a pool.json entry.
fn resolve_removal_target(
    devid: u64,
    membership: &membership::PoolMembership,
) -> Result<(u64, String), RemoveMissingError> {
    let name = membership
        .disks
        .iter()
        .find(|(_, member)| member.devid == Some(devid))
        .map(|(name, _)| name.clone())
        .ok_or_else(|| {
            RemoveMissingError::Validation(format!(
                "devid {devid} not found in pool.json membership -- \
                 no disk entry has this device ID. \
                 Pool membership may need manual repair."
            ))
        })?;

    Ok((devid, name))
}

pub struct RemoveMissingParams<'a> {
    pub config_path: &'a Path,
    pub missing_id: u64,
    pub dry_run: bool,
    pub yes: bool,
    pub progress: ProgressOutput,
    pub paths: &'a StatePaths,
    /// Seam for acquiring a logind sleep inhibitor before the irreversible
    /// portion of the remove-missing. Production passes `&RealSleepInhibitor`;
    /// unit tests pass `&RecordingInhibitor` to avoid spawning subprocesses.
    pub sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
    /// Seam for the device-remove heartbeat loop. Production passes
    /// `&progress::RealSleeper`; tests pass `&progress::NoopSleeper`
    /// so progress-path coverage does not pay real wall-clock time.
    pub sleeper: &'a dyn progress::Sleeper,
}

/// Dry-run preview source of truth for `braid remove-missing` plus the
/// execute inputs pre-computed during planning. `notes` + `steps` are
/// both rendered by `preview()`; `execute()` consumes the preflight
/// state (`missing_id`, `missing_count`, `remaining_present`) and
/// renders any accumulated notes to stderr via the shared
/// `preview::render_notes_for_stderr` helper (canonical `[warn] <body>`
/// wording) before mutating.
pub struct RemoveMissingPlan {
    pub notes: Vec<PreviewNote>,
    pub steps: Vec<Step>,
    pub missing_id: u64,
    pub will_clear_last_missing: bool,
    pub remaining_present: usize,
    pub missing_count: u64,
    pub mount_point: MountPoint,
}

/// Report returned by `plan_remove_missing`. On the `Ok` branch,
/// accumulated notes have moved into `plan.notes` and `report.notes`
/// is empty. Post-preflight failures preserve accumulated notes on
/// `report.notes` so `cmd_remove_missing` can render them before
/// returning the error.
pub struct RemoveMissingPlanReport {
    pub notes: Vec<PreviewNote>,
    pub result: Result<RemoveMissingPlan, RemoveMissingError>,
}

impl RemoveMissingPlan {
    /// Real-run and failure-path stderr for `remove-missing` use
    /// `Bracketed` per-disk style to match the canonical dry-run render.
    /// `remove-missing` does not emit `PerDisk` notes today, but the
    /// constant keeps the Shape A contract uniform with the other
    /// migrated commands.
    pub const STDERR_STYLE: PerDiskStyle = PerDiskStyle::Bracketed;

    pub fn preview(&self) -> Preview {
        Preview {
            completeness: PreviewCompleteness::Complete,
            notes: self.notes.clone(),
            steps: self.steps.clone(),
        }
    }

    pub fn execute<R: CommandRunner + Sync>(
        self,
        runner: &R,
        params: &RemoveMissingParams<'_>,
    ) -> Result<(), RemoveMissingError> {
        // Render accumulated notes to stderr via the shared helper
        // before any mutation. Warn notes emit as the canonical
        // `[warn] <body>` (same as dry-run stdout), so both modes
        // share one render contract for plan-derived notes.
        eprint!(
            "{}",
            preview::render_notes_for_stderr_with(
                &self.notes,
                Self::STDERR_STYLE,
                crate::status_tag::color_enabled_for_stderr(),
            ),
        );

        // Resolve devid->name from enriched pool.json before confirmation and journal.
        let pre_membership = membership::load_membership(params.paths).map_err(|e| {
            RemoveMissingError::Validation(format!("failed to load pool membership: {e}"))
        })?;
        let (resolved_devid, name_to_remove) =
            resolve_removal_target(self.missing_id, &pre_membership)?;

        // Confirm
        if !params.yes {
            eprintln!(
                "{}",
                format_remove_missing_confirm(
                    &name_to_remove,
                    resolved_devid,
                    self.remaining_present,
                    self.missing_count,
                )
            );
            confirm::confirm_yes().map_err(RemoveMissingError::Validation)?;
        }

        // Hold a logind sleep inhibitor for the rest of the remove-missing
        // operation -- covers the btrfs device remove (chunk relocation; can
        // run for minutes when the missing device had data allocated) and the
        // post-op maybe_restore_raid1 soft balance, which converts single-profile
        // chunks created during degraded operation back to RAID1. Suspending
        // mid-operation can leave chunks unprotected or force recovery.
        //
        // Acquired here, AFTER all interactive/reversible work (confirmation)
        // and BEFORE journal::write_journal, so that:
        //   - operator-idle prompts do not block suspend
        //   - a logind failure aborts cleanly without stranding pending-op.json
        //     and forcing the user into recovery mode for an environmental error.
        let _sleep_inhibitor_guard = params
            .sleep_inhibitor
            .acquire("removing missing device from pool")
            .map_err(|e| {
                RemoveMissingError::Validation(format!(
                    "could not acquire sleep inhibitor (is logind running?): {e}"
                ))
            })?;

        // Build journal before btrfs operation.
        let mut target_membership = pre_membership.clone();
        target_membership.disks.remove(&name_to_remove);
        let journal = journal::build_journal(
            pre_membership,
            target_membership.clone(),
            journal::OpKind::RemoveMissing {
                devid: resolved_devid,
            },
        );
        journal::write_journal(params.paths, &journal)
            .map_err(|e| RemoveMissingError::Validation(e.to_string()))?;

        // Execute
        eprintln!(
            "Removing missing device (devid {}) from pool...",
            resolved_devid
        );
        pool_remove_device_using(
            runner,
            &resolved_devid.to_string(),
            &self.mount_point,
            params.progress,
            params.sleeper,
            &progress::StderrSink,
        )?;

        // Membership committed by btrfs device remove. Persist before the
        // post-remove soft balance; the journal still covers maintenance,
        // so recovery can replay it if we crash before clear_journal.
        membership::save_membership(&target_membership, params.paths).map_err(|e| {
            RemoveMissingError::Validation(format!("failed to persist pool membership: {e}"))
        })?;

        crate::pool::maybe_restore_raid1(
            runner,
            &self.mount_point,
            self.missing_count,
            params.progress,
        )
        .map_err(RemoveMissingError::Pool)?;

        // Maintenance complete -- safe to clear the journal.
        journal::clear_journal(params.paths)
            .map_err(|e| RemoveMissingError::Validation(e.to_string()))?;

        eprintln!("Done. Missing device removed from pool.");
        Ok(())
    }
}

/// Plan a `braid remove-missing` run. Owns everything above today's
/// `--dry-run` gate: pending-op preflight, config read, pool probe /
/// mounted validation, mutation preflight, UPS preflight,
/// missing-device validations, the relocation-space preflight, and
/// `compile_steps`. Returns a
/// `RemoveMissingPlanReport`: on success, accumulated notes move into
/// `plan.notes`; on post-preflight failure, accumulated notes stay on
/// `report.notes` so `cmd_remove_missing` can render them before
/// returning the error.
pub fn plan_remove_missing<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RemoveMissingParams<'_>,
) -> RemoveMissingPlanReport {
    // Notes accumulator. `err_empty` is correct for pre-preflight exits
    // (no notes can have accumulated yet). Post-preflight exits return
    // a notes-preserving report so preflight diagnostics (busy-op Info,
    // readonly-probe-fail Warn) reach `cmd_remove_missing`'s stderr
    // render.
    let mut notes: Vec<PreviewNote> = Vec::new();
    let err_empty = |e: RemoveMissingError| RemoveMissingPlanReport {
        notes: Vec::new(),
        result: Err(e),
    };

    if let Err(msg) = preflight::check_no_pending_operation(params.paths) {
        return err_empty(RemoveMissingError::Validation(msg));
    }

    let config = match config_read(params.config_path) {
        Ok(c) => c,
        Err(e) => return err_empty(e.into()),
    };

    let pool = match probe_pool(runner, config.mount_point()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return err_empty(RemoveMissingError::Validation(
                "pool is not mounted. Nothing to remove.".into(),
            ));
        }
        Err(e) => return err_empty(RemoveMissingError::Probe(e)),
    };

    if !pool.mounted {
        return err_empty(RemoveMissingError::Validation(
            "pool is not mounted. Nothing to remove.".into(),
        ));
    }

    // Preflight
    let fsid = pool.fsid.as_deref().expect("mounted pool must have FSID");
    match preflight::require_mutation_preflight(runner, fs, fsid, config.mount_point()) {
        Ok(preflight_notes) => notes.extend(preflight_notes),
        Err(msg) => return err_empty(RemoveMissingError::Validation(msg)),
    }
    if let Err(msg) = preflight::check_ups_not_on_battery(
        runner,
        config.ups().map(|u| u.name.as_str()),
        "remove-missing",
    ) {
        return RemoveMissingPlanReport {
            notes: std::mem::take(&mut notes),
            result: Err(RemoveMissingError::Validation(msg)),
        };
    }

    if pool.missing_count == 0 {
        return RemoveMissingPlanReport {
            notes: std::mem::take(&mut notes),
            result: Err(RemoveMissingError::Validation(format!(
                "no missing devices detected in pool (devid {} was not found among them).",
                params.missing_id
            ))),
        };
    }

    if pool.devices.iter().any(|d| d.devid == params.missing_id) {
        return RemoveMissingPlanReport {
            notes: std::mem::take(&mut notes),
            result: Err(RemoveMissingError::Validation(format!(
                "devid {} is a live device, not a missing one. \
                 Use 'braid remove' to remove live devices.",
                params.missing_id
            ))),
        };
    }
    if !pool.missing_devids.contains(&params.missing_id) {
        return RemoveMissingPlanReport {
            notes: std::mem::take(&mut notes),
            result: Err(RemoveMissingError::Validation(format!(
                "devid {} is not a device in this pool. \
                 Use 'braid status' to see device IDs.",
                params.missing_id
            ))),
        };
    }

    // Pre-flight: reject if survivors lack space to absorb the missing
    // device's data. Without this check, btrfs will either ENOSPC or
    // crash the filesystem to read-only mid-relocation (see tests/repro/).
    //
    // Skip when only 1 present device survives: in 2-device RAID1, the
    // survivor already has all data (every chunk is mirrored). This does
    // not match the reproduced relocation-failure mode.
    if pool.devices.len() >= 2 {
        match check_relocation_space(runner, config.mount_point(), Some(params.missing_id)) {
            Ok(RelocationCheck::Proceed) => {}
            Ok(RelocationCheck::ProceedWithWarning(body)) => {
                notes.push(PreviewNote::Warn(body));
            }
            Err(e) => {
                return RemoveMissingPlanReport {
                    notes: std::mem::take(&mut notes),
                    result: Err(e),
                };
            }
        }
    }

    let will_clear_last_missing = pool.missing_count == 1;
    let remaining_present = pool.devices.len();
    let steps = compile_steps(
        params.missing_id,
        will_clear_last_missing,
        remaining_present,
        config.mount_point(),
    );

    RemoveMissingPlanReport {
        notes: Vec::new(),
        result: Ok(RemoveMissingPlan {
            notes,
            steps,
            missing_id: params.missing_id,
            will_clear_last_missing,
            remaining_present,
            missing_count: pool.missing_count,
            mount_point: config.mount_point().clone(),
        }),
    }
}

pub fn cmd_remove_missing<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RemoveMissingParams<'_>,
) -> Result<(), RemoveMissingError> {
    let report = plan_remove_missing(runner, fs, params);
    let plan = match report.result {
        Ok(p) => p,
        Err(e) => {
            // Preserved-context failure: accumulated notes render to
            // stderr before the error via the SAME helper as the Ok
            // path (`RemoveMissingPlan::execute`), so preflight
            // diagnostics surface identically across success, failure,
            // and dry-run stdout.
            eprint!(
                "{}",
                preview::render_notes_for_stderr_with(
                    &report.notes,
                    RemoveMissingPlan::STDERR_STYLE,
                    crate::status_tag::color_enabled_for_stderr(),
                ),
            );
            return Err(e);
        }
    };

    if params.dry_run {
        plan.preview().print_colored();
        return Ok(());
    }

    plan.execute(runner, params)
}

/// Outcome of the relocation-space preflight. `Proceed` means either
/// the check ran and survivors have enough space, or it was skipped.
/// `ProceedWithWarning` means the check itself failed (command or
/// parse error) and the caller should surface the warning body to the
/// user but still proceed -- a bug in the safety net must not block a
/// valid operation. A hard "survivors lack space" outcome is a
/// `RemoveMissingError::Validation` instead.
#[derive(Debug)]
pub(crate) enum RelocationCheck {
    Proceed,
    ProceedWithWarning(String),
}

/// Check that surviving devices have enough RAID1-aware, per-type space to absorb
/// the missing device's allocations. If they don't, btrfs device remove will
/// either ENOSPC instantly or -- worse -- crash the filesystem to read-only
/// mid-relocation.
///
/// Missing devices are identified by `device_size == 0` in `btrfs device usage
/// --raw` output. This is reliable: present devices always have device_size > 0,
/// and missing devices always report 0. Their allocation lines (Data, Metadata,
/// System) are preserved and accurate.
///
/// If the check itself fails (parse error, command error), the caller receives
/// `ProceedWithWarning(body)` so it can surface the warning through the
/// preview + execute paths without this helper printing directly.
pub(crate) fn check_relocation_space<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    missing_id: Option<u64>,
) -> Result<RelocationCheck, RemoveMissingError> {
    let raw = match runner.run(&CmdRequest::BtrfsDeviceUsageRaw {
        mount_point: mount_point.clone(),
    }) {
        Ok(r) => r,
        Err(e) => {
            return Ok(RelocationCheck::ProceedWithWarning(format!(
                "ENOSPC pre-flight check failed: {e}; proceeding anyway"
            )));
        }
    };

    let usage = match parse_btrfs_device_usage(&raw) {
        Ok(u) => u,
        Err(e) => {
            return Ok(RelocationCheck::ProceedWithWarning(format!(
                "ENOSPC pre-flight check failed: {e}; proceeding anyway"
            )));
        }
    };

    // Partition: missing (device_size == 0, optionally filtered by devid) vs surviving (device_size > 0)
    let target: Vec<_> = usage
        .devices
        .iter()
        .filter(|d| d.device_size == 0 && (missing_id.is_none() || missing_id == Some(d.devid)))
        .collect();
    let remaining: Vec<_> = usage.devices.iter().filter(|d| d.device_size > 0).collect();

    preflight::check_raid1_relocation_space(&target, &remaining)
        .map(|()| RelocationCheck::Proceed)
        .map_err(|e| {
            RemoveMissingError::Validation(format!(
                "{e}\n\nFree up space by deleting files, or add a new device first with `braid add`."
            ))
        })
}

fn compile_steps(
    missing_id: u64,
    will_clear_last_missing: bool,
    remaining_present: usize,
    mount_point: &MountPoint,
) -> Vec<Step> {
    let mut steps = Vec::new();
    steps.push(Step {
        risk: "long",
        description: format!(
            "btrfs device remove {} (target specific missing device)",
            missing_id
        ),
        commands: vec![CmdRequest::BtrfsDeviceRemove {
            device: missing_id.to_string(),
            mount_point: mount_point.clone(),
        }],
    });
    if will_clear_last_missing && remaining_present >= 2 {
        steps.push(Step {
            risk: "long",
            description:
                "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft (restore redundancy)"
                    .into(),
            commands: vec![CmdRequest::BtrfsBalanceRaid1Soft {
                mount_point: mount_point.clone(),
            }],
        });
    }
    steps
}

// ---------------------------------------------------------------------------
// Confirmation formatter
// ---------------------------------------------------------------------------

fn format_remove_missing_confirm(
    name: &str,
    devid: u64,
    remaining_present: usize,
    missing_count: u64,
) -> String {
    let mut msg = "Remove missing device from pool:\n".to_string();
    msg.push_str(&format!(
        "  {} (devid {})  missing -- no hardware info available\n",
        name, devid
    ));
    if remaining_present >= 2 {
        msg.push_str("  Data on remaining disks will be rebalanced.\n");
    } else {
        msg.push_str("  Surviving disk already has all data.\n");
    }
    msg.push_str(&format!(
        "\nPool: {} present + {} missing -> {} {}\n",
        remaining_present,
        missing_count,
        remaining_present,
        if remaining_present == 1 {
            "disk"
        } else {
            "disks"
        },
    ));
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
    use crate::membership::{DiskMember, PoolMembership};
    use crate::probe::Filesystem;
    use crate::state_paths::StatePaths;
    use crate::types::ByIdPath;

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".into())
    }

    struct MockFs;

    impl Filesystem for MockFs {
        fn exists(&self, _path: &str) -> bool {
            false
        }
        fn is_block_device(&self, _path: &str) -> bool {
            false
        }
        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path.ends_with("/exclusive_operation") {
                Ok("none\n".to_owned())
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
            }
        }
        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    /// Create a StatePaths backed by a temp dir, with pool.json pre-populated.
    /// Each entry is (name, by_id_path, optional_devid).
    fn test_paths(disks: &[(&str, &str, Option<u64>)]) -> (tempfile::TempDir, StatePaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let mut m = PoolMembership::empty();
        for (name, by_id, devid) in disks {
            let mut member = DiskMember::from_by_id(ByIdPath(by_id.to_string()));
            member.devid = *devid;
            m.disks.insert(name.to_string(), member);
        }
        membership::save_membership(&m, &paths).unwrap();
        (tmp, paths)
    }

    struct EnospcRunner {
        device_usage_stdout: &'static str,
    }

    impl CommandRunner for EnospcRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(RawCommandOutput {
                    cmd: "btrfs device usage --raw /mnt/storage".to_owned(),
                    stdout: self.device_usage_stdout.to_owned(),
                    stderr: String::new(),
                    exit_status: 0,
                }),
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.run(request)
        }
    }

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// End-to-end runner that records all calls, modeling a pool with
    /// 1 present device + 1 missing device.
    #[derive(Clone)]
    struct RecordingRunner {
        log: Arc<Mutex<Vec<CmdRequest>>>,
    }

    impl RecordingRunner {
        fn new(log: Arc<Mutex<Vec<CmdRequest>>>) -> Self {
            Self { log }
        }
    }

    fn mock_out(cmd: &str, stdout: &str, exit_status: i32) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status,
        }
    }

    struct HealthyPoolRunner;

    impl CommandRunner for HealthyPoolRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_out(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                    0,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_out(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n",
                    0,
                )),
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_out(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                    ),
                    0,
                )),
                CmdRequest::CryptsetupLuksUuid { .. } => Ok(mock_out(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
                    0,
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_out(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                    0,
                )),
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.run(request)
        }
    }

    struct NullUnderlyingPoolRunner;

    impl CommandRunner for NullUnderlyingPoolRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_out(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                    0,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_out(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n",
                    0,
                )),
                CmdRequest::CryptsetupStatus { mapper } if mapper == "braid-disk2" => Ok(mock_out(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  (null)\n  mode:    read/write\n"
                    ),
                    0,
                )),
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_out(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                    ),
                    0,
                )),
                CmdRequest::CryptsetupLuksUuid { .. } => Ok(mock_out(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
                    0,
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_out(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                    0,
                )),
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.run(request)
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());

            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_out(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                    0,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_out(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 0 used 0 path MISSING\n",
                    0,
                )),
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_out(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                    ),
                    0,
                )),
                CmdRequest::CryptsetupLuksUuid { .. } => Ok(mock_out(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
                    0,
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_out(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                    0,
                )),
                CmdRequest::BtrfsDeviceRemove { .. } => Ok(mock_out("btrfs device remove", "", 0)),
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.run(request)
        }
    }

    /*
     * Intent: cmd_remove_missing succeeds when the pool has 1 present device
     * and 1 missing device without invoking `btrfs device usage --raw`.
     *
     * Why it exists: Single-survivor removal skips the ENOSPC pre-flight
     * check, and missing-id validation must use the already-probed pool
     * state instead of a redundant device-usage probe.
     *
     * Scenario: User's 2-disk NAS has one drive die. They run
     * `braid remove-missing`. The operation succeeds because no data
     * relocation is needed.
     */
    #[test]
    fn no_usage_probe_for_single_survivor() {
        let (_state_tmp, state_paths) = test_paths(&[
            ("disk1", "/dev/disk/by-id/virtio-disk1", Some(1)),
            ("disk2", "/dev/disk/by-id/virtio-disk2", Some(2)),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        let config_json = serde_json::json!({ "mount_point": "/mnt/storage" });
        std::fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();

        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = RecordingRunner::new(log.clone());
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        cmd_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 2,
                dry_run: false,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
                sleep_inhibitor: &inhibitor,
                sleeper: &crate::progress::NoopSleeper,
            },
        )
        .expect("remove-missing should succeed");

        // No BtrfsDeviceUsageRaw calls are expected: missing-id validation
        // uses PoolState::missing_devids, and check_relocation_space is
        // skipped for single-survivor removal.
        let usage_calls = log
            .lock()
            .unwrap()
            .iter()
            .filter(|c| matches!(c, CmdRequest::BtrfsDeviceUsageRaw { .. }))
            .count();
        assert_eq!(
            usage_calls, 0,
            "Expected no BtrfsDeviceUsageRaw calls; missing-id validation should \
             use the pool probe and ENOSPC pre-flight should be skipped for \
             single-survivor removal"
        );
        // Locks in the seam placement: a successful remove-missing must take
        // the inhibitor exactly once before journal::write_journal.
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the path through journal::write_journal"
        );
    }

    #[test]
    // Intent: check_relocation_space rejects when survivors lack space for the
    //   missing device's allocations.
    //
    // Why it exists: Without this pre-flight check, btrfs will either ENOSPC
    //   instantly or crash the filesystem to read-only mid-relocation.
    //
    // Scenario: 3-drive RAID1 pool, one drive dies. The dead drive has 2 GiB
    //   allocated but survivors only have 100 MiB unallocated total.
    fn check_relocation_space_rejects_insufficient_space() {
        // Missing device (devid 3): device_size=0, ~2 GiB allocated
        // Survivors (devid 1,2): 50 MiB unallocated each = 100 MiB total
        let fixture = "\
/dev/mapper/braid-disk1, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Metadata,RAID1:              0
   Unallocated:            50331648

/dev/mapper/braid-disk2, ID: 2
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Metadata,RAID1:              0
   Unallocated:            50331648

<missing disk>, ID: 3
   Device size:                  0
   Device slack:                  0
   Data,RAID1:           2147483648
   Metadata,RAID1:        268435456
   System,RAID1:           33554432
   Unallocated:          1828716544

";

        let runner = EnospcRunner {
            device_usage_stdout: fixture,
        };

        let result = check_relocation_space(&runner, &mp(), None);
        let err = result.expect_err("should reject insufficient space");
        let msg = err.to_string();
        assert!(
            msg.contains("not enough space to relocate"),
            "expected 'not enough space to relocate' in: {msg}"
        );
    }

    #[test]
    // Intent: check_relocation_space passes when survivors have enough space.
    //
    // Why it exists: Ensures the check doesn't false-positive and block valid
    //   remove-missing operations.
    //
    // Scenario: Missing device has small allocations, survivors have plenty of
    //   unallocated space.
    fn check_relocation_space_passes_sufficient_space() {
        let fixture = "\
/dev/mapper/braid-disk1, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:           452984832

/dev/mapper/braid-disk2, ID: 2
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:           452984832

<missing disk>, ID: 3
   Device size:                  0
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:                  0

";

        let runner = EnospcRunner {
            device_usage_stdout: fixture,
        };

        let result = check_relocation_space(&runner, &mp(), None);
        assert!(result.is_ok(), "should pass: {result:?}");
    }

    #[test]
    // Intent: check_relocation_space with --missing-id only counts allocations
    //   for the targeted devid, not all missing devices.
    //
    // Why it exists: When multiple devices are missing, removing just one may
    //   be feasible even if removing all isn't.
    //
    // Scenario: Two missing devices, but only one is targeted. The targeted
    //   device has small allocations that fit in survivors.
    fn check_relocation_space_with_missing_id_filters() {
        // Two surviving devices (4-disk pool, 2 missing). The RAID1-aware check
        // requires >= 2 surviving devices with space, which this fixture satisfies.
        let fixture = "\
/dev/mapper/braid-disk1, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:           200000000

/dev/mapper/braid-disk4, ID: 4
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:           200000000

<missing disk>, ID: 2
   Device size:                  0
   Device slack:                  0
   Data,RAID1:             50000000
   Unallocated:                  0

<missing disk>, ID: 3
   Device size:                  0
   Device slack:                  0
   Data,RAID1:           5000000000
   Unallocated:                  0

";

        let runner = EnospcRunner {
            device_usage_stdout: fixture,
        };

        // Targeting devid 2 (50 MB Data) -- should pass: RAID1 capacity = 200 MB >= 50 MB
        let result = check_relocation_space(&runner, &mp(), Some(2));
        assert!(result.is_ok(), "targeting devid 2 should pass: {result:?}");

        // Targeting devid 3 (5 GB Data) -- should fail: RAID1 capacity = 200 MB < 5 GB
        let result = check_relocation_space(&runner, &mp(), Some(3));
        assert!(result.is_err(), "targeting devid 3 should fail");
    }

    #[test]
    // Intent: check_relocation_space proceeds gracefully when the command fails.
    //
    // Why it exists: A bug in the safety check shouldn't block a valid operation.
    //
    // Scenario: btrfs device usage returns an error (e.g., old kernel, permission issue).
    fn check_relocation_space_proceeds_on_command_error() {
        struct FailingRunner;
        impl CommandRunner for FailingRunner {
            fn run(&self, _request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
                Err(CmdError::MissingMock)
            }
            fn run_with_stdin(
                &self,
                request: &CmdRequest,
                _stdin: &[u8],
            ) -> Result<RawCommandOutput, CmdError> {
                self.run(request)
            }
        }

        let result = check_relocation_space(&FailingRunner, &mp(), None);
        assert!(result.is_ok(), "should proceed on error: {result:?}");
    }

    // --- compile_steps tests ---

    #[test]
    // Intent: dry-run with 1 missing + ≥2 survivors shows rebalance step.
    // Why: operator should see the soft balance step in the plan.
    // Scenario: 3-disk pool, 1 disk failed. Dry run should show the balance.
    fn compile_steps_shows_rebalance_when_clearing_last_missing() {
        let steps = compile_steps(3, true, 2, &MountPoint("/mnt/storage".into()));
        assert!(
            steps
                .iter()
                .any(|s| s.description.contains("-dconvert=raid1,soft")),
            "expected soft balance step; got: {:?}",
            steps.iter().map(|s| &s.description).collect::<Vec<_>>()
        );
    }

    #[test]
    // Intent: dry-run with 1 survivor omits rebalance step.
    // Why: can't have RAID1 with only 1 device.
    // Scenario: 2-disk pool, 1 died. Only 1 survivor -- no balance.
    fn compile_steps_omits_rebalance_with_single_survivor() {
        let steps = compile_steps(3, true, 1, &MountPoint("/mnt/storage".into()));
        assert!(
            !steps
                .iter()
                .any(|s| s.description.contains("-dconvert=raid1,soft")),
            "should not show soft balance with 1 survivor; got: {:?}",
            steps.iter().map(|s| &s.description).collect::<Vec<_>>()
        );
    }

    #[test]
    // Intent: dry-run when not clearing last missing omits rebalance step.
    // Why: if more missing devices remain, balance would be premature.
    // Scenario: 4-disk pool, 2 missing, removing 1 of them.
    fn compile_steps_omits_rebalance_when_not_last_missing() {
        let steps = compile_steps(3, false, 2, &MountPoint("/mnt/storage".into()));
        assert!(
            !steps
                .iter()
                .any(|s| s.description.contains("-dconvert=raid1,soft")),
            "should not show soft balance when not clearing last missing; got: {:?}",
            steps.iter().map(|s| &s.description).collect::<Vec<_>>()
        );
    }

    // --- RecordingRunner for 3-device pool scenarios ---

    /// 3-device pool RecordingRunner: 2 present + 1 missing.
    /// After remove-missing, shows 2 present + 0 missing (healthy).
    #[derive(Clone)]
    struct ThreeDeviceRunner {
        log: Arc<Mutex<Vec<CmdRequest>>>,
        /// If true, post-op probe still shows 1 missing
        still_degraded_after: bool,
        remove_thread_id: Option<Arc<Mutex<Option<std::thread::ThreadId>>>>,
        remove_done: Option<Arc<AtomicBool>>,
    }

    impl ThreeDeviceRunner {
        fn new(log: Arc<Mutex<Vec<CmdRequest>>>, still_degraded: bool) -> Self {
            Self {
                log,
                still_degraded_after: still_degraded,
                remove_thread_id: None,
                remove_done: None,
            }
        }

        fn with_thread_recorder(
            log: Arc<Mutex<Vec<CmdRequest>>>,
            still_degraded: bool,
            remove_thread_id: Arc<Mutex<Option<std::thread::ThreadId>>>,
            remove_done: Arc<AtomicBool>,
        ) -> Self {
            Self {
                log,
                still_degraded_after: still_degraded,
                remove_thread_id: Some(remove_thread_id),
                remove_done: Some(remove_done),
            }
        }
    }

    impl CommandRunner for ThreeDeviceRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());

            // Track whether we've already removed the missing device
            let remove_done = self
                .log
                .lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. }));

            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_out(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                    0,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => {
                    let (missing_line, total) = if remove_done && !self.still_degraded_after {
                        ("", 2)
                    } else {
                        ("\tdevid    3 size 0 used 0 path MISSING\n", 3)
                    };
                    Ok(mock_out(
                        &format!("btrfs filesystem show {mount_point}"),
                        &format!(
                            "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices {total} FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n{missing_line}",
                        ),
                        0,
                    ))
                }
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_out(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                    ),
                    0,
                )),
                CmdRequest::CryptsetupLuksUuid { .. } => Ok(mock_out(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
                    0,
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_out(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                    0,
                )),
                CmdRequest::BtrfsDeviceRemove { .. } => {
                    if let Some(remove_thread_id) = &self.remove_thread_id {
                        *remove_thread_id.lock().unwrap() = Some(std::thread::current().id());
                    }
                    if let Some(remove_done) = &self.remove_done {
                        remove_done.store(true, Ordering::SeqCst);
                    }
                    Ok(mock_out("btrfs device remove", "", 0))
                }
                CmdRequest::BtrfsBalanceRaid1Soft { .. } => {
                    Ok(mock_out("btrfs balance start -dconvert=raid1,soft", "", 0))
                }
                CmdRequest::BtrfsDeviceUsageRaw { .. } => {
                    // Return enough space for relocation check to pass
                    Ok(mock_out(
                        "btrfs device usage --raw",
                        "/dev/mapper/braid-disk1, ID: 1\n   Device size:           520093696\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:           452984832\n\n/dev/mapper/braid-disk2, ID: 2\n   Device size:           520093696\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:           452984832\n\n<missing disk>, ID: 3\n   Device size:                  0\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:                  0\n\n",
                        0,
                    ))
                }
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.run(request)
        }
    }

    fn three_device_config() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        tempfile::TempDir,
        StatePaths,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        let config_json = serde_json::json!({ "mount_point": "/mnt/storage" });
        std::fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();
        let (state_tmp, state_paths) = test_paths(&[
            ("disk1", "/dev/disk/by-id/virtio-disk1", Some(1)),
            ("disk2", "/dev/disk/by-id/virtio-disk2", Some(2)),
            ("disk3", "/dev/disk/by-id/virtio-disk3", Some(3)),
        ]);
        (tmp, config_path, state_tmp, state_paths)
    }

    /*
     * Intent: `plan_remove_missing` rejects a wrong `--missing-id` by
     * checking the missing devids reported by the already-probed pool.
     *
     * Why it exists: The target-validation contract must not regress
     * to a redundant `btrfs device usage --raw` probe before rejecting
     * an ID that is absent from `PoolState::missing_devids`.
     *
     * Scenario: a 3-device pool has devid 3 reported as `MISSING` by
     * `btrfs filesystem show`, but the operator passes
     * `--missing-id 99`.
     */
    #[test]
    fn plan_remove_missing_rejects_wrong_missing_id_from_pool_state() {
        let (_tmp, config_path, _state_tmp, state_paths) = three_device_config();

        struct WrongMissingIdRunner {
            log: Arc<Mutex<Vec<CmdRequest>>>,
        }

        impl CommandRunner for WrongMissingIdRunner {
            fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
                self.log.lock().unwrap().push(request.clone());
                match request {
                    CmdRequest::FindmntJson { mount_point } => Ok(mock_out(
                        &format!("findmnt --json --mountpoint {mount_point}"),
                        r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                        0,
                    )),
                    CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_out(
                        &format!("btrfs filesystem show {mount_point}"),
                        "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 3 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n\tdevid    3 size 0 used 0 path MISSING\n",
                        0,
                    )),
                    CmdRequest::CryptsetupStatus { mapper } => Ok(mock_out(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                        ),
                        0,
                    )),
                    CmdRequest::CryptsetupLuksUuid { .. } => Ok(mock_out(
                        "cryptsetup luksUUID",
                        "11111111-1111-1111-1111-111111111111\n",
                        0,
                    )),
                    CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_out(
                        "btrfs balance status",
                        "No balance found on '/mnt/storage'\n",
                        0,
                    )),
                    _ => Err(CmdError::MissingMock),
                }
            }

            fn run_with_stdin(
                &self,
                request: &CmdRequest,
                _stdin: &[u8],
            ) -> Result<RawCommandOutput, CmdError> {
                self.run(request)
            }
        }

        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = WrongMissingIdRunner {
            log: Arc::clone(&log),
        };
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let report = plan_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 99,
                dry_run: true,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
                sleep_inhibitor: &inhibitor,
                sleeper: &crate::progress::NoopSleeper,
            },
        );

        match &report.result {
            Err(RemoveMissingError::Validation(msg)) => assert_eq!(
                msg,
                "devid 99 is not a device in this pool. Use 'braid status' to see device IDs.",
            ),
            Err(other) => panic!("expected Validation, got: {other:?}"),
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
        }
        assert!(
            !log.lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceUsageRaw { .. })),
            "wrong-id validation must not call BtrfsDeviceUsageRaw"
        );
    }

    #[test]
    // Intent: 3-disk pool, 1 missing -> soft rebalance runs after remove-missing.
    // Why: clearing the last missing device should restore RAID1 for chunks
    // written during degraded operation.
    // Scenario: 3-disk NAS, one drive dies. Operator runs remove-missing.
    // After the removal, pool is healthy with 2 survivors -> soft balance runs.
    fn three_device_pool_soft_rebalance_runs() {
        let (_tmp, config_path, _state_tmp, state_paths) = three_device_config();
        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = ThreeDeviceRunner::new(log.clone(), false);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        cmd_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 3,
                dry_run: false,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
                sleep_inhibitor: &inhibitor,
                sleeper: &crate::progress::NoopSleeper,
            },
        )
        .expect("remove-missing should succeed");

        let calls = log.lock().unwrap();
        let remove_pos = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. }));
        let balance_pos = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsBalanceRaid1Soft { .. }));
        assert!(
            remove_pos.is_some(),
            "expected BtrfsDeviceRemove; calls: {calls:?}"
        );
        assert!(
            balance_pos.is_some(),
            "expected BtrfsBalanceRaid1Soft; calls: {calls:?}"
        );
        assert!(
            remove_pos.unwrap() < balance_pos.unwrap(),
            "remove-missing must happen before soft balance"
        );
        // Locks in the seam placement: a remove-missing that triggers the soft
        // balance must take the inhibitor exactly once before journal::write_journal,
        // and hold it across both the device remove and the soft balance.
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the path through journal::write_journal"
        );
    }

    #[test]
    // Intent: 3-disk pool, 2 missing, targeting 1 -> NO rebalance (still degraded).
    // Why: running a balance while still degraded is pointless.
    // Scenario: 3-disk NAS, 2 drives die. Operator removes 1 missing entry.
    // Pool still has 1 missing device -> no rebalance.
    fn three_device_two_missing_no_rebalance() {
        let (_tmp, config_path, _state_tmp, state_paths) = three_device_config();
        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = ThreeDeviceRunner::new(log.clone(), true);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        cmd_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 3,
                dry_run: false,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
                sleep_inhibitor: &inhibitor,
                sleeper: &crate::progress::NoopSleeper,
            },
        )
        .expect("remove-missing should succeed");

        let calls = log.lock().unwrap();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "should NOT call BtrfsBalanceRaid1Soft when still degraded; calls: {calls:?}"
        );
        // Even when no soft balance runs, the inhibitor must still be acquired
        // unconditionally before journal::write_journal -- the rule is "acquire
        // before journal", not "acquire when slow phase will run".
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once before journal::write_journal, \
             even when no soft balance runs"
        );
    }

    /*
     * Intent: cmd_remove_missing routes the device-remove phase through the
     * progress helper when progress output is enabled.
     * Why it exists: a direct runner.run or forced ProgressOutput::Off at this
     * layer would make slow `btrfs device remove <devid>` operations silent
     * again even though the pool helper itself supports heartbeats.
     * Scenario: a 3-device pool has one targeted missing device and remains
     * degraded after removal, so the only BtrfsDeviceRemove observed is the
     * remove-missing phase; the mock records which thread ran that command.
     */
    #[test]
    fn device_remove_runs_on_progress_worker_thread() {
        let (_tmp, config_path, _state_tmp, state_paths) = three_device_config();
        let log = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::new(Mutex::new(None));
        let remove_done = Arc::new(AtomicBool::new(false));
        let runner = ThreeDeviceRunner::with_thread_recorder(
            log,
            true,
            Arc::clone(&recorded),
            Arc::clone(&remove_done),
        );
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        struct WaitForRemoveDoneSleeper {
            remove_done: Arc<AtomicBool>,
        }
        impl progress::Sleeper for WaitForRemoveDoneSleeper {
            fn sleep(&self, _duration: std::time::Duration) {
                while !self.remove_done.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        let sleeper = WaitForRemoveDoneSleeper {
            remove_done: Arc::clone(&remove_done),
        };
        let calling_thread = std::thread::current().id();

        cmd_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 3,
                dry_run: false,
                yes: true,
                progress: crate::progress::ProgressOutput::Human,
                paths: &state_paths,
                sleep_inhibitor: &inhibitor,
                sleeper: &sleeper,
            },
        )
        .expect("remove-missing should succeed");

        let observed = recorded
            .lock()
            .unwrap()
            .expect("BtrfsDeviceRemove must be dispatched");
        assert_ne!(
            observed, calling_thread,
            "BtrfsDeviceRemove must run on the progress helper worker thread when \
             ProgressOutput::Human is threaded through"
        );
    }

    /// Runner for 3-device pool where the soft balance fails after successful
    /// device removal. Everything succeeds except BtrfsBalanceRaid1Soft.
    struct FailingSoftBalanceRunner {
        log: Arc<Mutex<Vec<CmdRequest>>>,
    }

    impl FailingSoftBalanceRunner {
        fn new(log: Arc<Mutex<Vec<CmdRequest>>>) -> Self {
            Self { log }
        }
    }

    impl CommandRunner for FailingSoftBalanceRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());

            let remove_done = self
                .log
                .lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. }));

            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_out(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                    0,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => {
                    let (missing_line, total) = if remove_done {
                        ("", 2)
                    } else {
                        ("\tdevid    3 size 0 used 0 path MISSING\n", 3)
                    };
                    Ok(mock_out(
                        &format!("btrfs filesystem show {mount_point}"),
                        &format!(
                            "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices {total} FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n{missing_line}",
                        ),
                        0,
                    ))
                }
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_out(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                    ),
                    0,
                )),
                CmdRequest::CryptsetupLuksUuid { .. } => Ok(mock_out(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
                    0,
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_out(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                    0,
                )),
                CmdRequest::BtrfsDeviceRemove { .. } => Ok(mock_out("btrfs device remove", "", 0)),
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(mock_out(
                    "btrfs device usage --raw",
                    "/dev/mapper/braid-disk1, ID: 1\n   Device size:           520093696\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:           452984832\n\n/dev/mapper/braid-disk2, ID: 2\n   Device size:           520093696\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:           452984832\n\n<missing disk>, ID: 3\n   Device size:                  0\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:                  0\n\n",
                    0,
                )),
                CmdRequest::BtrfsBalanceRaid1Soft { .. } => Ok(RawCommandOutput {
                    cmd: "btrfs balance start -dconvert=raid1,soft".into(),
                    stdout: String::new(),
                    stderr: "ERROR: error during balancing: No space left on device".into(),
                    exit_status: 1,
                }),
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.run(request)
        }
    }

    /// Runner for 3-device pool where btrfs device remove itself fails.
    struct FailingRemoveRunner {
        log: Arc<Mutex<Vec<CmdRequest>>>,
    }

    impl FailingRemoveRunner {
        fn new(log: Arc<Mutex<Vec<CmdRequest>>>) -> Self {
            Self { log }
        }
    }

    impl CommandRunner for FailingRemoveRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());

            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_out(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                    0,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_out(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 3 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n\tdevid    3 size 0 used 0 path MISSING\n",
                    0,
                )),
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_out(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                    ),
                    0,
                )),
                CmdRequest::CryptsetupLuksUuid { .. } => Ok(mock_out(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
                    0,
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_out(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                    0,
                )),
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(mock_out(
                    "btrfs device usage --raw",
                    "/dev/mapper/braid-disk1, ID: 1\n   Device size:           520093696\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:           452984832\n\n/dev/mapper/braid-disk2, ID: 2\n   Device size:           520093696\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:           452984832\n\n<missing disk>, ID: 3\n   Device size:                  0\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:                  0\n\n",
                    0,
                )),
                CmdRequest::BtrfsDeviceRemove { .. } => Ok(RawCommandOutput {
                    cmd: "btrfs device remove 3 /mnt/storage".into(),
                    stdout: String::new(),
                    stderr: "ERROR: error removing device: No space left on device".into(),
                    exit_status: 1,
                }),
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.run(request)
        }
    }

    /*
     * Intent: pending-op.json survives when the btrfs device-remove phase
     * fails before save_membership can run.
     *
     * Why it exists: remove-missing must not persist the target pool.json
     * until `btrfs device remove <devid>` succeeds. If save_membership ran
     * before pool_remove_device_using, this device-remove failure would
     * leave pool.json reconciled without the btrfs operation having
     * committed.
     *
     * Scenario: 3-disk NAS, one drive dies. Operator runs remove-missing, but
     * `btrfs device remove <devid>` fails mid-relocation with ENOSPC. The
     * journal must persist and pool.json must still contain the target disk so
     * `braid recover` can reconcile.
     */
    #[test]
    fn journal_survives_device_remove_failure() {
        let (_tmp, config_path, _state_tmp, state_paths) = three_device_config();

        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = FailingRemoveRunner::new(log.clone());
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 3,
                dry_run: false,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
                sleep_inhibitor: &inhibitor,
                sleeper: &crate::progress::NoopSleeper,
            },
        );

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("btrfs device remove failed (exit 1)"),
            "remove-missing should fail from the device-remove step: {err}"
        );
        assert!(
            journal::load_journal(&state_paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
        assert!(
            membership::load_membership(&state_paths)
                .unwrap()
                .disks
                .contains_key("disk3"),
            "pool.json must still contain the target disk when device remove fails"
        );
        let calls = log.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. })),
            "expected BtrfsDeviceRemove; calls: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "soft balance must not run after device remove fails; calls: {calls:?}"
        );
        // The journal exists, which proves we got past journal::write_journal,
        // which proves the inhibitor was acquired exactly once on the way in.
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the path through journal::write_journal"
        );
    }

    #[test]
    // Intent: pending-op.json survives when soft balance fails after a successful
    //   device removal.
    //
    // Why it exists: remove-missing previously cleared the journal before
    //   maybe_restore_raid1(). If the soft balance failed, the journal was already
    //   gone despite an irreversible pool change, leaving pool.json stale with
    //   no recovery path.
    //
    // Scenario: 3-disk NAS, one drive dies. Operator runs remove-missing. The
    //   device removal succeeds but the post-removal soft balance fails. The
    //   journal must persist so `braid recover` can reconcile.
    fn journal_survives_soft_balance_failure() {
        let (_tmp, config_path, _state_tmp, state_paths) = three_device_config();

        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = FailingSoftBalanceRunner::new(log.clone());
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 3,
                dry_run: false,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
                sleep_inhibitor: &inhibitor,
                sleeper: &crate::progress::NoopSleeper,
            },
        );

        assert!(
            result.is_err(),
            "remove-missing should fail when soft balance fails"
        );
        assert!(
            journal::load_journal(&state_paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
        // Membership commits at btrfs device remove; pool.json must reflect
        // the removed missing disk even when the post-remove soft balance
        // fails. Reverting save_membership back to its old position (after
        // maybe_restore_raid1) makes these assertions fail.
        let saved = membership::load_membership(&state_paths)
            .expect("pool.json must exist after the membership commit");
        assert!(
            !saved.disks.contains_key("disk3"),
            "removed missing disk must be gone from pool.json even when the \
             post-remove soft balance fails (saved: {:?})",
            saved.disks.keys().collect::<Vec<_>>()
        );
        assert!(
            saved.disks.contains_key("disk1") && saved.disks.contains_key("disk2"),
            "surviving disks must remain in pool.json (saved: {:?})",
            saved.disks.keys().collect::<Vec<_>>()
        );
        // The journal exists, which proves we got past journal::write_journal,
        // which proves the inhibitor was acquired exactly once on the way in.
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the path through journal::write_journal"
        );
    }

    #[test]
    // Intent: when soft balance fails with ENOSPC, the surfaced error includes
    //   the recovery hint with a concrete `dusage=0` command.
    // Why: the hint is appended in pool::balance_error, but it must survive
    //   PoolError -> RemoveMissingError::Pool -> Display without being lost.
    // Scenario: 3-disk NAS, one drive dies. Operator runs remove-missing. Device
    //   removal succeeds but the post-removal soft balance hits ENOSPC. The error
    //   message should guide the user to free empty block groups.
    fn enospc_hint_surfaces_through_error_chain() {
        let (_tmp, config_path, _state_tmp, state_paths) = three_device_config();

        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = FailingSoftBalanceRunner::new(log.clone());
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 3,
                dry_run: false,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
                sleep_inhibitor: &inhibitor,
                sleeper: &crate::progress::NoopSleeper,
            },
        );

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("hint:"),
            "error should contain recovery hint: {err}"
        );
        assert!(
            err.contains("dusage=0"),
            "error should suggest dusage=0 filter: {err}"
        );
    }

    // --- resolve_removal_target tests ---

    #[test]
    // Intent: resolve_removal_target fails when devid is known but no
    //   pool.json member has that devid enriched.
    //
    // Why it exists: If devid enrichment was skipped or failed, the lookup
    //   returns None. Previously the code silently proceeded, leaving
    //   pool.json unchanged despite the btrfs device being removed.
    //
    // Scenario: User enrolled a disk before devid enrichment existed, then
    //   the disk fails. remove-missing has a devid from btrfs but can't
    //   match it to any pool.json entry.
    fn resolve_target_fails_when_devid_not_in_membership() {
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".to_string(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".to_string())),
        );
        let err = resolve_removal_target(99, &m).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not found in pool.json"),
            "expected pool.json membership error; got: {msg}"
        );
    }

    #[test]
    // Intent: dry-run for targeted missing-device removal shows the devid command.
    // Why: verifies CmdRequest integration for the targeted removal path.
    // Scenario: one missing device (devid 2), last missing, 2 present -> includes balance.
    fn dry_run_render_targeted_removal_with_balance() {
        let mount_point = MountPoint("/mnt/storage".into());
        let steps = compile_steps(2, true, 2, &mount_point);
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // 2 steps: device remove + balance, each with 1 command = 4 lines
        assert_eq!(lines.len(), 4, "expected 4 lines, got:\n{output}");
        assert!(lines[0].contains("target specific missing device"));
        assert_eq!(
            lines[1],
            "               $ btrfs device remove --enqueue 2 /mnt/storage"
        );
        assert!(lines[2].contains("restore redundancy"));
        assert_eq!(
            lines[3],
            "               $ btrfs balance start --enqueue '-dconvert=raid1,soft' '-mconvert=raid1,soft' /mnt/storage"
        );
    }

    // --- Confirmation formatter tests ---

    #[test]
    fn remove_missing_confirm_with_rebalance() {
        let msg = format_remove_missing_confirm("toshiba", 2, 2, 1);
        assert!(msg.contains("Remove missing device from pool:"));
        assert!(msg.contains("toshiba (devid 2)"));
        assert!(msg.contains("missing"));
        assert!(msg.contains("no hardware info available"));
        assert!(msg.contains("rebalanced"));
        assert!(msg.contains("2 present + 1 missing -> 2 disks"));
    }

    #[test]
    fn remove_missing_confirm_single_survivor() {
        let msg = format_remove_missing_confirm("toshiba", 2, 1, 1);
        assert!(msg.contains("Surviving disk already has all data"));
        assert!(msg.contains("1 present + 1 missing -> 1 disk"));
    }

    // --- plan_remove_missing soft-warn tests ---

    /// 3-device pool runner where the single `BtrfsDeviceUsageRaw`
    /// call comes from `check_relocation_space` and fails per
    /// `failure_mode`. Lets us exercise the soft-warn paths from
    /// `plan_remove_missing` without tripping the earlier validations.
    #[derive(Clone, Copy)]
    enum UsageFailureMode {
        /// Command error from the relocation-space preflight (models a failing
        /// `btrfs device usage --raw` invocation).
        CmdError,
        /// Unparseable output from the relocation-space preflight (models upstream
        /// output drift that breaks `parse_btrfs_device_usage`).
        ParseError,
    }

    struct ThreeDeviceSoftWarnRunner {
        failure_mode: UsageFailureMode,
    }

    impl ThreeDeviceSoftWarnRunner {
        fn new(failure_mode: UsageFailureMode) -> Self {
            Self { failure_mode }
        }
    }

    impl CommandRunner for ThreeDeviceSoftWarnRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_out(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                    0,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_out(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 3 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n\tdevid    3 size 0 used 0 path MISSING\n",
                    0,
                )),
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_out(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                    ),
                    0,
                )),
                CmdRequest::CryptsetupLuksUuid { .. } => Ok(mock_out(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
                    0,
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_out(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                    0,
                )),
                CmdRequest::BtrfsDeviceUsageRaw { .. } => match self.failure_mode {
                    UsageFailureMode::CmdError => Err(CmdError::MissingMock),
                    UsageFailureMode::ParseError => {
                        // Nonzero exit_status funnels through
                        // `ParseError::CommandFailed` inside
                        // `parse_btrfs_device_usage`; the parser itself
                        // is forgiving of unknown lines, so this is the
                        // narrowest way to force the parse-error soft-warn
                        // branch.
                        Ok(RawCommandOutput {
                            cmd: "btrfs device usage --raw".to_owned(),
                            stdout: String::new(),
                            stderr: "boom".to_owned(),
                            exit_status: 1,
                        })
                    }
                },
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.run(request)
        }
    }

    /* Intent: when the relocation-space preflight fails with a command
     * error, `plan_remove_missing` returns a successful plan carrying
     * one `PreviewNote::Warn` with the ENOSPC soft-warn body and the
     * usual compiled steps.
     *
     * Why it exists: PR 3 moves the direct `eprintln!("warning: ...")`
     * into the preview model. Without this test, a regression that
     * dropped the note or re-added the direct stderr print from the
     * preflight helper would still pass the older "proceeds on command
     * error" test (which only asserts control flow).
     *
     * Scenario: 3-disk RAID1 pool with devid 3 missing; the
     * `btrfs device usage --raw` call from check_relocation_space fails
     * with a CmdError so the planner routes the soft-warn into
     * plan.notes instead of stderr.
     */
    #[test]
    fn plan_remove_missing_surfaces_soft_warn_on_command_error() {
        let (_tmp, config_path, _state_tmp, state_paths) = three_device_config();
        let runner = ThreeDeviceSoftWarnRunner::new(UsageFailureMode::CmdError);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let report = plan_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 3,
                dry_run: true,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
                sleep_inhibitor: &inhibitor,
                sleeper: &crate::progress::NoopSleeper,
            },
        );
        assert!(
            report.notes.is_empty(),
            "report.notes must stay empty on success -- notes land on plan.notes"
        );
        let plan = report.result.expect("planning should succeed");
        assert_eq!(plan.notes.len(), 1, "expected exactly one soft-warn note");
        match &plan.notes[0] {
            PreviewNote::Warn(body) => {
                assert!(
                    body.starts_with("ENOSPC pre-flight check failed: "),
                    "warning body must start with the canonical prefix; got {body:?}"
                );
                assert!(
                    body.ends_with("; proceeding anyway"),
                    "warning body must end with the canonical suffix; got {body:?}"
                );
                assert!(
                    !body.starts_with("warning:"),
                    "warn body must not carry the legacy 'warning:' prefix; got {body:?}"
                );
            }
            other => panic!("expected PreviewNote::Warn, got {other:?}"),
        }
        // Steps are still compiled: devid 3 is the last missing in a
        // 2-survivor pool so the soft balance step must be present.
        let descriptions: Vec<&str> = plan.steps.iter().map(|s| s.description.as_str()).collect();
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("target specific missing device")),
            "expected device remove step; got {descriptions:?}"
        );
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("restore redundancy")),
            "expected soft balance step; got {descriptions:?}"
        );
    }

    /* Intent: when the relocation-space preflight's parse fails, the
     * planner still returns a successful plan carrying the soft-warn
     * note, same as the command-error branch.
     *
     * Why it exists: `check_relocation_space` has two soft-warn
     * branches (command error, parse error). Both used to share one
     * `eprintln!` site; after the refactor, each builds a warn body
     * independently. This test is the parse-error twin of the
     * command-error test above and guards against drift between the
     * two bodies.
     *
     * Scenario: same 3-disk pool; the `btrfs device usage --raw` call
     * returns unparseable stdout, triggering
     * `parse_btrfs_device_usage` to error.
     */
    #[test]
    fn plan_remove_missing_surfaces_soft_warn_on_parse_error() {
        let (_tmp, config_path, _state_tmp, state_paths) = three_device_config();
        let runner = ThreeDeviceSoftWarnRunner::new(UsageFailureMode::ParseError);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let report = plan_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 3,
                dry_run: true,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
                sleep_inhibitor: &inhibitor,
                sleeper: &crate::progress::NoopSleeper,
            },
        );
        let plan = report.result.expect("planning should succeed");
        assert_eq!(plan.notes.len(), 1, "expected exactly one soft-warn note");
        match &plan.notes[0] {
            PreviewNote::Warn(body) => {
                assert!(
                    body.starts_with("ENOSPC pre-flight check failed: "),
                    "warning body must start with the canonical prefix; got {body:?}"
                );
                assert!(
                    body.ends_with("; proceeding anyway"),
                    "warning body must end with the canonical suffix; got {body:?}"
                );
            }
            other => panic!("expected PreviewNote::Warn, got {other:?}"),
        }
        assert!(!plan.steps.is_empty(), "steps must still be compiled");
    }

    /* Intent: `plan.preview().render()` places the ENOSPC soft-warn
     * line above the step block and uses the canonical `[warn] <body>`
     * shape (no legacy `warning:` prefix).
     *
     * Why it exists: the dry-run stdout contract for remove-missing in
     * PR 3 is "warn note(s) render before steps" and the warn body is
     * body-only. Without a preview-boundary test, a regression that
     * rendered the warn inline with steps, dropped the `[warn] `
     * prefix, or re-added the `warning:` prefix would only surface in
     * the VM stream-routing test -- adding a unit guardrail here is
     * cheap and catches drift before the VM layer.
     *
     * Scenario: a hand-built plan with one soft-warn note and the
     * compiled steps for devid-3 removal on a 2-survivor pool; assert
     * the rendered byte sequence starts with the warn line and is
     * followed by the dry-run step lines.
     */
    #[test]
    fn plan_preview_renders_warn_above_steps() {
        let plan = RemoveMissingPlan {
            notes: vec![PreviewNote::Warn(
                "ENOSPC pre-flight check failed: boom; proceeding anyway".into(),
            )],
            steps: compile_steps(3, true, 2, &MountPoint("/mnt/storage".into())),
            missing_id: 3,
            will_clear_last_missing: true,
            remaining_present: 2,
            missing_count: 1,
            mount_point: MountPoint("/mnt/storage".into()),
        };
        let rendered = plan.preview().render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines[0], "[warn] ENOSPC pre-flight check failed: boom; proceeding anyway",
            "warn note must render first; got full output:\n{rendered}"
        );
        assert!(
            lines[1].starts_with("[long"),
            "step block must follow the warn line; got lines[1]={:?}",
            lines[1]
        );
        assert!(
            lines[1].contains("target specific missing device"),
            "first step must be the device-remove step; got lines[1]={:?}",
            lines[1]
        );
    }

    /* Intent: plan-derived Warn notes for `remove-missing` render
     * through the shared `preview::render_notes_for_stderr` helper as
     * the canonical `[warn] <body>\n` shape -- the same shape that
     * `Preview::render` emits on dry-run stdout. Legacy `warning: `
     * prefixes do not appear.
     * Why it exists: this follow-up removes the direct
     * `eprintln!("warning: {body}")` replay from
     * `RemoveMissingPlan::execute` so real-run stderr now uses the
     * canonical form. A regression that reintroduces the legacy
     * prefix -- either in execute's replay or by re-wrapping the body
     * -- fails here.
     * Scenario: hand-built notes vec with one soft-warn body; render
     * via `RemoveMissingPlan::STDERR_STYLE` and assert byte-exact
     * output with no `warning:` substring.
     */
    #[test]
    fn remove_missing_warn_notes_render_canonical_bracketed_form() {
        let notes = vec![PreviewNote::Warn(
            "ENOSPC pre-flight check failed: boom; proceeding anyway".into(),
        )];
        let rendered = preview::render_notes_for_stderr(&notes, RemoveMissingPlan::STDERR_STYLE);
        assert_eq!(
            rendered,
            "[warn] ENOSPC pre-flight check failed: boom; proceeding anyway\n",
        );
        assert!(
            !rendered.contains("warning:"),
            "legacy `warning:` prefix must be gone from remove-missing's render;\n{rendered}",
        );
    }

    /// MockFs variant serving a configurable sysfs exclusive_operation
    /// body. Drives preflight's busy-op / paused-balance branches from
    /// the plan_remove_missing boundary tests.
    struct MockFsWithExclop(String);

    impl Filesystem for MockFsWithExclop {
        fn exists(&self, _path: &str) -> bool {
            false
        }
        fn is_block_device(&self, _path: &str) -> bool {
            false
        }
        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path.ends_with("/exclusive_operation") {
                Ok(format!("{}\n", self.0))
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
            }
        }
        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    /* Intent: plan_remove_missing surfaces an in-flight exclusive op
     * as a PreviewNote::Info on `plan.notes`, and the rendered preview
     * contains the "waiting for in-flight <op>" line.
     * Why it exists: PR 7 moves the busy-op diagnostic from a direct
     * stderr eprintln! into plan.notes. A regression that leaked the
     * wording back to stderr would break the dry-run stdout-only
     * contract.
     * Scenario: 3-device pool with 1 missing device, sysfs reports
     * "device add". Operator runs `braid remove-missing --missing-id 3
     * --dry-run`.
     */
    #[test]
    fn plan_remove_missing_preflight_busy_op_becomes_info_note() {
        let (_tmp, config_path, _state_tmp, state_paths) = three_device_config();
        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = ThreeDeviceRunner::new(log.clone(), false);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let report = plan_remove_missing(
            &runner,
            &MockFsWithExclop("device add".into()),
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 3,
                dry_run: true,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
                sleep_inhibitor: &inhibitor,
                sleeper: &crate::progress::NoopSleeper,
            },
        );
        let plan = report
            .result
            .expect("plan_remove_missing should succeed with 1 missing + busy op");
        let info_notes: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Info(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(
            info_notes.len(),
            1,
            "expected exactly one Info note, got: {:?}",
            plan.notes
        );
        assert!(
            info_notes[0].contains("waiting for in-flight"),
            "info body={:?}",
            info_notes[0],
        );
        assert!(
            info_notes[0].contains("device add"),
            "info body={:?}",
            info_notes[0],
        );
        let rendered = plan.preview().render();
        assert!(
            rendered.contains("waiting for in-flight device add"),
            "rendered preview must carry the busy-op Info line, got:\n{rendered}",
        );
    }

    /* Intent: when plan_remove_missing accumulates a preflight Info
     * note and then fails on the "no missing devices" validation
     * (missing_count == 0), the accumulated notes survive on
     * `report.notes`.
     * Why it exists: Shape A "notes-carrying report" promises
     * preserved context; a spurious remove-missing invocation during
     * an in-flight balance must not hide the busy-op context from the
     * operator.
     * Scenario: 2-device healthy pool (zero missing), sysfs reports
     * "device add". Operator runs `braid remove-missing --missing-id
     * 999 --dry-run`.
     */
    #[test]
    fn plan_remove_missing_preserves_preflight_notes_on_no_missing_devices() {
        let (_state_tmp, state_paths) = test_paths(&[
            ("disk1", "/dev/disk/by-id/virtio-disk1", Some(1)),
            ("disk2", "/dev/disk/by-id/virtio-disk2", Some(2)),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        let config_json = serde_json::json!({ "mount_point": "/mnt/storage" });
        std::fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();

        let runner = HealthyPoolRunner;
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let report = plan_remove_missing(
            &runner,
            &MockFsWithExclop("device add".into()),
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 999,
                dry_run: true,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
                sleep_inhibitor: &inhibitor,
                sleeper: &crate::progress::NoopSleeper,
            },
        );
        match &report.result {
            Err(RemoveMissingError::Validation(msg)) => {
                assert!(
                    msg.contains("no missing devices detected"),
                    "expected 'no missing devices detected' in: {msg}"
                );
                assert!(
                    msg.contains("devid 999"),
                    "expected requested devid in no-missing validation: {msg}"
                );
            }
            Err(other) => panic!("expected Validation, got: {other:?}"),
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
        }
        assert_eq!(
            report.notes.len(),
            1,
            "busy-op Info note must survive the no-missing failure, got: {:?}",
            report.notes,
        );
        assert!(
            matches!(
                &report.notes[0],
                PreviewNote::Info(b) if b.contains("waiting for in-flight") && b.contains("device add")
            ),
            "notes[0]={:?}",
            report.notes[0],
        );
    }

    /* Intent: a healthy pool with a live-device `--missing-id` still fails
     * with the zero-missing validation before live-device validation.
     * Why it exists: the explicit `missing_count == 0` branch preserves
     * existing error precedence; moving that wording into the membership
     * check would change the user-facing error for live devids in healthy
     * pools.
     * Scenario: 2-device healthy pool. Operator accidentally runs
     * `braid remove-missing --missing-id 1 --dry-run`.
     */
    #[test]
    fn plan_remove_missing_zero_missing_precedes_live_device_validation() {
        let (_state_tmp, state_paths) = test_paths(&[
            ("disk1", "/dev/disk/by-id/virtio-disk1", Some(1)),
            ("disk2", "/dev/disk/by-id/virtio-disk2", Some(2)),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        let config_json = serde_json::json!({ "mount_point": "/mnt/storage" });
        std::fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();

        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let report = plan_remove_missing(
            &HealthyPoolRunner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 1,
                dry_run: true,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
                sleep_inhibitor: &inhibitor,
                sleeper: &crate::progress::NoopSleeper,
            },
        );

        match &report.result {
            Err(RemoveMissingError::Validation(msg)) => {
                assert!(
                    msg.contains("no missing devices detected"),
                    "expected no-missing validation, got: {msg}"
                );
                assert!(
                    !msg.contains("live device"),
                    "zero-missing validation must precede live-device validation: {msg}"
                );
                assert!(
                    msg.contains("devid 1"),
                    "expected requested devid in no-missing validation: {msg}"
                );
            }
            Err(other) => panic!("expected Validation, got: {other:?}"),
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
        }
    }

    /* Intent: a null-underlying hot-unplugged device is not reported as
     * "no missing devices" just because `PoolState::missing_devids` is empty.
     * Why it exists: `remove-missing` must key the no-missing wording on
     * `missing_count == 0`, not `missing_devids.is_empty()`. Hot-unplugged
     * mapper-present devices can yield `missing_count > 0` while btrfs has
     * not emitted a `MISSING` sentinel devid.
     * Scenario: 2-device pool where btrfs still lists both mapper paths, but
     * `cryptsetup status braid-disk2` reports `device: (null)`. Operator tries
     * `braid remove-missing --missing-id 2 --dry-run`.
     */
    #[test]
    fn plan_remove_missing_null_underlying_empty_missing_devids_not_no_missing() {
        let (_state_tmp, state_paths) = test_paths(&[
            ("disk1", "/dev/disk/by-id/virtio-disk1", Some(1)),
            ("disk2", "/dev/disk/by-id/virtio-disk2", Some(2)),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        let config_json = serde_json::json!({ "mount_point": "/mnt/storage" });
        std::fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();

        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let report = plan_remove_missing(
            &NullUnderlyingPoolRunner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 2,
                dry_run: true,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
                sleep_inhibitor: &inhibitor,
                sleeper: &crate::progress::NoopSleeper,
            },
        );

        match &report.result {
            Err(RemoveMissingError::Validation(msg)) => {
                assert!(
                    !msg.contains("no missing devices detected"),
                    "null-underlying pool must not use no-missing wording: {msg}"
                );
                assert_eq!(
                    msg,
                    "devid 2 is not a device in this pool. Use 'braid status' to see device IDs.",
                );
            }
            Err(other) => panic!("expected Validation, got: {other:?}"),
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
        }
    }
}
