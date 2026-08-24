use crate::cmd::RawCommandOutput;
use crate::parse::types::{
    SelftestEntry, SelftestKind, SelftestSummary, SmartEvidence, SmartHealth, SmartProbe,
};
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
                evidence: None,
                celsius: None,
            };
        }
    }

    let parsed: RawSmartctlOutput = match serde_json::from_str(&raw.stdout) {
        Ok(v) => v,
        Err(_) => {
            return SmartProbe {
                health: SmartHealth::Unknown,
                evidence: None,
                celsius: None,
            };
        }
    };

    let celsius = parsed
        .temperature
        .as_ref()
        .and_then(|t| t.current)
        .and_then(|v| i16::try_from(v).ok());

    // Build evidence once from the protocol's source detail log, then derive the
    // verdict from it -- a single threshold definition lives in
    // `SmartEvidence::fields` (it must agree with the column, the human line, and
    // the TUI rows). `evidence` is `None` in two cases: no `smart_status`
    // (Unknown), or `smart_status` present but the per-protocol detail log absent
    // -- e.g. a passing USB-NVMe bridge that emits no health log, where a
    // zero-filled `Nvme { available_spare: 0, .. }` would read as total spare
    // exhaustion.
    let evidence = parsed
        .smart_status
        .as_ref()
        .and_then(|_| match detect_protocol(&parsed) {
            DiskProtocol::Nvme => parsed
                .nvme_smart_health_information_log
                .as_ref()
                .map(nvme_evidence),
            DiskProtocol::Sata => parsed.ata_smart_attributes.as_ref().map(sata_evidence),
        });

    let health = match &parsed.smart_status {
        None => SmartHealth::Unknown,
        Some(status) if !status.passed => SmartHealth::Failing,
        Some(_) => match evidence {
            Some(e) if !e.concerns().is_empty() => SmartHealth::Degraded,
            _ => SmartHealth::Healthy,
        },
    };

    SmartProbe {
        health,
        evidence,
        celsius,
    }
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

/// Protocol of the probed drive, derived from `device.protocol`. Defaults to
/// SATA when the field is absent or unrecognized. Factored out of the old
/// `classify_health` so `parse_smartctl` can pick the evidence source on the
/// `Failing` path too, not only when `smart_status.passed` is true.
fn detect_protocol(parsed: &RawSmartctlOutput) -> DiskProtocol {
    match parsed.device.as_ref().and_then(|d| d.protocol.as_deref()) {
        Some(p) if p.eq_ignore_ascii_case("nvme") => DiskProtocol::Nvme,
        _ => DiskProtocol::Sata,
    }
}

/// SATA evidence from ATA attributes, reusing the exact reads the old
/// `classify_sata` performed: `Reallocated_Sector_Ct` masked to its lower 16
/// bits, plus the raw48 pending/uncorrectable counts. The mask keeps a drive
/// that reports reallocation *events* in the upper words (0 actual sectors) out
/// of the concern set.
fn sata_evidence(attrs: &RawAtaSmartAttributes) -> SmartEvidence {
    let read = |name: &str| {
        attrs
            .table
            .iter()
            .find(|a| a.name == name)
            .map_or(0, |a| a.raw.value)
    };
    SmartEvidence::Sata {
        // raw16(raw16) format: sector count is the lower 16 bits
        // (https://github.com/smartmontools/smartmontools/blob/RELEASE_7_5/smartmontools/drivedb.h#L83)
        reallocated_sectors: read("Reallocated_Sector_Ct") & 0xFFFF,
        // raw48 format: the full value is the count
        // (https://github.com/smartmontools/smartmontools/blob/RELEASE_7_5/smartmontools/drivedb.h#L118-L119)
        pending_sectors: read("Current_Pending_Sector"),
        offline_uncorrectable: read("Offline_Uncorrectable"),
    }
}

