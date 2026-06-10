# Newtype migration: `RawDeviceSize` / `Luks2SegmentOffset`

## Context

`check_replace_target_capacity` gates whether a replacement disk is large enough
for a RAID1 `btrfs replace`. To decide, it models the LUKS mapper capacity the
replacement will expose via a private helper:

```rust
fn mapper_capacity_from_dynamic_segment(raw_target: u64, offset: u64, by_id: &str) -> Result<u64, String>
```

`raw_target` is the candidate device's raw byte size (lsblk `-b`); `offset` is the
LUKS2 data-segment offset. Both are `u64`, so **transposing them compiles cleanly**.
A swap inverts the `raw_target <= offset` guard and corrupts the `raw - offset`
capacity math, so the check either spuriously rejects a valid disk or, worse,
under-sizes the mapper and lets an undersized disk through to `luksFormat` before
btrfs's own replace-time size check would reject it.

This is the highest safety/churn-ratio item in the newtype survey: safety-critical,
swap-compiles-clean, and trivially contained. The fix is two distinct newtypes so
the swapped call fails to typecheck. Intended outcome: the transposition becomes a
compile error, with zero behavior change and zero test churn.

## Scope (verified against the code)

- All changes land in `cli/src/preflight.rs`. `grep` confirms
  `mapper_capacity_from_dynamic_segment` and its sibling `target_raw_size` have
  **no callers outside this file**, and nothing outside constructs the newtypes.
- Both consumers reach the helper through the public entry
  `check_replace_target_capacity` (`PresentLuks` dynamic arm + `PresentNotLuks`
  arm), which takes `runner`/`by_id` -- not the newtypes -- so the public API is
  unchanged.
- The offset sources stay `u64` at their origin: the parser field
  `CryptsetupLuksDumpOutput::segment_offset_bytes` (`cli/src/parse/types.rs`) and
  the `LUKS2_DEFAULT_HDR_SIZE` const (`cli/src/luks.rs`). We wrap at the two
  preflight call sites, leaving the parser and its fixture-asserting tests
  untouched. **1 file, 2 call sites.**

### Type placement

Define both as **private tuple structs in `preflight.rs`**, not in `crate::types`.
Their only job is to disambiguate two args of one private function in this file;
there are no cross-module consumers, so centralizing in `types.rs` would add import
surface without adding safety. (Deliberately deferred, separate survey item: typing
the parser field as `Luks2SegmentOffset` in `parse/types.rs` next to
`Luks2SegmentSize` -- that broadens the blast radius into the parser tests and is
not this entry.)

## Changes (all in `cli/src/preflight.rs`)

**1. Add two newtypes** near `ReplaceSourceProbe`. Each gets a `///` per the
doc-comment convention (intent at the boundary, not a signature restatement).
`Copy` matches the surrounding `u64`/`ReplaceSourceProbe`/`Luks2SegmentSize` style
and keeps call sites pass-by-value:

```rust
/// Candidate replacement device's raw byte size (lsblk `-b`). Distinct from
/// `Luks2SegmentOffset` so `mapper_capacity_from_dynamic_segment` cannot
/// transpose size and offset: a swap inverts the capacity guard and would
/// format or accept an undersized disk before `btrfs replace`'s own check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawDeviceSize(u64);

/// LUKS2 data-segment offset in bytes (real luksDump offset for existing
/// targets, the default header size for fresh ones). Subtracted from
/// `RawDeviceSize` to model mapper capacity; typed apart from it so the
/// subtraction operands cannot be reversed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Luks2SegmentOffset(u64);
```

**2. `target_raw_size` returns the newtype** -- one wrap point feeds both call
sites via their existing `let raw_target = target_raw_size(...)?` bindings. Map
the `Option<u64>` before the `ok_or_else`, keeping the existing error text:

```rust
fn target_raw_size<R: CommandRunner>(runner: &R, by_id: &str) -> Result<RawDeviceSize, String> {
    confirm::query_disk_hw_info(runner, by_id)
        .size
        .map(RawDeviceSize)
        .ok_or_else(|| format!(
            "failed to read raw size for target {by_id} with lsblk -- cannot verify the new disk is large enough"
        ))
}
```

**3. `mapper_capacity_from_dynamic_segment` takes the newtypes**, returns `u64`
capacity unchanged (the mapper capacity is out of scope -- only the two
transposable inputs are typed). Body unwraps via `.0` for the guard, the
subtraction, and the four `format!` args, so **no `Display` impl is needed**:

```rust
fn mapper_capacity_from_dynamic_segment(
    raw_target: RawDeviceSize,
    offset: Luks2SegmentOffset,
    by_id: &str,
) -> Result<u64, String> {
    if raw_target.0 <= offset.0 {
        return Err(format!(
            "target raw size {} ({}) is not larger than LUKS2 segment offset {} ({}) for {} -- header may be corrupt",
            raw_target.0, format_bytes(raw_target.0),
            offset.0, format_bytes(offset.0), by_id,
        ));
    }
    Ok(raw_target.0 - offset.0)
}
```

**4. Wrap `offset` at the two arms of `check_replace_target_capacity`:**

- `PresentLuks` dynamic arm: `parsed.segment_offset_bytes` ->
  `Luks2SegmentOffset(parsed.segment_offset_bytes)`
- `PresentNotLuks` arm: `LUKS2_DEFAULT_HDR_SIZE` ->
  `Luks2SegmentOffset(LUKS2_DEFAULT_HDR_SIZE)`

`raw_target` already flows in typed from step 2, so only the offset is wrapped here.

## Tests

**No new tests, no test edits.** The protection is a compile-time guarantee -- no
runtime test can assert "the swapped call won't compile," and there is no new
behavior to cover (project bar: behavioral, structure-insensitive tests only). The
existing suite in `preflight.rs` already pins everything the newtype guards, and
all of it drives the public `check_replace_target_capacity`, so it compiles and
passes unchanged -- which is itself the regression check that the wrap/unwrap
plumbing is correct:

- `check_replace_target_capacity_refuses_when_raw_below_offset` -- pins guard
  direction (exactly what a swap inverts).
- `..._existing_dynamic_segment`, `..._existing_dynamic_segment_does_not_round_sector_size`,
  `..._fresh_accepts_equal_and_larger` -- pin the `raw - offset` math.

## Verification

- `just test-rust` (or `cargo build -p braid && cargo test -p braid`) -- green with
  zero test changes is the success signal.
- Review-only compile sanity (do not commit): transpose the two args at one call
  site, confirm it fails to typecheck, revert.
- ASCII-output check (`scripts/docs/check-output-ascii.py`) is unaffected -- the
  refusal message text is byte-identical.

## Non-goals

- Not typing `DiskHwInfo.size` (`confirm.rs`) or the parser's
  `segment_offset_bytes` field -- separate, broader survey items.
- Not introducing a `MapperCapacity` return newtype -- only the two transposable
  inputs are in scope.

## Risk

Minimal: single file, no public-API change, no behavior change, no test churn. Only
care-point is unwrapping `.0` in the four `format!` args plus the guard/subtraction
so no `Display` impl is required.
