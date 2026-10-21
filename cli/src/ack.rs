use std::time::SystemTime;

use crate::alert::{
    self, AlertCause, EnospcAck, UnbackedCause, commit_latched_evidence, live_pool_key,
    load_acked_stats_or_quarantine, save_acked_stats, save_enospc_ack,
};
use crate::capacity::evaluate_enospc_risk;
use crate::cmd::{CmdRequest, CommandRunner, apply_child_env};
use crate::filesystem::Filesystem;
use crate::parse::parse_btrfs_device_usage;
use crate::probe::{AlertPoolState, ProbeError, probe_pool_alerts};
use crate::state_paths::StatePaths;
use crate::types::MountPoint;
use crate::util::detail_suffix;

/// Shared no-count ack confirmation for acks that complete real cleanup but
/// have no latched cause to count: a bare smartd or scrub-failed flag, a
/// corrupt latch, or the cleanup-only retry.
const ACK_NO_COUNT_LINE: &str = "acknowledged current alerts";

/// Production entry point that wires ack to the real alert-unit stop hook.
///
/// Tests call `cmd_ack_impl` directly with explicit hooks so they never shell
/// out to host systemd while still exercising the same ack logic.
pub fn cmd_ack<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> Result<(), AckError> {
    cmd_ack_impl(runner, fs, mount_point, paths, &stop_beeper)
}

/// Injectable-hook variant used by tests -- in this module and the monitor's
/// re-ack integration test -- to exercise the real ack path without shelling out
/// to systemd.
///
/// Production goes through `cmd_ack`, which supplies the real systemd hook.
/// This is ack's one stop call site: it runs after the record work on every
/// outcome except a refusal (`AckError::is_refusal`), and is not gated on the
/// entry snapshot, because the units can run with no record on disk. See
/// `docs/design/decisions/014-alerts.md#ack-always-silences-after-its-record-work`.
pub(crate) fn cmd_ack_impl<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
    stop_beeper: &dyn Fn(),
) -> Result<(), AckError> {
    let records = ack_alert_records(runner, fs, mount_point, paths);
    if !matches!(&records, Err(e) if e.is_refusal()) {
        stop_beeper();
    }
    // Print the confirmation only after the stop, so it follows any stop
    // warning and reports a finished ack.
    println!("{}", records?);
    Ok(())
}

/// Ack's record work -- latched-evidence commit, flag removal, latch removal --
/// gated on one snapshot of the alert files taken at entry.
///
/// Returns the confirmation line instead of printing it, so `cmd_ack_impl`
/// can stop the alert units first. Silencing is not this function's job.
fn ack_alert_records<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> Result<String, AckError> {
    // 1. Snapshot the entry state before probing (see `AckEntry::read`). A
    //    sentinel-only snapshot finishes the interrupted cleanup here, before
    //    the probe, so recovery does not depend on probe success and does not
    //    rewrite the acknowledged baseline. `None` below is a `Quiet` snapshot.
    let pending = match AckEntry::read(paths) {
        AckEntry::CleanupOnly => {
            cleanup_alert_files(paths, &CleanupPlan::default()).map_err(AckError::CleanupFailed)?;
            return Ok(ACK_NO_COUNT_LINE.to_owned());
        }
        AckEntry::Quiet => None,
        AckEntry::Alert(pending) => Some(pending),
    };

    // 2. Classify the mount point: mounted btrfs, genuinely unmounted, or a
    //    foreign filesystem. The probe runs before the no-signal no-op so a
    //    foreign fstype or a mountinfo fault still fails ack (ADR 014 Offline
    //    ack policy) instead of hiding behind `no active alerts`.
    let pool = match probe_pool_alerts(runner, fs, mount_point) {
        Ok(p) => p,
        Err(e) => return Err(AckError::Probe(e)),
    };

    // Nothing to acknowledge is one no-op, decided from the entry snapshot
    // alone: mount state never changes its outcome. It changes no alert
    // file, but `cmd_ack_impl` still stops the alert units after it.
    let Some(pending) = pending else {
        return Ok("no active alerts".to_owned());
    };

    // 3. Commit what the latch says the operator is acknowledging, mounted or
    //    offline alike. Ack reads no live counters or membership, so nothing
    //    the monitor never latched can be acknowledged unseen.
    commit_acked_baseline(&pending.causes, paths)?;

    // 4. ENOSPC snooze (mounted only: it needs a live usage probe). If the
    //    latch carries an EnospcRisk cause, snooze the reminder from one fresh
    //    usage probe -- when the pool is still at risk, write the live pool key
    //    + a reminder deadline one interval out. Ack snoozes the reminder, it
    //    does not resolve the risk. Best-effort: a probe/parse failure, an
    //    absent FSID, or a pool that recovered by ack time clears the latch but
    //    writes no marker (a later recurrence fires armed). Offline ack writes
    //    no marker either.
    if pool.mounted
        && pending
            .causes
            .iter()
            .any(|c| matches!(c, AlertCause::EnospcRisk { .. }))
    {
        let missing_count = pool.alert_devids().missing.len() as u64;
        let now = SystemTime::now();
        write_enospc_baseline(runner, mount_point, &pool, missing_count, paths, now);
    }

    cleanup_alert_files(paths, &pending.cleanup).map_err(AckError::CleanupFailed)?;

    // 5. Report a count for latched causes. Flag-only and corrupt-latch
    // acknowledgments still completed real cleanup, but have no latched cause
    // to count.
    Ok(format_ack_confirmation(pending.causes.len()))
}

/// Ack's entry snapshot, reduced to the states that gate its record work.
///
/// ADR 014 gates every record decision in one ack on a single snapshot taken
/// before the probe. Only `AckEntry::read` sees the raw latch, flag, and
/// sentinel inputs, so the has-alert test and the flag remove rule exist once
/// and no later gate can re-spell them.
enum AckEntry {
    /// No alert signal and no interrupted cleanup.
    Quiet,
    /// Only the `alert-cleanup-pending` sentinel: an earlier ack failed partway
    /// through cleanup and left nothing else to acknowledge.
    CleanupOnly,
    /// At least one alert signal: a latched cause, an unreadable latch, or a
    /// flag file.
    Alert(PendingAck),
}

/// The alert an `AckEntry::Alert` snapshot asks ack to clear.
///
/// Holds no unreadable-latch bit: that bit only decides whether an alert
/// exists. A corrupt latch arrives here with no causes, so it commits no
/// baseline, and cleanup removes the latch and its corrupt sidecar either way.
struct PendingAck {
    /// Latched causes with their first-detection stamps stripped: ack decides
    /// what to clear and which evidence to commit from the causes alone, never
    /// the timestamps.
    causes: Vec<AlertCause>,
    cleanup: CleanupPlan,
}

/// Which flag files this ack's cleanup may remove, fixed at entry.
///
/// A flag that a hook writes during the probe survives for the next monitor
/// cycle unless the snapshot already covered its source. The default plan
/// removes no flag, which is what sentinel-only recovery needs.
#[derive(Default)]
struct CleanupPlan {
    smartd: bool,
    scrub_failed: bool,
}

impl AckEntry {
    /// Reads the latch, both flag files, and the cleanup-pending sentinel once,
    /// before `probe_pool_alerts`.
    ///
    /// The smartd and scrub-failed hooks run without the pool lock and can
    /// fire during the probe, so a flag read after it could flip the no-op
    /// gate or be swallowed by cleanup. An unreadable latch counts as an alert
    /// so the user can clear it even with the pool offline.
    fn read(paths: &StatePaths) -> Self {
        let (causes, latch_corrupt) = match alert::load_alert_latch(paths) {
            Ok(Some(s)) => (
                s.causes.into_iter().map(|c| c.cause).collect::<Vec<_>>(),
                false,
            ),
            Ok(None) => (Vec::new(), false),
            Err(e) => {
                eprintln!(
                    "warning: alert latch unreadable -- treating as active for ack gating: {e}"
                );
                (Vec::new(), true)
            }
        };
        let smartd_active = alert::smartd_alert_active(paths);
        let scrub_failed_active = alert::scrub_failed_active(paths);
        let cleanup_pending = alert::alert_cleanup_pending(paths);

        // The one alert-signal test. The cleanup-pending sentinel is left out
        // on purpose: alone, it means `CleanupOnly`.
        let has_alert = !causes.is_empty() || smartd_active || scrub_failed_active || latch_corrupt;
        if !has_alert {
            return if cleanup_pending {
                Self::CleanupOnly
            } else {
                Self::Quiet
            };
        }

        // ADR 014's two-arm rule: remove a flag when the snapshot saw it, or
        // when the latch carries its cause (crash recovery, where the flag was
        // already gone at entry).
        let cleanup = CleanupPlan {
            smartd: smartd_active || causes.iter().any(|c| matches!(c, AlertCause::SmartdAlert)),
            scrub_failed: scrub_failed_active
                || causes.iter().any(|c| matches!(c, AlertCause::ScrubFailed)),
        };
        Self::Alert(PendingAck { causes, cleanup })
    }
}

/// Ack's single baseline path, shared by mounted and offline ack: load
/// acked-stats (quarantining an unparseable file), apply the latched evidence,
/// and save when anything changed. A corrupt latch arrives as no causes and
/// commits nothing.
fn commit_acked_baseline(causes: &[AlertCause], paths: &StatePaths) -> Result<(), AckError> {
    let (mut acked, quarantined) = load_acked_stats_or_quarantine(paths).map_err(AckError::Io)?;
    if let Some(detail) = &quarantined {
        eprintln!(
            "warning: acked-stats.json unreadable -- {detail}; ack starts from an empty \
             baseline, so previously acknowledged errors may alert again"
        );
    }
    let before = acked.clone();
    for unbacked in commit_latched_evidence(&mut acked, causes) {
        match unbacked {
            UnbackedCause::DeviceErrors(devid) => eprintln!(
                "warning: no latched error counts for devid {devid} -- its baseline is unchanged, \
                 so the alert fires again if the errors persist"
            ),
            UnbackedCause::MissingDevice(devid) => eprintln!(
                "warning: devid {devid} is no longer known to be missing -- ack does not mark it \
                 acknowledged, so it alerts again if it is missing"
            ),
        }
    }
    if quarantined.is_some() || acked != before {
        save_acked_stats(&acked, paths).map_err(AckError::Io)?;
    }
    Ok(())
}

/// Ack confirmation builder so the user-facing count remains strictly tied to
/// latched causes, not synthesized status-only alert signals.
fn format_ack_confirmation(latched_count: usize) -> String {
    if latched_count == 0 {
        ACK_NO_COUNT_LINE.to_owned()
    } else {
        format!(
            "acknowledged {latched_count} alert{}",
            if latched_count == 1 { "" } else { "s" }
        )
    }
}

/// Cleanup of all alert-side files, used by every ack that reaches cleanup in
/// `ack_alert_records`: mounted, offline, and the cleanup-only retry.
///
/// Each `remove_*` call is NotFound-tolerant, so a missing file is not an
/// error. A real I/O error on any `remove_*` short-circuits the remaining
/// removals via `?` and propagates the error. Cleanup does not stop the alert
/// units: `cmd_ack_impl` does that after cleanup returns, whether it succeeded
/// or failed.
///
/// `plan` is either the `CleanupPlan` of an `AckEntry::Alert` snapshot or the
/// empty plan for `CleanupOnly` recovery, so cleanup deletes a flag only when
/// the entry snapshot already covered its source (`AckEntry::read` owns that
/// rule). A flag that arrives after a snapshot that did not cover it is left
/// for the next monitor cycle.
///
/// Cleanup marks `alert-cleanup-pending` before any destructive removal. The
/// removals then run in smartd-flag, scrub-failed-flag, latch, corrupt-sidecar
/// order so ADR 014's forensic sidecar is the last destructive step. The marker
/// is cleared only after every removal succeeds. If marker creation itself
/// fails, no destructive removal has run and the original entry signals still
/// drive retry; if a later step fails, the marker remains to drive the
/// cleanup-only retry branch in `ack_alert_records`.
fn cleanup_alert_files(paths: &StatePaths, plan: &CleanupPlan) -> Result<(), std::io::Error> {
    alert::mark_alert_cleanup_pending(paths)?;
    if plan.smartd {
        alert::remove_smartd_alert_flag(paths)?;
    }
    if plan.scrub_failed {
        alert::remove_scrub_failed_flag(paths)?;
    }
    alert::remove_alert_latch(paths)?;
    alert::remove_alert_latch_corrupt(paths)?;
    alert::clear_alert_cleanup_pending(paths)?;
    Ok(())
}

