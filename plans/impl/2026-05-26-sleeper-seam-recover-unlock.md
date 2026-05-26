# Thread the `Sleeper` seam through recover + unlock cleanup/poll paths

Date: 2026-05-26

## Context

A review finding flagged that `RecoverWorkAction::WaitForKernelReplace::execute`
(`cli/src/recover.rs:444`) hardcodes `&progress::RealSleeper` instead of the
injected `RecoverParams.sleeper`. Investigation showed this is one instance of a
wider inconsistency: braid has an established invariant for the `Sleeper` test
seam, and several command-execute/cleanup paths violate it.

**The invariant (already exemplified in the codebase):** `RealSleeper` is
constructed exactly once per command, at the thin command-wiring boundary, and
threaded down as an injected `Sleeper` seam. Every retry/poll helper takes its
sleeper as a parameter; execute/cleanup code never constructs `RealSleeper`
inline; tests drive the implementation with a non-real sleeper. Exemplars:
`cmd_lock` (`lock.rs:1017` injects at `1028` into the seam-threaded
`cmd_lock_impl_with_notes`), `run_device_remove_with_progress` (`progress.rs:279`
injects at `284` into the `_using` seam), and `remove_missing.rs:75` which
documents it ("prod passes `&RealSleeper`; tests pass `&NoopSleeper`").

**Violations this plan fixes:**
- **recover** has the seam (`RecoverParams.sleeper`) but *bypasses* it at 4
  sites (`recover.rs:444`, `1006`, `1036`, `3532`). Purely latent today (no test
  drives them with a non-real sleeper).
- **unlock** has *no* seam at all, yet its cleanup path (`unlock.rs:121`) drives
  the **same shared helper** `mount::close_opened_mappers` that recover does.
  Its test `cmd_unlock_preserves_mount_error_when_cleanup_close_fails`
  (`unlock.rs:519`) models a persistently-busy close, so it burns
  `(CLOSE_RETRY_ATTEMPTS-1) * CLOSE_RETRY_DELAY = 2 * 500ms = ~1s` of **real
  wall-clock time on every `just test-rust` run**.

The clincher for fixing both: `close_opened_mappers` has exactly two production
callers (recover, unlock). Fixing only recover leaves that one helper with a
split contract -- honors injection from recover, ignores it from unlock.
Outcome: zero inline `RealSleeper` in recover/unlock outside the `main.rs`
wiring boundary, matching lock and device-remove.

## Changes

### A. Shared enabler -- relax the helper bound

- `cli/src/mount.rs:700-711` -- change `close_opened_mappers`'s bound from
  `S: Sleeper` to `S: Sleeper + ?Sized`. One token. Safe: every existing caller
  passes a sized `&RealSleeper`/`&NoopSleeper`; relaxing is strictly more
  permissive and lets a `&dyn Sleeper` (what `params.sleeper` is) pass. Its own
  callee `close_mapper_with_retry` (`mapper_close.rs:22`) is already
  `S: Sleeper + ?Sized`, so no further bound changes downstream.

### B. Recover -- stop bypassing the existing seam (4 sites)

1. `recover.rs:444` (`WaitForKernelReplace` arm): `&progress::RealSleeper` ->
   `params.sleeper`. No signature change -- `wait_for_kernel_replace_to_finish`
   (`recover.rs:3274`) already takes `sleeper: &dyn Sleeper`.
2. `recover.rs:1006` and `recover.rs:1036` (`execute_recover_initial_open`
   bootstrap/unlock-failure cleanup): `&RealSleeper` -> `params.sleeper`.
   `params` is in scope (see `params.paths` at `1028`). Relies on (A).
3. `recover.rs:3532` (`relock_and_remount` re-mount-failure cleanup): this
   function (`recover.rs:3378`) takes individual args, not `params`. Add a
   `sleeper: &dyn Sleeper` parameter (place it right after `fs`), pass it to
   `close_opened_mappers` at `3530`, and forward `params.sleeper` from the
   `RemountCycle` arm callsite (`recover.rs:457`). Update **all** callers of
   `relock_and_remount` (grep `relock_and_remount`; known prod caller is the
   arm at `457`; any test caller passes a non-real sleeper).
4. Doc: broaden the `RecoverParams.sleeper` doc comment (`recover.rs:182-184`).
   It currently says only "retrying transiently-busy mapper closes"; it now also
   gates the kernel `dev_replace` status poll and the remount-cycle cleanup.
   Keep to 1-3 lines.
5. Cleanup: after the 4 edits, `recover.rs` has no remaining `RealSleeper` use,
   so drop `RealSleeper` from the `use` at `recover.rs:19` (keep `Sleeper`,
   `self`, `ProgressOutput`).

### C. Unlock -- add the matching seam

1. `unlock.rs:23-35` (`UnlockParams`): add field
   `pub sleeper: &'a dyn progress::Sleeper,` with a `///` doc comment mirroring
   `RecoverParams.sleeper`'s justification (required for a new `pub` item).
2. `unlock.rs:121` (cleanup close): `&RealSleeper` -> `params.sleeper`.
   Relies on (A).
