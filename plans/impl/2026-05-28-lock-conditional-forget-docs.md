# Plan: fix `braid lock` cookbook step 4 to match execute's conditional forget

## Context

`/verify-issue` confirmed a doc/behavior mismatch in
`docs/commands/lock.md`. Step 4 of "What happens under the hood" presents
`btrfs device scan --forget` as an unconditional flat step between
unmount and mapper close. The real `LockPlan::execute`
(`cli/src/lock.rs:594-654`) only issues that call when *all three* of the
following hold:

1. The unmount succeeded -- the deferred-error branch at
   `cli/src/lock.rs:642-652` skips forget entirely and proceeds straight
   to mapper close.
2. The planned close set is non-empty after filtering through
   `fs.exists` -- `forget_devs.retain(|p| fs.exists(p))` at
   `cli/src/lock.rs:607` drops any planned mapper paths that have
   already disappeared.
3. The resulting list is non-empty -- the gate at `cli/src/lock.rs:608`
   skips the call when there is nothing left to forget.

The forget is also *scoped* -- the planner hands it the explicit
`/dev/mapper/braid-*` paths via `LockCloseSet::forget_paths()`
(`cli/src/lock.rs:146-151`, used at `cli/src/lock.rs:606-607`), where
the list contains both member-owned mappers and orphaned `braid-*`
mappers picked up from prior crashes. The inline comment at
`cli/src/lock.rs:601-605` notes the no-arg form is deliberately avoided
because it would be kernel-global and would invalidate scan entries for
unrelated btrfs filesystems on the host. The cookbook does not currently
convey that scoping either.

`compile_lock_steps` (`cli/src/lock.rs:466-505`) shares the planned
close-set source (`LockCloseSet::forget_paths()`) with execute and
mirrors the close-set-empty gate at line 482, so the dry-run preview
already omits the forget step when nothing is planned. The execute-only
`fs.exists` disappearance filter (gate 2 above) has no preview-side
equivalent -- preview shows the planned step list, while execute drops
paths that vanish between plan and execute. The doc is the only
out-of-sync surface today; the new Test 2 below pins the execute-only
filter so the doc claim does not become unenforced.

Severity is low (no code bug, no incident risk), but an operator
debugging a stuck unmount or stale btrfs scan state would currently
expect forget to have run when it actually didn't. The intended outcome
is that the cookbook narrates the same contract `LockPlan::execute`
implements at runtime.

## Scope

Two files:

- `docs/commands/lock.md` -- the cookbook rewrite (two edits, below).
- `cli/src/lock.rs` -- two new Rust unit tests, one per execute-only
  gate the doc now documents: skip-on-umount-failure and the
  `fs.exists` disappearance filter (see "Test additions" below).

No edits to other docs. Sibling surfaces verified clean:

- `docs/commands/unlock.md:67` describes `btrfs device scan` (no
  `--forget`) -- a different invocation, unrelated.
- `README.md:30-31` lists only `sudo braid lock` with no step
  narration.
- `docs/design/decisions/022-dry-run-preview-model.md:113-119` already
  states preview, forget, and execute share the close set; consistent
  with the fix.

## Edits to `docs/commands/lock.md`

### 1. Rewrite step 4 (line 38)

Before:

```
4. Runs `btrfs device scan --forget` to clear the kernel's device registry (prevents stale references from racing with mapper close)
```

After:

```
4. After a successful unmount, runs `btrfs device scan --forget` for the planned close-set mappers (member-owned plus any orphaned `braid-*` mappers from a prior crash) that still exist on disk, clearing the kernel's device registry so stale references do not race with mapper close. Skipped when there is nothing left to forget.
```

Rationale, point-by-point:

- "After a successful unmount" -- matches the `Ok(())` arm gate at
  `cli/src/lock.rs:595`, and implicitly conveys the skip-on-failure
  behavior without restating it.
- "for the planned close-set mappers ... that still exist on disk" --
  matches all three gates that the previous wording papered over: the
  close set is built by the planner (`LockCloseSet::forget_paths` at
  `cli/src/lock.rs:146-151`), `LockPlan::execute` filters that list
  through `fs.exists` (`cli/src/lock.rs:607`), and only a non-empty
  result is passed to the runner (`cli/src/lock.rs:608`). Avoids the
  earlier "pool's own mappers (members + orphans)" phrasing, which was
  technically loose -- `LockMapperCloseKind::Orphan` is defined as
  "not in pool.json -- likely a prior crash" (`cli/src/lock.rs:247`),
  so orphans are not pool-owned membership.
- "(member-owned plus any orphaned `braid-*` mappers from a prior
  crash)" -- parenthetical reuses braid's existing internal vocabulary
  (`MemberOwned` / `Orphan` from `LockMapperCloseKind`) without
  conflating orphans with pool membership.
- "Skipped when there is nothing left to forget." -- covers both the
  empty-close-set case (`!forget_devs.is_empty()` gate) and the
  disappear-between-plan-and-execute case (the `fs.exists` filter
  yielding an empty list) in one operator-readable phrase. Forecloses
  any reading of "this is the kernel-global no-arg form" by virtue of
  the "for the planned close-set mappers" framing above.

