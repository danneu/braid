# Name the missing-device remove for what it is

## Context

A review finding flagged `pool_remove_device_using` (`cli/src/pool.rs`) as a dead
near-duplicate of `pool_remove_device`, to be deleted. Verification showed the
opposite: `pool_remove_device` is live (it is the `braid remove` present-disk
path, called from `cli/src/remove.rs`), and the two functions are *not*
duplicates -- they model two distinct operations:

- `pool_remove_device` -- removes a **present** device by **mapper path**
  (`work_plan.target_mapper.dev_path()`), passing `RemoveContext::Live`.
- `pool_remove_device_using` -- removes an **absent** device by **btrfs devid**
  (`work_plan.missing_id`, a `Devid`), passing `RemoveContext::Missing`.

The two contexts drive materially different operator recovery hints
(`cli/src/pool.rs#device_remove_error`), and welding each context to a named
function is a fail-closed property: a call site cannot pass the wrong context.
That guarantee is incomplete today, though: the path-vs-devid boundary is only
*socially* enforced. Both helpers take `device: &str`, and the missing-path
helper degrades its caller's `Devid` to a string
(`work_plan.missing_id.to_string()`) at the boundary -- so a future caller could
hand it a mapper path and still receive `RemoveContext::Missing` hints.

The actual defect is the **name**. Everywhere else in the crate the `_using`
suffix means "the same operation, with injected dependencies" (e.g.
`cli/src/progress.rs#run_device_remove_with_progress` vs
`run_device_remove_with_progress_using`). `pool_remove_device_using` breaks that
contract -- it *also* swaps the context to `Missing` -- so the name falsely
implies it is the deps-injected twin of `pool_remove_device`. That false signal
is exactly what produced the finding. Compounding it, `pool_remove_device_using`
is `pub(crate)` with **no `///`**, a doc-comment convention violation; the
`progress.rs` seam pair is undocumented for the same reason.

Outcome: fix the misleading name at the root and bring the affected
`pub(crate)` items into doc-comment compliance, so a future reader (or
reviewer) cannot make the same mistake. No behavior changes.

## Approach

Rename + document. Do **not** merge the two functions or extract a shared core
-- the path-vs-devid and Live-vs-Missing distinctions are real, and the
per-function context binding is a deliberate fail-safe. Keep `pool_remove_device`
(the common, present-disk case) named as-is; qualify only the special case.

### 1. Rename and type the missing-device remove (`cli/src/pool.rs`)

- Rename `pool_remove_device_using` -> `pool_remove_missing_device`. Drop the
  `_using` suffix: there is no non-`_using` base to pair with (production must
  thread the sleeper from `RemoveMissingParams`, so a hidden-real-deps wrapper
  is impossible), and the `sleeper`/`sink` parameters already advertise the
  injectable seam. Body and `RemoveContext::Missing` are unchanged.
- Tighten the device parameter: `device: &str` -> `missing_devid: Devid`, and
  build the btrfs request string with `missing_devid.to_string()` inside the
  helper. This makes the path-vs-devid boundary type-enforced instead of social
  -- a mapper path can no longer reach `RemoveContext::Missing`. `Devid` is
  already in scope in `pool.rs` (used by `pool_replace_device` /
  `pool_resize_device`), so no new import. The live helper `pool_remove_device`
  keeps `device: &str`: it legitimately takes a mapper path
  (`target_mapper.dev_path()`), and the now-asymmetric signatures encode the
  path-vs-devid distinction in the types.

### 2. Add the missing `///` doc comments

Per the convention (intent/invariant/ownership, not the signature):

- `pool_remove_device` -- state it is the **live/present-disk** remove (by mapper
  path, `RemoveContext::Live`, `braid remove`), and that the **missing**
  counterpart is `pool_remove_missing_device`. Replace today's signature-restating
  line ("Gracefully remove a specific device from the pool with progress.").
- `pool_remove_missing_device` -- state it is the **missing-device cleanup**
  remove (by **devid**, `RemoveContext::Missing`, `braid remove-missing`), and
  that the `sleeper`/`sink` seam exists so the heartbeat + hint tests drive the
  progress loop without real wall-clock sleeps. Make explicit it is **not** a
  deps-injected variant of `pool_remove_device` -- it is a different operation.
