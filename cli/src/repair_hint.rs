use crate::types::DiskName;

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
pub(crate) fn missing_replace_command_with_devid(old: Option<&DiskName>, devid: u64) -> String {
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
            missing_replace_command_with_devid(None, 3),
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
}