Style notes:

- Uses `--` not `—` per `AGENTS.md` CLI Output Style and
  `CLAUDE.md` writing-style rule.
- Reuses the existing inline-parenthetical pattern from step 3 so the
  bullet style stays consistent across the list.

### 2. Make the deferred-error contract explicit (line 59)

The Error handling bullet currently implies, but does not state, that
forget is skipped when umount fails:

Before:

```
- If unmount fails after 3 retry attempts (e.g. a process has files open on the mount), lock still attempts to close the LUKS mappers and reports the failure
```

After:

```
- If unmount fails after 3 retry attempts (e.g. a process has files open on the mount), lock skips `btrfs device scan --forget` and still attempts to close the LUKS mappers, reporting the failure
```

Rationale: this ties step 4's "after a successful unmount" clause back
to the dedicated Error handling section, so an operator scanning either
section gets the same story. Matches the branch at
`cli/src/lock.rs:642-652` where `umount_error` is set and control falls
through directly to the mapper-close loop without touching
`BtrfsDeviceScanForget`.

## Test additions (`cli/src/lock.rs`)

The doc rewrite now makes two execute-only claims that no existing test
enforces, so each gets a focused Rust unit test. Both tests use
`MockRunner::requests()` (`cli/src/cmd.rs:1477`) and follow the
Test Conventions preamble (Intent / Why it exists / Scenario) per
AGENTS.md.

### Test 1: `lock_umount_failure_skips_forget`

Pins the skip-on-umount-failure contract documented by step 4 / the
Error handling bullet. Without it, existing umount-failure tests would
still pass if a refactor regressed the gate: the forget-error path at
`cli/src/lock.rs:627-637` only emits a warn line and continues, and the
existing tests (`lock_umount_busy_fails` at `cli/src/lock.rs:1890`,
`lock_umount_non_busy_failure_does_not_retry` at `cli/src/lock.rs:2076`)
neither stub `BtrfsDeviceScanForget` nor assert its absence -- a stray
forget would resolve to `CmdError::MissingMock`
(`cli/src/cmd.rs:1454`), trigger the warn path, and leave the
assertions green.

**Shape.** Model on `lock_umount_busy_fails` (`cli/src/lock.rs:1890`).
Preamble:

- *Intent:* a failed unmount must skip the `BtrfsDeviceScanForget`
  request and proceed straight to mapper close.
- *Why it exists:* the lock cookbook documents this contract; without a
  pin, a refactor that always called forget would only surface a runtime
  warn (the forget error path is non-fatal) and existing umount-failure
  tests would stay green.
- *Scenario:* umount fails three times with "target is busy"; lock must
  issue zero `BtrfsDeviceScanForget` requests, still attempt
  `CryptsetupClose` for each member mapper, and return the umount error.

**Assertions.** Filter `runner.requests()` for
`CmdRequest::BtrfsDeviceScanForget` and assert count is zero. Reuse
`cryptsetup_close_request_count` (`cli/src/lock.rs:1260-1266`) to assert
mapper close was still attempted (count = 2 for the canonical aaa/bbb
membership). Assert the returned error contains the umount stderr.

### Test 2: `lock_execute_forget_filters_disappeared_mapper`