/// NVMe evidence from the health-information log, reusing the exact five reads
/// the old `classify_nvme` performed.
fn nvme_evidence(nvme: &RawNvmeHealth) -> SmartEvidence {
    SmartEvidence::Nvme {
        media_errors: nvme.media_errors,
        critical_warning: nvme.critical_warning,
        percentage_used: nvme.percentage_used,
        available_spare: nvme.available_spare,
        available_spare_threshold: nvme.available_spare_threshold,
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

    const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nixos-26.05");

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{FIXTURE_DIR}/{name}")).expect("selftest fixture reads")
    }

    // Intent: the required stable NVMe contract fixture parses as healthy and
    //   preserves every evidence field braid uses for its health verdict.
    // Why it exists: the old test silently returned when the fixture was
    //   absent, so it asserted nothing from its introduction onward.
    // Scenario: a healthy NVMe reports no warnings or media errors, ample
    //   spare capacity, and low endurance usage through smartctl JSON.
    #[test]
    fn nvme_fixture_healthy() {
        let probe = parse_smartctl(&raw(&fixture("smartctl-nvme-healthy.json")));
        assert_eq!(probe.health, SmartHealth::Healthy);
        assert_eq!(
            probe.evidence,
            Some(SmartEvidence::Nvme {
                media_errors: 0,
                critical_warning: 0,
                percentage_used: 12,
                available_spare: 100,
                available_spare_threshold: 10,
            })
        );
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

    // -- Evidence: health is now derived from the evidence the parser carries.
    // These assert the (health, evidence) pair end-to-end so the verdict and the
    // itemized counts can never disagree.

    // Intent: clean SATA reports zeroed Sata evidence and a Healthy verdict.
    // Why it exists: the verdict is `concerns().is_empty()` over the same reads
    //   the old classify_sata used; the zeroed evidence proves no concern fired.
    // Scenario: a healthy SATA drive with all three attributes at 0.
    #[test]
    fn sata_clean_evidence_and_health() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "ATA"},
            "ata_smart_attributes": {"table": [
                {"name": "Reallocated_Sector_Ct", "raw": {"value": 0}},
                {"name": "Current_Pending_Sector", "raw": {"value": 0}},
                {"name": "Offline_Uncorrectable", "raw": {"value": 0}}
            ]}
        }"#;
        let probe = parse_smartctl(&raw(json));
        assert_eq!(probe.health, SmartHealth::Healthy);
        assert_eq!(
            probe.evidence,
            Some(SmartEvidence::Sata {
                reallocated_sectors: 0,
                pending_sectors: 0,
                offline_uncorrectable: 0,
            })
        );
    }

    // Intent: a degraded SATA drive carries the reallocated count as evidence.
    // Why it exists: the human line and TUI row render this count; it must be
    //   the masked lower-16-bit value, not a bare boolean.
    // Scenario: 8 reallocated sectors -> Degraded with Sata{8,0,0}.
    #[test]
    fn sata_degraded_evidence_carries_count() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "ATA"},
            "ata_smart_attributes": {"table": [
                {"name": "Reallocated_Sector_Ct", "raw": {"value": 8}},
                {"name": "Current_Pending_Sector", "raw": {"value": 0}},
                {"name": "Offline_Uncorrectable", "raw": {"value": 0}}
            ]}
        }"#;
        let probe = parse_smartctl(&raw(json));
        assert_eq!(probe.health, SmartHealth::Degraded);
        assert_eq!(
            probe.evidence,
            Some(SmartEvidence::Sata {
                reallocated_sectors: 8,
                pending_sectors: 0,
                offline_uncorrectable: 0,
            })
        );
    }

    // Intent: a `passed:false` SATA drive with readable attributes is Failing but
    //   still carries its Sata evidence.
    // Why it exists: the verdict reaches Failing via smart_status, independent of
    //   attributes; evidence must still be built so the human/TUI can itemize the
    //   non-nominal attribute behind a failing drive.
    // Scenario: drive self-reports failure while attribute 5 shows 5 reallocated.
    #[test]
    fn sata_failing_with_attributes_keeps_evidence() {
        let json = r#"{
            "smart_status": {"passed": false},
            "device": {"protocol": "ATA"},
            "ata_smart_attributes": {"table": [
                {"name": "Reallocated_Sector_Ct", "raw": {"value": 5}},
                {"name": "Current_Pending_Sector", "raw": {"value": 0}},
                {"name": "Offline_Uncorrectable", "raw": {"value": 0}}
            ]}
        }"#;
        let probe = parse_smartctl(&raw(json));
        assert_eq!(probe.health, SmartHealth::Failing);
        assert_eq!(
            probe.evidence,
            Some(SmartEvidence::Sata {
                reallocated_sectors: 5,
                pending_sectors: 0,
                offline_uncorrectable: 0,
            })
        );
    }

    // Intent: a healthy NVMe drive carries its five-field Nvme evidence.
    // Why it exists: NVMe is fully implemented, not TODO'd; available_spare 100
    //   over a threshold of 10 with low wear must read Healthy with no concern.
    // Scenario: a fresh enterprise NVMe at 12% wear.
    #[test]
    fn nvme_healthy_evidence_and_health() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "NVMe"},
            "nvme_smart_health_information_log": {
                "critical_warning": 0,
                "media_errors": 0,
                "available_spare": 100,
                "available_spare_threshold": 10,
                "percentage_used": 12
            }
        }"#;
        let probe = parse_smartctl(&raw(json));
        assert_eq!(probe.health, SmartHealth::Healthy);
        assert_eq!(
            probe.evidence,
            Some(SmartEvidence::Nvme {
                media_errors: 0,
                critical_warning: 0,
                percentage_used: 12,
                available_spare: 100,
                available_spare_threshold: 10,
            })
        );
    }

    // Intent: NVMe wear at/over 90% is Degraded and carries the wear figure.
    // Why it exists: percentage_used >= 90 is the wear threshold; it must drive
    //   the verdict and surface the exact value.
    // Scenario: an NVMe drive at 92% rated endurance.
    #[test]
    fn nvme_wear_degraded_evidence() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "NVMe"},
            "nvme_smart_health_information_log": {
                "critical_warning": 0,
                "media_errors": 0,
                "available_spare": 100,
                "available_spare_threshold": 10,
                "percentage_used": 92
            }
        }"#;
        let probe = parse_smartctl(&raw(json));
        assert_eq!(probe.health, SmartHealth::Degraded);
        assert_eq!(
            probe.evidence,
            Some(SmartEvidence::Nvme {
                media_errors: 0,
                critical_warning: 0,
                percentage_used: 92,
                available_spare: 100,
                available_spare_threshold: 10,
            })
        );
    }

    // Intent: a passing NVMe drive that emits no health-information log yields
    //   `evidence: None`, not a zero-filled Nvme that reads as spare exhaustion.
    // Why it exists (F2): every RawNvmeHealth field is `#[serde(default)]` -> 0,
    //   and 0 is the *failure* value for available_spare. A USB-NVMe bridge that
    //   omits the log must stay Healthy with no evidence.
    // Scenario: a passing NVMe behind a bridge that drops the health log.
    #[test]
    fn nvme_logless_passing_has_no_evidence() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "NVMe"}
        }"#;
        let probe = parse_smartctl(&raw(json));
        assert_eq!(probe.health, SmartHealth::Healthy);
        assert_eq!(probe.evidence, None);
    }

    // Intent: a passing SATA drive with no ata_smart_attributes log yields
    //   `evidence: None` (gated on log presence for symmetry with NVMe).
    // Why it exists (F2): even though SATA zero-fill is benign, evidence is gated
    //   on the source log so a drive with no attribute table itemizes nothing.
    // Scenario: a SATA drive that reports smart_status but no attribute table.
    #[test]
    fn sata_logless_passing_has_no_evidence() {
        let json = r#"{
            "smart_status": {"passed": true},
            "device": {"protocol": "ATA"}
        }"#;
        let probe = parse_smartctl(&raw(json));
        assert_eq!(probe.health, SmartHealth::Healthy);
        assert_eq!(probe.evidence, None);
    }

    // Intent: no smart_status -> Unknown and `evidence: None`.
    // Why it exists: the verdict is Unknown and there is nothing to itemize even
    //   if a detail log happens to be present.
    // Scenario: smartctl JSON omits smart_status entirely.
    #[test]
    fn unknown_has_no_evidence() {
        let json = r#"{
            "device": {"protocol": "NVMe"},
            "nvme_smart_health_information_log": {
                "critical_warning": 0,
                "media_errors": 0,
                "available_spare": 100,
                "available_spare_threshold": 10,
                "percentage_used": 0
            }
        }"#;
        let probe = parse_smartctl(&raw(json));
        assert_eq!(probe.health, SmartHealth::Unknown);
        assert_eq!(probe.evidence, None);
    }
}
