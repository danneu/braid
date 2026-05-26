# Plan: make tool-parsed display/diagnostic arithmetic overflow-safe

## Context

`UpscOutput::watts_estimated` (`cli/src/parse/types.rs:683`) computes
`(u32::from(pct) * nominal + 50) / 100`, where `nominal`
(`realpower_nominal_watts`) is an **ungated** `u32` parsed from NUT driver
output (`cli/src/parse/upsc.rs:86`). `pct` is bounded to `0..=100`, so the
multiply overflows `u32` once `nominal >= ~42.9M`. braid builds release via
crane (`overflow-checks` off) -> silent wraparound (nonsense watts in
`braid ups status`); `cargo test`/debug -> panic.

This is one instance of a class: **arithmetic on an ungated, tool-parsed
integer**. A sweep of the CLI found more sites than the cited one, which split
by what the arithmetic feeds:

- **Display / diagnostic** (watts, data-ratio validation, scrub durations, LUKS
  key size, error tallies) -- a saturated/None/widened result can at worst
  mis-render a number. This plan fixes these (sites 1-5 below), making each
  operation *total* (cannot panic or wrap) without inventing semantic bounds.
- **Capacity / preflight** (btrfs space-accounting sums that feed
  `check_raid1_relocation_space` at `preflight.rs:328` and
  `raid1_chunk_pair_capacity` at `capacity.rs:29`) -- here a wrong total can
  mask ENOSPC and corrupt a device-removal decision, so the right per-site
  behavior needs real analysis, not a blanket sweep. **Deferred** to a separate
  whole-CLI audit (see below); this plan neither touches them nor flips the
  global release `overflow-checks` profile.

Precedent already in the tree: `parse_pct` (`upsc.rs:147`) gates percent to
`0..=100`. We deliberately do **not** copy that "magic ceiling" approach for
watts/duration/key-size -- unlike percent, those have no semantically-correct
upper bound, so a ceiling would silently drop legitimately-large values.
Total arithmetic is the principled fix.

## The sites and per-site fix

Mechanism is chosen by what the value means and the function's return shape:

- `Option`/`Result`-returning parsers propagate `None`/`Err` (fail-closed --
  callers already handle it);
- a bare-integer return that provably fits a wider type widens (e.g. u32 ->
  u64 math, then cast back);
- diagnostic **counters** (error tallies) saturate, matching the existing
  `saturating_sub` on SMART error counts (`smartctl.rs:218-219`): a saturated
  count still reads as "too many," and we never drop a whole status parse
  because one counter is huge.

### 1. `watts_estimated` -- `cli/src/parse/types.rs:681-686` (returns `Option<u32>`)

Widen to u64, cast back. `pct <= 100` so the result is at most `u32::MAX`
(`(100 * u32::MAX + 50) / 100 == u32::MAX`), making the cast lossless.

```rust
(Some(pct), Some(nominal)) => {
    // Widen so a garbage `realpower.nominal` cannot overflow the multiply;
    // pct <= 100 means the result is at most u32::MAX, so the cast is lossless.
    Some(((u64::from(pct) * u64::from(nominal) + 50) / 100) as u32)
}
```

### 2. `DataRatio::parse` -- `cli/src/parse/types.rs:191-195` (returns `Option<Self>`)

`whole`/`frac_val` are ungated `u32` parses fed from `btrfs filesystem usage`
(`btrfs_filesystem_usage.rs:60`). Use checked ops; `None` becomes a clean parse
error at the caller.

```rust
let hundredths = match frac.len() {
    1 => whole.checked_mul(100)?.checked_add(frac_val.checked_mul(10)?)?,
    2 => whole.checked_mul(100)?.checked_add(frac_val)?,
    _ => return None,
};
```

### 3. `parse_duration_hms` -- `cli/src/parse/helpers.rs:26` (returns `Option<u64>`)

Ungated `u64` parses from a `btrfs scrub status` "H:MM:SS" string. Checked ops.

```rust
h.checked_mul(3600)?
    .checked_add(m.checked_mul(60)?)?
    .checked_add(s)
```

