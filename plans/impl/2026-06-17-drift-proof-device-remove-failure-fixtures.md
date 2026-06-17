# Plan: drift-proof the `btrfs device remove` failure-stderr test fixtures

## Context

braid's `remove-missing` command removes a dead drive by its **bare numeric
devid** (`pool_remove_device_using(runner, &work_plan.missing_id.to_string(), ...)`
in `remove_missing.rs`; `Devid`'s `Display` is `self.0.fmt(f)`, so it emits a bare
number like `3`). btrfs-progs routes a numeric argument through the
`string_is_numerical` -> `BTRFS_DEVICE_SPEC_BY_ID` branch and, on failure, prints
`ERROR: error removing devid <n>: <msg>` -- the word **devid**, no quotes
(`reference/btrfs-progs/cmds/device.c`, the `is_devid` arm). The live `remove`
command removes by **mapper path**, which takes the other arm and prints
`ERROR: error removing device '<path>': <msg>` -- the word **device**, quoted.

The test fixtures that simulate the by-devid (remove-missing) failure path were
hand-written in the **by-path** shape (`error removing device '3'`), a string
btrfs-progs never emits for a numeric devid. This went unnoticed because the only
consumer, `device_remove_error` in `pool.rs`, decodes purely on the substring
`"unable to go below"` (`result.stderr.to_lowercase().contains("unable to go below")`)
-- the prefix is not load-bearing. So there is **zero production-behavior bug**;
the cost is fixture realism: the fixtures misrepresent the output this code path
actually sees, giving false confidence and masking any future change that started
parsing the prefix.

A full sweep found this is broader than first reported: **five** by-devid fixtures
use the wrong (by-path) shape, and **three** live by-path fixtures in `remove.rs`
are truncated to a bare `ERROR: error removing device` (no quoted path, no
message) -- realistic in *branch* but not in *shape*.

**Intended outcome:** every `btrfs device remove` failure fixture renders the exact
shape btrfs-progs would emit for that removal kind, and that shape becomes
*unwritable-wrong*: all fixtures route through a single pinned builder, **and** a
source-guard test rejects any inline `ERROR: error removing devi...` literal that
would bypass the builder -- so a future author cannot silently reintroduce the
drift while behavioral tests stay green (they key only on `"unable to go below"`,
not the prefix). This mirrors the existing `device_usage_raw_body` pattern braid
already uses to keep `btrfs device usage` output from drifting.

## Approach (Option A: shared builder + route all)

This is not a new abstraction; it applies braid's own established pattern. The
direct precedent is `device_usage_raw_body` in
[`cli/src/test_fixtures/shared.rs`](cli/src/test_fixtures/shared.rs): a
`pub(crate)` builder that encodes a btrfs-progs output shape, cites the kernel
source in a `///` comment, splits its variants into named constructors
(`DeviceUsageSpec::live` / `::missing`), and is locked by a byte-exact unit test
(`device_usage_raw_body_renders_canonical_live_and_missing_devices`).

### 1. Add the builder pair (`cli/src/test_fixtures/shared.rs`)

Place next to `mock_ok`. Two `pub(crate)` functions returning `String`, each with a
`///` doc comment citing `reference/btrfs-progs/cmds/device.c` and naming which
braid command produces that shape. Take `u64` for the devid (house style: matches
`DeviceUsageSpec.devid`, not the `Devid` newtype).

```rust
/// Render a `btrfs device remove` failure stderr line in btrfs-progs' by-id
/// shape. A bare numeric devid takes the `string_is_numerical` ->
/// `BTRFS_DEVICE_SPEC_BY_ID` arm in `reference/btrfs-progs/cmds/device.c`,
/// printing `error removing devid <n>` -- the word "devid", no quotes. braid's
/// `remove-missing` always removes by devid, so its failure fixtures use this.
pub(crate) fn btrfs_remove_devid_error(devid: u64, msg: &str) -> String {
    format!("ERROR: error removing devid {devid}: {msg}")
}

/// Render a `btrfs device remove` failure stderr line in btrfs-progs' by-path
/// shape: a block-device argument prints `error removing device '<path>'`
/// (quoted), per the non-`is_devid` arm in `reference/btrfs-progs/cmds/device.c`.
/// braid's live `remove` removes by mapper path, so its failure fixtures use this.
pub(crate) fn btrfs_remove_path_error(path: &str, msg: &str) -> String {
    format!("ERROR: error removing device '{path}': {msg}")
}
```

Re-export both via the same facade mechanism as `mock_ok` in
[`cli/src/test_fixtures.rs`](cli/src/test_fixtures.rs) so the existing
`use crate::test_fixtures::*` globs in the three test modules reach them (the
`remove_missing.rs` test module already calls `mock_ok`, so the glob is in scope).

### 2. Pin the builder and enforce its use (two tests in `shared.rs`)