3. Add `sleeper: ...` to **all 15 `UnlockParams` construction sites** (no
   builder exists; these are inline literals):
   - prod: `cli/src/main.rs:709` -> `sleeper: &braid_cli::progress::RealSleeper,`
   - 14 test literals in `unlock.rs` (`336, 409, 483, 573, 706, 740, 798, 871,
     944, 1049, 1153, 1274, 1445, 1549`) -> `sleeper: &progress::NoopSleeper,`,
     except the cleanup test at `573` which uses the recording sleeper (see E2).
4. Cleanup: `unlock.rs:121` was the only `RealSleeper` use in `unlock.rs`, so
   remove the now-unused `use crate::progress::RealSleeper;` at `unlock.rs:8`;
   add a `NoopSleeper` import for the tests.

### D. Test infrastructure (reuse, don't reinvent)

1. Shared recording sleeper: add a `pub(crate)` recording `Sleeper` to
   `cli/src/test_fixtures/` (the cross-module test-double home), modeled on the
   existing `FakeSleeper` (`progress.rs:668-683`) -- records `sleep()` durations
   into `Arc<Mutex<Vec<Duration>>>` with a `.calls()` accessor. Needs a `///`
   doc comment. (Existing `progress.rs::FakeSleeper` and the two test-local
   `RecordingSleeper`s in `lock.rs` are duplicates; migrating them is an
   optional follow-up dedup, not required here.)
2. Recover params builder `.sleeper()` override: in
   `cli/src/test_fixtures/recover.rs`, add a `sleeper: &'a dyn Sleeper` field to
   the builder (default `&progress::NoopSleeper`), a
   `.sleeper(self, &'a dyn Sleeper) -> Self` setter mirroring `.tty()`
   (`recover.rs:104-107`), and make `.build()` (`recover.rs:119`) use
   `self.sleeper` instead of the hardcoded `&progress::NoopSleeper`.

### E. Regression tests (structure-insensitive: count recorded sleeps)

One seam-pinning test per distinct changed *call site*, so reverting *any* fixed
site back to `RealSleeper` fails a test. The five sites map to five tests: the
`WaitForKernelReplace` arm (E1, `recover.rs:444`), unlock's cleanup (E2,
`unlock.rs:121`), `execute_recover_initial_open`'s **general** unlock-failure
cleanup (E3, `recover.rs:1034`), `relock_and_remount`'s cleanup (E4,
`recover.rs:3530`), and `execute_recover_initial_open`'s **bootstrap-add**
cleanup (E5, `recover.rs:1004`). Note `1004` and `1034` are *separate,
mutually-exclusive branches* (bootstrap-add+MountFailed+OpKind::Add vs. the
general fall-through), so each needs its own test -- one does not cover the
other. The mechanic for the four close paths is identical: inject the shared
recording sleeper, force a mapper close to return persistent EBUSY so
`close_mapper_with_retry` runs all `CLOSE_RETRY_ATTEMPTS` attempts, and assert
the recorder saw `CLOSE_RETRY_ATTEMPTS - 1 == 2` sleeps (0 before the fix -> the
assertion fails; 2 after).

