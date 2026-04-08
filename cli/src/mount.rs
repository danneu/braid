use crate::cmd::{CmdError, CmdRequest, CommandRunner, Step};
use crate::config::{mapper_name, Config};
use crate::luks::{self, LuksError};
use crate::membership::PoolMembership;
use crate::probe::{self, Filesystem, ProbeError};
use crate::types::{ByIdPath, ConfigDiskState, MountPoint};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("{0}")]
    Probe(#[from] ProbeError),
    #[error("{0}")]
    Luks(#[from] LuksError),
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("{0}")]
    Failed(String),
    #[error("{0}")]
    MountFailed(String),
    #[error("{0}")]
    DegradedRefused(String),
}

/// A fully-resolved credential ready to drive `cryptsetup open`. Owned (no
/// lifetime parameter); plaintext is scrubbed on drop via `Zeroizing`.
///
/// Constructed by `resolve_credential` from a `CredentialSource`. Callers
/// hold the resolved value and pass it (by reference, optionally) to
/// `execute_open_plan`, which strict-validates that the credential's
/// presence matches the plan's `to_unlock` state.
pub enum OpenCredential {
    Passphrase(Zeroizing<String>),
    KeyFile(PathBuf),
}

/// Where to read a credential from. Mirrors the existing
/// `passphrase_stdin`/`passphrase_file`/`key_file` fields on `UnlockParams`
/// and `RecoverParams`. Constructed at the callsite, consumed by
/// `resolve_credential`.
pub struct CredentialSource<'a> {
    pub passphrase_stdin: bool,
    pub passphrase_file: Option<&'a Path>,
    pub key_file: Option<&'a Path>,
}

/// Resolve a credential source into an owned, fully-resolved
/// `OpenCredential`. ALWAYS reads — callers decide whether to invoke this,
/// because the "should we prompt now?" rule differs by command:
///
/// - `cmd_unlock` skips this call entirely when `plan.to_unlock` is empty
///   (the no-prompt-when-all-mappers-open UX rule).
/// - `cmd_recover` calls this whenever the pool is not yet mounted, even
///   if the initial plan's `to_unlock` is empty, because the post-mount
///   relock cycle will close every mapper and need to reopen them.
///
/// Resolution order: `key_file` (if provided) → passphrase
/// (file/stdin/TTY).
pub fn resolve_credential(
    source: &CredentialSource<'_>,
) -> Result<OpenCredential, MountError> {
    if let Some(kf) = source.key_file {
        return Ok(OpenCredential::KeyFile(kf.to_path_buf()));
    }
    let pp = luks::read_passphrase(source.passphrase_file, source.passphrase_stdin)?;
    Ok(OpenCredential::Passphrase(Zeroizing::new(pp)))
}

/// Status line tag for output.
fn tag(label: &str) -> String {
    format!("[{:<4}]", label)
}

/// Why a membership disk is missing from the pool at unlock time.
/// Used to format the structured `DegradedRefused` error in probe order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingReason {
    /// Device file does not exist on the host (`ConfigDiskState::Absent`).
    Unplugged,
    /// Device exists but `cryptsetup isLuks` exits non-zero — the LUKS
    /// magic is gone or otherwise unrecognizable. Distinct from Damaged
    /// because there is no metadata structure left to repair via
    /// `cryptsetup repair`.
    LuksHeaderUnreadable,
    /// Device exists, has valid LUKS magic, but `cryptsetup luksDump`
    /// fails to parse the metadata blocks. Potentially repairable via
    /// `cryptsetup repair --type luks2 <device>`.
    LuksHeaderDamaged,
}

impl MissingReason {
    fn is_luks_header_state(self) -> bool {
        matches!(
            self,
            MissingReason::LuksHeaderUnreadable | MissingReason::LuksHeaderDamaged
        )
    }
}

/// Format a structured `DegradedRefused` error message that names each
/// missing disk and the reason in probe order. Preserves the substrings
/// `"refusing to mount degraded"` and
/// `"braid <command_hint> --allow-degraded"` that existing tests anchor on.
///
/// `missing` is guaranteed non-empty by the caller.
fn format_degraded_refused(
    missing: &[(String, MissingReason)],
    command_hint: &str,
) -> String {
    let total = missing.len();
    let header = if total == 1 {
        "pool has 1 missing device — refusing to mount degraded".to_owned()
    } else {
        format!("pool has {total} missing devices — refusing to mount degraded")
    };

    let mut lines = vec![header];
    for (name, reason) in missing {
        let reason_text = match reason {
            MissingReason::Unplugged => "not found (unplugged?)",
            MissingReason::LuksHeaderUnreadable => "LUKS header unreadable",
            MissingReason::LuksHeaderDamaged => "LUKS header metadata damaged",
        };
        lines.push(format!("  {name}: {reason_text}"));
    }
    lines.push("new writes would have ZERO redundancy (single-profile chunks)".to_owned());
    lines.push(format!("hint: braid {command_hint} --allow-degraded"));
    if missing.iter().any(|(_, r)| r.is_luks_header_state()) {
        lines.push("run 'braid doctor' for recovery guidance".to_owned());
    }
    lines.join("\n")
}

/// Result of the read-only probe + validate phase.
pub struct OpenPlan {
    /// Disks that need LUKS open (name, by_id pairs).
    pub to_unlock: Vec<(String, ByIdPath)>,
    /// At least one mapper was already open.
    pub any_open: bool,
    /// At least one membership disk was absent/damaged.
    pub any_missing_member: bool,
    /// Device path to use for mount (e.g. "/dev/mapper/braid-disk1").
    pub mount_device: String,
}

/// Probe membership disks, validate UUIDs, check degraded policy.
/// Returns the planning errors that `execute_open_plan` would otherwise
/// surface (degraded refusal, UUID mismatch, no unlockable disks).
/// No mutations — safe for dry-run.
///
/// Returns `Ok(None)` when pool is already mounted.
pub fn plan_open_pool<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    allow_degraded: bool,
    command_hint: &str,
) -> Result<Option<OpenPlan>, MountError> {
    let mount_point = config.mount_point();

    // 1. If pool already mounted → None
    let mp_result = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.clone(),
    })?;
    if mp_result.exit_status == 0 {
        eprintln!("pool already mounted at {mount_point}");
        return Ok(None);
    }

    // 2. Probe each membership disk
    let mut to_unlock = Vec::new();
    let mut any_open = false;
    let mut missing: Vec<(String, MissingReason)> = Vec::new();

    for (name, member) in &membership.disks {
        let probed = probe::probe_config_disk(runner, fs, name, &member.by_id)?;
        match &probed.state {
            ConfigDiskState::Absent => {
                eprintln!("{}  disk: {:<10}not found (unplugged?)", tag("skip"), name);
                missing.push((name.clone(), MissingReason::Unplugged));
            }
            ConfigDiskState::PresentNotLuks => {
                // Refine PresentNotLuks (luksUuid failed) into Unreadable vs
                // Damaged for diagnostic reporting only — do NOT propagate
                // this back into ConfigDiskState. add/replace must keep
                // seeing the coarse PresentNotLuks state to preserve their
                // destructive-format guards on potentially recoverable
                // damaged headers.
                let reason = match luks::probe_luks_header(runner, &member.by_id.0) {
                    luks::LuksHeaderState::Damaged => MissingReason::LuksHeaderDamaged,
                    // Unreadable, the inconsistent Ok-but-luksUuid-failed
                    // case, and ProbeFailed all collapse to Unreadable.
                    // Damaged is the only refinement we promote out of
                    // this branch because it has a distinct
                    // `cryptsetup repair` recovery story; everything else
                    // gets the conservative Unreadable label.
                    _ => MissingReason::LuksHeaderUnreadable,
                };
                let reason_text = match reason {
                    MissingReason::LuksHeaderDamaged => "LUKS header metadata damaged",
                    _ => "LUKS header unreadable",
                };
                eprintln!("{}  disk: {:<10}{reason_text}", tag("skip"), name);
                missing.push((name.clone(), reason));
            }
            ConfigDiskState::PresentLuks { uuid, mapper_open } => {
                if let Some(expected) = &member.luks_uuid
                    && expected != uuid {
                        return Err(MountError::Failed(format!(
                            "disk '{}' LUKS UUID mismatch at {}:\n  \
                             expected  {}\n  \
                             found     {}",
                            name, member.by_id, expected, uuid
                        )));
                    }

                if *mapper_open {
                    eprintln!("{}  disk: {:<10}already open", tag("ok"), name);
                    any_open = true;
                } else {
                    eprintln!("{}  disk: {:<10}found", tag("ok"), name);
                    to_unlock.push((name.clone(), member.by_id.clone()));
                }
            }
        }
    }

    // 3. If no disks to unlock AND none already open → error
    if to_unlock.is_empty() && !any_open {
        return Err(MountError::Failed("no unlockable disks found".into()));
    }

    let any_missing_member = !missing.is_empty();

    // 4. Degraded check (before any mutations)
    if any_missing_member && !allow_degraded {
        return Err(MountError::DegradedRefused(format_degraded_refused(
            &missing,
            command_hint,
        )));
    }

    // 5. Compute mount device
    let mount_key = to_unlock
        .first()
        .map(|(k, _)| k.as_str())
        .or_else(|| membership.disks.keys().next().map(|k| k.as_str()))
        .unwrap_or("unknown");
    let mount_device = format!("/dev/mapper/{}", mapper_name(mount_key).0);

    Ok(Some(OpenPlan {
        to_unlock,
        any_open,
        any_missing_member,
        mount_device,
    }))
}

