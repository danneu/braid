# Plan: reclassify kernel-canceled dev_replace from `[fail]` to `[warn]` in recover

## Context

In `cli/src/recover.rs#wait_for_kernel_replace_to_finish`, the `ReplaceState::Cancelled`
arm emits a `StatusTag::Fail` row (`"pool: kernel dev_replace canceled"`) and then
`return Ok(())`. It is the only place in the recover progress stream where `[fail]`
precedes a successful continuation: recover goes on to run the remount cycle,
`finish_uncommitted_replace_recovery` cleans up, prints `replace did not complete --
pool still has the pre-replace topology. Re-run braid replace to retry.`, and the
journal is cleared normally. An operator (or a support reader of a captured log) sees a
`[fail]` line on an otherwise-successful recovery and reasonably concludes recovery
failed.

The behavior itself is correct (kernel-cancel reverts topology; the uncommitted-replace
branch handles it). This is a messaging defect, and it contradicts the function's own
documented contract:

- The function's docstring (written in `ed096408`, *after* `b5515552` added the
  `Cancelled` arm) states the binary plainly: runner errors "emit `[warn]` and proceed";
  `Suspended` and parser errors "emit `[fail]` and return `RecoverError::Failed`." The
  docstring **never mentions `Cancelled`**, and the live `[fail]` + `Ok` behavior
  directly violates the `[fail]` <-> `Err` mapping it codifies.
- `docs/design/principles.md` §13 ("Announce long-running work") -- the authoritative
  status-row contract -- describes the `[warn]` wait-closer as the one used "on a
  non-fatal best-effort failure ... the command continues despite the failure," and
  cites *this function* as the example. Kernel-cancel (topology reverted, recover
  proceeds to clean up) fits that `[warn]` case exactly.
- The same function's runner-error arm already pairs `[warn]` + `Ok` with the body
  `"pool: kernel dev_replace status check failed -- proceeding"`. `Cancelled` is the
  lone "proceed" path wearing `[fail]`.

**Outcome:** flip the `Cancelled` arm to `[warn]` so `[fail]` stays reserved for hard
stops that return `Err`, making the code honor its own docstring and §13. This also
sharpens the canceled-vs-suspended distinction the original commit (`b5515552
fix(recover): preserve journal on suspended dev_replace`) was built to draw -- the two
states no longer share a tag. Control flow is unchanged; only the status row moves.

## Changes

Changes #1-#3 are in `cli/src/recover.rs`; #4 is a one-line append to the frozen
split-replace plan.

### 1. `wait_for_kernel_replace_to_finish` -- `ReplaceState::Cancelled` arm

- `StatusTag::Fail` -> `StatusTag::Warn`.
- Row body `"pool: kernel dev_replace canceled"` ->
  `"pool: kernel dev_replace canceled -- proceeding"` -- reuses the exact `-- proceeding`
  token from the sibling runner-error arm in this same function (`recover.rs`, same
  output stream: the strongest possible precedent). The only other verbatim `-- proceeding`
  is `main.rs`'s `EMPTY_MEMBERSHIP_WARN_SUFFIX`. (braid's broader warn-and-proceed idiom
  elsewhere -- `remove.rs`, `preflight.rs`, `remove_missing.rs`, `replace.rs` -- renders
  as `; proceeding anyway` / `; proceeding as if ...`, a different token, so it is not
  cited as precedent for the punctuation.) `--` is the existing ASCII em-dash substitute.
- Keep `return Ok(())` -- behavior is identical; only the row changes.

### 2. Docstring of `wait_for_kernel_replace_to_finish`

The second paragraph currently splits behavior into runner-Err (`[warn]` + proceed) and
`Suspended`/parser-Err (`[fail]` + `Err`), and omits `Cancelled` entirely. Reframe so
the binary is complete and accurate:

- **warn + proceed** = { a `runner.run` `Err` -- the subprocess never produced an exit
  code (signal-kill, spawn/IO failure: transient races, ENOMEM, signals per
  `output_to_raw`, `cmd.rs`), so kernel state is unreadable ; `Cancelled` (kernel
  reverted topology to pre-replace -- recoverable; downstream
  `finish_uncommitted_replace_recovery` handles cleanup) }.
- **fail + `Err`** = { `Suspended` (kernel still treats the replace as ongoing -- a hard
  stop, journal preserved) ; **any** `parse_btrfs_replace_status` error -- both a
  non-zero `btrfs replace status` exit (`ParseError::CommandFailed`) and unrecognised
  zero-exit stdout (`ParseError::InvalidText`) }.

Note the seam so the binary stays accurate: a non-zero exit is **not** a `runner.run`
`Err`. `output_to_raw` returns `Ok(RawCommandOutput { exit_status, .. })` for any process
that exits with a code (`cmd.rs`), so a non-zero `btrfs` exit reaches the parser as
`Ok(raw)` and is classified there as `CommandFailed` -- landing in the fail bucket, not
the runner-`Err` warn bucket. Only a signal-kill (no exit code) surfaces as `runner.run`
`Err`.

