# Pin confirm-prompt hardware-info device routing through `execute()`

## Context

`braid`'s mutating commands (`remove`, `replace`, `add`) print an operator
confirm prompt that includes a hardware line (model / size / serial) for each
disk. That line is produced by `confirm::query_disk_hw_info(runner, <device>)`,
and **which device path is passed is governed by an Active invariant**, decision
024 (`docs/design/decisions/024-luks-uuid-identity.md`, "Present-device probes
use live paths"):

- a disk that is **present** in the live pool -> probe the **live backing path**
  (`PoolState::underlying_for_uuid`, i.e. `PoolDevice.underlying`);
- a disk that is **not yet present** (an add/replace target) -> probe the
  **by-id** handle.

The production code already routes correctly at every callsite. The problem is
**test coverage**: nothing pins the routing through `execute()`. Today the only
hw-line tests are pure formatter tests that hand-build `DiskHwInfo` and call the
`format_*_confirm` functions directly (e.g. `remove.rs#remove_confirm_normal`),
and the execute-level confirm tests assert prompts built with
`DiskHwInfo::default()` (empty hw) against runners that register **no** matching
`LsblkField` handler. Because `confirm::get_lsblk_field` swallows
`CmdError::MissingMock` to `None` (`.ok()?`), the hw line silently disappears and
the assertions pass regardless of which device was queried.

Consequence: a regression that fed a present-disk prompt a persisted by-id
handle, the mapper path, or an empty string -- exactly what decision 024
forbids -- would pass every existing test. The blast radius is cosmetic (a
wrong/blank model/serial/size line in an interactive prompt), so this is a
low-severity but genuinely unpinned wiring. This plan closes the gap for all
three sibling commands with one shared test pattern.

This originated as a Low/Testing finding scoped to `remove`; investigation
(`/verify-issue`) showed the identical untested wiring in `replace` and `add`,
so the ideal fix is the consistent cross-command pin, not a lone `remove` test.

## Goal / non-goals

- **Goal:** add execute-level tests that prove each command's confirm prompt
  resolves its hw line from the *correct* device path -- live backing for
  present disks, by-id for not-yet-present disks -- so a routing regression
  fails a test.
- **Non-goal (no production change):** the routing is already correct. Do **not**
  introduce a runtime "is it present?" abstraction over the device choice -- each
  command knows statically whether its disk is present (remove target = present;
  add target = not present; replace = one of each), so a shared runtime helper
  would add indirection without value.
- **Out of scope:** `preflight.rs#target_raw_size`
  (`confirm::query_disk_hw_info(runner, by_id)`) is a capacity *validation* gate,
  not prompt-facing, and is already covered by
  `preflight.rs#check_replace_target_capacity_refuses_when_lsblk_none` and
  siblings. Leave it.

## What's already correct (do not change)

| Command | Callsite | Device passed | Why |
|---|---|---|---|
| `remove` | `remove.rs#execute` | `&work_plan.target_underlying` | present -> live backing |
| `replace` (old) | `replace.rs#execute` | `pool.underlying_for_uuid(&old_uuid)` | present -> live backing |
| `replace` (new) | `replace.rs#execute` | `new_by_id.as_str()` | not present -> by-id |
| `add` | `add.rs#execute` | `disk.by_id.as_str()` | not present -> by-id |

## Plan

### 1. Shared test helper (`cli/src/test_fixtures/shared.rs`)

Add a helper next to `shared.rs#mock_ok` that registers the three single-field
lsblk outputs `query_disk_hw_info` issues, keyed on one device path:

```rust
/// Register `lsblk` Model/Serial/Size outputs for `device` so a confirm
/// prompt's hw line resolves only when the probe is routed to THIS path.
/// Lets routing tests pin that a present-disk prompt queries the live
/// backing path (decision 024) and a target prompt queries the by-id handle.
pub(crate) fn with_lsblk_hw_info(
    runner: MockRunner,
    device: &str,
    model: &str,
    serial: &str,
    size: u64,
) -> MockRunner { /* three .with_output(CmdRequest::LsblkField{device, field}, mock_ok(..)) */ }
```

Notes:
- Returns exit-0 outputs via `mock_ok`; `get_lsblk_field` trims and `Size` is
  parsed with `parse::<u64>()`, so emit the integer (`format!("{size}")`).
- `.with_output` is checked *after* fixture `with_handler` closures
  (`cmd.rs#dispatch` tries handlers first, then the outputs map), and the
  existing `RemovalPool::install` handler returns `None` for `LsblkField`, so a
  fixture-installed runner falls through to these outputs cleanly. No ordering
  trap.
- Also expose the value-per-field core (e.g. `lsblk_hw_field_output(field,
  model, serial, size) -> RawCommandOutput`) if convenient, so `add`'s custom
  runner (below) can reuse it. Optional -- inline arms are fine.
- **Re-export through the facade.** `cli/src/test_fixtures.rs` is the
  `pub(crate)` facade; the `remove`/`replace` tests reach shared helpers only
  through it. Add `with_lsblk_hw_info` (and `lsblk_hw_field_output` if added) to
  the existing `pub(crate) use shared::{...}` list there -- alongside `mock_ok` --
  or the new tests cannot import it.

### 2. `remove` -- new test (`cli/src/remove.rs` tests)

Mirror the existing `remove.rs#cmd_remove_accepted_confirm_records_prompt_with_warning`
harness (`RemovalPool::two_disk().install(MockRunner::default())`,
`f.remove_params().yes(false)`, `f.confirm.accept()`), but wrap the runner with
`with_lsblk_hw_info(runner, "/dev/vdc", MODEL, SERIAL, SIZE)`. `/dev/vdc` is
disk2's live backing path (the fixture's `CryptsetupStatus` handler reports
`device: /dev/vdc` for `braid-disk2`, which `probe_pool` records as
`PoolDevice.underlying`).

