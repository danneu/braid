# Pivot: decouple the remove-confirmation warning test from prompt assembly, and cover the gate's negative side

## Context

`cmd_remove_accepted_confirm_records_prompt_with_warning` (`cli/src/remove.rs`,
~lines 1031-1055) verifies that a 2->1 `braid remove` shows the single-survivor
warning. It does so by reconstructing the *entire* expected prompt byte-for-byte:
it re-calls `format_remove_confirm(...)` with the same args production uses, wraps
it in `format!("{}\n", ...)`, appends the warning literal, and asserts
`prompts() == vec![expected]`. This mirrors the production assembly at
`remove.rs:264-289` line-for-line.

Two problems:

1. **Coupling to assembly, not behavior.** Any cosmetic tweak to how
   `execute()` assembles the prompt (a blank line, the `\n` wrapper) breaks the
   test even though the contract -- *the no-RAID1 warning appears once on the
   remove prompt, gated behind `!yes && remaining == 1`* -- is unchanged.
   Conversely, because the expected text is rebuilt from the same
   `format_remove_confirm`, a formatter-internal change reflects on both sides
   and is invisible here (it is caught instead by the dedicated formatter tests
   `remove_confirm_normal/degraded/no_hw_info` at `remove.rs:1708-1770`, which
   use structure-insensitive `contains` assertions).

2. **The finding's safety net does not exist.** The originating finding claimed
   the byte-exact warning is "already pinned end-to-end in
   `tests/cli/braid-remove-disk.py`." It is not: that VM test's only
   redundancy-reducing remove runs `braid remove disk2 --yes`
   (`braid-remove-disk.py:146-148`), and `--yes` bypasses the prompt -- the doc
   comment at `remove.rs:115-117` states the warning "never appears in
   `--dry-run` or `--yes` runs." So this Rust unit test is the **only** coverage
   of the warning prompt. We keep the warning wording pinned as a literal in the
   loosened test (so wording regressions are still caught), but we do not lean on
   a VM pin that isn't there.

**Gap this pivot also closes.** The real contract of the `if work_plan.remaining
== 1` gate is two-directional: warning IFF `remaining == 1`. The *negative*
direction (no warning when `remaining > 1`) is currently untested -- `prompts()`
is asserted exactly once in the whole module (line 1054), always on a 2->1
remove. We add a 3->2 case that proves the warning is absent.

The opposite axis -- "no prompt at all when `--yes`" -- is already implicitly
guarded: `RecordingConfirm`'s default `Verdict` is `Unexpected`
(`confirm.rs:30-36`), which `panic!`s on any unarmed `confirm()` call, so every
default-params (`yes = true`) test in the module would panic if a prompt leaked.
No new test needed for that direction.

**Scope decision:** unit-level only. No end-to-end interactive VM coverage is
added (no interactive remove path exists in the VM suite, and exercising
`RealConfirm` over stdin is a separate, larger effort). The unit seam test plus
the formatter tests cover the contract.

## Changes

All edits are in the `#[cfg(test)] mod tests` block of `cli/src/remove.rs`. No
production code changes.

### 1. Loosen `cmd_remove_accepted_confirm_records_prompt_with_warning`

Replace the byte-exact reconstruction (the `format!`/`push_str`/`assert_eq!`
block at ~1041-1054) with behavioral assertions on the single recorded prompt:

- Exactly one prompt was recorded (`prompts().len() == 1`) -- the seam is
  invoked once.
- That prompt contains the warning sentence **exactly once**
  (`prompt.matches(WARNING).count() == 1`) -- guards against a double-append
  regression while pinning the wording.
- That prompt names the target: contains `"disk2"` and `"devid 2"`.
- That prompt shows the correct pool transition: contains `"2 disks -> 1 disk"`.
  This preserves call-site coverage that `execute` passes the right
  `remaining`/`total` into the formatter -- coverage the byte-exact assert had
  incidentally. The formatter tests (`remove_confirm_degraded`) only check
  `format_remove_confirm` in isolation with hardcoded counts, never the `execute`
  wiring, so without this a future call-site bug could render `1 disk -> 2 disks`
  and still pass.

