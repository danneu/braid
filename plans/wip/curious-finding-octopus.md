# Rename StatusCode: Healthy→Intact, merge status fields

## Context

`StatusCode::Healthy` doesn't accurately describe the state. `Intact` (mounted, all devices present) is clearer. Also, `StatusReport` has redundant `status_code: StatusCode` and `status: String` fields — JSON should just have `"status": "intact"`.

## Changes

### `cli/src/status.rs`

**Enum (line 24):**

- `Healthy` → `Intact`

**Method (line 31):**

- Rename `display_status` → `display_human` (now only used for human output)
- `StatusCode::Healthy` → `StatusCode::Intact` in match arm

**Struct `StatusReport` (line 42):**

- Remove `status_code: StatusCode`
- Change `status: String` → `status: StatusCode`

**Construction sites (~8 places, lines 283-466):**

- Remove `status_code: code,` lines
- Change `status: code.display_status(...)` → `status: code`

**Human formatter `format_status_human` (line 811):**

- `report.status` → `report.status.display_human(report.missing_count.unwrap_or(0))`
- `report.status_code == StatusCode::NotMounted` → `report.status == StatusCode::NotMounted`

**All `StatusCode::Healthy` refs** (~30 in tests) → `StatusCode::Intact`

**Test assertions:**

- Remove `assert_eq!(obj["status_code"], ...)` lines
- `assert_eq!(obj["status"], "healthy")` → `assert_eq!(obj["status"], "intact")`
- `assert_eq!(obj["status"], "not mounted")` → `assert_eq!(obj["status"], "not_mounted")`
- `assert!(obj["status"]...contains("DEGRADED"))` → `assert_eq!(obj["status"], "degraded")`
- Human output assertions: `"healthy"` → `"intact"` in `assert!(human.contains(...))`

**Test StatusReport constructions (~25 places):**

- Remove `status_code: code,` / `status_code: StatusCode::*,`
- Change `status: code.display_status(...)` → `status: code`
- Change `status: "healthy".to_owned()` → `status: StatusCode::Intact`
- Change `status: "DEGRADED (1 missing device)".to_owned()` → `status: StatusCode::Degraded`

### CLI integration tests

**`tests/cli/braid-status-rust.py`:**

- Line 69, 90: `"healthy"` → `"intact"`
- Line 116: `s["status_code"]` → `s["status"]`, `"healthy"` → `"intact"`
- Line 161: `s["status_code"]` → `s["status"]`
- Line 178: `s["status_code"]` → `s["status"]`

**`tests/cli/braid-status-during-balance.py`:**

- Line 79-80: `s["status_code"]` → `s["status"]`, `"healthy"` → `"intact"`

**`tests/cli/braid-unified.py`:**

- Line 50: `"healthy"` → `"intact"`
- Line 66: `s["status"]` value `"healthy"` → `"intact"`

**`tests/cli/braid-bootstrap.py`:**

- Line 92: `"healthy"` → `"intact"`

### `README.md` (lines 195–222)

- Document JSON `"status"` field values: `"intact"`, `"degraded"`, `"not_mounted"` (single field, replaces old `"status_code"` + `"status"`)
- Document human output status text: `intact`, `DEGRADED (N missing device(s))`, `not mounted`

## Verification

- `just test-rust`
- `just test braid-status-rust braid-status-during-balance braid-unified braid-bootstrap`
