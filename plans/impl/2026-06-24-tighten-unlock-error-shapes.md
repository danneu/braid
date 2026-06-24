# Plan: tighten `UnlockError` to its two real shapes

## Context

A simplicity finding flagged `UnlockError::Failed(String)` in `cli/src/unlock.rs`
as a near-dead, redundant message-passthrough variant and proposed dropping it.
Verification inverted the finding: `Failed` is **live** (its sole producer is the
pending-operation preflight), and the genuinely **dead** variant is
`Membership(#[from] membership::MembershipError)` -- it is never constructed and
no `?`-on-`MembershipError` exists to fire its derived `From` (the only post-mount
membership calls are `enrich_from_pool_state`, which is infallible, and
`save_membership`, which is best-effort `if let Err`, not `?`-propagated). The
compiler cannot see this: thiserror's derived `From` impl counts as a construction
site, so there is no dead-code warning -- a human has to remove it.

The finding's real grievance -- "a reader must trace call sites to learn `Failed`
is exclusively the pending-op string" -- is a naming problem, and braid already
solved it elsewhere: `discover.rs#DiscoverWriteError` names the identical
`check_no_pending_operation` passthrough `PendingOperation(String)`, doc-commented
to [ADR 017](../../docs/design/decisions/017-runtime-disk-membership.md) as
refusing pending operations "identically to add/remove/replace." The governing
rule is *rename single-producer preflight passthroughs only*: unlock is the last
command whose **sole-producer** preflight message still rides a generically-named
variant, while `discover` already names its single-producer wrapper
`PendingOperation`. Commands whose generic arm has many producers (enroll's
`Validation`; mount/recover/lock's `Failed`) keep the generic name correctly --
see Out of scope.

Outcome: `UnlockError` ends with exactly the two shapes it actually produces --
`Mount` (typed delegation) and `PendingOperation` (preflight passthrough) -- each
self-documenting, plus a regression test for the preflight branch that currently
has none.

## Scope decision: unlock-only

The dead-`Membership` pattern does **not** recur. Among the four command error
enums carrying `#[from] membership::MembershipError`, only unlock's is dead:

- `add.rs#AddError::Membership` -- **live** (`save_membership(...)?` in `execute`).
- `recover.rs#RecoverError::Membership` -- **live** (many `save_membership(...)?`).
- `replace.rs#ReplaceError::Membership` -- **live**: `derive_replace_target_membership(...)?`
  (`replace.rs#cmd_replace` execute path) and `e.into()` (the dry-run gate in
  `plan_replace`) both fire the `#[from]` on a duplicate-UUID `insert`, pinned by
  the test asserting `Err(ReplaceError::Membership(MembershipError::Conflict(_)))`.

So no cross-command unification applies; touching the live siblings would be
wrong. Likewise leave the generic `Failed(String)` arms in `mount.rs#MountError`,
`recover.rs#RecoverError`, and `lock.rs#LockError` alone -- those are real
multi-producer catch-alls, not single-producer preflight passthroughs.

## Changes (all in `cli/src/unlock.rs`)

### 1. Remove the dead `Membership` variant

Delete the variant and its `#[from]` from `unlock.rs#UnlockError`. Safe: no
non-test code constructs it and no `?` depends on the conversion, so removal
compiles. The `use crate::membership::{self, PoolMembership}` import stays --
`enrich_from_pool_state` / `save_membership` / `PoolMembership` are still used.

### 2. Rename `Failed(String)` -> `PendingOperation(String)`

Match `discover.rs#DiscoverWriteError::PendingOperation` and braid's cause-named
variant convention (`DegradedRefused`, `DuplicateDevid`, `Conflict`). Update the
sole producer in `unlock.rs#plan_unlock`:

```rust
if let Err(msg) = preflight::check_no_pending_operation(params.paths) {
    return Err(PlanFailure::empty(UnlockError::PendingOperation(msg)));
}
```

`main.rs` matches only `UnlockError::Mount(MountError::DegradedRefused(_))` (exit
2) and funnels everything else to a generic exit 1, so the rename cannot affect
dispatch. No non-test code references `UnlockError::Failed`.

### 3. Add the missing doc comments (AGENTS.md convention)

`UnlockError` is a `pub` item with no `///`. Add an enum-level comment plus a
variant comment on `PendingOperation`, mirroring the `discover.rs` style:

```rust
/// Errors surfaced by `braid unlock` planning and execution. Two shapes only:
/// `Mount` delegates every probe/open/mount/credential failure to its typed
/// `MountError` source (whose `DegradedRefused` arm `main.rs` matches to set
/// exit code 2); `PendingOperation` carries the canonical recovery-mode message
/// from `preflight::check_no_pending_operation`, shared verbatim with
/// add/remove/replace/discover (ADR 017) so unlock refuses an interrupted
/// operation in the same words.
#[derive(Debug, thiserror::Error)]
pub enum UnlockError {
    #[error("{0}")]
    Mount(#[from] MountError),
    /// Pending-operation journal is present or unreadable; message forwarded
    /// from `preflight::check_no_pending_operation`. Mirrors
    /// `discover::DiscoverWriteError::PendingOperation`.
    #[error("{0}")]
    PendingOperation(String),
}
```

The `#[error("{0}")]` passthrough is unchanged, so the user-facing string still
comes verbatim from `preflight` and stays ASCII.

### 4. Backfill the preflight regression test

The sole producer of `PendingOperation` has no unlock-level coverage. Add a test
to the `unlock.rs` test module mirroring `preflight.rs#pending_op_refuses_when_present`
(journal-seeding recipe) and `unlock.rs#plan_unlock_preserves_notes_on_degraded_refused`
(params/assert shape):

```rust
// Intent: plan_unlock refuses at the preflight gate when a pending-op journal
//   exists, surfacing UnlockError::PendingOperation before any probe runs.
// Why it exists: check_no_pending_operation is the sole producer of
//   PendingOperation and had no unlock-level test; a regression that dropped it
//   or reordered it past mount::plan_open_pool would unlock against possibly
//   inconsistent membership and start emitting probe notes before the refusal.
// Scenario: an add was interrupted, leaving pending-op.json on disk; the
//   operator runs `braid unlock` and must be routed to `braid recover` instead.
#[test]
fn plan_unlock_refuses_when_pending_operation_present() {
    let (_state_dir, sp) = isolated_paths();
    let config = test_config();
    let membership = two_disk_membership();
    let fs = unlock_storage_fs(&[
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]);
    // Seed a pending-op journal so the gate fires before any probe.
    let journal = crate::journal::build_journal(
        crate::membership::PoolMembership::empty(),
        crate::membership::PoolMembership::empty(),
        crate::journal::OpKind::Add {
            phase: crate::journal::AddPhase::PoolMutation,
            targets: crate::membership::LuksUuidMap::new(),
        },
    );
    crate::journal::write_journal(&sp, &journal).expect("seed pending-op.json");

    let runner = MockRunner::default(); // no expectations: refusal precedes commands
    let params = UnlockParams {
        config: &config,
        membership: &membership,
        paths: &sp,
        passphrase_stdin: false,
        passphrase_file: None,
        key_file: None,
        allow_degraded: false,
        dry_run: true,
        sleeper: &crate::progress::NoopSleeper,
        backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
    };

    let failure = match plan_unlock(&runner, &fs, &params) {
        Ok(_) => panic!("pending operation must refuse plan_unlock"),
        Err(failure) => failure,
    };
    assert!(
        failure.notes.is_empty(),
        "preflight refusal precedes probe, so notes must be empty, got: {:?}",
        failure.notes,
    );
    match &failure.error {
        UnlockError::PendingOperation(msg) => assert!(
            msg.contains("interrupted operation detected"),
            "expected canonical preflight message, got: {msg}",
        ),
        other => panic!("expected PendingOperation, got: {other:?}"),
    }
    assert!(
        runner.requests().is_empty(),
        "refusal must precede any subprocess, got: {:?}",
        runner.requests(),
    );
}
```

This is behavioral and structure-insensitive: it pins the gate's outcome (refuse,
no notes, no subprocess, canonical message), not the variant's internal shape.

