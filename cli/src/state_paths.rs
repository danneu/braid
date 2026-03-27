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

    pub fn acked_stats_json(&self) -> PathBuf {
        self.root.join("acked-stats.json")
    }

    pub fn smartd_alert(&self) -> PathBuf {
        self.root.join("smartd-alert")
    }

    pub fn alert_latch_json(&self) -> PathBuf {
        self.root.join("alert-latch.json")
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
            p.smartd_alert(),
            PathBuf::from("/var/lib/braid/smartd-alert")
        );
        assert_eq!(
            p.alert_latch_json(),
            PathBuf::from("/var/lib/braid/alert-latch.json")
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
            p.luks_headers_dir(),
            PathBuf::from("/tmp/test-braid/luks-headers")
        );
    }
}
