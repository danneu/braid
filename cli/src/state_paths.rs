use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct StatePaths {
    root: PathBuf,
}

impl StatePaths {
    pub fn production() -> Self {
        Self {
            root: PathBuf::from("/var/lib/braid"),
        }
    }

    pub fn custom(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn pool_json(&self) -> PathBuf {
        self.root.join("pool.json")
    }

    pub fn pending_op_json(&self) -> PathBuf {
        self.root.join("pending-op.json")
    }

    pub fn acked_stats_json(&self) -> PathBuf {
        self.root.join("acked-stats.json")
    }

    /// Monotonic ENOSPC-risk suppression baseline (`EnospcAck`), separate from
    /// `acked-stats.json` because it keys on pool geometry, not per-device error
    /// counters. Written only by `braid ack`, removed only by the monitor on
    /// re-arm / key mismatch / corruption (ADR 014).
    pub fn enospc_ack_json(&self) -> PathBuf {
        self.root.join("enospc-ack.json")
    }

    pub fn smartd_alert(&self) -> PathBuf {
        self.root.join("smartd-alert")
    }

    /// Ephemeral per-run coordination marker the scrub teardown writes and the
    /// scrub runner reads to tell a deliberate cancel apart from a genuine
    /// failure. btrfs exits 1 for *both* a cancelled scrub and a fatal scrub
    /// error, and scrub status renders both as `aborted`, so the only
    /// authoritative "this stop was intentional" signal is braid's own intent:
    /// `scrubCancelScript` (ExecStop) touches this marker, and
    /// `cmd_scrub_resume_or_start` keys off it. NOT a durable alert flag --
    /// it lives in the state dir only so `StatePaths::custom` relocates it
    /// under a temp dir for unit tests, and the path literal is shared with the
    /// ExecStop shell script exactly as `smartd_alert()` is shared with
    /// `smartdAlertScript`.
    pub fn scrub_cancel_requested(&self) -> PathBuf {
        self.root.join("scrub-cancel-requested")
    }

    /// Durable alert flag for a failed maintenance scrub, written by
    /// `braid-scrub-failed.service` (the scrub unit's `onFailure`) and cleared
    /// by `braid ack`. Mirrors `smartd_alert()`: an event source with no device
    /// counter, latched as `AlertCause::ScrubFailed` by the monitor.
    pub fn scrub_failed(&self) -> PathBuf {
        self.root.join("scrub-failed")
    }

    pub fn alert_latch_json(&self) -> PathBuf {
        self.root.join("alert-latch.json")
    }

    pub fn alert_latch_corrupt(&self) -> PathBuf {
        self.root.join("alert-latch.json.corrupt")
    }

    /// Explicit retry marker for ack cleanup after the latch signal is gone.
    pub fn alert_cleanup_pending(&self) -> PathBuf {
        self.root.join("alert-cleanup-pending")
    }

    pub fn luks_headers_dir(&self) -> PathBuf {
        self.root.join("luks-headers")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_resolves_expected_paths() {
        let p = StatePaths::production();
        assert_eq!(p.pool_json(), PathBuf::from("/var/lib/braid/pool.json"));
        assert_eq!(
            p.acked_stats_json(),
            PathBuf::from("/var/lib/braid/acked-stats.json")
        );
        assert_eq!(
            p.enospc_ack_json(),
            PathBuf::from("/var/lib/braid/enospc-ack.json")
        );
        assert_eq!(
            p.smartd_alert(),
            PathBuf::from("/var/lib/braid/smartd-alert")
        );
        assert_eq!(
            p.scrub_cancel_requested(),
            PathBuf::from("/var/lib/braid/scrub-cancel-requested")
        );
        assert_eq!(
            p.scrub_failed(),
            PathBuf::from("/var/lib/braid/scrub-failed")
        );
        assert_eq!(
            p.alert_latch_json(),
            PathBuf::from("/var/lib/braid/alert-latch.json")
        );
        assert_eq!(
            p.alert_latch_corrupt(),
            PathBuf::from("/var/lib/braid/alert-latch.json.corrupt")
        );
        assert_eq!(
            p.alert_cleanup_pending(),
            PathBuf::from("/var/lib/braid/alert-cleanup-pending")
        );
        assert_eq!(
            p.luks_headers_dir(),
            PathBuf::from("/var/lib/braid/luks-headers")
        );
    }

    #[test]
    fn custom_resolves_under_given_root() {
        let p = StatePaths::custom(PathBuf::from("/tmp/test-braid"));
        assert_eq!(p.pool_json(), PathBuf::from("/tmp/test-braid/pool.json"));
        assert_eq!(
            p.scrub_cancel_requested(),
            PathBuf::from("/tmp/test-braid/scrub-cancel-requested")
        );
        assert_eq!(
            p.scrub_failed(),
            PathBuf::from("/tmp/test-braid/scrub-failed")
        );
        assert_eq!(
            p.enospc_ack_json(),
            PathBuf::from("/tmp/test-braid/enospc-ack.json")
        );
        assert_eq!(
            p.alert_latch_corrupt(),
            PathBuf::from("/tmp/test-braid/alert-latch.json.corrupt")
        );
        assert_eq!(
            p.alert_cleanup_pending(),
            PathBuf::from("/tmp/test-braid/alert-cleanup-pending")
        );
        assert_eq!(
            p.luks_headers_dir(),
            PathBuf::from("/tmp/test-braid/luks-headers")
        );
    }
}
