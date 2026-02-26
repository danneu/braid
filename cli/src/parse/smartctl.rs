use crate::cmd::RawCommandOutput;
use crate::parse::types::SmartHealth;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiskProtocol {
    Nvme,
    Sata,
}

#[derive(Deserialize)]
struct RawSmartctlOutput {
    #[serde(default)]
    smart_status: Option<RawSmartStatus>,
    #[serde(default)]
    device: Option<RawDevice>,
    #[serde(default)]
    nvme_smart_health_information_log: Option<RawNvmeHealth>,
    #[serde(default)]
    ata_smart_attributes: Option<RawAtaSmartAttributes>,
}

#[derive(Deserialize)]
struct RawSmartStatus {
    passed: bool,
}

#[derive(Deserialize)]
struct RawDevice {
    #[serde(default)]
    protocol: Option<String>,
}

#[derive(Deserialize)]
struct RawNvmeHealth {
    #[serde(default)]
    critical_warning: u64,
    #[serde(default)]
    media_errors: u64,
    #[serde(default)]
    available_spare: u64,
    #[serde(default)]
    available_spare_threshold: u64,
    #[serde(default)]
    percentage_used: u64,
}

#[derive(Deserialize)]
struct RawAtaSmartAttributes {
    #[serde(default)]
    table: Vec<RawAtaAttribute>,
}

#[derive(Deserialize)]
struct RawAtaAttribute {
    #[serde(default)]
    name: String,
    #[serde(default)]
    raw: RawAtaAttributeValue,
}

#[derive(Deserialize, Default)]
struct RawAtaAttributeValue {
    #[serde(default)]
    value: u64,
}

pub fn parse_smartctl_health(raw: &RawCommandOutput) -> SmartHealth {
    if raw.exit_status != 0 && raw.exit_status & 0x07 != 0 {
        // Bits 0-2 indicate command-line/device errors (not SMART failures)
        // Bits 3+ are SMART status bits — we still parse JSON for those
        if raw.stdout.is_empty() {
            return SmartHealth::Unknown;
        }
    }

    let parsed: RawSmartctlOutput = match serde_json::from_str(&raw.stdout) {
        Ok(v) => v,
        Err(_) => return SmartHealth::Unknown,
    };

    let passed = match &parsed.smart_status {
        Some(s) => s.passed,
        None => return SmartHealth::Unknown,
    };

    if !passed {
        return SmartHealth::Failing;
    }

    let protocol = parsed
        .device
        .as_ref()
        .and_then(|d| d.protocol.as_deref())
        .map(|p| {
            if p.eq_ignore_ascii_case("nvme") {
                DiskProtocol::Nvme
            } else {
                DiskProtocol::Sata
            }
        })
        .unwrap_or(DiskProtocol::Sata);

    match protocol {
        DiskProtocol::Nvme => classify_nvme(&parsed),
        DiskProtocol::Sata => classify_sata(&parsed),
    }
}

fn classify_nvme(parsed: &RawSmartctlOutput) -> SmartHealth {
    let Some(nvme) = &parsed.nvme_smart_health_information_log else {
        return SmartHealth::Healthy;
    };
    if nvme.critical_warning != 0
        || nvme.media_errors != 0
        || (nvme.available_spare_threshold > 0
            && nvme.available_spare <= nvme.available_spare_threshold)
        || nvme.percentage_used >= 90
    {
        SmartHealth::Degraded
    } else {
        SmartHealth::Healthy
    }
}

