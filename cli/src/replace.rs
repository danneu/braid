use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::{Config, config_read, mapper_name, name_from_mapper};
use crate::confirm;
use crate::credential_verify::{
    Credential, CredentialVerifyError, CredentialVerifyTarget, verify_credential_for_targets,
};
use crate::inhibit::AcquireSleepInhibitor;
use crate::journal;
use crate::luks::{
    backup_luks_header, ensure_luks_open, format_keyfile_asymmetry_warning,
    format_keyfile_enrollment_probe_failure, luks_format, probe_pool_keyfile_enrollment,
    read_passphrase,
};
use crate::membership::{self, PoolMembership};
use crate::parse::parse_btrfs_device_stats;
use crate::pool::{pool_replace_device, pool_resize_device};
use crate::preflight;
use crate::preview::{self, PerDiskStyle, Preview, PreviewCompleteness, PreviewNote};
use crate::probe::{Filesystem, ProbeError, probe_config_disk, probe_pool};
use crate::progress::ProgressOutput;
use crate::state_paths::StatePaths;
use crate::status_tag::{StatusTag, color_enabled_for_stderr, emit_status, status_line};
use crate::types::*;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ReplaceError {
    #[error("{0}")]
    Validation(String),
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("luks error: {0}")]
    Luks(#[from] crate::luks::LuksError),
    #[error("pool error: {0}")]
    Pool(#[from] crate::pool::PoolError),
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] crate::parse::ParseError),
}

pub struct ReplaceParams<'a> {
    pub config_path: &'a Path,
    pub old_name: &'a str,
    pub new_name: &'a str,
    pub missing_id: Option<u64>,
    pub dry_run: bool,
    pub yes: bool,
    pub passphrase_stdin: bool,
    pub passphrase_file: Option<&'a Path>,
    pub enroll_key_file: Option<&'a Path>,
    pub luks_format_extra_opts: &'a [String],
    pub progress: ProgressOutput,
    pub paths: &'a StatePaths,
    /// Seam for acquiring a logind sleep inhibitor before the irreversible
    /// portion of the replace. Production passes `&RealSleepInhibitor`;
    /// unit tests pass `&NoopSleepInhibitor` to avoid spawning subprocesses.
    pub sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
}

/// Dry-run preview source of truth for `braid replace` plus the preflight
/// state `execute()` needs to finish the operation. `notes` carries
/// plan-derived preflight diagnostics (busy-op Info, readonly-probe-fail
/// Warn) and keyfile enrollment diagnostics so both dry-run stdout and
/// real-run stderr see the same wording. The 1-disk leftover `WARNING:`
/// remains confirmation-only behind the `!params.yes` gate.
pub struct ReplacePlan {
    pub notes: Vec<PreviewNote>,
    pub steps: Vec<Step>,
    pub config: Config,
    pub new_name: String,
    pub new_by_id: ByIdPath,
    pub pool: PoolState,
    pub replace_source: ReplaceSource,
    pub new_probed: ConfigDisk,
    pub pre_membership: PoolMembership,
    pub target_membership: PoolMembership,
}

/// Report returned by `plan_replace`. Shape A: when planning fails after
/// notes have been accumulated (e.g. a post-preflight validation rejects),
/// the notes survive on `report.notes` so `cmd_replace` can render them
/// to stderr before the error. On the `Ok` branch, accumulated notes have
/// moved into `plan.notes` and `report.notes` is empty.
pub struct ReplacePlanReport {
    pub notes: Vec<PreviewNote>,
    pub result: Result<ReplacePlan, ReplaceError>,
}

impl ReplacePlan {
    /// Real-run and failure-path stderr for `replace` use `Bracketed`
    /// per-disk style to match the canonical dry-run render. `replace`
    /// does not emit `PerDisk` notes today, but the constant keeps the
    /// Shape A contract uniform with the other migrated commands.
    pub const STDERR_STYLE: PerDiskStyle = PerDiskStyle::Bracketed;

    /// Build a `Preview` carrying any plan-derived notes. The 1-disk
    /// leftover `WARNING:` line stays in `execute()` behind the
    /// `!params.yes` gate and does not appear here.
    pub fn preview(&self) -> Preview {
        Preview {
            completeness: PreviewCompleteness::Complete,
            notes: self.notes.clone(),
            steps: self.steps.clone(),
        }
    }

    pub fn execute<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
        self,
        runner: &R,
        fs: &F,
        params: &ReplaceParams<'_>,
    ) -> Result<(), ReplaceError> {
        let color_enabled = color_enabled_for_stderr();
        let ReplacePlan {
            notes,
            steps: _,
            config,
            new_name,
            new_by_id,
            pool,
            replace_source,
            new_probed,
            pre_membership,
            target_membership,
        } = self;

        // Render accumulated notes to stderr via the shared helper
        // BEFORE any mutation. Matches the other Shape A commands so
        // preflight diagnostics surface identically across success,
        // failure, and dry-run stdout.
        emit_replace_notes_to_stderr(&notes);

        let old_mn = mapper_name(params.old_name);
        let new_mn = mapper_name(&new_name);
        let new_target = build_replace_journal_target(
            &new_name,
            &new_by_id,
            &new_probed,
            params.enroll_key_file,
            params.luks_format_extra_opts,
        )?;
        let journal_source = build_replace_journal_source(&replace_source);
        let restore_raid1_after_commit = matches!(&replace_source, ReplaceSource::Missing { .. })
            && pool.missing_count == 1
            && pool.devices.len() + 1 >= 2;

        // Confirm
        if !params.yes {
            let old_underlying = match &replace_source {
                ReplaceSource::Live { .. } => pool
                    .devices
                    .iter()
                    .find(|d| d.mapper == old_mn)
                    .map(|d| d.underlying.as_str()),
                ReplaceSource::Missing { .. } => None,
            };
            let old_hw = old_underlying.map(|u| confirm::query_disk_hw_info(runner, u));
            let new_hw = confirm::query_disk_hw_info(runner, &new_by_id.0);
            let is_missing = matches!(&replace_source, ReplaceSource::Missing { .. });

            emit_replace_stderr(&format!(
                "{}\n",
                format_replace_confirm(
                    &ReplaceConfirmOld {
                        name: params.old_name,
                        hw: old_hw.as_ref(),
                        source: &replace_source,
                    },
                    &ReplaceConfirmNew {
                        name: new_name.as_str(),
                        by_id: &new_by_id.0,
                        hw: &new_hw,
                        needs_luks_format: matches!(
                            new_probed.state,
                            ConfigDiskState::PresentNotLuks
                        ),
                        is_rebuild: is_missing,
                    },
                    pool.total_devices,
                )
            ));
            if pool.total_devices == 1 {
                emit_replace_stderr(
                    "WARNING: This replace leaves only 1 disk -- no redundancy.\n\n",
                );
            }
            confirm::confirm_yes().map_err(ReplaceError::Validation)?;
        }

        // Read passphrase
        let passphrase = read_passphrase(params.passphrase_file, params.passphrase_stdin)?;

        // Reversible checks: reject absent disk, verify passphrase, check not already in pool.
        if matches!(new_probed.state, ConfigDiskState::Absent) {
            return Err(ReplaceError::Validation(format!(
                "new disk '{}' ({}) is not present. Is it plugged in?",
                new_name, new_by_id
            )));
        }

        let retained_members: Vec<_> = match &replace_source {
            ReplaceSource::Live { .. } => pool
                .devices
                .iter()
                .filter(|device| device.mapper != old_mn)
                .collect(),
            ReplaceSource::Missing { .. } => pool.devices.iter().collect(),
        };
        let anchor_members: Vec<_> = if matches!(&replace_source, ReplaceSource::Live { .. })
            && retained_members.is_empty()
        {
            pool.devices
                .iter()
                .filter(|device| device.mapper == old_mn)
                .collect()
        } else {
            retained_members
        };
        let mut credential_targets: Vec<CredentialVerifyTarget> = anchor_members
            .into_iter()
            .map(|device| CredentialVerifyTarget {
                name: name_from_mapper(&device.mapper.0)
                    .unwrap_or(device.mapper.0.as_str())
                    .to_owned(),
                device: device.underlying.clone(),
            })
            .collect();
        let new_disk_target = match &new_probed.state {
            ConfigDiskState::PresentLuks { .. } => Some(CredentialVerifyTarget {
                name: new_name.clone(),
                device: new_by_id.0.clone(),
            }),
            ConfigDiskState::Absent | ConfigDiskState::PresentNotLuks => None,
        };
        if let Some(target) = &new_disk_target {
            credential_targets.push(target.clone());
        }

        if !credential_targets.is_empty() {
            match verify_credential_for_targets(
                runner,
                &credential_targets,
                Credential::Passphrase(&passphrase),
                color_enabled,
                |line| emit_replace_stderr(line),
            ) {
                Ok(()) => {}
                Err(CredentialVerifyError::Rejected { target }) => {
                    let is_new_disk = new_disk_target.as_ref() == Some(&target);
                    return Err(ReplaceError::Validation(if is_new_disk {
                        format!(
                            "passphrase rejected by new disk '{}' ({})",
                            target.name, target.device
                        )
                    } else {
                        format!(
                            "passphrase does not match existing pool member '{}'",
                            target.name
                        )
                    }));
                }
                Err(CredentialVerifyError::Luks { source, .. }) => {
                    return Err(ReplaceError::Luks(source));
                }
            }
        }

        // Guard: new disk must not already be in the pool.
        check_new_not_in_pool(&new_name, &new_mn, &pool)?;

        // Hold a logind sleep inhibitor for the rest of the replace operation --
        // covers Step 1 LUKS init, the long-running btrfs replace start, and
        // the post-replace soft balance for missing-path replaces. Suspending
        // mid-replace produces kernel-level topology corruption on every kernel
        // -- see issues #45 and #48 and the upstream warning at
        // reference/btrfs-progs/Documentation/btrfs-replace.rst:49-50.
        //
        // Acquired here, AFTER all interactive/reversible work (confirmation,
        // passphrase read+verify, check_new_not_in_pool) and BEFORE
        // journal::write_journal, so that:
        //   - operator-idle prompts do not block suspend
        //   - a logind failure aborts cleanly without stranding pending-op.json
        //     and forcing the user into recovery mode for a preflight failure.
        let _sleep_inhibitor_guard = params
            .sleep_inhibitor
            .acquire("replace in progress")
            .map_err(|e| {
                ReplaceError::Validation(format!(
                    "could not acquire sleep inhibitor (is logind running?): {e}"
                ))
            })?;

        // Write journal before irreversible disk ops. pre_membership and
        // target_membership were computed earlier, before the inhibitor.
        let journal = journal::build_journal(
            pre_membership,
            target_membership.clone(),
            journal::OpKind::Replace {
                phase: journal::ReplacePhase::PoolMutation,
                old_name: params.old_name.to_owned(),
                new_name: new_name.clone(),
                new_by_id: new_by_id.clone(),
                new_target: new_target.clone(),
                source: journal_source.clone(),
                restore_raid1_after_commit,
            },
        );
        journal::write_journal(params.paths, &journal)
            .map_err(|e| ReplaceError::Validation(e.to_string()))?;

        // Step 1: Init new disk (LUKS format/open) -- irreversible from here.
        match new_probed.state {
            ConfigDiskState::Absent => unreachable!("already checked above"),
            ConfigDiskState::PresentNotLuks => {
                // Passphrase already verified above.
                let luks_opts =
                    effective_luks_format_opts(&new_name, params.luks_format_extra_opts);
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Wait,
                        color_enabled,
                        &format!("disk {new_name}: formatting LUKS..."),
                    )
                );
                luks_format(runner, &new_by_id.0, &passphrase, &luks_opts)?;
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Ok,
                        color_enabled,
                        &format!("disk {new_name}: LUKS formatted"),
                    )
                );

                if let Some(kf) = params.enroll_key_file {
                    eprint!(
                        "{}",
                        status_line(
                            StatusTag::Wait,
                            color_enabled,
                            &format!("disk {new_name}: enrolling keyfile in slot 1..."),
                        )
                    );
                    crate::luks::enroll_key_file(runner, &new_by_id.0, &passphrase, kf)?;
                    eprint!(
                        "{}",
                        status_line(
                            StatusTag::Ok,
                            color_enabled,
                            &format!("disk {new_name}: keyfile enrolled in slot 1"),
                        )
                    );
                }

                let backup_path =
                    backup_luks_header(runner, &new_by_id.0, &new_mn.0, params.paths)?;
                eprintln!("LUKS header backed up: {}", backup_path.display());

                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Wait,
                        color_enabled,
                        &format!("disk {new_name}: unlocking..."),
                    )
                );
                ensure_luks_open(runner, fs, &new_name, &new_by_id, &passphrase)?;
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Ok,
                        color_enabled,
                        &format!("disk {new_name}: unlocked"),
                    )
                );
            }
            ConfigDiskState::PresentLuks { mapper_open, .. } => {
                if !mapper_open {
                    eprint!(
                        "{}",
                        status_line(
                            StatusTag::Wait,
                            color_enabled,
                            &format!("disk {new_name}: unlocking..."),
                        )
                    );
                    ensure_luks_open(runner, fs, &new_name, &new_by_id, &passphrase)?;
                    eprint!(
                        "{}",
                        status_line(
                            StatusTag::Ok,
                            color_enabled,
                            &format!("disk {new_name}: unlocked"),
                        )
                    );
                } else if !pool.devices.iter().any(|d| d.mapper == new_mn) {
                    eprintln!(
                        "note: LUKS mapper is already open but device is not yet in pool. Completing replace."
                    );
                }
            }
        }

        let new_mapper_path = format!("/dev/mapper/{}", new_mn.0);

        // Step 2+: Execute replacement -- both paths use btrfs replace start.
        // Live-only: warn if the source device has accumulated I/O errors.
        if let ReplaceSource::Live { mapper: _, devid } = &replace_source {
            let stats_raw = runner.run(&CmdRequest::BtrfsDeviceStatsJson {
                mount_point: config.mount_point().clone(),
            });
            if let Ok(ref raw) = stats_raw
                && let Ok(stats) = parse_btrfs_device_stats(raw)
                && source_has_io_errors(&stats, *devid)
            {
                eprintln!(
                    "Warning: source device (devid {devid}) has I/O errors. \
                     btrfs replace will read from mirrors where possible, \
                     but may fail if any data lacks a healthy mirror copy."
                );
            }
        }

        // Kickoff wording differs (replace-in-place vs rebuild-missing), but the
        // underlying `btrfs replace start` + resize sequence is identical. Bind
        // devid here so the shared spine below runs once.
        let devid = match &replace_source {
            ReplaceSource::Live { devid, .. } => {
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Wait,
                        color_enabled,
                        &format!("pool: replacing devid {devid} with {new_mn}..."),
                    )
                );
                *devid
            }
            ReplaceSource::Missing { devid } => {
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Wait,
                        color_enabled,
                        &format!("pool: rebuilding missing devid {devid} onto {new_mn}..."),
                    )
                );
                *devid
            }
        };

        pool_replace_device(
            runner,
            devid,
            &new_mapper_path,
            config.mount_point(),
            params.progress,
        )?;
        eprint!(
            "{}",
            status_line(StatusTag::Ok, color_enabled, "pool: replace complete")
        );

        // Membership committed by btrfs replace. Enrich with kernel-assigned
        // devid + observed luks_uuid from a fresh probe, then persist before
        // the post-replace cleanup, resize, and (missing-path) soft balance.
        // The journal still covers maintenance, so recovery can replay it if
        // we crash before clear_journal.
        let mut target_membership = target_membership;
        if let Ok(pool_after) = probe_pool(runner, fs, config.mount_point()) {
            membership::enrich_from_pool_state(&pool_after, &mut target_membership);
        }
        membership::save_membership(&target_membership, params.paths).map_err(|e| {
            ReplaceError::Validation(format!("failed to persist pool membership: {e}"))
        })?;
        let journal = journal::rewrite_journal(
            params.paths,
            &journal,
            journal::OpKind::Replace {
                phase: journal::ReplacePhase::PostReplaceMaintenance,
                old_name: params.old_name.to_owned(),
                new_name: new_name.clone(),
                new_by_id: new_by_id.clone(),
                new_target,
                source: journal_source,
                restore_raid1_after_commit,
            },
            Some(target_membership.clone()),
        )
        .map_err(|e| ReplaceError::Validation(e.to_string()))?;

        // Live-only: best-effort close of old mapper. Runs BEFORE the resize
        // so a resize failure does not `?` out and strand the old dm slot
        // bound to the backing disk until `braid lock` or reboot. Missing has
        // no old mapper to close.
        if let journal::OpKind::Replace {
            source:
                journal::ReplaceJournalSource::Live {
                    old_mapper: mapper, ..
                },
            ..
        } = &journal.op
        {
            let old_label = mapper.0.strip_prefix("braid-").unwrap_or(&mapper.0);
            emit_status(&status_line(
                StatusTag::Wait,
                color_enabled,
                &format!("disk {old_label}: locking..."),
            ));
            let close_result = runner.run(&CmdRequest::CryptsetupClose {
                mapper: mapper.0.clone(),
            });
            match close_result {
                Ok(r) if r.exit_status == 0 => {
                    emit_status(&status_line(
                        StatusTag::Ok,
                        color_enabled,
                        &format!("disk {old_label}: locked"),
                    ));
                    eprintln!(
                        "Old device closed. If repurposing the physical disk, wipe it separately."
                    );
                }
                Ok(r) => {
                    emit_status(&status_line(
                        StatusTag::Warn,
                        color_enabled,
                        &format!("disk {old_label}: lock failed (exit {})", r.exit_status),
                    ));
                }
                Err(e) => {
                    emit_status(&status_line(
                        StatusTag::Warn,
                        color_enabled,
                        &format!("disk {old_label}: lock failed ({e})"),
                    ));
                }
            }
        }

        pool_resize_device(runner, devid, config.mount_point())?;

        // Restore RAID1 redundancy for missing-path replacements that clear the last missing device
        if restore_raid1_after_commit {
            crate::pool::maybe_restore_raid1(
                runner,
                fs,
                config.mount_point(),
                pool.missing_count,
                params.progress,
            )
            .map_err(ReplaceError::Pool)?;
        }

        // Maintenance complete -- safe to clear the journal.
        journal::clear_journal(params.paths)
            .map_err(|e| ReplaceError::Validation(e.to_string()))?;

        eprintln!("Done. Replaced {} with {}.", params.old_name, new_name);
        Ok(())
    }
}

