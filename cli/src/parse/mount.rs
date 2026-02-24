/// Classify mount stderr into typed outcomes.
/// All tolerant text matching lives here — domain code branches on enums only.
pub enum MountOutcome {
    MissingMembersDeferred,
    HardError(String),
}

pub fn classify_mount_error(stderr: &str) -> MountOutcome {
    let s = stderr.to_lowercase();
    if s.contains("missing") || s.contains("devid")
        || (s.contains("fsconfig") && s.contains("dmesg"))
    {
        MountOutcome::MissingMembersDeferred
    } else {
        MountOutcome::HardError(stderr.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_devices_is_deferred() {
        let outcome = classify_mount_error("ERROR: cannot mount: missing devices");
        assert!(matches!(outcome, MountOutcome::MissingMembersDeferred));
    }

    #[test]
    fn devid_not_found_is_deferred() {
        let outcome = classify_mount_error("ERROR: cannot mount /mnt/storage: devid 2 not found");
        assert!(matches!(outcome, MountOutcome::MissingMembersDeferred));
    }

    #[test]
    fn fsconfig_dmesg_is_deferred() {
        let stderr = "mount: /mnt/storage: fsconfig() failed: No such file or directory.\n\
                       \x20      dmesg(1) may have more information after failed mount system call.";
        let outcome = classify_mount_error(stderr);
        assert!(matches!(outcome, MountOutcome::MissingMembersDeferred));
    }

    #[test]
    fn no_such_file_is_hard_error() {
        let outcome = classify_mount_error("mount: /mnt/storage: No such file or directory.");
        assert!(matches!(outcome, MountOutcome::HardError(_)));
    }
}
