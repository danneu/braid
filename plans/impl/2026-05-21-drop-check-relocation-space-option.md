# Plan: drop vestigial `Option<u64>` from `check_relocation_space`

## Context

`check_relocation_space` in `cli/src/remove_missing.rs:541-581` takes
`missing_id: Option<u64>`, but since commit `7c32641c` ("require
--missing-id for remove-missing") the sole production callsite at
`cli/src/remove_missing.rs:466` always passes
`Some(params.missing_id)`. The `None`-arm of the filter on line 570
(`missing_id.is_none() || missing_id == Some(d.devid)`) -- which means
"count all missing devices" -- is unreachable from production code.

The prior cleanup punted this cleanup deliberately:
`plans/wip/bubbly-toasting-cerf.md:25` notes the callsite was wrapped in
`Some(missing_id)` because "callee still takes `Option`". This plan
finishes that cleanup.

Goal: collapse the helper to a `u64` parameter so the function signature
matches its actual usage and the filter expresses the single real
contract -- "find the targeted missing device's allocations."

## Changes

### `cli/src/remove_missing.rs`

1. **Signature** (line 544): `missing_id: Option<u64>` -> `missing_id: u64`.
2. **Filter** (line 570): simplify to
   `.filter(|d| d.device_size == 0 && d.devid == missing_id)`.
   Drop the comment fragment "(optionally filtered by devid)" on line
   566 -- the filter is no longer optional.
3. **Production callsite** (line 466): unwrap the `Some(...)`:
   `check_relocation_space(runner, config.mount_point(), params.missing_id)`.

### Tests in `cli/src/remove_missing.rs`

Each test below owns an inline `device_usage_stdout` fixture; no shared
helpers to update. The fixture's single missing devid is the value to
pass.

| Test | Line | Current call | New call |
|------|------|--------------|----------|
| `check_relocation_space_rejects_insufficient_space` | 978 | `..., None` | `..., 3` (fixture's missing devid) |
| `check_relocation_space_passes_sufficient_space` | 1021 | `..., None` | `..., 3` (fixture's missing devid) |
| `check_relocation_space_with_missing_id_filters` | 1069, 1073 | `..., Some(2)` / `..., Some(3)` | `..., 2` / `..., 3` (unwrap the `Some`; fixture and assertions unchanged) |
| `check_relocation_space_proceeds_on_command_error` | 1098 | `..., None` | `..., 3` (devid is arbitrary -- runner errors before the filter runs) |

Behavior is preserved: for the three `None`-arm tests, each fixture
has exactly one missing devid, so the targeted filter matches the same
device the `None`-arm previously matched by "all missing." The
`with_missing_id_filters` test's `Some(_)` calls already used the
targeted-filter path; only the `Some` wrapper is removed.

Test names describe behaviors (insufficient, sufficient, command-error),
not parameters -- no renames needed.

## Out of scope

- The doc comment on lines 533-536 already describes the "targeted
  missing device" contract correctly. Leave it.
- `RelocationCheck` enum and the `ProceedWithWarning` path are
  unaffected.
- No callers outside `cli/src/remove_missing.rs` -- helper is
  module-private (`fn`, not `pub(crate)`). No cross-file edits.

## Verification

1. `just test-rust` -- the four `check_relocation_space_*` unit tests
   must still pass. The other `remove_missing` tests are unaffected (they
   exercise `cmd_remove_missing`, not the helper directly).
2. `cargo build -p braid-cli` -- the type-system change is the real
   guarantee; if the signature and callsite are out of sync the build
   fails.

No VM tests needed -- this is an internal refactor with identical
runtime behavior.
