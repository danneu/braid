# Fix: `braid lock --dry-run` must preview the lenient teardown on stdout

## Context

`braid lock --dry-run` previews the shutdown teardown a real `braid lock` would
perform. Today the preview and the real run disagree on a corrupt/missing
pool.json, and the disagreement has two layers:

- **Real lock** (`run_plain_lock`, `run_systemd_stop_lock`) loads membership
  with the lenient `load_membership_for_lock` (`cli/src/main.rs:1104`): on any
  load failure it warns and proceeds with **empty** membership, then closes
  every observed `braid-*` mapper after per-candidate `cryptsetup luksUUID`
  verification. This is the documented safe teardown
  (decision 018 step 146; `docs/design/decisions/018-systemd-lifecycle.md:146`).
- **Dry-run** (`Commands::Lock` arm, `cli/src/main.rs:760-769`) loads membership
  with the strict `load_membership_or_exit` (`cli/src/main.rs:1090`), which
  prints an error and `exit(1)` on the same input.

Both feed the *same* planner -- `cmd_lock` -> `plan_lock` (`cli/src/lock.rs:1094`)
-- so membership is the only divergent input. On exactly the prior-crash /
recovery host that `lock` exists to clean up, `braid lock --dry-run` aborts with
exit 1 while the subsequent real `braid lock` succeeds. That violates decision
022's contract that dry-run preview and execution share one planning path
(`docs/design/decisions/022-dry-run-preview-model.md:37-39,87`).

