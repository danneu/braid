use crate::types::{Devid, DiskName, MountPoint};

const MISSING_NAME_PLACEHOLDER: &str = "<missing-name>";
const NEW_NAME_PLACEHOLDER: &str = "<new-name>=/dev/disk/by-id/<...>";

/// Central missing-device replace shape so operator hints do not drift from
/// replace's `--old`-first identity model.
pub(crate) fn missing_replace_command(old: Option<&DiskName>) -> String {
    let old = old
        .map(|name| name.as_str())
        .unwrap_or(MISSING_NAME_PLACEHOLDER);
    format!("braid replace --old {old} --new {NEW_NAME_PLACEHOLDER}")
}

/// Placeholder cross-check form for docs and warnings that know a missing
/// device exists but should not imply `--missing-id` is required.
pub(crate) fn missing_replace_command_with_devid_placeholder(old: Option<&DiskName>) -> String {
    format!("{} --missing-id <devid>", missing_replace_command(old))
}

/// Concrete cross-check form for hints that have already named the actual
/// btrfs missing devid and want to keep `--missing-id` after required args.
pub(crate) fn missing_replace_command_with_devid(old: Option<&DiskName>, devid: Devid) -> String {
    format!("{} --missing-id {devid}", missing_replace_command(old))
}

/// Shared wording that marks `--missing-id` as an optional validation aid,
/// not the primary missing-device replace command.
pub(crate) fn optional_missing_id_cross_check_phrase() -> String {
    let placeholder_command = missing_replace_command_with_devid_placeholder(None);
    let base_command = missing_replace_command(None);
    let prefix = format!("{base_command} ");
    let placeholder_arg = placeholder_command
        .strip_prefix(&prefix)
        .unwrap_or("--missing-id <devid>");
    format!("Optionally add `{placeholder_arg}` as a cross-check.")
}

/// Name-only member of the status trailer family; centralizing plurality here
/// retires doctor drift while `devid` stays reserved for status-rendered btrfs
/// targets in repair diagnostics.
pub(crate) fn see_missing_names_in_status(missing_count: u64) -> String {
    if missing_count == 1 {
        "Use `braid status` to see the missing disk's name.".into()
    } else {
        "Use `braid status` to see the missing disks' names.".into()
    }
}

/// Shared status lookup hint for repair paths that need names plus the literal
/// `devid` targets rendered by status, with plurality kept out of callers.
pub(crate) fn see_missing_names_and_devids_in_status(missing_count: u64) -> String {
    if missing_count == 1 {
        "Use `braid status` to see the missing disk's name and devid.".into()
    } else {
        "Use `braid status` to see the missing disks' names and devids.".into()
    }
}

/// Shared bad-`--missing-id` trailer that points operators at the literal
/// `devid` tokens rendered by status without another caller-owned plural.
pub(crate) fn see_devids_in_status() -> String {
    "Use `braid status` to see which devids are missing.".into()
}

/// Operator remediation for a member btrfs has not yet promoted to MISSING.
///
/// `actor` names the command that only acts on btrfs-authoritative MISSING
/// devids so the promote-then-retry guidance points back at the right command.
pub(crate) fn hot_unplug_not_yet_missing(devid: Devid, actor: &str) -> String {
    let promote = lock_and_reopen(ReopenWith::Unlock, true);
    format!(
        "devid {devid} is hot-unplugged but btrfs has not yet promoted it to \
         MISSING (LUKS mapper open, backing device gone). `{actor}` only \
         operates on btrfs-authoritative MISSING devids. Confirm the disk is \
         truly gone, then relock and re-unlock the pool degraded ({promote}) \
         so btrfs promotes devid {devid}, and retry."
    )
}

