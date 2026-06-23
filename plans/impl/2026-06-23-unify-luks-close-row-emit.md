# Unify LUKS close-row emission under the ADR 024 label-provenance invariant

## Context

ADR 024 (`docs/design/decisions/024-luks-uuid-identity.md`, **Active**) makes a
hard invariant: *every user-facing disk row is labeled with an operator
`DiskName`, never a mapper basename*. Its enforcement mechanism is the typed
`disk_label: &DiskName` parameter on `mapper_close::close_mapper_best_effort`
"so no caller can pass a mapper-derived label." Commit `fb23e72c` ("type close
mapper labels as disk names") brought `remove`/`replace`/`recover` under that
rule; commits `324fee31` ("label add cleanup rows from disk names") and
`1963e84a` ("carry disk names through mount cleanup") have since closed the
`add.rs` and `mount.rs` label-provenance gaps. The typed `DiskName` now reaches
every close-row site -- what remains is the **simplicity** half: each still
hand-rolls the wait/ok(/warn) row sequence instead of sharing one core.

The work spans three close-row sites that hand-roll the `disk <label>:
locking...` / `locked` wait/ok(/warn) sequence around a LUKS mapper close
instead of using the shared helper:

1. `cli/src/add.rs` -- `LuksCleanupGuard::drop` (the cited finding). Commit
   `324fee31` closed the **label-provenance** half: the guard carries the
   operator `DiskName` (`TrackedMapper { name, mapper }`), `Drop` labels its
   `disk <name>: ... (cleanup)` rows from `tracked.name` (not `strip_prefix`),
   the drift + non-drift guard tests are green, and the `add` paragraph of ADR
   024 is updated. What remains is the **simplicity** half: `Drop` still
   hand-rolls the wait/ok/warn match around `close_mapper_with_retry` rather than
   routing through the shared helper. The remaining add work is pure dedup.
2. `cli/src/mount.rs` -- `close_opened_mappers`. The label-provenance half landed
   in commit `1963e84a`: `UnlockAndMountFailure.opened_mappers` is now
   `Vec<TrackedMapper>`, the unlock path records `TrackedMapper { name, mapper }`
   on `OpenOutcome::Opened`, the `strip_prefix("braid-")` derivation is gone, and
   `cleanup_row_uses_disk_name_under_mapper_drift` pins the helper-level
   invariant (green). What remains is the **simplicity** half: the loop still
   hand-rolls the wait/ok/fail match around `close_mapper_with_retry` instead of
   routing through the shared core.
3. `cli/src/recover.rs` -- `relock_and_remount`'s remount-cycle close loop
   (`recover.rs#relock_and_remount`, step 3 of the cycle). Its label is
   **already** typed (`close_names: &[DiskName]`), so there is no ADR 024
   *violation* here -- this is a goal-completeness + robustness gap, not a label
   bug. It emits the byte-identical `disk {name}: locking...` / `locked` rows
   but calls `CryptsetupClose` directly instead of going through
   `close_mapper_with_retry`, so unlike every other close path it hard-fails the
   recovery on the first transient EBUSY busy-close. Folding it through the
   shared core both removes the duplicated row wording and gains the busy-retry
   it currently lacks. (`recover` also has a separate post-resume best-effort
   close at `recover.rs:3122` that already uses the typed helper; that one only
   needs the `CloseContext::Normal` argument.)

`lock.rs` (`CloseMapperCtx::close_one`) already does this correctly and is
**deliberately out of scope**: its orphan rows legitimately have no `DiskName`
(orphans are not pool members, so the `&str`/basename label is the documented
carve-out), and its umount-stuck two-phase failure policy is unique. Folding it
onto a `&DiskName`-typed core would either weaken the type guarantee or be
impossible.

**Outcome:** close both ADR 024 gaps by construction, collapse the duplicated
wait/ok/retry row wrapper from all three sites into one `&DiskName`-typed core
(giving recover's remount cycle the busy-retry it currently lacks), and leave
`lock.rs` as the documented exception.

## Design

Introduce a single shared row-emit core in `cli/src/mapper_close.rs` that owns
the `disk <label>: locking<suffix>...` / `locked<suffix>` wording and the
busy-retry close, and returns the error to the caller on failure so each caller
keeps its own failure-row severity/wording:

```rust
/// Single source of truth for both the wait/ok suffix AND the failure wording
/// of a disk-status close row -- one encoding, so a caller cannot pass an
/// arbitrary suffix the enum was meant to constrain.
pub(crate) enum CloseContext { Normal, Cleanup }
// Normal  -> wait/ok suffix "",           failure "lock failed ({e})"
// Cleanup -> wait/ok suffix " (cleanup)",  failure "lock failed (cleanup, {e})"

/// Emit the `disk <label>: locking<suffix>...` wait row, close the mapper with
/// busy-retry, and on success emit `disk <label>: locked<suffix>` (suffix
/// derived from `context`). On failure returns the error WITHOUT a closing row;
/// the caller owns failure severity.
pub(crate) fn emit_close_progress<R, S>(
    runner: &R, sleeper: &S, mapper: &MapperName,
    disk_label: &DiskName, context: CloseContext, color_enabled: bool,
) -> Result<(), CloseMapperError>;
```

- `close_mapper_best_effort` gains a `context: CloseContext` param, wraps
  `emit_close_progress(.., context, ..)`, and on `Err` emits its `[warn]` row
  using the **same** context's failure wording, returning `bool` as today. This
  is the only helper the warn-and-continue callers (remove/replace + recover's
  post-resume close) need.
- The add guard routes through `close_mapper_best_effort(.., CloseContext::Cleanup, ..)`.
- `mount::close_opened_mappers` wraps `emit_close_progress(.., CloseContext::Normal, ..)`
  directly, keeping its fatal `[fail]` row + `first_error` accumulation policy.
- `recover::relock_and_remount`'s remount-cycle close loop wraps
  `emit_close_progress(.., CloseContext::Normal, ..)` directly and `?`-maps
  `CloseMapperError` -> `RecoverError`, preserving its hard-abort policy (no
  closing row on failure) while gaining busy-retry. `sleeper` is already in
  `RelockAndRemountCtx` and `close_names` is already `&[DiskName]`, so the only
  behavior change is the added retry.
- `lock.rs` is untouched (the type guarantee requires `&DiskName`; orphans are `&str`).

The seam is clean because the *only* shared part across these wrappers is
wait + close-with-retry + ok; the failure handling already differs at every site
(warn-and-continue, fatal `[fail]` + `first_error` accumulate, hard-abort with no
row), so the core never emits a failure row.

## Changes by file

**`cli/src/mapper_close.rs`** -- add the `CloseContext` enum (single source of
the wait/ok suffix + failure wording) and the `emit_close_progress` core (takes
`CloseContext`, derives the suffix internally); rewrite `close_mapper_best_effort`
to take `CloseContext`, wrap the core, and derive its `[warn]` failure wording
from the same enum.

**`cli/src/add.rs`** -- the typed-`DiskName` work landed in commit `324fee31`:
`TrackedMapper { name, mapper }`, `track(name, mapper)`, both `cmd_add` call
sites (`add.rs:1186`/`1403`), every guard test call site, and a `Drop` that
already labels from `tracked.name`. The **only** remaining change is to collapse
`Drop`'s hand-rolled wait/ok/warn match into the shared helper -- one call per
tracked mapper, in the existing `.rev()` loop:
`close_mapper_best_effort(self.runner, &RealSleeper, &tracked.mapper,
&tracked.name, CloseContext::Cleanup, color_enabled)` (the returned bool is
ignored in `Drop`). Then drop the now-unused
`use crate::mapper_close::close_mapper_with_retry;` (line 17) and import
`close_mapper_best_effort` + `CloseContext`. The `(cleanup)` wording is
byte-identical under `CloseContext::Cleanup`, so every guard test (drift and
non-drift) stays green -- this is a pure refactor.

**`cli/src/mount.rs`** -- the typed-pair work landed in commit `1963e84a`
(`opened_mappers: Vec<TrackedMapper>`, populated on `OpenOutcome::Opened`,
`strip_prefix` removed, all construction/consumer sites flowed). The **only**
remaining change is in `close_opened_mappers`: replace the hand-rolled
wait/ok/fail match around `close_mapper_with_retry` with
`emit_close_progress(.., &tracked.name, CloseContext::Normal, ..)`; on `Err`
keep the existing `[fail]` row + `first_error` accumulation, and keep both
cleanup trailers verbatim. The btrfs `--forget` block and the `TrackedMapper`
loop are unchanged.

**`cli/src/remove.rs`, `cli/src/replace.rs`, `cli/src/recover.rs`** -- add
`CloseContext::Normal` to each command's single post-commit/post-resume
best-effort `close_mapper_best_effort` call (`remove.rs:458`, `replace.rs:898`,
`recover.rs:3122`). No output change.

**`cli/src/recover.rs` (`relock_and_remount`)** -- additionally fold the
remount-cycle close loop (`recover.rs#relock_and_remount`, step 3) through the
shared core. Keep the `fs.exists(&mapper_path)` skip guard, then replace the
hand-rolled wait-row / direct `CryptsetupClose` / ok-row block with
`emit_close_progress(runner, sleeper, &mn, name, CloseContext::Normal,
color_enabled).map_err(|e| RecoverError::Failed(...))?` -- `name: &DiskName` is
the loop binding, `mn = config::mapper_name(name)` is the existing mapper. This
swaps the raw `eprint!(status_line(...))` rows for `emit_status` (byte-identical
in production, and now test-capturable) and the direct close for the busy-retry
close. The only behavior change is gaining busy-retry; failure stays hard-abort
(no closing row, `RecoverError`). No remount-cycle test pins the failure string,
so the `map_err` wording is implementer's discretion.

**`docs/design/decisions/024-luks-uuid-identity.md`** -- the `add` cleanup-guard
paragraph + `guard_cleanup_row_uses_disk_name_under_mapper_drift` citation
(commit `324fee31`) and the `mount.rs` `close_opened_mappers` paragraph +
`cleanup_row_uses_disk_name_under_mapper_drift` citation (commit `1963e84a`) are
already in place. Remaining ADR edits:
- Implementation-notes list: add `recover.rs`'s `relock_and_remount` remount
  cycle to the paths that carry the typed `DiskName` close row (its label was
  already typed; the note records that it now also routes through the shared
  busy-retry core).
- "Tests That Enforce This" ledger: add a citation for the new remount-cycle
  busy-retry regression test.

`lock.rs`'s orphan `&str` label remains the documented carve-out alongside the
busy-retry command-echo.

## Tests (TDD: the add guard + mount drift tests are already green; write the remount busy-retry test before its impl)

- **`add.rs` guard tests already exist and are GREEN -- preserve them.**
  `guard_cleanup_row_uses_disk_name_under_mapper_drift` (tracks `disk("disk2")` +
  `braid-WRONG`; asserts `disk disk2: locking (cleanup)...`, `!contains("WRONG")`,
  and close targets `braid-WRONG`) already passes because `Drop` labels from
  `tracked.name`. The dedup routing must keep it green; do **not** re-author it.
  The close-target assertion (`*closed == ["braid-WRONG"]`) guards that device
  ops keep using the mapper, not the label.
- **The other `add.rs` guard tests already pass a `DiskName` to `track`** and
  stay green (the `(cleanup)` wording is unchanged by the routing). Keep
  `guard_retries_busy_close_before_success` -- ADR 024 cites it as the pin for
  the busy-retry command-echo carve-out, which survives (best_effort still calls
  `close_mapper_with_retry`).
- **`mount.rs` drift unit test already exists and is GREEN -- preserve it.**
  `cleanup_row_uses_disk_name_under_mapper_drift` (commit `1963e84a`) calls
  `close_opened_mappers` directly with a synthetic divergent
  `TrackedMapper { name: "disk1", mapper: "braid-WRONG" }`, the mock `Filesystem`
  reporting `/dev/mapper/braid-WRONG` present (so the forget fires), and asserts
  the row reads `disk disk1: locking...` (label from the `DiskName`), the
  `CryptsetupClose` targets `braid-WRONG`, and the `BtrfsDeviceScanForget`
  devices contain `/dev/mapper/braid-WRONG` and **never** `/dev/mapper/braid-disk1`.
  Routing the loop through the shared core must keep it green
  (`CloseContext::Normal` wait/ok wording is byte-identical); do **not**
  re-author it.
- **New `recover.rs` remount-cycle busy-retry test** -- drive
  `relock_and_remount`'s step-3 close with a busy-then-success sequence (exit 5
  -> exit 0) for one planned mapper and assert the cycle completes (the close is
  retried, not hard-aborted), pinning the busy-retry the fold adds. The existing
  remount-cycle close tests feed step-3 only exit-0 closes
  (`recover_remount_cycle_honors_close_names_over_membership`,
  `recover_remount_cycle_skips_disappeared_planned_mapper`,
  `recover_remount_cycle_mount_failure_closes_reopened_mappers`), so the fold
  breaks none of them. `recover_remount_cycle_mount_failure_cleanup_honors_injected_sleeper`
  (`recover.rs:14163`) *does* inject `[ok, busy, busy, busy]` for `braid-disk1`,
  but step-3 closes `braid-disk1` first and takes the leading exit-0 output
  (success, no retry); the three busy outputs are consumed downstream by the
  post-mount-failure reopened-mapper cleanup (`close_opened_mappers`, already
  retry-wrapped). After the fold, step-3 still consumes exactly that one leading
  output, so this test -- the strongest existing guard that the fold preserves
  step-3's output-consumption order -- stays green.
- **Update `mapper_close.rs` tests** -- thread `CloseContext::Normal` through the
  `run_best_effort` helper; expected `[wait]/[ok]/[warn]` strings unchanged.
- **`mount.rs` tests that construct/consume `UnlockAndMountFailure`** already use
  `TrackedMapper` pairs (commit `1963e84a`); routing the close loop through the
  shared core leaves their non-drift output strings unchanged.

## Verification

1. `just test-rust` -- all CLI unit tests, including the new remount-cycle
   busy-retry test, the preserved-green mount drift test, and the updated
   guard/mapper_close/mount suites.
2. `just check-output-ascii` -- ASCII guard over `cli/src/**/*.rs` echo lines
   (touched wait/ok/warn/fail rows).
3. `just docs-build` -- mdbook + linkcheck over the edited ADR 024.
4. NixOS VM lane (slow, aarch64-darwin via linux-builder): `tests/cli/braid-unlock.py`
   exercises the real unlock-cleanup close path (non-drift -- production never
   feeds a divergent pair here); confirm its `disk <name>: locking...`/`locked`
   rows still render with the typed label. Note: `tests/cli/luks-mapper-drift.py`
   covers `braid lock`, which this change does not touch, so it is **not** a
   targeted check here -- the drift invariant for `close_opened_mappers` is
   pinned by the existing unit test above, not the VM lane.

## Out of scope (deliberate)

- `cli/src/lock.rs` `CloseMapperCtx::close_one` -- orphan labels are legitimately
  `&str` (no `DiskName` exists) and its umount-stuck failure policy is unique;
  unifying it would weaken the core's type guarantee. It stays as the reference
  precedent and documented carve-out.
- `cli/src/config.rs#name_from_mapper` / `lock.rs#orphan_disk_label` -- sanctioned
  display-only basename parsers for the no-DiskName cases; not violations.

## Implementation notes

- `CloseContext` derives `Copy` so the by-value `context` parameter can be
  passed to `emit_close_progress` and then reused in `close_mapper_best_effort`'s
  warn-row wording without a clone or borrow. The "single source of the wait/ok
  suffix + failure wording" is realized as two private methods on the enum --
  `row_suffix()` and `failure_detail(&CloseMapperError)` -- rather than inline
  match arms at each call site.
- The new `recover_remount_cycle_retries_busy_step3_close` test asserts the
  `braid-disk1` close was issued exactly twice (busy then success) in addition to
  the cycle completing, making the "retry happened, not hard-abort" signal
  explicit rather than inferred from completion alone.