- `cli/src/progress.rs#run_device_remove_with_progress` and
  `run_device_remove_with_progress_using` -- the real-deps wrapper delegates to
  the injectable seam with `RealSleeper` + `StderrSink`; the seam takes injected
  `sleeper`/`sink` so tests drive the heartbeat without real sleeps.
- `cli/src/progress.rs` seam types the comments above now lean on -- `Sleeper`
  (`pub` trait), `RealSleeper` (`pub`), `ProgressSink` (`pub(crate)` trait),
  `StderrSink` (`pub(crate)`): add an intent-focused `///` to each (the trait =
  the injectable contract that lets tests substitute fakes; the concrete type =
  the production impl). All four are currently undocumented public-boundary
  items the convention covers. (`NoopSleeper` is `#[cfg(test)]` -- exempt.)

### 3. Update references to the renamed function (all within `cli/src`)

- `cli/src/remove_missing.rs` (`RemoveMissingPlan::execute`) -- the
  `use crate::pool::pool_remove_device_using;` import and the call site, which
  now passes `work_plan.missing_id` (a `Devid`) directly, dropping the
  `.to_string()`.
- `cli/src/pool.rs` tests -- rename the two test fns
  (`pool_remove_device_using_emits_heartbeat`,
  `pool_remove_device_using_failure_emits_missing_replace_hint`) and their call
  sites to the new name, and pass a `Devid` instead of a string device arg: the
  hint test's `"2"` -> `Devid::new(2)`; the heartbeat test's
  `"/dev/mapper/braid-disk2"` -> `Devid::new(2)`. No mock-runner edits are
  needed -- `BlockingRemoveRunner` matches any `BtrfsDeviceRemove`, and the hint
  test's `MockRunner` is keyed on device `"2"`, which `Devid::new(2).to_string()`
  still produces. (Passing a `Devid` also makes the heartbeat test match how the
  missing path is really called, rather than a mapper path it never receives.)
- Code comments mentioning the old name in `cli/src/pool.rs` and
  `cli/src/remove_missing.rs` (the `// Intent:` test preambles and the inline
  rationale comments) -> new name.

## Out of scope

- No merging into one parameterized entry point or a shared `remove_device_impl`
  core (would hide the path-vs-devid distinction, dilute the fail-closed context
  binding, and risk orphaning `run_device_remove_with_progress`).
- No rename of `pool_remove_device` (the bare name correctly belongs to the
  common present-disk case).
- No new ADR or `internals/` page: the `RemoveContext` enum doc already records
  why the two paths need different followups, and ADRs 001/012/019 cover the
  command-level distinction.
- No new live-path heartbeat test, and no signature change to `pool_remove_device`
  (it keeps its mapper-path `&str`; there is no device-path newtype to adopt).
- No file-wide `progress.rs` doc-comment audit: only the device-remove seam
  functions and the four dependency types they thread are documented here;
  unrelated undocumented items (e.g. `ProgressMode`, the `format_*` helpers) are
  left for a separate pass.
- No `README.md` / user-facing docs: the renamed symbol is internal
  (`pub(crate)`); no CLI command or output string changes.

## Verification

- `cargo build` and `cargo clippy --all-targets -- -D warnings` -- compile + lint
  clean (a stale reference to the old name fails the build, since it is
  `pub(crate)` and crate-internal).
- The `Devid` tightening is compile-enforced -- a `&str` mapper path no longer
  type-checks against `pool_remove_missing_device`, so `cargo build` catches any
  miswired call site or test. No new behavioral test is added: the heartbeat,
  Live-vs-Missing hint, and command-level `remove-missing` wiring tests already
  fail if behavior changes.
- `just test-rust` -- the renamed unit tests
  (`pool_remove_missing_device_emits_heartbeat`,
  `pool_remove_missing_device_failure_emits_missing_replace_hint`) and the live
  counterpart (`pool_remove_device_failure_emits_live_balance_hint`) still pass,
  confirming each context still maps to its hint. The `remove_missing`
  command-path tests still pass, confirming the call site rewired correctly.
- `rg 'pool_remove_device_using' cli/` -- returns zero hits (no stale references
  in code, tests, or comments).
- Sanity-read the four touched `///` blocks against
  `docs/dev/doc-comments.md` (Good/Bad catalog): each states why the item exists
  at its boundary, not what the signature says.

## Suggested commit

`refactor(pool): name the missing-device remove for what it is`
(or `docs(pool): ...` if the rename is split out) -- single commit; rename +
`Devid` typing + doc comments are one coherent change with no runtime
behavioral effect.