/// Snooze ENOSPC reminders from one fresh `btrfs device usage --raw` probe: when
/// the probe still sees the pool at risk, write a snooze marker (live `PoolKey`
/// plus a reminder deadline one `ENOSPC_REMINDER_INTERVAL` past `now`).
///
/// Writes no marker when the fresh probe is NOT at risk: the pool recovered
/// between fire and ack, and a snooze stamped on a not-at-risk pool would wrongly
/// suppress a recurrence inside the window (the dead-band monitor branch keeps the
/// marker). No marker -> a later recurrence fires armed.
///
/// Best-effort by contract. A usage-probe failure, a parse failure, or an absent
/// FSID (no usable `PoolKey`) logs and writes no marker -- the same end-state
/// as an offline ack (one quiet re-fire next cycle at the non-beeping Warning
/// level, then a mounted ack snoozes it). It never fails the ack: the latch is
/// already cleared by the time this runs.
fn write_enospc_baseline<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    pool: &AlertPoolState,
    missing_count: u64,
    paths: &StatePaths,
    now: SystemTime,
) {
    let raw = match runner.run(&CmdRequest::BtrfsDeviceUsageRaw {
        mount_point: mount_point.clone(),
    }) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!(
                "warning: could not probe usage to baseline ENOSPC risk -- {e}; ack cleared the alert but wrote no baseline"
            );
            return;
        }
    };
    let entries = match parse_btrfs_device_usage(&raw) {
        Ok(parsed) => parsed.devices,
        Err(e) => {
            eprintln!(
                "warning: could not parse usage to baseline ENOSPC risk -- {e}; ack cleared the alert but wrote no baseline"
            );
            return;
        }
    };
    // ADR 014: never baseline a snapshot that carries a zero-sized device. btrfs renders a
    // missing member as `<missing disk>` with device size 0, and a present device whose size
    // probe failed also reports 0; either makes the baseline untrustworthy, so the bail-out keys
    // on size, not on the missing marker.
    if entries.iter().any(|entry| entry.device_size == 0) {
        eprintln!(
            "warning: usage reports a missing device; ack cleared the alert but wrote no ENOSPC baseline"
        );
        return;
    }
    let Some(pool_key) = live_pool_key(pool.fsid.as_ref(), &entries) else {
        eprintln!(
            "warning: no FS UUID to key the ENOSPC baseline; ack cleared the alert but wrote no baseline"
        );
        return;
    };
    let assessment = evaluate_enospc_risk(&entries, missing_count);
    // Snooze only a live risk. If the pool recovered (dead-band or past re-arm) by
    // ack time, write no marker: a snooze stamped on a not-at-risk pool would
    // wrongly suppress a recurrence inside the window, since the dead-band monitor
    // branch keeps the marker. No marker -> a later recurrence fires armed.
    if !assessment.at_risk() {
        return;
    }
    if let Err(e) = save_enospc_ack(&EnospcAck::snooze(pool_key, now), paths) {
        eprintln!(
            "warning: failed to persist ENOSPC snooze marker -- {e}; ack cleared the alert but wrote no marker"
        );
    }
}

/// Shells out directly rather than via `OnlineStateOps::systemctl_stop`
/// because the beeper stop also runs after offline acks and the cleanup-only
/// retry, which issue no `CommandRunner` requests.
///
/// Stops both alert units: the Critical beeper (`braid-alert.service`) and the
/// non-beeping Warning advisory (`braid-alert-advisory.service`), so one ack
/// silences whichever tier the last monitor cycle started. Best-effort: a
/// failed spawn or a non-zero exit logs a warning and never fails the ack.
fn stop_beeper() {
    stop_unit("braid-alert.service");
    stop_unit("braid-alert-advisory.service");
}

fn stop_unit(unit: &str) {
    let result = stop_beeper_program("systemctl", &["stop", unit]);
    match result {
        Err(e) => {
            eprintln!("warning: could not stop {unit}: {e}");
        }
        Ok(output) => {
            if let Some(msg) = format_systemctl_stop_failure(unit, &output) {
                eprintln!("{msg}");
            }
        }
    }
}

fn stop_beeper_program(program: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    apply_child_env(&mut command);
    command.output()
}

