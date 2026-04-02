# Plan: simplify status fields in StatusReport

## Context

`StatusReport` has both `status_code` (enum: healthy/degraded/not_mounted) and `status` (human string like "DEGRADED (1 missing device)"). The human string is redundant — it duplicates info from `status_code` + `missing_count`. But `status_code` itself is needed: not-mounted reports omit `missing_count`, so consumers can't derive mount state without it.

Goal: keep `status_code` as a machine-readable field, drop the `status` human string, and clean up the human output.

## Changes

### 1. `cli/src/status.rs` — StatusReport struct

- Remove `status: String` field
- Rename `status_code` → `status` (same enum, values: `healthy`/`degraded`/`not_mounted`)
- Delete `display_status()` method on StatusCode

### 2. `cli/src/status.rs` — report construction (~5 sites)

- Remove `status: code.display_status(...)` lines
- Rename `status_code: code` → `status: code`

### 3. `cli/src/status.rs` — `format_status_human`

- Remove the `Status:   {report.status}` line
- Update NotMounted early-return check: `report.status == StatusCode::NotMounted`
- Add WARNING line when degraded (before balance, after allocation):
  ```
  WARNING:  degraded - 1 missing device
  WARNING:  degraded - 2 missing devices
  ```
  Derive from `report.missing_count`. Singular when 1, plural otherwise.

### 4. `cli/src/status.rs` — tests

- Update all ~25 StatusReport constructions: remove `status:` string field, rename `status_code:` → `status:`
- `status_json_not_mounted`: assert `obj["status"] == "not_mounted"`, update key count (drops by 1)
- `status_json_healthy`: assert `obj["status"] == "healthy"`, no `status_code` key
- `status_json_degraded`: assert `obj["status"] == "degraded"`
- Human tests: replace `contains("not mounted")` / `contains("DEGRADED")` with `contains("WARNING:")` where applicable, or just check absence of WARNING for healthy

### 5. `tests/cli/braid-status-rust.py`

- `s["status_code"]` → `s["status"]` everywhere
- Remove assertions on the old `s["status"]` string field

### 6. `tests/cli/braid-status-during-balance.py`

- `s["status_code"]` → `s["status"]`

### 7. `README.md`

- No change needed

## Files to modify

- `cli/src/status.rs`
- `tests/cli/braid-status-rust.py`
- `tests/cli/braid-status-during-balance.py`

## Verification

1. `just test-rust` — all Rust unit tests pass
2. `just test braid-status-rust` — integration test passes
3. `just test braid-status-during-balance` — integration test passes