## Out of scope (deliberately)

By the *rename single-producer preflight passthroughs only* rule, the following
correctly stay as they are:

- `enroll_key_file.rs#plan_enroll` forwards the **same** `check_no_pending_operation`
  message -- `PlanFailure::empty(EnrollKeyFileError::Validation(msg))`, structurally
  identical to unlock's old producer -- but `Validation` is a ~20-producer catch-all
  (occupied slot, wrong passphrase, bad keyfile path, target already exists, ...),
  not a single-producer wrapper, so it keeps its generic name. (Enroll already has
  preflight-branch regression coverage; step 4 closes the same gap for unlock.)
- The generic `Failed(String)` arms in `mount`/`recover`/`lock` -- multi-producer
  catch-alls, and not even preflight-fed (`recover` is the journal-clearing path;
  `lock` stays available during recovery, per `preflight.rs#check_no_pending_operation`).
- The live `Membership` variants in `add`/`recover`/`replace` -- removing them
  would break compilation and the replace duplicate-UUID test.
- No behavior change: the preflight message, exit codes, and stderr ordering are
  all untouched.

## Verification

1. `cargo build -p braid-cli` -- confirms the `Membership` removal compiles (no
   `?` depended on the dropped `#[from]`).
2. `cargo clippy -p braid-cli --all-targets` -- confirms no newly-unused
   `membership` import or other warnings.
3. `just test-rust` (or `cargo test -p braid-cli plan_unlock_refuses_when_pending_operation_present`)
   -- the new test passes; the existing unlock suite (`passphrase_mismatch_names_failing_disk`,
   `plan_unlock_preserves_notes_on_degraded_refused`, etc.) still passes,
   confirming the rename touched no other path.
4. `python3 scripts/docs/check-output-ascii.py` -- error strings stay ASCII
   (unchanged; sourced from `preflight`).
5. `git grep -n 'UnlockError::Failed\|UnlockError::Membership' -- cli/src tests`
   returns nothing -- both old names are fully gone from source and tests.

## Implementation notes

- Scoped the old-name grep verification to `cli/src` and `tests` because
  historical implemented plans under `plans/impl/` intentionally preserve old
  design snippets that still mention `UnlockError::Failed` and
  `UnlockError::Membership`.
