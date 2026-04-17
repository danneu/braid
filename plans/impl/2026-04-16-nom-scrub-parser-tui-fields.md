# Rewrite aggregate scrub parser to nom + add missing fields to TUI

## Context

The aggregate scrub parser (`btrfs scrub status --raw`) is half-nom half-ad-hoc string matching, inconsistent with the per-device parser which is fully nom-based. During a running scrub, the TUI only shows "now", Total, and Rate -- missing progress %, bytes scrubbed, duration, time left, and ETA, all of which are present in the command output. The parser also looks for a standalone `% done` line that doesn't exist in `--raw` format; the percentage is embedded in the `Bytes scrubbed:` line as `(14.78%)`.

## Approach

### 1. Extract shared helpers into `cli/src/parse/helpers.rs`

Move `parse_ctime` (currently in `btrfs_scrub_status.rs`, already `pub(super)`) and `parse_duration_hms` (currently private in `btrfs_scrub_status_per_device.rs`) into a new `cli/src/parse/helpers.rs` module. Register it in `parse/mod.rs`. Update both parser modules to import from `super::helpers`.

### 2. Update `ScrubState` in `cli/src/parse/types.rs`

```rust
pub enum ScrubState {
    Never,
    Running {
        started_at: Option<ScrubTimestamp>,
        duration_secs: Option<u64>,
        time_left_secs: Option<u64>,
        eta: Option<ScrubTimestamp>,
        total_bytes: Option<u64>,
        bytes_scrubbed: Option<u64>,
        rate_bytes_per_sec: Option<u64>,
        error_count: u64,
    },
    Completed {
        started_at: ScrubTimestamp,
        error_count: u64,
        duration_secs: Option<u64>,    // was: duration: Option<String>
        total_bytes: Option<u64>,
        rate_bytes_per_sec: Option<u64>,
    },
    Unknown,
}
```

Key decisions:
- **No `pct` field.** Percentage is computed from `bytes_scrubbed / total_bytes` at display time. Avoids float-in-Eq issues (keeps `#[derive(Eq)]`). The btrfs source computes it the same way: `100.0 * bytes_scrubbed / bytes_total`.
- **`duration` changes from `Option<String>` to `Option<u64>` (seconds).** Consistent with the per-device parser. Enables formatted display ("5m 58s" instead of raw "0:05:58").
- **`started_at` added to Running.** Always emitted by btrfs for running scrubs. Also handles `Scrub resumed:` (btrfs emits this instead of `Scrub started:` when a scrub is resumed).
- **`error_count` added to Running.** `Error summary:` is always emitted regardless of state.

### 3. Rewrite aggregate parser (`cli/src/parse/btrfs_scrub_status.rs`)

Replace ad-hoc `strip_prefix`/`ends_with` matching with nom combinators. Keep the line-by-line iteration + accumulator pattern (same as the per-device parser).

**Nom combinators to write:**

| Combinator | Parses | Returns |
|---|---|---|
| `parse_scrub_started_or_resumed` | `"Scrub started:"` or `"Scrub resumed:"` + ws + ctime | `&str` (timestamp) |
| `parse_status_line` | `"Status:"` + ws + value | `&str` |
| `parse_duration_line` | `"Duration:"` + ws + H:M:S | `u64` (seconds) |
| `parse_time_left_line` | `"Time left:"` + ws + H:M:S | `u64` (seconds) |
| `parse_eta_line` | `"ETA:"` + ws + ctime | `ScrubTimestamp` |
| `parse_total_to_scrub` | `"Total to scrub:"` + ws + u64 | `u64` |
| `parse_bytes_scrubbed` | `"Bytes scrubbed:"` + ws + u64 + opaque trailing suffix | `u64` (bytes only; pct computed at display) |
| `parse_rate_line` | `"Rate:"` + ws + u64 + `"/s"` (ignore optional limit suffix) | `u64` |
| `parse_error_summary` | `"Error summary:"` + ws + (`"no errors found"` \| key=val pairs) | `u64` |
| `parse_error_continuation` | `"Corrected:"` / `"Uncorrectable:"` / `"Unverified:"` + ws + u64 | `u64` |