fn classify_sata(parsed: &RawSmartctlOutput) -> SmartHealth {
    // TODO: validate with real SATA fixture
    let Some(attrs) = &parsed.ata_smart_attributes else {
        return SmartHealth::Healthy;
    };
    let bad = attrs.table.iter().any(|a| {
        matches!(
            a.name.as_str(),
            "Reallocated_Sector_Ct" | "Current_Pending_Sector" | "Offline_Uncorrectable"
        ) && a.raw.value > 0
    });
    if bad {
        SmartHealth::Degraded
    } else {
        SmartHealth::Healthy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::RawCommandOutput;

    fn raw(stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "smartctl".into(),
            stdout: stdout.into(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nixos-25.11");

    #[test]
    fn nvme_fixture_healthy() {
        let path = format!("{FIXTURE_DIR}/smartctl-nvme-healthy.json");
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("SKIP: fixture not captured yet");
                return;
            }
            Err(e) => panic!("reading fixture: {e}"),
        };
        assert_eq!(parse_smartctl_health(&raw(&content)), SmartHealth::Healthy);
    }

    #[test]
    fn nvme_degraded_critical_warning() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "NVMe"},
            "nvme_smart_health_information_log": {
                "critical_warning": 1,
                "media_errors": 0,
                "available_spare": 100,
                "available_spare_threshold": 10,
                "percentage_used": 0
            }
        }"#;
        assert_eq!(parse_smartctl_health(&raw(json)), SmartHealth::Degraded);
    }

    #[test]
    fn nvme_degraded_media_errors() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "NVMe"},
            "nvme_smart_health_information_log": {
                "critical_warning": 0,
                "media_errors": 5,
                "available_spare": 100,
                "available_spare_threshold": 10,
                "percentage_used": 0
            }
        }"#;
        assert_eq!(parse_smartctl_health(&raw(json)), SmartHealth::Degraded);
    }

    #[test]
    fn nvme_degraded_spare_at_threshold() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "NVMe"},
            "nvme_smart_health_information_log": {
                "critical_warning": 0,
                "media_errors": 0,
                "available_spare": 10,
                "available_spare_threshold": 10,
                "percentage_used": 0
            }
        }"#;
        assert_eq!(parse_smartctl_health(&raw(json)), SmartHealth::Degraded);
    }

    #[test]
    fn nvme_degraded_percentage_used_90() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "NVMe"},
            "nvme_smart_health_information_log": {
                "critical_warning": 0,
                "media_errors": 0,
                "available_spare": 100,
                "available_spare_threshold": 10,
                "percentage_used": 90
            }
        }"#;
        assert_eq!(parse_smartctl_health(&raw(json)), SmartHealth::Degraded);
    }

    #[test]
    fn nvme_failing_not_passed() {
        let json = r#"{
            "smart_status": {"passed": false},
            "device": {"protocol": "NVMe"},
            "nvme_smart_health_information_log": {
                "critical_warning": 0,
                "media_errors": 0,
                "available_spare": 100,
                "available_spare_threshold": 10,
                "percentage_used": 0
            }
        }"#;
        assert_eq!(parse_smartctl_health(&raw(json)), SmartHealth::Failing);
    }

    #[test]
    fn sata_degraded_reallocated_sectors() {
        // TODO: validate with real SATA fixture
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "ATA"},
            "ata_smart_attributes": {
                "table": [
                    {"name": "Reallocated_Sector_Ct", "raw": {"value": 8}},
                    {"name": "Current_Pending_Sector", "raw": {"value": 0}},
                    {"name": "Offline_Uncorrectable", "raw": {"value": 0}}
                ]
            }
        }"#;
        assert_eq!(parse_smartctl_health(&raw(json)), SmartHealth::Degraded);
    }

    #[test]
    fn sata_healthy_all_zeros() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "ATA"},
            "ata_smart_attributes": {
                "table": [
                    {"name": "Reallocated_Sector_Ct", "raw": {"value": 0}},
                    {"name": "Current_Pending_Sector", "raw": {"value": 0}},
                    {"name": "Offline_Uncorrectable", "raw": {"value": 0}}
                ]
            }
        }"#;
        assert_eq!(parse_smartctl_health(&raw(json)), SmartHealth::Healthy);
    }

    #[test]
    fn unknown_bad_json() {
        assert_eq!(
            parse_smartctl_health(&raw("not json")),
            SmartHealth::Unknown
        );
    }

    #[test]
    fn unknown_missing_smart_status() {
        let json = r#"{"device": {"protocol": "NVMe"}}"#;
        assert_eq!(parse_smartctl_health(&raw(json)), SmartHealth::Unknown);
    }

    #[test]
    fn unknown_nonzero_exit_no_stdout() {
        let r = RawCommandOutput {
            cmd: "smartctl".into(),
            stdout: String::new(),
            stderr: "device not found".into(),
            exit_status: 2,
        };
        assert_eq!(parse_smartctl_health(&r), SmartHealth::Unknown);
    }
}