/// Operator remediation for members whose LUKS mapper is open while the
/// disk under it is gone. It owns the whole reopen route, so remove and
/// doctor cannot drift from
/// `docs/internals/real-world/sata-hot-unplug.md#recovery-path`.
///
/// `hot_unplugged` lists the devids the hint speaks about. `missing` is the
/// pool's btrfs-MISSING set. The disk-is-back command adds
/// `--allow-degraded` only when some MISSING devid is not listed, because
/// the gone branch already covers a listed one. `next_step` is the
/// caller's closing sentence.
pub(crate) fn hot_unplug_reopen(
    hot_unplugged: &[Devid],
    missing: &[Devid],
    reopen: ReopenWith,
    next_step: &str,
) -> String {
    let other_missing = missing.iter().any(|devid| !hot_unplugged.contains(devid));
    let back = lock_and_reopen(reopen, other_missing);
    let gone = lock_and_reopen(reopen, true);
    let devids = devid_list(hot_unplugged);
    let (lead, disk_back, gone_promotes) = if hot_unplugged.len() == 1 {
        (
            format!(
                "{devids} is hot-unplugged (LUKS mapper open, backing device gone). \
                 Re-plugging the disk alone never reopens its mapper."
            ),
            format!("If the disk is back, run {back} to close and reopen its mapper."),
            format!("If it is truly gone, run {gone} so btrfs promotes {devids} to MISSING."),
        )
    } else {
        (
            format!(
                "{devids} are hot-unplugged (LUKS mappers open, backing devices gone). \
                 Re-plugging the disks alone never reopens their mappers."
            ),
            format!("If the disks are back, run {back} to close and reopen their mappers."),
            format!(
                "If any is truly gone, run {gone} so btrfs promotes each gone devid to MISSING."
            ),
        )
    };
    format!(
        "{lead} {disk_back} {gone_promotes} A reboot can stand in for `braid lock`. {next_step}"
    )
}

/// Operator remediation for a pool member whose disk is attached, or about
/// to be, while no open mapper in the mounted pool holds it. btrfs does not
/// take a returning disk back into a mounted pool, so only a fresh mount
/// brings it back, and doctor owns the advice for data written while it was
/// away. Returns a lowercase clause for the caller's sentence.
/// `others_missing` adds `--allow-degraded` when another member would still
/// be missing after the reopen.
pub(crate) fn not_live_reopen(reopen: ReopenWith, others_missing: bool) -> String {
    format!(
        "run {} to reopen the pool with it, and run `braid doctor` to check for data \
         written while it was away",
        lock_and_reopen(reopen, others_missing)
    )
}

/// The one rendering of the close-and-reopen command pair, so every
/// hot-unplug and not-live hint names the same commands in the same order.
fn lock_and_reopen(reopen: ReopenWith, allow_degraded: bool) -> String {
    let reopen = match reopen {
        ReopenWith::Unlock => "braid unlock",
        ReopenWith::Recover => "braid recover",
    };
    let flag = if allow_degraded {
        " --allow-degraded"
    } else {
        ""
    };
    format!("`braid lock` then `{reopen}{flag}`")
}

/// `devid 3`, `devids 2 and 3`, or `devids 2, 3 and 5`, so a hint that
/// lists hot-unplugged members reads the same everywhere.
fn devid_list(devids: &[Devid]) -> String {
    let names: Vec<String> = devids.iter().map(Devid::to_string).collect();
    match names.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("devids {} and {last}", rest.join(", ")),
        _ => format!("devid {}", names.concat()),
    }
}

/// Operator remediation when members other than the command's target are
/// hot-unplugged but not yet MISSING. Separate from
/// `hot_unplug_not_yet_missing` because the refusal is about where btrfs
/// would write, not about the target, while the promote-then-retry route
/// is the same.
pub(crate) fn hot_unplugged_bystander_not_yet_missing(devids: &[Devid], actor: &str) -> String {
    let devids = devid_list(devids);
    let promote = lock_and_reopen(ReopenWith::Unlock, true);
    format!(
        "`{actor}` cannot run while btrfs still treats a hot-unplugged member as \
         present. Hot-unplugged but not yet promoted to MISSING: {devids} (LUKS \
         mapper open, backing device gone). btrfs places new chunk copies on such a \
         member, so they could land on a disk that is gone. Confirm each \
         hot-unplugged disk is truly gone, then relock and re-unlock the pool \
         degraded ({promote}) so btrfs promotes {devids}, and retry."
    )
}

/// The two routes off a 2-disk pool that never leave the departing disk as
/// the only holder of any data, for remove's 2->1 health refusal: replace it
/// with a new disk, or collapse the pool onto the other disk while the
/// departing one is detached. Detached and relocked, its devid comes back
/// MISSING at the degraded unlock, so btrfs never writes to it and
/// `braid remove-missing` accepts it.
pub(crate) fn unhealthy_two_to_one_routes(name: &DiskName, devid: Devid) -> String {
    let replace = missing_replace_command(Some(name));
    format!(
        "Replace it with a new disk: `{replace}`. With no spare, move the pool \
         onto the other disk without writing to this one: run `braid lock`, \
         disconnect {name}, run `braid unlock --allow-degraded`, then \
         `braid remove-missing --missing-id {devid}`."
    )
}