### 4. cryptsetup key size -- `cli/src/parse/cryptsetup_luks_dump.rs:98-103` (feeds `u32` field; fn returns `Result`)

`k.key_size * 8` (u64, from luksDump JSON) then `as u32`. Two defects: u64
multiply overflow (astronomical) and the silent `as u32` truncation
(reachable at `key_size > ~536M`). **Fail closed, not to `0`:** this function
already rejects every other malformed field (`segments.0`/`offset`/`size`/
empty `encryption`) as `ParseError::InvalidJson` with a field-specific detail
(`cryptsetup_luks_dump.rs:65-96`), so an oversized key must do the same.
`keyslots` is a `BTreeMap<String, _>`, so iterate to keep the slot id for the
detail. Keep only the genuine empty-keyslots case as `0`.

```rust
let key_size_bits = match parsed.keyslots.iter().next() {
    None => 0, // no keyslots: tolerated as today
    Some((slot, k)) => {
        let bits = k.key_size.checked_mul(8).ok_or_else(|| ParseError::InvalidJson {
            cmd: raw.cmd.clone(),
            detail: format!("keyslots.{slot}.key_size {} overflows u64 (*8)", k.key_size),
        })?;
        u32::try_from(bits).map_err(|_| ParseError::InvalidJson {
            cmd: raw.cmd.clone(),
            detail: format!("keyslots.{slot}.key_size {bits} bits exceeds u32"),
        })?
    }
};
```

### 5. error-counter sums -- `btrfs_scrub_status.rs`, `types.rs`, `status.rs` (u64 counters)

Ungated `u64` btrfs error counters, summed/accumulated. These are diagnostic
tallies (display only -- no capacity/preflight decision reads them), so they
saturate per the counter rule above. Source: btrfs prints these as plain
numbers (`reference/btrfs-progs/cmds/scrub.c:128,238`).

- `parse_error_summary` (`btrfs_scrub_status.rs:113-129`): the `.sum::<u64>()`
  over per-bucket `key=value` counts -> `.fold(0u64, |a, v| a.saturating_add(v))`.
- `acc.error_count += count` (`btrfs_scrub_status.rs:225,227`, Error-summary and
  continuation-line accumulation) -> `acc.error_count = acc.error_count.saturating_add(count)`.
- `DeviceScrubEntry::total_errors` (`types.rs:292-299`): the six-field sum ->
  chained `.saturating_add(...)`.
- `DiskErrors::total` (`status.rs:220-222`): the five-field sum (`btrfs device
  stats` counters) -> chained `.saturating_add(...)`. Same shape as
  `total_errors`; fixed here for consistency rather than left as a deferred
  sibling. **Also** route the inline duplicate of this sum in the human status
  renderer (`status.rs:1367`, `e.read + e.write + ... > 0`) through `e.total() >
  0`: it gates the "Action: add replacement disk" guidance at `status.rs:1393`,
  so a wrapped-to-zero sum would suppress that guidance. Dedup and fix in one.

## Deferred: whole-CLI release overflow-checks (separate plan)

An earlier draft proposed `[profile.release] overflow-checks = true` as a global
backstop. **Dropped from this plan.** The switch is whole-CLI, and the CLI still
has unaudited parsed-value arithmetic *outside* the display/diagnostic set that
feeds **mutating** preflight: `allocated_by_type` -> `check_raid1_relocation_space`
(`preflight.rs:328`), `raid1_chunk_pair_capacity` (`capacity.rs:29-30`,
`largest + rest`), and the byte sums `logical_used_bytes` / `used_bytes`
(`types.rs:105,454`). Flipping the profile before those are handled would turn a
silent wrap into a production panic on a removal/replace path; saturating them
blindly could instead mask ENOSPC and wrongly pass a preflight. Both are worse
than the status-quo wrap, and choosing checked-with-error vs saturating is a
per-site judgement.

So release hardening belongs in its own plan that audits every space-accounting
/ capacity / preflight site, picks the right behavior per site, and *then* flips
the profile. This plan stands alone without it: sites 1-5 are correctness
improvements for garbage tool input regardless of build profile, and debug-build
overflow checks already make the regression tests below meaningful.