Assert (structure-insensitive, matching remove's existing `contains` style):
the single recorded prompt `contains(MODEL)` and `contains("serial SERIAL")`.
Single disk in the prompt, so `contains` is sufficient and discriminating: those
strings enter the prompt *only* if the probe hit `/dev/vdc`; a regression to the
mapper path, a by-id handle, or `""` leaves the hw line blank and fails.

### 3. `replace` -- new test (`cli/src/replace.rs` tests)

Mirror `replace.rs#cmd_replace_accepted_confirm_records_prompt_with_warning`
(bare `MockRunner::default()`, plan built via `replace_work_plan_for_test` with
`ReplaceSource::Live{ mapper: braid-disk2, devid 2 }`, `total_devices: 1`,
`f.replace_params().yes(false)`, `f.confirm.accept()`, then
`let _ = plan.execute(...)` -- execute may fail downstream on the bare runner,
but the prompt is already recorded at the confirm gate).

Register **two** hw mocks with **distinct** values:
- old disk (present): `with_lsblk_hw_info(runner, "/dev/test-2", OLD_MODEL,
  OLD_SERIAL, OLD_SIZE)` -- `replace.rs#replace_work_plan_test_pool` sets the live
  source's `underlying` to `/dev/test-{devid}` = `/dev/test-2`, which is what
  `pool.underlying_for_uuid(&old_uuid)` returns;
- new disk (not present): `with_lsblk_hw_info(runner, "/dev/disk/by-id/virtio-disk3",
  NEW_MODEL, NEW_SERIAL, NEW_SIZE)`.

Assert byte-exact (matching replace's existing prompt-test style): build
`expected` via `format_replace_confirm` with `ReplaceConfirmOld{ hw:
Some(&old_hw) }` and `ReplaceConfirmNew{ hw: &new_hw }` populated to the same
values, append the single-disk warning, and `assert_eq!(f.confirm.prompts(),
vec![expected])`. Byte-exact + distinct old/new values makes this both
path-sensitive (wrong path -> blank line -> mismatch) and **swap-sensitive**
(old/new device args transposed -> old line shows NEW_MODEL -> mismatch), which
is the most plausible replace regression.

### 4. `add` -- extend the existing accepted-confirm test (`cli/src/add.rs` tests)

`add`'s `add.rs#RecoverableAddRunner` is a custom `CommandRunner` shared by
several tests and returns `CmdError::MissingMock` for `LsblkField`. Adding lsblk
output unconditionally would change prompts for every yes(false) test, so
**isolate it**: gate the lsblk arm behind a constructor/flag variant (e.g.
`RecoverableAddRunner::with_hw_info()` setting a `report_hw: bool`; when set,
match `CmdRequest::LsblkField{ device, field } if device ==
"/dev/disk/by-id/virtio-disk2"` and return Model/Serial/Size before the `_ =>
MissingMock` arm). `::new()` callers (the declined test and the
proceeds-to-device-add test) are unaffected.

Then update `add.rs#add_accepted_confirm_records_prompt` to use the hw-enabled
runner and rebuild its `expected` with a populated `DiskHwInfo` (the same
model/serial/size the runner returns) instead of `DiskHwInfo::default()`. The
test stays byte-exact; the populated hw now only matches if the probe was routed
to the by-id path `/dev/disk/by-id/virtio-disk2`.

(Choice: extend the existing test rather than add a new one, because add's
execute path is heavy -- full luksFormat/open/device-add/balance -- and a second
near-duplicate run buys nothing over enabling hw in the existing byte-exact
prompt test. remove/replace get dedicated new tests because their harnesses are
light and they had no byte-exact execute prompt test to extend.)

### Test preamble

Every new/changed test keeps the project's `// Intent: / Why it exists: /
Scenario:` preamble (`docs/dev/testing.md`). Frame "Why it exists" around
decision 024 and the silent-`MissingMock`-to-`None` swallow that hides the gap.

## Critical files

- `cli/src/test_fixtures/shared.rs` -- new `with_lsblk_hw_info` helper.
- `cli/src/test_fixtures.rs` -- re-export `with_lsblk_hw_info` from the
  `pub(crate) use shared::{...}` facade list (next to `mock_ok`).
- `cli/src/remove.rs` -- new routing test (tests module).
- `cli/src/replace.rs` -- new routing test (tests module).
- `cli/src/add.rs` -- gated lsblk arm on `RecoverableAddRunner` + update
  `add_accepted_confirm_records_prompt`.
- Reuse: `confirm.rs#query_disk_hw_info`, `confirm.rs#format_hw_info_line`,
  `confirm::RecordingConfirm`, `shared.rs#mock_ok`, `cmd.rs#MockRunner`
  (`with_output`/`with_handler`), `types.rs#underlying_for_uuid`.

## Verification

1. `just test-rust` (or targeted -- the package is `braid-cli`, not `braid`:
   `cargo test -p braid-cli --lib remove::tests`,
   `cargo test -p braid-cli --lib replace::tests`,
   `cargo test -p braid-cli --lib add::tests`) -- all green.
2. **Prove the tests are discriminating** (gold standard for a coverage pin):
   temporarily break each production callsite and confirm the matching test goes
   red, then revert. E.g.:
   - `remove.rs#execute`: change `&work_plan.target_underlying` ->
     `work_plan.target_mapper.dev_path().as_str()` (or `""`) -> remove test fails.
   - `replace.rs#execute`: swap the old/new device args -> replace test fails
     (swap-sensitivity).
   - `add.rs#execute`: change `disk.by_id.as_str()` to a wrong path -> add test
     fails.
3. Confirm no unrelated test regressed from the gated `RecoverableAddRunner`
   change (the `::new()`-based add tests must stay green).
4. `cargo clippy` / `cargo fmt --check` per repo norms.

## Risks / notes

- Test-only change; no production behavior moves, so no docs/ADR update required
  (decision 024 already states the invariant being pinned).
- The replace test ignores `execute`'s `Result` (the bare runner can't complete
  the replace); that is intentional and matches the existing replace prompt test
  -- the assertion target is `f.confirm.prompts()`, recorded before any failure.
- Device-path constants are load-bearing and fixture-specific: `/dev/vdc`
  (remove disk2 backing), `/dev/test-2` (replace old backing),
  `/dev/disk/by-id/virtio-disk3` (replace new), `/dev/disk/by-id/virtio-disk2`
  (add target). If a fixture's mapping changes, these move with it.