State the resulting invariant explicitly: in this stream `[fail]` always pairs with
`return RecoverError::Failed`.

### 3. The two canceled unit tests

- `wait_for_kernel_replace_emits_fail_on_canceled_returns_ok` -> rename to
  `wait_for_kernel_replace_emits_warn_on_canceled_returns_ok`. Assertion
  `"[fail] pool: kernel dev_replace canceled"` ->
  `"[warn] pool: kernel dev_replace canceled -- proceeding"`. In the `// Intent` /
  `// Why it exists` preamble, change "reports a fail row" -> "reports a warn row"; keep
  the topology-rollback + downstream-cleanup rationale (still accurate).
- `wait_for_kernel_replace_emits_fail_on_canceled_first_poll` -> rename to
  `wait_for_kernel_replace_emits_warn_on_canceled_first_poll`. Same assertion flip. Drop
  the "canceled is terminal and diagnostic" framing from the preamble; keep the point
  the test actually pins: canceled is surfaced **unconditionally**, even on the first
  poll with no prior `[wait]` row, unlike the silent `Finished`/`NotStarted` fast-path
  (which only emits `[ok]` when a wait was already announced).

### 4. Reconcile the frozen split-replace plan (append-only)

`plans/impl/2026-05-06-split-replace-state-cancelled-suspended.md` specifies the canceled
row as `[fail]` (its behavior table + test list, ~rows 76/113/115); this change makes
those stale. The project reconciles such drift rather than leaving it: precedent
`944ab33b docs(plan): reconcile balance-soft plan with the gate removal` appended a
"Follow-up (`<commit>`): ..." note to an already-landed plan -- its commit message frames
the goal as killing "a stale pointer a later reader or repo grep would otherwise trust."
Do the same here -- append ONE follow-up note at the end of that doc; leave the historical
rows in place (no in-place rewrite):

> Follow-up (`<impl commit>`): the `Cancelled` arm was reclassified from `[fail]` to
> `[warn] pool: kernel dev_replace canceled -- proceeding` (still `return Ok(())`),
> restoring the `[fail]` <-> `Err` invariant the function docstring states and matching
> `principles.md` §13's `[warn]`-closer example. The two canceled tests were renamed
> `..._emits_warn_on_canceled_returns_ok` / `..._emits_warn_on_canceled_first_poll`. The
> `[fail]` references above are superseded; see `plans/impl/<this-plan>.md`.

Fill the hash at commit time (or cite this plan's promoted filename). This matches the
`944ab33b` reconcile precedent, kept to a single forward pointer. (AGENTS.md's
docs-in-sync rule is scoped to README + the `docs/guides/`/`docs/commands/` mdBook, not
`plans/impl/`, so it is not the authority here -- the `944ab33b` precedent is.)

## Explicitly NOT changed (considered and skipped)

- **`docs/design/principles.md` §13** -- its `[warn]` example (the status-poll error)
  stays valid, and the principle is descriptive, not exhaustive, so the canceled case
  need not be enumerated. No staleness, no edit.
- **`cli/src/progress.rs`** -- `ReplaceState::Cancelled` there sits in a display-only
  `continue` arm (the foreground `braid replace` poller), not a status-tag emit.
  Untouched.
- **Call site (`RecoverWorkAction::WaitForKernelReplace`, uses `?`)** -- `Cancelled`
  keeps returning `Ok(())`, so propagation is unaffected.
- **No structural helper** coupling `[fail]` + `Err` -- only two `[fail]` sites remain
  (parser-Err, `Suspended`); a 2-site helper is less readable than the docstring-stated
  invariant plus the tests. Simpler wins.

## Verification

- `just test-rust` -- the two renamed tests pass with the new `[warn] ... -- proceeding`
  assertions; the neighboring `wait_for_kernel_replace_emits_warn_on_status_error_after_wait`
  and `..._emits_fail_on_suspended_returns_err` tests stay green (confirms only the
  canceled arm moved, and `Suspended` still emits `[fail]` + `Err`).
- `python3 scripts/docs/check-output-ascii.py` (the output-ASCII enforcer over
  `cli/src/**/*.rs`) -- confirms the new row text is ASCII-clean.
- `rg "dev_replace canceled" cli/ tests/ docs/ plans/` -- after the change, every `cli/`
  hit reads `[warn] ... canceled -- proceeding`; the split-replace plan keeps its
  historical `[fail]` rows (by design) now carrying the reconciling follow-up note (#4).
- Optional manual check: run a recover whose kernel replace is observed `CANCELED` (or
  inspect a captured log) and confirm the stream shows `[warn] ... canceled --
  proceeding` mid-run, recover completes, and the downstream `replace did not complete
  ... Re-run braid replace to retry.` line still prints.
