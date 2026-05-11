# Unify best-effort LUKS-close call sites on `close_mapper_with_retry`

## Context

`pool::evict_present_device` (`cli/src/pool.rs:589`; close block at
`pool.rs:636-668`) does a single-shot `cryptsetup close` after
`btrfs device remove` returns and emits one `[warn]` row on any
non-zero exit. On a transient EBUSY (udev / blkid / stale btrfs worker
still holding the mapper for a brief settle window), this turns a
recoverable race into a permanently leaked `/dev/mapper/braid-*` that
the operator must close by hand at the end of a multi-hour
`braid remove`. The retry helper `close_mapper_with_retry` in
`cli/src/mapper_close.rs:21-65` already handles exactly this case (3
attempts, 500 ms backoff, exit-5-only retry classifier) and is used by
`lock.rs:157`, `mount.rs:727`, and `add.rs:223` -- but never propagated
to `pool`/`replace`/`recover`.

The same single-shot best-effort-warn pattern is duplicated nearly
verbatim in two siblings, so the same EBUSY race exists in:

- `cli/src/replace.rs:702-749` -- live-replace post-maintenance close of
  the old mapper.
- `cli/src/recover.rs:2769-2806` (`close_old_mapper_best_effort`) -- the
  recover.rs counterpart of replace's close.

`docs/principles.md:75-101` already accepts both "single attempt" and
"busy-retry loop" for `cryptsetup close`; the `[wait]` row contract is
what matters and is preserved either way. There is no documented design
intent to keep the three sites single-shot -- the gap is historical.

Outcome: a shared helper that wraps `close_mapper_with_retry` with the
`[wait] disk X: locking...` -> `[ok]` / `[warn]` UI contract, used by
all three sites. The flaky-EBUSY-leaked-mapper class of bug is dissolved
across the project, and three near-identical 30-line blocks collapse
into one.

## Change summary

1. Add `pub(crate) fn close_mapper_best_effort` to
   `cli/src/mapper_close.rs`. It emits `[wait]`, calls
   `close_mapper_with_retry`, emits `[ok]` or `[warn]`, and returns
   `bool` indicating whether the close succeeded so callers can print
   their post-success trailer (replace/recover only).
2. Use the existing **Params-struct seam** to inject `Sleeper`,
   mirroring the in-place `sleep_inhibitor: &'a dyn
   AcquireSleepInhibitor` field on `RemoveParams` (`remove.rs:70`),
   `ReplaceParams` (`replace.rs:60`), and `RecoverParams`
   (`recover.rs:174`). Add `sleeper: &'a dyn progress::Sleeper` to
   all three Params structs. `main.rs` sets it to `&RealSleeper` at
   the three command entrypoints; the in-test fixture builders
   (`f.replace_params()` etc.) default it to `&NoopSleeper`
   (`progress.rs:24-27`). This avoids signature ripple at the 13+
   `cmd_replace(...)` test call sites and the comparable sets at
   `cmd_remove`/`cmd_recover`: the builder default propagates
   automatically.

   The three close-path helpers themselves still take a generic
   `sleeper: &S` arg with the bound `S: Sleeper + ?Sized` (same
   idiom already used at `progress.rs:296`) so they accept both the
   trait-object sleeper from `params.sleeper` and concrete sleepers
   from direct tests. Pool/recover tests call them directly without
   going through the cmd-level entrypoint:
   - `evict_present_device` (`pool.rs:589`) -- current signature is
     `(runner, mapper, mount_point, needs_balance, progress)` (no
     `fs` arg). Prod caller `remove.rs:334` passes `params.sleeper`;
     the single remaining direct pool test caller, inside
     `evict_present_device_close_failure_emits_warn_row` at
     `pool.rs:1811` (call site at `pool.rs:1819`), passes
     `&NoopSleeper`. The former guard-path tests have been moved
     into `validate_pool_topology` and no longer exercise
     `evict_present_device` directly -- leave them alone.
   - `close_old_mapper_best_effort` (`recover.rs:2769`) and its
     parent `execute_replace_post_maintenance_recovery`
     (`recover.rs:2814`) -- prod callers `recover.rs:627` and
     `recover.rs:2733` pass `params.sleeper`; three test callers
     (`recover.rs:8737`, `8805`, `8860`) pass `&NoopSleeper`
     directly.
   - `ReplacePlan::execute` (the close block at
     `replace.rs:702-749` lives inside it) reads `params.sleeper`
     directly -- no signature change needed.