When errors are present, btrfs-progs prints continuation lines after `Error summary:` (scrub.c:245-247):
```
Error summary:    read=3 csum=1
  Corrected:      2
  Uncorrectable:  1
  Unverified:     0
```

The continuation lines appear only when `err_cnt || err_cnt2` is true. `err_cnt2 = corrected_errors + uncorrectable_errors` can be nonzero even when the `Error summary:` line has no `key=val` entries (all of read/super/verify/csum are zero). In that case the output is:
```
Error summary:   
  Corrected:      5
  Uncorrectable:  0
  Unverified:     0
```

The parser must sum values from both the summary line (`read=N`, `csum=N`, etc.) **and** the continuation lines (`Corrected:`, `Uncorrectable:`, `Unverified:`) into `error_count` to avoid reporting 0 when errors exist. Add an inline test where `Error summary:` has no key=value entries but `Uncorrectable:` is nonzero.

**Accumulator:**
```rust
struct PartialScrub {
    started_at: Option<ScrubTimestamp>,
    is_running: bool,
    error_count: u64,
    bytes_scrubbed: Option<u64>,
    duration_secs: Option<u64>,
    time_left_secs: Option<u64>,
    eta: Option<ScrubTimestamp>,
    total_bytes: Option<u64>,
    rate_bytes_per_sec: Option<u64>,
}
```

Finalize: `is_running` -> `Running { ... }`, else `started_at.is_some()` -> `Completed { ... }`, else `Unknown`.

`parse_bytes_scrubbed` extracts only the leading raw byte count. The parenthesized percentage suffix (e.g. `(14.78%)`) is consumed but not validated -- btrfs computes it via `100.0 * bytes_scrubbed / bytes_total` (scrub.c:201) and can produce `nan`/`inf` when `bytes_total` is 0 or under unusual libc formatting. The parser must not fail on non-numeric suffix content.

Remove the dead `ends_with("% done")` code path entirely.

### 4. Fix downstream consumers

All guided by compiler errors after the type change.

- **`cli/src/status.rs`** (`ScrubReport::Running`): Currently `pct: Option<u8>`. Compute from `bytes_scrubbed / total_bytes` in the `get_scrub_report` conversion, truncated to u8. No change to JSON API shape.
- **`cli/src/idle.rs`** (`BusyReason::ScrubRunning`): Currently `pct: Option<u8>`. Same: compute from bytes, truncated to u8.
- **`cli/src/scrub_cancel.rs`**: Pattern-matches `ScrubState::Running { .. }` with no field extraction. No change needed.
- **`cli/src/tui/mod.rs`** (demo mode): Update `ScrubState::Completed` literal: `duration: Some("0:00:00".to_owned())` -> `duration_secs: Some(0)`.
- **`cli/src/tui/view/mod.rs`** (tests): Update `sample_pool()` same way.

### 5. Update TUI scrub tab (`cli/src/tui/view/mod.rs`)

**Running state -- new layout:**

```
 Scrub ─────────────────────────────────────────────────────
 Status       running (14.78%)
 Progress     82.1 GiB / 555.4 GiB
 Rate         234.8 MiB/s
 Time left    34m 24s
 ETA          Thu Apr 16 19:09
 Errors       0
```

- **Status row:** "running" + percentage if both `bytes_scrubbed` and `total_bytes` are present, formatted as `(XX.XX%)`.
- **Progress row:** `bytes_scrubbed` / `total_bytes`, both human-formatted via `ByteUnit::friendliest`. Only shown if `bytes_scrubbed` is present.
- **Rate row:** same as current, shown if present.
- **Time left row:** `format_duration_secs(time_left_secs)`, shown if present.
- **ETA row:** formatted timestamp, shown if present.
- **Errors row:** always shown (0 or count). During a running scrub, errors mean corruption is being found in real time.

**Completed state changes:**
- Duration row: use `format_duration_secs(duration_secs)` instead of raw H:M:S string.

**Add helper:**
```rust
fn format_duration_secs(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 { format!("{}h {}m {}s", h, m, s) }
    else if m > 0 { format!("{}m {}s", m, s) }
    else { format!("{}s", s) }
}
```

**Update `scrub_lines()`** to return correct row counts for new Running layout.

### 6. Add running fixture via progress-monitoring capture

