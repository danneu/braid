use crate::cmd::{CommandRunner, Step};
use crate::config::Config;
use crate::membership::{self, PoolMembership};
use crate::mount::{self, MountError, OpenPlan, ProbeEvent};
use crate::preflight;
use crate::preview::{self, PerDiskStyle, PlanFailure, Preview, PreviewCompleteness, PreviewNote};
use crate::probe::{self, Filesystem};
use crate::progress::RealSleeper;
use crate::state_paths::StatePaths;
use crate::status_tag::color_enabled_for_stderr;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum UnlockError {
    #[error("{0}")]
    Mount(#[from] MountError),
    #[error("{0}")]
    Membership(#[from] membership::MembershipError),
    #[error("{0}")]
    Failed(String),
}

pub struct UnlockParams<'a> {
    pub config: &'a Config,
    pub membership: &'a PoolMembership,
    pub paths: &'a StatePaths,
    pub passphrase_stdin: bool,
    pub passphrase_file: Option<&'a Path>,
    pub key_file: Option<&'a Path>,
    pub allow_degraded: bool,
    pub dry_run: bool,
    /// Seam for resolving by-id paths and mapper backings during probe
    /// and already-open unlock checks.
    pub backing_path_resolver: &'a dyn crate::luks::BackingPathResolver,
}

/// Dry-run preview source of truth for `braid unlock` plus the execute
/// inputs pre-computed during planning. `notes` + `steps` are both
/// rendered by `preview()`; `execute()` renders `notes` to stderr with
/// `STDERR_STYLE` before any mutation, preserving today's "probe
/// context then work" real-run sequence.
///
/// `open_plan` is `None` when the pool was already mounted at probe
/// time. `notes` still carries the `AlreadyMounted` Info note in that
/// case so `preview()` and `execute()` both surface it.
#[derive(Debug)]
pub struct UnlockPlan {
    pub notes: Vec<PreviewNote>,
    pub steps: Vec<Step>,
    pub open_plan: Option<OpenPlan>,
}

impl UnlockPlan {
    /// Real-run and failure-path stderr both use `Bracketed`, matching
    /// today's `mount::render_probe_events` output. `Preview::render`
    /// is already `Bracketed`, so success/failure/real-run all share
    /// the same per-disk wording.
    pub const STDERR_STYLE: PerDiskStyle = PerDiskStyle::Bracketed;

    pub fn preview(&self) -> Preview {
        Preview {
            completeness: PreviewCompleteness::Complete,
            notes: self.notes.clone(),
            steps: self.steps.clone(),
        }
    }

    pub fn execute<R: CommandRunner, F: Filesystem + ?Sized>(
        self,
        runner: &R,
        fs: &F,
        params: &UnlockParams<'_>,
    ) -> Result<(), UnlockError> {
        // Render accumulated probe notes to stderr before any mutation.
        // In the already-mounted case this emits the "pool already
        // mounted at <mp>" Info note (byte-identical to today's
        // `print_probe_events` output for that event).
        preview::emit_notes_to_stderr(&self.notes, Self::STDERR_STYLE);

        let Some(plan) = self.open_plan else {
            // Pool was already mounted (plan_open_pool returned None).
            return Ok(());
        };

        // Contract:
        // - Pure operator command: bring the pool online from authoritative state.
        // - Membership comes from pool.json; unlock never mutates membership
        //   topology and never creates or repairs invalid/missing membership.
        // - Probe only configured members, open what is available, and mount the pool.
        // - Refuse degraded mounts unless --allow-degraded is explicit.
        // - After a successful mount, pool.json enrichment fields (devid,
        //   added_at) are refreshed best-effort, but correctness never
        //   depends on that write.

        // Unlock-specific gate: only resolve a credential if there is
        // something to unlock. Preserves the "no prompt when every
        // mapper is already open" UX rule that used to live inside
        // open_and_mount_pool. Pinned by
        // cmd_unlock_skips_credential_resolution_when_nothing_to_unlock.
        if plan.to_unlock.is_empty() {
            mount::execute_mount_only(runner, fs, params.config, &plan)?;
        } else {
            let credential = crate::credential::resolve_credential(
                params.passphrase_stdin,
                params.passphrase_file,
                params.key_file,
            )
            .map_err(MountError::from)?;
            match mount::execute_unlock_and_mount(
                runner,
                fs,
                params.config,
                &plan,
                params.backing_path_resolver,
                &credential,
            ) {
                Ok(_) => {}
                Err(failure) => {
                    let _ = mount::close_opened_mappers(
                        runner,
                        &RealSleeper,
                        fs,
                        &failure.opened_mappers,
                        color_enabled_for_stderr(),
                    );
                    return Err(failure.error.into());
                }
            }
        }

        let mount_point = params.config.mount_point();

        // Enrich pool.json with live metadata (devid, added_at) -- best-effort.
        // The in-memory membership clone is authoritative here because the
        // Rust dispatch holds the pool flock for the lifetime of unlock.
        // Three outcomes are tolerated and leave membership data unenriched:
        //   * `Ok(PoolState { mounted: false, devices: vec![], ... })` -- a
        //     mountinfo race after a successful mount; enrich_from_pool_state
        //     walks an empty devices vec so no fields change.
        //   * `Err(_)` from probe_pool itself (e.g. a parser drift in
        //     `btrfs filesystem show`) -- a Warning line is emitted,
        //     and pool.json is not rewritten.
        //   * enrich/save failure -- a Warning line is emitted and unlock
        //     still succeeds because the mount has already completed.
        // Correctness never depends on this enrichment (see contract above).
        // Pinned by unlock_tolerates_post_mount_probe_mounted_false and
        // unlock_warns_when_post_mount_probe_errors.
        match probe::probe_pool(runner, fs, mount_point) {
            Ok(pool_after) => {
                let mut enriched = params.membership.clone();
                match membership::enrich_from_pool_state(&mut enriched, &pool_after) {
                    Ok(_report) => {
                        if let Err(e) = membership::save_membership(&enriched, params.paths) {
                            emit_post_mount_enrichment_warning(format_args!(
                                "Warning: failed to save enriched membership: {e}"
                            ));
                        }
                    }
                    Err(e) => {
                        emit_post_mount_enrichment_warning(format_args!(
                            "Warning: failed to enrich pool membership: {e}"
                        ));
                    }
                }
            }
            Err(e) => crate::status_tag::emit_status(&format!(
                "Warning: failed to probe pool for metadata refresh: {e}\n"
            )),
        }

        // Best-effort: warn if a paused balance was found on mount.
        // skip_balance prevents the kernel from resuming it silently, but the
        // user should know so they can resume or cancel explicitly.
        crate::status::emit_paused_balance_warning(runner, mount_point, &mut std::io::stderr());

        Ok(())
    }
}

/// Plan a `braid unlock` run. Owns everything above today's `--dry-run`
/// gate: pending-op preflight, `mount::plan_open_pool`, conversion of
/// every `ProbeEvent` to a `PreviewNote`, and `compile_open_steps` when
/// an `OpenPlan` is produced. On success, accumulated probe notes live
/// on `plan.notes` (the single render source for both preview and
/// execute). On failure, notes move to `PlanFailure::notes` so the caller
/// can render them to stderr before the error.
pub fn plan_unlock<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &UnlockParams<'_>,
) -> Result<UnlockPlan, PlanFailure<UnlockError>> {
    // `plan_add` also runs `check_pool_unlocked_if_membership_exists`
    // here; unlock skips it because unlock never mutates pool.json
    // membership topology. Execute may refresh runtime metadata
    // (devid, added_at) after mount, but the set of members is
    // authoritative on entry. See the "Contract:" block in
    // `UnlockPlan::execute`.
    if let Err(msg) = preflight::check_no_pending_operation(params.paths) {
        return Err(PlanFailure::empty(UnlockError::Failed(msg)));
    }

    let report = mount::plan_open_pool(
        runner,
        fs,
        params.config,
        params.membership,
        params.backing_path_resolver,
        params.allow_degraded,
        "unlock",
    );
    let notes: Vec<PreviewNote> = report
        .events
        .iter()
        .map(ProbeEvent::to_preview_note)
        .collect();

    match report.result {
        Ok(open_plan) => {
            let steps = match &open_plan {
                Some(op) => {
                    mount::compile_open_steps(op, params.config.mount_point(), params.key_file)
                }
                None => Vec::new(),
            };
            Ok(UnlockPlan {
                notes,
                steps,
                open_plan,
            })
        }
        Err(e) => Err(PlanFailure::with_notes(notes, UnlockError::Mount(e))),
    }
}