3. Replace the three single-shot blocks with calls to the helper.
   Replace's and recover's `Ok` branch keeps the existing
   `eprintln!("Old device closed. If repurposing the physical disk,
   wipe it separately.")` trailer, gated on the helper's `bool` return.
4. Update the two existing terminal-warn tests
   (`pool.rs:1811` `evict_present_device_close_failure_emits_warn_row`
   and `replace.rs:2832`
   `live_replace_old_close_failure_emits_warn_row`) to:
   - Use `&NoopSleeper` (pool test adds it explicitly to the
     `evict_present_device(...)` call; replace test inherits it from
     the fixture builder's default).
   - Switch the mocked close exit from 5 to 4 (ENODEV, non-busy) so
     the test stays focused on the immediate-fail-on-non-busy
     contract without coincidentally also exercising retry. The
     replace test mocks via the
     `ReplacementPool::two_disk_healthy().install(...).with_handler`
     fixture; change the `CmdRequest::CryptsetupClose { mapper }
     if mapper == "braid-disk2"` arm there to return exit 4 with an
     ENODEV-style stderr.
   - Update the asserted warn text to match what the helper emits on
     `CloseMapperError::Failed`.
5. Add three new behavioral wiring tests, one per changed call site,
   that assert the call site actually goes through the retry helper:
   - Stateful runner / handler: first `CryptsetupClose` for the
     target mapper returns exit 5 (EBUSY), second returns exit 0.
     For replace this slots into the existing `with_handler` fixture
     pattern at `replace.rs:2832`; for pool it lives next to
     `EvictRunner` in `pool.rs`; for recover it slots into the
     existing recover-replace test fixture style.
   - Use `&NoopSleeper` (direct calls pass it explicitly; the
     replace wiring test inherits it from the fixture default).
   - Assert two `CryptsetupClose` requests were issued for the
     target mapper.
   - Assert the captured stderr contains the terminal
     `[ok] disk {label}: locked` row (proving the success path was
     taken after retry).
   These tests fail if any of the three call sites is later
   regressed back to a single-shot close.
6. Add one helper-level unit test in `mapper_close.rs` (using
   `&NoopSleeper`) covering the four paths of
   `close_mapper_best_effort`: success, EBUSY-then-success,
   persistent-EBUSY, and non-busy-fail. The test asserts on
   `CryptsetupClose` request count (proving retry happened) and
   the final returned `bool`, not on sleep duration.

## Critical files

- `cli/src/mapper_close.rs` -- add `close_mapper_best_effort` plus its
  unit test. Re-use `CLOSE_RETRY_ATTEMPTS`, `CLOSE_RETRY_DELAY`, and
  `CloseMapperError`. Also relax `close_mapper_with_retry`'s bound at
  `mapper_close.rs:21` from `S: Sleeper` to `S: Sleeper + ?Sized` so
  it accepts the trait-object sleeper flowing in from
  `close_mapper_best_effort`. Existing callers (lock/mount/add) pass
  concrete sized sleepers and are unaffected by the relaxation.
- `cli/src/progress.rs:24-27` -- no change; existing `NoopSleeper` is
  used as the test-fixture default.
- `cli/src/remove.rs:60` (`RemoveParams<'a>`) -- add
  `pub sleeper: &'a dyn progress::Sleeper`.
- `cli/src/replace.rs:44` (`ReplaceParams<'a>`) -- same field.
- `cli/src/recover.rs:163` (`RecoverParams<'a>`) -- same field.
- `cli/src/main.rs` -- set `sleeper: &RealSleeper` at each of the
  three Params constructions (the existing `sleep_inhibitor:
  &RealSleepInhibitor` lines are the model).
- Replace-test fixture builder (the `f.replace_params()...build()`
  path used by the 13 `cmd_replace(...)` test callers) and its
  remove/recover analogues -- default `sleeper: &NoopSleeper`.
- `cli/src/pool.rs:589` (`evict_present_device`) -- current sig is
  `(runner, mapper, mount_point, needs_balance, progress)`. Add
  `S: Sleeper + ?Sized` generic + `sleeper: &S` arg (slot it right
  after `runner` to match the convention in `mount.rs::cleanup` /
  `lock.rs::execute`). Replace the inline close+status block at
  `pool.rs:636-668` (close_label derivation + Wait/CryptsetupClose +
  Ok/Err match arms) with one helper call. Keep the `close_label`
  derivation as the helper's `disk_label`.
- `cli/src/remove.rs:334` -- prod call site for
  `evict_present_device`; pass `params.sleeper`.
- `cli/src/replace.rs:702-749` -- `ReplacePlan::execute` reads
  `params.sleeper` and passes it to the helper; replace the inline
  block; on `true` print the existing `eprintln!("Old device
  closed. ...")` trailer.
- `cli/src/recover.rs:2769` (`close_old_mapper_best_effort`) and
  `recover.rs:2814` (`execute_replace_post_maintenance_recovery`) --
  thread Sleeper through both with the `S: Sleeper + ?Sized` bound;
  replace `close_old_mapper_best_effort`'s body with a call to the
  shared helper. Keep the `fs.exists` guard (early return so the
  helper isn't invoked for an already-absent mapper). Update the
  two prod callers (`recover.rs:627`, `recover.rs:2733`) to pass
  `params.sleeper`, and the three test callers
  (`recover.rs:8737`, `8805`, `8860`) to pass `&NoopSleeper`.
- `cli/src/pool.rs` tests -- update existing
  `evict_present_device_close_failure_emits_warn_row` at
  `pool.rs:1811` (call site at `pool.rs:1819`): exit 4 +
  `&NoopSleeper`. This is the only direct `evict_present_device`
  test in pool.rs now; the former target-missing /
  null-underlying guard tests live under `validate_pool_topology`
  (`pool.rs:1959-2109`) and are unaffected. Add the new
  `evict_present_device_retries_on_busy_then_succeeds` wiring test
  next to the existing one, extending `EvictRunner` (at
  `pool.rs:1749`) with a per-call counter (e.g. `AtomicU32`) so
  the first `CryptsetupClose` returns exit 5 and the second
  returns exit 0.
- `cli/src/replace.rs:2832` -- update existing
  `live_replace_old_close_failure_emits_warn_row` (exit 4 via the
  `with_handler` arm; fixture's `&NoopSleeper` default applies). Add
  a new `live_replace_old_retries_on_busy_then_succeeds` wiring test
  in the same module using an `AtomicU32`-backed handler arm to
  flip exit 5 -> exit 0 across the two close attempts.
- `cli/src/recover.rs` -- there is no existing close-failure test
  for `close_old_mapper_best_effort`; add a fresh
  `recover_replace_old_close_retries_on_busy_then_succeeds` wiring
  test alongside the existing recover-replace test fixtures at
  `recover.rs:8737+`.
- `cli/src/replace.rs:3982`, `4040`, `4097` -- three direct
  `ReplaceParams { ... }` test literals (not produced via the fixture
  builder); each needs `sleeper: &progress::NoopSleeper` added.
- `cli/src/main.rs:379` (`RemoveParams { ... }`), `main.rs:431`
  (`ReplaceParams { ... }` -- already mentioned), and `main.rs:776`
  (`RecoverParams { ... }`) -- direct struct literals at the cmd
  entrypoints; each needs `sleeper: &RealSleeper` added. (Type-
  annotation sites like `&RemoveParams<'_>` are unaffected -- adding
  a field doesn't break a typed-reference signature.)

## Helper contract

```rust
/// Best-effort close of a LUKS mapper with retry, emitting the
/// principle-13 [wait] -> [ok|warn] status-row pair on the same
/// subject. Returns true iff the close succeeded; callers that print
/// a post-success trailer (e.g. replace/recover's "Old device
/// closed...") gate it on the return value.
pub(crate) fn close_mapper_best_effort<R, S>(
    runner: &R,
    sleeper: &S,
    mapper: &str,
    disk_label: &str,
    color_enabled: bool,
) -> bool
where
    R: CommandRunner,
    S: Sleeper + ?Sized;
```

`S: Sleeper + ?Sized` is required (not just `S: Sleeper`) so the
helper accepts the `&dyn progress::Sleeper` stored on the Params
structs. The codebase already uses this idiom for the same reason
on `run_device_remove_with_progress_using` (`progress.rs:296`).
Existing concrete-Sleeper callers (`&RealSleeper`, `&NoopSleeper`,
`&FakeSleeper`) continue to satisfy the relaxed bound.

The helper internally:
1. Emits `[wait] disk {disk_label}: locking...`.
2. Calls `close_mapper_with_retry(runner, sleeper, mapper, color_enabled)`.
   The retry helper already prints its own
   `[warn] cryptsetup close X busy, retrying (n/3)...` between
   attempts -- those don't close the wait row (different subject), the
   terminal row below does.
3. On `Ok(())`: emits `[ok] disk {disk_label}: locked`, returns `true`.
4. On `Err(e)`: emits `[warn] disk {disk_label}: lock failed ({e})`,
   returns `false`. The error display already covers the
   `Cmd`/`Failed`/`DeviceBusy` variants.

No trailer is baked in; replace/recover keep their `eprintln!("Old
device closed. If repurposing the physical disk, wipe it separately.")`
at the call site.

## Verification

- `just test-rust` -- runs the helper unit test, the two updated
  terminal-warn tests, and the three new retry-wiring tests. All must
  pass.
- `just test-repro repro-cryptsetup-close-mounted` -- the existing
  ENODEV-on-already-closed-mapper repro; pinned in
  `mapper_close.rs:46-47` and must not regress.
- `just test-vm braid-remove-disk braid-remove-disk-busy
  replace-live-disk replace-live-disk-busy
  recover-replace-completed` -- the actual VM check names that
  exercise the three changed call sites end-to-end. Confirm
  `[wait]` -> `[ok]` rows on success and unchanged operator-visible
  output when nothing is busy.
- Manual smoke (informational, not required): in a test VM with three
  pool members, run `braid remove disk2` and confirm
  `[wait] disk disk2: locking...` -> `[ok] disk disk2: locked` and
  that no `/dev/mapper/braid-disk2` remains afterward.

## Out of scope

- `lock.rs::CloseMapperCtx::close_one` (orphan-mapper "(orphan)" suffix
  + umount-error context) and `add.rs::LuksCleanupGuard::drop`
  ("(cleanup)" suffix) are intentionally distinct UI variants and stay
  separate.
- `recover.rs:3088` (the recover-remount-cycle close) is fail-hard, not
  best-effort warn, and is a different contract; not touched.
- Public-shape note: `RemoveParams`, `ReplaceParams`, and
  `RecoverParams` each gain one new field (`sleeper: &'a dyn
  progress::Sleeper`), mirroring the existing `sleep_inhibitor` seam
  on all three. `cmd_remove`/`cmd_replace`/`cmd_recover` signatures
  do NOT change -- so the ~13 `cmd_replace(...)` test callers and the
  comparable sets at `cmd_remove`/`cmd_recover` need no per-test
  edits; just the fixture builder's default-field setting and
  `main.rs`'s three Params constructions. Direct helper callers
  (`pool.rs:1819` -- the lone remaining direct
  `evict_present_device` test call; `recover.rs:8737`/`8805`/`8860`) do
  add `&NoopSleeper` to the call line because those tests bypass the
  cmd entrypoint.
