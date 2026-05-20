use crate::cmd::RawCommandOutput;
use crate::parse::types::{SelftestEntry, SelftestKind, SelftestSummary, SmartHealth, SmartProbe};
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
    #[serde(default)]
    power_on_time: Option<RawPowerOnTime>,
    #[serde(default)]
    ata_smart_self_test_log: Option<RawAtaSelfTestLog>,
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

#[derive(Deserialize)]
struct RawPowerOnTime {
    #[serde(default)]
    hours: Option<u64>,
}

#[derive(Deserialize)]
struct RawAtaSelfTestLog {
    #[serde(default)]
    standard: Option<RawAtaSelfTestStandard>,
}

#[derive(Deserialize, Default)]
struct RawAtaSelfTestStandard {
    #[serde(default, rename = "count")]
    _count: u32,
    #[serde(default)]
    table: Vec<RawAtaSelfTestEntry>,
    #[serde(default)]
    error_count_total: u32,
    #[serde(default)]
    error_count_outdated: u32,
}

#[derive(Deserialize)]
struct RawAtaSelfTestEntry {
    #[serde(default, rename = "type")]
    kind: RawSelfTestType,
    #[serde(default)]
    status: RawSelfTestStatus,
    #[serde(default)]
    lifetime_hours: u32,
}

#[derive(Deserialize, Default)]
struct RawSelfTestType {
    #[serde(default)]
    value: Option<u8>,
    #[serde(default)]
    string: String,
}