/// The braid command that reopens the pool in a hint's fresh-mount or
/// reopen step. A command caller passes the one it knows can run: it knows
/// whether a pending-op journal can exist when it refuses. doctor, which
/// does not read the journal, passes `Unlock`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReopenWith {
    /// The pool mutators, refusing before they write a journal.
    /// `cli/src/main.rs#dispatch_policy` marks them `Gated`, so the recovery
    /// gate refuses them while a journal exists. None exists here, and
    /// `braid unlock` can run. The operator then retries the refused command.
    Unlock,
    /// A caller refusing while a pending-op journal exists: recover itself,
    /// or a mutator after it wrote its journal. `braid unlock` refuses then,
    /// and `braid recover` opens its own fresh mount. When recover refused,
    /// that is also the retry. When a mutator refused, recover resolves the
    /// journal, and the operator then re-runs the refused command.
    Recover,
}

/// Operator remediation for a pool mounted read-only.
///
/// States both causes every time, because mountinfo cannot tell them apart.
/// An operator or tool remounted the pool read-only, which
/// `mount -o remount,rw` undoes. Or btrfs forced it read-only after an
/// error, and then the kernel refuses that remount (linux 6.18.33,
/// `fs/btrfs/super.c` (`btrfs_remount_rw`): "remounting read-write after
/// error is not allowed"). btrfs keeps that error only in memory, so a
/// fresh mount clears it (linux 6.18.33, `fs/btrfs/messages.c`
/// (`__btrfs_handle_fs_error`): "Today we only save the error info to
/// memory", then `btrfs_info(fs_info, "forced readonly")`). That kernel log
/// line tells the operator which case applies; the remount failing covers a
/// log that has wrapped. The docs owner of the same split is
/// `docs/guides/troubleshooting.md#pool-is-mounted-read-only`.
pub(crate) fn read_only_pool(mount_point: &MountPoint, reopen: ReopenWith) -> String {
    let fresh_mount = match reopen {
        ReopenWith::Unlock => {
            "`braid lock` and `braid unlock` (add `--allow-degraded` if a disk \
             is missing), and retry"
        }
        ReopenWith::Recover => {
            "`braid lock` and `braid recover` (add `--allow-degraded` if a disk \
             is missing)"
        }
    };
    format!(
        "Check the kernel log for a btrfs \"forced readonly\" line \
         (`journalctl -k --grep 'forced readonly'`).\n\
         No such line: an operator or a tool remounted the pool read-only. \
         Run `mount -o remount,rw {mount_point}` and retry.\n\
         A \"forced readonly\" line, or that remount fails: btrfs hit an \
         error, and only a fresh mount clears it. Fix the cause, then run \
         {fresh_mount}.\n\
         See docs/guides/troubleshooting.md#pool-is-mounted-read-only."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Intent: the generic missing-device repair hint carries the required
    // replace arguments in the canonical order.
    // Why it exists: every operator-facing repair hint should show the same
    // command shape instead of shorthand `replace --missing-id` guidance.
    // Scenario: a degraded-pool guard knows a member is missing but does not
    // know the member's presentation name.
    #[test]
    fn missing_replace_command_uses_missing_name_placeholder() {
        assert_eq!(
            missing_replace_command(None),
            "braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>"
        );
    }

    // Intent: named missing-device rows substitute the member name into the
    // same canonical replace command.
    // Why it exists: verbose status can name the missing member and should not
    // fall back to the generic placeholder when it has better context.
    // Scenario: `braid status --verbose` renders an action for missing disk2.
    #[test]
    fn missing_replace_command_uses_actual_old_name() {
        let old = DiskName::parse("disk2").unwrap();
        assert_eq!(
            missing_replace_command(Some(&old)),
            "braid replace --old disk2 --new <new-name>=/dev/disk/by-id/<...>"
        );
    }

    // Intent: placeholder cross-check rendering keeps `--missing-id` after
    // the required replace arguments.
    // Why it exists: help text may show how to add the optional btrfs devid
    // assertion without making it look like the command's primary selector.
    // Scenario: generic guidance mentions an optional `--missing-id <devid>`.
    #[test]
    fn missing_replace_command_with_devid_placeholder_appends_after_required_args() {
        assert_eq!(
            missing_replace_command_with_devid_placeholder(None),
            "braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...> --missing-id <devid>"
        );
    }

    // Intent: actual-devid rendering keeps the concrete cross-check after the
    // required replace arguments.
    // Why it exists: diagnostics that already have the btrfs devid should not
    // regress to bare `braid replace --missing-id <devid>` guidance.
    // Scenario: doctor reports missing devid 3 and offers an optional check.
    #[test]
    fn missing_replace_command_with_devid_appends_actual_id_after_required_args() {
        assert_eq!(
            missing_replace_command_with_devid(None, Devid::new(3)),
            "braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...> --missing-id 3"
        );
    }

    // Intent: the optional cross-check phrase names `--missing-id` without
    // turning it into the replacement command.
    // Why it exists: multi-missing diagnostics list devids once and then tell
    // the operator how to use one as an optional assertion.
    // Scenario: `braid doctor` reports two missing devids.
    #[test]
    fn optional_missing_id_phrase_marks_cross_check_optional() {
        assert_eq!(
            optional_missing_id_cross_check_phrase(),
            "Optionally add `--missing-id <devid>` as a cross-check."
        );
    }

    // Intent: status lookup guidance for missing names renders the singular
    // and plural possessives from one canonical helper.
    // Why it exists: doctor and command refusals should not drift on missing
    // disk name wording or lose the trailing period.
    // Scenario: a degraded pool has either one missing member or multiple
    // missing members whose names the operator can inspect in status.
    #[test]
    fn see_missing_names_in_status_pluralizes_disk_names() {
        assert_eq!(
            see_missing_names_in_status(1),
            "Use `braid status` to see the missing disk's name."
        );
        assert_eq!(
            see_missing_names_in_status(2),
            "Use `braid status` to see the missing disks' names."
        );
    }

    // Intent: status lookup guidance for missing-name plus devid repair paths
    // uses the literal `devid` token status renders.
    // Why it exists: generic ID wording is ambiguous with by-id hardware paths on
    // the same status row and has drifted across command refusals.
    // Scenario: an operator needs both the missing member name and the btrfs
    // devid accepted by `--missing-id`.
    #[test]
    fn see_missing_names_and_devids_in_status_pluralizes_devid_hint() {
        assert_eq!(
            see_missing_names_and_devids_in_status(1),
            "Use `braid status` to see the missing disk's name and devid."
        );
        assert_eq!(
            see_missing_names_and_devids_in_status(2),
            "Use `braid status` to see the missing disks' names and devids."
        );
    }

    // Intent: bad `--missing-id` guidance points at the status-rendered devid
    // set without reintroducing the ambiguous generic ID noun.
    // Why it exists: replace and remove-missing should share the same target
    // lookup sentence when rejecting an invalid missing devid.
    // Scenario: an operator supplies a devid that is neither live nor missing
    // in the current pool.
    #[test]
    fn see_devids_in_status_names_missing_devid_targets() {
        assert_eq!(
            see_devids_in_status(),
            "Use `braid status` to see which devids are missing."
        );
    }

    // Intent: hot-unplug targets render the promote-then-act guidance with the
    // caller command named in the refusal.
    // Why it exists: replace, remove-missing, and doctor should not drift into
    // conflicting null-underlying remediation.
    // Scenario: an operator targets a hot-unplugged devid before btrfs has
    // promoted it to MISSING.
    #[test]
    fn hot_unplug_not_yet_missing_renders_actor_specific_guidance() {
        assert_eq!(
            hot_unplug_not_yet_missing(Devid::new(2), "braid replace"),
            "devid 2 is hot-unplugged but btrfs has not yet promoted it to MISSING \
             (LUKS mapper open, backing device gone). `braid replace` only operates on \
             btrfs-authoritative MISSING devids. Confirm the disk is truly gone, then \
             relock and re-unlock the pool degraded (`braid lock` then `braid unlock \
             --allow-degraded`) so btrfs promotes devid 2, and retry."
        );
        assert_eq!(
            hot_unplug_not_yet_missing(Devid::new(2), "braid remove-missing"),
            "devid 2 is hot-unplugged but btrfs has not yet promoted it to MISSING \
             (LUKS mapper open, backing device gone). `braid remove-missing` only \
             operates on btrfs-authoritative MISSING devids. Confirm the disk is truly \
             gone, then relock and re-unlock the pool degraded (`braid lock` then \
             `braid unlock --allow-degraded`) so btrfs promotes devid 2, and retry."
        );
    }

    // Intent: the bystander hot-unplug hint names every unpromoted devid,
    // with the right number, in both places it lists them.
    // Why it exists: lock and degraded unlock promote every gone disk at
    // once, so naming only one would hide the others from the operator.
    // The full one-devid wording is pinned by remove-missing's
    // `hot_unplugged_bystander_refused_*` tests.
    // Scenario: one, two, or three other members are hot-unplugged while
    // the operator runs `braid remove-missing` on a MISSING devid.
    #[test]
    fn hot_unplugged_bystander_names_every_devid() {
        let hint = |devids: &[u64]| {
            let devids: Vec<Devid> = devids.iter().copied().map(Devid::new).collect();
            hot_unplugged_bystander_not_yet_missing(&devids, "braid remove-missing")
        };
        let one = hint(&[3]);
        assert!(
            one.contains("MISSING: devid 3 (") && one.contains("promotes devid 3,"),
            "{one}"
        );
        let two = hint(&[2, 3]);
        assert!(
            two.contains("MISSING: devids 2 and 3 (") && two.contains("promotes devids 2 and 3,"),
            "{two}"
        );
        let three = hint(&[2, 3, 5]);
        assert!(
            three.contains("MISSING: devids 2, 3 and 5 (")
                && three.contains("promotes devids 2, 3 and 5,"),
            "{three}"
        );
    }

    // Intent: the hot-unplug reopen hint renders the full route for one
    // hot-unplugged devid with nothing else missing.
    // Why it exists: finding 261022-1230-017. remove offered re-plugging
    // alone as a fix, and re-plugging never rebinds a dm-crypt mapper
    // (docs/internals/real-world/sata-hot-unplug.md#recovery-path).
    // Scenario: disk2's cable came loose before `braid remove disk2`.
    #[test]
    fn hot_unplug_reopen_renders_one_devid_route() {
        assert_eq!(
            hot_unplug_reopen(
                &[Devid::new(2)],
                &[],
                ReopenWith::Unlock,
                "Then re-run `braid remove`."
            ),
            "devid 2 is hot-unplugged (LUKS mapper open, backing device gone). \
             Re-plugging the disk alone never reopens its mapper. If the disk is back, \
             run `braid lock` then `braid unlock` to close and reopen its mapper. If it \
             is truly gone, run `braid lock` then `braid unlock --allow-degraded` so \
             btrfs promotes devid 2 to MISSING. A reboot can stand in for `braid lock`. \
             Then re-run `braid remove`."
        );
    }

    // Intent: the reopen hint names only the reopen command the caller can
    // run, adds `--allow-degraded` to the disk-back step only while another
    // member is btrfs-MISSING, and always ends with the caller's next step.
    // Why it exists: one helper owns this route for remove's plan-time and
    // post-journal refusals and for doctor. A wrong switch
    // sends the operator to `braid unlock` with a journal (it refuses), or to
    // a plain unlock that refuses a degraded pool.
    // Scenario: one or two hot-unplugged members, with or without a dead
    // disk elsewhere, before and after remove writes its journal.
    #[test]
    fn hot_unplug_reopen_matrix() {
        for (reopen, command, wrong_command) in [
            (ReopenWith::Unlock, "braid unlock", "braid recover"),
            (ReopenWith::Recover, "braid recover", "braid unlock"),
        ] {
            for other_missing in [false, true] {
                for (listed, lead) in [
                    (&[2][..], "devid 2 is hot-unplugged"),
                    (&[2, 3][..], "devids 2 and 3 are hot-unplugged"),
                ] {
                    let listed: Vec<Devid> = listed.iter().copied().map(Devid::new).collect();
                    let missing = if other_missing {
                        vec![Devid::new(5)]
                    } else {
                        vec![]
                    };
                    let hint = hot_unplug_reopen(&listed, &missing, reopen, "NEXT.");
                    let back_flag = if other_missing {
                        " --allow-degraded"
                    } else {
                        ""
                    };
                    let case = format!("{reopen:?} other_missing={other_missing} {listed:?}");
                    assert!(hint.starts_with(lead), "{case}: {hint}");
                    assert!(hint.contains("alone never reopens"), "{case}: {hint}");
                    assert!(
                        hint.contains(&format!(
                            "back, run `braid lock` then `{command}{back_flag}` to close"
                        )),
                        "{case}: {hint}"
                    );
                    assert!(
                        hint.contains(&format!(
                            "gone, run `braid lock` then `{command} --allow-degraded` so btrfs \
                             promotes"
                        )),
                        "{case}: {hint}"
                    );
                    assert!(!hint.contains(wrong_command), "{case}: {hint}");
                    assert!(hint.contains("reboot"), "{case}: {hint}");
                    assert!(hint.ends_with(" NEXT."), "{case}: {hint}");
                }
            }
        }

        // A listed devid that btrfs has already promoted is covered by the
        // gone step, so it does not make the disk-back step degraded.
        let hint = hot_unplug_reopen(&[Devid::new(2)], &[Devid::new(2)], ReopenWith::Unlock, "");
        assert!(
            hint.contains("back, run `braid lock` then `braid unlock` to close"),
            "{hint}"
        );
    }

    // Intent: the not-live reopen clause names the shared lock-and-reopen
    // pair and adds `--allow-degraded` only while another member is missing.
    // Why it exists: finding 261022-1230-005. `braid add` told the operator
    // to detach a member that came back after a degraded mount, as if it were
    // a dd clone. A wrong flag here sends the operator to a plain unlock that
    // refuses a degraded pool, or degrades a pool that is whole again.
    // Scenario: disk2 comes back after a degraded mount, alone or while
    // another member is still gone.
    #[test]
    fn not_live_reopen_renders_flag_per_other_missing() {
        assert_eq!(
            not_live_reopen(ReopenWith::Unlock, false),
            "run `braid lock` then `braid unlock` to reopen the pool with it, and run \
             `braid doctor` to check for data written while it was away"
        );
        assert_eq!(
            not_live_reopen(ReopenWith::Unlock, true),
            "run `braid lock` then `braid unlock --allow-degraded` to reopen the pool with \
             it, and run `braid doctor` to check for data written while it was away"
        );
    }

    // Intent: the read-only hint gives both remedies every time and names
    // the fresh-mount command each caller can actually run.
    // Why it exists: the old hint offered only `mount -o remount,rw`, which
    // the kernel refuses once btrfs forced the pool read-only after an error.
    // The mutators must send the operator to `braid unlock` and a retry,
    // while recover, which always has a journal, must send them to
    // `braid recover`, because `braid unlock` refuses with a journal.
    // Scenario: a degraded pool that btrfs forced read-only refuses
    // `braid replace`; an interrupted remove leaves recover facing the same
    // read-only pool.
    #[test]
    fn read_only_pool_renders_both_remedies_per_reopen_command() {
        let mount_point = MountPoint::new("/mnt/storage".into());
        assert_eq!(
            read_only_pool(&mount_point, ReopenWith::Unlock),
            "Check the kernel log for a btrfs \"forced readonly\" line \
             (`journalctl -k --grep 'forced readonly'`).\n\
             No such line: an operator or a tool remounted the pool read-only. \
             Run `mount -o remount,rw /mnt/storage` and retry.\n\
             A \"forced readonly\" line, or that remount fails: btrfs hit an error, \
             and only a fresh mount clears it. Fix the cause, then run `braid lock` \
             and `braid unlock` (add `--allow-degraded` if a disk is missing), and \
             retry.\n\
             See docs/guides/troubleshooting.md#pool-is-mounted-read-only."
        );
        assert_eq!(
            read_only_pool(&mount_point, ReopenWith::Recover),
            "Check the kernel log for a btrfs \"forced readonly\" line \
             (`journalctl -k --grep 'forced readonly'`).\n\
             No such line: an operator or a tool remounted the pool read-only. \
             Run `mount -o remount,rw /mnt/storage` and retry.\n\
             A \"forced readonly\" line, or that remount fails: btrfs hit an error, \
             and only a fresh mount clears it. Fix the cause, then run `braid lock` \
             and `braid recover` (add `--allow-degraded` if a disk is missing).\n\
             See docs/guides/troubleshooting.md#pool-is-mounted-read-only."
        );
    }
}