**2a. Output pin.** Add one `#[test]` in the `shared.rs` `mod tests`, with the
house `// Intent / Why it exists / Scenario` preamble, asserting both arms
byte-for-byte:

```rust
assert_eq!(
    btrfs_remove_devid_error(3, "unable to go below three devices on raid1c3"),
    "ERROR: error removing devid 3: unable to go below three devices on raid1c3"
);
assert_eq!(
    btrfs_remove_path_error("/dev/mapper/braid-disk2", "No space left on device"),
    "ERROR: error removing device '/dev/mapper/braid-disk2': No space left on device"
);
```

The "Why it exists" note should record that `device_remove_error` keys only on the
`"unable to go below"` substring, so this pin is the *only* guard on the prefix
shape -- which is why per-fixture literals had already drifted.

**2b. Source guard (closes the enforcement gap).** The output pin only protects
the builders; it does nothing to stop a future author from reintroducing an inline
`stderr: "ERROR: error removing device ...".into()` literal at a call site, which
behavioral tests would not catch (they key on `"unable to go below"`, not the
prefix). Add a second `#[test]` that `include_str!`-scans the three call-site files
and rejects any inline device-remove failure literal, forcing all of them through
the builders:

```rust
// include_str! resolves relative to shared.rs (cli/src/test_fixtures/), so the
// three call-site files are one dir up. shared.rs itself is NOT scanned, so the
// builder bodies and this test's own needle never trip the guard.
for (name, src) in [
    ("pool.rs", include_str!("../pool.rs")),
    ("remove.rs", include_str!("../remove.rs")),
    ("remove_missing.rs", include_str!("../remove_missing.rs")),
] {
    assert!(
        !src.contains("ERROR: error removing devi"),
        "{name}: inline `btrfs device remove` failure literal found -- route it \
         through btrfs_remove_devid_error / btrfs_remove_path_error in \
         test_fixtures::shared instead of hardcoding the stderr shape"
    );
}
```

Needle rationale (verified by grep against the current tree):

- The anchored prefix `ERROR: error removing devi` matches **both** real shapes --
  by-path (`device '<path>'`) and by-id (`devid <n>`) -- so it catches a
  reintroduced literal of *either* branch, including a correctly-shaped `devid`
  one. The reviewer's narrower `error removing device` would miss an inline
  `devid` literal (`device` != `devid`).
- It is false-positive-free: the only non-fixture occurrence of `removing devi` in
  these files is the comment `// Scenario: removing devid 3 ...` in
  `remove_missing.rs`, which lacks the `ERROR: error ` anchor and so does not
  match. After step 3-4 routes every fixture through the builders, the needle
  appears **zero** times in the three files.
- Limitation to record in the test's preamble: the scan list is explicit. If a
  future command adds device-remove failure fixtures in a new module, that file
  must be added to this list. (A `cli/src/**` glob is not available to a unit test
  without a build script; the explicit list is the simple, fail-loud choice --
  a moved/renamed file breaks `include_str!` at compile time.)

### 3. Route the by-devid fixtures (the correctness core)

Replace each inline `stderr:` literal with `btrfs_remove_devid_error(...)`. Cited
by test-fn name (braid cites `path#symbol`, never line numbers):

- `cli/src/remove_missing.rs`
  - `two_missing_journal_persists_restore_raid1_false` -> `btrfs_remove_devid_error(3, "No space left on device")`
  - `journal_survives_device_remove_failure` -> `btrfs_remove_devid_error(3, "No space left on device")`
  - `cmd_remove_missing_failure_emits_missing_replace_hint` -> `btrfs_remove_devid_error(3, "unable to go below three devices on raid1c3")`
- `cli/src/pool.rs`
  - `device_remove_result_missing_raid1c3_min_includes_replace_hint` -> `btrfs_remove_devid_error(2, "unable to go below three devices on raid1c3")`
  - `pool_remove_device_using_failure_emits_missing_replace_hint` -> `btrfs_remove_devid_error(2, "unable to go below three devices on raid1c3")`

The two ENOSPC fixtures keep `"No space left on device"` as the message (the
kernel `strerror(ENOSPC)` text); they were previously *doubly* wrong -- bare
`error removing device:` with the devid omitted entirely.

### 4. Route the by-path fixtures (consistency + de-truncation)

The three `pool.rs` live fixtures are already the correct *shape*; route them
through the builder anyway so every fixture has one source of truth. The three
`remove.rs` fixtures are truncated and get completed in the process.

- `cli/src/pool.rs` (keep existing `"unable to go below ..."` messages -- they are
  load-bearing for the hint decode):
  - `device_remove_result_live_raid1_min_includes_balance_hint` -> `btrfs_remove_path_error("/dev/mapper/braid-disk2", "unable to go below two devices on raid1")`
  - `device_remove_result_live_raid1c3_min_includes_balance_hint` -> `btrfs_remove_path_error("/dev/mapper/braid-disk2", "unable to go below three devices on raid1c3")`
  - `pool_remove_device_failure_emits_live_balance_hint` -> `btrfs_remove_path_error("/dev/mapper/braid-disk2", "unable to go below two devices on raid1")`
