# smartd alert conditions

Reference for what triggers smartd to call the notification script.

## Braid's current smartd config

```
-a -o on -S on -m <nomailer> -M exec ${smartdAlertScript}
```

`-a` expands to: `-H -f -t -l error -l selftest -l selfteststs -C 197 -U 198`

`-o on` and `-S on` are non-monitoring config flags (enable offline testing and attribute autosave on the drive).

## SATA: conditions that fire the alert script

smartd polls every 30 minutes. Each condition has a `SMARTD_FAILTYPE` value passed to the script.

| SMARTD_FAILTYPE | Directive | Trigger |
|---|---|---|
| `Health` | `-H` | Overall SMART health status = FAILING |
| `Usage` | `-f` | Any Usage (Old_age) attribute value <= vendor threshold |
| `ErrorCount` | `-l error` | ATA error log count increased since last poll |
| `SelfTest` | `-l selftest` | New self-test failures detected |
| `CurrentPendingSector` | `-C 197` | Non-zero raw value on attr 197 |
| `OfflineUncorrectableSector` | `-U 198` | Non-zero raw value on attr 198 |
| `FailedHealthCheck` | `-H` | SMART health command itself failed |
| `FailedReadSmartData` | | Could not read SMART attribute data |
| `FailedReadSmartErrorLog` | | Could not read SMART error log |
| `FailedReadSmartSelfTestLog` | | Could not read self-test log |
| `FailedOpenDevice` | | `open()` failed — device disappeared |
| `Temperature` | `-W` | Temperature >= CRIT threshold (NOT in `-a`, must be added explicitly) |

## SATA: what `-a` does NOT alert on

These are only logged to syslog, not sent to the script:

- **Reallocated_Sector_Ct (5)** raw value increases — only alerted if value crosses the vendor threshold (via `-f`). To alert on raw value changes, add `-R 5!`.
- **Reported_Uncorrect (187)**, **End-to-End_Error (184)**, **Reallocated_Event_Count (196)** — same: threshold breach only via `-f`, no raw-value alerts.
- **Temperature** — not monitored at all without `-W DIFF,INFO,CRIT`.
- **Prefail/Usage attribute value changes** — `-t` (= `-p -u`) logs these to syslog at LOG_INFO, but does not fire the script.

## SATA: syslog-only directives (no script trigger)

| Directive | What it monitors |
|---|---|
| `-p` | Prefail attribute value changes (LOG_INFO) |
| `-u` | Usage attribute value changes (LOG_INFO) |
| `-t` | All attribute changes (= `-p -u`) |
| `-r ID` | Report raw value alongside normalized (informational) |
| `-R ID` (without `!`) | Track raw value changes (LOG_INFO, no email) |
| `-R ID!` (with `!`) | Track raw value changes (LOG_CRIT + fires script) |
| `-l offlinests` | Offline Data Collection status changes (LOG_CRIT, no email) |
| `-l selfteststs` | Self-Test execution status changes (LOG_CRIT, no email) |

## NVMe: how `-a` works differently

NVMe has a standardized health model — no vendor-specific attribute IDs or thresholds. The ATA-only parts of `-a` (`-C 197`, `-U 198`, `-o on`, `-S on`) are silently ignored.

### NVMe conditions that fire the alert script

| SMARTD_FAILTYPE | Directive | Trigger |
|---|---|---|
| `Health` | `-H` | Critical Warning byte != 0 (any bit set) |
| `Usage` | `-f` | Percentage Used > 95% or Media and Data Integrity Errors increased |
| `ErrorCount` | `-l error` | Error Information Log Entries count increased (device-related errors only, since smartmontools 7.4) |
| `SelfTest` | `-l selftest` | New self-test failures (requires smartmontools 7.5+) |
| `FailedHealthCheck` | `-H` | SMART health command itself failed |
| `FailedReadSmartData` | | Could not read SMART data |
| `FailedReadSmartErrorLog` | | Could not read error log |
| `FailedReadSmartSelfTestLog` | | Could not read self-test log |
| `FailedOpenDevice` | | `open()` failed — device disappeared |

### The Critical Warning byte (`-H`)

A bitmask where any bit set fires the alert:

| Bit | Meaning |
|-----|---------|
| 0 | Available spare fallen below threshold |
| 1 | Temperature above/below acceptable range |
| 2 | Reliability degraded (excessive writes beyond warranty) |
| 3 | Media placed in read-only mode |
| 4 | Volatile memory backup (power-loss protection capacitor) failed |

As of smartmontools 7.5, `-H MASK` (hex) can ignore specific bits, e.g. `-H 0xfb` ignores bit 2 (reliability/warranty warning).

### NVMe syslog-only tracking (no script trigger)

| Directive | What it monitors |
|---|---|
| `-p` | Available Spare changes (LOG_INFO) |
| `-u` | Percentage Used and Media Errors changes (LOG_INFO) |
| `-t` | All of the above (= `-p -u`) |
| `-l selfteststs` | Self-test execution status changes (LOG_CRIT, no email) |

### NVMe vs SATA summary

NVMe monitoring is more straightforward because the spec defines exactly what "unhealthy" means, whereas SATA relies on vendor-specific attribute definitions and generously-set thresholds. The same `-a` config line works for both — smartd adapts per device type.

## SATA attributes worth monitoring

Based on real Seagate SATA output.

### Reliable indicators (unambiguous, no vendor-encoding issues)

| ID | Name | Notes |
|----|------|-------|
| 5 | `Reallocated_Sector_Ct` | Sectors remapped due to read errors |
| 184 | `End-to-End_Error` | Internal data path integrity failure. Non-zero = serious. |
| 187 | `Reported_Uncorrect` | Uncorrectable errors reported to host. Non-zero = data loss occurred. |
| 196 | `Reallocated_Event_Count` | Remap operations (complements attr 5). Non-zero = active reallocation. |
| 197 | `Current_Pending_Sector` | Sectors waiting to be remapped |
| 198 | `Offline_Uncorrectable` | Sectors unreadable during offline test |

### Useful but with caveats

| ID | Name | Notes |
|----|------|-------|
| 10 | `Spin_Retry_Count` | Failed spin-up. Non-zero = mechanical trouble. |
| 188 | `Command_Timeout` | High values = dying drive, but some timeouts normal during power events. |

### Avoid using raw values for comparison

| ID | Name | Why |
|----|------|-----|
| 1 | `Raw_Read_Error_Rate` | Seagate packs composite value (errors in lower bits, total ops in upper). Raw number is meaningless for threshold comparison. Other vendors vary too. |
| 7 | `Seek_Error_Rate` | Same Seagate composite encoding. |

### Not disk errors

| ID | Name | Why |
|----|------|-----|
| 191 | `G-Sense_Error_Rate` | Shock sensor. Low values normal for a moved drive. |
| 193 | `Load_Cycle_Count` | Wear indicator, not an error. |
| 199 | `UDMA_CRC_Error_Count` | Almost always a cable/connection problem, not the drive. |

## Relationship to TUI `classify_sata`

The TUI currently checks raw values of 3 attributes: `Reallocated_Sector_Ct`, `Current_Pending_Sector`, `Offline_Uncorrectable`. This is complementary to smartd — smartd handles real-time alerts (with its own set of checks), while the TUI gives at-a-glance status from `smartctl --json` output. They don't need to be identical but should cover the same ground between them.