Add aggregate running-scrub capture to `tests/progress-monitoring.py` alongside the existing per-device capture. The per-device capture already uses `dm_delay` to slow scrub on disk3 and polls in a tight shell loop (line 120-148). Add a parallel capture of `btrfs scrub status --raw` (no `-d`) during the same window, writing to `btrfs-scrub-running.txt`.

This ensures `just capture-all-fixtures` and `just capture-all-fixtures-unstable` refresh the running fixture deterministically, keeping it in the same lifecycle as the existing per-device running fixture. Add a contract test in the parser module.

### 7. Update tests

Every new or materially changed test must have the standard Intent / Why / Scenario block comment per AGENTS.md.

**Parser tests (`btrfs_scrub_status.rs`):**
- Update `scrub_running_inline`: add `Bytes scrubbed:`, `Time left:`, `ETA:`, `Duration:` lines. Assert new fields.
- Update `scrub_parses_nixos_25_11_completed`: destructure `duration_secs` instead of `duration`.
- Update `scrub_completed_with_errors_inline`: same.
- Update `scrub_completed_with_rate_limit`: same.
- Add `scrub_running_fixture`: contract test against new `btrfs-scrub-running.txt` fixture.
- Add `scrub_running_minimal`: running with only `Status: running` and no optional fields -- all new fields are `None`/0.
- Add `scrub_errors_uncorrectable_only`: `Error summary:` line with no key=value entries, but `Uncorrectable: 2` on continuation line. Assert `error_count == 2`.
- Remove dead `% done` test path if any exists.

**TUI view tests (`view/mod.rs`):**
- Update `sample_pool()`: `duration_secs: Some(0)` instead of `duration: Some(...)`.
- Update affected snapshots (scrub tab renders Duration differently).
- Add `snapshot_scrub_tab_running`: sample pool with `ScrubState::Running { ... }` including new fields.

**Status/idle tests:** follow compiler errors.

## Files to modify

| File | Change |
|---|---|
| `cli/src/parse/helpers.rs` | **New.** Shared `parse_ctime`, `parse_duration_hms` |
| `cli/src/parse/mod.rs` | Register `helpers` module |
| `cli/src/parse/types.rs` | `ScrubState` enum changes |
| `cli/src/parse/btrfs_scrub_status.rs` | Full nom rewrite + test updates |
| `cli/src/parse/btrfs_scrub_status_per_device.rs` | Import `parse_ctime`/`parse_duration_hms` from helpers instead of local/sibling |
| `cli/src/tui/view/mod.rs` | Scrub tab rendering + `format_duration_secs` + test updates |
| `cli/src/tui/mod.rs` | Demo mode `ScrubState` literal |
| `cli/src/status.rs` | Compute pct from bytes in `get_scrub_report` |
| `cli/src/idle.rs` | Compute pct from bytes in scrub check |
| `cli/tests/fixtures/nixos-25.11/btrfs-scrub-running.txt` | **New.** Running scrub fixture (captured by progress-monitoring) |
| `tests/progress-monitoring.py` | Add aggregate running-scrub capture alongside existing per-device capture |

## Implementation order

1. Create `parse/helpers.rs` -- extract shared helpers, update imports in both parsers. `cargo test` to confirm no regressions.
2. Update `ScrubState` in `types.rs` -- compiler errors guide all remaining work.
3. Rewrite the aggregate parser with nom combinators (including error continuation lines).
4. Add aggregate running-scrub capture to `tests/progress-monitoring.py`. Run `just capture-all-fixtures` to produce `btrfs-scrub-running.txt`. Add contract test.
5. Fix downstream consumers (`status.rs`, `idle.rs`, `scrub_cancel.rs`).
6. Update TUI rendering + add `format_duration_secs`.
7. Update TUI demo mode + tests + snapshots.
8. `cargo test` -- all green.

## Verification

1. `just test-rust` -- all parser + TUI snapshot tests pass.
2. `just test-parsers` -- parser compatibility canary (covers aggregate scrub parser against live VM output).
3. `just capture-all-fixtures` -- confirms running fixture is captured by the progress-monitoring test.
4. Manual: on a running scrub, `braid tui` scrub tab shows progress %, bytes scrubbed, rate, time left, ETA, errors.