#[derive(Deserialize, Default)]
struct RawSelfTestStatus {
    #[serde(default)]
    value: Option<u8>,
    #[serde(default)]
    string: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelftestStatusClass {
    Passed,
    Aborted,
    Failed,
    InProgress,
    Unknown,
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
            };
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

/// Parse SMART self-test logs with stricter command-error handling than
/// `parse_smartctl`.
///
/// Health parsing can safely fall back to `Unknown` on partial JSON. Self-test
/// classification drives Fail/Warn decisions, so bits 0-2 short-circuit before
/// JSON interpretation and bit 7 still parses as the active-failure path.
pub fn parse_smartctl_selftest_log(raw: &RawCommandOutput) -> SelftestSummary {
    if raw.exit_status & 0x07 != 0 {
        return SelftestSummary {
            command_error: true,
            ..SelftestSummary::default()
        };
    }

    let parsed: RawSmartctlOutput = match serde_json::from_str(&raw.stdout) {
        Ok(v) => v,
        Err(_) => {
            return SelftestSummary {
                parse_failure: true,
                ..SelftestSummary::default()
            };
        }
    };

    let protocol = parsed.device.as_ref().and_then(|d| d.protocol.as_deref());
    match protocol {
        Some(p) if p.eq_ignore_ascii_case("ata") || p.eq_ignore_ascii_case("sata") => {}
        Some(p) => {
            return SelftestSummary {
                unsupported_protocol: Some(p.to_owned()),
                ..SelftestSummary::default()
            };
        }
        None => {
            return SelftestSummary {
                unsupported_protocol: Some("unknown".to_owned()),
                ..SelftestSummary::default()
            };
        }
    }

    let power_on_hours = parsed.power_on_time.and_then(|p| p.hours);
    let standard = parsed
        .ata_smart_self_test_log
        .and_then(|log| log.standard)
        .unwrap_or_default();
    let active_errors = standard
        .error_count_total
        .saturating_sub(standard.error_count_outdated);

    let mut last_passing = None;
    let mut last_failure = None;

    for entry in standard.table {
        match classify_selftest_status(entry.status.value) {
            SelftestStatusClass::Passed if last_passing.is_none() => {
                last_passing = Some(selftest_entry(entry));
            }
            SelftestStatusClass::Failed if last_failure.is_none() => {
                last_failure = Some(selftest_entry(entry));
            }
            SelftestStatusClass::Passed
            | SelftestStatusClass::Failed
            | SelftestStatusClass::Aborted
            | SelftestStatusClass::InProgress
            | SelftestStatusClass::Unknown => {}
        }

        if last_passing.is_some() && last_failure.is_some() {
            break;
        }
    }

    SelftestSummary {
        command_error: false,
        parse_failure: false,
        unsupported_protocol: None,
        power_on_hours,
        active_errors,
        last_passing,
        last_failure,
    }
}

/// Age in powered-on hours, wrap-aware for ATA self-test entries.
/// ATA `lifetime_hours` in the self-test log wraps at 2^16 while
/// `power_on_hours` from attribute 9 does not, so both values are masked into
/// the same 16-bit window before subtraction.
pub(crate) fn selftest_age_hours(power_on_hours: u64, entry_lifetime_hours: u32) -> u64 {
    let poh_mod = power_on_hours % 65536;
    let entry_mod = (entry_lifetime_hours as u64) % 65536;
    (poh_mod + 65536 - entry_mod) % 65536
}

fn selftest_entry(raw: RawAtaSelfTestEntry) -> SelftestEntry {
    SelftestEntry {
        kind: selftest_kind(raw.kind.value, &raw.kind.string),
        lifetime_hours: raw.lifetime_hours,
        status_value: raw.status.value.unwrap_or(0),
        status_string: raw.status.string,
    }
}

fn selftest_kind(value: Option<u8>, label: &str) -> SelftestKind {
    match value {
        Some(0) => SelftestKind::Offline,
        Some(1) => SelftestKind::Short,
        Some(2) => SelftestKind::Extended,
        Some(3) => SelftestKind::Conveyance,
        Some(4) => SelftestKind::Selective,
        _ if label.eq_ignore_ascii_case("short") => SelftestKind::Short,
        _ if label.eq_ignore_ascii_case("extended") => SelftestKind::Extended,
        _ if label.eq_ignore_ascii_case("conveyance") => SelftestKind::Conveyance,
        _ if label.eq_ignore_ascii_case("selective") => SelftestKind::Selective,
        _ if label.eq_ignore_ascii_case("offline") => SelftestKind::Offline,
        _ => SelftestKind::Other(label.to_owned()),
    }
}

fn classify_selftest_status(value: Option<u8>) -> SelftestStatusClass {
    match value.map(|v| v >> 4) {
        Some(0x0) => SelftestStatusClass::Passed,
        Some(0x1 | 0x2) => SelftestStatusClass::Aborted,
        Some(0x3..=0x8) => SelftestStatusClass::Failed,
        Some(0xf) => SelftestStatusClass::InProgress,
        _ => SelftestStatusClass::Unknown,
    }
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

    fn raw_with_status(stdout: &str, exit_status: i32) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "smartctl".into(),
            stdout: stdout.into(),
            stderr: String::new(),
            exit_status,
        }
    }

    const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nixos-25.11");

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{FIXTURE_DIR}/{name}")).expect("selftest fixture reads")
    }

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

    // Intent: parse the happy-path ATA self-test JSON shape.
    // Why it exists: doctor depends on the most recent passing entry and
    //   active-error counters rather than raw smartctl JSON.
    // Scenario: a drive completed a short self-test roughly two powered-on
    //   days ago.
    #[test]
    fn selftest_recent_pass_parsed() {
        let summary =
            parse_smartctl_selftest_log(&raw(&fixture("smartctl-selftest-ata-recent-pass.json")));
        assert_eq!(summary.active_errors, 0);
        assert_eq!(summary.power_on_hours, Some(5000));
        let entry = summary.last_passing.expect("passing entry");
        assert_eq!(entry.lifetime_hours, 4950);
        assert_eq!(entry.kind, SelftestKind::Short);
        assert_eq!(summary.last_failure, None);
    }

    // Intent: smartctl's bit-7 exit status still parses as a self-test
    //   failure report.
    // Why it exists: bit 7 means the self-test log contains active errors;
    //   treating all non-zero exits as command errors would hide failures.
    // Scenario: smartctl exits 128 while returning a valid failed extended
    //   self-test row.
    #[test]
    fn selftest_active_failure_with_exit_128() {
        let content = fixture("smartctl-selftest-ata-active-failure.json");
        let summary = parse_smartctl_selftest_log(&raw_with_status(&content, 128));
        assert!(!summary.command_error);
        assert_eq!(summary.active_errors, 1);
        let failure = summary.last_failure.expect("failure entry");
        assert_eq!(failure.kind, SelftestKind::Extended);
        assert_eq!(failure.status_value, 80);
    }

    // Intent: command-error bits short-circuit even with parseable stdout.
    // Why it exists: bits 0-2 mean smartctl could not safely query the device;
    //   reading partial JSON could misclassify the drive.
    // Scenario: smartctl exits with bit 2 set but still prints a JSON body.
    #[test]
    fn selftest_command_error_bit_2_does_not_parse() {
        let content = fixture("smartctl-selftest-ata-command-error.json");
        let summary = parse_smartctl_selftest_log(&raw_with_status(&content, 4));
        assert!(summary.command_error);
        assert_eq!(summary.active_errors, 0);
        assert_eq!(summary.last_passing, None);
        assert_eq!(summary.last_failure, None);
        assert_eq!(summary.power_on_hours, None);
    }

    // Intent: classify ATA status 0x3 as a failure without relying on
    //   `status.passed`.
    // Why it exists: smartctl omits `passed` for fatal/unknown errors, but
    //   those entries still count toward active self-test errors.
    // Scenario: the table contains a fatal-or-unknown extended entry.
    #[test]
    fn selftest_fatal_or_unknown_classified_as_failed() {
        let summary = parse_smartctl_selftest_log(&raw(&fixture(
            "smartctl-selftest-ata-fatal-or-unknown.json",
        )));
        assert_eq!(summary.active_errors, 1);
        assert_eq!(summary.last_failure.expect("failure").status_value, 48);
    }

    // Intent: aborted entries are neither passing nor failing entries.
    // Why it exists: aborted/interrupted rows should drive the "never" path,
    //   not a false Ok or Fail.
    // Scenario: a drive has only aborted or interrupted self-test rows.
    #[test]
    fn selftest_aborted_not_failed() {
        let summary =
            parse_smartctl_selftest_log(&raw(&fixture("smartctl-selftest-ata-aborted-only.json")));
        assert_eq!(summary.active_errors, 0);
        assert_eq!(summary.last_passing, None);
        assert_eq!(summary.last_failure, None);
    }

    // Intent: smartctl outdated counters clear old failures.
    // Why it exists: a failed self-test superseded by a newer passing extended
    //   test must not remain active.
    // Scenario: `error_count_total == error_count_outdated == 1`.
    #[test]
    fn selftest_outdated_failure_not_active() {
        let summary = parse_smartctl_selftest_log(&raw(&fixture(
            "smartctl-selftest-ata-failure-outdated.json",
        )));
        assert_eq!(summary.active_errors, 0);
        assert!(summary.last_failure.is_some());
        assert!(summary.last_passing.is_some());
    }

    // Intent: a passing short test does not supersede a prior failed extended
    //   test.
    // Why it exists: smartctl's active-error counters encode this distinction;
    //   braid must not infer supersession from recency alone.
    // Scenario: the newest row passes short, but the active-error counter
    //   still reports the older extended failure.
    #[test]
    fn selftest_short_pass_does_not_supersede_failure() {
        let summary = parse_smartctl_selftest_log(&raw(&fixture(
            "smartctl-selftest-ata-short-pass-does-not-supersede.json",
        )));
        assert_eq!(summary.active_errors, 1);
        assert!(summary.last_passing.is_some());
        assert!(summary.last_failure.is_some());
    }

    // Intent: smartctl's real empty-log shape parses without synthetic fields.
    // Why it exists: empty logs omit `table` and error counters, so serde
    //   defaults are part of the parser contract.
    // Scenario: a drive has never logged a completed SMART self-test.
    #[test]
    fn selftest_empty_log_real_shape() {
        let summary =
            parse_smartctl_selftest_log(&raw(&fixture("smartctl-selftest-ata-empty.json")));
        assert!(!summary.parse_failure);
        assert_eq!(summary.active_errors, 0);
        assert_eq!(summary.last_passing, None);
        assert_eq!(summary.last_failure, None);
    }

    // Intent: a non-empty log can still have no completed passing entry.
    // Why it exists: doctor must render the "never" warning for rows that are
    //   only aborted or interrupted.
    // Scenario: the self-test table has entries, but none classify as Passed.
    #[test]
    fn selftest_aborted_only_no_passing() {
        let summary =
            parse_smartctl_selftest_log(&raw(&fixture("smartctl-selftest-ata-aborted-only.json")));
        assert_eq!(summary.last_passing, None);
        assert_eq!(summary.last_failure, None);
        assert_eq!(summary.active_errors, 0);
    }

    // Intent: NVMe is explicitly skipped by the ATA self-test parser.
    // Why it exists: NVMe self-test logs have a different JSON schema and
    //   must not look like an ATA drive with no tests.
    // Scenario: smartctl reports `device.protocol = "NVMe"`.
    #[test]
    fn selftest_nvme_unsupported_protocol() {
        let summary =
            parse_smartctl_selftest_log(&raw(&fixture("smartctl-selftest-nvme-unsupported.json")));
        assert_eq!(summary.unsupported_protocol.as_deref(), Some("NVMe"));
        assert_eq!(summary.power_on_hours, None);
        assert_eq!(summary.last_passing, None);
    }

    // Intent: unsupported protocol handling preserves the raw protocol.
    // Why it exists: doctor messages should name SCSI or another observed
    //   protocol, not collapse every non-ATA drive into NVMe.
    // Scenario: smartctl reports a SCSI device.
    #[test]
    fn selftest_scsi_unsupported_protocol() {
        let json = r#"{"device":{"protocol":"SCSI"}}"#;
        let summary = parse_smartctl_selftest_log(&raw(json));
        assert_eq!(summary.unsupported_protocol.as_deref(), Some("SCSI"));
    }

    // Intent: missing protocol is not treated as SATA for self-test parsing.
    // Why it exists: the self-test log schema is brittle enough that absent
    //   protocol should Skip with a deterministic reason.
    // Scenario: smartctl JSON either omits `device` entirely or includes it
    //   without `protocol`.
    #[test]
    fn selftest_missing_protocol_unsupported() {
        for json in [r#"{}"#, r#"{"device":{}}"#] {
            let summary = parse_smartctl_selftest_log(&raw(json));
            assert_eq!(summary.unsupported_protocol.as_deref(), Some("unknown"));
        }
    }

    // Intent: malformed active-error counters clamp instead of wrapping.
    // Why it exists: `outdated > total` is malformed but must not become a huge
    //   active-error count in release or a debug panic.
    // Scenario: JSON reports one total error and five outdated errors.
    #[test]
    fn selftest_malformed_outdated_exceeds_total() {
        let json = r#"{
            "device": {"protocol": "ATA"},
            "power_on_time": {"hours": 5000},
            "ata_smart_self_test_log": {
                "standard": {
                    "count": 0,
                    "error_count_total": 1,
                    "error_count_outdated": 5
                }
            }
        }"#;
        let summary = parse_smartctl_selftest_log(&raw(json));
        assert_eq!(summary.active_errors, 0);
    }

    // Intent: active-error counters survive even if no failure entry is parsed.
    // Why it exists: doctor has a fallback Fail message for parser drift or
    //   malformed-but-parseable logs.
    // Scenario: counters report one active error but the table contains only
    //   aborted rows.
    #[test]
    fn selftest_active_errors_without_failure_entry() {
        let json = r#"{
            "device": {"protocol": "ATA"},
            "power_on_time": {"hours": 5000},
            "ata_smart_self_test_log": {
                "standard": {
                    "count": 1,
                    "error_count_total": 1,
                    "error_count_outdated": 0,
                    "table": [
                        {
                            "type": {"value": 1, "string": "Short"},
                            "status": {"value": 16, "string": "Aborted by host"},
                            "lifetime_hours": 4990
                        }
                    ]
                }
            }
        }"#;
        let summary = parse_smartctl_selftest_log(&raw(json));
        assert_eq!(summary.active_errors, 1);
        assert_eq!(summary.last_failure, None);
    }

    // Intent: missing `power_on_time.hours` is represented distinctly.
    // Why it exists: doctor can still Fail active self-test errors without POH,
    //   but must Skip age-based branches when POH is absent.
    // Scenario: ATA self-test table is present but attribute 9 is not emitted.
    #[test]
    fn selftest_no_power_on_time() {
        let json = r#"{
            "device": {"protocol": "ATA"},
            "ata_smart_self_test_log": {
                "standard": {
                    "count": 1,
                    "error_count_total": 0,
                    "error_count_outdated": 0,
                    "table": [
                        {
                            "type": {"value": 1, "string": "Short"},
                            "status": {"value": 0, "string": "Completed without error"},
                            "lifetime_hours": 4990
                        }
                    ]
                }
            }
        }"#;
        let summary = parse_smartctl_selftest_log(&raw(json));
        assert_eq!(summary.power_on_hours, None);
        assert!(summary.last_passing.is_some());
    }

    // Intent: bad JSON is a parse failure, not an unsupported protocol.
    // Why it exists: doctor messages need to distinguish corrupt smartctl
    //   output from non-ATA devices.
    // Scenario: smartctl exits 0 but stdout is not JSON.
    #[test]
    fn selftest_bad_json() {
        let summary = parse_smartctl_selftest_log(&raw("not json"));
        assert!(summary.parse_failure);
        assert_eq!(summary.unsupported_protocol, None);
    }

    // Intent: ATA self-test lifetime-hour age is wrap-aware.
    // Why it exists: self-test entries wrap at 16 bits while attribute-9
    //   power-on hours does not.
    // Scenario: current POH has crossed one wrap window and the entry is from
    //   the current 16-bit window.
    #[test]
    fn selftest_age_wraps() {
        assert_eq!(selftest_age_hours(70000, 3964), 500);
        assert_eq!(selftest_age_hours(70000, 4464), 0);
        assert_eq!(selftest_age_hours(131073, 1), 0);
    }

    // Intent: the parser trusts smartctl's reverse-chronological table order.
    // Why it exists: ATA lifetime hours can wrap, so sorting by lifetime hour
    //   would pick the wrong "last" row after enough runtime.
    // Scenario: the first passing row has a lower lifetime hour than a later
    //   passing row.
    #[test]
    fn selftest_table_is_reverse_chronological() {
        let json = r#"{
            "device": {"protocol": "ATA"},
            "power_on_time": {"hours": 70000},
            "ata_smart_self_test_log": {
                "standard": {
                    "count": 2,
                    "error_count_total": 0,
                    "error_count_outdated": 0,
                    "table": [
                        {
                            "type": {"value": 1, "string": "Short"},
                            "status": {"value": 0, "string": "Completed without error"},
                            "lifetime_hours": 100
                        },
                        {
                            "type": {"value": 1, "string": "Short"},
                            "status": {"value": 0, "string": "Completed without error"},
                            "lifetime_hours": 65000
                        }
                    ]
                }
            }
        }"#;
        let summary = parse_smartctl_selftest_log(&raw(json));
        assert_eq!(summary.last_passing.expect("passing").lifetime_hours, 100);
    }
}