1. **Recover -- `WaitForKernelReplace` arm honors the seam.** New test in the
   `recover.rs` test module, modeled on the setup of
   `wait_for_kernel_replace_no_ops_when_just_mounted_false` (`recover.rs:14933`):
   - `state.just_mounted = true`; plan via
     `recover_work_plan_for_journal(replace_journal())` with `open_plan = Some`.
   - `MockRunner::default().with_output_sequence(BtrfsReplaceStatus { mount_point },
     vec![running_raw, finished_raw])` -- Running on poll 1, Finished on poll 2
     (one sleep). Reuse the raw strings from the existing direct-wait tests
     (`recover.rs:3879+`): running `"5.0% done, 0 write errs, 0 uncorr. read errs\n"`,
     finished `"Started on ..., finished on ..., 0 write errs, 0 uncorr. read errs\n"`.
     `MockRunner` is `Sync` (required by the arm's `execute`) and already
     supports `with_output_sequence` (`cmd.rs`).
   - Inject the recording sleeper: `f.recover_params().sleeper(&rec).build()`.
   - Assert `rec.calls().len() == 1`. **Before the (B1) fix this fails** (arm
     uses hardcoded `RealSleeper`, recorder sees 0 -- and incurs one real 200ms
     sleep); after, it sees exactly 1 with no real sleep.
   - Add the 3-section `// Intent / Why it exists / Scenario` preamble matching
     sibling tests in the same module region.
2. **Unlock -- cleanup close honors the seam.** Modify
   `cmd_unlock_preserves_mount_error_when_cleanup_close_fails` (`unlock.rs:519`)
   to inject the recording sleeper via the new `UnlockParams.sleeper` field and
   assert it recorded `CLOSE_RETRY_ATTEMPTS - 1 == 2` sleeps. This both removes
   the ~1s real sleep (the recorder's `sleep()` is a no-op that only records)
   and pins the seam.
3. **Recover -- initial-open *general* cleanup honors the seam** (pins
   `recover.rs:1034` only). `execute_recover_initial_open` has no direct test
   caller, so drive the **full plan**: build a plan whose `open_plan` is `Some`
   (via `recover_work_plan_for_journal`) with a **non-bootstrap** journal (so the
   bootstrap branch at `1004` is skipped and control reaches the general
   `InitialOpenFailure::Unlock` cleanup at `1034`), model a partial open that
   fails with mappers already opened (non-empty `opened_mappers`), and force one
   mapper's `CryptsetupClose` to return persistent EBUSY (`with_output_sequence`).
   Inject the recorder via `f.recover_params().sleeper(&rec).build()` and run
   `plan.execute(...)`; assert `rec.calls().len() == 2`. Template off an existing
   initial-open failure scenario (e.g. the mount-fail/degraded-refusal tests near
   `recover.rs:8763` / `12985`).
4. **Recover -- remount-cycle cleanup honors the seam** (pins `recover.rs:3530`).
   Add a **sibling** to `recover_remount_cycle_mount_failure_closes_reopened_mappers`
   (`recover.rs:12839`) -- do not mutate it, since its "4 closes, all OK"
   assertion would break. The sibling calls `relock_and_remount` directly (as
   `12947` does) passing the recorder as the new `sleeper` arg, sequences one
   reopened mapper's `CryptsetupClose` as `[OK (cycle close), EBUSY, EBUSY,
   EBUSY (cleanup retries)]` via `with_output_sequence`, lets the final mount
   fail, and asserts `rec.calls().len() == 2`.
5. **Recover -- initial-open *bootstrap-add* cleanup honors the seam** (pins
   `recover.rs:1004`). Adapt the existing
   `recover_bootstrap_crash_gives_actionable_instructions` (`recover.rs:13488`),
   which already drives full `cmd_recover` down the bootstrap branch (mount
   fails -> `BtrfsFilesystemShowTarget` reports NoBtrfs -> `all_no_btrfs`
   cleanup at `1004`): (a) change its cleanup `CryptsetupClose { braid-disk1 }`
   output (`recover.rs:13559`) from `ok_raw_empty` to a static EBUSY
   (`err_raw("cryptsetup close", 5, "busy")`) so all `CLOSE_RETRY_ATTEMPTS`
   attempts are busy; (b) inject the recorder via
   `f.recover_params().sleeper(&rec).build()`; (c) add `rec.calls().len() == 2`.
   The cleanup close is best-effort (`let _ = ...`), so the test's existing
   error assertions (bootstrap message, `pending-op.json`, `wipefs`,
   `virtio-disk1`) are unaffected; add a line to its preamble noting the busy
   cleanup close.

## Files touched

- `cli/src/mount.rs` -- bound relaxation (A).
- `cli/src/recover.rs` -- 4 seam edits + `relock_and_remount` signature/callers +
  doc + import cleanup (B), regression tests: arm (E1), general initial-open
  cleanup (E3), remount-cycle cleanup (E4), and an adaptation of the existing
  bootstrap test for the bootstrap-add cleanup branch (E5).
- `cli/src/unlock.rs` -- new seam field + thread + 14 test-literal updates +
  import cleanup (C), cleanup-test update (E2).
- `cli/src/main.rs` -- `UnlockParams { sleeper: &RealSleeper }` at `709` (C3).
- `cli/src/test_fixtures/recover.rs` -- `.sleeper()` builder override (D2).
- `cli/src/test_fixtures/` (mod) -- shared recording sleeper (D1).

## Verification

- `just test-rust` is the primary gate (these are unit-test-level seams).
  - Each seam test (E1-E5) must **fail before** its corresponding fix and pass
    after -- verify by reverting each changed site to `RealSleeper` in turn and
    confirming the matching test goes red: E1->`444`, E2->`unlock.rs:121`,
    E3->`1034`, E4->`3530`, E5->`1004`. Reverting `1004` and `1034`
    independently must each turn a test red (separate branches). This is the
    check that closes the asymmetric-coverage gap.
  - The modified unlock cleanup test passes and the suite is ~1s faster.
- `cargo build` + clippy: confirm no unused-import warnings remain (the removed
  `RealSleeper` imports in `recover.rs:19` and `unlock.rs:8`).
- No parser-critical tool versions change, so no fixture refresh and no VM tests
  are required. Production behavior is unchanged everywhere (all sites still
  receive `RealSleeper` via `main.rs` wiring), so this is a pure test-seam /
  consistency change.

## Out of scope (noted follow-up)

`cli/src/add.rs:396` -- a `&RealSleeper` inside `Drop for LuksCleanupGuard`. It
is the last non-wiring inline `RealSleeper` in the CLI, but it belongs to a
different command, is unrelated to `close_opened_mappers`, and is structurally
harder (a `Drop` impl can't take a parameter -- the guard would have to store a
`&dyn Sleeper`). Fully realizing the invariant CLI-wide eventually addresses it;
it is deliberately excluded here.

## Implementation notes

- The general initial-open cleanup regression test uses `committed_two_disk_add_journal()` instead of a recoverable pool-mutation add journal, because the recoverable add fixture enters live-add replay before the initial mount cleanup branch this test pins.