pub fn cmd_unlock<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &UnlockParams<'_>,
) -> Result<(), UnlockError> {
    let plan = match plan_unlock(runner, fs, params) {
        Ok(p) => p,
        Err(PlanFailure { notes, error }) => {
            // Preserved-context failure: accumulated probe notes render
            // to stderr before the error, mirroring today's
            // `print_probe_events` + `?` sequence.
            preview::emit_notes_to_stderr(&notes, UnlockPlan::STDERR_STYLE);
            return Err(error);
        }
    };

    if params.dry_run {
        plan.preview().print_colored();
        return Ok(());
    }

    plan.execute(runner, fs, params)
}

/// Keeps post-mount enrichment warnings byte-identical in production while
/// giving unit tests a narrow capture seam for best-effort write failures.
fn emit_post_mount_enrichment_warning(args: std::fmt::Arguments<'_>) {
    #[cfg(test)]
    if unlock_stderr_capture::write(&format!("{args}\n")) {
        return;
    }
    eprintln!("{args}");
}

#[cfg(test)]
mod unlock_stderr_capture {
    use std::cell::RefCell;

    thread_local! {
        static CAPTURED_STDERR: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    pub(super) fn capture<F, T>(f: F) -> (T, String)
    where
        F: FnOnce() -> T,
    {
        CAPTURED_STDERR.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none(), "nested unlock stderr capture");
            *slot = Some(String::new());
        });

        let result = f();
        let stderr = CAPTURED_STDERR.with(|slot| {
            slot.borrow_mut()
                .take()
                .expect("unlock stderr capture must be active")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::mount::MountError;
    use crate::preview::NoteLevel;
    use crate::test_fixtures::{
        MOUNT_TEST_PASSPHRASE_BYTES, base_two_disk_runner, err_raw as unlock_err_raw,
        isolated_paths, luks_uuid_ok, mount_fs, ok_raw as unlock_ok_raw, test_config,
        test_passphrase_fail, three_disk_membership as unlock_three_disk_membership,
        two_disk_membership, unlock_btrfs_balance_status_idle, unlock_btrfs_balance_status_paused,
        unlock_btrfs_device_scan_ok, unlock_luks_uuid_not_luks, unlock_passphrase_file,
        unlock_storage_fs, unlock_with_mount_degraded_ok, unlock_with_mount_ok,
        unlock_with_open_mapper_ok, unlock_with_test_passphrase_ok, unlock_with_three_mappers_open,
    };
    use crate::types::MapperName;
    use crate::types::MountPoint;

    // Intent: a bricked LUKS header (PresentNotLuks) on a known pool member
    //   must trigger a degraded mount when --allow-degraded is passed.
    // Why it exists: a regression that picks plain Mount over MountWithOptions
    //   would mount without the degraded flag and turn a recoverable missing
    //   member into a hard failure.
    // Scenario: 3-disk RAID1, disk3's LUKS header is zeroed. Probe sees disk3
    //   as PresentNotLuks; the surviving two disks unlock normally; Mount must
    //   use `-o degraded`.
    #[test]
    fn unlock_bricked_disk_uses_degraded_mount() {
        let (_state_dir, sp) = isolated_paths();
        let config = test_config();
        let membership = unlock_three_disk_membership();
        let fs = unlock_storage_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ]);

        let mp = MountPoint("/mnt/storage".to_owned());
        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "11111111-1111-1111-1111-111111111111",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );
        let (uuid3_req, uuid3_out) = unlock_luks_uuid_not_luks("/dev/disk/by-id/virtio-disk3");
        let (scan_req, scan_out) = unlock_btrfs_device_scan_ok();
        let (balance_req, balance_out) = unlock_btrfs_balance_status_idle(&mp);

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck { path: mp.clone() },
                unlock_err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output(uuid3_req, uuid3_out)
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk1");
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk2");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk1", "braid-disk1");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk2", "braid-disk2");
        let runner = runner.with_output(scan_req, scan_out);
        let runner = unlock_with_mount_degraded_ok(runner, "/dev/mapper/braid-disk1", &mp)
            .with_output(balance_req, balance_out);
        let tmp = unlock_passphrase_file();

        let result = cmd_unlock(
            &runner,
            &fs,
            &UnlockParams {
                config: &config,
                membership: &membership,
                paths: &sp,
                passphrase_stdin: false,
                passphrase_file: Some(tmp.path()),
                key_file: None,
                allow_degraded: true,
                dry_run: false,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
            },
        );

        // If the code incorrectly uses Mount instead of MountWithOptions,
        // MockRunner returns MissingMock -- the test fails.
        result.expect("unlock with bricked disk should use degraded mount and succeed");
    }

    // Intent: a bricked LUKS header on a known pool member must refuse a
    //   degraded mount when --allow-degraded is not passed.
    // Why it exists: degraded operation must stay explicit so users do not
    //   unknowingly mount a pool with a missing member.
    // Scenario: same topology as unlock_bricked_disk_uses_degraded_mount, but
    //   without the flag. The error must tell the user how to proceed.
    #[test]
    fn unlock_bricked_disk_refuses_without_flag() {
        let (_state_dir, sp) = isolated_paths();
        let config = test_config();
        let membership = unlock_three_disk_membership();
        let fs = unlock_storage_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ]);

        let mp = MountPoint("/mnt/storage".to_owned());
        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "11111111-1111-1111-1111-111111111111",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );
        let (uuid3_req, uuid3_out) = unlock_luks_uuid_not_luks("/dev/disk/by-id/virtio-disk3");
        let (scan_req, scan_out) = unlock_btrfs_device_scan_ok();

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck { path: mp },
                unlock_err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output(uuid3_req, uuid3_out)
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk1");
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk2");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk1", "braid-disk1");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk2", "braid-disk2")
                .with_output(scan_req, scan_out);
        // No mount mock -- should never reach mount.
        let tmp = unlock_passphrase_file();

        let result = cmd_unlock(
            &runner,
            &fs,
            &UnlockParams {
                config: &config,
                membership: &membership,
                paths: &sp,
                passphrase_stdin: false,
                passphrase_file: Some(tmp.path()),
                key_file: None,
                allow_degraded: false,
                dry_run: false,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
            },
        );

        let err = result.expect_err("should refuse degraded mount without --allow-degraded");
        assert!(
            matches!(&err, UnlockError::Mount(MountError::DegradedRefused(_))),
            "expected DegradedRefused, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("refusing to mount degraded"),
            "error should mention refusal, got: {msg}"
        );
        assert!(
            msg.contains("--allow-degraded"),
            "error should hint at the flag, got: {msg}"
        );
    }

    // Intent: passphrase mismatch on a non-first disk must identify the
    //   failing disk.
    // Why it exists: when the single-passphrase invariant is violated by
    //   external LUKS manipulation, the error must not degrade into a generic
    //   "Wrong passphrase?" message.
    // Scenario: 2-disk RAID1 where someone changed disk2's passphrase outside
    //   braid. `braid unlock` verifies against disk1, then fails on disk2.
    #[test]
    fn passphrase_mismatch_names_failing_disk() {
        let (_state_dir, sp) = isolated_paths();
        let config = test_config();
        let membership = two_disk_membership();
        let fs = unlock_storage_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (fail_req, fail_out) = test_passphrase_fail("/dev/disk/by-id/virtio-disk2");
        let runner = base_two_disk_runner()
            .with_output_stdin(fail_req, MOUNT_TEST_PASSPHRASE_BYTES.to_vec(), fail_out)
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                unlock_ok_raw("cryptsetup isLuks"),
            );
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk1", "braid-disk1")
                .with_output_stdin(
                    CmdRequest::CryptsetupLuksOpen {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                        mapper: MapperName("braid-disk2".into()),
                    },
                    MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
                    unlock_err_raw(
                        "cryptsetup open",
                        2,
                        "No key available with this passphrase.",
                    ),
                );
        let tmp = unlock_passphrase_file();

        let result = cmd_unlock(
            &runner,
            &fs,
            &UnlockParams {
                config: &config,
                membership: &membership,
                paths: &sp,
                passphrase_stdin: false,
                passphrase_file: Some(tmp.path()),
                key_file: None,
                allow_degraded: false,
                dry_run: false,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
            },
        );

        let err = result.expect_err("should fail when disk2 rejects passphrase");
        let msg = err.to_string();
        assert!(
            msg.contains("disk2"),
            "error should name the failing disk, got: {msg}"
        );
        assert!(
            !msg.contains("disk1"),
            "preflight rejection should not report disk1 as the verification disk, got: {msg}"
        );
        assert!(
            !msg.contains("Wrong passphrase?"),
            "error should not say 'Wrong passphrase?', got: {msg}"
        );
    }

    // Intent: unlock reports the original post-open mount failure even if
    //   best-effort cleanup also fails.
    // Why it exists: a cleanup regression must not replace the primary user
    //   action failure with a secondary `cryptsetup close` error.
    // Scenario: two disks open successfully, mount fails, and one mapper stays
    //   busy through cleanup retries.
    #[test]
    fn cmd_unlock_preserves_mount_error_when_cleanup_close_fails() {
        let (_state_dir, sp) = isolated_paths();
        let config = test_config();
        let membership = two_disk_membership();
        let fs = unlock_storage_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1",
            "/dev/mapper/braid-disk2",
        ]);

        let mp = MountPoint("/mnt/storage".to_owned());
        let (scan_req, scan_out) = unlock_btrfs_device_scan_ok();
        let runner = base_two_disk_runner();
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk1", "braid-disk1");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk2", "braid-disk2")
                .with_output(scan_req, scan_out)
                .with_output(
                    CmdRequest::Mount {
                        device: "/dev/mapper/braid-disk1".into(),
                        mount_point: mp,
                    },
                    unlock_err_raw("mount", 32, "wrong fs type"),
                )
                .with_output(
                    CmdRequest::BtrfsDeviceScanForget {
                        devices: vec![
                            "/dev/mapper/braid-disk1".into(),
                            "/dev/mapper/braid-disk2".into(),
                        ],
                    },
                    unlock_ok_raw("btrfs device scan --forget"),
                )
                .with_output(
                    CmdRequest::CryptsetupClose {
                        mapper: MapperName("braid-disk1".into()),
                    },
                    unlock_err_raw("cryptsetup close", 5, "busy"),
                )
                .with_output(
                    CmdRequest::CryptsetupClose {
                        mapper: MapperName("braid-disk2".into()),
                    },
                    unlock_ok_raw("cryptsetup close"),
                );
        let tmp = unlock_passphrase_file();

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_unlock(
                &runner,
                &fs,
                &UnlockParams {
                    config: &config,
                    membership: &membership,
                    paths: &sp,
                    passphrase_stdin: false,
                    passphrase_file: Some(tmp.path()),
                    key_file: None,
                    allow_degraded: false,
                    dry_run: false,
                    backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(
                    ),
                },
            ));
        });
        let err = result
            .expect("cmd_unlock should run")
            .expect_err("mount failure should fail unlock");

        match &err {
            UnlockError::Mount(MountError::MountFailed(msg)) => {
                assert!(
                    msg.contains("mount failed (exit 32): wrong fs type"),
                    "primary error should remain the mount failure, got: {msg}"
                );
            }
            other => panic!("expected mount failure, got: {other:?}"),
        }
        assert!(
            captured.contains("cleanup failed: one or more LUKS mappers opened by this command"),
            "cleanup failure guidance should be emitted, got: {captured:?}"
        );
        assert!(
            runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupClose {
                    mapper
                } if mapper.as_str() == "braid-disk2"
            )),
            "cleanup should keep closing later mappers after the busy close"
        );
    }

    // Intent: the unlock_btrfs_balance_status_paused fixture body is the
    //   bytes that classify as Paused through the same parser and emitter path
    //   cmd_unlock takes post-mount, and the warning text matches production.
    // Why it exists: unlock_warns_on_paused_balance only asserts Ok(()), so
    //   parser, fixture-body, or warning-literal drift could otherwise silently
    //   downgrade the warning.
    // Scenario: feed the fixture output through status::emit_paused_balance_warning
    //   against a Vec<u8> writer; expect the warning flag and exact output.
    #[test]
    fn unlock_btrfs_balance_status_paused_classifies_as_paused() {
        let mp = MountPoint("/mnt/storage".to_owned());
        let (req, out) = unlock_btrfs_balance_status_paused(&mp);
        let runner = MockRunner::default().with_output(req, out);
        let mut sink = Vec::new();

        let warned = crate::status::emit_paused_balance_warning(&runner, &mp, &mut sink);

        assert!(warned, "fixture body should classify as paused balance");
        let output = String::from_utf8(sink).expect("warning is utf-8");
        let expected = concat!(
            "\n",
            "  paused balance detected -- will not auto-resume\n",
            "    resume:  btrfs balance resume /mnt/storage\n",
            "    cancel:  btrfs balance cancel /mnt/storage\n",
        );
        assert_eq!(output, expected);
    }

    // Intent: when a paused balance is detected after mount, unlock must still
    //   return Ok(()); the warning is informational, not an error.
    // Why it exists: the post-mount balance check must not accidentally convert
    //   an informational warning into a failure that breaks auto-unlock.
    // Scenario: 3-disk RAID1, all healthy. A balance was paused before lock; on
    //   re-unlock, skip_balance prevents kernel auto-resume and unlock succeeds.
    #[test]
    fn unlock_warns_on_paused_balance() {
        let (_state_dir, sp) = isolated_paths();
        let config = test_config();
        let membership = unlock_three_disk_membership();
        let fs = unlock_storage_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ]);

        let mp = MountPoint("/mnt/storage".to_owned());
        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "11111111-1111-1111-1111-111111111111",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );
        let (uuid3_req, uuid3_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk3",
            "33333333-3333-3333-3333-333333333333",
        );
        let (scan_req, scan_out) = unlock_btrfs_device_scan_ok();
        let (balance_req, balance_out) = unlock_btrfs_balance_status_paused(&mp);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck { path: mp.clone() },
                unlock_err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output(uuid3_req, uuid3_out)
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/disk/by-id/virtio-disk3",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2", "braid-disk3"]);
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk1");
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk2");
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk3");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk1", "braid-disk1");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk2", "braid-disk2");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk3", "braid-disk3");
        let runner = runner.with_output(scan_req, scan_out);
        let runner = unlock_with_mount_ok(runner, "/dev/mapper/braid-disk1", &mp)
            .with_output(balance_req, balance_out);
        let tmp = unlock_passphrase_file();

        let result = cmd_unlock(
            &runner,
            &fs,
            &UnlockParams {
                config: &config,
                membership: &membership,
                paths: &sp,
                passphrase_stdin: false,
                passphrase_file: Some(tmp.path()),
                key_file: None,
                allow_degraded: false,
                dry_run: false,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
            },
        );

        // The paused balance warning must not cause unlock to fail.
        result.expect("unlock should succeed even with paused balance");
    }

    // Intent: dry-run success for two closed disks renders probe notes above
    //   the step block, and the block still carries the expected open, scan,
    //   and mount steps.
    // Why it exists: a regression that drops probe notes, drops steps, or swaps
    //   their order would break the probe-context-before-steps contract.
    // Scenario: 2-disk pool, both present and closed, `--dry-run`.
    #[test]
    fn plan_unlock_dry_run_render_2_closed_disks() {
        let (_state_dir, sp) = isolated_paths();
        let config = test_config();
        let membership = two_disk_membership();
        let fs = unlock_storage_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);
        let runner = base_two_disk_runner();

        let params = UnlockParams {
            config: &config,
            membership: &membership,
            paths: &sp,
            passphrase_stdin: false,
            passphrase_file: None,
            key_file: None,
            allow_degraded: false,
            dry_run: true,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };

        let rendered = plan_unlock(&runner, &fs, &params)
            .expect("plan_unlock should succeed on 2-disk closed pool")
            .preview()
            .render();

        let note1 = "[ok]   disk disk1: found\n";
        let note2 = "[ok]   disk disk2: found\n";
        let scan = "btrfs device scan";

        let pos1 = rendered
            .find(note1)
            .unwrap_or_else(|| panic!("probe note for disk1 missing: {rendered:?}"));
        let pos2 = rendered
            .find(note2)
            .unwrap_or_else(|| panic!("probe note for disk2 missing: {rendered:?}"));
        let scan_pos = rendered
            .find(scan)
            .unwrap_or_else(|| panic!("btrfs device scan step missing: {rendered:?}"));

        assert!(
            pos1 < pos2 && pos2 < scan_pos,
            "probe notes must render before the step block, got: {rendered:?}",
        );

        assert!(
            rendered.contains("LUKS open /dev/disk/by-id/virtio-disk1"),
            "expected disk1 LUKS open step, got: {rendered:?}",
        );
        assert!(
            rendered.contains("LUKS open /dev/disk/by-id/virtio-disk2"),
            "expected disk2 LUKS open step, got: {rendered:?}",
        );
        assert!(
            rendered.contains("mount"),
            "expected mount step, got: {rendered:?}",
        );
    }

    // Intent: with `--key-file <path>`, dry-run preview emits the keyfile
    //   cryptsetup invocation, not the passphrase-via-stdin form.
    // Why it exists: `compile_open_steps` is the only place the keyfile dry-run
    //   branch is rendered, so the preview needs a direct assertion.
    // Scenario: auto-unlock user runs `braid unlock --key-file
    //   /run/keys/braid.key --dry-run` against a 2-disk closed pool.
    #[test]
    fn plan_unlock_dry_run_render_2_closed_disks_with_key_file() {
        let (_state_dir, sp) = isolated_paths();
        let config = test_config();
        let membership = two_disk_membership();
        let fs = unlock_storage_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);
        let runner = base_two_disk_runner();

        let params = UnlockParams {
            config: &config,
            membership: &membership,
            paths: &sp,
            passphrase_stdin: false,
            passphrase_file: None,
            key_file: Some(Path::new("/run/keys/braid.key")),
            allow_degraded: false,
            dry_run: true,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };

        let rendered = plan_unlock(&runner, &fs, &params)
            .expect("plan_unlock should succeed on 2-disk closed pool")
            .preview()
            .render();

        assert!(
            rendered.contains("LUKS open /dev/disk/by-id/virtio-disk1"),
            "expected disk1 LUKS open step, got: {rendered:?}",
        );
        assert!(
            rendered.contains("LUKS open /dev/disk/by-id/virtio-disk2"),
            "expected disk2 LUKS open step, got: {rendered:?}",
        );
        assert!(
            rendered.contains(
                "cryptsetup open --type luks --key-file /run/keys/braid.key --keyfile-size 4096"
            ),
            "expected keyfile LUKS open argv, got: {rendered:?}",
        );
        assert!(
            !rendered.contains("--key-file=-"),
            "dry-run should not render passphrase-via-stdin argv, got: {rendered:?}",
        );
    }

    // Intent: when the pool is already mounted, `plan_unlock` returns an
    //   `UnlockPlan` with no open plan, the AlreadyMounted Info note, and zero
    //   steps; preview rendering emits exactly that note.
    // Why it exists: the note-only success path must not regress to the generic
    //   `nothing to do.` fallback or append spurious steps.
    // Scenario: 2-disk pool whose mountpoint check returns exit 0.
    #[test]
    fn plan_unlock_note_only_success_when_already_mounted() {
        let (_state_dir, sp) = isolated_paths();
        let config = test_config();
        let membership = two_disk_membership();
        let fs = unlock_storage_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);
        let runner = base_two_disk_runner().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            unlock_ok_raw("mountpoint"),
        );

        let params = UnlockParams {
            config: &config,
            membership: &membership,
            paths: &sp,
            passphrase_stdin: false,
            passphrase_file: None,
            key_file: None,
            allow_degraded: false,
            dry_run: true,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };

        let plan = plan_unlock(&runner, &fs, &params)
            .expect("plan_unlock should succeed on already-mounted pool");

        assert!(
            plan.open_plan.is_none(),
            "open_plan must be None on already-mounted pool",
        );
        assert!(
            plan.steps.is_empty(),
            "steps must be empty on already-mounted pool, got: {:?}",
            plan.steps,
        );

        let rendered = plan.preview().render();
        assert_eq!(
            rendered, "pool already mounted at /mnt/storage\n",
            "note-only success must render exactly the Info note",
        );
    }

    // Intent: a degraded refusal at the planner boundary preserves per-disk
    //   probe notes on `PlanFailure::notes` in probe order and routes the
    //   error as `UnlockError::Mount(MountError::DegradedRefused(_))`.
    // Why it exists: accumulated notes must survive the Err path so cmd_unlock
    //   can render context before the refusal message.
    // Scenario: 3-disk pool, disk3 classified as PresentNotLuks, no
    //   `--allow-degraded`.
    #[test]
    fn plan_unlock_preserves_notes_on_degraded_refused() {
        let (_state_dir, sp) = isolated_paths();
        let config = test_config();
        let membership = unlock_three_disk_membership();
        let fs = unlock_storage_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let mp = MountPoint("/mnt/storage".to_owned());
        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "11111111-1111-1111-1111-111111111111",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );
        let (uuid3_req, uuid3_out) = unlock_luks_uuid_not_luks("/dev/disk/by-id/virtio-disk3");
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck { path: mp },
                unlock_err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output(uuid3_req, uuid3_out)
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);

        let params = UnlockParams {
            config: &config,
            membership: &membership,
            paths: &sp,
            passphrase_stdin: false,
            passphrase_file: None,
            key_file: None,
            allow_degraded: false,
            dry_run: true,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };

        let failure = match plan_unlock(&runner, &fs, &params) {
            Ok(_) => panic!("degraded refusal must surface as Err"),
            Err(failure) => failure,
        };

        let per_disk: Vec<&PreviewNote> = failure
            .notes
            .iter()
            .filter(|n| matches!(n, PreviewNote::PerDisk { .. }))
            .collect();
        assert_eq!(
            per_disk.len(),
            3,
            "PlanFailure::notes must carry one per-disk note per membership disk, got: {:?}",
            failure.notes,
        );
        assert!(
            matches!(
                per_disk[0],
                PreviewNote::PerDisk { name, level: NoteLevel::Ok, .. } if name == "disk1",
            ),
            "first note must be disk1 Ok, got: {:?}",
            per_disk[0],
        );
        assert!(
            matches!(
                per_disk[1],
                PreviewNote::PerDisk { name, level: NoteLevel::Ok, .. } if name == "disk2",
            ),
            "second note must be disk2 Ok, got: {:?}",
            per_disk[1],
        );
        assert!(
            matches!(
                per_disk[2],
                PreviewNote::PerDisk { name, level: NoteLevel::Skip, .. } if name == "disk3",
            ),
            "third note must be disk3 Skip, got: {:?}",
            per_disk[2],
        );

        let err = failure.error;
        assert!(
            matches!(&err, UnlockError::Mount(MountError::DegradedRefused(_))),
            "expected DegradedRefused, got: {err:?}",
        );
    }

    // Intent: `cmd_unlock` warns when its best-effort post-mount
    //   `probe_pool` returns an error, while still succeeding.
    // Why it exists: post-mount enrichment failures used to disappear
    //   silently, leaving operators no hint that pool.json metadata refresh
    //   was skipped.
    // Scenario: 2-disk pool, clean unlock and mount. Mountinfo declares
    //   `/mnt/storage` as btrfs, but `btrfs filesystem show` fails during the
    //   best-effort metadata probe.
    #[test]
    fn unlock_warns_when_post_mount_probe_errors() {
        let (_state_dir, sp) = isolated_paths();
        let config = test_config();
        let membership = two_disk_membership();
        let fs = unlock_storage_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let mp = MountPoint("/mnt/storage".to_owned());
        let (scan_req, scan_out) = unlock_btrfs_device_scan_ok();
        let (balance_req, balance_out) = unlock_btrfs_balance_status_idle(&mp);
        let runner = base_two_disk_runner();
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk1", "braid-disk1");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk2", "braid-disk2");
        let runner = unlock_with_mount_ok(
            runner.with_output(scan_req, scan_out),
            "/dev/mapper/braid-disk1",
            &mp,
        )
        .with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: mp.clone(),
            },
            unlock_err_raw("btrfs filesystem show", 1, "no devices found"),
        )
        .with_output(balance_req, balance_out);
        let tmp = unlock_passphrase_file();

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_unlock(
                &runner,
                &fs,
                &UnlockParams {
                    config: &config,
                    membership: &membership,
                    paths: &sp,
                    passphrase_stdin: false,
                    passphrase_file: Some(tmp.path()),
                    key_file: None,
                    allow_degraded: false,
                    dry_run: false,
                    backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(
                    ),
                },
            ));
        });

        result
            .expect("cmd_unlock should run")
            .expect("unlock should tolerate post-mount probe errors");
        assert_eq!(
            captured
                .matches("Warning: failed to probe pool for metadata refresh: ")
                .count(),
            1,
            "expected one metadata-refresh warning, got: {captured:?}"
        );
        assert!(
            captured.contains("no devices found"),
            "warning should include the probe error detail, got: {captured:?}"
        );
    }

    // Intent: `cmd_unlock` must tolerate a post-mount `probe_pool` that returns
    //   `Ok(PoolState { mounted: false, devices: vec![], ... })` without
    //   enriching pool.json, without failing, and without emitting the
    //   probe-error warning.
    // Why it exists: post-mount enrichment is best-effort; a readable
    //   mountinfo without the target must not become a hard failure or stale
    //   metadata write, and must stay distinct from real probe errors.
    // Scenario: 3-disk pool, clean mount. The best-effort post-mount probe sees
    //   well-formed mountinfo with no `/mnt/storage` entry.
    #[test]
    fn unlock_tolerates_post_mount_probe_mounted_false() {
        let (_state_dir, sp) = isolated_paths();
        let config = test_config();
        let membership = unlock_three_disk_membership();
        membership::save_membership(&membership, &sp)
            .expect("seed pool.json for assertion baseline");

        // Use the rootfs-only mountinfo body so the post-mount probe sees no
        // `/mnt/storage` entry and must leave pool.json unenriched.
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ]);

        let mp = MountPoint("/mnt/storage".to_owned());
        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "11111111-1111-1111-1111-111111111111",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );
        let (uuid3_req, uuid3_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk3",
            "33333333-3333-3333-3333-333333333333",
        );
        let (scan_req, scan_out) = unlock_btrfs_device_scan_ok();
        let (balance_req, balance_out) = unlock_btrfs_balance_status_idle(&mp);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck { path: mp.clone() },
                unlock_err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output(uuid3_req, uuid3_out)
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/disk/by-id/virtio-disk3",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2", "braid-disk3"]);
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk1");
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk2");
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk3");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk1", "braid-disk1");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk2", "braid-disk2");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk3", "braid-disk3");
        let runner = runner.with_output(scan_req, scan_out);
        let runner = unlock_with_mount_ok(runner, "/dev/mapper/braid-disk1", &mp)
            .with_output(balance_req, balance_out);
        let tmp = unlock_passphrase_file();

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_unlock(
                &runner,
                &fs,
                &UnlockParams {
                    config: &config,
                    membership: &membership,
                    paths: &sp,
                    passphrase_stdin: false,
                    passphrase_file: Some(tmp.path()),
                    key_file: None,
                    allow_degraded: false,
                    dry_run: false,
                    backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(
                    ),
                },
            ));
        });

        result
            .expect("cmd_unlock should run")
            .expect("unlock should tolerate probe_pool returning mounted=false");
        assert!(
            !captured.contains("Warning: failed to probe pool for metadata refresh: "),
            "mounted=false race should not emit probe-error warning, got: {captured:?}"
        );

        let loaded = membership::load_membership(&sp)
            .expect("pool.json should still be loadable after unlock");
        for name in ["disk1", "disk2", "disk3"] {
            let disk_name = crate::types::DiskName::parse(name).unwrap();
            let member = loaded
                .by_name(&disk_name)
                .map(|(_, member)| member)
                .unwrap_or_else(|| panic!("missing disk {name} in pool.json"));
            assert!(
                member.devid.is_none(),
                "{name}.devid must remain None when probe_pool returns \
                 mounted=false, got: {:?}",
                member.devid
            );
        }
    }

    // Intent: `cmd_unlock` must tolerate a post-mount `probe_pool` that
    //   returns `Err(ProbeError::Parse(_))` without enriching membership
    //   metadata and without failing.
    // Why it exists: post-mount enrichment is best-effort. The sibling test
    //   pins the `Ok(mounted=false)` race; this pins the realistic Err branch
    //   -- a parser drift after a nixpkgs bump.
    // Scenario: 3-disk pool, clean mount. Mountinfo declares `/mnt/storage` as
    //   btrfs, but `btrfs filesystem show` returns malformed stdout lacking
    //   `Total devices`, so the post-mount probe yields
    //   `Err(ProbeError::Parse(_))`.
    #[test]
    fn unlock_tolerates_post_mount_probe_err() {
        let (_state_dir, sp) = isolated_paths();
        let config = test_config();
        let membership = unlock_three_disk_membership();
        membership::save_membership(&membership, &sp)
            .expect("seed pool.json for assertion baseline");

        let fs = unlock_storage_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ]);

        let mp = MountPoint("/mnt/storage".to_owned());
        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "11111111-1111-1111-1111-111111111111",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );
        let (uuid3_req, uuid3_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk3",
            "33333333-3333-3333-3333-333333333333",
        );
        let (scan_req, scan_out) = unlock_btrfs_device_scan_ok();
        let (balance_req, balance_out) = unlock_btrfs_balance_status_idle(&mp);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck { path: mp.clone() },
                unlock_err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output(uuid3_req, uuid3_out)
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/disk/by-id/virtio-disk3",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2", "braid-disk3"]);
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk1");
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk2");
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk3");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk1", "braid-disk1");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk2", "braid-disk2");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk3", "braid-disk3");
        let runner = runner.with_output(scan_req, scan_out);
        let runner = unlock_with_mount_ok(runner, "/dev/mapper/braid-disk1", &mp)
            .with_output(balance_req, balance_out);
        let runner = runner.with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: mp.clone(),
            },
            RawCommandOutput {
                cmd: "btrfs filesystem show".to_owned(),
                stdout: "This is not btrfs output at all\nrandom garbage data".to_owned(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let tmp = unlock_passphrase_file();

        let result = cmd_unlock(
            &runner,
            &fs,
            &UnlockParams {
                config: &config,
                membership: &membership,
                paths: &sp,
                passphrase_stdin: false,
                passphrase_file: Some(tmp.path()),
                key_file: None,
                allow_degraded: false,
                dry_run: false,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
            },
        );

        result.expect("unlock should tolerate probe_pool returning Err(ProbeError::Parse(_))");

        let loaded = membership::load_membership(&sp)
            .expect("pool.json should still be loadable after unlock");
        for name in ["disk1", "disk2", "disk3"] {
            let disk_name = crate::types::DiskName::parse(name).unwrap();
            let member = loaded
                .by_name(&disk_name)
                .map(|(_, member)| member)
                .unwrap_or_else(|| panic!("missing disk {name} in pool.json"));
            assert!(
                member.devid.is_none(),
                "{name}.devid must remain None when probe_pool returns Err, got: {:?}",
                member.devid
            );
            assert!(
                member.added_at.is_none(),
                "{name}.added_at must remain None when probe_pool returns Err, got: {:?}",
                member.added_at
            );
        }
    }

    // Intent: `cmd_unlock` must tolerate a `save_membership` failure on the
    //   post-mount enrichment path without failing the command.
    // Why it exists: the mount has already succeeded by the time enrichment
    //   runs; using `?` on the best-effort pool.json write would report a
    //   failed unlock even though the pool is online.
    // Scenario: 3-disk pool, clean mount, and a successful post-mount probe,
    //   but pool.json is a directory so the atomic save cannot replace it.
    #[test]
    fn unlock_tolerates_post_mount_save_membership_failure() {
        let (_state_dir, sp) = isolated_paths();
        let config = test_config();
        let membership = unlock_three_disk_membership();
        std::fs::create_dir(&sp.pool_json()).expect("pool.json blocker directory");

        let fs = unlock_storage_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ]);

        let mp = MountPoint("/mnt/storage".to_owned());
        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "11111111-1111-1111-1111-111111111111",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );
        let (uuid3_req, uuid3_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk3",
            "33333333-3333-3333-3333-333333333333",
        );
        let (scan_req, scan_out) = unlock_btrfs_device_scan_ok();
        let (balance_req, balance_out) = unlock_btrfs_balance_status_idle(&mp);

        let closed_status = |mapper: &str| RawCommandOutput {
            cmd: format!("cryptsetup status {mapper}"),
            stdout: String::new(),
            stderr: format!("/dev/mapper/{mapper} is inactive.\n"),
            exit_status: 4,
        };
        let active_status = |mapper: &str, underlying: &str| RawCommandOutput {
            cmd: format!("cryptsetup status {mapper}"),
            stdout: format!(
                "/dev/mapper/{mapper} is active and is in use.\n\
                 \ttype:    LUKS2\n\
                 \tcipher:  aes-xts-plain64\n\
                 \tdevice:  {underlying}\n\
                 \tsector size:  512\n"
            ),
            stderr: String::new(),
            exit_status: 0,
        };

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck { path: mp.clone() },
                unlock_err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output(uuid3_req, uuid3_out)
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/disk/by-id/virtio-disk3",
            ])
            // Status is checked during planning, checked again immediately
            // before open, then checked by the post-mount metadata probe.
            .with_output_sequence(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName("braid-disk1".into()),
                },
                vec![
                    closed_status("braid-disk1"),
                    closed_status("braid-disk1"),
                    active_status("braid-disk1", "/dev/disk/by-id/virtio-disk1"),
                ],
            )
            .with_output_sequence(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName("braid-disk2".into()),
                },
                vec![
                    closed_status("braid-disk2"),
                    closed_status("braid-disk2"),
                    active_status("braid-disk2", "/dev/disk/by-id/virtio-disk2"),
                ],
            )
            .with_output_sequence(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName("braid-disk3".into()),
                },
                vec![
                    closed_status("braid-disk3"),
                    closed_status("braid-disk3"),
                    active_status("braid-disk3", "/dev/disk/by-id/virtio-disk3"),
                ],
            );
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk1");
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk2");
        let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk3");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk1", "braid-disk1");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk2", "braid-disk2");
        let runner =
            unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk3", "braid-disk3");
        let runner = runner.with_output(scan_req, scan_out).with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: mp.clone(),
            },
            RawCommandOutput {
                cmd: "btrfs filesystem show".to_owned(),
                stdout: "\
Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
\tTotal devices 3 FS bytes used 16.17MiB\n\
\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n\
\tdevid    3 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk3\n"
                    .to_owned(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let runner = unlock_with_mount_ok(runner, "/dev/mapper/braid-disk1", &mp)
            .with_output(balance_req, balance_out);
        let tmp = unlock_passphrase_file();

        let (result, stderr) = super::unlock_stderr_capture::capture(|| {
            cmd_unlock(
                &runner,
                &fs,
                &UnlockParams {
                    config: &config,
                    membership: &membership,
                    paths: &sp,
                    passphrase_stdin: false,
                    passphrase_file: Some(tmp.path()),
                    key_file: None,
                    allow_degraded: false,
                    dry_run: false,
                    backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(
                    ),
                },
            )
        });

        result.expect("unlock should tolerate post-mount save_membership failure");
        assert!(
            runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::Mount {
                    device,
                    mount_point
                } if device == "/dev/mapper/braid-disk1" && mount_point == &mp
            )),
            "unlock should have mounted the pool before the save failure"
        );
        assert_eq!(
            stderr
                .matches("Warning: failed to save enriched membership: ")
                .count(),
            1,
            "expected one save-membership warning, got: {stderr:?}"
        );
    }

    // Intent: when every membership disk's mapper is already open, `cmd_unlock`
    //   must take the mount-only branch and never call `resolve_credential`.
    // Why it exists: a refactor that hoists credential resolution above the
    //   empty-unlock branch would break redundant unlock UX and error on
    //   operators who supplied no real credentials for an already-open pool.
    // Scenario: 3-disk pool, all mappers already open, with `passphrase_file`
    //   pointing at a path that does not exist.
    #[test]
    fn cmd_unlock_skips_credential_resolution_when_nothing_to_unlock() {
        let (_state_dir, sp) = isolated_paths();
        let config = test_config();
        let membership = unlock_three_disk_membership();

        // Use the rootfs-only mountinfo body; the pool starts unmounted while
        // all three by-id devices and all three mapper paths exist.
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
            "/dev/mapper/braid-disk1",
            "/dev/mapper/braid-disk2",
            "/dev/mapper/braid-disk3",
        ]);

        let mp = MountPoint("/mnt/storage".to_owned());
        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "11111111-1111-1111-1111-111111111111",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );
        let (uuid3_req, uuid3_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk3",
            "33333333-3333-3333-3333-333333333333",
        );
        let (scan_req, scan_out) = unlock_btrfs_device_scan_ok();
        let (balance_req, balance_out) = unlock_btrfs_balance_status_idle(&mp);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck { path: mp.clone() },
                unlock_err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output(uuid3_req, uuid3_out)
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/disk/by-id/virtio-disk3",
            ]);
        let runner = unlock_with_three_mappers_open(runner)
            // No CryptsetupTestPassphrase / CryptsetupLuksOpen mocks -- if
            // the code takes the unlock-and-mount branch, these lookups miss.
            .with_output(scan_req, scan_out);
        let runner = unlock_with_mount_ok(runner, "/dev/mapper/braid-disk1", &mp)
            .with_output(balance_req, balance_out);

        // passphrase_file points at a path that does not exist. If dispatch
        // regresses and hoists resolve_credential above the empty-unlock check,
        // read_passphrase will fail before this test reaches Ok(()).
        let bogus = std::path::PathBuf::from("/definitely/not/a/real/path/passphrase");

        let result = cmd_unlock(
            &runner,
            &fs,
            &UnlockParams {
                config: &config,
                membership: &membership,
                paths: &sp,
                passphrase_stdin: false,
                passphrase_file: Some(&bogus),
                key_file: None,
                allow_degraded: false,
                dry_run: false,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
            },
        );

        result.expect(
            "unlock with all mappers already open must take the mount-only \
             branch and never attempt to read the (nonexistent) passphrase file",
        );
    }
}