Keep the test's setup (`two_disk_healthy()`, `confirm.accept()`,
`RemovalPool::two_disk()`, `yes(false)`) and its `//` preamble, updating the
preamble wording from "records the exact assembled prompt" to the behavioral
contract (warning shown once on the named target's 2->1 prompt). Drop the now
unused `format_remove_confirm` / `RemoveConfirmDisk` / `DiskHwInfo::default()`
imports from the test body if they become unused (they remain used by the
formatter tests, so the `use super::*;` import stays).

Sketch (final wording at implementer's discretion, matching module idioms):

```rust
let prompts = f.confirm.prompts();
assert_eq!(prompts.len(), 1, "confirm must be invoked exactly once: {prompts:?}");
let prompt = &prompts[0];
assert_eq!(
    prompt.matches(SINGLE_SURVIVOR_WARNING).count(),
    1,
    "single-survivor warning must appear exactly once: {prompt:?}"
);
assert!(prompt.contains("disk2"), "prompt must name the target disk: {prompt:?}");
assert!(prompt.contains("devid 2"), "prompt must name the target devid: {prompt:?}");
assert!(
    prompt.contains("2 disks -> 1 disk"),
    "prompt must show the 2->1 pool transition: {prompt:?}"
);
```

### 2. Add a 3->2 negative-case test

New sibling test (place it directly after the loosened test, ~line 1055) proving
the warning is absent when the pool keeps redundancy. There is currently **no**
happy-path 3->2 `cmd_remove` test; the existing 2->1 accepted tests
(`remove.rs:1062-1079`) are the template. The 3-disk fixtures support a full
run: `RemovalPool::three_disk().install(MockRunner::default())` mocks every
command a 3->2 remove issues (no balance-to-single, since `remaining == 2`), and
`three_disk_healthy().remove_params()` targets `disk2` / devid 2 by default.

```rust
// Intent: a redundancy-preserving remove (3->2) shows the normal confirm
//   prompt WITHOUT the single-survivor warning.
// Why it exists: the warning is gated on `remaining == 1`; the negative
//   side of that gate was untested, so a regression that always (or never)
//   appended the warning would pass the 2->1 test alone.
// Scenario: removing disk2 from a three-disk pool leaves two disks, so the
//   operator sees the remove prompt but no no-RAID1 warning.
#[test]
fn cmd_remove_3to2_confirm_omits_redundancy_warning() {
    let f = PoolFixture::three_disk_healthy();
    f.confirm.accept();
    let runner = RemovalPool::three_disk().install(MockRunner::default());
    let fs = MockFs::storage(vec![]);

    cmd_remove(&runner, &fs, &f.remove_params().yes(false).build())
        .expect("accepted confirm should proceed");

    let prompts = f.confirm.prompts();
    assert_eq!(prompts.len(), 1, "confirm must be invoked exactly once: {prompts:?}");
    let prompt = &prompts[0];
    // Positive: it is the real 3->2 remove prompt for the named target...
    assert!(prompt.contains("disk2"), "prompt must name the target disk: {prompt:?}");
    assert!(prompt.contains("devid 2"), "prompt must name the target devid: {prompt:?}");
    assert!(
        prompt.contains("3 disks -> 2 disks"),
        "prompt must show the 3->2 pool transition: {prompt:?}"
    );
    // ...negative: but no single-survivor warning, because two disks remain.
    assert!(
        !prompt.contains(SINGLE_SURVIVOR_WARNING),
        "3->2 remove must not show the no-RAID1 warning: {prompt:?}"
    );
}
```

### 3. Single source of truth for the warning sentence

Both tests reference the same warning text -- one asserts its presence, the other
its absence -- so a wording change must update them together. Hoist a shared
`const` just above the loosened test to prevent drift:

```rust
/// The single-survivor warning sentence emitted on a 2->1 remove confirm
/// (see `RemovePlan::execute`). Pinned here so the present/absent assertions
/// in the two confirm tests stay consistent.
const SINGLE_SURVIVOR_WARNING: &str = "WARNING: Pool will have 1 disk -- no RAID1 redundancy.";
```

Use the sentence only (no trailing `\n\n`) so the assertions are robust to
surrounding-whitespace changes while still pinning the wording. This is an
independent copy of production's literal (`remove.rs:283`), so a production
wording change still fails the test.

## Critical files

- `cli/src/remove.rs` -- the only file edited (test module). Loosen one test,
  add one test, add one `const`.

## Existing helpers reused (no new code)

- `PoolFixture::two_disk_healthy()` / `three_disk_healthy()` and
  `.remove_params()` builder (`.yes(false)`, `.build()`) -- both default to
  target `disk2` / devid 2.
- `RemovalPool::two_disk()` / `three_disk()` + `.install(MockRunner::default())`
  -- supplies all command mocks; 3->2 runs to completion.
- `RecordingConfirm::accept()` and `.prompts()` (`confirm.rs:52,60`). Use
  `prompts().len()` for the count (no count helper exists).
- `MockFs::storage(vec![])` -- identical for both pool sizes.
- Assertion idiom: `assert!(x.contains(...), "...: {x:?}")`;
  `str::matches(..).count()` for the exactly-once check.

## Verification

- `just test-rust` -- both tests pass (it runs `cargo test --lib ...`, which
  includes this module's unit tests).
- Confirm the negative test actually exercises the gate: temporarily drop the
  `if work_plan.remaining == 1` guard at `remove.rs:279` so the warning is
  appended unconditionally; `cmd_remove_3to2_confirm_omits_redundancy_warning`
  must then fail. Revert.
- Confirm the loosened test still catches wording regressions: temporarily edit
  the production warning literal at `remove.rs:283`;
  `cmd_remove_accepted_confirm_records_prompt_with_warning` must fail. Revert.
- `just check-output-ascii` (the new strings are ASCII; the warning uses `--`,
  not an em dash) and `cargo fmt --check`.

## Out of scope (noted, not done)

- **End-to-end VM pin of the warning.** No interactive (non-`--yes`) remove case
  exists in `tests/cli/braid-remove-disk.py`; adding stdin-driven `RealConfirm`
  coverage is a separate, larger effort. Deferred per scope decision.
- **ADR 007 drift.** `docs/design/decisions/007-disk-pool-management.md:52-65`
  shows an original confirmation *sketch* ("Type 'remove this disk' to confirm",
  a second "A single disk failure..." line) that the shipped `confirm_yes()`
  seam never implemented -- a broad historical divergence, not a one-line warning
  fix. Out of scope for this test pivot.
