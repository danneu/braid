# Command Parsing Capabilities

## Error Handling Policy

The parse layer defaults to **fail-hard**: unexpected output, missing fields, or non-zero exit codes produce `ParseError`. Domain code never silently swallows failures.

### Exceptions

| Parser | Behavior | Rationale |
|--------|----------|-----------|
| `parse_findmnt_json` | Non-zero exit with empty stderr → empty result | findmnt exits non-zero when a mount point doesn't exist; this is a normal query, not an error. |
| `parse_cryptsetup_status` | Non-zero exit with empty stderr or "is not active" → `is_active: false` | cryptsetup exits non-zero for inactive devices; expected during probing. |
| `parse_btrfs_device_stats` | Unknown stat field names are silently ignored | btrfs-progs may add new per-device stat counters in future kernel versions (e.g. a hypothetical `discard_errs`). Rejecting unknown fields would break the parser on a routine toolchain bump. The five fields we extract (`read_io_errs`, `write_io_errs`, `flush_io_errs`, `corruption_errs`, `generation_errs`) are stable since btrfs-progs v4.x. Unknown fields are dropped, not propagated — domain code only sees the typed `DeviceErrorStats` struct. |
| `parse_btrfs_device_usage` | Unknown allocation keys are silently ignored | btrfs-progs may add new per-device allocation categories in future versions. Required fields (`Device size`, `Device slack`, `Unallocated`, device header) are fail-hard. Allocation lines (comma-separated type,profile keys) are collected; unrecognized indented key-value lines are dropped. Domain code only sees the typed `BtrfsDeviceUsageEntry` struct. |