/// True iff the stats row identified by `devid` has any non-zero error
/// counter. Pairing is by devid (canonical identity from btrfs), not by
/// mapper path -- the row's path string can differ from the canonical
/// /dev/mapper/braid-X without changing which physical device it describes.
fn source_has_io_errors(stats: &crate::parse::types::BtrfsDeviceStatsOutput, devid: u64) -> bool {
    stats.devices.iter().any(|d| {
        d.devid == devid
            && (d.read_io_errs > 0
                || d.write_io_errs > 0
                || d.flush_io_errs > 0
                || d.corruption_errs > 0
                || d.generation_errs > 0)
    })
}

fn emit_replace_notes_to_stderr(notes: &[PreviewNote]) {
    let rendered =
        render_replace_notes_for_stderr(notes, crate::status_tag::color_enabled_for_stderr());
    emit_replace_stderr(&rendered);
}

fn render_replace_notes_for_stderr(notes: &[PreviewNote], color_enabled: bool) -> String {
    preview::render_notes_for_stderr_with(notes, ReplacePlan::STDERR_STYLE, color_enabled)
}

fn emit_replace_stderr(rendered: &str) {
    #[cfg(test)]
    if replace_stderr_capture::write(rendered) {
        return;
    }
    eprint!("{rendered}");
}

#[cfg(test)]
mod replace_stderr_capture {
    use std::cell::RefCell;

    thread_local! {
        static CAPTURED_STDERR: RefCell<Option<String>> = RefCell::new(None);
    }

    pub(super) fn capture<F, T>(f: F) -> (T, String)
    where
        F: FnOnce() -> T,
    {
        CAPTURED_STDERR.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none(), "nested replace stderr capture");
            *slot = Some(String::new());
        });

        let result = f();
        let stderr = CAPTURED_STDERR.with(|slot| {
            slot.borrow_mut()
                .take()
                .expect("replace stderr capture must be active")
        });
        (result, stderr)
    }

    pub(super) fn write(text: &str) -> bool {
        CAPTURED_STDERR.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.as_mut() {
                Some(stderr) => {
                    stderr.push_str(text);
                    true
                }
                None => false,
            }
        })
    }
}

/// Plan a `braid replace` run. Owns everything above today's `--dry-run`
/// gate: pending-op preflight, config read, `--new` spec parsing,
/// `--old == --new` guard, keyfile path validation, `probe_pool` + mounted
/// validation, mutation/UPS preflight, replace-source resolution,
/// membership load + `build_replacement_membership`, new-disk probe, and
/// step compilation. Returns a `ReplacePlanReport`: on success, accumulated
/// notes move into `plan.notes`; on post-preflight failure, accumulated
/// notes stay on `report.notes` so `cmd_replace` can render them before
/// returning the error.
///
/// Does not read or verify the passphrase, acquire the sleep inhibitor,
/// or run `check_new_not_in_pool` -- those happen inside
/// `ReplacePlan::execute` so `--dry-run` keeps short-circuiting before
/// them.
pub fn plan_replace<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &ReplaceParams<'_>,
) -> ReplacePlanReport {
    // Notes accumulator. `err_empty` is correct for pre-preflight exits
    // (no notes can have accumulated yet). Post-preflight exits return
    // a notes-preserving report so preflight diagnostics (busy-op Info,
    // readonly-probe-fail Warn) reach `cmd_replace`'s stderr render.
    let mut notes: Vec<PreviewNote> = Vec::new();
    let err_empty = |e: ReplaceError| ReplacePlanReport {
        notes: Vec::new(),
        result: Err(e),
    };

    if let Err(msg) = preflight::check_no_pending_operation(params.paths) {
        return err_empty(ReplaceError::Validation(msg));
    }

    let config = match config_read(params.config_path) {
        Ok(c) => c,
        Err(e) => return err_empty(e.into()),
    };

    // Parse new_name as name=by_id spec
    let (new_name_parsed, new_by_id) = match membership::parse_disk_spec(params.new_name) {
        Ok(v) => v,
        Err(e) => return err_empty(ReplaceError::Validation(e.to_string())),
    };
    let new_name = new_name_parsed.as_str();

    // --old == --new: reject before any pool or disk probes.
    if params.old_name == new_name {
        return err_empty(ReplaceError::Validation(
            "--old and --new must be different disks".into(),
        ));
    }

    if let Some(kf) = params.enroll_key_file
        && let Err(e) = crate::enroll_key_file::validate_key_file_path(kf, false)
    {
        return err_empty(ReplaceError::Validation(e.to_string()));
    }

    let pool = match probe_pool(runner, fs, config.mount_point()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return err_empty(ReplaceError::Validation(
                "pool is not mounted. Cannot replace.".into(),
            ));
        }
        Err(e) => return err_empty(ReplaceError::Probe(e)),
    };

    if !pool.mounted {
        return err_empty(ReplaceError::Validation(
            "pool is not mounted. Cannot replace.".into(),
        ));
    }

    // Preflight
    let fsid = pool.fsid.as_deref().expect("mounted pool must have FSID");
    match preflight::require_mutation_preflight(runner, fs, fsid, config.mount_point()) {
        Ok(preflight_notes) => notes.extend(preflight_notes),
        Err(msg) => return err_empty(ReplaceError::Validation(msg)),
    }
    if let Err(msg) = preflight::check_ups_not_on_battery(
        runner,
        config.ups().map(|u| u.name.as_str()),
        "replace",
    ) {
        return ReplacePlanReport {
            notes: std::mem::take(&mut notes),
            result: Err(ReplaceError::Validation(msg)),
        };
    }

    // Resolve --old: live or missing (by devid).
    let old_mn = mapper_name(params.old_name);
    let replace_source = match resolve_replace_source(
        runner,
        params.old_name,
        &old_mn,
        params.missing_id,
        &pool,
        config.mount_point(),
    ) {
        Ok(v) => v,
        Err(e) => {
            return ReplacePlanReport {
                notes: std::mem::take(&mut notes),
                result: Err(e),
            };
        }
    };

    // Validate --old against pool.json membership before any irreversible work.
    // build_replacement_membership rejects absent old_name and (on Missing path)
    // a devid mismatch between pool.json and the resolved missing devid. Running
    // it here -- before the inhibitor and journal write -- means a typo in --old
    // aborts cleanly with no pending-op.json on disk and no systemd-inhibit held.
    let pre_membership = match membership::load_membership(params.paths) {
        Ok(m) => m,
        Err(e) => {
            return ReplacePlanReport {
                notes: std::mem::take(&mut notes),
                result: Err(ReplaceError::Validation(format!(
                    "failed to load pool membership: {e}"
                ))),
            };
        }
    };
    let target_membership = match build_replacement_membership(
        &pre_membership,
        params.old_name,
        new_name,
        &new_by_id,
        &replace_source,
    ) {
        Ok(m) => m,
        Err(e) => {
            return ReplacePlanReport {
                notes: std::mem::take(&mut notes),
                result: Err(e),
            };
        }
    };

    // Probe --new disk state
    let new_probed = match probe_config_disk(runner, fs, new_name, &new_by_id) {
        Ok(p) => p,
        Err(e) => {
            return ReplacePlanReport {
                notes: std::mem::take(&mut notes),
                result: Err(e.into()),
            };
        }
    };

    // Keyfile diagnostics are plan notes, not confirmation-only stderr.
    // This keeps dry-run stdout, real-run stderr, and preserved-error
    // stderr on the same PreviewNote contract used by `add`.
    if matches!(new_probed.state, ConfigDiskState::PresentNotLuks)
        && params.enroll_key_file.is_none()
    {
        let keyfile_probe = probe_pool_keyfile_enrollment(runner, &pool.devices);
        if keyfile_probe.has_enrollment {
            notes.push(PreviewNote::Warn(format_keyfile_asymmetry_warning()));
        } else {
            notes.extend(keyfile_probe.failures.iter().map(|failure| {
                PreviewNote::Warn(format_keyfile_enrollment_probe_failure(failure))
            }));
        }
    }

    // Compile steps
    let will_clear_last_missing =
        matches!(&replace_source, ReplaceSource::Missing { .. }) && pool.missing_count == 1;
    let steps = match compile_replace_steps(&ReplaceStepsInput {
        new_name,
        new_by_id: &new_by_id,
        new_probed: &new_probed,
        replace_source: &replace_source,
        mount_point: config.mount_point(),
        will_clear_last_missing,
        total_devices: pool.total_devices,
        paths: params.paths,
        enroll_key_file: params.enroll_key_file,
        luks_format_extra_opts: params.luks_format_extra_opts,
    }) {
        Ok(s) => s,
        Err(e) => {
            return ReplacePlanReport {
                notes: std::mem::take(&mut notes),
                result: Err(e),
            };
        }
    };

    ReplacePlanReport {
        notes: Vec::new(),
        result: Ok(ReplacePlan {
            notes,
            steps,
            config,
            new_name: new_name.to_owned(),
            new_by_id,
            pool,
            replace_source,
            new_probed,
            pre_membership,
            target_membership,
        }),
    }
}

pub fn cmd_replace<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &ReplaceParams<'_>,
) -> Result<(), ReplaceError> {
    let report = plan_replace(runner, fs, params);
    let plan = match report.result {
        Ok(p) => p,
        Err(e) => {
            // Preserved-context failure: accumulated notes render to
            // stderr before the error via the SAME helper as the Ok
            // path (`ReplacePlan::execute`), so preflight diagnostics
            // surface identically across success, failure, and dry-run
            // stdout.
            emit_replace_notes_to_stderr(&report.notes);
            return Err(e);
        }
    };
    if params.dry_run {
        plan.preview().print_colored();
        return Ok(());
    }
    plan.execute(runner, fs, params)
}

#[derive(Debug)]
pub enum ReplaceSource {
    /// Old disk is alive in the pool -- replace via `btrfs replace start`.
    Live { mapper: MapperName, devid: u64 },
    /// Old disk is missing -- replace via `btrfs replace start` by devid.
    Missing { devid: u64 },
}

fn effective_luks_format_opts(new_name: &str, extra_opts: &[String]) -> Vec<String> {
    let mut opts = extra_opts.to_vec();
    opts.push("--label".into());
    opts.push(format!("braid-{new_name}"));
    opts
}

fn build_replace_journal_source(source: &ReplaceSource) -> journal::ReplaceJournalSource {
    match source {
        ReplaceSource::Live { mapper, devid } => journal::ReplaceJournalSource::Live {
            old_devid: *devid,
            old_mapper: mapper.clone(),
        },
        ReplaceSource::Missing { devid } => {
            journal::ReplaceJournalSource::Missing { old_devid: *devid }
        }
    }
}

fn build_replace_journal_target(
    new_name: &str,
    new_by_id: &ByIdPath,
    new_probed: &ConfigDisk,
    enroll_key_file: Option<&Path>,
    luks_format_extra_opts: &[String],
) -> Result<journal::ReplaceJournalTarget, ReplaceError> {
    let mode = match &new_probed.state {
        ConfigDiskState::PresentNotLuks => journal::ReplaceJournalMode::FreshLuks {
            luks_label: format!("braid-{new_name}"),
            luks_format_extra_opts: effective_luks_format_opts(new_name, luks_format_extra_opts),
            enroll_key_file: enroll_key_file.map(|p| p.to_path_buf()),
        },
        ConfigDiskState::PresentLuks { uuid, .. } => journal::ReplaceJournalMode::ExistingLuks {
            luks_uuid: uuid.clone(),
        },
        ConfigDiskState::Absent => {
            return Err(ReplaceError::Validation(format!(
                "new disk '{}' ({}) is not present. Is it plugged in?",
                new_name, new_by_id
            )));
        }
    };
    Ok(journal::ReplaceJournalTarget {
        by_id: new_by_id.clone(),
        mapper_name: mapper_name(new_name).0,
        mode,
    })
}

fn check_new_not_in_pool(
    new_name: &str,
    new_mn: &MapperName,
    pool: &PoolState,
) -> Result<(), ReplaceError> {
    if pool.devices.iter().any(|d| d.mapper == *new_mn) {
        return Err(ReplaceError::Validation(format!(
            "new disk '{}' is already a member of the pool. Cannot replace with an existing member.",
            new_name
        )));
    }
    Ok(())
}

fn resolve_replace_source<R: CommandRunner>(
    runner: &R,
    old_name: &str,
    old_mn: &MapperName,
    missing_id: Option<u64>,
    pool: &PoolState,
    mount_point: &MountPoint,
) -> Result<ReplaceSource, ReplaceError> {
    let old_in_pool = pool.devices.iter().any(|d| d.mapper == *old_mn);

    if old_in_pool {
        // Live old disk in pool.
        if missing_id.is_some() {
            return Err(ReplaceError::Validation(
                "--missing-id cannot be used when the old disk is still alive in the pool".into(),
            ));
        }
        if pool.missing_count > 0 {
            return Err(ReplaceError::Validation(format!(
                "pool has {} missing device{}. \
                 Repair the missing device{} first with `braid replace --missing-id <devid>`, \
                 then retry this live replace. Use `braid status` to see device IDs.",
                pool.missing_count,
                if pool.missing_count == 1 { "" } else { "s" },
                if pool.missing_count == 1 { "" } else { "s" },
            )));
        }
        let devid = pool
            .devices
            .iter()
            .find(|d| d.mapper == *old_mn)
            .map(|d| d.devid)
            .expect("old_in_pool was true but device not found");
        return Ok(ReplaceSource::Live {
            mapper: old_mn.clone(),
            devid,
        });
    }

    // Old disk not in pool -- dead/missing path.
    // Probe actual missing devids for validation and auto-resolution.
    let missing_devids =
        preflight::probe_missing_devids(runner, mount_point).map_err(ReplaceError::Validation)?;

    if let Some(devid) = missing_id {
        // Validate --missing-id refers to an actually-missing device.
        if pool.devices.iter().any(|d| d.devid == devid) {
            return Err(ReplaceError::Validation(format!(
                "devid {devid} is a live device, not a missing one."
            )));
        }
        if !missing_devids.contains(&devid) {
            return Err(ReplaceError::Validation(format!(
                "devid {devid} is not a missing device in this pool. \
                 Use 'braid status' to see device IDs."
            )));
        }
        return Ok(ReplaceSource::Missing { devid });
    }

    if missing_devids.is_empty() {
        return Err(ReplaceError::Validation(format!(
            "disk '{}' not found in pool and no missing devices detected.",
            old_name
        )));
    }

    if missing_devids.len() == 1 {
        return Ok(ReplaceSource::Missing {
            devid: missing_devids[0],
        });
    }

    Err(ReplaceError::Validation(format!(
        "multiple missing devices ({} missing). Pass --missing-id <devid> to target the specific dead disk. Use 'braid status' to see device IDs.",
        missing_devids.len()
    )))
}

struct ReplaceStepsInput<'a> {
    new_name: &'a str,
    new_by_id: &'a ByIdPath,
    new_probed: &'a ConfigDisk,
    replace_source: &'a ReplaceSource,
    mount_point: &'a MountPoint,
    will_clear_last_missing: bool,
    total_devices: u64,
    paths: &'a StatePaths,
    enroll_key_file: Option<&'a Path>,
    luks_format_extra_opts: &'a [String],
}

fn compile_replace_steps(input: &ReplaceStepsInput<'_>) -> Result<Vec<Step>, ReplaceError> {
    let new_mn = mapper_name(input.new_name);
    let mut steps = Vec::new();

    match &input.new_probed.state {
        ConfigDiskState::Absent => {
            return Err(ReplaceError::Validation(format!(
                "new disk '{}' ({}) is not present. Is it plugged in?",
                input.new_name, input.new_by_id
            )));
        }
        ConfigDiskState::PresentNotLuks => {
            let extra_opts =
                effective_luks_format_opts(input.new_name, input.luks_format_extra_opts);
            steps.push(Step {
                risk: "destructive",
                description: format!("LUKS format {}", input.new_by_id),
                commands: vec![CmdRequest::CryptsetupLuksFormat {
                    device: input.new_by_id.0.clone(),
                    extra_opts,
                }],
            });
            if let Some(kf) = input.enroll_key_file {
                steps.push(Step {
                    risk: "safe",
                    description: format!("enroll keyfile -> LUKS slot 1 on {}", input.new_by_id),
                    commands: vec![CmdRequest::CryptsetupLuksAddKeyFile {
                        device: input.new_by_id.0.clone(),
                        key_file_path: kf.display().to_string(),
                    }],
                });
            }
            let backup_path = input
                .paths
                .luks_headers_dir()
                .join(format!("{}.luksheader", new_mn.0));
            steps.push(Step {
                risk: "safe",
                description: format!("LUKS header backup -> {}", backup_path.display()),
                commands: vec![CmdRequest::CryptsetupLuksHeaderBackup {
                    device: input.new_by_id.0.clone(),
                    backup_path: backup_path.display().to_string(),
                }],
            });
            steps.push(Step {
                risk: "safe",
                description: format!("LUKS open -> {}", new_mn),
                commands: vec![CmdRequest::CryptsetupLuksOpen {
                    device: input.new_by_id.0.clone(),
                    mapper: new_mn.0.clone(),
                }],
            });
        }
        ConfigDiskState::PresentLuks { mapper_open, .. } => {
            if !mapper_open {
                steps.push(Step {
                    risk: "safe",
                    description: format!("LUKS open -> {}", new_mn),
                    commands: vec![CmdRequest::CryptsetupLuksOpen {
                        device: input.new_by_id.0.clone(),
                        mapper: new_mn.0.clone(),
                    }],
                });
            }
        }
    }

    let new_mapper_path = format!("/dev/mapper/{}", new_mn.0);
    let devid = match input.replace_source {
        ReplaceSource::Live { devid, .. } | ReplaceSource::Missing { devid } => *devid,
    };

    // Shared: btrfs replace start.
    steps.push(Step {
        risk: "long",
        description: format!(
            "btrfs replace start {} /dev/mapper/{} {}",
            devid, new_mn, input.mount_point
        ),
        commands: vec![CmdRequest::BtrfsReplaceStart {
            devid,
            target_device: new_mapper_path,
            mount_point: input.mount_point.clone(),
        }],
    });

    // Live-only: close old mapper before the resize -- mirrors the ordering
    // in cmd_replace, which runs the close before resize so a resize error
    // does not strand the old dm slot.
    if let ReplaceSource::Live { mapper, .. } = input.replace_source {
        steps.push(Step {
            risk: "safe",
            description: format!("cryptsetup close {}", mapper),
            commands: vec![CmdRequest::CryptsetupClose {
                mapper: mapper.0.clone(),
            }],
        });
    }

    // Shared: btrfs filesystem resize.
    steps.push(Step {
        risk: "safe",
        description: format!(
            "btrfs filesystem resize {}:max {}",
            devid, input.mount_point
        ),
        commands: vec![CmdRequest::BtrfsFilesystemResize {
            devid,
            mount_point: input.mount_point.clone(),
        }],
    });

    // Missing-only: restore RAID1 redundancy after the last missing device
    // clears. Live replace never creates single-profile chunks, so this
    // step is unconditionally absent on the Live path.
    if let ReplaceSource::Missing { .. } = input.replace_source
        && input.will_clear_last_missing
        && input.total_devices >= 2
    {
        steps.push(Step {
            risk: "long",
            description:
                "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft (restore redundancy)"
                    .into(),
            commands: vec![CmdRequest::BtrfsBalanceRaid1Soft {
                mount_point: input.mount_point.clone(),
            }],
        });
    }

    Ok(steps)
}