/// Compile dry-run steps from a validated OpenPlan.
pub fn compile_open_steps(
    plan: &OpenPlan,
    mount_point: &MountPoint,
    key_file: Option<&Path>,
) -> Vec<Step> {
    let mut steps = Vec::new();

    for (name, by_id) in &plan.to_unlock {
        let mn = mapper_name(name);
        if let Some(kf) = key_file {
            steps.push(Step {
                risk: "safe",
                description: format!("LUKS open {} → {}", by_id, mn),
                commands: vec![CmdRequest::CryptsetupLuksOpenKeyFile {
                    device: by_id.0.clone(),
                    mapper: mn.0.clone(),
                    key_file_path: kf.display().to_string(),
                }],
            });
        } else {
            steps.push(Step {
                risk: "safe",
                description: format!("LUKS open {} → {}", by_id, mn),
                commands: vec![CmdRequest::CryptsetupLuksOpen {
                    device: by_id.0.clone(),
                    mapper: mn.0.clone(),
                }],
            });
        }
    }

    steps.push(Step {
        risk: "safe",
        description: "btrfs device scan".into(),
        commands: vec![CmdRequest::BtrfsDeviceScanAll],
    });

    if plan.any_missing_member {
        steps.push(Step {
            risk: "safe",
            description: format!("mount → {} (degraded)", mount_point),
            commands: vec![CmdRequest::MountWithOptions {
                device: plan.mount_device.clone(),
                mount_point: mount_point.clone(),
                options: vec!["degraded".to_owned()],
            }],
        });
    } else {
        steps.push(Step {
            risk: "safe",
            description: format!("mount → {}", mount_point),
            commands: vec![CmdRequest::Mount {
                device: plan.mount_device.clone(),
                mount_point: mount_point.clone(),
            }],
        });
    }

    steps
}

/// Classify an unlock-time failure against the LUKS header state of the
/// affected disk, producing the best user-facing error.
///
/// The four match arms each represent a distinct user story:
///
/// - `Unreadable` → corruption is confirmed severe; emit the off-system
///   backup guidance regardless of what cryptsetup originally said.
/// - `Damaged` → corruption is confirmed at the metadata level; emit
///   the `cryptsetup repair` guidance with a safe-backup warning.
/// - `Ok` → the header is intact, so the failure really is about the
///   caller-supplied context (passphrase, invariant, device state,
///   generic I/O error, etc.); use `ok_fallback` unchanged.
/// - `ProbeFailed` → we genuinely do not know whether the header is
///   sound, so we must NOT confidently pick a narrative. Emit a
///   dedicated "diagnosis incomplete" message that surfaces both the
///   original cryptsetup signal (`original_summary`) and the probe
///   error, without narrowing the cause to any particular class of
///   failure. This is load-bearing: `explain_open_failure` is called
///   from all four unlock-failure sites, including the generic
///   non-auth open-loop path, so the wording cannot assume the
///   underlying cause was auth-related.
fn explain_open_failure(
    disk_name: &str,
    device: &str,
    header_state: luks::LuksHeaderState,
    original_summary: &str,
    ok_fallback: MountError,
) -> MountError {
    match header_state {
        luks::LuksHeaderState::Unreadable => MountError::Failed(format!(
            "failed to unlock disk '{disk_name}' ({device}): {}",
            luks::luks_header_unreadable_guidance()
        )),
        luks::LuksHeaderState::Damaged => MountError::Failed(format!(
            "failed to unlock disk '{disk_name}' ({device}): {}",
            luks::luks_header_damaged_guidance(device)
        )),
        luks::LuksHeaderState::Ok => ok_fallback,
        luks::LuksHeaderState::ProbeFailed(probe_err) => MountError::Failed(format!(
            "failed to unlock disk '{disk_name}' ({device}): {original_summary}. \
             LUKS header diagnosis could not be completed: {probe_err}. \
             Cannot determine whether this failure is due to LUKS header damage \
             or the reported cryptsetup error — inspect the disk manually."
        )),
    }
}

/// Verify a passphrase against the first to-unlock disk and then open every
/// to-unlock disk with it. Called from the `OpenCredential::Passphrase`
/// arm of `execute_open_plan`. Mirrors the structure of the `KeyFile` arm:
/// `explain_open_failure` handles header-state classification for both
/// verification and per-disk open failures.
fn open_disks_with_passphrase<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    to_unlock: &[(String, ByIdPath)],
    passphrase: &str,
) -> Result<(), MountError> {
    let (ref first_name, ref first_by_id) = to_unlock[0];
    let ok = luks::verify_passphrase(runner, &first_by_id.0, passphrase)?;
    if !ok {
        let original_summary = format!("passphrase rejected on '{first_name}'");
        let ok_fallback = MountError::Failed(format!(
            "wrong passphrase (verified against {first_name})"
        ));
        let header_state = luks::probe_luks_header(runner, &first_by_id.0);
        return Err(explain_open_failure(
            first_name,
            &first_by_id.0,
            header_state,
            &original_summary,
            ok_fallback,
        ));
    }

    for (name, by_id) in to_unlock {
        if let Err(e) = luks::ensure_luks_open(runner, fs, name, by_id, passphrase) {
            let header_state = luks::probe_luks_header(runner, &by_id.0);
            let (original_summary, ok_fallback) = match &e {
                LuksError::OpenFailed {
                    exit_code: 2,
                    hint,
                    stderr,
                    ..
                } => (
                    format!(
                        "cryptsetup open rejected on '{name}' despite verified passphrase on '{first_name}' — {hint} ({stderr})"
                    ),
                    MountError::Failed(format!(
                        "failed to open disk '{}': passphrase was verified \
                         against '{}' but rejected here — {} ({}). \
                         If the passphrase is correct, the single-passphrase \
                         invariant may be violated by external LUKS manipulation",
                        name, first_name, hint, stderr
                    )),
                ),
                _ => {
                    let summary = format!("cryptsetup open failed on '{name}': {e}");
                    (summary, MountError::Luks(e))
                }
            };
            return Err(explain_open_failure(
                name,
                &by_id.0,
                header_state,
                &original_summary,
                ok_fallback,
            ));
        }
        eprintln!("{}  disk: {:<10}unlocked", tag("ok"), name);
    }

    Ok(())
}