- `cli/src/remove.rs` (currently bare `"ERROR: error removing device"`; complete to
  the full by-path shape with a realistic errno -- ENOSPC matches the
  mid-relocation failure these journal-survival tests model):
  - `remove_journal_pre_membership_carries_live_member_devids` -> `btrfs_remove_path_error("/dev/mapper/braid-disk2", "No space left on device")`
  - `journal_survives_evict_failure` -> `btrfs_remove_path_error("/dev/mapper/braid-disk2", "No space left on device")`
  - `cmd_remove_resolves_name_to_uuid_and_journals_uuid` -> `btrfs_remove_path_error(<mapper path this test removes>, "No space left on device")` (confirm the disk -- the sweep read it as `disk2`)

## Critical files

- `cli/src/test_fixtures/shared.rs` -- new builder pair + two tests (output pin
  2a, `include_str!` source guard 2b), modeled on `device_usage_raw_body` and its
  test in the same file.
- `cli/src/test_fixtures.rs` -- re-export the two fns alongside `mock_ok`.
- `cli/src/remove_missing.rs`, `cli/src/pool.rs`, `cli/src/remove.rs` -- route the
  11 fixture call sites.

## Non-goals (do not change)

- **No production code changes.** `device_remove_error` / `device_remove_result`
  in `pool.rs` stay as-is; the `"unable to go below"` substring decode is correct
  and intentionally prefix-agnostic. This plan is fixture realism only.
- Do not alter the decode to start parsing the prefix -- the builder's job is to
  make fixtures honest *now* so such a future change would be testable, not to
  invite it.
- Do not touch parser golden fixtures under `cli/tests/fixtures/` -- device-remove
  failure stderr is not a `just capture-all-fixtures` artifact (you cannot force a
  real ENOSPC / min-devices failure during capture), which is exactly why a
  hand-authored, test-pinned builder is the right mechanism here.

## Verification

1. `just test-rust` (or, without `just`, its exact curated lane
   `cargo test --lib --bin braid --test golden_nixos_26_05 --test tty_guard --test confirm_yes`
   -- **not** `cargo test -p ...`, which also runs the fixture-gated
   `golden_nixos_unstable` lane and would fail on missing unstable fixtures
   unrelated to this change) -- all device-remove tests must
   still pass. The behavioral assertions key on `"unable to go below"`, `"hint:"`,
   the `braid replace`/`braid recover` substrings, journal survival, and
   `"btrfs device remove failed (exit 1)"` -- none assert the prefix, so the shape
   change is behavior-preserving. Both new tests (output pin 2a + source guard 2b)
   should pass on first run.
2. Confirm the source guard closes the gap it claims to: temporarily revert one
   call site to an inline `stderr: "ERROR: error removing device ...".into()`
   literal and re-run `just test-rust`. The expected result is the precise
   demonstration of the finding -- every *behavioral* test still passes (proving
   the prefix was never load-bearing), but the **source-guard test (2b) fails**
   and names the offending file. Revert the experiment.
3. `just test-parsers` is unaffected (no golden-fixture or parser change) but run
   it to confirm nothing in the parser lane regressed.
4. No manual grep step is needed: the source-guard test (2b) runs in the normal
   `cargo test` lane and continuously enforces "zero inline device-remove failure
   literals" -- a one-time grep would only check the moment it was run.

## Implementation notes

- The plan (step 1) assumed all three test modules reach the builders via a
  `use crate::test_fixtures::*` glob. None of them actually use a glob:
  `remove.rs` and `remove_missing.rs` have explicit `use crate::test_fixtures::{...}`
  import lists, and `pool.rs::tests` has no module-level `test_fixtures` import at
  all (it fully-qualifies `crate::test_fixtures::MockFs` inline). Adapted per file
  to match each module's existing idiom: added `btrfs_remove_path_error` /
  `btrfs_remove_devid_error` to the explicit import lists in `remove.rs` /
  `remove_missing.rs`, and fully-qualified `crate::test_fixtures::btrfs_remove_*`
  at the five `pool.rs` call sites (matching pool.rs's own `crate::test_fixtures::MockFs`
  pattern). The re-export in `test_fixtures.rs` is still required so the names are
  reachable by either form.
- Skipped the optional negative-control experiment (Verification step 2) that
  temporarily reverts a call site to prove the source guard fails: it is a
  throwaway demonstration that would dirty the tree, and the guard's `include_str!`
  paths are already proven to resolve (the test compiled and passed) while the
  anchored-needle grep returns zero matches across the three call-site files.
- Skipped `just test-parsers` (Verification step 3): it boots NixOS VMs to validate
  parser golden fixtures, none of which this fixture-shape-only change touches. The
  plan itself flags the parser lane as unaffected.
