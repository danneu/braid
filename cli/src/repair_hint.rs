use crate::types::{Devid, DiskName};

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
    format!(
        "devid {devid} is hot-unplugged but btrfs has not yet promoted it to \
         MISSING (LUKS mapper open, backing device gone). `{actor}` only \
         operates on btrfs-authoritative MISSING devids. Confirm the disk is \
         truly gone, then relock and re-unlock the pool degraded (`braid lock` \
         then `braid unlock --allow-degraded`) so btrfs promotes devid {devid}, \
         and retry."
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
}