/// Execute a pre-built `OpenPlan`: open LUKS devices (if any) and mount
/// the btrfs pool.
///
/// Phases: validate credential/plan agreement → open LUKS (if needed) →
/// btrfs device scan → mkdir + mount.
///
/// Planning + probing + validation lives in `plan_open_pool`, which the
/// caller invokes first. This function does NOT plan; it only executes.
///
/// `credential` must be `Some` iff `plan.to_unlock` is non-empty:
/// - `(false, false)` (need a credential, none provided) → `MountError::Failed`
/// - `(true, true)` (provided a credential we don't need) → `MountError::Failed`
/// - `(true, false)` and `(false, true)` are the normal cases.
///
/// Returns `Ok(true)` once the mount succeeds. (Callers detect the
/// already-mounted case earlier, by `plan_open_pool` returning `None`.)
pub fn execute_open_plan<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    plan: &OpenPlan,
    credential: Option<&OpenCredential>,
) -> Result<bool, MountError> {
    let mount_point = config.mount_point();

    // 1. Validate credential/plan agreement, STRICTLY in both directions.
    //    Each caller is expected to gate credential presence to match
    //    `plan.to_unlock` itself; mismatches mean a caller bug.
    match (credential.is_some(), plan.to_unlock.is_empty()) {
        (false, false) => {
            return Err(MountError::Failed(
                "internal: credential required for unlock but none was provided".into(),
            ));
        }
        (true, true) => {
            return Err(MountError::Failed(
                "internal: credential provided but plan has no disks to unlock".into(),
            ));
        }
        // (false, true): mount-only path. (true, false): normal unlock.
        _ => {}
    }

    // 2. If disks need opening → verify credential, then open each disk.
    if !plan.to_unlock.is_empty() {
        match credential.expect("checked above") {
            OpenCredential::KeyFile(kf) => {
                let kf = kf.as_path();
                let (ref first_name, ref first_by_id) = plan.to_unlock[0];
                let ok = luks::verify_key_file(runner, &first_by_id.0, kf)?;
                if !ok {
                    let original_summary = format!("keyfile rejected on '{first_name}'");
                    let ok_fallback = MountError::Failed(format!(
                        "wrong keyfile (verified against {first_name})"
                    ));
                    let header_state = luks::probe_luks_header(runner, &first_by_id.0);
                    return Err(explain_open_failure(
                        first_name,
                        &first_by_id.0,
                        header_state,
                        &original_summary,
                        ok_fallback,
                    ));
                }

                for (name, by_id) in &plan.to_unlock {
                    if let Err(e) =
                        luks::ensure_luks_open_with_key_file(runner, fs, name, by_id, kf)
                    {
                        let header_state = luks::probe_luks_header(runner, &by_id.0);
                        let (original_summary, ok_fallback) = match &e {
                            LuksError::OpenFailed {
                                exit_code: 2,
                                hint,
                                stderr,
                                ..
                            } => (
                                format!(
                                    "cryptsetup open rejected on '{name}' despite verified keyfile on '{first_name}' — {hint} ({stderr})"
                                ),
                                MountError::Failed(format!(
                                    "failed to open disk '{}': keyfile was verified against \
                                     '{}' but rejected here — {} ({}). \
                                     If the keyfile is correct, the single-passphrase \
                                     invariant may be violated by external LUKS manipulation",
                                    name, first_name, hint, stderr
                                )),
                            ),
                            _ => {
                                let summary = format!("cryptsetup open failed on '{name}': {e}");
                                (summary, MountError::Luks(e))
                            }
                        };
                        return Err(explain_open_failure(
                            name,
                            &by_id.0,
                            header_state,
                            &original_summary,
                            ok_fallback,
                        ));
                    }
                    eprintln!("{}  disk: {:<10}unlocked", tag("ok"), name);
                }
            }
            OpenCredential::Passphrase(pp) => {
                open_disks_with_passphrase(runner, fs, &plan.to_unlock, pp.as_str())?;
            }
        }
    }

    // 3. btrfs device scan
    let scan = runner.run(&CmdRequest::BtrfsDeviceScanAll)?;
    if scan.exit_status != 0 {
        return Err(MountError::Failed(format!(
            "btrfs device scan failed (exit {}): {}",
            scan.exit_status,
            scan.stderr.trim()
        )));
    }

    // 4. mkdir + mount
    let _ = std::fs::create_dir_all(mount_point.as_str());

    let mount_result = if plan.any_missing_member {
        runner.run(&CmdRequest::MountWithOptions {
            device: plan.mount_device.clone(),
            mount_point: mount_point.clone(),
            options: vec!["degraded".to_owned()],
        })?
    } else {
        runner.run(&CmdRequest::Mount {
            device: plan.mount_device.clone(),
            mount_point: mount_point.clone(),
        })?
    };

    if mount_result.exit_status != 0 {
        return Err(MountError::MountFailed(format!(
            "mount failed (exit {}): {}",
            mount_result.exit_status,
            mount_result.stderr.trim()
        )));
    }

    eprintln!("{}  {:<10}mounted {}", tag("ok"), "pool", mount_point);

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::config::Config;
    use crate::membership::{DiskMember, PoolMembership};
    use crate::types::{ByIdPath, LuksUuid, MountPoint};
    use std::collections::BTreeMap;

    struct MockFs {
        paths: Vec<String>,
    }

    impl MockFs {
        fn new(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
            }
        }
    }

    impl Filesystem for MockFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.contains(&path.to_string())
        }

        fn is_block_device(&self, _path: &str) -> bool {
            false
        }

        fn read_to_string(&self, _path: &str) -> Result<String, std::io::Error> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    fn ok_raw(cmd: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
            stderr: stderr.to_owned(),
            exit_status: exit_code,
        }
    }

    /// Test-only helper that mirrors the legacy `open_and_mount_pool` flow
    /// (plan + optional resolve + execute) so existing test bodies don't
    /// need to spell out both phases. Production callers (`cmd_unlock`,
    /// `cmd_recover`) compose the phases explicitly per the refactor's
    /// design — this helper exists ONLY for the test module.
    ///
    /// Mirrors the unlock-style gating: if `plan.to_unlock` is empty, the
    /// credential is dropped before reaching `execute_open_plan` (so the
    /// strict `(true, true)` validation does not fire). Tests for the
    /// recover-specific (true, true) path live in recover.rs.
    fn open_and_mount_for_test<R: CommandRunner, F: Filesystem + ?Sized>(
        runner: &R,
        fs: &F,
        config: &Config,
        membership: &PoolMembership,
        credential: Option<OpenCredential>,
        allow_degraded: bool,
        command_hint: &str,
    ) -> Result<bool, MountError> {
        let plan =
            match plan_open_pool(runner, fs, config, membership, allow_degraded, command_hint)? {
                Some(p) => p,
                None => return Ok(false),
            };
        let cred_for_plan = if plan.to_unlock.is_empty() {
            None
        } else {
            credential.as_ref()
        };
        execute_open_plan(runner, fs, config, &plan, cred_for_plan)
    }

    /// Convenience constructor used in tests.
    fn test_passphrase() -> OpenCredential {
        OpenCredential::Passphrase(Zeroizing::new("testpass".to_owned()))
    }

    fn test_config() -> Config {
        Config::new(MountPoint("/mnt/storage".to_owned())).unwrap()
    }

    fn two_disk_membership() -> PoolMembership {
        let mut disks = BTreeMap::new();
        for (name, path) in [
            ("disk1", "/dev/disk/by-id/virtio-disk1"),
            ("disk2", "/dev/disk/by-id/virtio-disk2"),
        ] {
            disks.insert(
                name.to_owned(),
                DiskMember::from_by_id(ByIdPath(path.to_owned())),
            );
        }
        PoolMembership { disks }
    }

    fn three_disk_membership() -> PoolMembership {
        let mut disks = BTreeMap::new();
        for (name, path) in [
            ("disk1", "/dev/disk/by-id/virtio-disk1"),
            ("disk2", "/dev/disk/by-id/virtio-disk2"),
            ("disk3", "/dev/disk/by-id/virtio-disk3"),
        ] {
            disks.insert(
                name.to_owned(),
                DiskMember::from_by_id(ByIdPath(path.to_owned())),
            );
        }
        PoolMembership { disks }
    }

    fn luks_uuid_ok(device: &str, uuid: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupLuksUuid {
                device: device.into(),
            },
            RawCommandOutput {
                cmd: "cryptsetup luksUUID".into(),
                stdout: format!("{uuid}\n"),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    /// Intent: When the pool is already mounted, open_and_mount_pool should
    /// return Ok(false) without issuing any LUKS commands.
    ///
    /// Why: Callers use the return value to decide post-mount actions
    /// (e.g. unlock refreshes metadata, recover continues to rebuild).
    ///
    /// Scenario: Pool was previously unlocked and is still mounted. A
    /// redundant mount attempt should be a no-op.
    #[test]
    fn mount_already_mounted_returns_false() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = MockFs::new(&[]);

        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("mountpoint"),
        );

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            None,
            false,
            "unlock",
        );

        assert!(!result.unwrap());
    }

    /// Intent: Two healthy disks with LUKS closed should be opened, scanned,
    /// and mounted successfully.
    ///
    /// Why: This is the core happy path that both unlock and recover rely on.
    ///
    /// Scenario: 2-disk RAID1, both present, both LUKS-closed. Passphrase
    /// provided via file. All commands succeed.
    #[test]
    fn mount_two_disk_happy_path() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "aaaaaaaa-1111-2222-3333-444444444444",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("mount"),
            );

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            false,
            "unlock",
        );

        assert!(result.unwrap());
    }

    /// Intent: When a disk is absent and --allow-degraded is passed, the pool
    /// should mount with the degraded option.
    ///
    /// Why: Recovery after interrupted remove may leave a disk absent. The
    /// pool must still be mountable.
    ///
    /// Scenario: 3-disk RAID1, disk3 absent. allow_degraded=true. Mount uses
    /// MountWithOptions with "degraded".
    #[test]
    fn mount_degraded_with_flag() {
        let config = test_config();
        let membership = three_disk_membership();
        // disk3 is absent — not in fs paths
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "aaaaaaaa-1111-2222-3333-444444444444",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            .with_output(
                CmdRequest::MountWithOptions {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                    options: vec!["degraded".to_owned()],
                },
                ok_raw("mount -o degraded"),
            );

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            true,
            "unlock",
        );

        assert!(result.unwrap());
    }

    /// Intent: When a disk is absent and --allow-degraded is NOT passed, the
    /// mount must be refused with a clear error including the command hint.
    ///
    /// Why: Principle 1 requires explicit opt-in for degraded mounts.
    ///
    /// Scenario: 3-disk RAID1, disk3 absent, allow_degraded=false. The error
    /// must mention "braid recover --allow-degraded" when command_hint is "recover".
    #[test]
    fn mount_degraded_refused() {
        let config = test_config();
        let membership = three_disk_membership();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "aaaaaaaa-1111-2222-3333-444444444444",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"));
        // No mount mock — should never reach mount

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            false,
            "recover",
        );

        let err = result.expect_err("should refuse degraded mount");
        assert!(
            matches!(&err, MountError::DegradedRefused(_)),
            "expected DegradedRefused, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("braid recover --allow-degraded"),
            "hint should reference 'braid recover --allow-degraded', got: {msg}"
        );
    }

    /// Intent: format_degraded_refused must surface the disk name, the
    /// reason ("LUKS header unreadable"), the substring contracts that
    /// existing tests anchor on, and the singular "1 missing device" form
    /// for a single-disk failure.
    ///
    /// Why: The Test 7 VM scenario hits exactly this shape (one raw
    /// member in a 2-disk pool). If any of these substrings drift, both
    /// the existing degraded-refused unit tests and the VM test would
    /// fail with confusing diffs.
    ///
    /// Scenario: A pool has one missing member ("raw") with an unreadable
    /// LUKS header, command_hint is "unlock".
    #[test]
    fn format_degraded_refused_single_unreadable_includes_disk_name_and_reason() {
        let msg = format_degraded_refused(
            &[("raw".to_owned(), MissingReason::LuksHeaderUnreadable)],
            "unlock",
        );
        assert!(
            msg.contains("refusing to mount degraded"),
            "missing 'refusing to mount degraded': {msg}"
        );
        assert!(
            msg.contains("braid unlock --allow-degraded"),
            "missing 'braid unlock --allow-degraded': {msg}"
        );
        assert!(
            msg.contains("raw: LUKS header unreadable"),
            "missing per-disk line 'raw: LUKS header unreadable': {msg}"
        );
        assert!(
            msg.contains("1 missing device"),
            "expected singular '1 missing device': {msg}"
        );
        // Make sure the singular form is not "1 missing devices"
        assert!(
            !msg.contains("1 missing devices"),
            "singular form should not have trailing 's': {msg}"
        );
        assert!(
            msg.contains("new writes would have ZERO redundancy"),
            "missing redundancy warning preserved from old message: {msg}"
        );
    }

    /// Intent: format_degraded_refused must enumerate disks in probe
    /// order, even when reasons are interleaved (unplugged → unreadable
    /// → unplugged).
    ///
    /// Why: This is the regression test for the parallel-vector design
    /// considered in an earlier draft. Two parallel `Vec<String>` lists
    /// would group all unplugged disks before all unreadable ones,
    /// reordering the final error relative to the preceding eprintln!
    /// status stream. The test asserts byte-offset ordering to make any
    /// future regression to a category-grouped layout fail loudly.
    ///
    /// Scenario: A 3-missing-disk pool: disk2 unplugged, disk3
    /// unreadable, disk5 unplugged. command_hint is "recover".
    #[test]
    fn format_degraded_refused_mixed_reasons_enumerates_each_disk_in_order() {
        let msg = format_degraded_refused(
            &[
                ("disk2".to_owned(), MissingReason::Unplugged),
                ("disk3".to_owned(), MissingReason::LuksHeaderUnreadable),
                ("disk5".to_owned(), MissingReason::Unplugged),
            ],
            "recover",
        );

        assert!(
            msg.contains("disk2: not found (unplugged?)"),
            "missing disk2 line: {msg}"
        );
        assert!(
            msg.contains("disk3: LUKS header unreadable"),
            "missing disk3 line: {msg}"
        );
        assert!(
            msg.contains("disk5: not found (unplugged?)"),
            "missing disk5 line: {msg}"
        );

        // Probe-order assertion: disk2 < disk3 < disk5 in byte offsets.
        let pos_disk2 = msg.find("disk2:").expect("disk2 should appear");
        let pos_disk3 = msg.find("disk3:").expect("disk3 should appear");
        let pos_disk5 = msg.find("disk5:").expect("disk5 should appear");
        assert!(
            pos_disk2 < pos_disk3,
            "disk2 must appear before disk3 (probe order): {msg}"
        );
        assert!(
            pos_disk3 < pos_disk5,
            "disk3 must appear before disk5 (probe order): {msg}"
        );

        assert!(
            msg.contains("3 missing devices"),
            "expected plural '3 missing devices': {msg}"
        );
        assert!(
            msg.contains("braid recover --allow-degraded"),
            "missing 'braid recover --allow-degraded': {msg}"
        );
    }

    /// Intent: format_degraded_refused must use "1 missing device" for a
    /// single missing disk and "N missing devices" for two or more.
    ///
    /// Why: User-facing pluralization is small but cumulative — trivial
    /// to get right with a one-line check, distracting if wrong.
    ///
    /// Scenario: One call with one entry, one call with two entries;
    /// assert the singular and plural forms are correct.
    #[test]
    fn format_degraded_refused_uses_singular_for_one_disk_and_plural_otherwise() {
        let one = format_degraded_refused(
            &[("raw".to_owned(), MissingReason::LuksHeaderUnreadable)],
            "unlock",
        );
        assert!(
            one.contains("1 missing device") && !one.contains("1 missing devices"),
            "expected singular '1 missing device' (no trailing s): {one}"
        );

        let two = format_degraded_refused(
            &[
                ("disk2".to_owned(), MissingReason::Unplugged),
                ("disk3".to_owned(), MissingReason::LuksHeaderUnreadable),
            ],
            "unlock",
        );
        assert!(
            two.contains("2 missing devices"),
            "expected plural '2 missing devices': {two}"
        );
    }

    /// Intent: format_degraded_refused must never reference local LUKS
    /// header backup paths.
    ///
    /// Why: The cross-command negative invariant established in
    /// `plans/wip/cheeky-questing-popcorn.md` says that user-facing
    /// recovery messages must use generic off-system backup language and
    /// must not point at `/var/lib/braid/luks-headers/` or `.luksheader`
    /// files. Locking this in at the formatter level means the invariant
    /// holds for every caller of `DegradedRefused`, present and future.
    ///
    /// Scenario: A non-empty input with both reason kinds. Both negative
    /// substrings must be absent.
    #[test]
    fn format_degraded_refused_does_not_reference_local_header_backups() {
        let msg = format_degraded_refused(
            &[
                ("disk2".to_owned(), MissingReason::Unplugged),
                ("disk3".to_owned(), MissingReason::LuksHeaderUnreadable),
            ],
            "unlock",
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
        assert!(
            !msg.contains(".luksheader"),
            "must not reference local .luksheader files: {msg}"
        );
    }

    /// Intent: format_degraded_refused must surface a distinct
    /// "LUKS header metadata damaged" line for `MissingReason::LuksHeaderDamaged`.
    ///
    /// Why: damaged metadata has a different recovery story
    /// (`cryptsetup repair`) than an unreadable header. Collapsing both
    /// into the same label would hide the actionable distinction the
    /// upstream probe just made.
    ///
    /// Scenario: a single declared disk whose LUKS magic is intact but
    /// luksDump fails to parse the metadata blocks.
    #[test]
    fn format_degraded_refused_damaged_includes_disk_name_and_reason() {
        let msg = format_degraded_refused(
            &[("raw".to_owned(), MissingReason::LuksHeaderDamaged)],
            "unlock",
        );
        assert!(
            msg.contains("raw: LUKS header metadata damaged"),
            "missing damaged label: {msg}"
        );
        assert!(
            msg.contains("1 missing device") && !msg.contains("1 missing devices"),
            "expected singular: {msg}"
        );
    }

    /// Intent: format_degraded_refused must append a doctor-guidance footer
    /// when at least one disk has an Unreadable LUKS header.
    ///
    /// Why: the inline list keeps short labels; the proactive doctor
    /// command holds the full recovery guidance. The footer is the bridge
    /// between the two.
    ///
    /// Scenario: a single LuksHeaderUnreadable disk; the footer is the
    /// last line of the message.
    #[test]
    fn format_degraded_refused_unreadable_includes_doctor_footer() {
        let msg = format_degraded_refused(
            &[("raw".to_owned(), MissingReason::LuksHeaderUnreadable)],
            "unlock",
        );
        assert!(
            msg.contains("run 'braid doctor' for recovery guidance"),
            "missing doctor footer: {msg}"
        );
    }

    /// Intent: format_degraded_refused must append a doctor-guidance footer
    /// when at least one disk has a Damaged LUKS header.
    ///
    /// Why: same bridge as the unreadable case — the per-disk label is
    /// short, and `braid doctor` carries the full repair guidance.
    ///
    /// Scenario: a single LuksHeaderDamaged disk.
    #[test]
    fn format_degraded_refused_damaged_includes_doctor_footer() {
        let msg = format_degraded_refused(
            &[("raw".to_owned(), MissingReason::LuksHeaderDamaged)],
            "unlock",
        );
        assert!(
            msg.contains("run 'braid doctor' for recovery guidance"),
            "missing doctor footer: {msg}"
        );
    }

    /// Intent: format_degraded_refused must NOT append the doctor footer
    /// for an Unplugged-only failure.
    ///
    /// Why: a hot-unplugged cable does not need recovery guidance — the
    /// fix is to plug the cable back in. Adding doctor noise to that case
    /// would dilute the signal where it actually matters.
    ///
    /// Scenario: a single Unplugged disk.
    #[test]
    fn format_degraded_refused_unplugged_only_omits_doctor_footer() {
        let msg = format_degraded_refused(
            &[("raw".to_owned(), MissingReason::Unplugged)],
            "unlock",
        );
        assert!(
            !msg.contains("braid doctor"),
            "unplugged-only must not include doctor footer: {msg}"
        );
    }

    /// Intent: format_degraded_refused must include the doctor footer at
    /// most once, even when multiple LUKS-header-state disks are present
    /// alongside an unplugged disk.
    ///
    /// Why: emitting the footer per-disk would be noisy and could
    /// confuse the user about whether each line is a separate
    /// recommendation. The footer is a single trailing instruction.
    ///
    /// Scenario: one Unplugged + one Damaged disk; both labels appear,
    /// the footer appears exactly once.
    #[test]
    fn format_degraded_refused_mixed_includes_doctor_footer_once() {
        let msg = format_degraded_refused(
            &[
                ("disk2".to_owned(), MissingReason::Unplugged),
                ("disk3".to_owned(), MissingReason::LuksHeaderDamaged),
            ],
            "unlock",
        );
        assert!(
            msg.contains("disk2: not found (unplugged?)"),
            "missing unplugged line: {msg}"
        );
        assert!(
            msg.contains("disk3: LUKS header metadata damaged"),
            "missing damaged line: {msg}"
        );
        assert_eq!(
            msg.matches("run 'braid doctor' for recovery guidance")
                .count(),
            1,
            "doctor footer must appear exactly once: {msg}"
        );
    }

    /// Intent: When a passphrase is verified against disk1 but rejected by
    /// disk2, the error must name both disks.
    ///
    /// Why: The single-passphrase invariant (Principle 4) may be violated by
    /// external LUKS manipulation. The error must help the user identify which
    /// disk is different.
    ///
    /// Scenario: 2-disk RAID1, passphrase verified on disk1, disk2 rejects it.
    #[test]
    fn mount_passphrase_mismatch_names_disk() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "aaaaaaaa-1111-2222-3333-444444444444",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                err_raw(
                    "cryptsetup open",
                    2,
                    "No key available with this passphrase.",
                ),
            );

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            false,
            "unlock",
        );

        let err = result.expect_err("should fail when disk2 rejects passphrase");
        let msg = err.to_string();
        assert!(
            msg.contains("disk2"),
            "error should name the failing disk, got: {msg}"
        );
        assert!(
            msg.contains("disk1"),
            "error should name the verification disk, got: {msg}"
        );
    }

    /// Intent: When all disks are absent and none are already open, the helper
    /// must return a clear error.
    ///
    /// Why: Cannot mount what doesn't exist.
    ///
    /// Scenario: 2-disk pool, both disks unplugged.
    #[test]
    fn mount_no_unlockable_disks() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = MockFs::new(&[]); // no devices present

        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("mountpoint", 1, ""),
        );

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            None,
            false,
            "unlock",
        );

        let err = result.expect_err("should fail with no unlockable disks");
        let msg = err.to_string();
        assert!(
            msg.contains("no unlockable disks"),
            "expected 'no unlockable disks', got: {msg}"
        );
    }

    /// Intent: When all LUKS mappers are already open, the helper should skip
    /// passphrase prompting and proceed directly to scan + mount.
    ///
    /// Why: Idempotency. User may have partially recovered manually before
    /// running braid recover.
    ///
    /// Scenario: 2-disk pool, both mappers already open, pool not yet mounted.
    #[test]
    fn mount_skip_already_open() {
        let config = test_config();
        let membership = two_disk_membership();
        // Devices exist and mappers exist
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1",
            "/dev/mapper/braid-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "aaaaaaaa-1111-2222-3333-444444444444",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            // No passphrase or LUKS open mocks — should not be called
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("mount"),
            );

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            None,
            false,
            "unlock",
        );

        assert!(result.unwrap());
    }

    /// Intent: When a disk's probed LUKS UUID doesn't match pool.json's stored
    /// UUID, unlock must fatally error before attempting to open the device.
    ///
    /// Why: A UUID mismatch means the physical drive has been swapped,
    /// reformatted, or corrupted. Proceeding would mount the wrong data.
    ///
    /// Scenario: 2-disk RAID1. disk1 has a stored luks_uuid from a prior
    /// unlock, but the device now reports a different UUID (drive was swapped).
    /// Both LUKS devices are closed.
    #[test]
    fn mount_luks_uuid_mismatch_closed() {
        let config = test_config();
        let mut membership = two_disk_membership();
        membership.disks.get_mut("disk1").unwrap().luks_uuid =
            Some(LuksUuid("aaaaaaaa-1111-2222-3333-444444444444".into()));

        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "ffffffff-ffff-ffff-ffff-ffffffffffff", // different from stored
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out);

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            None,
            false,
            "unlock",
        );

        let err = result.expect_err("should fail on LUKS UUID mismatch");
        let msg = err.to_string();
        assert!(
            msg.contains("disk1"),
            "error should name the disk, got: {msg}"
        );
        assert!(
            msg.contains("aaaaaaaa"),
            "error should show expected UUID, got: {msg}"
        );
        assert!(
            msg.contains("ffffffff"),
            "error should show found UUID, got: {msg}"
        );
    }

    /// Intent: UUID mismatch must be caught even when the LUKS mapper is
    /// already open (e.g. from a previous partial unlock or manual intervention).
    ///
    /// Why: The check must fire in both PresentLuks branches — mapper_open
    /// status doesn't make a swapped drive safe.
    ///
    /// Scenario: Same as mount_luks_uuid_mismatch_closed, but disk1's mapper
    /// is already open.
    #[test]
    fn mount_luks_uuid_mismatch_already_open() {
        let config = test_config();
        let mut membership = two_disk_membership();
        membership.disks.get_mut("disk1").unwrap().luks_uuid =
            Some(LuksUuid("aaaaaaaa-1111-2222-3333-444444444444".into()));

        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1", // mapper already open
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "ffffffff-ffff-ffff-ffff-ffffffffffff", // different from stored
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out);

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            None,
            false,
            "unlock",
        );

        let err = result.expect_err("should fail on LUKS UUID mismatch even with open mapper");
        let msg = err.to_string();
        assert!(
            msg.contains("disk1"),
            "error should name the disk, got: {msg}"
        );
        assert!(
            msg.contains("aaaaaaaa"),
            "error should show expected UUID, got: {msg}"
        );
        assert!(
            msg.contains("ffffffff"),
            "error should show found UUID, got: {msg}"
        );
    }

    /// Intent: When cryptsetup open fails with a non-auth exit code (e.g. exit 4,
    /// device not found), the error must propagate as-is — not be rewritten as a
    /// single-passphrase invariant violation.
    ///
    /// Why: The mount helper previously replaced all open failures with the
    /// invariant message, masking non-auth causes like device disappearance.
    ///
    /// Scenario: 2-disk RAID1, passphrase verified against disk1, disk2 disappears
    /// (hot-unplug) before cryptsetup open runs.
    #[test]
    fn mount_non_auth_open_failure_propagates_passphrase() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "aaaaaaaa-1111-2222-3333-444444444444",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            // disk2 disappears → exit 4 (ENODEV)
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                err_raw(
                    "cryptsetup open",
                    4,
                    "Device /dev/disk/by-id/virtio-disk2 does not exist.",
                ),
            );

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            false,
            "unlock",
        );

        let err = result.expect_err("should fail when disk2 disappears");
        let msg = err.to_string();
        assert!(
            msg.contains("device not found"),
            "non-auth failure should propagate original hint, got: {msg}"
        );
        assert!(
            !msg.contains("single-passphrase invariant"),
            "non-auth failure should not be rewritten as invariant violation, got: {msg}"
        );
    }

    /// Intent: When cryptsetup open with keyfile fails with a non-auth exit code
    /// (e.g. exit 4), the error must propagate as-is.
    ///
    /// Why: Same masking bug as the passphrase path — all keyfile open failures
    /// were rewritten as invariant violations.
    ///
    /// Scenario: 2-disk RAID1 with keyfile unlock, disk2 disappears before open.
    #[test]
    fn mount_non_auth_open_failure_propagates_keyfile() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "aaaaaaaa-1111-2222-3333-444444444444",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let kf = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            kf.as_file().write_all(b"keydata").unwrap();
        }

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            // verify keyfile against disk1 → success
            .with_output(
                CmdRequest::CryptsetupTestKeyFile {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    key_file_path: kf.path().display().to_string(),
                },
                ok_raw("cryptsetup open --test-passphrase"),
            )
            // open disk1 with keyfile → success
            .with_output(
                CmdRequest::CryptsetupLuksOpenKeyFile {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                    key_file_path: kf.path().display().to_string(),
                },
                ok_raw("cryptsetup open"),
            )
            // disk2 disappears → exit 4 (ENODEV)
            .with_output(
                CmdRequest::CryptsetupLuksOpenKeyFile {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                    key_file_path: kf.path().display().to_string(),
                },
                err_raw(
                    "cryptsetup open",
                    4,
                    "Device /dev/disk/by-id/virtio-disk2 does not exist.",
                ),
            );

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(OpenCredential::KeyFile(kf.path().to_path_buf())),
            false,
            "unlock",
        );

        let err = result.expect_err("should fail when disk2 disappears");
        let msg = err.to_string();
        assert!(
            msg.contains("device not found"),
            "non-auth failure should propagate original hint, got: {msg}"
        );
        assert!(
            !msg.contains("single-passphrase invariant"),
            "non-auth failure should not be rewritten as invariant violation, got: {msg}"
        );
    }

    // --- explain_open_failure pure-helper tests ---
    //
    // These target the classification logic directly. The mount-level
    // integration tests further down prove the call sites are wired up
    // correctly; these tests prove the helper itself picks the right
    // branch for each header state.

    fn arbitrary_fallback() -> MountError {
        MountError::Failed("ARBITRARY FALLBACK TEXT".into())
    }

    /*
     * Intent: Unreadable overrides whatever cryptsetup originally reported.
     * Why it exists: the whole point of probing is that header corruption
     *   should win over exit-code interpretation — an exit 2 from
     *   cryptsetup open should not surface "wrong passphrase" when the
     *   header is actually gone. Also pins the cross-command invariant
     *   that no message references local /var/lib/braid/luks-headers/.
     * Scenario: disk2's LUKS magic has been wiped by a misdirected dd,
     *   and cryptsetup open (unsurprisingly) also fails.
     */
    #[test]
    fn explain_open_failure_unreadable_overrides_fallback() {
        let err = explain_open_failure(
            "disk2",
            "/dev/disk/by-id/wwn-0xDEAD",
            luks::LuksHeaderState::Unreadable,
            "some original summary",
            arbitrary_fallback(),
        );
        let msg = err.to_string();
        assert!(msg.contains("disk2"), "missing disk name: {msg}");
        assert!(
            msg.contains("header unreadable"),
            "missing 'header unreadable': {msg}"
        );
        assert!(
            msg.contains("luksHeaderRestore"),
            "missing 'luksHeaderRestore': {msg}"
        );
        assert!(msg.contains("off-system"), "missing 'off-system': {msg}");
        assert!(
            !msg.contains("ARBITRARY FALLBACK TEXT"),
            "unreadable branch must override fallback: {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
        assert!(
            !msg.contains(".luksheader"),
            "must not reference local .luksheader files: {msg}"
        );
    }

    /*
     * Intent: Damaged overrides fallback and suggests cryptsetup repair
     *   with a safe-backup warning.
     * Why it exists: damaged metadata is recoverable via cryptsetup repair,
     *   but repair mutates the header so the user MUST back up first. This
     *   is the pairing that makes the suggestion safe to follow.
     * Scenario: disk2's LUKS2 metadata was corrupted but the magic bytes
     *   survived, so cryptsetup open fails on keyslot validation.
     */
    #[test]
    fn explain_open_failure_damaged_overrides_fallback() {
        let device = "/dev/disk/by-id/wwn-0xCAFE";
        let err = explain_open_failure(
            "disk2",
            device,
            luks::LuksHeaderState::Damaged,
            "some original summary",
            arbitrary_fallback(),
        );
        let msg = err.to_string();
        assert!(msg.contains("disk2"), "missing disk name: {msg}");
        assert!(
            msg.contains("metadata damaged"),
            "missing 'metadata damaged': {msg}"
        );
        assert!(
            msg.contains(&format!("cryptsetup repair --type luks2 {device}")),
            "missing repair command with device path: {msg}"
        );
        assert!(
            msg.contains("safe backup"),
            "missing safe-backup warning: {msg}"
        );
        assert!(
            !msg.contains("ARBITRARY FALLBACK TEXT"),
            "damaged branch must override fallback: {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
        assert!(
            !msg.contains(".luksheader"),
            "must not reference local .luksheader files: {msg}"
        );
    }

    /*
     * Intent: Ok uses the caller's fallback verbatim.
     * Why it exists: when the header is proven intact, the failure really
     *   is about the passphrase/invariant/device — the existing error
     *   messages are correct and must be preserved untouched.
     * Scenario: user types the wrong passphrase on a healthy pool.
     */
    #[test]
    fn explain_open_failure_ok_uses_fallback_verbatim() {
        let fallback_text = "wrong passphrase (verified against disk1)";
        let err = explain_open_failure(
            "disk1",
            "/dev/disk/by-id/wwn-0xOK",
            luks::LuksHeaderState::Ok,
            "original summary not used in Ok branch",
            MountError::Failed(fallback_text.into()),
        );
        assert_eq!(err.to_string(), fallback_text);
    }

    /*
     * Intent: Ok preserves the single-passphrase-invariant message exactly
     *   for the exit-2-on-subsequent-disk scenario.
     * Why it exists: the invariant-violation message is the subtlest existing
     *   message, and misrouting it would either break a valid warning or
     *   misdiagnose corruption as an invariant violation. This is a specific
     *   regression test pinning the fallback path.
     * Scenario: disk2 rejects a verified passphrase but its header is intact,
     *   indicating real external LUKS manipulation.
     */
    #[test]
    fn explain_open_failure_ok_preserves_invariant_message() {
        let fallback_text =
            "failed to open disk 'disk2': passphrase was verified against 'disk1' but \
             rejected here — wrong passphrase or permission denied (EPERM). If the \
             passphrase is correct, the single-passphrase invariant may be violated \
             by external LUKS manipulation";
        let err = explain_open_failure(
            "disk2",
            "/dev/disk/by-id/wwn-0xOK",
            luks::LuksHeaderState::Ok,
            "unused",
            MountError::Failed(fallback_text.into()),
        );
        let msg = err.to_string();
        assert!(
            msg.contains("single-passphrase invariant"),
            "Ok branch must preserve invariant-violation text: {msg}"
        );
    }

    // --- integration tests for enrichment wiring in open_and_mount_pool ---
    //
    // These prove the four call sites pass the right arguments into
    // `explain_open_failure`. The pure helper tests (above) cover each
    // classification branch; these cover the wiring.

    fn test_passphrase_fail(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupTestPassphrase {
                device: device.into(),
            },
            err_raw(
                "cryptsetup open --test-passphrase",
                2,
                "No key available with this passphrase.",
            ),
        )
    }

    fn test_keyfile_fail(device: &str, key_file_path: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupTestKeyFile {
                device: device.into(),
                key_file_path: key_file_path.into(),
            },
            err_raw(
                "cryptsetup open --test-passphrase",
                2,
                "No key available with this passphrase.",
            ),
        )
    }

    fn is_luks_fail(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupIsLuks {
                device: device.into(),
            },
            err_raw(
                "cryptsetup isLuks",
                1,
                &format!("Device {device} is not a valid LUKS device.\n"),
            ),
        )
    }

    fn is_luks_ok(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupIsLuks {
                device: device.into(),
            },
            ok_raw("cryptsetup isLuks"),
        )
    }

    fn luks_dump_text_ok(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupLuksDumpText {
                device: device.into(),
            },
            RawCommandOutput {
                cmd: "cryptsetup luksDump".into(),
                stdout: "LUKS header information\nVersion: 2\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn luks_dump_text_fail(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupLuksDumpText {
                device: device.into(),
            },
            err_raw("cryptsetup luksDump", 1, "Cannot read LUKS header metadata."),
        )
    }

    /// Common setup: a 2-disk pool where both disks are probed as LUKS ok.
    /// Returns a MockRunner with the base cryptsetup mocks seeded.
    fn base_two_disk_runner() -> MockRunner {
        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "aaaaaaaa-1111-2222-3333-444444444444",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );
        MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
    }

    fn two_disk_fs() -> MockFs {
        MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ])
    }

    /*
     * Intent: When verify_passphrase fails on disk1 AND disk1's LUKS header
     *   is unreadable, the error tells the user to restore from an off-system
     *   backup — not that the passphrase is wrong.
     * Why it exists: a fully-wiped disk1 header causes cryptsetup
     *   --test-passphrase to fail, which looks exactly like a wrong
     *   passphrase at the boolean level. Without probing, the user gets
     *   pointed at the wrong problem. This is the Ultraplan Medium
     *   coverage for the passphrase verify-step wiring.
     * Scenario: disk1's header was clobbered by a misdirected dd; the
     *   user tries to unlock with a perfectly correct passphrase.
     */
    #[test]
    fn unlock_passphrase_verify_fails_unreadable_header_emits_guidance() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = two_disk_fs();

        let (tp_req, tp_out) = test_passphrase_fail("/dev/disk/by-id/virtio-disk1");
        let (is_req, is_out) = is_luks_fail("/dev/disk/by-id/virtio-disk1");

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let runner = base_two_disk_runner()
            .with_output_stdin(tp_req, b"testpass".to_vec(), tp_out)
            .with_output(is_req, is_out);

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            false,
            "unlock",
        );

        let msg = result.expect_err("expected failure").to_string();
        assert!(msg.contains("disk1"), "missing disk name: {msg}");
        assert!(
            msg.contains("header unreadable"),
            "missing 'header unreadable': {msg}"
        );
        assert!(
            msg.contains("luksHeaderRestore"),
            "missing 'luksHeaderRestore': {msg}"
        );
        assert!(
            !msg.contains("wrong passphrase"),
            "unreadable header must not blame passphrase: {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
    }

    /*
     * Intent: When verify_passphrase fails on disk1 AND disk1's LUKS header
     *   is intact, the existing "wrong passphrase" message is preserved.
     * Why it exists: the intact-header fallback must still wire through
     *   the enrichment path unchanged — any regression would break the
     *   most common wrong-passphrase UX.
     * Scenario: user types a wrong passphrase on a healthy pool.
     */
    #[test]
    fn unlock_passphrase_verify_fails_ok_header_preserves_wrong_passphrase() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = two_disk_fs();

        let (tp_req, tp_out) = test_passphrase_fail("/dev/disk/by-id/virtio-disk1");
        let (is_req, is_out) = is_luks_ok("/dev/disk/by-id/virtio-disk1");
        let (dump_req, dump_out) = luks_dump_text_ok("/dev/disk/by-id/virtio-disk1");

        let runner = base_two_disk_runner()
            .with_output_stdin(tp_req, b"wrongpass".to_vec(), tp_out)
            .with_output(is_req, is_out)
            .with_output(dump_req, dump_out);

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(OpenCredential::Passphrase(Zeroizing::new("wrongpass".to_owned()))),
            false,
            "unlock",
        );

        let msg = result.expect_err("expected failure").to_string();
        assert!(
            msg.contains("wrong passphrase (verified against"),
            "intact header must preserve existing wrong-passphrase message: {msg}"
        );
        assert!(
            !msg.contains("header unreadable"),
            "intact header must not route to unreadable guidance: {msg}"
        );
    }

    /*
     * Intent: When verify_key_file fails on disk1 AND disk1's LUKS header
     *   has damaged metadata, the error suggests `cryptsetup repair` with
     *   a safe-backup warning.
     * Why it exists: the keyfile verify path needs the same enrichment
     *   wiring as the passphrase path. This test proves the keyfile
     *   branch hits the damaged helper.
     * Scenario: disk1's LUKS2 keyslot metadata is partially corrupted
     *   but the magic is intact; auto-unlock via keyfile fails.
     */
    #[test]
    fn unlock_keyfile_verify_fails_damaged_header_emits_repair_guidance() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = two_disk_fs();

        let kf = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            kf.as_file().write_all(b"keydata").unwrap();
        }
        let kf_path = kf.path().display().to_string();

        let (tk_req, tk_out) = test_keyfile_fail("/dev/disk/by-id/virtio-disk1", &kf_path);
        let (is_req, is_out) = is_luks_ok("/dev/disk/by-id/virtio-disk1");
        let (dump_req, dump_out) = luks_dump_text_fail("/dev/disk/by-id/virtio-disk1");

        let runner = base_two_disk_runner()
            .with_output(tk_req, tk_out)
            .with_output(is_req, is_out)
            .with_output(dump_req, dump_out);

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(OpenCredential::KeyFile(kf.path().to_path_buf())),
            false,
            "unlock",
        );

        let msg = result.expect_err("expected failure").to_string();
        assert!(msg.contains("disk1"), "missing disk name: {msg}");
        assert!(
            msg.contains("metadata damaged"),
            "missing 'metadata damaged': {msg}"
        );
        assert!(
            msg.contains("cryptsetup repair --type luks2"),
            "missing repair command: {msg}"
        );
        assert!(
            msg.contains("safe backup"),
            "missing safe-backup warning: {msg}"
        );
        assert!(
            !msg.contains("wrong keyfile"),
            "damaged header must not blame keyfile: {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
    }

    /*
     * Intent: When ensure_luks_open fails with exit 2 on disk2 AND the
     *   header probe itself fails to execute, the error must surface
     *   diagnostic uncertainty — NOT confidently blame the single-
     *   passphrase invariant.
     * Why it exists: this is the Ultraplan High finding coverage at the
     *   integration level. With cryptsetup missing from PATH (or any
     *   runner error during the probe), the old fallback would have
     *   silently reverted to the "single-passphrase invariant may be
     *   violated" text, which is exactly the misdiagnosis this plan is
     *   meant to stop. The probe mocks are deliberately not seeded so
     *   MockRunner returns CmdError::MissingMock, which classifies as
     *   ProbeFailed.
     * Scenario: disk2 rejects a verified passphrase (could be real
     *   invariant violation OR corruption), and cryptsetup is missing
     *   from PATH on the machine.
     */
    #[test]
    fn unlock_passphrase_open_exit2_probe_failed_does_not_blame_invariant() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = two_disk_fs();

        let (tp_req, tp_out) = (
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/disk/by-id/virtio-disk1".into(),
            },
            ok_raw("cryptsetup open --test-passphrase"),
        );
        let (open1_req, open1_out) = (
            CmdRequest::CryptsetupLuksOpen {
                device: "/dev/disk/by-id/virtio-disk1".into(),
                mapper: "braid-disk1".into(),
            },
            ok_raw("cryptsetup open"),
        );
        let (open2_req, open2_out) = (
            CmdRequest::CryptsetupLuksOpen {
                device: "/dev/disk/by-id/virtio-disk2".into(),
                mapper: "braid-disk2".into(),
            },
            err_raw(
                "cryptsetup open",
                2,
                "No key available with this passphrase.",
            ),
        );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        // Deliberately NOT seeding CryptsetupIsLuks on disk2 → MissingMock
        // → ProbeFailed. This is the point of the test.
        let runner = base_two_disk_runner()
            .with_output_stdin(tp_req, b"testpass".to_vec(), tp_out)
            .with_output_stdin(open1_req, b"testpass".to_vec(), open1_out)
            .with_output_stdin(open2_req, b"testpass".to_vec(), open2_out);

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            false,
            "unlock",
        );

        let msg = result.expect_err("expected failure").to_string();
        assert!(msg.contains("disk2"), "missing failing disk: {msg}");
        assert!(
            msg.contains("disk1"),
            "missing verification disk in original summary: {msg}"
        );
        assert!(
            msg.contains("diagnosis could not be completed"),
            "missing 'diagnosis could not be completed': {msg}"
        );
        // Load-bearing: the old design would leak the invariant-violation
        // text here; the new design must not.
        assert!(
            !msg.contains("single-passphrase invariant"),
            "ProbeFailed must not blame invariant: {msg}"
        );
    }

    /*
     * Intent: When ensure_luks_open_with_key_file fails with a non-2 exit
     *   on disk2 AND disk2's LUKS header is unreadable, the error emits
     *   the off-system backup guidance.
     * Why it exists: covers the keyfile-open-loop wiring with the
     *   Unreadable classification. Also verifies the fallback-construction
     *   path for non-exit-2 cases (the `_` arm in the match).
     * Scenario: a keyfile-driven auto-unlock where disk2's header has
     *   been wiped; cryptsetup open reports a generic failure (exit 1).
     */
    #[test]
    fn unlock_keyfile_open_exit_nonzero_unreadable_header_emits_guidance() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = two_disk_fs();

        let kf = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            kf.as_file().write_all(b"keydata").unwrap();
        }
        let kf_path = kf.path().display().to_string();

        let (tk_req, tk_out) = (
            CmdRequest::CryptsetupTestKeyFile {
                device: "/dev/disk/by-id/virtio-disk1".into(),
                key_file_path: kf_path.clone(),
            },
            ok_raw("cryptsetup open --test-passphrase"),
        );
        let (open1_req, open1_out) = (
            CmdRequest::CryptsetupLuksOpenKeyFile {
                device: "/dev/disk/by-id/virtio-disk1".into(),
                mapper: "braid-disk1".into(),
                key_file_path: kf_path.clone(),
            },
            ok_raw("cryptsetup open"),
        );
        let (open2_req, open2_out) = (
            CmdRequest::CryptsetupLuksOpenKeyFile {
                device: "/dev/disk/by-id/virtio-disk2".into(),
                mapper: "braid-disk2".into(),
                key_file_path: kf_path,
            },
            err_raw("cryptsetup open", 1, "Cannot read LUKS header"),
        );
        let (is_req, is_out) = is_luks_fail("/dev/disk/by-id/virtio-disk2");

        let runner = base_two_disk_runner()
            .with_output(tk_req, tk_out)
            .with_output(open1_req, open1_out)
            .with_output(open2_req, open2_out)
            .with_output(is_req, is_out);

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(OpenCredential::KeyFile(kf.path().to_path_buf())),
            false,
            "unlock",
        );

        let msg = result.expect_err("expected failure").to_string();
        assert!(msg.contains("disk2"), "missing disk name: {msg}");
        assert!(
            msg.contains("header unreadable"),
            "missing 'header unreadable': {msg}"
        );
        assert!(
            msg.contains("luksHeaderRestore"),
            "missing 'luksHeaderRestore': {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
        assert!(
            !msg.contains(".luksheader"),
            "must not reference local .luksheader files: {msg}"
        );
    }

    /*
     * Intent: ProbeFailed emits a distinct "diagnosis could not be completed"
     *   message and does NOT fall back to passphrase or invariant wording.
     * Why it exists: this is the executable form of an Ultraplan High
     *   finding. If the probe itself errors (e.g. cryptsetup missing from
     *   PATH), a naive design would route to the fallback — which is exactly
     *   the "wrong passphrase" or "single-passphrase invariant" text this
     *   enrichment is meant to stop. Probe-execution failure must surface
     *   as uncertainty, not as confident blame of the passphrase.
     * Scenario: cryptsetup binary is missing from PATH while a disk has
     *   also legitimately failed to open for an unknown reason.
     */
    #[test]
    fn explain_open_failure_probe_failed_emits_diagnosis_incomplete() {
        let fallback_text =
            "failed to open disk 'disk2': passphrase was verified against 'disk1' but \
             rejected here. If the passphrase is correct, the single-passphrase \
             invariant may be violated by external LUKS manipulation";
        let err = explain_open_failure(
            "disk2",
            "/dev/disk/by-id/wwn-0xDEAD",
            luks::LuksHeaderState::ProbeFailed("simulated probe error".into()),
            "cryptsetup open rejected on 'disk2' despite verified passphrase on 'disk1'",
            MountError::Failed(fallback_text.into()),
        );
        let msg = err.to_string();
        assert!(
            msg.contains("diagnosis could not be completed"),
            "missing 'diagnosis could not be completed': {msg}"
        );
        assert!(
            msg.contains("simulated probe error"),
            "missing probe error surface: {msg}"
        );
        assert!(
            msg.contains("cryptsetup open rejected on 'disk2'"),
            "missing original summary: {msg}"
        );
        // The new wording must stay neutral about the underlying cause —
        // the helper is used for non-auth failures too (device not found,
        // busy, generic I/O), so it cannot narrow to "passphrase".
        assert!(
            msg.contains("Cannot determine whether this failure"),
            "missing neutral 'Cannot determine' framing: {msg}"
        );
        // Load-bearing negative assertions: probe-failed must NOT leak the
        // misleading passphrase/invariant wording from the fallback, and
        // must NOT narrow the cause to "passphrase" (the review finding
        // this test's wording pins).
        assert!(
            !msg.contains("wrong passphrase"),
            "ProbeFailed must not blame passphrase: {msg}"
        );
        assert!(
            !msg.contains("single-passphrase invariant"),
            "ProbeFailed must not blame invariant: {msg}"
        );
        assert!(
            !msg.contains("passphrase problem"),
            "ProbeFailed must not narrow cause to 'passphrase problem' — the \
             helper is called from non-auth failure sites too: {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
        assert!(
            !msg.contains(".luksheader"),
            "must not reference local .luksheader files: {msg}"
        );
    }
}
