use crate::cmd::RawCommandOutput;
use crate::parse::types::{SmartHealth, SmartProbe};
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
    #[serde(default)]
    temperature: Option<RawTemperature>,
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

#[derive(Deserialize)]
struct RawTemperature {
    #[serde(default)]
    current: Option<i32>,
}

pub fn parse_smartctl(raw: &RawCommandOutput) -> SmartProbe {
    if raw.exit_status != 0 && raw.exit_status & 0x07 != 0 {
        // Bits 0-2 indicate command-line/device errors (not SMART failures)
        // Bits 3+ are SMART status bits -- we still parse JSON for those
        if raw.stdout.is_empty() {
            return SmartProbe {
                health: SmartHealth::Unknown,
                celsius: None,
            };
        }
    }

    let parsed: RawSmartctlOutput = match serde_json::from_str(&raw.stdout) {
        Ok(v) => v,
        Err(_) => {
            return SmartProbe {
                health: SmartHealth::Unknown,
                celsius: None,
            }
        }
    };

    let celsius = parsed
        .temperature
        .as_ref()
        .and_then(|t| t.current)
        .and_then(|v| i16::try_from(v).ok());

    let health = classify_health(&parsed);
    SmartProbe { health, celsius }
}

fn classify_health(parsed: &RawSmartctlOutput) -> SmartHealth {
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
        DiskProtocol::Nvme => classify_nvme(parsed),
        DiskProtocol::Sata => classify_sata(parsed),
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
    let bad = attrs.table.iter().any(|a| match a.name.as_str() {
        // raw16(raw16) format: sector count is lower 16 bits
        // (https://github.com/smartmontools/smartmontools/blob/RELEASE_7_5/smartmontools/drivedb.h#L83)
        "Reallocated_Sector_Ct" => a.raw.value & 0xFFFF > 0,
        // raw48 format: full value is the count
        // (https://github.com/smartmontools/smartmontools/blob/RELEASE_7_5/smartmontools/drivedb.h#L118-L119)
        "Current_Pending_Sector" | "Offline_Uncorrectable" => a.raw.value > 0,
        _ => false,
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
        assert_eq!(parse_smartctl(&raw(&content)).health, SmartHealth::Healthy);
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
        assert_eq!(parse_smartctl(&raw(json)).health, SmartHealth::Degraded);
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
        assert_eq!(parse_smartctl(&raw(json)).health, SmartHealth::Degraded);
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
        assert_eq!(parse_smartctl(&raw(json)).health, SmartHealth::Degraded);
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
        assert_eq!(parse_smartctl(&raw(json)).health, SmartHealth::Degraded);
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
        assert_eq!(parse_smartctl(&raw(json)).health, SmartHealth::Failing);
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
        assert_eq!(parse_smartctl(&raw(json)).health, SmartHealth::Degraded);
    }

    #[test]
    fn sata_healthy_reallocated_zero_with_nonzero_upper_bytes() {
        // Intent: Reallocated_Sector_Ct with 0 sectors must not false-positive
        //   as Degraded when upper bytes of the raw value are non-zero.
        // Why: smartctl raw.value is the full 48-bit raw. Attribute 5 uses
        //   raw16(raw16) format where only the lower 16 bits are the sector
        //   count; upper words carry supplementary event data.
        // Scenario: a Toshiba N300 (or similar HDD using the drivedb default
        //   for attribute 5) reports 0 reallocated sectors but 5 reallocation
        //   events in the middle word -> raw.value = 5 << 16 = 327680.
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "ATA"},
            "ata_smart_attributes": {
                "table": [
                    {"name": "Reallocated_Sector_Ct", "raw": {"value": 327680}},
                    {"name": "Current_Pending_Sector", "raw": {"value": 0}},
                    {"name": "Offline_Uncorrectable", "raw": {"value": 0}}
                ]
            }
        }"#;
        assert_eq!(parse_smartctl(&raw(json)).health, SmartHealth::Healthy);
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
        assert_eq!(parse_smartctl(&raw(json)).health, SmartHealth::Healthy);
    }

    #[test]
    fn unknown_bad_json() {
        let probe = parse_smartctl(&raw("not json"));
        assert_eq!(probe.health, SmartHealth::Unknown);
        assert_eq!(probe.celsius, None);
    }

    #[test]
    fn unknown_missing_smart_status() {
        let json = r#"{"device": {"protocol": "NVMe"}}"#;
        assert_eq!(parse_smartctl(&raw(json)).health, SmartHealth::Unknown);
    }

    #[test]
    fn unknown_nonzero_exit_no_stdout() {
        let r = RawCommandOutput {
            cmd: "smartctl".into(),
            stdout: String::new(),
            stderr: "device not found".into(),
            exit_status: 2,
        };
        let probe = parse_smartctl(&r);
        assert_eq!(probe.health, SmartHealth::Unknown);
        assert_eq!(probe.celsius, None);
    }

    // Intent: verify that `temperature.current` in smartctl JSON is surfaced
    //         as `SmartProbe.celsius`.
    // Why: the TUI's session hi/lo watermark column depends on this value;
    //      a silent None here would leave every drive showing "-" in the
    //      Temp column regardless of what smartctl reported.
    // Scenario: a SATA drive reports `temperature.current = 38` via the
    //          SCT / SMART attribute 194 pathway.
    #[test]
    fn celsius_extracted_when_present() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "ATA"},
            "temperature": {"current": 38}
        }"#;
        assert_eq!(parse_smartctl(&raw(json)).celsius, Some(38));
    }

    // Intent: drives that don't emit a "temperature" block produce
    //         `celsius: None` rather than defaulting to 0.
    // Why: USB-bridged drives frequently omit the block entirely; 0 would
    //      be read as "very cold" and pollute watermarks.
    // Scenario: SATA drive JSON with smart_status and attributes but no
    //           top-level "temperature" key.
    #[test]
    fn celsius_none_when_temperature_missing() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "ATA"}
        }"#;
        assert_eq!(parse_smartctl(&raw(json)).celsius, None);
    }

    // Intent: a "temperature" block that's present but empty (no `current`)
    //         still produces `celsius: None`.
    // Why: smartctl emits other temperature fields (op_limit_max,
    //      critical_limit_max on NVMe) without always emitting `current`;
    //      we must not treat the block's mere presence as a reading.
    // Scenario: NVMe drive where smartctl emits op/critical limits but no
    //           current temp.
    #[test]
    fn celsius_none_when_current_missing() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "NVMe"},
            "temperature": {"op_limit_max": 80, "critical_limit_max": 84}
        }"#;
        assert_eq!(parse_smartctl(&raw(json)).celsius, None);
    }

    // Intent: negative temperatures are preserved (signed i16).
    // Why: cold-storage drives or uncalibrated sensors can legitimately
    //      report sub-zero Celsius; narrowing through u16 would corrupt
    //      them.
    // Scenario: a drive reports -5 C.
    #[test]
    fn celsius_negative_preserved() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "ATA"},
            "temperature": {"current": -5}
        }"#;
        assert_eq!(parse_smartctl(&raw(json)).celsius, Some(-5));
    }

    // Intent: health and celsius are independent -- health=Unknown does
    //         not suppress a valid temperature reading.
    // Why: the TUI's Temp column should light up even if SMART status
    //      couldn't be classified; they're separate signals.
    // Scenario: a drive that returned temperature but no smart_status block.
    #[test]
    fn celsius_survives_unknown_health() {
        let json = r#"{
            "device": {"protocol": "ATA"},
            "temperature": {"current": 42}
        }"#;
        let probe = parse_smartctl(&raw(json));
        assert_eq!(probe.health, SmartHealth::Unknown);
        assert_eq!(probe.celsius, Some(42));
    }
}