// ---------------------------------------------------------------------------
// Confirmation formatter
// ---------------------------------------------------------------------------

struct ReplaceConfirmOld<'a> {
    name: &'a str,
    hw: Option<&'a confirm::DiskHwInfo>,
    source: &'a ReplaceSource,
}

struct ReplaceConfirmNew<'a> {
    name: &'a str,
    by_id: &'a str,
    hw: &'a confirm::DiskHwInfo,
    needs_luks_format: bool,
    is_rebuild: bool,
}

fn format_replace_confirm(
    old: &ReplaceConfirmOld,
    new: &ReplaceConfirmNew,
    total_devices: u64,
) -> String {
    let mut msg = "Replace disk:\n".to_string();

    // Old disk
    match old.source {
        ReplaceSource::Live { devid, .. } => {
            let old_hw_line = old.hw.and_then(confirm::format_hw_info_line);
            if let Some(hw) = &old_hw_line {
                msg.push_str(&format!("  old: {}   {}\n", old.name, hw));
                msg.push_str(&format!(
                    "  {:width$}devid {} | will be replaced in-place\n",
                    "",
                    devid,
                    width = old.name.len() + 7,
                ));
            } else {
                msg.push_str(&format!(
                    "  old: {}   devid {} | will be replaced in-place\n",
                    old.name, devid
                ));
            }
        }
        ReplaceSource::Missing { devid } => {
            msg.push_str(&format!(
                "  old: {} (devid {})  missing -- no hardware info available\n",
                old.name, devid
            ));
        }
    }

    // New disk
    let new_hw_line = confirm::format_hw_info_line(new.hw);
    let indent = new.name.len() + 7; // "  new: " + name + "  "
    msg.push_str(&format!("  new: {}  {}\n", new.name, new.by_id));
    if let Some(hw) = &new_hw_line {
        msg.push_str(&format!("  {:width$}{}\n", "", hw, width = indent));
    }
    if new.needs_luks_format {
        msg.push_str(&format!(
            "  {:width$}Will be LUKS-formatted (existing data will be inaccessible)\n",
            "",
            width = indent,
        ));
    }
    if new.is_rebuild {
        msg.push_str(&format!(
            "  {:width$}Data will be rebuilt from RAID redundancy.\n",
            "",
            width = indent,
        ));
    }

    // Pool summary
    msg.push_str(&format!(
        "\nPool: {} {} -> {} {}\n",
        total_devices,
        if total_devices == 1 { "disk" } else { "disks" },
        total_devices,
        if total_devices == 1 { "disk" } else { "disks" },
    ));

    msg
}