Pins the `forget_devs.retain(|p| fs.exists(p))` filter at
`cli/src/lock.rs:607` -- the third gate the doc now documents ("for the
planned close-set mappers ... that still exist on disk"). Without it,
the dry-run helpers `lock_count_forget_steps` and
`lock_forget_step_devices` (`cli/src/test_fixtures/lock.rs:241-262`)
only cover `compile_lock_steps`, which checks
`close_set.forget_paths()` emptiness but does *not* model the
`fs.exists` filter (`cli/src/lock.rs:481`). A regression that dropped
the `retain(...)` call would pass vanished mapper paths to the
runner -- the doc would lie about the contract, and btrfs-progs is not
guaranteed to silently ignore non-existent device paths in a multi-arg
`device scan --forget` call.

**Shape.** Model on the existing mounted-runner tests. Preamble:

- *Intent:* execute drops planned close-set mappers whose
  `/dev/mapper/<name>` path has disappeared between plan and execute
  before issuing `BtrfsDeviceScanForget`, rather than handing the
  runner a path that no longer exists.
- *Why it exists:* the lock cookbook documents this filter; preview-side
  helpers only pin `compile_lock_steps`, which has no such filter, so a
  refactor that dropped `forget_devs.retain(|p| fs.exists(p))` would
  silently break the documented contract while preview tests stayed
  green.
- *Scenario:* plan sees both `braid-aaa` and `braid-bbb` (so the close
  set has two entries), but at execute time `/dev/mapper/braid-bbb`
  has already disappeared; lock must issue exactly one
  `BtrfsDeviceScanForget` request containing only
  `/dev/mapper/braid-aaa`, and the close loop must attempt close only
  for `braid-aaa` (`braid-bbb` is reported "already closed" via the
  member-owned skip at `cli/src/lock.rs:681-688`).

**Setup.** Wrap `MockRunner::default().with_output(CmdRequest::MountpointCheck { path: MountPoint("/mnt/storage".to_owned()) }, lock_ok_raw("mountpoint -q /mnt/storage"))`
in `lock_with_fsid_probe_mocks` so classification still observes both
mappers at plan time -- the same wrap pattern used by every existing
mounted-pool test (`lock_umount_busy_fails` at `cli/src/lock.rs:1890`,
`lock_umount_non_busy_failure_does_not_retry` at
`cli/src/lock.rs:2076`). Without the inner `MountpointCheck` stub, the
first runner call resolves to `CmdError::MissingMock` and the test
fails before reaching the disappearance branch. Then stub a successful
umount and a single forget call with `devices = vec!["/dev/mapper/braid-aaa".into()]`
(not the two-element list `lock_mounted_runner` uses); stub
`CryptsetupClose` only for `braid-aaa`. Seed
`lock_fs(&["/dev/mapper/braid-aaa"])` so `fs.exists` reports
`braid-bbb` absent.

**Assertions.** Filter `runner.requests()` for
`CmdRequest::BtrfsDeviceScanForget` and assert exactly one entry whose
`devices` slice equals `["/dev/mapper/braid-aaa"]`. Filter for
`CmdRequest::CryptsetupClose` and assert one entry for `braid-aaa`,
zero for `braid-bbb`. Lock returns `Ok(())`.

### Helpers

`MockRunner::requests()` plus pattern-match filtering is sufficient,
matching the existing `umount_request_count` /
`cryptsetup_close_request_count` style. A small
`forget_request_count(&runner)` helper next to those two is fine if the
implementor prefers symmetry; not required.

### Why these are in scope

Per AGENTS.md "Audit test coverage on two axes", the plan now makes
two claims about existing execute behavior that no current test
enforces: "After a successful unmount, ..." (Test 1) and "for the
planned close-set mappers ... that still exist on disk" (Test 2). Both
tests are structure-insensitive (they assert request payload
presence/absence and content, not internal helper names) and
behavioral (they assert user-visible contracts). The empty-close-set
case is already pinned by the preview-side helpers
(`lock_count_forget_steps` at `cli/src/test_fixtures/lock.rs:256-262`),
so it is not duplicated here.

## What this plan deliberately does *not* change

- No production code changes. Execute already implements every gate
  the new wording describes; preview shares the planned close-set
  source and the close-set-empty omission with execute but does not
  model the execute-only `fs.exists` disappearance filter (that
  branch's coverage is what Test 2 adds). Only the doc and the test
  pins are drifting.
- No edits to design ADRs or internals. The cookbook is the only
  end-user surface that describes the step ordering; the ADR coverage
  at `docs/design/decisions/022-dry-run-preview-model.md:113-119` is
  already accurate.
- No README change. README's lock section is just a copy-paste example,
  not narrated steps.
- No third test for the empty-close-set branch. That branch is already
  covered by the preview-side helpers `lock_count_forget_steps` and
  `lock_forget_step_devices` (`cli/src/test_fixtures/lock.rs:241-262`),
  and the execute-side branch reads the same
  `LockCloseSet::forget_paths()` source -- duplicating the assertion on
  the execute side would be structural overlap rather than new
  behavioral coverage. The `fs.exists` filter is *not* in that
  category: it exists only in execute and is now covered by Test 2.

## Verification

1. **Render check.** `mdbook build docs` succeeds (per AGENTS.md, broken
   cross-links would fail CI). No new links introduced.
2. **Cross-read.** Reread the updated step 4 against
   `cli/src/lock.rs:594-654` (execute) and `cli/src/lock.rs:466-505`
   (`compile_lock_steps`) and confirm every clause the doc claims
   ("after successful unmount", "planned close-set mappers that still
   exist on disk", "skipped when there is nothing left to forget")
   maps to a real gate in the code (`Ok(())` arm at line 595,
   `fs.exists` retain at line 607, `!is_empty()` gate at line 608).
3. **Operator-perspective read.** Read the section top to bottom and
   confirm the unmount-failed scenario is now consistent between step
   4 and the Error handling bullet -- no operator could read one and
   form an expectation the other contradicts.
4. **Test pins prove themselves.** Run `just test-rust` and confirm
   both `lock_umount_failure_skips_forget` and
   `lock_execute_forget_filters_disappeared_mapper` pass. Then
   sanity-check each pin is load-bearing -- one gate per test, so a
   regression in either gate is caught by the corresponding test:
   - Test 1: temporarily hoist the forget call out of the `Ok(())` arm
     at `cli/src/lock.rs:594-654` so forget runs even on umount
     failure; confirm `lock_umount_failure_skips_forget` fails. Revert.
   - Test 2: temporarily replace `forget_devs.retain(|p| fs.exists(p))`
     at `cli/src/lock.rs:607` with a no-op (e.g. drop the line so
     `forget_devs` keeps the disappeared path); confirm
     `lock_execute_forget_filters_disappeared_mapper` fails. Revert.

No VM test or fixture refresh is required; the code contract under
test is execute-local and already exercised by `MockRunner`.
