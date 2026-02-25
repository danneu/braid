use crate::config::config_hash;
use crate::state_io::atomic_write;
use crate::types::PoolState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const CHECKPOINT_FILE: &str = "/var/lib/braid/op-state.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpCheckpoint {
    pub op: String,
    pub disk: String,
    pub step: u8,
    pub started_at: String,
    pub config_hash: String,
    pub args_hash: String,
    pub pool_fingerprint: PoolFingerprint,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_disk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_disk: Option<String>,
}

/// Pool topology snapshot for checkpoint staleness detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolFingerprint {
    pub devices: Vec<PoolFingerprintDevice>,
    pub missing_count: u32,
    pub total_devices: u32,
    pub mounted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolFingerprintDevice {
    pub devid: u64,
    pub luks_uuid: Option<String>,
    pub mapper: String,
}

impl PoolFingerprint {
    pub fn from_pool_state(pool: &PoolState) -> Self {
        let mut devices: Vec<PoolFingerprintDevice> = pool
            .devices
            .iter()
            .map(|d| PoolFingerprintDevice {
                devid: d.devid,
                luks_uuid: Some(d.luks_uuid.0.clone()),
                mapper: d.mapper.0.clone(),
            })
            .collect();
        devices.sort_by_key(|d| d.devid);
        PoolFingerprint {
            devices,
            missing_count: pool.missing_count as u32,
            total_devices: pool.total_devices as u32,
            mounted: pool.mounted,
        }
    }
}

/// Compute a hash for the command arguments (for staleness detection).
pub fn hash_args(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!("{:x}", hasher.finalize())
}

#[derive(Debug)]
pub enum CheckpointValidity {
    /// Checkpoint matches current state — safe to resume.
    Valid(OpCheckpoint),
    /// Checkpoint is stale — auto-invalidated, reason printed.
    Stale(String),
    /// No checkpoint exists.
    None,
}

/// Load and validate a checkpoint against current state.
pub fn load_checkpoint(
    config_raw: &str,
    pool: &PoolState,
    expected_op: &str,
    expected_args_hash: &str,
) -> CheckpointValidity {
    let path = Path::new(CHECKPOINT_FILE);
    if !path.exists() {
        return CheckpointValidity::None;
    }

    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return CheckpointValidity::None,
    };

    let checkpoint: OpCheckpoint = match serde_json::from_str(&contents) {
        Ok(c) => c,
        Err(_) => {
            let _ = std::fs::remove_file(path);
            return CheckpointValidity::Stale("corrupted checkpoint file".into());
        }
    };

    let current_hash = config_hash(config_raw);
    if checkpoint.config_hash != current_hash {
        let _ = std::fs::remove_file(path);
        return CheckpointValidity::Stale("config changed since checkpoint was created".into());
    }

    if checkpoint.op != expected_op {
        let _ = std::fs::remove_file(path);
        return CheckpointValidity::Stale(format!(
            "checkpoint is for '{}' but running '{}'",
            checkpoint.op, expected_op
        ));
    }

    if checkpoint.args_hash != expected_args_hash {
        let _ = std::fs::remove_file(path);
        return CheckpointValidity::Stale("command arguments changed".into());
    }

    let current_fp = PoolFingerprint::from_pool_state(pool);
    if checkpoint.pool_fingerprint != current_fp {
        let _ = std::fs::remove_file(path);
        return CheckpointValidity::Stale("pool topology changed since checkpoint".into());
    }

    CheckpointValidity::Valid(checkpoint)
}

/// Save a checkpoint atomically.
pub fn save_checkpoint(checkpoint: &OpCheckpoint) -> Result<(), std::io::Error> {
    let json = serde_json::to_string_pretty(checkpoint)
        .map_err(std::io::Error::other)?;
    atomic_write(Path::new(CHECKPOINT_FILE), json.as_bytes())?;
    Ok(())
}

/// Clear the checkpoint file on successful completion.
pub fn clear_checkpoint() {
    let _ = std::fs::remove_file(CHECKPOINT_FILE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{LuksUuid, MapperName, PoolDevice};

    fn test_pool() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-toshiba".into()),
                luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".into()),
                devid: 1,
            }],
            missing_count: 0,
            total_devices: 1,
        }
    }

    #[test]
    fn fingerprint_sorts_by_devid() {
        let pool = PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName("braid-b".into()),
                    luks_uuid: LuksUuid("bbb".into()),
                    devid: 5,
                },
                PoolDevice {
                    mapper: MapperName("braid-a".into()),
                    luks_uuid: LuksUuid("aaa".into()),
                    devid: 1,
                },
            ],
            missing_count: 0,
            total_devices: 2,
        };
        let fp = PoolFingerprint::from_pool_state(&pool);
        assert_eq!(fp.devices[0].devid, 1);
        assert_eq!(fp.devices[1].devid, 5);
    }

    #[test]
    fn hash_args_deterministic() {
        let h1 = hash_args(&["add", "toshiba"]);
        let h2 = hash_args(&["add", "toshiba"]);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_args_different_for_different_args() {
        let h1 = hash_args(&["add", "toshiba"]);
        let h2 = hash_args(&["add", "ironwolf"]);
        assert_ne!(h1, h2);
    }

    #[test]
    fn load_checkpoint_none_when_no_file() {
        let pool = test_pool();
        let result = load_checkpoint("config", &pool, "add", "hash");
        assert!(matches!(result, CheckpointValidity::None));
    }
}