fn build_replacement_membership(
    existing: &membership::PoolMembership,
    old_name: &str,
    new_name: &str,
    new_by_id: &ByIdPath,
    replace_source: &ReplaceSource,
) -> Result<membership::PoolMembership, ReplaceError> {
    let existing_member = existing.disks.get(old_name).ok_or_else(|| {
        ReplaceError::Validation(format!(
            "'{old_name}' not found in pool.json membership -- \
             no disk entry has this name. Pool membership may need manual repair."
        ))
    })?;

    if let ReplaceSource::Missing { devid } = replace_source
        && existing_member.devid != Some(*devid)
    {
        return Err(ReplaceError::Validation(format!(
            "--old '{old_name}' records devid {pool_devid:?} in pool.json, \
             but btrfs reports missing devid {devid}. \
             --old and --missing-id disagree about which member is being replaced.",
            pool_devid = existing_member.devid
        )));
    }

    let mut next = existing.clone();
    next.disks.remove(old_name);
    membership::validate_no_conflicts(&next, new_name, &new_by_id.0)
        .map_err(|e| ReplaceError::Validation(e.to_string()))?;
    next.disks.insert(
        new_name.to_owned(),
        membership::DiskMember::from_by_id(new_by_id.clone()),
    );
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::state_paths::StatePaths;

    fn test_paths() -> (tempfile::TempDir, StatePaths) {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        (tmp, paths)
    }

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".into())
    }

    struct PanicRunner;

    impl CommandRunner for PanicRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, crate::cmd::CmdError> {
            panic!("planner-boundary test: runner must not be invoked; got: {request:?}");
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, crate::cmd::CmdError> {
            panic!("planner-boundary test: runner must not be invoked; got: {request:?}");
        }
    }

    struct PanicFilesystem;

    impl Filesystem for PanicFilesystem {
        fn exists(&self, path: &str) -> bool {
            panic!("planner-boundary test: fs.exists must not be called; got: {path}");
        }

        fn is_block_device(&self, path: &str) -> bool {
            panic!("planner-boundary test: fs.is_block_device must not be called; got: {path}");
        }

        fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
            panic!("planner-boundary test: fs.list_dir must not be called; got: {path}");
        }

        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            panic!("planner-boundary test: fs.read_to_string must not be called; got: {path}");
        }
    }

    /*
     * Intent: source_has_io_errors pairs a stats row to the live source
     * by devid, even when the row's path differs from the canonical
     * /dev/mapper/braid-<name>. With non-zero counters on the matched
     * row, returns true; with zero counters or no devid match, false.
     *
     * Why it exists: the live-replace warning previously matched on
     * `target.as_path() == /dev/mapper/<mapper>`, which silently dropped
     * the warning whenever btrfs reported the row by an alternate path
     * (e.g. /dev/dm-N). Pairing by devid removes that identity weakness.
     * This test pins the new behavior so a future revert to path
     * matching cannot land silently.
     *
     * Scenario: stats row for devid 1 carries path "/dev/dm-1" (not
     * "/dev/mapper/disk1") and read_io_errs = 5; replace must still
     * recognize the I/O errors on the live source.
     */
    #[test]
    fn live_replace_detects_io_errors_when_stats_path_differs() {
        use crate::cmd::RawCommandOutput;
        let stats_raw = RawCommandOutput {
            cmd: "btrfs device stats".into(),
            stdout: r#"{"device-stats": [
                {"device": "/dev/dm-1", "devid": 1, "write_io_errs": 0, "read_io_errs": 5, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0},
                {"device": "/dev/dm-2", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
            ]}"#
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let stats = parse_btrfs_device_stats(&stats_raw).expect("parses");

        assert!(
            source_has_io_errors(&stats, 1),
            "devid 1 has read_io_errs=5; mismatched path must not hide it"
        );
        assert!(
            !source_has_io_errors(&stats, 2),
            "devid 2 has zero counters; must not report errors"
        );
        assert!(
            !source_has_io_errors(&stats, 99),
            "no row for devid 99; must not report errors"
        );
    }

    /* Intent: replace's stderr-note wrapper colors only bracketed
     * warning tags.
     * Why it exists: replace routes notes through a capture-aware
     * wrapper, so it needs direct coverage in addition to Preview's
     * shared renderer tests.
     * Scenario: an Info note followed by a Warn note is rendered in
     * plain and colored modes.
     */
    #[test]
    fn render_replace_notes_for_stderr_colors_only_warn_tag() {
        let notes = vec![
            PreviewNote::Info("waiting for in-flight device add".into()),
            PreviewNote::Warn("pool is mounted read-only".into()),
        ];

        let plain = render_replace_notes_for_stderr(&notes, false);
        assert_eq!(
            plain,
            "waiting for in-flight device add\n[warn] pool is mounted read-only\n"
        );

        let colored = render_replace_notes_for_stderr(&notes, true);
        assert_eq!(
            colored,
            "waiting for in-flight device add\n\x1b[33m[warn]\x1b[0m pool is mounted read-only\n"
        );
    }

    #[test]
    fn replace_confirm_warns_about_luks_format_for_non_luks_disk() {
        let new_hw = confirm::DiskHwInfo {
            model: Some("WD Elements".into()),
            serial: Some("5678EFGH".into()),
            size: Some(12_000_000_000_000),
        };
        let msg = format_replace_confirm(
            &ReplaceConfirmOld {
                name: "old1",
                hw: None,
                source: &ReplaceSource::Missing { devid: 2 },
            },
            &ReplaceConfirmNew {
                name: "new1",
                by_id: "/dev/disk/by-id/usb-WD_5678",
                hw: &new_hw,
                needs_luks_format: true,
                is_rebuild: true,
            },
            3,
        );
        assert!(msg.contains("LUKS-formatted"), "should mention LUKS-format");
        assert!(msg.contains("new1"), "should mention new disk name");
        assert!(
            msg.contains("/dev/disk/by-id/usb-WD_5678"),
            "should mention by-id"
        );
        assert!(
            msg.contains("inaccessible"),
            "should say data will be inaccessible"
        );
    }

    #[test]
    fn replace_confirm_missing_shows_rebuild_message() {
        let new_hw = confirm::DiskHwInfo::default();
        let msg = format_replace_confirm(
            &ReplaceConfirmOld {
                name: "old1",
                hw: None,
                source: &ReplaceSource::Missing { devid: 2 },
            },
            &ReplaceConfirmNew {
                name: "new1",
                by_id: "/dev/disk/by-id/usb-WD_5678",
                hw: &new_hw,
                needs_luks_format: false,
                is_rebuild: true,
            },
            3,
        );
        assert!(
            msg.contains("devid 2"),
            "should mention missing devid, got: {}",
            msg
        );
        assert!(
            msg.contains("missing"),
            "should indicate missing device, got: {}",
            msg
        );
        assert!(
            msg.contains("rebuilt from RAID redundancy"),
            "should mention rebuild, got: {}",
            msg
        );
        assert!(
            !msg.contains("LUKS-formatted"),
            "should not warn about formatting"
        );
    }

    fn two_device_pool() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName("braid-disk1".into()),
                    luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".into()),
                    devid: 1,
                    underlying: "/dev/vda".into(),
                },
                PoolDevice {
                    mapper: MapperName("braid-disk2".into()),
                    luks_uuid: LuksUuid("22222222-2222-2222-2222-222222222222".into()),
                    devid: 2,
                    underlying: "/dev/vdb".into(),
                },
            ],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 2,
            fsid: None,
            null_underlying: vec![],
        }
    }

    /// Create a mock runner that returns device usage output with specific
    /// missing devids (device_size == 0). Devid 1 is always present.
    fn mock_with_missing_devids(missing_devids: &[u64]) -> MockRunner {
        let mut output = String::new();
        output.push_str("/dev/mapper/braid-disk1, ID: 1\n");
        output.push_str("   Device size:           520093696\n");
        output.push_str("   Device slack:                  0\n");
        output.push_str("   Data,RAID1:            469762048\n");
        output.push_str("   Unallocated:            50331648\n\n");
        for &devid in missing_devids {
            output.push_str(&format!("<missing disk>, ID: {}\n", devid));
            output.push_str("   Device size:                  0\n");
            output.push_str("   Device slack:                  0\n");
            output.push_str("   Data,RAID1:            469762048\n");
            output.push_str("   Unallocated:                  0\n\n");
        }
        MockRunner::default().with_output(
            CmdRequest::BtrfsDeviceUsageRaw { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs device usage --raw /mnt/storage".into(),
                stdout: output,
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    #[test]
    // Intent: live old disk in healthy pool resolves to ReplaceSource::Live.
    // Why: core behavior -- replace must accept live disks when pool has no missing.
    // Scenario: operator swaps a slow-but-alive drive for a faster one.
    fn live_old_resolution_succeeds_no_missing() {
        let pool = two_device_pool();
        let runner = MockRunner::default();
        let mn = MapperName("braid-disk2".into());
        let result = resolve_replace_source(&runner, "disk2", &mn, None, &pool, &mp());
        assert!(
            matches!(result, Ok(ReplaceSource::Live { .. })),
            "expected Live target, got: {result:?}"
        );
    }

    #[test]
    // Intent: live old + --missing-id is rejected.
    // Why: --missing-id only makes sense for dead disks.
    // Scenario: operator passes --missing-id when old disk is still alive.
    fn live_old_with_missing_id_rejects() {
        let pool = two_device_pool();
        let runner = MockRunner::default();
        let mn = MapperName("braid-disk2".into());
        let err =
            resolve_replace_source(&runner, "disk2", &mn, Some(99), &pool, &mp()).unwrap_err();
        assert!(
            err.to_string().contains("--missing-id cannot be used"),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Intent: live old + pool has missing devices is rejected.
    // Why: mixed state (live + missing) is ambiguous and dangerous.
    // Scenario: operator tries live replace but a different disk has died.
    fn live_old_with_pool_missing_rejects() {
        let mut pool = two_device_pool();
        pool.missing_count = 1;
        pool.total_devices = 3;
        let runner = MockRunner::default();
        let mn = MapperName("braid-disk2".into());
        let err = resolve_replace_source(&runner, "disk2", &mn, None, &pool, &mp()).unwrap_err();
        assert!(
            err.to_string().contains("missing device"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("replace --missing-id"),
            "should suggest replace --missing-id: {err}"
        );
        assert!(
            !err.to_string().contains("remove-missing"),
            "should not suggest remove-missing: {err}"
        );
    }

    #[test]
    // Intent: replace must reject a post-replace membership that reuses another
    // member's by-id under a new name.
    // Why: docs and invariants say mutating commands reject name reassignment /
    // by-id rename rather than silently corrupting pool membership.
    // Scenario: operator tries `braid replace --old disk1 --new newname=<disk2 by-id>`.
    fn build_replacement_membership_rejects_by_id_rename_conflict() {
        let mut membership = membership::PoolMembership::empty();
        membership.disks.insert(
            "disk1".into(),
            membership::DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        membership.disks.insert(
            "disk2".into(),
            membership::DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );

        let err = build_replacement_membership(
            &membership,
            "disk1",
            "newname",
            &ByIdPath("/dev/disk/by-id/virtio-disk2".into()),
            &ReplaceSource::Live {
                mapper: MapperName("braid-disk1".into()),
                devid: 1,
            },
        )
        .expect_err("should reject by-id rename conflict");

        assert!(
            err.to_string().contains("cannot register"),
            "unexpected error: {err}"
        );
    }

    fn disk_member_with_devid(by_id: &str, devid: u64) -> membership::DiskMember {
        let mut m = membership::DiskMember::from_by_id(ByIdPath(by_id.into()));
        m.devid = Some(devid);
        m
    }

    #[test]
    // Intent: Missing-path build rejects when --old is absent from pool.json.
    // Why: silent HashMap::remove on a missing key previously produced orphan
    //   entries in pool.json on operator typo, which broke the next unlock
    //   via mount::plan_open_pool's Absent-member detection.
    // Scenario: operator types `braid replace --old disk2 --missing-id 2 --new ...`
    //   but pool.json only knows about disk1.
    fn build_replacement_membership_missing_rejects_absent_old_name() {
        let mut m = membership::PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk1", 1),
        );

        let result = build_replacement_membership(
            &m,
            "disk2",
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &ReplaceSource::Missing { devid: 2 },
        );

        assert!(
            matches!(result, Err(ReplaceError::Validation(_))),
            "expected Err(Validation(_)), got: {result:?}"
        );
    }

    #[test]
    // Intent: Missing-path build rejects when pool.json's devid for --old
    //   disagrees with the resolved missing devid.
    // Why: --old and --missing-id disagreeing silently would let the journal
    //   record one devid while pool.json describes another, leaving
    //   pool.json inconsistent with btrfs.
    // Scenario: operator runs --old disk2 --missing-id 2, but pool.json
    //   records disk2 with devid 3.
    fn build_replacement_membership_missing_rejects_devid_mismatch() {
        let mut m = membership::PoolMembership::empty();
        m.disks.insert(
            "disk2".into(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk2", 3),
        );

        let result = build_replacement_membership(
            &m,
            "disk2",
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &ReplaceSource::Missing { devid: 2 },
        );

        assert!(
            matches!(result, Err(ReplaceError::Validation(_))),
            "expected Err(Validation(_)), got: {result:?}"
        );
    }

    #[test]
    // Intent: Live-path build also rejects when --old is absent from pool.json.
    // Why: symmetric guard -- the silent .remove no-op applies to both paths;
    //   a Live-path typo would also leave an orphan btrfs member in pool.json.
    // Scenario: operator runs live replace with a typo in --old.
    fn build_replacement_membership_live_rejects_absent_old_name() {
        let mut m = membership::PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk1", 1),
        );

        let result = build_replacement_membership(
            &m,
            "disk2",
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &ReplaceSource::Live {
                mapper: MapperName("braid-disk2".into()),
                devid: 2,
            },
        );

        assert!(
            matches!(result, Err(ReplaceError::Validation(_))),
            "expected Err(Validation(_)), got: {result:?}"
        );
    }

    #[test]
    // Intent: Missing-path happy path returns Ok with the old entry removed
    //   and the new entry inserted.
    // Why: pins the positive branch so the rejection tests can't drift into
    //   false positives (e.g. a bug that rejects everything).
    // Scenario: operator replaces disk2 (missing devid 2) with disk3; pool.json
    //   has disk2 recorded with devid 2.
    fn build_replacement_membership_missing_happy_path() {
        let mut m = membership::PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk1", 1),
        );
        m.disks.insert(
            "disk2".into(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk2", 2),
        );

        let next = build_replacement_membership(
            &m,
            "disk2",
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &ReplaceSource::Missing { devid: 2 },
        )
        .expect("happy path");

        assert!(!next.disks.contains_key("disk2"));
        assert!(next.disks.contains_key("disk3"));
        assert!(next.disks.contains_key("disk1"));
    }

    #[test]
    // Intent: Live-path happy path returns Ok with the old entry removed and
    //   the new entry inserted. Devid cross-check does not apply.
    // Why: same rationale as the Missing-path happy path.
    // Scenario: operator swaps a live disk2 for a fresh disk3.
    fn build_replacement_membership_live_happy_path() {
        let mut m = membership::PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk1", 1),
        );
        m.disks.insert(
            "disk2".into(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk2", 2),
        );

        let next = build_replacement_membership(
            &m,
            "disk2",
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &ReplaceSource::Live {
                mapper: MapperName("braid-disk2".into()),
                devid: 2,
            },
        )
        .expect("happy path");

        assert!(!next.disks.contains_key("disk2"));
        assert!(next.disks.contains_key("disk3"));
    }

    #[test]
    // Intent: dry-run for live path shows btrfs replace and resize steps.
    // Why: operator should see what the live replace will do before committing.
    // Scenario: operator runs --dry-run to preview live replace.
    fn dry_run_live_path_shows_btrfs_replace() {
        let config_json = serde_json::json!({
            "disks": {
                "disk1": { "by_id": "/dev/disk/by-id/virtio-disk1" },
                "disk2": { "by_id": "/dev/disk/by-id/virtio-disk2" },
                "disk3": { "by_id": "/dev/disk/by-id/virtio-disk3" },
            },
            "mount_point": "/mnt/storage"
        });
        let _config: crate::config::Config =
            serde_json::from_value(config_json).expect("valid config");
        let new_probed = ConfigDisk {
            name: "disk3".into(),
            by_id_path: ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            state: ConfigDiskState::PresentNotLuks,
        };
        let source = ReplaceSource::Live {
            mapper: MapperName("braid-disk2".into()),
            devid: 2,
        };
        let steps = compile_replace_steps(&ReplaceStepsInput {
            new_name: "disk3",
            new_by_id: &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .unwrap();
        let descriptions: Vec<&str> = steps.iter().map(|s| s.description.as_str()).collect();
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("btrfs replace start")),
            "expected btrfs replace start step for live path, got: {descriptions:?}"
        );
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("btrfs filesystem resize")),
            "expected btrfs filesystem resize step for live path, got: {descriptions:?}"
        );
        assert!(
            !descriptions
                .iter()
                .any(|d| d.contains("btrfs device remove")),
            "live path should NOT show btrfs device remove, got: {descriptions:?}"
        );
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("cryptsetup close braid-disk2")),
            "expected LUKS close step for live path, got: {descriptions:?}"
        );
    }

    #[test]
    // Intent: dry-run for missing path shows btrfs replace start, not add/balance/remove.
    // Why: operator should see the unified replace path, not the old degraded balance path.
    // Scenario: operator runs --dry-run to preview dead-disk replace.
    fn dry_run_missing_path_shows_btrfs_replace() {
        let config_json = serde_json::json!({
            "disks": {
                "disk1": { "by_id": "/dev/disk/by-id/virtio-disk1" },
                "disk2": { "by_id": "/dev/disk/by-id/virtio-disk2" },
                "disk3": { "by_id": "/dev/disk/by-id/virtio-disk3" },
            },
            "mount_point": "/mnt/storage"
        });
        let _config: crate::config::Config =
            serde_json::from_value(config_json).expect("valid config");
        let new_probed = ConfigDisk {
            name: "disk3".into(),
            by_id_path: ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            state: ConfigDiskState::PresentNotLuks,
        };
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = compile_replace_steps(&ReplaceStepsInput {
            new_name: "disk3",
            new_by_id: &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: true,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .unwrap();
        let descriptions: Vec<&str> = steps.iter().map(|s| s.description.as_str()).collect();
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("btrfs replace start")),
            "expected btrfs replace start step for missing path, got: {descriptions:?}"
        );
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("btrfs filesystem resize")),
            "expected btrfs filesystem resize step for missing path, got: {descriptions:?}"
        );
        assert!(
            !descriptions.iter().any(|d| d.contains("btrfs device add")),
            "missing path should NOT show btrfs device add, got: {descriptions:?}"
        );
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("-dconvert=raid1,soft")),
            "missing path (clearing last missing, ≥2 devices) should show soft balance, got: {descriptions:?}"
        );
        assert!(
            !descriptions
                .iter()
                .any(|d| d.contains("btrfs device remove")),
            "missing path should NOT show btrfs device remove, got: {descriptions:?}"
        );
        assert!(
            !descriptions.iter().any(|d| d.contains("cryptsetup close")),
            "missing path should NOT show cryptsetup close (no old mapper), got: {descriptions:?}"
        );
    }

    #[test]
    // Intent: replacing with a disk that's already in the pool is rejected.
    // Why: without the guard, the Live path would pass an existing pool member
    //   to `btrfs replace start`. The btrfs replace path has no natural guard
    //   against this, so we need an explicit one.
    // Scenario: operator typo -- specifies an existing pool member as --new.
    fn new_disk_already_in_pool_rejected() {
        let pool = two_device_pool(); // has braid-disk1 and braid-disk2
        let new_mn = mapper_name("disk2"); // -> "braid-disk2"
        let err = check_new_not_in_pool("disk2", &new_mn, &pool).unwrap_err();
        assert!(
            err.to_string().contains("already a member"),
            "expected 'already a member' error, got: {err}"
        );
    }

    #[test]
    // Intent: a disk NOT in the pool passes the guard.
    // Why: regression -- the guard must not block valid replacements.
    // Scenario: normal replace with a fresh disk.
    fn new_disk_not_in_pool_passes() {
        let pool = two_device_pool();
        let new_mn = mapper_name("disk3");
        check_new_not_in_pool("disk3", &new_mn, &pool)
            .expect("disk3 is not in pool -- should pass");
    }

    #[test]
    // Intent: confirm text for live path does NOT say "dead".
    // Why: calling a live disk "dead" is confusing.
    // Scenario: operator sees confirmation prompt for live replace.
    fn replace_confirm_live_does_not_say_dead() {
        let old_hw = confirm::DiskHwInfo {
            model: Some("Toshiba MN07".into()),
            serial: None,
            size: Some(12_000_000_000_000),
        };
        let new_hw = confirm::DiskHwInfo::default();
        let msg = format_replace_confirm(
            &ReplaceConfirmOld {
                name: "disk2",
                hw: Some(&old_hw),
                source: &ReplaceSource::Live {
                    mapper: MapperName("braid-disk2".into()),
                    devid: 2,
                },
            },
            &ReplaceConfirmNew {
                name: "disk3",
                by_id: "/dev/disk/by-id/virtio-disk3",
                hw: &new_hw,
                needs_luks_format: false,
                is_rebuild: false,
            },
            3,
        );
        assert!(
            !msg.contains("dead"),
            "live replace prompt should not say 'dead', got: {msg}"
        );
        assert!(
            msg.contains("replaced in-place"),
            "expected in-place replace prompt, got: {msg}"
        );
    }

    #[test]
    // Intent: dead path resolution auto-detects the missing devid.
    // Why: when exactly one device is missing, the operator shouldn't need --missing-id.
    // Scenario: operator replaces a dead disk (1 missing device, no --missing-id).
    fn dead_old_resolution_single_missing() {
        let mut pool = two_device_pool();
        // Simulate disk2 missing
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let runner = mock_with_missing_devids(&[2]);
        let mn = MapperName("braid-disk2".into());
        let result = resolve_replace_source(&runner, "disk2", &mn, None, &pool, &mp());
        assert!(
            matches!(result, Ok(ReplaceSource::Missing { devid: 2 })),
            "expected Missing {{ devid: 2 }}, got: {result:?}"
        );
    }

    #[test]
    // Intent: dead path with explicit --missing-id resolves to that devid.
    // Why: regression guard for --missing-id path.
    // Scenario: operator passes --missing-id for a specific dead device.
    fn dead_old_resolution_with_devid() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let runner = mock_with_missing_devids(&[2]);
        let mn = MapperName("braid-disk2".into());
        let result = resolve_replace_source(&runner, "disk2", &mn, Some(2), &pool, &mp());
        assert!(
            matches!(result, Ok(ReplaceSource::Missing { devid: 2 })),
            "expected Missing {{ devid: 2 }}, got: {result:?}"
        );
    }

    #[test]
    // Intent: --missing-id pointing to a live device is rejected.
    // Why: the operator may have confused devids; replacing a live device
    //   via the missing path would corrupt data.
    // Scenario: operator passes --missing-id with the devid of a healthy disk.
    fn missing_id_pointing_to_live_device_rejected() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let runner = mock_with_missing_devids(&[2]);
        let mn = MapperName("braid-disk2".into());
        // Devid 1 is live (in pool.devices)
        let err = resolve_replace_source(&runner, "disk2", &mn, Some(1), &pool, &mp()).unwrap_err();
        assert!(
            err.to_string().contains("live device"),
            "expected 'live device' error, got: {err}"
        );
    }

    #[test]
    // Intent: --missing-id pointing to a nonexistent devid is rejected.
    // Why: a bogus devid would cause btrfs replace start to fail; catch it early.
    // Scenario: operator typos the devid.
    fn missing_id_nonexistent_devid_rejected() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let runner = mock_with_missing_devids(&[2]);
        let mn = MapperName("braid-disk2".into());
        let err =
            resolve_replace_source(&runner, "disk2", &mn, Some(99), &pool, &mp()).unwrap_err();
        assert!(
            err.to_string().contains("not a missing device"),
            "expected 'not a missing device' error, got: {err}"
        );
    }

    #[test]
    // Intent: multiple missing devices without --missing-id is rejected.
    // Why: auto-detect is ambiguous when multiple devices are missing.
    // Scenario: two drives died; operator must specify which to replace first.
    fn multiple_missing_without_id_rejected() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 2;
        pool.total_devices = 3;
        let runner = mock_with_missing_devids(&[2, 3]);
        let mn = MapperName("braid-disk2".into());
        let err = resolve_replace_source(&runner, "disk2", &mn, None, &pool, &mp()).unwrap_err();
        assert!(
            err.to_string().contains("multiple missing"),
            "expected 'multiple missing' error, got: {err}"
        );
    }

    fn make_replace_config() -> crate::config::Config {
        let config_json = serde_json::json!({
            "mount_point": "/mnt/storage"
        });
        serde_json::from_value(config_json).expect("valid config")
    }

    fn new_probed_not_luks() -> ConfigDisk {
        ConfigDisk {
            name: "disk3".into(),
            by_id_path: ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            state: ConfigDiskState::PresentNotLuks,
        }
    }

    // Intent: build_replace_journal_target records a fresh replacement disk
    // as FreshLuks with the generated braid label.
    // Why it exists: recovery relies on the journaled FreshLuks label and
    // effective luksFormat args to recognize an interrupted prepared target.
    // Scenario: replace plans against a present non-LUKS disk with an enroll
    // key file and extra luksFormat options.
    #[test]
    fn build_replace_journal_target_records_fresh_luks_target() {
        let new_by_id = ByIdPath("/dev/disk/by-id/virtio-disk3".into());
        let key_file = std::path::Path::new("/run/keys/braid-disk3.key");
        let extra_opts = vec!["--pbkdf".to_owned(), "pbkdf2".to_owned()];
        let new_probed = ConfigDisk {
            name: "disk3".into(),
            by_id_path: new_by_id.clone(),
            state: ConfigDiskState::PresentNotLuks,
        };

        let target = build_replace_journal_target(
            "disk3",
            &new_by_id,
            &new_probed,
            Some(key_file),
            &extra_opts,
        )
        .expect("fresh disk should build a replace journal target");

        assert_eq!(target.by_id, new_by_id);
        assert_eq!(target.mapper_name, "braid-disk3");
        match target.mode {
            journal::ReplaceJournalMode::FreshLuks {
                luks_label,
                luks_format_extra_opts,
                enroll_key_file,
            } => {
                assert_eq!(luks_label, "braid-disk3");
                assert_eq!(
                    luks_format_extra_opts,
                    vec!["--pbkdf", "pbkdf2", "--label", "braid-disk3"]
                );
                assert_eq!(enroll_key_file, Some(key_file.to_path_buf()));
            }
            other => panic!("expected FreshLuks journal target, got {other:?}"),
        }
    }

    // Intent: build_replace_journal_target records an already-LUKS
    // replacement disk as ExistingLuks with its UUID.
    // Why it exists: recovery must not run FreshLuks label matching or
    // keyfile/header-prep replay for a disk that was already LUKS.
    // Scenario: replace plans against a present LUKS disk whose mapper is not
    // yet open.
    #[test]
    fn build_replace_journal_target_records_existing_luks_target() {
        let new_by_id = ByIdPath("/dev/disk/by-id/virtio-disk3".into());
        let luks_uuid = LuksUuid("33333333-3333-3333-3333-333333333333".into());
        let new_probed = ConfigDisk {
            name: "disk3".into(),
            by_id_path: new_by_id.clone(),
            state: ConfigDiskState::PresentLuks {
                uuid: luks_uuid.clone(),
                mapper_open: false,
            },
        };

        let target = build_replace_journal_target(
            "disk3",
            &new_by_id,
            &new_probed,
            None,
            &["--label".to_owned(), "ignored".to_owned()],
        )
        .expect("existing LUKS disk should build a replace journal target");

        assert_eq!(target.by_id, new_by_id);
        assert_eq!(target.mapper_name, "braid-disk3");
        match target.mode {
            journal::ReplaceJournalMode::ExistingLuks { luks_uuid: got } => {
                assert_eq!(got, luks_uuid);
            }
            other => panic!("expected ExistingLuks journal target, got {other:?}"),
        }
    }

    // Intent: build_replace_journal_source preserves the live-source mapper
    // and devid in the journal.
    // Why it exists: recovery uses this identity to close only the replaced
    // old mapper and to distinguish live-source from missing-source cleanup.
    // Scenario: replace plans from an old disk that is still present in the
    // live btrfs pool.
    #[test]
    fn build_replace_journal_source_records_live_mapper() {
        let source = ReplaceSource::Live {
            mapper: MapperName("braid-disk2".into()),
            devid: 2,
        };

        let journal_source = build_replace_journal_source(&source);

        assert_eq!(
            journal_source,
            journal::ReplaceJournalSource::Live {
                old_devid: 2,
                old_mapper: MapperName("braid-disk2".into()),
            }
        );
    }

    // Intent: build_replace_journal_source preserves the missing-source
    // devid without inventing an old mapper.
    // Why it exists: missing-source recovery must not try to close a mapper
    // for a device that was already gone.
    // Scenario: replace plans by `--missing-id 2`.
    #[test]
    fn build_replace_journal_source_records_missing_devid() {
        let source = ReplaceSource::Missing { devid: 2 };

        let journal_source = build_replace_journal_source(&source);

        assert_eq!(
            journal_source,
            journal::ReplaceJournalSource::Missing { old_devid: 2 }
        );
    }

    #[test]
    // Intent: missing-path dry-run (not last missing) omits rebalance step.
    // Why: if other missing devices remain, a rebalance would be premature.
    // Scenario: 3-disk pool, 2 missing, replacing 1 -- still degraded after.
    fn dry_run_missing_not_last_omits_rebalance() {
        let _config = make_replace_config();
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = compile_replace_steps(&ReplaceStepsInput {
            new_name: "disk3",
            new_by_id: &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 3,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .unwrap();
        let descriptions: Vec<&str> = steps.iter().map(|s| s.description.as_str()).collect();
        assert!(
            !descriptions
                .iter()
                .any(|d| d.contains("-dconvert=raid1,soft")),
            "should NOT show soft balance when not clearing last missing, got: {descriptions:?}"
        );
    }

    #[test]
    // Intent: missing-path dry-run with total_devices == 1 omits rebalance.
    // Why: can't have RAID1 with 1 device.
    // Scenario: single-device pool with a missing ghost entry.
    fn dry_run_missing_single_device_omits_rebalance() {
        let _config = make_replace_config();
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = compile_replace_steps(&ReplaceStepsInput {
            new_name: "disk3",
            new_by_id: &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: true,
            total_devices: 1,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .unwrap();
        let descriptions: Vec<&str> = steps.iter().map(|s| s.description.as_str()).collect();
        assert!(
            !descriptions
                .iter()
                .any(|d| d.contains("-dconvert=raid1,soft")),
            "should NOT show soft balance with total_devices == 1, got: {descriptions:?}"
        );
    }

    use crate::cmd::{CmdError, CommandRunner as CmdRunner2};
    use crate::membership::{self, DiskMember, PoolMembership};

    fn mock_ok(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    /// Mock filesystem where specific paths exist.
    struct ReplaceMockFs(Vec<String>);
    impl crate::probe::Filesystem for ReplaceMockFs {
        fn exists(&self, path: &str) -> bool {
            self.0.iter().any(|p| p == path)
        }
        fn is_block_device(&self, _path: &str) -> bool {
            false
        }
        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path == "/proc/self/mountinfo" {
                Ok(
                    "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/braid-disk1 rw\n"
                        .to_owned(),
                )
            } else if path.ends_with("/exclusive_operation") {
                Ok("none\n".to_owned())
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
            }
        }
        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    /// Runner for live replace that fails on BtrfsReplaceStart.
    /// Handles all preflight/probe commands successfully.
    struct FailingReplaceRunner;

    impl CmdRunner2 for FailingReplaceRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n",
                )),
                CmdRequest::CryptsetupStatus { mapper } => {
                    let dev = match mapper.as_str() {
                        "braid-disk1" => "/dev/vdb",
                        "braid-disk2" => "/dev/vdc",
                        "braid-disk3" => "/dev/vdd",
                        _ => "/dev/vdz",
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"
                        ),
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = match device.as_str() {
                        "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => {
                            "11111111-1111-1111-1111-111111111111"
                        }
                        "/dev/vdc" | "/dev/disk/by-id/virtio-disk2" => {
                            "22222222-2222-2222-2222-222222222222"
                        }
                        // new disk: its backing via the braid-disk3 mapper is /dev/vdd
                        "/dev/vdd" | "/dev/disk/by-id/virtio-disk3" => {
                            "33333333-3333-3333-3333-333333333333"
                        }
                        _ => "99999999-9999-9999-9999-999999999999",
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{uuid}\n"),
                    ))
                }
                CmdRequest::CryptsetupLuksDumpText { device } => Ok(mock_ok(
                    &format!("cryptsetup luksDump {device}"),
                    "LUKS header information\nVersion:       \t2\n",
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
                CmdRequest::BtrfsDeviceStatsJson { .. } => {
                    Ok(mock_ok("btrfs device stats", r#"{"device-stats": []}"#))
                }
                CmdRequest::BtrfsReplaceStart { .. } => Ok(RawCommandOutput {
                    cmd: "btrfs replace start".into(),
                    stdout: String::new(),
                    stderr: "ERROR: target device is too small".into(),
                    exit_status: 1,
                }),
                CmdRequest::CryptsetupTestPassphrase { device } => Ok(mock_ok(
                    &format!("cryptsetup open --test-passphrase {device}"),
                    "",
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

    #[test]
    // Intent: pending-op.json survives when btrfs replace start fails.
    //
    // Why it exists: JournalGuard previously cleared the journal on any exit,
    //   including error returns. After LUKS init on the new disk, a failed
    //   btrfs replace would leave pool.json stale with no recovery path.
    //
    // Scenario: live replace, new disk already LUKS-open, btrfs replace start
    //   fails (e.g. target too small). Journal must persist for recovery.
    fn journal_survives_replace_failure() {
        // Set up state
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        // Passphrase file (required by cmd_replace)
        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        // Filesystem mock: new disk and its mapper exist
        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let runner = FailingReplaceRunner;
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        assert!(
            result.is_err(),
            "replace should fail when btrfs replace fails"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
        // The journal exists, which proves we got past journal::write_journal,
        // which proves the inhibitor was acquired exactly once on the way in.
        // Locks in the seam placement: if a refactor moves the acquire to a
        // post-journal point or skips it entirely, this assert flips.
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the path through journal::write_journal"
        );
    }

    #[test]
    // Intent: cmd_replace rejects --old == --new (post-parse) with a
    //   Validation error, on the reversible side of the inhibitor/journal
    //   seam.
    //
    // Why it exists: the old==new guard at replace.rs:94-98 is a
    //   user-visible CLI contract (operator typo protection). It fires
    //   before probe_config_disk's mapper-conflict detection would
    //   otherwise surface the same bug as a confusing MapperConflict
    //   probe error. Without direct cmd-level coverage, a refactor that
    //   drops the guard would change the rejection variant from
    //   Validation("must be different") to Probe(MapperConflict), and a
    //   refactor that moved the guard past the inhibitor/journal seam
    //   would strand a pending-op.json and a held logind inhibitor on
    //   what is conceptually a preflight rejection. Replaces a prior
    //   tautological test (assert_eq!("disk1", "disk1", ...)) that
    //   exercised no production code.
    //
    // Scenario: operator runs
    //   `braid replace --old disk1 --new disk1=/dev/disk/by-id/virtio-disk3`
    //   -- same name on both sides after parsing the new-name spec.
    fn cmd_replace_rejects_old_equals_new() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let runner = FailingReplaceRunner;
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk1",
                new_name: "disk1=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        match &result {
            Err(ReplaceError::Validation(msg)) => {
                assert!(
                    msg.contains("must be different"),
                    "expected old==new guard message, got: {msg}"
                );
            }
            other => panic!("expected Err(ReplaceError::Validation), got: {other:?}"),
        }
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "old==new typo must be caught before the inhibitor seam -- a caught typo must not hold logind"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "no journal may be written when old==new"
        );
    }

    #[test]
    // Intent: --dry-run must not acquire the sleep inhibitor.
    //
    // Why it exists: dry-run takes no irreversible action and never reaches
    //   the irreversible section that the inhibitor is meant to protect. If
    //   acquisition leaks into the dry-run path it would spawn systemd-inhibit
    //   for nothing -- wasteful and a UX surprise (operators do not expect
    //   --dry-run to require logind).
    //
    // Scenario: operator runs `braid replace --old disk2 --new disk3=... --dry-run`
    //   to preview the plan. cmd_replace must short-circuit at the dry-run
    //   branch before the inhibitor seam fires.
    fn dry_run_does_not_acquire_inhibitor() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let runner = FailingReplaceRunner;
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        assert!(result.is_ok(), "dry-run should succeed: {result:?}");
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "dry-run must NOT acquire the sleep inhibitor -- it has no irreversible work to protect"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "dry-run must not write the journal"
        );
    }

    /// Runner for a live replace where btrfs replace + cryptsetup close
    /// succeed but the post-replace `btrfs filesystem resize` fails.
    /// Records every request so the test can assert that the close ran
    /// BEFORE the failing resize (regression for the Live arm ordering
    /// bug where a resize `?` would skip the close).
    struct ResizeFailingLoggingRunner {
        log: std::sync::Arc<std::sync::Mutex<Vec<CmdRequest>>>,
        // Flips when BtrfsReplaceStart returns success so subsequent
        // BtrfsFilesystemShow probes report the post-replace topology.
        // The post-replace early `save_membership` call probes here
        // before persisting, and the test asserts the new disk is
        // enriched -- which requires the probe to see disk3, not disk2.
        replace_done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl CmdRunner2 for ResizeFailingLoggingRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());
            match request {
                CmdRequest::BtrfsFilesystemShow { mount_point } => {
                    let show = if self.replace_done.load(std::sync::atomic::Ordering::Relaxed) {
                        // post-replace: disk1 + disk3 (devid 2 reassigned to disk3)
                        "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk3\n"
                    } else {
                        // pre-replace: disk1 + disk2
                        "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n"
                    };
                    Ok(mock_ok(
                        &format!("btrfs filesystem show {mount_point}"),
                        show,
                    ))
                }
                CmdRequest::CryptsetupStatus { mapper } => {
                    let dev = match mapper.as_str() {
                        "braid-disk1" => "/dev/vdb",
                        "braid-disk2" => "/dev/vdc",
                        "braid-disk3" => "/dev/vdd",
                        _ => "/dev/vdz",
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"
                        ),
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = match device.as_str() {
                        "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => {
                            "11111111-1111-1111-1111-111111111111"
                        }
                        "/dev/vdc" | "/dev/disk/by-id/virtio-disk2" => {
                            "22222222-2222-2222-2222-222222222222"
                        }
                        "/dev/vdd" | "/dev/disk/by-id/virtio-disk3" => {
                            "33333333-3333-3333-3333-333333333333"
                        }
                        _ => "99999999-9999-9999-9999-999999999999",
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{uuid}\n"),
                    ))
                }
                CmdRequest::CryptsetupLuksDumpText { device } => Ok(mock_ok(
                    &format!("cryptsetup luksDump {device}"),
                    "LUKS header information\nVersion:       \t2\n",
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
                CmdRequest::BtrfsDeviceStatsJson { .. } => {
                    Ok(mock_ok("btrfs device stats", r#"{"device-stats": []}"#))
                }
                CmdRequest::BtrfsReplaceStart { .. } => {
                    self.replace_done
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(mock_ok("btrfs replace start", ""))
                }
                CmdRequest::CryptsetupClose { .. } => Ok(mock_ok("cryptsetup close", "")),
                CmdRequest::BtrfsFilesystemResize { .. } => Ok(RawCommandOutput {
                    cmd: "btrfs filesystem resize".into(),
                    stdout: String::new(),
                    stderr: "ERROR: unable to resize".into(),
                    exit_status: 1,
                }),
                CmdRequest::CryptsetupTestPassphrase { device } => Ok(mock_ok(
                    &format!("cryptsetup open --test-passphrase {device}"),
                    "",
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

    #[test]
    // Intent: close of old mapper must run even when the post-replace
    //   `btrfs filesystem resize` fails.
    //
    // Why it exists: a resize failure returning `?` previously skipped the
    //   best-effort cryptsetup close of the old mapper, leaving the old
    //   dm slot bound to its backing disk until the next `braid lock` or
    //   reboot. The ordering in the Live arm of cmd_replace must be
    //   close-then-resize so the close always runs.
    //
    // Scenario: live replace of disk2 -> disk3. `btrfs replace start`
    //   succeeds; `btrfs filesystem resize devid=2:max` fails (exit 1);
    //   cmd_replace must still have issued `cryptsetup close braid-disk2`
    //   before the resize error propagated out.
    fn close_runs_before_resize_on_live_replace() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let replace_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let runner = ResizeFailingLoggingRunner {
            log: log.clone(),
            replace_done: replace_done.clone(),
        };
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        match &result {
            Err(ReplaceError::Pool(crate::pool::PoolError::Failed(msg))) => {
                assert!(
                    msg.contains("btrfs filesystem resize failed"),
                    "expected typed PoolError::Failed carrying resize message, got: {msg}"
                );
            }
            other => {
                panic!("expected Err(ReplaceError::Pool(PoolError::Failed(..))), got: {other:?}")
            }
        }

        let log = log.lock().unwrap();
        let close_idx = log
            .iter()
            .position(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupClose { mapper } if mapper == "braid-disk2"
                )
            })
            .expect("cryptsetup close on braid-disk2 must be issued even when resize fails");
        let resize_idx = log
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsFilesystemResize { devid: 2, .. }))
            .expect("btrfs filesystem resize on devid 2 must be issued");
        assert!(
            close_idx < resize_idx,
            "close (index {close_idx}) must run BEFORE resize (index {resize_idx}) \
             so a resize failure does not strand the old dm slot"
        );

        assert!(
            journal::load_journal(&paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
        let journal = journal::load_journal(&paths)
            .unwrap()
            .expect("journal should remain after post-replace resize failure");
        assert!(
            matches!(
                journal.op,
                journal::OpKind::Replace {
                    phase: journal::ReplacePhase::PostReplaceMaintenance,
                    ..
                }
            ),
            "journal should advance after btrfs replace commits: {:?}",
            journal.op
        );

        // Membership commits at btrfs replace start; pool.json must reflect
        // the new topology even when the post-replace resize fails.
        let saved = membership::load_membership(&paths)
            .expect("pool.json must exist after the membership commit");
        assert!(
            !saved.disks.contains_key("disk2"),
            "old disk must be gone from pool.json once btrfs replace succeeds, \
             even when the post-replace resize fails (saved: {:?})",
            saved.disks.keys().collect::<Vec<_>>()
        );
        let disk3 = saved.disks.get("disk3").unwrap_or_else(|| {
            panic!(
                "new disk must be in pool.json once btrfs replace succeeds (saved: {:?})",
                saved.disks.keys().collect::<Vec<_>>()
            )
        });
        assert!(
            disk3.luks_uuid.is_some() && disk3.devid.is_some() && disk3.added_at.is_some(),
            "new disk must carry enriched metadata (luks_uuid, devid, added_at) \
             from the post-replace probe: {disk3:?}"
        );
    }

    /// Runner for a live replace where every step succeeds except the
    /// best-effort `cryptsetup close` of the old mapper, which returns
    /// non-zero. Mirrors `ResizeFailingLoggingRunner` so the rest of the
    /// flow reaches the close site.
    struct CloseFailingReplaceRunner {
        replace_done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl CmdRunner2 for CloseFailingReplaceRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsFilesystemShow { mount_point } => {
                    let show = if self.replace_done.load(std::sync::atomic::Ordering::Relaxed) {
                        "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk3\n"
                    } else {
                        "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n"
                    };
                    Ok(mock_ok(
                        &format!("btrfs filesystem show {mount_point}"),
                        show,
                    ))
                }
                CmdRequest::CryptsetupStatus { mapper } => {
                    let dev = match mapper.as_str() {
                        "braid-disk1" => "/dev/vdb",
                        "braid-disk2" => "/dev/vdc",
                        "braid-disk3" => "/dev/vdd",
                        _ => "/dev/vdz",
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"
                        ),
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = match device.as_str() {
                        "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => {
                            "11111111-1111-1111-1111-111111111111"
                        }
                        "/dev/vdc" | "/dev/disk/by-id/virtio-disk2" => {
                            "22222222-2222-2222-2222-222222222222"
                        }
                        "/dev/vdd" | "/dev/disk/by-id/virtio-disk3" => {
                            "33333333-3333-3333-3333-333333333333"
                        }
                        _ => "99999999-9999-9999-9999-999999999999",
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{uuid}\n"),
                    ))
                }
                CmdRequest::CryptsetupLuksDumpText { device } => Ok(mock_ok(
                    &format!("cryptsetup luksDump {device}"),
                    "LUKS header information\nVersion:       \t2\n",
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
                CmdRequest::BtrfsDeviceStatsJson { .. } => {
                    Ok(mock_ok("btrfs device stats", r#"{"device-stats": []}"#))
                }
                CmdRequest::BtrfsReplaceStart { .. } => {
                    self.replace_done
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(mock_ok("btrfs replace start", ""))
                }
                CmdRequest::CryptsetupClose { mapper } if mapper == "braid-disk2" => {
                    Ok(RawCommandOutput {
                        cmd: "cryptsetup close".into(),
                        stdout: String::new(),
                        stderr: "device is busy".into(),
                        exit_status: 5,
                    })
                }
                CmdRequest::CryptsetupClose { .. } => Ok(mock_ok("cryptsetup close", "")),
                CmdRequest::BtrfsFilesystemResize { .. } => {
                    Ok(mock_ok("btrfs filesystem resize", ""))
                }
                CmdRequest::CryptsetupTestPassphrase { device } => Ok(mock_ok(
                    &format!("cryptsetup open --test-passphrase {device}"),
                    "",
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

    /* Intent: live-replace's best-effort close of the old mapper closes its
     * [wait] row with [warn] when cryptsetup returns non-zero exit.
     * Why it exists: Principle 13 forbids dangling [wait] rows; a best-effort
     * close that exits the command 0 must still announce the failure on the
     * same subject so the wait window is closed for the operator.
     * Scenario: live replace of disk2 -> disk3 succeeds end-to-end except the
     * trailing cryptsetup close of the old mapper, which returns busy.
     */
    #[test]
    fn live_replace_old_close_failure_emits_warn_row() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let replace_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let runner = CloseFailingReplaceRunner {
            replace_done: replace_done.clone(),
        };
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            let result = cmd_replace(
                &runner,
                &fs,
                &ReplaceParams {
                    config_path: &config_path,
                    old_name: "disk2",
                    new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                    missing_id: None,
                    dry_run: false,
                    yes: true,
                    passphrase_stdin: false,
                    passphrase_file: Some(pass_path.as_path()),
                    enroll_key_file: None,
                    luks_format_extra_opts: &[],
                    progress: crate::progress::ProgressOutput::Off,
                    paths: &paths,
                    sleep_inhibitor: &inhibitor,
                },
            );
            assert!(
                result.is_ok(),
                "best-effort close failure must not fail the replace command, got: {result:?}"
            );
        });

        let wait = "[wait] disk disk2: locking...";
        let warn = "[warn] disk disk2: lock failed (exit 5)";
        assert!(captured.contains(wait), "missing wait row: {captured:?}");
        assert!(captured.contains(warn), "missing warn row: {captured:?}");
        assert!(
            captured.find(wait) < captured.find(warn),
            "wait must precede warn, got: {captured:?}"
        );
    }

    /// Runner for a missing-path replace where --old is a typo'd name absent
    /// from pool.json. probe_pool sees 1 live disk + 1 missing devid (devid 2);
    /// probe_missing_devids reports [2]. The runner is scoped narrowly so
    /// cmd_replace can reach the `build_replacement_membership` guard before
    /// touching any downstream commands.
    struct MissingPathReplaceRunner;

    impl CmdRunner2 for MissingPathReplaceRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
                     \tTotal devices 2 FS bytes used 16.17MiB\n\
                     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
                     \t*** Some devices missing\n",
                )),
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_ok(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vda\n  mode:    read/write\n"
                    ),
                )),
                CmdRequest::CryptsetupLuksUuid { device } => Ok(mock_ok(
                    &format!("cryptsetup luksUUID {device}"),
                    "11111111-1111-1111-1111-111111111111\n",
                )),
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(mock_ok(
                    "btrfs device usage --raw",
                    "/dev/mapper/braid-disk1, ID: 1\n\
                     \tDevice size:           520093696\n\
                     \tDevice slack:                  0\n\
                     \tData,RAID1:            469762048\n\
                     \tUnallocated:            50331648\n\n\
                     <missing disk>, ID: 2\n\
                     \tDevice size:                  0\n\
                     \tDevice slack:                  0\n\
                     \tData,RAID1:            469762048\n\
                     \tUnallocated:                  0\n\n",
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
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

    #[test]
    // Intent: cmd_replace's missing path rejects a --old name that is absent
    //   from pool.json, with no inhibitor acquired and no journal written.
    //
    // Why it exists: resolve_replace_source only consulted btrfs state, so a
    //   typo in --old on the missing path slipped through and
    //   build_replacement_membership's HashMap::remove silently no-oped before
    //   inserting the new name. pool.json kept the orphan old entry, and the
    //   next `braid unlock` tripped DegradedRefused in mount::plan_open_pool.
    //
    // Scenario: pool has 1 live disk (disk1, devid 1) and 1 missing
    //   (devid 2). pool.json only records disk1. Operator runs
    //   `braid replace --old disk2 --missing-id 2 --new disk3=...`. The guard
    //   must fire before the inhibitor seam at the "reversible preflight
    //   before inhibitor" boundary (cli/src/replace.rs:224-229).
    fn cmd_replace_missing_path_rejects_old_name_absent_from_membership() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());

        // Seed pool.json with ONLY disk1 -- no disk2 entry. This is the typo
        // scenario: btrfs knows devid 2 is missing, but pool.json does not
        // record any member named "disk2".
        let mut m = PoolMembership::empty();
        let mut disk1 = DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into()));
        disk1.devid = Some(1);
        m.disks.insert("disk1".into(), disk1);
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let runner = MissingPathReplaceRunner;
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: Some(2),
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        assert!(
            matches!(result, Err(ReplaceError::Validation(_))),
            "expected Err(ReplaceError::Validation(_)) for --old absent from pool.json, got: {result:?}"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "validation must fire before the inhibitor seam -- a caught typo must not hold logind"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "no journal may be written when --old is absent from pool.json"
        );
    }

    #[test]
    // Intent: live-path dry-run still shows NO soft balance step.
    // Why: live replace doesn't create single-profile chunks -- no degraded mode involved.
    // Scenario: swapping a working drive for a bigger one.
    fn dry_run_live_path_no_soft_balance() {
        let _config = make_replace_config();
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Live {
            mapper: MapperName("braid-disk2".into()),
            devid: 2,
        };
        let steps = compile_replace_steps(&ReplaceStepsInput {
            new_name: "disk3",
            new_by_id: &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .unwrap();
        let descriptions: Vec<&str> = steps.iter().map(|s| s.description.as_str()).collect();
        assert!(
            !descriptions
                .iter()
                .any(|d| d.contains("-dconvert=raid1,soft")),
            "live path should NOT show soft balance, got: {descriptions:?}"
        );
    }

    #[test]
    // Intent: dry-run for fresh-disk live replace shows full LUKS init + replace commands.
    // Why: verifies header backup and keyfile enrollment appear in dry-run.
    // Scenario: replacing disk2 with a fresh disk3, with keyfile enrollment.
    fn dry_run_render_fresh_disk_live_replace_with_keyfile() {
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Live {
            mapper: MapperName("braid-disk2".into()),
            devid: 2,
        };
        let kf = Path::new("/mnt/usb/braid.key");
        let luks_format_extra_opts = vec![
            "--pbkdf".to_owned(),
            "pbkdf2".to_owned(),
            "--iter-time".to_owned(),
            "1".to_owned(),
        ];
        let steps = compile_replace_steps(&ReplaceStepsInput {
            new_name: "disk3",
            new_by_id: &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: Some(kf),
            luks_format_extra_opts: &luks_format_extra_opts,
        })
        .unwrap();
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // Steps: LUKS format, keyfile enroll, header backup, LUKS open,
        //        replace start, close old, resize = 7 steps x 2 lines each = 14
        // Header backup runs after the final keyslot mutation so the backup
        // captures slot 1; ordering invariant is format < addKey < backup < open.
        assert_eq!(lines.len(), 14, "expected 14 lines, got:\n{output}");

        // LUKS format
        assert!(lines[0].contains("[destructive]"));
        assert!(lines[1].contains("$ cryptsetup luksFormat"));
        assert!(lines[1].contains("--pbkdf pbkdf2 --iter-time 1"));
        assert!(lines[1].contains("--label braid-disk3"));

        // Keyfile enrollment (runs before backup so slot 1 lands in the backup)
        assert!(lines[2].contains("enroll keyfile"));
        assert!(lines[3].contains("$ cryptsetup luksAddKey"));
        assert!(lines[3].contains("/mnt/usb/braid.key"));

        // Header backup
        assert!(lines[4].contains("LUKS header backup"));
        assert!(lines[5].contains("$ cryptsetup luksHeaderBackup"));

        // LUKS open
        assert!(lines[6].contains("LUKS open"));
        assert!(lines[7].contains("$ cryptsetup open --type luks"));

        // Replace start
        assert!(lines[8].contains("[long       ]"));
        assert!(lines[9].contains("$ btrfs replace start"));

        // Close old mapper (before resize: a resize failure must not strand
        // the old dm slot)
        assert!(lines[10].contains("cryptsetup close"));
        assert_eq!(lines[11], "               $ cryptsetup close braid-disk2");

        // Resize
        assert!(lines[12].contains("btrfs filesystem resize"));
    }

    #[test]
    // Intent: dry-run for a fresh-disk missing-path replace renders the
    //   expected step ordering: LUKS init of the new disk, then
    //   `btrfs replace start`, then `btrfs filesystem resize`, then the
    //   post-replace soft balance. No `cryptsetup close` step -- the missing
    //   path has no old mapper.
    //
    // Why it exists: the live-path render order is pinned by
    //   `dry_run_render_fresh_disk_live_replace_with_keyfile`, but the
    //   missing path only had presence/absence coverage. A regression that
    //   moved the soft balance before `btrfs replace start`/`resize` would
    //   ship broken dry-run output without tripping the existing test. This
    //   test fails if the order breaks even when every substring is still
    //   present.
    //
    // Scenario: operator replaces a missing disk with a fresh disk3. The
    //   pool has 2 devices and this clears the last missing one, so the
    //   soft-balance tail appears.
    fn dry_run_render_missing_path_ordering() {
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = compile_replace_steps(&ReplaceStepsInput {
            new_name: "disk3",
            new_by_id: &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: true,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .unwrap();
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // Substring order: LUKS format -> header backup -> LUKS open ->
        // btrfs replace start -> btrfs filesystem resize -> soft balance.
        // Pin the order by resolving each substring to an index and
        // asserting strict monotonic increase.
        let find = |needle: &str| -> usize {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("expected '{needle}' in dry-run output:\n{output}"))
        };
        let luks_format = find("$ cryptsetup luksFormat");
        let header_backup = find("$ cryptsetup luksHeaderBackup");
        let luks_open = find("$ cryptsetup open --type luks");
        let replace_start = find("$ btrfs replace start");
        let resize = find("btrfs filesystem resize");
        let soft_balance = find("-dconvert=raid1,soft");

        assert!(
            luks_format < header_backup
                && header_backup < luks_open
                && luks_open < replace_start
                && replace_start < resize
                && resize < soft_balance,
            "missing-path dry-run step ordering violated \
             (format={luks_format}, header_backup={header_backup}, \
             luks_open={luks_open}, replace_start={replace_start}, \
             resize={resize}, soft_balance={soft_balance}):\n{output}"
        );

        // Missing path has no old mapper, so no cryptsetup close anywhere.
        assert!(
            !output.contains("cryptsetup close"),
            "missing path must not render a cryptsetup close step:\n{output}"
        );
    }

    /// Runner for a replace where the new disk is already LUKS-formatted but
    /// the mapper is closed (PresentLuks { mapper_open: false }) and the
    /// supplied passphrase is wrong: CryptsetupTestPassphrase on the new
    /// disk's by_id returns exit 2 (EPERM). Everything else mirrors
    /// FailingReplaceRunner except that braid-disk3's mapper is inactive,
    /// so probe_config_disk reports mapper_open: false.
    struct ClosedLuksWrongPassRunner;

    impl CmdRunner2 for ClosedLuksWrongPassRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n",
                )),
                CmdRequest::CryptsetupStatus { mapper } => match mapper.as_str() {
                    "braid-disk1" => Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                        ),
                    )),
                    "braid-disk2" => Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdc\n  mode:    read/write\n"
                        ),
                    )),
                    // new disk's mapper is closed -- this is the key
                    // difference vs FailingReplaceRunner.
                    "braid-disk3" => Ok(RawCommandOutput {
                        cmd: format!("cryptsetup status {mapper}"),
                        stdout: String::new(),
                        stderr: format!("/dev/mapper/{mapper} is inactive.\n"),
                        exit_status: 4,
                    }),
                    _ => Err(CmdError::MissingMock),
                },
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = match device.as_str() {
                        "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => {
                            "11111111-1111-1111-1111-111111111111"
                        }
                        "/dev/vdc" | "/dev/disk/by-id/virtio-disk2" => {
                            "22222222-2222-2222-2222-222222222222"
                        }
                        "/dev/disk/by-id/virtio-disk3" => "33333333-3333-3333-3333-333333333333",
                        _ => "99999999-9999-9999-9999-999999999999",
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{uuid}\n"),
                    ))
                }
                CmdRequest::CryptsetupLuksDumpText { device } => Ok(mock_ok(
                    &format!("cryptsetup luksDump {device}"),
                    "LUKS header information\nVersion:       \t2\n",
                )),
                CmdRequest::CryptsetupTestPassphrase { device } => {
                    if device == "/dev/disk/by-id/virtio-disk3" {
                        Ok(RawCommandOutput {
                            cmd: format!("cryptsetup luksOpen --test-passphrase {device}"),
                            stdout: String::new(),
                            stderr: "No key available with this passphrase.\n".into(),
                            exit_status: 2,
                        })
                    } else {
                        Ok(mock_ok(
                            &format!("cryptsetup luksOpen --test-passphrase {device}"),
                            "",
                        ))
                    }
                }
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
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

    #[test]
    // Intent: wrong passphrase on a PresentLuks { mapper_open: false } new
    //   disk must fail before the journal is written.
    //
    // Why it exists: the closed-LUKS replacement path previously deferred
    //   passphrase verification to the post-journal ensure_luks_open call,
    //   so a wrong passphrase stranded pending-op.json and forced the user
    //   into braid recover for a pure preflight failure -- contradicting
    //   decision 019's "logind failure aborts cleanly without stranding
    //   pending-op.json...for a preflight failure" guidance. Re-introducing
    //   that ordering must flip this assertion.
    //
    // Scenario: operator runs `braid replace --old disk2 --new disk3=...`
    //   where disk3 is already LUKS-formatted (mapper closed) and types the
    //   wrong passphrase. The command must abort cleanly: no journal, no
    //   inhibitor acquired, Err(Validation).
    fn wrong_passphrase_on_closed_luks_new_disk_does_not_write_journal() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"wrong-passphrase\n").unwrap();

        // Only the new disk's by_id exists. /dev/mapper/braid-disk3 is
        // absent because the mapper is closed.
        let fs = ReplaceMockFs(vec!["/dev/disk/by-id/virtio-disk3".into()]);

        let runner = ClosedLuksWrongPassRunner;
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        assert!(
            matches!(result, Err(ReplaceError::Validation(_))),
            "expected Err(ReplaceError::Validation(_)) for wrong passphrase on a closed-LUKS new disk, got: {result:?}"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "pending-op.json must not be written -- wrong passphrase is a reversible preflight failure"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "sleep inhibitor must not be acquired before passphrase verification"
        );
    }

    /// Recording wrapper around FailingReplaceRunner that logs every
    /// CmdRequest before dispatching. Used by the mapper_open: true test to
    /// assert the new disk is verified but not opened again.
    struct RecordingReplaceRunner {
        inner: FailingReplaceRunner,
        log: std::sync::Arc<std::sync::Mutex<Vec<CmdRequest>>>,
    }

    impl RecordingReplaceRunner {
        fn new() -> Self {
            Self {
                inner: FailingReplaceRunner,
                log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }

    impl CmdRunner2 for RecordingReplaceRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());
            self.inner.run(request)
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());
            self.inner.run_with_stdin(request, stdin)
        }
    }

    #[test]
    // Intent: when the new disk is PresentLuks { mapper_open: true },
    //   cmd_replace verifies its slot 0 but must not issue a second
    //   CryptsetupLuksOpen against that disk's by_id.
    //
    // Why it exists: already-open LUKS candidates enter post-operation
    //   membership, so their slot 0 must be verified before the journal.
    //   The already-open path still must not run ensure_luks_open and bind
    //   the mapper a second time.
    //
    // Scenario: a previous replace/add opened /dev/mapper/braid-disk3 but
    //   never added it to the pool (e.g. crash). Operator retries
    //   `braid replace --old disk2 --new disk3=...`; the command picks up
    //   the already-open mapper and proceeds to btrfs replace start without
    //   a second LUKS interaction on the new disk.
    fn mapper_open_true_verifies_but_does_not_open_new_disk_luks() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let runner = RecordingReplaceRunner::new();
        let log = runner.log.clone();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        // The inner runner forces BtrfsReplaceStart to fail (exit 1), so
        // cmd_replace must return a Pool error -- this confirms the flow
        // reached the btrfs phase rather than stopping short, which is a
        // prerequisite for the zero-counts below to mean "not called"
        // instead of "test aborted early".
        assert!(
            matches!(result, Err(ReplaceError::Pool(_))),
            "expected Err(ReplaceError::Pool(_)) from btrfs replace start failure, got: {result:?}"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_some(),
            "journal must be written -- the failure is post-journal"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the way in"
        );

        let log = log.lock().unwrap();
        let new_by_id = "/dev/disk/by-id/virtio-disk3";

        let test_passphrase_calls = log
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupTestPassphrase { device } if device == new_by_id))
            .count();
        assert_eq!(
            test_passphrase_calls, 1,
            "mapper_open: true must verify CryptsetupTestPassphrase on the new disk exactly once"
        );

        let open_calls = log
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupLuksOpen { device, .. } if device == new_by_id))
            .count();
        assert_eq!(
            open_calls, 0,
            "mapper_open: true must not trigger CryptsetupLuksOpen on the new disk"
        );
    }

    /// Runner for a successful missing-path replace. Drives cmd_replace all
    /// the way through the replace -> resize -> soft-balance sequence.
    ///
    /// Stateful: `BtrfsFilesystemShow` returns a degraded layout (disk1 live,
    /// devid 2 missing) until `BtrfsReplaceStart` is issued, then flips to a
    /// healthy 2-device layout (disk1 + disk3) so the second `probe_pool`
    /// inside `maybe_restore_raid1` sees `missing_count == 0` with
    /// `devices.len() >= 2` -- the minimal condition set for the soft
    /// balance to fire.
    struct MissingPathSuccessRunner {
        log: std::sync::Arc<std::sync::Mutex<Vec<CmdRequest>>>,
        replace_done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl CmdRunner2 for MissingPathSuccessRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());
            match request {
                CmdRequest::BtrfsFilesystemShow { mount_point } => {
                    let show = if self.replace_done.load(std::sync::atomic::Ordering::Relaxed) {
                        // post-replace: disk1 + disk3, no missing
                        "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
                         \tTotal devices 2 FS bytes used 16.17MiB\n\
                         \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
                         \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk3\n"
                    } else {
                        // pre-replace: disk1 live, devid 2 missing
                        "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
                         \tTotal devices 2 FS bytes used 16.17MiB\n\
                         \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
                         \t*** Some devices missing\n"
                    };
                    Ok(mock_ok(
                        &format!("btrfs filesystem show {mount_point}"),
                        show,
                    ))
                }
                CmdRequest::CryptsetupStatus { mapper } => {
                    // disk3's mapper is already open: skips the LUKS
                    // format/open/enroll init steps so the test focuses on
                    // the shared replace spine + missing-path tail.
                    let dev = match mapper.as_str() {
                        "braid-disk1" => "/dev/vdb",
                        "braid-disk3" => "/dev/vdd",
                        _ => return Err(CmdError::MissingMock),
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"
                        ),
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = match device.as_str() {
                        "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => {
                            "11111111-1111-1111-1111-111111111111"
                        }
                        "/dev/vdd" | "/dev/disk/by-id/virtio-disk3" => {
                            "33333333-3333-3333-3333-333333333333"
                        }
                        _ => return Err(CmdError::MissingMock),
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{uuid}\n"),
                    ))
                }
                CmdRequest::CryptsetupLuksDumpText { device } => Ok(mock_ok(
                    &format!("cryptsetup luksDump {device}"),
                    "LUKS header information\nVersion:       \t2\n",
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(mock_ok(
                    "btrfs device usage --raw",
                    "/dev/mapper/braid-disk1, ID: 1\n\
                     \tDevice size:           520093696\n\
                     \tDevice slack:                  0\n\
                     \tData,RAID1:            469762048\n\
                     \tUnallocated:            50331648\n\n\
                     <missing disk>, ID: 2\n\
                     \tDevice size:                  0\n\
                     \tDevice slack:                  0\n\
                     \tData,RAID1:            469762048\n\
                     \tUnallocated:                  0\n\n",
                )),
                CmdRequest::BtrfsReplaceStart { .. } => {
                    self.replace_done
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(mock_ok("btrfs replace start", ""))
                }
                CmdRequest::BtrfsFilesystemResize { .. } => {
                    Ok(mock_ok("btrfs filesystem resize", ""))
                }
                CmdRequest::BtrfsBalanceRaid1Soft { .. } => {
                    Ok(mock_ok("btrfs balance raid1 soft", ""))
                }
                CmdRequest::CryptsetupTestPassphrase { device } => Ok(mock_ok(
                    &format!("cryptsetup open --test-passphrase {device}"),
                    "",
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

    #[test]
    // Intent: on the missing path, `cmd_replace` issues the soft-balance
    //   follow-up after the replace-start + resize sequence, and does not
    //   close any old LUKS mapper (there is none).
    //
    // Why it exists: the missing arm of `cmd_replace` delegates the
    //   post-replace redundancy restoration to `crate::pool::maybe_restore_raid1`.
    //   An end-to-end VM test of this is infeasible -- the only way to
    //   create the single-profile chunks the soft balance is meant to
    //   clean up is to write while degraded, and that same state prevents
    //   `btrfs replace start` from succeeding (kernel returns ENOSPC from
    //   `inc_block_group_ro` during staging; see
    //   `reference/linux/fs/btrfs/block-group.c:1366`). Without a wiring
    //   test at this layer, a refactor that dropped the
    //   `maybe_restore_raid1` call on the missing path -- or reordered it
    //   before the replace/resize -- would ship undetected.
    //
    // Scenario: pool has disk1 live + devid 2 missing. Operator runs
    //   `braid replace --old disk2 --missing-id 2 --new disk3=...` with an
    //   already-LUKS-open disk3 (PresentLuks { mapper_open: true }), which
    //   skips the LUKS init steps and focuses the test on the shared
    //   replace spine + missing-path tail. The runner reports degraded
    //   btrfs state until `BtrfsReplaceStart` is issued, then flips to a
    //   healthy 2-device layout so `maybe_restore_raid1`'s probe sees
    //   `missing_count == 0` with `devices.len() >= 2` and fires the soft
    //   balance.
    fn cmd_replace_missing_path_runs_soft_balance_after_replace_and_resize() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        // disk2 is the missing entry being replaced. Record its devid so
        // `build_replacement_membership` matches the --missing-id argument
        // to the right pool.json row.
        let mut disk2 = DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into()));
        disk2.devid = Some(2);
        m.disks.insert("disk2".into(), disk2);
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        // disk3 is already LUKS-open (PresentLuks { mapper_open: true }),
        // so cmd_replace skips LUKS format/open/enroll. That keeps the test
        // focused on the replace+resize+balance sequence.
        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let replace_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let runner = MissingPathSuccessRunner {
            log: log.clone(),
            replace_done: replace_done.clone(),
        };
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: Some(2),
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        assert!(
            matches!(result, Ok(())),
            "expected Ok(()) from successful missing-path replace, got: {result:?}"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "pending-op.json must be cleared on successful completion"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the way in"
        );

        let log = log.lock().unwrap();
        let replace_idx = log
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsReplaceStart { devid: 2, .. }))
            .expect("btrfs replace start on devid 2 must be issued");
        let resize_idx = log
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsFilesystemResize { devid: 2, .. }))
            .expect("btrfs filesystem resize on devid 2 must be issued");
        let balance_idx = log
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsBalanceRaid1Soft { .. }))
            .expect(
                "btrfs soft balance must be issued after replace+resize on missing path \
                 -- maybe_restore_raid1 is part of the `replace` contract per \
                 docs/principles.md",
            );
        assert!(
            replace_idx < resize_idx && resize_idx < balance_idx,
            "missing-path command order violated \
             (replace={replace_idx}, resize={resize_idx}, balance={balance_idx}) -- \
             soft balance must run AFTER the replace-start and resize"
        );

        let close_calls = log
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupClose { .. }))
            .count();
        assert_eq!(
            close_calls, 0,
            "missing path has no old LUKS mapper to close -- CryptsetupClose must not be issued"
        );
    }

    /// Runner for the keyfile-ordering regression test. Stubs every probe
    /// command needed to reach `ReplacePlan::execute` for a missing-path
    /// replace where the new disk is `PresentNotLuks` and `--enroll-key-file`
    /// is set, then succeeds at `LuksFormat`, `LuksAddKeyFile`, and
    /// `LuksHeaderBackup` (writing the backup file like `MockRunner` does)
    /// so execution proceeds to `LuksOpen`. `LuksOpen` is left unmocked so
    /// it returns `MissingMock`, aborting cleanly with the full LUKS
    /// request log intact for ordering assertions.
    struct KeyfileOrderingReplaceRunner {
        log: std::sync::Arc<std::sync::Mutex<Vec<CmdRequest>>>,
    }

    impl CmdRunner2 for KeyfileOrderingReplaceRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());
            match request {
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
                     \tTotal devices 2 FS bytes used 16.17MiB\n\
                     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
                     \t*** Some devices missing\n",
                )),
                CmdRequest::CryptsetupStatus { mapper } => {
                    let dev = match mapper.as_str() {
                        "braid-disk1" => "/dev/vdb",
                        _ => return Err(CmdError::MissingMock),
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"
                        ),
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => match device.as_str() {
                    "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => Ok(mock_ok(
                        &format!("cryptsetup luksUUID {device}"),
                        "11111111-1111-1111-1111-111111111111\n",
                    )),
                    "/dev/disk/by-id/virtio-disk3" => Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksUUID {device}"),
                        stdout: String::new(),
                        stderr: "Device is not a valid LUKS device.\n".into(),
                        exit_status: 1,
                    }),
                    _ => Err(CmdError::MissingMock),
                },
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(mock_ok(
                    "btrfs device usage --raw",
                    "/dev/mapper/braid-disk1, ID: 1\n\
                     \tDevice size:           520093696\n\
                     \tDevice slack:                  0\n\
                     \tData,RAID1:            469762048\n\
                     \tUnallocated:            50331648\n\n\
                     <missing disk>, ID: 2\n\
                     \tDevice size:                  0\n\
                     \tDevice slack:                  0\n\
                     \tData,RAID1:            469762048\n\
                     \tUnallocated:                  0\n\n",
                )),
                CmdRequest::CryptsetupTestPassphrase { device } => Ok(mock_ok(
                    &format!("cryptsetup open --test-passphrase {device}"),
                    "",
                )),
                CmdRequest::CryptsetupLuksFormat { device, .. } => {
                    Ok(mock_ok(&format!("cryptsetup luksFormat {device}"), ""))
                }
                CmdRequest::CryptsetupLuksAddKeyFile { device, .. } => {
                    Ok(mock_ok(&format!("cryptsetup luksAddKey {device}"), ""))
                }
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device,
                    backup_path,
                } => {
                    // Match MockRunner's behavior: write the backup file so
                    // `backup_luks_header_to`'s rename step succeeds and we
                    // proceed past the backup step to `ensure_luks_open`.
                    if let Some(parent) = std::path::Path::new(backup_path.as_str()).parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| CmdError::Failed(format!("mock: create_dir_all: {e}")))?;
                    }
                    std::fs::write(backup_path, b"")
                        .map_err(|e| CmdError::Failed(format!("mock: write backup: {e}")))?;
                    Ok(mock_ok(
                        &format!("cryptsetup luksHeaderBackup {device}"),
                        "",
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

    /*
     * Intent: cmd_replace with `--enroll-key-file` against a fresh
     *   (PresentNotLuks) new disk emits LUKS commands in the order
     *   LuksFormat -> LuksAddKeyFile -> LuksHeaderBackup -> LuksOpen
     *   in the real execute path.
     *
     * Why it exists: `ReplacePlan::execute` discards the precompiled
     *   `steps` and re-implements the LUKS init sequence inline, so a
     *   reorder-only fix to `compile_replace_steps` would not protect
     *   the production path. Pinning the chain at the real-execute layer
     *   also covers the "no backup before open" guarantee that keeps the
     *   no-backup window narrow if `LuksAddKeyFile` ever fails between
     *   `LuksFormat` and `LuksHeaderBackup`. The dry-run preview path is
     *   pinned by `dry_run_render_fresh_disk_live_replace_with_keyfile`.
     *
     * Scenario: 2-disk pool with disk2 missing. Operator runs
     *   `braid replace --old disk2 --missing-id 2 --new disk3=...
     *   --enroll-key-file=/tmp/braid.key`. The recording runner makes
     *   header backup succeed (so we proceed past it) and falls through
     *   to MissingMock at LuksOpen, leaving the full LUKS request log
     *   for ordering assertions.
     */
    #[test]
    fn cmd_replace_with_keyfile_orders_format_addkey_backup_open() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let mut disk2 = DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into()));
        disk2.devid = Some(2);
        m.disks.insert("disk2".into(), disk2);
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let kf_dir = tempfile::tempdir().unwrap();
        let kf_path = kf_dir.path().join("braid.key");
        std::fs::write(&kf_path, [0u8; crate::luks::KEYFILE_SIZE]).unwrap();

        // Only disk3's by_id exists; the mapper /dev/mapper/braid-disk3 is
        // absent so `ensure_luks_open` proceeds to issue `LuksOpen`.
        let fs = ReplaceMockFs(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let runner = KeyfileOrderingReplaceRunner { log: log.clone() };
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let luks_format_extra_opts = vec![
            "--pbkdf".to_owned(),
            "pbkdf2".to_owned(),
            "--iter-time".to_owned(),
            "1".to_owned(),
        ];

        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: Some(2),
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: Some(kf_path.as_path()),
                luks_format_extra_opts: &luks_format_extra_opts,
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        assert!(
            result.is_err(),
            "cmd_replace must abort at the unmocked LuksOpen request, got: {result:?}"
        );

        let log = log.lock().unwrap();
        let position = |label: &str, pred: fn(&CmdRequest) -> bool| -> usize {
            log.iter()
                .position(pred)
                .unwrap_or_else(|| panic!("{label} not found in log: {log:?}"))
        };
        let format = position("LuksFormat", |r| {
            matches!(r, CmdRequest::CryptsetupLuksFormat { .. })
        });
        let CmdRequest::CryptsetupLuksFormat { extra_opts, .. } = &log[format] else {
            unreachable!("format index matched CryptsetupLuksFormat")
        };
        assert_eq!(
            extra_opts,
            &vec![
                "--pbkdf".to_owned(),
                "pbkdf2".to_owned(),
                "--iter-time".to_owned(),
                "1".to_owned(),
                "--label".to_owned(),
                "braid-disk3".to_owned(),
            ]
        );
        let addkey = position("LuksAddKeyFile", |r| {
            matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { .. })
        });
        let backup = position("LuksHeaderBackup", |r| {
            matches!(r, CmdRequest::CryptsetupLuksHeaderBackup { .. })
        });
        let open = position("LuksOpen", |r| {
            matches!(r, CmdRequest::CryptsetupLuksOpen { .. })
        });

        assert!(
            format < addkey && addkey < backup && backup < open,
            "expected order LuksFormat({format}) < LuksAddKeyFile({addkey}) < \
             LuksHeaderBackup({backup}) < LuksOpen({open}); log = {log:?}"
        );
    }

    /// Runner mirror of `MissingPathSuccessRunner` that succeeds through
    /// `btrfs replace start` and `btrfs filesystem resize` but fails the
    /// post-replace `btrfs balance start -dconvert=raid1,soft`. Lets the
    /// regression test pin that `pool.json` was already persisted before
    /// the soft-balance step ran.
    struct MissingPathBalanceFailingRunner {
        log: std::sync::Arc<std::sync::Mutex<Vec<CmdRequest>>>,
        replace_done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl CmdRunner2 for MissingPathBalanceFailingRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());
            match request {
                CmdRequest::BtrfsFilesystemShow { mount_point } => {
                    let show = if self.replace_done.load(std::sync::atomic::Ordering::Relaxed) {
                        "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
                         \tTotal devices 2 FS bytes used 16.17MiB\n\
                         \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
                         \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk3\n"
                    } else {
                        "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
                         \tTotal devices 2 FS bytes used 16.17MiB\n\
                         \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
                         \t*** Some devices missing\n"
                    };
                    Ok(mock_ok(
                        &format!("btrfs filesystem show {mount_point}"),
                        show,
                    ))
                }
                CmdRequest::CryptsetupStatus { mapper } => {
                    let dev = match mapper.as_str() {
                        "braid-disk1" => "/dev/vdb",
                        "braid-disk3" => "/dev/vdd",
                        _ => return Err(CmdError::MissingMock),
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"
                        ),
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = match device.as_str() {
                        "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => {
                            "11111111-1111-1111-1111-111111111111"
                        }
                        "/dev/vdd" | "/dev/disk/by-id/virtio-disk3" => {
                            "33333333-3333-3333-3333-333333333333"
                        }
                        _ => return Err(CmdError::MissingMock),
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{uuid}\n"),
                    ))
                }
                CmdRequest::CryptsetupLuksDumpText { device } => Ok(mock_ok(
                    &format!("cryptsetup luksDump {device}"),
                    "LUKS header information\nVersion:       \t2\n",
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(mock_ok(
                    "btrfs device usage --raw",
                    "/dev/mapper/braid-disk1, ID: 1\n\
                     \tDevice size:           520093696\n\
                     \tDevice slack:                  0\n\
                     \tData,RAID1:            469762048\n\
                     \tUnallocated:            50331648\n\n\
                     <missing disk>, ID: 2\n\
                     \tDevice size:                  0\n\
                     \tDevice slack:                  0\n\
                     \tData,RAID1:            469762048\n\
                     \tUnallocated:                  0\n\n",
                )),
                CmdRequest::BtrfsReplaceStart { .. } => {
                    self.replace_done
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(mock_ok("btrfs replace start", ""))
                }
                CmdRequest::BtrfsFilesystemResize { .. } => {
                    Ok(mock_ok("btrfs filesystem resize", ""))
                }
                CmdRequest::BtrfsBalanceRaid1Soft { .. } => Ok(RawCommandOutput {
                    cmd: "btrfs balance raid1 soft".into(),
                    stdout: String::new(),
                    stderr: "ERROR: error during balancing".into(),
                    exit_status: 1,
                }),
                CmdRequest::CryptsetupTestPassphrase { device } => Ok(mock_ok(
                    &format!("cryptsetup open --test-passphrase {device}"),
                    "",
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

    /*
     * Intent: on the missing path, `pool.json` already reflects the new
     * membership (disk2 gone, disk3 enriched) when the post-replace soft
     * balance fails, and `pending-op.json` survives so `braid recover`
     * can drive replay.
     *
     * Why it exists: replace previously persisted `pool.json` only after
     * the entire post-mutation maintenance chain (resize + soft balance)
     * succeeded. A soft-balance failure therefore left `pool.json`
     * naming the now-replaced missing disk while the live btrfs pool
     * already had disk3 in its place -- forcing the operator into
     * recovery just to reconcile bookkeeping. The fix moves the
     * `save_membership` call to immediately after `btrfs replace start`,
     * which is the membership commit point. This test pins that
     * ordering: revert the early save and the assertions on
     * `pool.json`'s contents fail because the FailingSoftBalance branch
     * returns before the late-save would have run.
     *
     * Scenario: pool has disk1 live + devid 2 missing (disk2 row).
     * Operator runs `braid replace --old disk2 --missing-id 2 --new
     * disk3=...`. `btrfs replace start` and `btrfs filesystem resize`
     * succeed; the post-replace `btrfs balance start -dconvert=raid1,soft`
     * fails (e.g. ENOSPC, kernel I/O error). The command exits non-zero
     * but `pool.json` already has disk3 enriched and `pending-op.json`
     * is still on disk for recovery.
     */
    #[test]
    fn pool_json_persisted_when_missing_path_soft_balance_fails() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let mut disk2 = DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into()));
        disk2.devid = Some(2);
        m.disks.insert("disk2".into(), disk2);
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let replace_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let runner = MissingPathBalanceFailingRunner {
            log: log.clone(),
            replace_done: replace_done.clone(),
        };
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: Some(2),
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        match &result {
            Err(ReplaceError::Pool(_)) => {}
            other => panic!("expected Err(ReplaceError::Pool(..)), got: {other:?}"),
        }

        assert!(
            journal::load_journal(&paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
        let journal = journal::load_journal(&paths)
            .unwrap()
            .expect("journal should remain after post-replace balance failure");
        assert!(
            matches!(
                journal.op,
                journal::OpKind::Replace {
                    phase: journal::ReplacePhase::PostReplaceMaintenance,
                    ..
                }
            ),
            "journal should advance after btrfs replace commits: {:?}",
            journal.op
        );

        let saved = membership::load_membership(&paths)
            .expect("pool.json must exist after the membership commit");
        assert!(
            !saved.disks.contains_key("disk2"),
            "old missing disk must be gone from pool.json once btrfs replace succeeds, \
             even when the post-replace soft balance fails (saved: {:?})",
            saved.disks.keys().collect::<Vec<_>>()
        );
        let disk3 = saved.disks.get("disk3").unwrap_or_else(|| {
            panic!(
                "new disk must be in pool.json once btrfs replace succeeds (saved: {:?})",
                saved.disks.keys().collect::<Vec<_>>()
            )
        });
        assert!(
            disk3.luks_uuid.is_some() && disk3.devid.is_some() && disk3.added_at.is_some(),
            "new disk must carry enriched metadata (luks_uuid, devid, added_at) \
             from the post-replace probe: {disk3:?}"
        );
    }

    /* Intent: the live-path dry-run preview flows through
     * `plan_replace(...).preview().render()`, and the confirmation-only
     * 1-disk `WARNING:` line must never leak into `--dry-run` stdout.
     *
     * Why it exists: `braid replace --dry-run` routes through
     * `ReplacePlan::preview()` instead of `Step::print_dry_run(&steps)`.
     * A regression that surfaced the confirmation-only `WARNING:` line
     * on dry-run would change the bytes an operator sees. A regression
     * that reordered steps or dropped the cryptsetup-close step would
     * also change the rendered output. Both failure modes are caught
     * here.
     *
     * Scenario: 2-disk pool, operator previews
     * `braid replace --old disk2 --new disk3=...` with a fresh (but
     * already-LUKS-open) replacement. Mirrors the fixture used by
     * `dry_run_does_not_acquire_inhibitor`.
     */
    #[test]
    fn plan_replace_live_preview_has_no_notes_and_matches_legacy_step_render() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();
        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let runner = FailingReplaceRunner;
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let report = plan_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );
        assert!(
            report.notes.is_empty(),
            "replace preview fixture must not accumulate preflight notes on the Ok path: {:?}",
            report.notes,
        );
        let plan = report
            .result
            .expect("plan_replace should succeed on live-path fixture");

        let preview = plan.preview();
        let rendered = preview.render();
        let legacy = Step::render_dry_run(&plan.steps);
        // Byte-equivalence holds because this fixture produces zero
        // notes (clean preflight on a rw pool with no busy op). A
        // future fixture with real preflight notes would render them
        // above the step block and byte-equivalence would no longer
        // hold.
        assert_eq!(
            rendered, legacy,
            "plan.preview().render() must be byte-equivalent to Step::render_dry_run(&plan.steps) for the live path",
        );

        assert!(
            rendered.contains("btrfs replace start"),
            "live-path preview must contain `btrfs replace start` step, got:\n{rendered}",
        );
        assert!(
            rendered.contains("cryptsetup close braid-disk2"),
            "live-path preview must close the old mapper, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("WARNING:"),
            "live-path dry-run preview must not leak confirmation-only WARNING lines, got:\n{rendered}",
        );
        assert!(
            inhibitor.acquire_count() == 0,
            "plan_replace must not acquire the sleep inhibitor",
        );
    }

    /* Intent: the missing-path dry-run preview flows through
     * `plan_replace(...).preview().render()`, and the confirmation-only
     * 1-disk `WARNING:` line must never leak into `--dry-run` stdout.
     *
     * Why it exists: same regression surface as the live-path
     * preview test, but for the `ReplaceSource::Missing` branch
     * (different step sequence: no cryptsetup close, soft balance at
     * the tail when clearing the last missing device).
     *
     * Scenario: 2-disk pool, devid 2 missing, operator previews
     * `braid replace --old disk2 --missing-id 2 --new disk3=...`.
     * Mirrors the fixture used by
     * `cmd_replace_missing_path_runs_soft_balance_after_replace_and_resize`.
     */
    #[test]
    fn plan_replace_missing_preview_has_no_notes_and_matches_legacy_step_render() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let mut disk2 = DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into()));
        disk2.devid = Some(2);
        m.disks.insert("disk2".into(), disk2);
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();
        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let replace_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let runner = MissingPathSuccessRunner {
            log: log.clone(),
            replace_done: replace_done.clone(),
        };
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let report = plan_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: Some(2),
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );
        assert!(
            report.notes.is_empty(),
            "replace preview fixture must not accumulate preflight notes on the Ok path: {:?}",
            report.notes,
        );
        let plan = report
            .result
            .expect("plan_replace should succeed on missing-path fixture");

        let preview = plan.preview();
        let rendered = preview.render();
        let legacy = Step::render_dry_run(&plan.steps);
        // Byte-equivalence holds because this fixture produces zero
        // notes (clean preflight on a rw pool with no busy op). A
        // future fixture with real preflight notes would render them
        // above the step block and byte-equivalence would no longer
        // hold.
        assert_eq!(
            rendered, legacy,
            "plan.preview().render() must be byte-equivalent to Step::render_dry_run(&plan.steps) for the missing path",
        );

        assert!(
            rendered.contains("btrfs replace start"),
            "missing-path preview must contain `btrfs replace start`, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("cryptsetup close"),
            "missing-path preview must NOT carry a cryptsetup close step (no old mapper), got:\n{rendered}",
        );
        assert!(
            rendered.contains("-dconvert=raid1,soft"),
            "missing-path preview must carry the soft balance step when clearing the last missing device, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("WARNING:"),
            "missing-path dry-run preview must not leak confirmation-only WARNING lines, got:\n{rendered}",
        );

        // plan_replace must not mutate btrfs state -- replace_done
        // flips only inside execute()'s BtrfsReplaceStart dispatch.
        assert!(
            !replace_done.load(std::sync::atomic::Ordering::Relaxed),
            "plan_replace must not issue btrfs replace start",
        );
        assert!(
            inhibitor.acquire_count() == 0,
            "plan_replace must not acquire the sleep inhibitor",
        );
    }

    /// ReplaceMockFs variant with a configurable sysfs
    /// exclusive_operation body. Drives preflight's busy-op /
    /// paused-balance branches from the plan_replace boundary tests.
    struct ReplaceMockFsWithSysfs {
        inner: ReplaceMockFs,
        sysfs_body: String,
    }

    impl ReplaceMockFsWithSysfs {
        fn new(paths: Vec<String>, sysfs_body: &str) -> Self {
            Self {
                inner: ReplaceMockFs(paths),
                sysfs_body: sysfs_body.to_owned(),
            }
        }
    }

    impl crate::probe::Filesystem for ReplaceMockFsWithSysfs {
        fn exists(&self, path: &str) -> bool {
            self.inner.exists(path)
        }
        fn is_block_device(&self, path: &str) -> bool {
            self.inner.is_block_device(path)
        }
        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path == "/proc/self/mountinfo" {
                Ok(
                    "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/braid-disk1 rw\n"
                        .to_owned(),
                )
            } else if path.ends_with("/exclusive_operation") {
                Ok(self.sysfs_body.clone())
            } else {
                self.inner.read_to_string(path)
            }
        }
        fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
            self.inner.list_dir(path)
        }
    }

    /* Intent: plan_replace surfaces an in-flight exclusive op as a
     * PreviewNote::Info on `plan.notes`, and the rendered preview
     * contains the "waiting for in-flight <op>" line. Confirmation-only
     * 1-disk `WARNING:` output still does not leak into the preview.
     * Why it exists: Shape A migration moves the busy-op diagnostic
     * from stderr into plan.notes; a regression leaking it back to
     * stderr breaks the dry-run stdout-only contract.
     * Scenario: 2-disk pool, sysfs reports "device add", operator
     * previews `braid replace --old disk2 --new disk3=...`. Mirrors
     * the live-path preview fixture.
     */
    #[test]
    fn plan_replace_preflight_busy_op_becomes_info_note() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();
        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFsWithSysfs::new(
            vec![
                "/dev/disk/by-id/virtio-disk3".into(),
                "/dev/mapper/braid-disk3".into(),
            ],
            "device add\n",
        );
        let runner = FailingReplaceRunner;
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let report = plan_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );
        let plan = report
            .result
            .expect("plan_replace should succeed on live-path fixture + busy op");
        assert_eq!(
            plan.notes.len(),
            1,
            "expected one preflight Info note, got {:?}",
            plan.notes,
        );
        assert!(
            matches!(
                &plan.notes[0],
                PreviewNote::Info(b) if b.contains("waiting for in-flight") && b.contains("device add")
            ),
            "notes[0]={:?}",
            plan.notes[0],
        );
        let rendered = plan.preview().render();
        assert!(
            rendered.contains("waiting for in-flight device add"),
            "rendered preview must carry the busy-op Info line, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("WARNING:"),
            "confirmation-only WARNING lines must not leak into preview, got:\n{rendered}",
        );
    }

    // Intent: a same-name `braid replace --old/--new` typo aborts in the
    // planner before any shell or Filesystem-backed probe.
    // Why it exists: the pre-hoist preserved-note behavior conflated a pure
    // input-shape error with I/O-precondition context; this path now fails
    // before preflight can accumulate state-context notes.
    // Scenario: user runs `braid replace --old disk2 --new disk2=/...`;
    // the planner returns the same-name validation error with no probes.
    #[test]
    fn plan_replace_old_equals_new_aborts_before_any_probe() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let report = plan_replace(
            &PanicRunner,
            &PanicFilesystem,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk2=/dev/disk/by-id/virtio-disk2",
                missing_id: None,
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );
        match &report.result {
            Err(ReplaceError::Validation(msg)) => {
                assert!(
                    msg.contains("--old and --new must be different"),
                    "expected same-name refusal wording, got: {msg}"
                );
            }
            Err(other) => panic!("expected Validation, got: {other:?}"),
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
        }
        assert_eq!(
            report.notes.len(),
            0,
            "same-name input validation must not preserve preflight notes, got: {:?}",
            report.notes,
        );
        assert_eq!(inhibitor.acquire_count(), 0);
    }

    // Intent: `braid replace --enroll` rejects a missing braid.key during
    // planning, before shell probes or Filesystem-backed pool/disk probes.
    // Why it exists: a typoed keyfile path must not let replace format the
    // new disk and then fail only at keyfile enrollment.
    // Scenario: user passes a nonexistent enroll directory while replacing a
    // disk; the command refuses with a keyfile error and no probes run.
    #[test]
    fn plan_replace_aborts_when_keyfile_missing_before_any_probe() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();
        let kf_path = config_tmp.path().join("does-not-exist").join("braid.key");
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let report = plan_replace(
            &PanicRunner,
            &PanicFilesystem,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: Some(kf_path.as_path()),
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        match report.result {
            Err(ReplaceError::Validation(msg)) => assert!(
                msg.contains("keyfile not found"),
                "expected missing keyfile validation, got: {msg}"
            ),
            Err(other) => panic!("expected Validation, got: {other:?}"),
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
        }
        assert!(
            report.notes.is_empty(),
            "expected no notes: {:?}",
            report.notes
        );
        assert_eq!(inhibitor.acquire_count(), 0);
    }

    // Intent: `braid replace --enroll` rejects a directory at braid.key during
    // planning, before shell probes or Filesystem-backed pool/disk probes.
    // Why it exists: checking only existence would still allow an invalid
    // keyfile path to reach destructive LUKS work before enrollment fails.
    // Scenario: user points --enroll at a directory containing a subdirectory
    // named braid.key; the command refuses before any disk inspection.
    #[test]
    fn plan_replace_aborts_when_keyfile_is_directory_before_any_probe() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();
        let kf_path = config_tmp.path().join("braid.key");
        std::fs::create_dir(&kf_path).unwrap();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let report = plan_replace(
            &PanicRunner,
            &PanicFilesystem,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: Some(kf_path.as_path()),
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        match report.result {
            Err(ReplaceError::Validation(msg)) => assert!(
                msg.contains("is not a regular file"),
                "expected directory keyfile validation, got: {msg}"
            ),
            Err(other) => panic!("expected Validation, got: {other:?}"),
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
        }
        assert!(
            report.notes.is_empty(),
            "expected no notes: {:?}",
            report.notes
        );
        assert_eq!(inhibitor.acquire_count(), 0);
    }

    #[derive(Clone, Copy)]
    enum ReplaceKeyfileProbe {
        Occupied,
        Failure,
    }

    struct ReplaceKeyfileProbeRunner {
        probes: Vec<ReplaceKeyfileProbe>,
        log: std::sync::Arc<std::sync::Mutex<Vec<CmdRequest>>>,
    }

    impl ReplaceKeyfileProbeRunner {
        fn new(probes: Vec<ReplaceKeyfileProbe>) -> Self {
            Self {
                probes,
                log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn requests(&self) -> Vec<CmdRequest> {
            self.log.lock().unwrap().clone()
        }

        fn probe_for_device(&self, device: &str) -> Option<ReplaceKeyfileProbe> {
            ["/dev/vda", "/dev/vdb", "/dev/vdc"]
                .iter()
                .position(|candidate| *candidate == device)
                .and_then(|index| self.probes.get(index).copied())
        }
    }

    impl CmdRunner2 for ReplaceKeyfileProbeRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());
            match request {
                CmdRequest::CryptsetupLuksUuid { device }
                    if device == "/dev/disk/by-id/virtio-disk3" =>
                {
                    Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksUUID {device}"),
                        stdout: String::new(),
                        stderr: "Device is not a valid LUKS device.\n".into(),
                        exit_status: 1,
                    })
                }
                CmdRequest::CryptsetupLuksDump { device } => match self.probe_for_device(device) {
                    Some(ReplaceKeyfileProbe::Occupied) => Ok(mock_ok(
                        "cryptsetup luksDump --dump-json-metadata",
                        r#"{"keyslots":{"0":{"type":"luks2"},"1":{"type":"luks2"}}}"#,
                    )),
                    Some(ReplaceKeyfileProbe::Failure) => Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksDump --dump-json-metadata {device}"),
                        stdout: String::new(),
                        stderr: format!("forced luksDump failure on {device}"),
                        exit_status: 5,
                    }),
                    None => Err(CmdError::MissingMock),
                },
                _ => FailingReplaceRunner.run(request),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());
            self.run(request)
        }
    }

    struct PlanReplaceFixture {
        _state_tmp: tempfile::TempDir,
        paths: StatePaths,
        _config_tmp: tempfile::TempDir,
        config_path: std::path::PathBuf,
        pass_path: std::path::PathBuf,
        inhibitor: crate::inhibit::RecordingInhibitor,
    }

    fn plan_replace_fixture() -> PlanReplaceFixture {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();
        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        PlanReplaceFixture {
            _state_tmp: state_tmp,
            paths,
            _config_tmp: config_tmp,
            config_path,
            pass_path,
            inhibitor: crate::inhibit::RecordingInhibitor::new(),
        }
    }

    impl PlanReplaceFixture {
        fn params(&self, dry_run: bool, yes: bool) -> ReplaceParams<'_> {
            ReplaceParams {
                config_path: &self.config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run,
                yes,
                passphrase_stdin: false,
                passphrase_file: Some(self.pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &self.paths,
                sleep_inhibitor: &self.inhibitor,
            }
        }
    }

    /* Intent: plan_replace turns keyfile probe failures into warning notes
     * on the replacement plan.
     * Why it exists: dry-run, real-run, and preserved-error output should use
     * the same PreviewNote contract instead of a confirmation-only stderr
     * side path.
     * Scenario: operator previews replacing disk2 with raw disk3 while
     * existing pool members would fail the keyfile probe.
     */
    #[test]
    fn plan_replace_keyfile_probe_failure_becomes_warn_notes() {
        let fixture = plan_replace_fixture();
        let fs = ReplaceMockFs(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let runner = ReplaceKeyfileProbeRunner::new(vec![
            ReplaceKeyfileProbe::Failure,
            ReplaceKeyfileProbe::Failure,
            ReplaceKeyfileProbe::Failure,
        ]);

        let report = plan_replace(&runner, &fs, &fixture.params(true, false));
        let plan = report.result.expect("plan_replace should succeed");
        let rendered = plan.preview().render();

        assert!(
            rendered.contains(
                "[warn] could not check keyfile enrollment on /dev/vdb: cryptsetup luksDump failed (exit 5): forced luksDump failure on /dev/vdb; proceeding as if no keyfile is enrolled"
            ),
            "dry-run preview must include the first probe-failure warning, got:\n{rendered}",
        );
        assert!(
            rendered.contains(
                "[warn] could not check keyfile enrollment on /dev/vdc: cryptsetup luksDump failed (exit 5): forced luksDump failure on /dev/vdc; proceeding as if no keyfile is enrolled"
            ),
            "dry-run preview must include the second probe-failure warning, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("Existing pool drives have a keyfile"),
            "probe-failure-only case must not emit the keyfile-asymmetry warning, got:\n{rendered}",
        );
        assert!(
            runner
                .requests()
                .iter()
                .any(|request| matches!(request, CmdRequest::CryptsetupLuksDump { .. })),
            "plan_replace must run the keyfile enrollment probe"
        );
    }

    /* Intent: plan_replace emits keyfile-asymmetry as a PreviewNote::Warn
     * once any pool member proves slot 1 is occupied.
     * Why it exists: replace should use the same keyfile warning policy and
     * wording as add, with no legacy `WARNING:` block.
     * Scenario: disk1's luksDump fails, disk2 reports slot 1 occupied, and
     * the operator replaces with a raw disk without --enroll.
     */
    #[test]
    fn plan_replace_keyfile_asymmetry_suppresses_probe_failure_warning() {
        let fixture = plan_replace_fixture();
        let fs = ReplaceMockFs(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let runner = ReplaceKeyfileProbeRunner::new(vec![
            ReplaceKeyfileProbe::Failure,
            ReplaceKeyfileProbe::Failure,
            ReplaceKeyfileProbe::Occupied,
        ]);

        let report = plan_replace(&runner, &fs, &fixture.params(true, false));
        let plan = report.result.expect("plan_replace should succeed");
        let rendered = plan.preview().render();

        assert_eq!(
            plan.notes.len(),
            1,
            "occupied slot 1 must emit exactly one warning, got {:?}",
            plan.notes
        );
        assert!(
            matches!(
                &plan.notes[0],
                PreviewNote::Warn(body) if body == &format_keyfile_asymmetry_warning()
            ),
            "occupied slot 1 must emit only the keyfile-asymmetry warning, got {:?}",
            plan.notes
        );
        assert!(
            rendered.contains("[warn] Existing pool drives have a keyfile (keyslot-1)"),
            "mixed probe result must render canonical keyfile-asymmetry warning, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("WARNING:"),
            "keyfile-asymmetry warning must not use legacy WARNING prefix, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("could not check keyfile enrollment"),
            "occupied slot 1 must suppress probe-failure uncertainty notes, got:\n{rendered}",
        );
    }

    // Intent: cmd_replace returns the same-name validation error without
    // rendering preserved preflight notes.
    // Why it exists: the pre-hoist preserved-note behavior was intentionally
    // retired; pure input-shape errors now abort before I/O-precondition
    // context can be gathered or rendered.
    // Scenario: user runs `braid replace --old disk2 --new disk2=/...`; the
    // command returns the validation error and stderr has no busy-op note.
    #[test]
    fn cmd_replace_old_equals_new_aborts_before_any_probe() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let (result, stderr) = super::replace_stderr_capture::capture(|| {
            cmd_replace(
                &PanicRunner,
                &PanicFilesystem,
                &ReplaceParams {
                    config_path: &config_path,
                    old_name: "disk2",
                    new_name: "disk2=/dev/disk/by-id/virtio-disk2",
                    missing_id: None,
                    dry_run: false,
                    yes: true,
                    passphrase_stdin: false,
                    passphrase_file: None,
                    enroll_key_file: None,
                    luks_format_extra_opts: &[],
                    progress: crate::progress::ProgressOutput::Off,
                    paths: &paths,
                    sleep_inhibitor: &inhibitor,
                },
            )
        });

        match &result {
            Err(ReplaceError::Validation(msg)) => {
                assert!(
                    msg.contains("--old and --new must be different"),
                    "expected same-name refusal wording, got: {msg}"
                );
            }
            Err(other) => panic!("expected Validation, got: {other:?}"),
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
        }
        assert!(
            !stderr.contains("waiting for in-flight"),
            "same-name validation must not render busy-op notes, got:\n{stderr}",
        );
        assert!(
            stderr.is_empty(),
            "expected no preserved notes, got:\n{stderr}"
        );
        assert_eq!(inhibitor.acquire_count(), 0);
    }
}