fn format_systemctl_stop_failure(unit: &str, output: &std::process::Output) -> Option<String> {
    if output.status.success() {
        return None;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    Some(format!(
        "warning: systemctl stop {unit}: {}{}",
        output.status,
        detail_suffix(stderr)
    ))
}

#[derive(Debug, thiserror::Error)]
pub enum AckError {
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    /// Pre-cleanup state-load/save I/O failure. `#[from]` is deliberately
    /// omitted so `AckError`-returning code must use `map_err(AckError::Io)`
    /// or wrap as `CleanupFailed`; a new `?` propagating an `io::Error` into
    /// `AckError` cannot silently bypass the partial-state recovery message.
    #[error("I/O error: {0}")]
    Io(#[source] std::io::Error),
    /// Cleanup of latch + smartd-alert + corrupt-latch files failed after ack
    /// had already started persisting state: after the latched evidence was
    /// committed to acked-stats, or after one cleanup file was already
    /// removed. Not a refusal: ack still stops the alert units after the
    /// failed removal. Retry still has a signal:
    /// if `mark_alert_cleanup_pending` failed, no destructive removal has run
    /// and the original entry signals drive the regular ack path; if it
    /// succeeded and a later step failed, the `alert-cleanup-pending` sentinel
    /// drives the hoisted cleanup-only retry before probing or committing a
    /// baseline.
    /// Re-running `braid ack` after fixing the I/O issue is idempotent because
    /// every cleanup removal is NotFound-tolerant.
    #[error(
        "alert state cleanup failed -- some files may be in a partial state; \
         fix the I/O error and re-run `braid ack`: {0}"
    )]
    CleanupFailed(#[source] std::io::Error),
}

impl AckError {
    /// Whether ack stopped before it acknowledged anything: it committed no
    /// baseline and removed no alert file. A refusal leaves the alert units
    /// running, so the alarm still marks the alert ack did not handle; every
    /// other outcome, a cleanup failure included, silences them.
    fn is_refusal(&self) -> bool {
        match self {
            AckError::CleanupFailed(_) => false,
            AckError::Probe(_) | AckError::Io(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{
        AckedDisk, AckedStats, DeviceErrorCounters, ENOSPC_REMINDER_INTERVAL, load_acked_stats,
        load_acked_stats_fallible, load_enospc_ack,
    };
    use crate::monitor::{MonitorResult, cmd_monitor};
    use crate::test_fixtures::{
        ACK_DEVICE_SIZE, ACK_FSID, AckPanicFilesystem, AckPanicRunner, BTRFS_SHOW_2DISK_1MISSING,
        MonitorOverride, MonitorTestRunner, ack_fs_btrfs, ack_fs_ext4, ack_fs_not_mounted,
        ack_mounted_fs_that_touches_smartd, ack_mounted_probe_runner,
        ack_mounted_probe_runner_no_uuid_with_enospc_usage,
        ack_mounted_probe_runner_with_enospc_usage,
        ack_mounted_probe_runner_with_healthy_enospc_usage,
        ack_mounted_probe_runner_with_missing_enospc_usage,
        ack_mounted_probe_runner_with_zero_size_real_path_enospc_usage, ack_mp, ack_noop_beeper,
        ack_offline_fs_that_touches_scrub_failed, ack_offline_fs_that_touches_smartd,
        ack_write_latch, isolated_paths, monitor_fs_btrfs, monitor_mp,
    };
    use crate::types::Devid;
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(unix)]
    use std::process::{ExitStatus, Output};
    use std::time::UNIX_EPOCH;

    /// The latched causes stripped of their `detected_at` stamp, for cause-set
    /// assertions on a `MonitorResult::Alert` latch in ack integration tests.
    fn latched_causes(state: &alert::AlertState) -> Vec<AlertCause> {
        state.causes.iter().map(|c| c.cause.clone()).collect()
    }

    /// A latched device-error cause carrying the counts the monitor saw.
    fn device_errors(devid: u64, read: u64, corruption: u64) -> AlertCause {
        AlertCause::BtrfsDeviceErrors {
            devid: Devid::new(devid),
            counters: Some(DeviceErrorCounters {
                read_io_errs: read,
                corruption_errs: corruption,
                ..Default::default()
            }),
        }
    }

    /// Asserts ack issued no `btrfs device stats` query. Ack commits only the
    /// latch's evidence, so it has no reason to read live counters.
    fn assert_no_stats_query(runner: &crate::cmd::MockRunner) {
        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceStatsJson { .. })),
            "ack must not query btrfs device stats"
        );
    }

    /// Stats payload for the monitor 2-disk pool: devid 1 at `read` read
    /// errors and 1 corruption error, devid 2 clean.
    fn stats_devid1_errors(read: u64) -> String {
        format!(
            r#"{{
    "__header": {{"version": "1"}},
    "device-stats": [
        {{"device": "/dev/mapper/braid-vdb", "devid": 1, "write_io_errs": 0, "read_io_errs": {read}, "flush_io_errs": 0, "corruption_errs": 1, "generation_errs": 0}},
        {{"device": "/dev/mapper/braid-vdc", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}}
    ]
}}"#
        )
    }

    // Intent: The ack beeper-stop spawn path gives systemctl only braid's
    // explicit child-environment allowlist.
    // Why it exists: stop_beeper bypasses RealRunner; without a boundary test
    // it could silently regress to inheriting the whole CLI environment or
    // locale-dependent stderr.
    // Scenario: Ack cleanup reaches the systemctl spawn seam, but the test
    // substitutes the real env binary and inspects the environment it received.
    #[test]
    fn stop_beeper_program_child_env_is_allowlisted() {
        let output = stop_beeper_program("env", &[]).expect("env should run through PATH");
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("env output should be UTF-8");

        crate::cmd::assert_exact_child_env_dump(&stdout);
    }

    // Intent: The mounted ack confirmation formatter preserves the no-count
    //   fallback and the singular/plural counted forms.
    // Why it exists: Ack counts only latched causes; regressions that print
    //   "acknowledged 0 alerts" or drop pluralization would make the CLI
    //   contradict the documented confirmation contract.
    // Scenario: A mounted operator ack covers zero, one, or several latched
    //   causes while synthesized smartd and cleanup-pending signals remain
    //   outside the reported count.
    #[test]
    fn format_ack_confirmation_pins_count_and_pluralization() {
        assert_eq!(format_ack_confirmation(0), "acknowledged current alerts");
        assert_eq!(format_ack_confirmation(1), "acknowledged 1 alert");
        assert_eq!(format_ack_confirmation(2), "acknowledged 2 alerts");
        assert_eq!(format_ack_confirmation(3), "acknowledged 3 alerts");
    }

    /*
     * Intent: Mounted ack with no latched alert, no smartd flag, and no
     * corrupt latch is a true no-op: it does not query btrfs device stats
     * or rewrite acked-stats.json.
     * Why it exists: A proactive ack on a healthy pool used to snapshot
     * current btrfs counters as the new baseline, which could bury errors
     * that appeared after the last monitor cycle but before ack.
     * Scenario: the pool is mounted and healthy; the user runs `braid ack`
     * "for good measure" before monitor has latched any alert.
     */
    #[test]
    fn cmd_ack_noop_when_no_alerts_does_not_query_btrfs_or_write_acked_stats() {
        let (_dir, paths) = isolated_paths();
        let runner = ack_mounted_probe_runner();

        let result = cmd_ack_impl(
            &runner,
            &ack_fs_btrfs(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );

        assert!(result.is_ok(), "no-op ack should succeed, got {result:?}");
        assert_no_stats_query(&runner);
        assert!(!paths.acked_stats_json().exists());
    }

    // Intent: Offline ack with no alert signal is the same no-op as mounted
    //   ack: it returns Ok, stops the alert units once like every
    //   non-refused ack, and creates no state file.
    // Why it exists: Offline ack used to refuse this case with exit 1
    //   (`pool is not mounted -- nothing to acknowledge`) while the mounted
    //   no-op exited 0, though the docs and clap help promised exit 0 for
    //   nothing to ack in both cases.
    // Scenario: The pool is locked, nothing is latched or flagged, and a script
    //   runs `braid ack` anyway.
    #[test]
    fn ack_offline_noop_when_no_alerts_creates_no_state() {
        let (dir, paths) = isolated_paths();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let result = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &beeper,
        );

        assert!(
            result.is_ok(),
            "offline no-op ack should succeed, got {result:?}"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "no-op ack still stops the alert units"
        );
        let created: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(
            created.is_empty(),
            "no-op ack must create no state files, got {created:?}"
        );
    }

    /*
     * Intent: Mounted ack still runs the full ack path when
     * alert-latch.json is corrupt, even though the parsed latch count is
     * zero. It commits no baseline: the corrupt bytes carry no evidence.
     * Why it exists: If `AckEntry::read` checked only latched causes and
     * flag files, it would classify corrupt latch bytes as Quiet and ack
     * would return early, leaving the corrupt file uncleared and violating
     * the corrupt-latch recovery contract. Ack used to save a live-counter
     * snapshot here, which buried any error the monitor had not latched.
     * Scenario: external tampering or filesystem damage leaves a corrupt
     * latch while the pool is mounted; `braid ack` must clear it.
     */
    #[test]
    fn cmd_ack_with_mounted_pool_and_corrupt_latch_runs_full_ack_path() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.alert_latch_json(), b"not json").unwrap();
        let runner = ack_mounted_probe_runner();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper);

        assert!(
            result.is_ok(),
            "corrupt-latch ack should succeed, got {result:?}"
        );
        assert_no_stats_query(&runner);
        assert!(
            !paths.alert_latch_json().exists(),
            "corrupt latch must be removed"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "a corrupt latch carries no evidence, so ack commits no baseline"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire once on mounted corrupt-latch ack"
        );
    }

    // Intent: the documented ack contract -- acking BtrfsDeviceErrors commits
    //   the error counts the monitor latched as the new baseline so the same
    //   condition won't re-trigger -- holds across the real cmd_monitor ->
    //   cmd_ack -> cmd_monitor wiring: monitor latches BtrfsDeviceErrors with
    //   its counts, ack commits them as the baseline keyed by devid, the next
    //   monitor pass at the same counters stays Ok, and counters above the
    //   baseline re-fire.
    // Why it exists: latch -> commit -> persist -> load -> compare -> key was
    //   only covered as isolated halves (commit_latched_evidence writes
    //   counters; compute_alert_state suppresses below a hand-built baseline).
    //   A regression where the monitor latches no counts, ack persists a
    //   wrong/empty baseline, or keys it differently than monitor reads
    //   (devid.to_string()) would re-fire the same disk-error alert forever,
    //   and no test would catch it. The MissingDevice round-trip
    //   (monitor-lifecycle.py) leaves this counter-baseline path unexercised
    //   end-to-end.
    // Scenario: devid 1 reports read/corruption errors on a mounted, recognized
    //   2-disk pool; monitor latches BtrfsDeviceErrors{devid:1} with read 3,
    //   corruption 1. The operator runs braid ack, which commits those counts.
    //   The next monitor cycle at the same counts must stay silent; a later
    //   cycle with higher counts on devid 1 must alert again -- the baseline is
    //   a floor, not a permanent mute.
    #[test]
    fn ack_baseline_suppresses_then_refires_btrfs_device_errors() {
        let (_dir, paths) = isolated_paths();
        let fs = monitor_fs_btrfs();
        let mp = monitor_mp();
        let runner = MonitorTestRunner::with_stats_payload(stats_devid1_errors(3));

        // Phase 1: monitor latches BtrfsDeviceErrors{devid:1} with its counts.
        let first = cmd_monitor(&runner, &fs, &mp, &paths);
        match first {
            MonitorResult::Alert(s) => assert_eq!(
                latched_causes(&s),
                vec![device_errors(1, 3, 1)],
                "monitor must latch exactly the devid-1 btrfs error and its counts"
            ),
            other => panic!("expected Alert, got {other:?}"),
        }
        assert!(
            paths.alert_latch_json().exists(),
            "phase 1 must write the latch"
        );

        // Phase 2: ack commits the latched counts as the baseline, keyed by
        // devid, and removes the latch. The value + key assertions directly
        // witness the named regressions, so a break pinpoints the layer rather
        // than only flipping phase 3 red.
        cmd_ack_impl(&runner, &fs, &mp, &paths, &ack_noop_beeper).expect("ack ok");
        let acked = load_acked_stats(&paths);
        let d1 = acked
            .0
            .get("1")
            .expect("latched devid 1 baseline persisted");
        assert_eq!(
            d1.device_stats.read_io_errs, 3,
            "right baseline + right key"
        );
        assert_eq!(d1.device_stats.corruption_errs, 1);
        assert!(
            !acked.0.contains_key("2"),
            "ack writes only the devids the latch names"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "ack must remove the latch"
        );

        // Phase 3: monitor with the SAME counters must NOT re-fire.
        let second = cmd_monitor(&runner, &fs, &mp, &paths);
        assert_eq!(
            second,
            MonitorResult::Ok,
            "counters at the acked baseline must stay suppressed"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "no latch on the suppressed cycle"
        );

        // Phase 4 (re-fire): counters above the baseline alert again -- the
        // baseline is a floor, not a permanent mute for the devid.
        let runner_higher = MonitorTestRunner::with_stats_payload(stats_devid1_errors(5));
        let third = cmd_monitor(&runner_higher, &fs, &mp, &paths);
        match third {
            MonitorResult::Alert(s) => assert!(
                latched_causes(&s).contains(&device_errors(1, 5, 1)),
                "errors above the baseline must re-fire, got {:?}",
                s.causes
            ),
            other => panic!("expected re-fire Alert, got {other:?}"),
        }
    }

    // Intent: ack stops the alert units even when no alert record reached disk:
    //   the mounted and offline `no active alerts` no-ops each invoke the stop
    //   hook once, and both still return Ok.
    // Why it exists: ack used to stop the units only after it found a record
    //   (latch, flag, corrupt latch, or cleanup-pending marker). cmd_monitor
    //   still returns Alert when the latch save fails, so the wrapper starts
    //   braid-alert.service with nothing on disk. Ack then printed `no active
    //   alerts`, and the beeper kept sounding.
    // Scenario: the latch write fails (here a directory sits at its
    //   atomic-write temp path). Monitor sees errors on devid 1 and alerts with
    //   no latch on disk. The operator runs `braid ack` on the mounted pool,
    //   then again after locking it.
    #[test]
    fn cmd_ack_stops_alert_units_when_monitor_alerted_without_a_latch() {
        let (_dir, paths) = isolated_paths();
        std::fs::create_dir(
            paths
                .alert_latch_json()
                .with_file_name(".alert-latch.json.tmp"),
        )
        .unwrap();
        let fs = monitor_fs_btrfs();
        let mp = monitor_mp();
        let runner = MonitorTestRunner::with_stats_payload(stats_devid1_errors(3));

        match cmd_monitor(&runner, &fs, &mp, &paths) {
            MonitorResult::Alert(s) => assert_eq!(
                latched_causes(&s),
                vec![device_errors(1, 3, 1)],
                "monitor must alert on the devid-1 btrfs error"
            ),
            other => panic!("expected Alert, got {other:?}"),
        }
        assert!(
            !paths.alert_latch_json().exists(),
            "the latch save must have failed"
        );

        let mounted_calls = std::cell::Cell::new(0u32);
        let mounted_beeper = || mounted_calls.set(mounted_calls.get() + 1);
        let mounted = cmd_ack_impl(&runner, &fs, &mp, &paths, &mounted_beeper);
        assert!(
            mounted.is_ok(),
            "mounted no-record ack stays a no-op, got {mounted:?}"
        );
        assert_eq!(
            mounted_calls.get(),
            1,
            "mounted no-record ack must stop the alert units"
        );

        let offline_calls = std::cell::Cell::new(0u32);
        let offline_beeper = || offline_calls.set(offline_calls.get() + 1);
        let offline = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &offline_beeper,
        );
        assert!(
            offline.is_ok(),
            "offline no-record ack stays a no-op, got {offline:?}"
        );
        assert_eq!(
            offline_calls.get(),
            1,
            "offline no-record ack must stop the alert units"
        );
    }

    // Intent: ack stops the alert units once, as its last step. When the stop
    //   hook runs, the snapshotted flags, the latch, and the cleanup-pending
    //   marker are already gone, and a flag written right after the stop
    //   survives the ack.
    // Why it exists: ack used to stop the units before its removals. A smartd
    //   or scrub hook that fired in between restarted the unit and then lost
    //   its flag to ack's removal, so the beeper sounded with no record and
    //   `braid status` showed nothing.
    // Scenario: monitor latched SmartdAlert and ScrubFailed, and both flags are
    //   on disk. The operator runs `braid ack` on the mounted pool, and smartd
    //   fires again just after ack stops the units.
    #[test]
    fn cmd_ack_stops_alert_units_after_removing_snapshotted_files() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::SmartdAlert, AlertCause::ScrubFailed],
        );
        std::fs::write(paths.smartd_alert(), b"").unwrap();
        std::fs::write(paths.scrub_failed(), b"").unwrap();
        let runner = ack_mounted_probe_runner();
        let files_at_stop = std::cell::RefCell::new(Vec::new());
        let beeper = || {
            let left: Vec<_> = [
                paths.smartd_alert(),
                paths.scrub_failed(),
                paths.alert_latch_json(),
                paths.alert_cleanup_pending(),
            ]
            .into_iter()
            .filter(|p| p.exists())
            .collect();
            files_at_stop.borrow_mut().push(left);
            // smartd fires right after the stop.
            std::fs::write(paths.smartd_alert(), b"").unwrap();
        };

        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper);

        assert!(result.is_ok(), "ack must succeed, got {result:?}");
        assert_eq!(
            files_at_stop.into_inner(),
            vec![Vec::<std::path::PathBuf>::new()],
            "the stop must run once, after every snapshotted file is gone"
        );
        assert!(
            paths.smartd_alert().exists(),
            "a flag written after the stop must survive for the next monitor cycle"
        );
    }

    // Intent: acking an unrelated latched cause leaves device errors the
    //   monitor never latched for the next monitor cycle, which raises them.
    // Why it exists: mounted ack used to save a whole-pool snapshot of live
    //   counters as the acked baseline, whatever the latch held. Acking a quiet
    //   ENOSPC warning then buried a Critical device error that appeared after
    //   the last monitor cycle, and the next cycle stayed silent.
    // Scenario: the monitor latched EnospcRisk. devid 1 then logs read and
    //   corruption errors while no monitor cycle runs (a `braid replace` holds
    //   the pool lock for hours). The operator acks the ENOSPC warning; the next
    //   monitor cycle must still alert on devid 1.
    #[test]
    fn ack_of_unrelated_latch_does_not_bury_unlatched_device_errors() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::EnospcRisk {
                margin: -5,
                count_below: 1,
                device_count: 2,
            }],
        );
        let fs = monitor_fs_btrfs();
        let mp = monitor_mp();
        let runner = MonitorTestRunner::with_stats_payload(stats_devid1_errors(3));

        cmd_ack_impl(&runner, &fs, &mp, &paths, &ack_noop_beeper).expect("ack ok");
        let after_ack = cmd_monitor(&runner, &fs, &mp, &paths);

        match after_ack {
            MonitorResult::Alert(s) => assert!(
                latched_causes(&s).iter().any(|c| matches!(
                    c,
                    AlertCause::BtrfsDeviceErrors { devid, .. } if *devid == Devid::new(1)
                )),
                "errors the monitor never latched must still alert after ack, got {:?}",
                s.causes
            ),
            other => panic!("expected Alert for unlatched devid-1 errors, got {other:?}"),
        }
    }

    // Intent: acking an unrelated latched cause leaves a missing device the
    //   monitor never latched for the next monitor cycle, which raises it.
    // Why it exists: mounted ack used to mark every currently missing devid
    //   `missing_acked`, whatever the latch held, so acking a SMART warning
    //   buried a Critical missing-device alert that appeared after the last
    //   monitor cycle.
    // Scenario: the monitor latched SmartdAlert. devid 3 then drops out of the
    //   pool before the next monitor cycle. The operator acks the SMART
    //   warning; the next monitor cycle must still alert on devid 3.
    #[test]
    fn ack_of_unrelated_latch_does_not_bury_unlatched_missing_device() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::SmartdAlert]);
        std::fs::write(paths.smartd_alert(), b"").unwrap();
        let fs = monitor_fs_btrfs();
        let mp = monitor_mp();
        let degraded = || {
            MonitorTestRunner::with_override(MonitorOverride::BtrfsShowPayload(
                BTRFS_SHOW_2DISK_1MISSING.to_owned(),
            ))
        };

        cmd_ack_impl(&degraded(), &fs, &mp, &paths, &ack_noop_beeper).expect("ack ok");
        let after_ack = cmd_monitor(&degraded(), &fs, &mp, &paths);

        match after_ack {
            MonitorResult::Alert(s) => assert!(
                latched_causes(&s).contains(&AlertCause::MissingDevice {
                    devid: Devid::new(3),
                    still_missing: true
                }),
                "a missing device the monitor never latched must still alert after ack, got {:?}",
                s.causes
            ),
            other => panic!("expected Alert for unlatched missing devid 3, got {other:?}"),
        }
    }

    // Intent: ack commits the counts the monitor latched, not the live counts,
    //   so errors that grew after the last monitor cycle alert again.
    // Why it exists: the latched counts are what the operator saw in the alert
    //   and in `braid status`. Committing the live counts would bury every
    //   error that arrived between that cycle and the ack.
    // Scenario: the monitor latched devid 1 at 3 read errors. Two more read
    //   errors arrive before the operator acks. The next monitor cycle must
    //   alert on devid 1 at 5.
    #[test]
    fn ack_commits_latched_counts_so_later_growth_refires() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![device_errors(1, 3, 1)]);
        let fs = monitor_fs_btrfs();
        let mp = monitor_mp();
        let grown = MonitorTestRunner::with_stats_payload(stats_devid1_errors(5));

        cmd_ack_impl(&grown, &fs, &mp, &paths, &ack_noop_beeper).expect("ack ok");

        let acked = load_acked_stats(&paths);
        assert_eq!(
            acked.0.get("1").map(|d| d.device_stats),
            Some(DeviceErrorCounters {
                read_io_errs: 3,
                corruption_errs: 1,
                ..Default::default()
            }),
            "ack must commit the latched counts, not the live ones"
        );
        match cmd_monitor(&grown, &fs, &mp, &paths) {
            MonitorResult::Alert(s) => assert_eq!(
                latched_causes(&s),
                vec![device_errors(1, 5, 1)],
                "growth past the latched counts must alert again"
            ),
            other => panic!("expected Alert for grown devid-1 errors, got {other:?}"),
        }
    }

    // Intent: a device-error cause latched by an older braid, which carries no
    //   counts, commits no baseline; the alert fires again on the next cycle.
    // Why it exists: ack reads no live counters, so a cause without evidence
    //   has nothing safe to commit. Leaving the baseline unchanged fails loud:
    //   one more alert, and the monitor latches the counts for the next ack.
    // Scenario: the host upgraded braid while a device-error alert was latched.
    //   The operator acks; the next monitor cycle alerts on devid 1 again, now
    //   with its counts.
    #[test]
    fn ack_of_device_errors_without_counts_leaves_baseline_and_refires() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1),
                counters: None,
            }],
        );
        let fs = monitor_fs_btrfs();
        let mp = monitor_mp();
        let runner = MonitorTestRunner::with_stats_payload(stats_devid1_errors(3));

        cmd_ack_impl(&runner, &fs, &mp, &paths, &ack_noop_beeper).expect("ack ok");

        assert!(
            !paths.acked_stats_json().exists(),
            "a cause without counts must not write a baseline"
        );
        assert!(!paths.alert_latch_json().exists(), "ack clears the latch");
        match cmd_monitor(&runner, &fs, &mp, &paths) {
            MonitorResult::Alert(s) => assert_eq!(
                latched_causes(&s),
                vec![device_errors(1, 3, 1)],
                "the unbaselined errors must alert again, now with counts"
            ),
            other => panic!("expected Alert for devid-1 errors, got {other:?}"),
        }
    }

    /// The monitor's degraded 2-disk pool: devid 3 btrfs-MISSING.
    fn devid3_missing() -> MonitorTestRunner {
        MonitorTestRunner::with_override(MonitorOverride::BtrfsShowPayload(
            BTRFS_SHOW_2DISK_1MISSING.to_owned(),
        ))
    }

    /// The monitor's 2-disk pool with devid 2 btrfs-MISSING and devid 1
    /// present, so a test can watch the same devid leave and come back.
    fn devid2_missing() -> MonitorTestRunner {
        MonitorTestRunner::with_override(MonitorOverride::BtrfsShowPayload(
            "Label: none  uuid: de2b8517-f972-45fc-b121-3e160c8ea432\n\
             \tTotal devices 2 FS bytes used 16.17MiB\n\
             \tdevid    1 size 1008.00MiB used 209.50MiB path /dev/mapper/braid-vdb\n\
             \tdevid    2 size 0 used 0 path MISSING\n"
                .to_owned(),
        ))
    }

    fn missing_device(devid: u64) -> AlertCause {
        AlertCause::MissingDevice {
            devid: Devid::new(devid),
            still_missing: true,
        }
    }

    // Intent: after a membership command hands a latched missing devid to a
    //   new disk, ack does not mark that devid missing_acked, so the new disk
    //   dropping out alerts.
    // Why it exists: ack used to set missing_acked for every latched
    //   MissingDevice. `braid replace --missing-id` keeps the old disk's cause
    //   latched until ack, so the ack marked the new disk acknowledged-missing
    //   while it was present. If the new disk then dropped before the next
    //   monitor cycle, reconcile (which clears missing_acked only for present
    //   devids) never cleared it, and the degraded pool stayed silent.
    // Scenario: the monitor latched devid 3 missing. The operator replaces it,
    //   which hands devid 3 to a new disk, and acks. The new disk drops out
    //   (a loose cable) before the next monitor cycle, which must alert.
    #[test]
    fn ack_after_devid_handover_does_not_mark_new_disk_missing_acked() {
        let (_dir, paths) = isolated_paths();
        let fs = monitor_fs_btrfs();
        let mp = monitor_mp();
        match cmd_monitor(&devid3_missing(), &fs, &mp, &paths) {
            MonitorResult::Alert(s) => assert_eq!(latched_causes(&s), vec![missing_device(3)]),
            other => panic!("expected Alert for missing devid 3, got {other:?}"),
        }

        alert::drop_ghost_alert_state_for_devids(&paths, &[Devid::new(3)])
            .expect("the replace handover strips devid 3's evidence");
        cmd_ack_impl(&devid3_missing(), &fs, &mp, &paths, &ack_noop_beeper).expect("ack ok");

        assert!(
            !load_acked_stats(&paths)
                .0
                .get("3")
                .is_some_and(|d| d.missing_acked),
            "ack must not mark the new disk on devid 3 missing_acked"
        );
        match cmd_monitor(&devid3_missing(), &fs, &mp, &paths) {
            MonitorResult::Alert(s) => assert_eq!(
                latched_causes(&s),
                vec![missing_device(3)],
                "the new disk dropping out must alert"
            ),
            other => panic!("expected Alert for the new disk on devid 3, got {other:?}"),
        }
    }

    // Intent: once a monitor cycle sees a latched missing devid back in the
    //   pool, ack does not mark it missing_acked, so its next absence alerts.
    // Why it exists: a flapping disk can return and drop again. If ack marked
    //   it missing_acked while present and it dropped before the next cycle,
    //   reconcile would never clear the flag and the absence would stay
    //   silent. A cycle that sees it missing again restores the evidence, so
    //   acking a disk that is still gone keeps working.
    // Scenario: devid 2 drops out (latched), comes back (a healthy cycle), and
    //   the operator acks. Devid 2 then drops out again.
    #[test]
    fn ack_after_missing_devid_returned_does_not_mark_it_missing_acked() {
        let (_dir, paths) = isolated_paths();
        let fs = monitor_fs_btrfs();
        let mp = monitor_mp();
        let healthy = MonitorTestRunner::with_stale_mapper_stats;
        cmd_monitor(&devid2_missing(), &fs, &mp, &paths);
        match cmd_monitor(&healthy(), &fs, &mp, &paths) {
            MonitorResult::Alert(s) => assert_eq!(
                latched_causes(&s),
                vec![AlertCause::MissingDevice {
                    devid: Devid::new(2),
                    still_missing: false,
                }],
                "the cause stays latched with its evidence withdrawn"
            ),
            other => panic!("expected the latched missing devid 2, got {other:?}"),
        }

        cmd_ack_impl(&healthy(), &fs, &mp, &paths, &ack_noop_beeper).expect("ack ok");

        assert!(
            !load_acked_stats(&paths)
                .0
                .get("2")
                .is_some_and(|d| d.missing_acked),
            "ack must not mark a returned devid missing_acked"
        );
        match cmd_monitor(&devid2_missing(), &fs, &mp, &paths) {
            MonitorResult::Alert(s) => assert_eq!(
                latched_causes(&s),
                vec![missing_device(2)],
                "the next absence must alert"
            ),
            other => panic!("expected Alert for devid 2 missing again, got {other:?}"),
        }
    }

    /*
     * Intent: Mounted ack still runs the full ack path when only the
     * smartd alert flag exists and monitor has not latched it yet.
     * Why it exists: The smartd hook can fire between monitor cycles. A
     * guard that ignores the flag would return early, leaving the flag in
     * place and failing to silence the alert source.
     * Scenario: smartd writes /var/lib/braid/smartd-alert; before monitor
     * runs, the user runs `braid ack` on a mounted pool.
     */
    #[test]
    fn cmd_ack_with_mounted_pool_and_smartd_flag_no_latch_runs_full_ack_path() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.smartd_alert(), b"").unwrap();
        let runner = ack_mounted_probe_runner();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper);

        assert!(
            result.is_ok(),
            "smartd-only ack should succeed, got {result:?}"
        );
        assert_no_stats_query(&runner);
        assert!(
            !paths.smartd_alert().exists(),
            "smartd flag must be removed"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "a smartd flag carries no device evidence, so ack commits no baseline"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire once on mounted smartd-only ack"
        );
    }

    /*
     * Intent: Offline ack with a bare smartd-alert flag present at entry and
     * no latch clears the flag and exits Ok.
     * Why it exists: `AckEntry::read` must classify a bare smartd flag as
     * `Alert`. Every other offline smartd test either carries a SmartdAlert
     * *cause* in the latch (alert by `causes`) or has the flag arrive
     * mid-probe (flag kept), so a regression dropping the smartd flag from the
     * has-alert test would slip through: ack would take the `no active alerts`
     * no-op and leave the flag. This pins that term directly.
     * Scenario: smartd wrote /var/lib/braid/smartd-alert; before monitor
     * latched it, the user locked the pool and runs `braid ack`.
     */
    #[test]
    fn ack_offline_smartd_flag_no_latch_clears_flag() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.smartd_alert(), b"").unwrap();

        let result = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );

        assert!(
            result.is_ok(),
            "offline smartd-flag ack must succeed, got {result:?}"
        );
        assert!(
            !paths.smartd_alert().exists(),
            "smartd flag must be removed"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "smartd-only offline ack must not write acked-stats"
        );
    }

    // Intent: Offline ack does not let a smartd flag written during probing
    // turn an empty entry snapshot into an acknowledged alert.
    // Why it exists: The smartd hook is not under the pool lock, so it can
    // fire while probe_pool_alerts is reading mountinfo. A post-probe gate read
    // would consume that new flag and hide it from the next monitor cycle.
    // Scenario: pool is offline and there are no alerts at ack entry, but
    // smartd writes the flag while ack is probing the mount point.
    #[test]
    fn ack_offline_does_not_consume_smartd_flag_arriving_during_probe() {
        let (_dir, paths) = isolated_paths();
        let fs = ack_offline_fs_that_touches_smartd(&paths);

        let result = cmd_ack_impl(&AckPanicRunner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

        assert!(
            result.is_ok(),
            "empty offline snapshot must take the no-op, got {result:?}"
        );
        assert!(
            paths.smartd_alert().exists(),
            "late smartd flag must remain for the next monitor cycle"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "ack must not create a latch"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "empty offline ack must not create acked-stats"
        );
    }

    // Intent: Mounted ack with only the scrub-failed flag (no latch) runs the
    //   full ack path -- removes the flag, commits no baseline (the flag
    //   carries no device evidence), and fires the beeper-stop hook once.
    // Why it exists: braid-scrub-failed.service can fire between monitor cycles.
    //   `AckEntry::read` must classify a bare scrub-failed flag as `Alert`; a
    //   regression dropping that term would no-op this ack and leave the flag +
    //   beeper. Mirrors the smartd-flag-no-latch mounted test.
    // Scenario: onFailure wrote /var/lib/braid/scrub-failed; before monitor
    //   latched it, the operator runs `braid ack` on a mounted pool.
    #[test]
    fn cmd_ack_with_mounted_pool_and_scrub_failed_flag_no_latch_runs_full_ack_path() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.scrub_failed(), b"").unwrap();
        let runner = ack_mounted_probe_runner();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper);

        assert!(
            result.is_ok(),
            "scrub-failed-only ack should succeed, got {result:?}"
        );
        assert_no_stats_query(&runner);
        assert!(
            !paths.scrub_failed().exists(),
            "scrub-failed flag must be removed"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "a scrub-failed flag carries no device evidence, so ack commits no baseline"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire once on mounted scrub-failed-only ack"
        );
    }

    // Intent: Offline ack of a bare scrub-failed flag (no latch) clears the flag
    //   and exits Ok, and writes no acked-stats.
    // Why it exists: `AckEntry::read` must classify a bare scrub-failed flag as
    //   `Alert`; a regression dropping that term would take the
    //   `no active alerts` no-op and leave the flag. Mirrors the smartd
    //   bare-flag offline test, and pins that a bare flag commits no baseline
    //   (no acked-stats).
    // Scenario: onFailure wrote scrub-failed; before monitor latched it, the
    //   operator locked the pool and runs `braid ack`.
    #[test]
    fn ack_offline_scrub_failed_flag_no_latch_clears_flag() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.scrub_failed(), b"").unwrap();

        let result = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );

        assert!(
            result.is_ok(),
            "offline scrub-failed-flag ack must succeed, got {result:?}"
        );
        assert!(
            !paths.scrub_failed().exists(),
            "scrub-failed flag must be removed"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "scrub-failed-only offline ack must not write acked-stats"
        );
    }

    // Intent: Offline ack does not let a scrub-failed flag written during probing
    //   turn an empty entry snapshot into an acknowledged alert.
    // Why it exists: onFailure is not under the pool lock, so it can fire while
    //   probe_pool_alerts reads mountinfo. `AckEntry::read` reads the flag at
    //   entry (before the probe); a post-probe read would consume the late flag
    //   and hide it from the next monitor cycle. Mirrors the smartd snapshot-race.
    // Scenario: pool is offline with no alerts at ack entry, but onFailure writes
    //   the flag while ack is probing the mount point.
    #[test]
    fn ack_offline_does_not_consume_scrub_failed_flag_arriving_during_probe() {
        let (_dir, paths) = isolated_paths();
        let fs = ack_offline_fs_that_touches_scrub_failed(&paths);

        let result = cmd_ack_impl(&AckPanicRunner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

        assert!(
            result.is_ok(),
            "empty offline snapshot must take the no-op, got {result:?}"
        );
        assert!(
            paths.scrub_failed().exists(),
            "late scrub-failed flag must remain for the next monitor cycle"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "ack must not create a latch"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "empty offline ack must not create acked-stats"
        );
    }

    // Intent: Offline cleanup removes a scrub-failed flag written during probing
    //   when the entry snapshot already had a latched ScrubFailed cause.
    // Why it exists: the crash-recovery arm treats a latched ScrubFailed as an
    //   active source even if the flag was absent at entry, so
    //   `CleanupPlan::scrub_failed` must also be set by the latched cause, not
    //   by the entry flag alone. Mirrors the smartd second-arm exception.
    // Scenario: a prior monitor cycle latched ScrubFailed, the flag is absent at
    //   ack entry, and onFailure writes it again during the offline probe.
    #[test]
    fn ack_offline_with_scrub_failed_latch_cleans_mid_probe_flag() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::ScrubFailed]);
        let fs = ack_offline_fs_that_touches_scrub_failed(&paths);

        let result = cmd_ack_impl(&AckPanicRunner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

        assert!(
            result.is_ok(),
            "offline scrub-failed-latch ack should succeed, got {result:?}"
        );
        assert!(!paths.alert_latch_json().exists(), "latch must be removed");
        assert!(
            !paths.scrub_failed().exists(),
            "latched ScrubFailed cleanup must remove the mid-probe flag"
        );
    }

    // Intent: Mounted no-op ack does not let a smartd flag written during
    // probing turn an empty entry snapshot into a full ack path.
    // Why it exists: Reading smartd after probe_pool_alerts would make the
    // no-alert gate observe the late flag, run the ack, and then delete the
    // flag before monitor could latch it.
    // Scenario: pool is mounted and healthy; there are no alerts at ack
    // entry, but smartd writes the flag while ack is probing the pool.
    #[test]
    fn cmd_ack_mounted_does_not_consume_smartd_flag_arriving_during_probe() {
        let (_dir, paths) = isolated_paths();
        let fs = ack_mounted_fs_that_touches_smartd(&paths);
        let runner = ack_mounted_probe_runner();

        let result = cmd_ack_impl(&runner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

        assert!(
            result.is_ok(),
            "no-op mounted ack should succeed, got {result:?}"
        );
        assert!(
            paths.smartd_alert().exists(),
            "late smartd flag must remain for the next monitor cycle"
        );
        assert_no_stats_query(&runner);
        assert!(
            !paths.acked_stats_json().exists(),
            "empty entry snapshot must not write acked-stats"
        );
    }

    // Intent: Mounted cleanup preserves a smartd flag written during probing
    // when the entry snapshot only had a non-smartd latched cause.
    // Why it exists: Cleanup used to remove smartd-alert unconditionally,
    // which swallowed a late smartd alert even though this ack was only
    // acknowledging a btrfs device error.
    // Scenario: monitor latched BtrfsDeviceErrors, smartd has not fired at
    // ack entry, and then smartd writes the flag during the mounted probe.
    #[test]
    fn cmd_ack_mounted_with_btrfs_errors_preserves_mid_probe_smartd_flag() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![device_errors(1, 3, 0)]);
        let fs = ack_mounted_fs_that_touches_smartd(&paths);
        let runner = ack_mounted_probe_runner();

        let result = cmd_ack_impl(&runner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

        assert!(
            result.is_ok(),
            "btrfs-error ack should succeed, got {result:?}"
        );
        assert!(!paths.alert_latch_json().exists(), "latch must be removed");
        assert!(
            paths.acked_stats_json().exists(),
            "mounted ack must commit the latched evidence"
        );
        assert!(
            paths.smartd_alert().exists(),
            "late smartd flag must remain for the next monitor cycle"
        );
    }

    // Intent: Offline cleanup preserves a smartd flag written during probing
    // when the entry snapshot only had a non-smartd latched cause.
    // Why it exists: Offline ack skips the probe-dependent steps of the
    // mounted path; it must still honor the same snapshot-scoped smartd rule.
    // Scenario: monitor latched MissingDevice, smartd has not fired at ack
    // entry, and then smartd writes the flag while ack confirms the pool is
    // offline.
    #[test]
    fn ack_offline_with_missing_device_preserves_mid_probe_smartd_flag() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(2),
                still_missing: true,
            }],
        );
        let fs = ack_offline_fs_that_touches_smartd(&paths);

        let result = cmd_ack_impl(&AckPanicRunner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

        assert!(
            result.is_ok(),
            "offline missing-device ack should succeed, got {result:?}"
        );
        assert!(!paths.alert_latch_json().exists(), "latch must be removed");
        let acked = load_acked_stats(&paths);
        let entry = acked.0.get("2").expect("devid 2 entry must be present");
        assert!(entry.missing_acked);
        assert!(
            paths.smartd_alert().exists(),
            "late smartd flag must remain for the next monitor cycle"
        );
    }

    // Intent: Offline cleanup removes a smartd flag written during probing
    // when the entry snapshot already had a latched SmartdAlert.
    // Why it exists: The crash-recovery exception treats a latched
    // SmartdAlert as an active smartd source even if the flag was absent at
    // entry, so `CleanupPlan::smartd` must not regress to the entry flag alone.
    // Scenario: a prior monitor cycle latched SmartdAlert, the flag is absent
    // at ack entry, and the smartd hook writes it again during the offline
    // probe.
    #[test]
    fn ack_offline_with_smartd_latch_cleans_mid_probe_smartd_flag() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::SmartdAlert]);
        let fs = ack_offline_fs_that_touches_smartd(&paths);

        let result = cmd_ack_impl(&AckPanicRunner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

        assert!(
            result.is_ok(),
            "offline smartd-latch ack should succeed, got {result:?}"
        );
        assert!(!paths.alert_latch_json().exists(), "latch must be removed");
        assert!(
            !paths.smartd_alert().exists(),
            "latched SmartdAlert cleanup must remove the mid-probe flag"
        );
    }

    // Intent: Mounted cleanup removes a smartd flag written during probing
    // when the entry snapshot already had a latched SmartdAlert.
    // Why it exists: The mounted branch must apply the shared entry-snapshotted
    // `CleanupPlan` instead of regressing to removing the smartd flag only when
    // it was present at entry.
    // Scenario: a prior monitor cycle latched SmartdAlert, the flag is absent
    // at ack entry, and the smartd hook writes it again during the mounted
    // probe.
    #[test]
    fn cmd_ack_mounted_with_smartd_latch_cleans_mid_probe_smartd_flag() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::SmartdAlert]);
        let fs = ack_mounted_fs_that_touches_smartd(&paths);
        let runner = ack_mounted_probe_runner();

        let result = cmd_ack_impl(&runner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

        assert!(
            result.is_ok(),
            "mounted smartd-latch ack should succeed, got {result:?}"
        );
        assert!(!paths.alert_latch_json().exists(), "latch must be removed");
        assert!(
            !paths.acked_stats_json().exists(),
            "a SmartdAlert latch carries no device evidence, so ack commits no baseline"
        );
        assert!(
            !paths.smartd_alert().exists(),
            "latched SmartdAlert cleanup must remove the mid-probe flag"
        );
    }

    // Intent: Mounted ack with a parseable latch whose only cause is
    // ComputationError runs the full ack path -- the latch is removed, no
    // baseline is committed (the cause carries no device evidence), and the
    // beeper hook fires exactly once.
    // Why it exists: `AckEntry::read` classifies any non-empty `causes` as
    // `Alert`. A future refactor that narrowed that test to "actionable"
    // causes, or that special-cased SmartdAlert without doing the same for
    // ComputationError, would silently no-op this mounted ack and
    // leave the latch on disk with the beeper still running. The beeper-call
    // assertion additionally pins the cleanup hook on the
    // mounted success path, which was previously only covered for NotBtrfs and
    // offline success.
    // Scenario: monitor latched a ComputationError on a prior cycle, such as a
    // transient probe failure. The pool is now mounted and healthy; the
    // operator runs `braid ack`. The latch must be cleared and the beeper
    // silenced.
    #[test]
    fn cmd_ack_with_mounted_pool_and_computation_error_only_latch_runs_full_ack_path() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::ComputationError {
                detail: "test".to_owned(),
            }],
        );
        let runner = ack_mounted_probe_runner();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper);

        assert!(
            result.is_ok(),
            "computation-error-only ack should succeed, got {result:?}"
        );
        assert_no_stats_query(&runner);
        assert!(!paths.alert_latch_json().exists(), "latch must be removed");
        assert!(
            !paths.acked_stats_json().exists(),
            "a ComputationError latch carries no device evidence, so ack commits no baseline"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire once on mounted-ack success"
        );
    }

    /*
     * Intent: When mounted ack succeeds at save_acked_stats but the third
     * cleanup file removal fails, the user-visible error names the partial
     * state and points at the recovery path. The new baseline is durable,
     * the latch is removed, the corrupt sidecar remains, and the beeper-stop
     * hook still fires after the failed removal.
     * Why it exists: Without the dedicated variant, a cleanup-phase I/O
     * error surfaces as "I/O error: <kind>" with no hint that re-running ack
     * will eventually clear the latch. The cleanup-pending sentinel preserves
     * the retry signal after the latch is gone, and a failed removal must not
     * skip the stop.
     * Scenario: a directory sits at the corrupt-latch sidecar path (manual
     * tampering, leftover from a previous bug, or permission drift), so
     * remove_file fails with EISDIR/EPERM. The latch carried
     * BtrfsDeviceErrors with its counts. Mounted pool. cmd_ack must commit
     * the latched evidence, fail cleanup, invoke the beeper hook, and
     * return the dedicated variant.
     */
    #[test]
    fn cmd_ack_returns_cleanup_failed_when_corrupt_latch_cleanup_errors_after_baseline_saved() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![device_errors(1, 3, 0)]);
        // remove_file on a directory returns EISDIR (Linux) / EPERM (macOS)
        // -- a platform-portable non-NotFound io::Error from
        // remove_alert_latch_corrupt.
        std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

        let runner = ack_mounted_probe_runner();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);
        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper)
            .expect_err("cleanup failure must propagate");

        assert!(
            matches!(err, AckError::CleanupFailed(_)),
            "expected AckError::CleanupFailed, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("partial state") && msg.contains("re-run `braid ack`"),
            "message must name partial state and recovery path, got: {msg}"
        );

        // Witnesses for why CleanupFailed is distinct from AckError::Io.
        // Starting from acked-stats absent makes the partial apply visible:
        // the file appears before cleanup fails.
        assert!(
            paths.acked_stats_json().exists(),
            "save_acked_stats runs before cleanup -- baseline must be durable"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "cleanup must remove the latch before failing on the corrupt sidecar"
        );
        assert!(
            paths.alert_latch_corrupt().exists(),
            "cleanup poison directory must remain and prove where cleanup failed"
        );
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "cleanup-pending sentinel must remain so retry re-enters cleanup"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire even when a later cleanup remove_* fails"
        );
    }

    /*
     * Intent: Mounted ack still invokes the beeper-stop hook when the first
     * cleanup file removal fails.
     * Why it exists: The corrupt-sidecar cleanup failure tests exercise the
     * third removal. This test exercises the first removal so the union pins
     * that the stop runs after a failure at any removal step, not only after
     * a late one.
     * Scenario: monitor latched SmartdAlert, but the smartd flag path is a
     * poison directory. Mounted pool. cmd_ack must fail on the smartd flag
     * removal, leave later cleanup files untouched, and then invoke the
     * beeper hook.
     */
    #[test]
    fn cmd_ack_stops_beeper_after_mounted_smartd_flag_cleanup_error() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::SmartdAlert]);
        // smartd_alert_active ignores directories, but the latched
        // SmartdAlert still opts cleanup into removing the flag path.
        std::fs::create_dir(paths.smartd_alert()).unwrap();

        let runner = ack_mounted_probe_runner();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);
        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper)
            .expect_err("smartd cleanup failure must propagate");

        assert!(
            matches!(err, AckError::CleanupFailed(_)),
            "expected AckError::CleanupFailed, got: {err:?}"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "a SmartdAlert latch carries no device evidence, so ack commits no baseline"
        );
        assert!(
            paths.alert_latch_json().exists(),
            "latch must remain because cleanup failed on the first removal"
        );
        assert!(
            paths.smartd_alert().exists(),
            "cleanup poison directory must remain and prove where cleanup failed"
        );
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "cleanup-pending sentinel must remain after mark succeeds"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire after the first cleanup remove_* fails"
        );
    }

    // Intent: After cleanup_alert_files fails at remove_alert_latch_corrupt,
    //   the alert-cleanup-pending sentinel remains on disk, so the retry
    //   re-enters cleanup, re-invokes the stop hook, and completes cleanup.
    // Why it exists: the retry on a poisoned corrupt sidecar used to hit the
    //   mounted no-op gate because the latch JSON had already been removed, so
    //   cleanup never finished. The sentinel preserves the retry signal
    //   without moving the corrupt sidecar's destructive step, which would
    //   break ADR 014's forensic guarantee.
    // Scenario: monitor latched BtrfsDeviceErrors{devid:1}. A directory sits
    //   at alert-latch.json.corrupt. `braid ack` returns CleanupFailed.
    //   Operator removes the directory and re-runs `braid ack`.
    #[test]
    fn cmd_ack_mounted_retry_after_cleanup_failed_completes_recovery() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![device_errors(1, 3, 0)]);
        std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

        let runner = ack_mounted_probe_runner();
        let beeper_calls_first = std::cell::Cell::new(0u32);
        let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_first)
            .expect_err("first call must fail on the poisoned corrupt sidecar");
        assert!(matches!(err, AckError::CleanupFailed(_)));
        assert_eq!(
            beeper_calls_first.get(),
            1,
            "stop hook must be invoked on the failing first call"
        );
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "sentinel must remain on disk so retry re-enters cleanup"
        );
        assert!(paths.alert_latch_corrupt().exists(), "poison still wedged");

        std::fs::remove_dir(paths.alert_latch_corrupt()).unwrap();

        let beeper_calls_retry = std::cell::Cell::new(0u32);
        let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);

        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_retry);
        assert!(
            result.is_ok(),
            "retry must succeed after operator clears poison"
        );
        assert_eq!(
            beeper_calls_retry.get(),
            1,
            "retry must re-invoke the stop hook"
        );
        assert!(
            !paths.alert_cleanup_pending().exists(),
            "retry must clear the sentinel"
        );
        assert!(!paths.alert_latch_corrupt().exists());
    }

    // Intent: After offline ack fails at the corrupt-sidecar removal, the
    //   alert-cleanup-pending sentinel remains on disk and the latch has been
    //   removed. The retry takes the hoisted cleanup-only branch before
    //   probe_pool_alerts, re-invokes the stop hook, and completes cleanup.
    // Why it exists: without the hoisted branch, this retry would see no other
    //   signal and take the `no active alerts` no-op, leaving the sentinel and
    //   unfinished cleanup right after the user followed the CleanupFailed
    //   recovery instruction.
    // Scenario: pool offline, monitor latched MissingDevice{devid:1}. A
    //   directory sits at alert-latch.json.corrupt. Operator runs `braid ack`,
    //   gets CleanupFailed, removes the poison, and re-runs `braid ack`.
    #[test]
    fn ack_offline_retry_after_cleanup_failed_completes_recovery() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(1),
                still_missing: true,
            }],
        );
        std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

        let beeper_calls_first = std::cell::Cell::new(0u32);
        let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

        let err = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &beeper_first,
        )
        .expect_err("first call must fail");
        assert!(matches!(err, AckError::CleanupFailed(_)));
        assert_eq!(
            beeper_calls_first.get(),
            1,
            "stop hook must fire on the failing first call"
        );
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "sentinel preserved on cleanup failure"
        );
        assert!(paths.alert_latch_corrupt().exists());

        std::fs::remove_dir(paths.alert_latch_corrupt()).unwrap();

        let beeper_calls_retry = std::cell::Cell::new(0u32);
        let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);

        let result = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &beeper_retry,
        );
        assert!(
            result.is_ok(),
            "offline retry must succeed after operator clears poison"
        );
        assert_eq!(
            beeper_calls_retry.get(),
            1,
            "retry must re-invoke the stop hook"
        );
        assert!(
            !paths.alert_cleanup_pending().exists(),
            "retry must clear the sentinel"
        );
        assert!(!paths.alert_latch_corrupt().exists());
        let acked = load_acked_stats(&paths);
        assert!(acked.0.get("1").unwrap().missing_acked);
    }

    // Intent: When the entry alert signal is a smartd flag and no latch JSON
    //   exists, a cleanup failure at remove_alert_latch_corrupt leaves the
    //   sentinel on disk, so the retry re-enters cleanup even though smartd
    //   was already removed during the first attempt.
    // Why it exists: latch-backed retry tests still pass if a future refactor
    //   narrows the sentinel to latch-present cases. This pins the smartd-only
    //   path where neither a latch nor an active smartd flag survives the
    //   first call's cleanup.
    // Scenario: smartd hook fired but monitor has not run yet, so there is no
    //   latch JSON. A directory sits at alert-latch.json.corrupt. Operator
    //   clears that directory and re-runs `braid ack`.
    #[test]
    fn cmd_ack_mounted_smartd_only_retry_after_cleanup_failed_completes_recovery() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.smartd_alert(), b"").unwrap();
        std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

        let runner = ack_mounted_probe_runner();
        let beeper_calls_first = std::cell::Cell::new(0u32);
        let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_first)
            .expect_err("first call must fail on the poisoned corrupt sidecar");
        assert!(matches!(err, AckError::CleanupFailed(_)));
        assert_eq!(
            beeper_calls_first.get(),
            1,
            "stop hook must be invoked on the failing first call"
        );
        assert!(
            !paths.smartd_alert().exists(),
            "smartd flag was removed before the corrupt-sidecar step failed"
        );
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "sentinel preserved on cleanup failure"
        );
        assert!(paths.alert_latch_corrupt().exists(), "poison still wedged");

        std::fs::remove_dir(paths.alert_latch_corrupt()).unwrap();

        let beeper_calls_retry = std::cell::Cell::new(0u32);
        let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);

        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_retry);
        assert!(
            result.is_ok(),
            "retry must succeed after operator clears poison"
        );
        assert_eq!(
            beeper_calls_retry.get(),
            1,
            "retry must re-invoke the stop hook"
        );
        assert!(
            !paths.alert_cleanup_pending().exists(),
            "retry must clear the sentinel"
        );
        assert!(!paths.alert_latch_corrupt().exists());
    }

    // Intent: When the retry's only entry signal is the cleanup-pending
    //   sentinel, ack_alert_records takes the hoisted sentinel-only branch
    //   before probe_pool_alerts. The retry issues zero runner requests and
    //   does not rewrite acked-stats.json.
    // Why it exists: a sentinel-aware gate below probe_pool_alerts would
    //   re-wedge cleanup on probe failure. The retry is finishing the
    //   previous ack, not starting a new one.
    // Scenario: a mounted ack saves a baseline, then cleanup fails at the
    //   corrupt sidecar. Operator removes the directory; retry must complete
    //   cleanup without re-querying btrfs.
    #[test]
    fn cmd_ack_mounted_sentinel_only_retry_does_not_query_btrfs_or_rewrite_baseline() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![device_errors(1, 3, 0)]);
        std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

        let runner = ack_mounted_probe_runner();
        let beeper_calls_first = std::cell::Cell::new(0u32);
        let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_first)
            .expect_err("first call must fail on the poisoned corrupt sidecar");
        assert!(matches!(err, AckError::CleanupFailed(_)));
        let baseline_after_first = std::fs::read(paths.acked_stats_json()).unwrap();
        let requests_after_first = runner.requests().len();

        std::fs::remove_dir(paths.alert_latch_corrupt()).unwrap();

        let beeper_calls_retry = std::cell::Cell::new(0u32);
        let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);

        let result = cmd_ack_impl(
            &runner,
            &AckPanicFilesystem,
            &ack_mp(),
            &paths,
            &beeper_retry,
        );
        assert!(result.is_ok(), "retry must succeed without probing");

        let retry_requests: Vec<_> = runner
            .requests()
            .into_iter()
            .skip(requests_after_first)
            .collect();
        assert!(
            retry_requests.is_empty(),
            "sentinel-only retry must issue zero runner requests; got {retry_requests:?}"
        );
        let baseline_after_retry = std::fs::read(paths.acked_stats_json()).unwrap();
        assert_eq!(
            baseline_after_first, baseline_after_retry,
            "sentinel-only retry must not rewrite acked-stats.json"
        );
        assert!(
            !paths.alert_cleanup_pending().exists(),
            "retry must clear the sentinel"
        );
        assert!(!paths.alert_latch_corrupt().exists());
    }

    // Intent: When mark_alert_cleanup_pending itself fails, cleanup
    //   short-circuits before any destructive removal runs. CleanupFailed is
    //   returned, every entry alert signal is byte-identical to entry, and the
    //   retry observes the original entry snapshot to re-enter cleanup.
    // Why it exists: the cleanup-pending sentinel is itself a file ack writes,
    //   so it can be poisoned like other alert-state paths. Any destructive
    //   removal before marker creation would destroy alert state before retry
    //   had a chance to record cleanup-pending.
    // Scenario: a directory sits at alert-cleanup-pending. Latch carries
    //   BtrfsDeviceErrors{devid:1}. After the operator removes the poison
    //   directory, retry completes cleanly.
    #[test]
    fn cmd_ack_mounted_retry_after_poisoned_sentinel_completes_recovery() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![device_errors(1, 3, 0)]);
        std::fs::create_dir(paths.alert_cleanup_pending()).unwrap();
        let original_latch_bytes = std::fs::read(paths.alert_latch_json()).unwrap();

        let runner = ack_mounted_probe_runner();
        let beeper_calls_first = std::cell::Cell::new(0u32);
        let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_first)
            .expect_err("marker creation must fail on the poisoned sentinel path");
        assert!(matches!(err, AckError::CleanupFailed(_)));
        assert_eq!(
            beeper_calls_first.get(),
            1,
            "stop hook must fire even when marker creation fails"
        );
        assert_eq!(
            std::fs::read(paths.alert_latch_json()).unwrap(),
            original_latch_bytes,
            "latch JSON must be preserved because no destructive removal ran"
        );
        assert!(
            paths.alert_cleanup_pending().exists(),
            "poison sentinel directory still wedged"
        );
        assert!(
            !alert::alert_cleanup_pending(&paths),
            "directory-form sentinel must not be treated as cleanup-pending"
        );
        assert!(
            paths.acked_stats_json().exists(),
            "save_acked_stats ran before cleanup"
        );

        std::fs::remove_dir(paths.alert_cleanup_pending()).unwrap();

        let beeper_calls_retry = std::cell::Cell::new(0u32);
        let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);
        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_retry);
        assert!(
            result.is_ok(),
            "retry must succeed after operator clears the sentinel poison"
        );
        assert_eq!(
            beeper_calls_retry.get(),
            1,
            "retry must re-invoke the stop hook"
        );
        assert!(!paths.alert_cleanup_pending().exists());
        assert!(!paths.alert_latch_json().exists());
    }

    // Intent: ack does not quarantine a corrupt alert-latch.json. The
    //   no-quarantine invariant is the monitor/ack asymmetry: monitor moves
    //   corrupt bytes to alert-latch.json.corrupt because it rewrites the
    //   latch, while ack deletes the latch outright in cleanup.
    // Why it exists: a symmetry edit could swap load_alert_latch for
    //   load_alert_latch_or_quarantine in AckEntry::read. On the success
    //   path, the sidecar would be created by quarantine and then deleted by
    //   remove_alert_latch_corrupt during cleanup, so a sidecar-absence
    //   assertion at the end of a successful ack proves nothing. Forcing
    //   cleanup to fail at mark_alert_cleanup_pending stops execution before
    //   destructive removal, so any sidecar created by quarantine persists.
    // Scenario: corrupt latch on disk; the alert-cleanup-pending path is a
    //   directory. ack must return CleanupFailed, preserve the original corrupt
    //   latch bytes verbatim, and not create a .corrupt sidecar.
    #[test]
    fn cmd_ack_mounted_corrupt_latch_does_not_quarantine_when_cleanup_fails() {
        let (_dir, paths) = isolated_paths();
        let original_bytes: &[u8] = b"not json";
        std::fs::write(paths.alert_latch_json(), original_bytes).unwrap();
        std::fs::create_dir(paths.alert_cleanup_pending()).unwrap();

        let runner = ack_mounted_probe_runner();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper)
            .expect_err("marker creation must fail on the poisoned sentinel path");
        assert!(
            matches!(err, AckError::CleanupFailed(_)),
            "expected AckError::CleanupFailed, got {err:?}"
        );

        assert_eq!(
            std::fs::read(paths.alert_latch_json()).unwrap(),
            original_bytes,
            "corrupt latch bytes must remain untouched because no destructive removal ran"
        );
        assert!(
            !paths.alert_latch_corrupt().exists(),
            "ack must not quarantine -- monitor is the only path that creates the sidecar"
        );
    }

    // Intent: When ack cleanup fails before reaching remove_alert_latch_corrupt,
    //   the corrupt sidecar's bytes are unchanged. Pins ADR 014's forensic
    //   guarantee at the unit level for the cleanup-failure path.
    // Why it exists: the cleanup-pending sentinel design keeps
    //   alert-latch.json.corrupt as the last destructive step so its bytes
    //   survive partial cleanup.
    // Scenario: a prior monitor cycle quarantined a corrupt latch. The current
    //   cycle latched SmartdAlert, but the smartd flag path is a poison
    //   directory. ack fails at remove_smartd_alert_flag and must leave the
    //   corrupt sidecar untouched.
    #[test]
    fn cmd_ack_preserves_corrupt_sidecar_bytes_through_cleanup_failure() {
        let (_dir, paths) = isolated_paths();
        let forensic_bytes: &[u8] = b"first corruption forensic data";
        std::fs::write(paths.alert_latch_corrupt(), forensic_bytes).unwrap();
        ack_write_latch(&paths, vec![AlertCause::SmartdAlert]);
        std::fs::create_dir(paths.smartd_alert()).unwrap();

        let runner = ack_mounted_probe_runner();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper)
            .expect_err("smartd cleanup failure must propagate");
        assert!(matches!(err, AckError::CleanupFailed(_)));
        assert_eq!(beeper_calls.get(), 1);

        let preserved = std::fs::read(paths.alert_latch_corrupt()).unwrap();
        assert_eq!(
            preserved, forensic_bytes,
            "corrupt sidecar bytes must survive cleanup failure"
        );
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "sentinel must be on disk after mark succeeds"
        );
    }

    /*
     * Intent: cmd_ack must surface ProbeError::NotBtrfs to the caller and
     * leave latched alert state intact when an alert is already on disk.
     * Why it exists: Prior behavior silently deleted the latch + smartd flag,
     * mutated acked-stats for any latched MissingDevice cause, and printed
     * "acknowledged current alerts" for a state that is not actually offline.
     * Pins the regression guard for the with-alerts case.
     * Scenario: operator left an ext4 partition mounted at /mnt/storage. A
     * pre-existing alert latch and smartd flag are on disk. Running
     * `braid ack` must error out without touching alert-latch.json,
     * smartd-alert, or acked-stats.json.
     */
    #[test]
    fn cmd_ack_with_foreign_fstype_and_alerts_returns_probe_error_and_preserves_state() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(2),
                still_missing: true,
            }],
        );
        let original_latch_bytes = std::fs::read(paths.alert_latch_json()).unwrap();
        std::fs::write(paths.smartd_alert(), b"").unwrap();

        let err = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_ext4(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        )
        .expect_err("must refuse -- mount is not btrfs");

        match &err {
            AckError::Probe(ProbeError::NotBtrfs { fstype, .. }) => {
                assert_eq!(fstype.as_str(), "ext4");
            }
            other => panic!("expected AckError::Probe(NotBtrfs), got: {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("not btrfs") && msg.contains("ext4"),
            "user-visible message must name fstype, got: {msg}"
        );

        assert_eq!(
            std::fs::read(paths.alert_latch_json()).unwrap(),
            original_latch_bytes,
            "latch bytes must be preserved"
        );
        assert!(
            paths.smartd_alert().exists(),
            "smartd flag must be preserved"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "acked-stats must not be created from a NotBtrfs path"
        );
    }

    /*
     * Intent: With no pre-existing alerts, NotBtrfs must surface the real
     * condition rather than the `no active alerts` no-op.
     * Why it exists: The no-signal no-op runs after the probe on purpose. An
     * older version claimed "pool is not mounted" here, and a no-op hoisted
     * above the probe would exit 0 while a foreign filesystem shadows the
     * pool path. Pins the no-alert branch so it cannot regress independently
     * of the with-alerts branch.
     * Scenario: clean state directory, but the mount target holds ext4.
     * `braid ack` must report the fstype, not claim there is nothing to do,
     * and must not create any alert files.
     */
    #[test]
    fn cmd_ack_with_foreign_fstype_and_no_alerts_returns_probe_error() {
        let (_dir, paths) = isolated_paths();

        let err = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_ext4(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        )
        .expect_err("must refuse -- mount is not btrfs");

        match &err {
            AckError::Probe(ProbeError::NotBtrfs { fstype, .. }) => {
                assert_eq!(fstype.as_str(), "ext4");
            }
            other => panic!("expected AckError::Probe(NotBtrfs), got: {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("not btrfs") && msg.contains("ext4"),
            "user-visible message must name fstype, got: {msg}"
        );

        assert!(!paths.alert_latch_json().exists(), "no latch should appear");
        assert!(
            !paths.smartd_alert().exists(),
            "no smartd flag should appear"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "no acked-stats should appear"
        );
    }

    /*
     * Intent: A corrupt alert-latch.json plus a foreign fstype still surfaces
     * ProbeError::NotBtrfs and preserves the unreadable latch bytes.
     * Why it exists: cmd_ack reads the latch before probing the pool. The
     * corrupt latch must count as active for gating, but a non-btrfs mount
     * target is not a genuine offline pool, so ack must not clean up the
     * corrupt latch on this path.
     * Scenario: alert-latch.json contains garbage bytes, and an ext4
     * filesystem is mounted at /mnt/storage. `braid ack` must report the
     * fstype mismatch and leave the corrupt file available for later ack
     * after the mount is fixed.
     */
    #[test]
    fn cmd_ack_with_foreign_fstype_and_corrupt_latch_preserves_latch_bytes() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.alert_latch_json(), b"not json").unwrap();
        let original_latch_bytes = std::fs::read(paths.alert_latch_json()).unwrap();

        let err = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_ext4(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        )
        .expect_err("must refuse -- mount is not btrfs");

        assert!(
            matches!(err, AckError::Probe(ProbeError::NotBtrfs { .. })),
            "expected AckError::Probe(NotBtrfs), got: {err:?}"
        );
        assert_eq!(
            std::fs::read(paths.alert_latch_json()).unwrap(),
            original_latch_bytes,
            "corrupt latch bytes must be preserved on NotBtrfs"
        );
        assert!(
            !paths.alert_latch_corrupt().exists(),
            "NotBtrfs must not quarantine or clean up the latch"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "no acked-stats should appear"
        );
    }

    /*
     * Intent: The NotBtrfs error path must not invoke the beeper hook.
     * Why it exists: Prior behavior routed NotBtrfs through ack_offline,
     * whose success path stops the beeper. The public cmd_ack tests above
     * pin the user-visible error and state preservation; this hook-only test
     * pins the side-effect boundary.
     * Scenario: mount target holds ext4 and a latch exists. The
     * implementation returns Probe(NotBtrfs) before reaching cleanup.
     */
    #[test]
    fn cmd_ack_impl_with_foreign_fstype_does_not_invoke_beeper() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(2),
                still_missing: true,
            }],
        );
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let err = cmd_ack_impl(&AckPanicRunner, &ack_fs_ext4(), &ack_mp(), &paths, &beeper)
            .expect_err("must refuse -- mount is not btrfs");

        assert!(
            matches!(err, AckError::Probe(ProbeError::NotBtrfs { .. })),
            "expected AckError::Probe(NotBtrfs), got: {err:?}"
        );
        assert_eq!(
            beeper_calls.get(),
            0,
            "stop_beeper must not be called on NotBtrfs"
        );
    }

    /*
     * Intent: Offline ack of a latched MissingDevice cause writes
     * missing_acked=true for that devid into acked-stats.json and removes
     * the latch.
     *
     * Why it exists: This is the regression gate for the bug where offline
     * ack only deleted the latch and never updated acked-stats, so the next
     * monitor cycle re-fired the same MissingDevice cause against an
     * unchanged baseline. Without this assertion, the bug returns silently.
     *
     * Scenario: pool was degraded with devid 2 missing; monitor latched
     * MissingDevice{devid:2}; user locked the pool and ran `braid ack`.
     * The next mounted monitor cycle must see missing_acked=true and not
     * re-fire.
     */
    #[test]
    fn ack_offline_with_missing_device_cause_marks_missing_acked() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(2),
                still_missing: true,
            }],
        );
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &beeper,
        )
        .unwrap();
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire once on offline-ack success"
        );

        let acked = load_acked_stats(&paths);
        let entry = acked.0.get("2").expect("devid 2 entry must be present");
        assert!(entry.missing_acked);
        assert!(!paths.alert_latch_json().exists());
    }

    /*
     * Intent: Offline ack with a latched MissingDevice cause persists the
     * missing-device ack update to acked-stats.json before cleanup. When the
     * third cleanup file removal then fails, the user-visible error names the
     * partial state, the latch is removed, the corrupt sidecar remains, and
     * the beeper-stop hook still fires.
     * Why it exists: offline ack skips the probe-dependent steps, so the
     * mounted test alone would not catch an offline-only regression that
     * falls back to AckError::Io. The beeper assertion independently pins
     * that an offline CleanupFailed still stops the alert units. The
     * cleanup-pending sentinel is the retry signal after the latch is
     * already removed.
     * Scenario: pool offline, latch contains MissingDevice{devid:1}, and a
     * directory sits at the corrupt-latch sidecar path so remove_file fails
     * with EISDIR/EPERM.
     */
    #[test]
    fn ack_offline_cleanup_failure_after_missing_acked_returns_cleanup_failed() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(1),
                still_missing: true,
            }],
        );
        // remove_file on a directory returns EISDIR (Linux) / EPERM (macOS)
        // -- a platform-portable non-NotFound io::Error from
        // remove_alert_latch_corrupt.
        std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let err = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &beeper,
        )
        .expect_err("offline cleanup failure must propagate");

        assert!(
            matches!(err, AckError::CleanupFailed(_)),
            "expected AckError::CleanupFailed, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("partial state") && msg.contains("re-run `braid ack`"),
            "message must name partial state and recovery path, got: {msg}"
        );

        // Witnesses for the partial-apply state in the offline branch:
        // missing-device ack state was persisted (acked-stats exists), latch
        // was removed, and cleanup stopped at the corrupt sidecar.
        assert!(
            paths.acked_stats_json().exists(),
            "save_acked_stats runs before cleanup -- baseline must be durable"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "cleanup must remove the latch before failing on the corrupt sidecar"
        );
        assert!(
            paths.alert_latch_corrupt().exists(),
            "cleanup poison directory must remain and prove where cleanup failed"
        );
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "cleanup-pending sentinel must remain so offline retry can resume"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire even when a later cleanup remove_* fails"
        );
    }

    // Intent: Offline ack treats an unreadable alert latch as active even
    //   when no causes can be parsed from it.
    // Why it exists: ADR 014's recovery contract depends on `AckEntry::read`
    //   treating both Read and Parse latch load failures as an alert. If a
    //   future refactor narrows that test to Parse only, ack takes the
    //   `no active alerts` no-op before cleanup and leaves the user unable to
    //   clear the broken latch while the pool is locked.
    // Scenario: pool is offline, and filesystem damage or external tampering
    //   leaves a directory where alert-latch.json should be.
    #[test]
    fn ack_offline_read_error_latch_reaches_cleanup_failed() {
        let (_dir, paths) = isolated_paths();
        std::fs::create_dir(paths.alert_latch_json()).unwrap();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let err = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &beeper,
        )
        .expect_err("unreadable latch on an offline pool must reach cleanup");

        assert!(
            matches!(err, AckError::CleanupFailed(_)),
            "Read-error latch must gate as active and reach cleanup, got {err:?}"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire after the failed latch removal"
        );
        assert!(
            paths.alert_latch_json().exists(),
            "latch directory cannot be removed by remove_file"
        );
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "sentinel must be marked before the failed latch removal"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "an unreadable latch carries no evidence, so ack writes no acked-stats"
        );
    }

    /*
     * Intent: Offline ack of a latch holding both a device-error cause and a
     * missing-device cause commits both: the latched counts as devid 1's
     * baseline and missing_acked for devid 2.
     *
     * Why it exists: offline ack used to refuse any latch with a device-error
     * cause, because it had no live counters to snapshot. The latch now
     * carries the counts the monitor saw, so mounted and offline ack share one
     * baseline path and the operator can clear the alert with the pool locked.
     *
     * Scenario: pool had btrfs device errors AND a missing device. The user
     * locked the pool and ran `braid ack`.
     */
    #[test]
    fn ack_offline_commits_device_error_and_missing_device_evidence() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![
                device_errors(1, 3, 1),
                AlertCause::MissingDevice {
                    devid: Devid::new(2),
                    still_missing: true,
                },
            ],
        );

        let result = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );
        assert!(result.is_ok(), "offline ack must succeed, got {result:?}");

        let acked = load_acked_stats(&paths);
        let d1 = acked.0.get("1").expect("devid 1 baseline committed");
        assert!(!d1.missing_acked);
        assert_eq!(
            d1.device_stats,
            DeviceErrorCounters {
                read_io_errs: 3,
                corruption_errs: 1,
                ..Default::default()
            },
            "offline ack commits the latched counts"
        );
        assert!(
            acked
                .0
                .get("2")
                .expect("devid 2 entry committed")
                .missing_acked,
            "offline ack marks the latched missing devid"
        );
        assert!(!paths.alert_latch_json().exists(), "ack clears the latch");
    }

    /*
     * Intent: Applying a latched MissingDevice cause to an existing
     * acked-stats entry sets missing_acked=true while preserving the
     * existing device_stats baseline.
     *
     * Why it exists: Pins the offline ack insert-or-update behavior. A
     * regression that overwrote the entry with a default device_stats would
     * silently zero out a previously-acked counter baseline, making the
     * next online monitor cycle re-alert on the same counters.
     *
     * Scenario: a previous online ack baselined devid 1 at read_io_errs=7.
     * The disk later went missing; monitor latched MissingDevice{devid:1};
     * user offline-acked. The device_stats baseline must survive.
     */
    #[test]
    fn ack_offline_preserves_existing_device_stats_baseline() {
        let (_dir, paths) = isolated_paths();
        let mut map = BTreeMap::new();
        map.insert(
            "1".to_owned(),
            AckedDisk {
                missing_acked: false,
                device_stats: DeviceErrorCounters {
                    read_io_errs: 7,
                    ..Default::default()
                },
            },
        );
        save_acked_stats(&AckedStats(map), &paths).unwrap();

        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(1),
                still_missing: true,
            }],
        );

        cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        )
        .unwrap();

        let acked = load_acked_stats(&paths);
        let entry = acked.0.get("1").unwrap();
        assert!(entry.missing_acked);
        assert_eq!(
            entry.device_stats.read_io_errs, 7,
            "existing baseline must be preserved"
        );
    }

    /*
     * Intent: A corrupt alert-latch.json is still acked offline -- the
     * latch file is removed and ack returns Ok, so the user can recover
     * from a manually-tampered or filesystem-damaged latch even with the
     * pool locked.
     *
     * Why it exists: Regression gate for the corrupt-latch recovery path.
     * If a future edit gates offline ack on latch parseability, the
     * operator would have no way to clear a bad file when the pool is
     * offline. acked-stats must remain absent because no causes can be
     * extracted from the corrupt file.
     *
     * Scenario: alert-latch.json contains garbage bytes (manual edit,
     * filesystem damage). Pool is offline. `braid ack` must succeed.
     */
    #[test]
    fn ack_offline_corrupt_latch_still_clears_files() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.alert_latch_json(), b"not json").unwrap();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &beeper,
        )
        .unwrap();

        assert!(!paths.alert_latch_json().exists());
        assert!(!paths.alert_latch_corrupt().exists());
        assert!(
            !paths.acked_stats_json().exists(),
            "an unreadable latch carries no evidence, so ack writes no acked-stats"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire once on offline corrupt-latch ack"
        );
    }

    /*
     * Intent: A corrupt acked-stats.json is moved aside to
     * acked-stats.json.corrupt with its bytes intact, and ack commits the
     * latched evidence onto an empty baseline.
     *
     * Why it exists: the monitor latches ComputationError for a corrupt
     * acked-stats.json every cycle and never rewrites the file. Mounted ack
     * used to write a fresh snapshot over the file without reading it, and
     * that blind overwrite was the operator's way out. Ack now does a
     * read-modify-write, so without the quarantine no braid command would
     * clear the alert. Quarantine keeps the bytes for forensics; the empty
     * baseline fails loud, since previously acknowledged errors alert again.
     *
     * Scenario: acked-stats.json was hand-edited (or filesystem-corrupted)
     * to invalid JSON. Latch contains MissingDevice{devid:1}. User runs
     * `braid ack` offline.
     */
    #[test]
    fn ack_quarantines_corrupt_acked_stats_and_commits_latched_evidence() {
        let (_dir, paths) = isolated_paths();
        let original_bytes: &[u8] = b"not json";
        std::fs::write(paths.acked_stats_json(), original_bytes).unwrap();
        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(1),
                still_missing: true,
            }],
        );

        let result = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );
        assert!(result.is_ok(), "ack must succeed, got {result:?}");

        assert_eq!(
            std::fs::read(paths.acked_stats_corrupt()).unwrap(),
            original_bytes,
            "the corrupt bytes must survive in the sidecar"
        );
        let acked = load_acked_stats_fallible(&paths).expect("acked-stats must parse again");
        assert_eq!(
            acked.0.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["1"],
            "ack starts from an empty baseline and adds only the latched evidence"
        );
        assert!(acked.0["1"].missing_acked);
        assert!(!paths.alert_latch_json().exists(), "ack clears the latch");
    }

    /*
     * Intent: Acking the ComputationError that a corrupt acked-stats.json
     * raises restores monitoring: the next monitor cycle reads the repaired
     * file and stays quiet on a healthy pool.
     *
     * Why it exists: the monitor latches ComputationError for an unreadable
     * acked-stats.json every cycle and never rewrites the file. If ack left
     * the file alone, the alert would come back after every ack.
     *
     * Scenario: acked-stats.json is corrupt on a healthy mounted pool. The
     * monitor raises ComputationError; the operator acks; the next monitor
     * cycle must return Ok.
     */
    #[test]
    fn ack_of_corrupt_acked_stats_alert_restores_monitoring() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.acked_stats_json(), b"not json").unwrap();
        let fs = monitor_fs_btrfs();
        let mp = monitor_mp();

        let first = cmd_monitor(
            &MonitorTestRunner::with_stale_mapper_stats(),
            &fs,
            &mp,
            &paths,
        );
        match first {
            MonitorResult::Alert(s) => assert!(
                latched_causes(&s).iter().any(|c| matches!(
                    c,
                    AlertCause::ComputationError { detail } if detail.contains("acked-stats")
                )),
                "a corrupt acked-stats must raise ComputationError, got {:?}",
                s.causes
            ),
            other => panic!("expected ComputationError alert, got {other:?}"),
        }

        cmd_ack_impl(
            &MonitorTestRunner::with_stale_mapper_stats(),
            &fs,
            &mp,
            &paths,
            &ack_noop_beeper,
        )
        .expect("ack ok");

        assert_eq!(
            cmd_monitor(
                &MonitorTestRunner::with_stale_mapper_stats(),
                &fs,
                &mp,
                &paths
            ),
            MonitorResult::Ok,
            "the cycle after ack must read the repaired acked-stats"
        );
    }

    /*
     * Intent: Non-zero `systemctl stop braid-alert.service` exits produce a
     * stderr warning that includes both the process status and systemctl's
     * own diagnostic.
     * Why it exists: `Command::output()` returns Ok(Output) for non-zero
     * exits. Without explicitly checking status, `braid ack` can silently
     * leave the beeper running after a cleanup failure.
     * Scenario: braid-alert.service is not loaded in the VM, so systemctl
     * exits 5 and explains that the unit could not be stopped.
     */
    #[cfg(unix)]
    #[test]
    fn format_systemctl_stop_failure_warns_on_nonzero_exit_with_stderr() {
        let output = Output {
            status: ExitStatus::from_raw(5 << 8),
            stdout: Vec::new(),
            stderr: b"Failed to stop braid-alert.service: Unit not loaded.\n".to_vec(),
        };

        let msg = format_systemctl_stop_failure("braid-alert.service", &output)
            .expect("non-zero systemctl exit must produce a warning");

        assert!(
            msg.contains("exit status: 5"),
            "warning must include status display, got: {msg}"
        );
        assert!(
            msg.contains("Failed to stop braid-alert.service: Unit not loaded."),
            "warning must include trimmed stderr, got: {msg}"
        );
    }

    // Intent: A non-zero `systemctl stop braid-alert.service` exit with empty
    //   stderr still warns, rendering the process status with no trailing
    //   diagnostic suffix.
    // Why it exists: The empty-stderr arm formats a distinct message
    //   ("...{status}" with no ": {stderr}" tail). A swapped empty/non-empty
    //   arm, a dropped status, or a lost prefix would ship a malformed or
    //   useless beeper-stop warning; only an empty-stderr input exercises that
    //   arm.
    // Scenario: systemctl exits non-zero but prints nothing to stderr (the
    //   stop is rejected with only an exit code), so braid must still surface
    //   the status.
    #[cfg(unix)]
    #[test]
    fn format_systemctl_stop_failure_warns_on_nonzero_exit_without_stderr() {
        let output = Output {
            status: ExitStatus::from_raw(5 << 8),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };

        assert_eq!(
            format_systemctl_stop_failure("braid-alert.service", &output),
            Some("warning: systemctl stop braid-alert.service: exit status: 5".to_string()),
        );
    }

    /*
     * Intent: Successful `systemctl stop braid-alert.service` exits do not
     * produce a warning.
     * Why it exists: The cleanup is best-effort; a healthy stop should keep
     * `braid ack` output focused on the ack confirmation.
     * Scenario: braid-alert.service exists and systemd accepts the stop
     * request with exit status 0.
     */
    #[cfg(unix)]
    #[test]
    fn format_systemctl_stop_failure_silent_on_zero_exit() {
        let output = Output {
            status: ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };

        assert_eq!(
            format_systemctl_stop_failure("braid-alert.service", &output),
            None
        );
    }

    // --- ENOSPC baseline ack (Step 5) ---

    // Intent: a mounted ack of an EnospcRisk latch whose fresh probe is still at
    //   risk writes enospc-ack.json keyed on the live (devid, device_size) pairs,
    //   with snoozed_until one reminder interval past ack time; it clears the
    //   latch, leaves the marker in place, and fires the cleanup hook once.
    // Why it exists: ack snoozes the reminder (it does not resolve), and the
    //   monitor's suppression relies on a fresh, correctly-keyed deadline. The
    //   re-probe (live key from the ack-time usage probe, not the latched fire-time
    //   cause) and the persist-past-ack are that contract.
    // Scenario: monitor latched EnospcRisk on a mounted 2-disk pool; the operator
    //   runs braid ack while the pool is still at risk.
    #[test]
    fn cmd_ack_mounted_enospc_risk_writes_reprobed_keyed_baseline() {
        let (_dir, paths) = isolated_paths();
        // A fire-time cause in the latch; the snooze marker must be keyed off the
        // fresh ack-time probe, not this latched value.
        ack_write_latch(
            &paths,
            vec![AlertCause::EnospcRisk {
                margin: -5,
                count_below: 1,
                device_count: 2,
            }],
        );
        let runner = ack_mounted_probe_runner_with_enospc_usage();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper);
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(result.is_ok(), "ack must succeed, got {result:?}");

        let ack = load_enospc_ack(&paths)
            .unwrap()
            .expect("mounted ack of a still-at-risk pool must write a snooze marker");
        assert_eq!(
            ack.pool_key.fsid.as_str(),
            ACK_FSID,
            "keyed on the live FS UUID"
        );
        assert_eq!(
            ack.pool_key.devices,
            vec![
                (Devid::new(1), ACK_DEVICE_SIZE),
                (Devid::new(3), ACK_DEVICE_SIZE),
            ],
            "pool_key carries (devid, device_size) from the ack-time usage probe"
        );
        let interval = ENOSPC_REMINDER_INTERVAL.as_secs();
        assert!(
            (before + interval..=after + interval).contains(&ack.snoozed_until),
            "snoozed_until {} must be one reminder interval past ack time [{}, {}]",
            ack.snoozed_until,
            before + interval,
            after + interval
        );
        assert!(!paths.alert_latch_json().exists(), "ack clears the latch");
        assert!(
            paths.enospc_ack_json().exists(),
            "the marker persists past ack (post-ack snooze memory)"
        );
        assert_eq!(beeper_calls.get(), 1, "cleanup hook fires once");
    }

    // Intent: a mounted ack of an EnospcRisk latch whose fresh probe is HEALTHY
    //   (dead-band, 0 <= margin < REARM) clears the latch but writes no snooze
    //   marker.
    // Why it exists (F1): the at-risk gate. Acking a recovered pool must not stamp
    //   a snooze onto a not-at-risk pool -- the dead-band monitor branch keeps a
    //   marker, so a snooze written here would wrongly suppress a recurrence inside
    //   the window. No marker -> a later recurrence fires armed (pairs with
    //   cmd_monitor_at_risk_no_marker_fires_armed).
    // Scenario: monitor latched EnospcRisk, the pool recovered into the dead-band
    //   between fire and ack, and the operator runs braid ack.
    #[test]
    fn cmd_ack_mounted_enospc_healthy_at_ack_writes_no_snooze() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::EnospcRisk {
                margin: -5,
                count_below: 1,
                device_count: 2,
            }],
        );
        let runner = ack_mounted_probe_runner_with_healthy_enospc_usage();

        let result = cmd_ack_impl(
            &runner,
            &ack_fs_btrfs(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );
        assert!(result.is_ok(), "ack must succeed, got {result:?}");
        assert!(!paths.alert_latch_json().exists(), "ack clears the latch");
        assert!(
            !paths.enospc_ack_json().exists(),
            "a not-at-risk ack-time probe writes no snooze marker"
        );
    }

    // Intent: a mounted ack whose fresh usage snapshot contains btrfs's missing
    //   marker clears the latch but writes no snooze marker.
    // Why it exists: a show-vs-usage skew can make `missing_count == 0` while the
    //   usage-derived key already contains `(devid, 0)`. Persisting that key would
    //   let a later skewed monitor cycle suppress a real ENOSPC risk.
    // Scenario: monitor latched EnospcRisk, `btrfs filesystem show` still reports
    //   both devices present, but the ack-time usage probe reports one missing
    //   device.
    #[test]
    fn cmd_ack_mounted_enospc_missing_usage_writes_no_snooze() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::EnospcRisk {
                margin: -5,
                count_below: 1,
                device_count: 2,
            }],
        );
        let runner = ack_mounted_probe_runner_with_missing_enospc_usage();

        let result = cmd_ack_impl(
            &runner,
            &ack_fs_btrfs(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );

        assert!(result.is_ok(), "ack must succeed, got {result:?}");
        assert!(!paths.alert_latch_json().exists(), "ack clears the latch");
        assert!(
            load_enospc_ack(&paths).unwrap().is_none(),
            "a usage-missing snapshot writes no snooze marker"
        );
    }

    // Intent: a mounted ack whose fresh usage snapshot carries a zero-sized device with a
    //   real path -- not the `<missing disk>` marker -- clears the latch but writes no snooze
    //   marker.
    // Why it exists: ADR 014's invariant is "a zero-sized device never appears in a baseline
    //   that suppresses an alert." btrfs-progs reports `Device size: 0` for a present device
    //   whose `device_get_partition_size` probe failed, rendering it with its real path; the
    //   guard keys on size, not on the missing marker, so a future narrowing to
    //   `has_missing_marker()` would let this real-path zero-sized row be baselined. Pins that.
    // Scenario: monitor latched EnospcRisk; the ack-time usage probe reports devid 3 with its
    //   real `/dev/mapper/braid-disk3` path but device size 0 (a transient size-probe failure).
    #[test]
    fn cmd_ack_mounted_enospc_zero_size_real_path_usage_writes_no_snooze() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::EnospcRisk {
                margin: -5,
                count_below: 1,
                device_count: 2,
            }],
        );
        let runner = ack_mounted_probe_runner_with_zero_size_real_path_enospc_usage();

        let result = cmd_ack_impl(
            &runner,
            &ack_fs_btrfs(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );

        assert!(result.is_ok(), "ack must succeed, got {result:?}");
        assert!(!paths.alert_latch_json().exists(), "ack clears the latch");
        assert!(
            load_enospc_ack(&paths).unwrap().is_none(),
            "a zero-sized real-path snapshot writes no snooze marker"
        );
    }

    // Intent: a mounted ack of an EnospcRisk latch whose usage probe is unstubbed
    //   (MissingMock) clears the latch but writes no baseline.
    // Why it exists: the baseline probe is best-effort -- a probe failure must not
    //   fail the ack or fabricate a baseline. One quiet re-fire next cycle, then a
    //   clean ack establishes it.
    // Scenario: the usage probe fails during a mounted ack of an ENOSPC alert.
    #[test]
    fn cmd_ack_mounted_enospc_risk_unstubbed_usage_writes_no_baseline() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::EnospcRisk {
                margin: -5,
                count_below: 1,
                device_count: 2,
            }],
        );
        // usage NOT stubbed -> MissingMock on the baseline probe.
        let runner = ack_mounted_probe_runner();

        let result = cmd_ack_impl(
            &runner,
            &ack_fs_btrfs(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );
        assert!(result.is_ok(), "best-effort baseline must not fail the ack");
        assert!(!paths.alert_latch_json().exists(), "ack clears the latch");
        assert!(
            !paths.enospc_ack_json().exists(),
            "a failed usage probe writes no baseline"
        );
    }

    // Intent: a mounted ack whose live pool has no FS UUID clears the latch but
    //   writes no baseline (no usable PoolKey).
    // Why it exists: a keyless baseline would be invalidated immediately anyway;
    //   the absent strongest-identity field must not produce a weak baseline.
    // Scenario: a transient mounted ack reads a btrfs show with no uuid line.
    #[test]
    fn cmd_ack_mounted_enospc_risk_no_fsid_writes_no_baseline() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::EnospcRisk {
                margin: -5,
                count_below: 1,
                device_count: 2,
            }],
        );
        let runner = ack_mounted_probe_runner_no_uuid_with_enospc_usage();

        let result = cmd_ack_impl(
            &runner,
            &ack_fs_btrfs(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );
        assert!(result.is_ok(), "ack must succeed, got {result:?}");
        assert!(!paths.alert_latch_json().exists(), "ack clears the latch");
        assert!(
            !paths.enospc_ack_json().exists(),
            "no FS UUID -> no keyed baseline"
        );
    }

    // Intent: an offline ack of an EnospcRisk-only latch is allowed, clears the
    //   latch, and writes no baseline.
    // Why it exists: offline ack cannot probe the live pool_key, so it
    //   intentionally writes no ENOSPC baseline (one quiet re-fire on remount,
    //   then a mounted ack establishes it).
    // Scenario: the pool is locked/offline and the operator acks a latched ENOSPC
    //   warning.
    #[test]
    fn ack_offline_enospc_risk_clears_latch_writes_no_baseline() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::EnospcRisk {
                margin: -5,
                count_below: 1,
                device_count: 2,
            }],
        );

        let result = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );
        assert!(
            result.is_ok(),
            "offline ack of an EnospcRisk-only latch must be allowed, got {result:?}"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "offline ack clears the latch"
        );
        assert!(
            !paths.enospc_ack_json().exists(),
            "offline ack cannot key a baseline, so it writes none"
        );
    }
}