## Tests (regression -- each with the Intent/Why/Scenario preamble per AGENTS.md)

The shared property: **the parser does not panic or wrap on adversarially
large but syntactically valid integer input.** Structure-insensitive
(asserts the output value / `None`, not internals). `cargo test` runs debug,
so each test panics against the *current* code (overflow) and passes after the
fix -- clean TDD signal.

- `cli/src/parse/upsc.rs` (existing `tests` mod): feed `ups.load: 100` +
  `ups.realpower.nominal: 4294967295`; assert `watts_estimated() == Some(4294967295)`
  (no panic/wrap).
- `cli/src/parse/types.rs` (existing `tests` mod): `DataRatio::parse("99999999.0")`
  (whole > 42.9M) `== None`.
- `cli/src/parse/helpers.rs` (**no test mod yet** -- add `#[cfg(test)] mod tests`;
  `parse_duration_hms` is `pub(super)`, reachable from a child module):
  `parse_duration_hms("99999999999999999:00:00") == None`.
- `cli/src/parse/cryptsetup_luks_dump.rs` (existing `tests` mod at :116), two
  cases, each asserting `Err(ParseError::InvalidJson { .. })` (fail closed --
  not `0`, not a truncated value):
  - `key_size: 600000000` -- `*8` fits u64 but exceeds `u32::MAX`, exercising
    the `u32::try_from` branch;
  - `key_size: 18446744073709551615` (u64::MAX) -- exercises the `checked_mul(8)`
    branch; an impl that dropped `checked_mul` and kept `key_size * 8` would
    panic here in debug.
- `cli/src/parse/btrfs_scrub_status.rs` (existing `tests` mod): feed an
  `Error summary: read=18446744073709551615 csum=1` line **followed by a
  `Corrected: 1` continuation line**, so the input overflows both the `.sum()`
  (read+csum) *and* the `acc.error_count += count` accumulation (saturated
  summary + continuation); assert the parse succeeds with `error_count ==
  u64::MAX`, no panic. The continuation line is what pins the `+=` fix -- a
  summary-only test would still pass even if `acc.error_count += count` were
  left unfixed.
- `cli/src/parse/btrfs_scrub_status_per_device.rs` (existing `tests` mod): build
  a per-device entry whose two counters sum past `u64::MAX`; assert
  `total_errors() == u64::MAX`, no panic.
- `cli/src/status.rs` (existing `tests` mod), two cases:
  - helper: build a `DiskErrors` whose fields sum past `u64::MAX` (e.g. `read =
    u64::MAX`, `write = 1`); assert `total() == u64::MAX`, no panic.
  - callsite: drive the human status renderer (the same entry point the existing
    `human_*` status tests use) with a disk whose `DiskErrors` sum overflows;
    assert no panic and that the `Action:` replacement guidance is emitted --
    pinning the `status.rs:1367` callsite, not just the helper.

## Verification

- `just test-rust` (preferred over raw `cargo test`; runs debug, so overflow
  checks are live and the regression tests actually exercise the panic path).
- No NixOS VM tests or fixtures needed: pure in-crate parser logic, no
  tool-version dependency. Not a fixture-refresh event.
- Do not run `cargo fmt`/`just fmt` (AGENTS.md) -- keep edits narrow.

## Dead code to delete

`DataRatio::logical_bytes` (`types.rs:202-204`, `device_size_bytes * 100` in
u64) has **no production caller** -- it is exercised only by its two unit tests
(`types.rs:734-740`). Its overflow threshold is ~184.5 PB (`u64::MAX / 100`,
i.e. ~0.18 EB -- petabyte-, not exabyte-scale, correcting an earlier draft).
Rather than carry an unused method with a latent overflow, delete the helper
and its two tests.

`DataRatio::parse` (site 2) stays: it is reached on every `btrfs filesystem
usage` parse as the "Data ratio" line format validator
(`btrfs_filesystem_usage.rs:60`), independent of `logical_bytes`.