**The stream layer (the part a naive loader swap gets wrong).** Even after the
dry-run path adopts `load_membership_for_lock`, that loader emits its warning
via `eprintln!` (`cli/src/main.rs:1111-1130`). A dry-run would then split its
output across two streams: the load-reason warning on **stderr**, the rest of
the preview on **stdout**. That breaks the output contract in
`README.md:121` ("A successful dry-run prints one complete preview to stdout --
warnings that qualify the preview are part of it") and decision 022's
"The structured dry-run preview lives on stdout. Preview notes are part of that
stdout preview" (`022:67-68`). The membership-load reason is exactly such a
qualifying warning: empty membership is why the plan falls back to scanning
every observed mapper.

**Root cause:** commit `8db7277 fix(lock): tolerate missing pool membership`
converted `run_plain_lock` and `run_systemd_stop_lock` to the lenient loader but
never touched the inline dry-run arm; and the lenient loader was written to
*print* its diagnostic rather than *return* it, so it could only ever reach
stderr. The dry-run path needs that diagnostic on stdout, as a preview note.

**Outcome:** `braid lock --dry-run` previews the real teardown (warning +
unmount + mapper closes) as one complete stdout stream on corrupt/missing/
conflicting pool.json, byte-compatible with the real run's stderr rendering of
the same warning.

## Design: route the load diagnostic through the existing preview-note pipeline

`cli/src/preview.rs` already owns the byte-compatible dual-stream renderer that
decision 022 mandates:

- `Preview::render_with` renders `PreviewNote::Warn(body)` as `[warn] <body>` on
  **stdout** (via `LockPlan::preview().print_colored()`).
- `emit_notes_to_stderr` / `render_notes_for_stderr_with` render the *same*
  `PreviewNote::Warn(body)` identically on **stderr**.

Every other lock warning (orphan, skip, fallback, scan-failure) is already a
`PreviewNote::Warn` flowing through this pipeline (`cli/src/lock.rs:276,320,
806,908,943`). The membership-load warning is the lone exception that bypasses
it via `eprintln!`. The fix is to make it a first-class `PreviewNote::Warn` too:
the loader **returns** it, dispatch routes it to the right stream, and both
streams use the shared renderer -- so wording stays byte-compatible for free.

## Fix

### 1. `load_membership_for_lock` returns the diagnostic instead of printing it

In `cli/src/main.rs`, change the signature to
`fn load_membership_for_lock(paths: &StatePaths) -> (PoolMembership, Option<PreviewNote>)`
(import `braid_cli::preview::PreviewNote`). Keep the four per-variant message
bodies, but **drop the literal `warn: ` prefix** (the renderer supplies the
`[warn]` tag) and return them instead of printing:

```rust
match braid_cli::membership::load_membership(paths) {
    Ok(membership) => (membership, None),
    Err(e) => {
        let body = match e {
            MembershipError::Io { path, source } => format!(
                "pool.json unreadable at {}: {source}{EMPTY_MEMBERSHIP_WARN_SUFFIX}",
                path.display(),
            ),
            MembershipError::Corrupt { .. } => /* ... */,
            MembershipError::Conflict(msg) => /* ... */,
            MembershipError::DuplicateDevid { .. } => /* ... */,
            MembershipError::Save { .. } => unreachable!("load_membership cannot return Save"),
        };
        (PoolMembership::empty(), Some(PreviewNote::Warn(body)))
    }
}
```

Update the doc comment to say it *returns* the diagnostic so each caller routes
it to the correct stream (stderr for real runs, the stdout preview for dry-run).

### 2. Extract `run_dry_run_lock`; route the note into the preview

Add a helper beside `run_plain_lock` (1139) and `run_systemd_stop_lock` (1176).
It takes only `config_path` + `paths` -- dry-run deliberately acquires **no**
pool lock or stop-coordinator (decision 022; pinned by
`tests/module/pool-lock-dry-run-bypass.py`), so the thinner signature is correct
and self-documenting. It passes the load note to `cmd_lock` as extra preview
notes:

```rust
/// Dry-run lock preview. Surfaces the membership-load diagnostic as a stdout
/// preview note (not stderr) so the preview is one complete stream and matches
/// the teardown a real lock performs on corrupt/missing pool.json (decision 022).
fn run_dry_run_lock(config_path: &Path, paths: &StatePaths) {
    let config = load_config_or_exit(config_path, 1);
    let (membership, load_note) = load_membership_for_lock(paths);
    let runner = RealRunner;
    let fs = RealFilesystem;
    let extra_notes: Vec<PreviewNote> = load_note.into_iter().collect();
    if let Err(e) = braid_cli::lock::cmd_lock(&runner, &fs, &config, &membership, true, extra_notes) {
        print_cli_error(&e.to_string());
        std::process::exit(1);
    }
}
```

### 3. Real lock paths render the diagnostic to stderr via the shared renderer

In `run_plain_lock` and `run_systemd_stop_lock`, destructure the loader and emit
the note to stderr **before** entering `cmd_lock`/orchestrate (preserving the
current early ordering), then pass empty extra notes downstream:

```rust
let (membership, load_note) = load_membership_for_lock(paths);
if let Some(note) = &load_note {
    braid_cli::preview::emit_notes_to_stderr(std::slice::from_ref(note), PerDiskStyle::Bracketed);
}
```

Real paths pass **empty** extra notes to `cmd_lock` so `execute()` does not
re-emit the warning (no double-print, ordering unchanged). The note is a
`Warn` (not `PerDisk`), so the `PerDiskStyle` argument is irrelevant to its
rendering.

### 4. `cmd_lock` accepts extra preview notes (thin-wrapper preserves test arity)

In `cli/src/lock.rs`, thread an `extra_notes: Vec<PreviewNote>` through `cmd_lock`
into a new private `cmd_lock_impl_with_notes`, and make the existing
`cmd_lock_impl` a thin wrapper passing `Vec::new()`. This keeps all **34**
`cmd_lock_impl(...)` test call sites unchanged.

```rust
pub fn cmd_lock<R, F>(/* ..., */ dry_run: bool, extra_notes: Vec<PreviewNote>) -> Result<(), LockError> {
    cmd_lock_impl_with_notes(runner, fs, &RealSleeper, config, membership, dry_run, extra_notes)
}

// Preserves the test-facing arity (34 call sites) while production routes
// dispatch-supplied notes (the membership-load diagnostic) into the preview.
fn cmd_lock_impl<R, F, S>(/* unchanged params */) -> Result<(), LockError> {
    cmd_lock_impl_with_notes(runner, fs, sleeper, config, membership, dry_run, Vec::new())
}

fn cmd_lock_impl_with_notes<R, F, S>(/* ..., */ extra_notes: Vec<PreviewNote>) -> Result<(), LockError> {
    if !dry_run { /* run_lock_pre_steps ... */ }
    let mut plan = plan_lock(runner, fs, config, membership)?;
    plan.notes.splice(0..0, extra_notes); // load-reason precedes planner notes
    if dry_run {
        plan.preview().print_colored();
        return Ok(());
    }
    plan.execute(runner, fs, sleeper)
}
```

Update the two other `cmd_lock` call sites to pass `Vec::new()`: the orchestrate
closure (`lock.rs:1049`) and `run_systemd_stop_lock` (`main.rs:1225`).

### 5. Leave `load_membership_or_exit` in place

It still has two correct callers -- `Commands::Unlock` (`main.rs:697`) and
`Commands::EnrollKeyFile` (`main.rs:736`) -- where pool.json *is* authoritative.
Do not remove or change it.

### Wording note (intended, safe)

The real-run stderr warning changes from `warn: pool.json unreadable ...` to
`[warn] pool.json unreadable ...`. This is deliberate: it makes the real-run
stderr byte-compatible with the dry-run stdout note (decision 022) and matches
every other lock warning's `[warn]` rendering. No test pins the `warn:` prefix
-- the only assertions are substring `"pool.json unreadable"`
(`lock-tolerates-missing-pool-json.py:35,53`), which still pass.

## Test

Extend `tests/module/lock-tolerates-missing-pool-json.py` (no flake.nix change --
the `.nix` reads the `.py` dynamically; `flake.nix:791-795`). The test already
unlocks a real pool (mappers open) and moves pool.json aside;
`load_membership` returns `Err(Io)` on a missing file -- exactly the input that
used to abort dry-run -- so the existing "missing" scaffold triggers the
regression with no new setup.

Insert a **non-destructive** subtest *between* the existing
"Unlock pool and remove pool.json" subtest and the "Plain braid lock closes
mappers..." subtest. Dry-run closes nothing, so it leaves the pool in the exact
state the next subtest expects. Capture stdout and stderr **separately** so the
test proves the stream split, not just presence:

```python
# Intent:
#   `braid lock --dry-run` previews the teardown without pool.json as one
#   complete stdout stream -- the load-reason warning and the teardown steps
#   both land on stdout -- exiting 0 and leaving the pool untouched.
# Why it exists:
#   Regression guard. Dry-run used the strict loader and exited 1 on
#   missing/corrupt pool.json; the lenient loader also printed its warning to
#   stderr, which would split the preview across streams. Both must match the
#   real run's stdout preview (decision 022, README dry-run output contract).
# Scenario:
#   Operator on a recovery host (pool.json moved aside) previews the teardown
#   before committing to it.
with subtest("Dry-run lock previews teardown on stdout without pool.json"):
    rc, _ = machine.execute("braid lock --dry-run >/tmp/dry.out 2>/tmp/dry.err")
    stdout = machine.succeed("cat /tmp/dry.out")
    stderr = machine.succeed("cat /tmp/dry.err")
    assert rc == 0, "dry-run should succeed without pool.json; stderr:\n" + stderr
    # Load-reason warning is part of the stdout preview.
    assert "pool.json unreadable" in stdout, "load warning missing from stdout:\n" + stdout
    # Representative teardown steps are previewed on stdout (compile_lock_steps).
    assert "unmount /mnt/storage" in stdout, "unmount step missing from preview:\n" + stdout
    assert "close LUKS mapper braid-" in stdout, "mapper-close step missing:\n" + stdout
    # The warning must NOT leak to stderr (stderr may still carry canonical
    # probe rows per README/Principle 13, so assert absence, not emptiness).
    assert "pool.json" not in stderr, "membership warning leaked to stderr:\n" + stderr
    # Non-destructive: pool stays mounted and mappers stay open.
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("ls /dev/mapper/braid-* >/dev/null")
```

Update the file's top-level preamble Scenario to mention the operator first
previews with `braid lock --dry-run`.

Also extend the four `load_membership_for_lock_*` Rust unit tests
(`main.rs:1408-1465`) for the new return shape: destructure `(loaded, note)`,
keep the membership assertions, and assert the note (e.g. present pool.json ->
`None`; corrupt -> `Some(PreviewNote::Warn(body))` whose body contains
`"corrupt"`). This pins the loader's stream-agnostic diagnostic at the unit
level; the VM subtest pins the dispatch-level stream routing.

Assertion rationale (structure-insensitive):
- `rc == 0` is the direct inverse of the bug (was exit 1).
- warning on **stdout** + absent from **stderr** proves the preview is one
  complete stream (the finding's core claim), not just that a warning exists.
- the two `compile_lock_steps` substrings prove the preview contains the real
  teardown, not an empty/`nothing to do` shell.
- mount + mappers still present proves a genuine preview, not an accidental
  real teardown.

## Files

- `cli/src/main.rs` -- `load_membership_for_lock` returns
  `(PoolMembership, Option<PreviewNote>)`; add `run_dry_run_lock`; route the note
  to stderr in `run_plain_lock`/`run_systemd_stop_lock`; collapse the
  `Commands::Lock` dry-run arm; update the four loader unit tests.
- `cli/src/lock.rs` -- add `extra_notes` to `cmd_lock`; add
  `cmd_lock_impl_with_notes`; make `cmd_lock_impl` a thin wrapper; update the
  orchestrate closure to pass `Vec::new()`.
- `tests/module/lock-tolerates-missing-pool-json.py` -- new dry-run subtest +
  preamble Scenario update.

## Verification

Focused run (localized change; no full suite per CLAUDE.md test-scope guidance):

1. `just test-rust` -- updated `load_membership_for_lock_*` unit tests, the
   `lock_policy` table, and `preview.rs` note-rendering tests pass.
2. `just test-vm lock-tolerates-missing-pool-json` -- exercises the new dry-run
   subtest (stream split) plus the existing real-lock / ExecStop subtests (which
   now see `[warn]`-prefixed stderr; substring assertions still pass).
3. `just test-vm pool-lock-dry-run-bypass braid-lock` -- confirms the dry-run
   lock arm still bypasses the pool lock, and that healthy-pool.json dry-run
   preview (already-locked "nothing to do", unverified-mapper warning routing)
   is unaffected.

Hand back to the user for a full-suite rerun rather than running the unscoped
`just test-vm` here.

## Implementation notes

- Kept `cmd_lock_impl` behind `#[cfg(test)]` because production dispatch now
  enters `cmd_lock_impl_with_notes`; this preserves the existing test helper
  arity without leaving a dead-code warning in normal builds.
