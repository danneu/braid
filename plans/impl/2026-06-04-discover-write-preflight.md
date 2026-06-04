# discover --write: gate before the scan

## Problem

`braid discover --write` runs the full multi-disk `cryptsetup` scan and
prints the preview to stdout *before* its fail-closed gates refuse. Against a
healthy pool.json, a refused `--write`:

- shells out `cryptsetup isLuks` + `luksDump` for every `/dev/disk/by-id`
  entry (`cli/src/discover.rs#discover_from_dir_inner`, loop at the
  `CryptsetupIsLuks` / `CryptsetupLuksDumpText` runner calls) -- a wasted
  scan, since the write was never going to happen, and
- emits `name = by-id` preview rows on stdout
  (`cli/src/main.rs#main` Discover arm, the `render_preview_lines` /
  `println!` loop) -- which a `discover --write > members` capture wrongly
  records even though the command exited non-zero.

The gates that cause the refusal -- pending-op present, and pool.json already
`ValidUuidKeyed` -- live inside `cli/src/discover.rs#write_discovered_membership`,
which the dispatch arm only calls *after* the scan and preview.

The pending-op gate there is also weaker than the rest of braid: it tests
`pending_op_json().exists()`, while every sibling mutator routes through the
canonical recovery-mode guard `cli/src/preflight.rs#check_no_pending_operation`
(ADR 017's recovery-mode hard-fail set explicitly includes `discover --write`,
and that guard's own doc comment already lists it as a caller). Extracting the
shared preflight is the moment to close that divergence, not preserve it.

This is the lone inconsistency among braid's pre-state-read gates:

- The **bare** discover path already gates before scanning:
  `cli/src/main.rs#main` calls `check_pool_json_for_bare_discover` before
  `discover_pool_members`.
- The **pool lock** for `discover --write` is acquired before dispatch
  (`lock_policy` maps `Discover{write:true}` to `NonBlocking`, acquired in
  `main` before the match), so lock contention already fails fast before the
  scan -- proven by `tests/module/pool-lock-discover-contention.py`.
- ADR 022 sets the mutating-command precedent: the pending-op preflight runs
  under the pool lock *before the planner starts* reading state
  (`docs/design/decisions/022-dry-run-preview-model.md`). `discover --write`'s
  pending-op gate currently runs after a state read (the scan), violating that
  precedent.

The gap is real but Low severity: no mutation occurs on the refuse path (the
authoritative gates still fire inside `write_discovered_membership`). This is a
consistency / wasted-work / misleading-stdout fix, not a correctness fix.

## Fix

Extract the two scan-independent gates into a preflight that the CLI runs
before the scan, and that `write_discovered_membership` keeps calling as the
authoritative mutation-layer check. The accept path is unchanged; only the
refuse path changes (no scan, empty stdout).

Scope of the preflight is exactly the two gates that need no scan result:

- pending-op present/unreadable -> route through the canonical recovery-mode
  guard `preflight::check_no_pending_operation` (the reviewer offered
  `load_journal` or this; this one is better -- it yields the canonical
  recovery-mode message so discover matches add/remove/replace). Do **not**
  re-implement a weaker `pending_op_json().exists()` probe.
- pool.json `ValidUuidKeyed` -> `ValidUuidKeyed`

It must NOT include:

- `Corrupt` refusal -- corrupt/off-schema pool.json is the documented rebuild
  path (ADR 017); the preflight returns `Ok(Corrupt)`.
- `--expect-count` (`ExpectCountUnmet`) -- needs `members.len()`, so it stays
  post-scan inside `write_discovered_membership`.
- the corrupt forensic sidecar write -- a mutation that must stay gated behind
  expect-count, post-scan.

### Edit 1 -- new preflight in `cli/src/discover.rs`

```rust
/// The scan-independent half of discover's `--write` fail-closed gates:
/// the two refusals that need no scan result. Factored out so the CLI can
/// run them before the multi-disk cryptsetup scan (fail-fast: no wasted
/// probe, no misleading preview rows) and `write_discovered_membership`
/// can re-run them as the authoritative check at the mutation layer.
/// Returns the classified shape on success so the writing path decides the
/// corrupt-sidecar branch without re-reading pool.json. `Corrupt` is an
/// accept (the ADR 017 rebuild path), not a refusal -- only a pending
/// operation and `ValidUuidKeyed` short-circuit here.
pub fn check_discover_write_preconditions(
    paths: &StatePaths,
) -> Result<PoolJsonShape, DiscoverWriteError> {
    // Canonical recovery-mode guard, same as add/remove/replace/unlock
    // (ADR 017). Fail-closed on a present, corrupt, OR unreadable journal --
    // not a bare `pending_op_json().exists()` probe.
    crate::preflight::check_no_pending_operation(paths)
        .map_err(DiscoverWriteError::PendingOperation)?;
    let pool_json_path = paths.pool_json();
    match classify_pool_json(&pool_json_path) {
        PoolJsonShape::ValidUuidKeyed => Err(DiscoverWriteError::ValidUuidKeyed {
            path: pool_json_path.display().to_string(),
        }),
        shape @ (PoolJsonShape::Corrupt | PoolJsonShape::Missing) => Ok(shape),
    }
}
```

This preserves the pending-op-before-shape ordering that
`write_discovered_membership` has today (a pending operation wins even over a
`ValidUuidKeyed` pool.json). The substantive win is consistency, not a flipped
outcome on present files: `.exists()` and the canonical guard both already
refuse a present journal, valid or corrupt. What changes is (a) the message --
discover emits the canonical recovery-mode text instead of its own wording, the
point of routing through the shared guard -- and (b) read-error handling:
`load_journal` fail-closes on an IO error (e.g. an ancestor-directory
permission error) that `Path::exists()` silently collapses to `false` and would
wave through.

**Enum change.** Replace `DiscoverWriteError::PendingOpExists { path }` with a
variant that forwards the canonical guard's recovery-mode message verbatim, so
`discover --write`'s pending-op refusal is byte-identical to its siblings:

```rust
/// A pending-operation journal is present or unreadable. Forwards the
/// canonical recovery-mode message from
/// `preflight::check_no_pending_operation` so `discover --write` refuses
/// pending operations identically to add/remove/replace (ADR 017).
#[error("{0}")]
PendingOperation(String),
```

The only references to the old variant are inside `discover.rs` (`rg
PendingOpExists` -> the enum def, its mention in the `write_discovered_membership`
doc-comment gate list, the construction site, and one unit test); `main.rs`
prints via `e.to_string()` and needs no match-arm change.

Decision (settled, reviewer-endorsed): forward the pending-op message *without*
discover's `"discover refusing to write pool.json: "` prefix that the other
`DiscoverWriteError` variants carry. Rationale -- uniform recovery-mode UX
across all commands is the stronger consistency axis (every sibling mutator --
add/remove/remove-missing/replace/unlock/enroll -- emits this exact canonical
message), and the text already self-identifies (`pending-op.json`, `run 'braid
recover'`). The within-discover-prefix alternative was considered and rejected:
it would make discover the lone outlier whose pending-op refusal diverges from
its siblings.

### Edit 2 -- `write_discovered_membership` composes the preflight

Replace its first two gate blocks (the pending-op `if` and the
`classify_pool_json` match) with a single call, keeping the rest
(expect-count, sidecar, save) verbatim:

```rust
pub fn write_discovered_membership(
    members: PoolMembership,
    paths: &StatePaths,
    expected_count: Option<usize>,
) -> Result<PoolMembership, DiscoverWriteError> {
    // Authoritative re-check at the mutation layer. The CLI also runs this
    // before the scan as a fail-fast preflight; re-running it here keeps the
    // invariant owned by the helper that performs the unsafe save (Mutation
    // Safety Heuristics; ADR 022 pending-op-preflight precedent).
    let needs_corrupt_sidecar =
        matches!(check_discover_write_preconditions(paths)?, PoolJsonShape::Corrupt);

    if let Some(expected) = expected_count {
        let actual = members.len();
        if actual != expected {
            return Err(DiscoverWriteError::ExpectCountUnmet { expected, actual });
        }
    }

    if needs_corrupt_sidecar {
        let pool_json_path = paths.pool_json();
        crate::membership::write_corrupt_sidecar(&pool_json_path).map_err(|e| {
            DiscoverWriteError::CorruptSidecarFailed {
                sidecar: e.target().display().to_string(),
                source: e.into_source(),
            }
        })?;
    }

    save_membership(&members, paths)?;
    Ok(members)
}
```

Also update this function's doc comment (the "three gates ... must fire BEFORE
any `save_membership` call" block) to say gates 1-2 now live in
`check_discover_write_preconditions` (pending-op via the canonical
`check_no_pending_operation` guard, then the `ValidUuidKeyed` shape refusal),
and that the CLI runs them pre-scan too.

### Edit 3 -- `cli/src/main.rs` Discover arm gates before the scan

Replace the bare-only preflight block with a symmetric pair that gates both
paths before `discover_pool_members`:

```rust
Commands::Discover(args) => {
    // Fail-closed state gates run BEFORE the multi-disk cryptsetup scan, so a
    // refusal costs no probe and prints no misleading preview rows. Bare
    // discover gates on pool.json shape; `--write` refuses a pending-op
    // journal or a healthy UUID-keyed pool.json (Corrupt/Missing are the
    // rebuild path). `write_discovered_membership` re-runs the `--write` gates
    // as the authoritative mutation-layer check (ADR 022 precedent).
    let pool_json = paths.pool_json();
    if args.write {
        if let Err(e) = braid_cli::discover::check_discover_write_preconditions(&paths) {
            print_cli_error(&e.to_string());
            std::process::exit(1);
        }
    } else if let Err(e) =
        braid_cli::discover::check_pool_json_for_bare_discover(&pool_json)
    {
        print_cli_error(&e.to_string());
        std::process::exit(1);
    }
    let runner = RealRunner;
    let scan = braid_cli::discover::discover_pool_members(&runner);
    // ... unchanged from here: drain_warnings, empty check, preview, write
}
```

The `Ok(PoolJsonShape)` from the preflight is intentionally discarded here;
`write_discovered_membership` re-derives it under the held lock.

No other production caller of `write_discovered_membership` exists
(`rg write_discovered_membership` -> `main.rs` + the test module only), so
nothing else needs touching.

## Tests

Two axes: the new preflight's logic (unit), and the observable "gate before
scan/preview" ordering (VM). Existing tests are the regression net for "the
refactor did not change write behavior."

### Unit -- `cli/src/discover.rs` tests (new seam logic)

Add direct tests for `check_discover_write_preconditions`, mirroring the
existing `classify_pool_json_*` / `check_pool_json_for_bare_discover_*` tests
and the `check_no_pending_operation` tests in `preflight.rs`:

1. returns `Ok(Missing)` over an empty state dir.
2. returns `Ok(Corrupt)` over off-schema / unparseable pool.json (assert it is
   an accept, not a refusal -- this is the ADR 017 rebuild path).
3. returns `Err(ValidUuidKeyed)` over a saved healthy membership; assert the
   "is already a healthy UUID-keyed membership" wording.
4. refuses over a **valid** pending journal -- seed via
   `journal::build_journal(PoolMembership::empty(), PoolMembership::empty(),
   OpKind::Add { phase: AddPhase::PoolMutation, targets: LuksUuidMap::new() })`
   + `journal::write_journal` (the pattern from `preflight.rs`'s
   `pending_op_refuses_when_present`); assert the message contains
   `"interrupted operation"`. Also assert a pending operation wins even when
   pool.json is `ValidUuidKeyed` (pins the gate order: pending-op before shape).
5. refuses over a **corrupt/unreadable** pending journal -- seed
   `std::fs::write(paths.pending_op_json(), "not json")`; assert the message
   contains `"cannot read"`. This is the case `.exists()` could not distinguish
   from a valid journal and is exactly why the gate routes through the canonical
   loader.

These are structure-insensitive (assert returned outcome + message wording,
not call sequence).

The existing `write_discovered_membership` gate tests stay as the mutation-layer
regression net, with one required update:
`discover_write_refuses_when_pool_json_is_valid_uuid_keyed` and
`discover_write_rebuilds_and_snapshots_when_pool_json_has_non_uuid_keys` are
unchanged. `discover_write_refuses_when_pending_op_exists` **must change**: it
seeds `"{}"` and asserts the old `"discover refusing to write pool.json:
pending-op.json exists at"` wording, which the canonical guard no longer emits
(`"{}"` now parses as a corrupt journal -> `"cannot read"`). Reseed it with a
valid journal (the `build_journal` pattern above) and assert the canonical
`"interrupted operation"` wording, so it still proves the composed helper
refuses through the canonical guard and leaves pool.json byte-for-byte
unchanged.

### VM -- `tests/cli/braid-discover.py` (the load-bearing behavioral test)

The ordering (preflight before scan/preview) lives in `main`'s dispatch and is
only observable by running the binary. The driver captures **stdout** by
default (existing subtests add `2>&1` to fold in stderr), so an empty stdout
capture on the refuse path is exactly "no preview rows were printed."

Extend the existing `discover --write also refuses healthy UUID-keyed pool.json`
subtest (currently uses `2>&1`) so it also captures stdout alone:

```python
with subtest("discover --write refuses healthy UUID-keyed pool.json before scanning"):
    before = read_pool_json()
    # stdout-only: a refusal that short-circuits before the scan prints no
    # preview rows. This is the regression guard for the pre-scan gate.
    stdout_only = machine.fail("braid discover --write")
    assert stdout_only.strip() == "", (
        "refused discover --write scanned and printed preview rows to stdout "
        "instead of short-circuiting; got:\n" + stdout_only
    )
    # stderr still carries the refusal, and pool.json is untouched.
    combined = machine.fail("braid discover --write 2>&1")
    assert "is already a healthy UUID-keyed membership" in combined, (
        "expected ValidUuidKeyed refusal; got:\n" + combined
    )
    assert read_pool_json() == before, "pool.json must be byte-for-byte unchanged"
```

Add a companion subtest for the pending-op gate (the second scan-independent
gate, and the one ADR 022 names), seeded over an absent pool.json so only the
pending-op gate fires:

```python
with subtest("discover --write refuses pending-op before scanning"):
    # Seed a pending-op journal with pool.json absent: the pending-op gate
    # must short-circuit before the scan, printing no preview rows.
    machine.succeed("mkdir -p /var/lib/braid")
    machine.succeed(
        "printf '%s' "
        + shlex.quote(json.dumps({
            "started_at": "2026-01-01T00:00:00Z",
            "op": {"op": "Add", "phase": "PoolMutation", "targets": {}},
            "pre_membership": {"disks": {}},
            "target_membership": {"disks": {}},
        }, sort_keys=True))
        + " > /var/lib/braid/pending-op.json"
    )
    assert_pool_json_absent()
    stdout_only = machine.fail("braid discover --write")
    assert stdout_only.strip() == "", (
        "refused discover --write (pending-op) printed preview rows to stdout; "
        "got:\n" + stdout_only
    )
    combined = machine.fail("braid discover --write 2>&1")
    # Robust to whether the seeded journal parses: the canonical guard emits
    # either "interrupted operation detected (pending-op.json exists, ...)"
    # (valid) or "cannot read pending-op.json" (corrupt); both name the file.
    # The exact valid-vs-corrupt wording is pinned by the Rust unit tests.
    assert "pending-op.json" in combined, (
        "expected a pending-op refusal; got:\n" + combined
    )
    assert_pool_json_absent()
    machine.succeed("rm /var/lib/braid/pending-op.json")
```

Sequencing note: place these subtests where pool.json is in the right state
(the UUID-keyed one after the existing rebuild subtest that leaves a healthy
pool.json; the pending-op one needs pool.json absent, so run it early -- near
the existing `bare discover ignores pending-op journal` subtest -- or remove
pool.json first).

Capture style: the two subtests above show a double invocation (stdout-only run,
then `2>&1`) for readability, but the cleaner pattern -- already used by
`braid-discover-empty-scan.py`'s `run_discover` helper -- is a single invocation
with `>/tmp/out 2>/tmp/err`, then assert `out.strip() == ""` and the refusal
substring in `err`. Either is correct (the refusal is idempotent and read-only);
prefer the single-invocation form.

Scope note (state in the plan, do not silently cap): the VM test pins the
user-visible harm (no preview rows on refuse). It does not assert "cryptsetup
was not invoked" -- that would require instrumenting the runner and is
structure-sensitive; the unit test + the early `return` in `main` cover the
no-wasted-scan half.

`tests/module/pool-lock-discover-contention.py` is unaffected: its post-release
subtest only checks the `ValidUuidKeyed` message and byte-unchanged pool.json,
both still true. Run it to confirm no regression, but it needs no edit.

## Docs

No user-facing behavior is added or removed -- `discover.md` already documents
the refusals. Optional one-line tightening in `docs/commands/discover.md`
(item that lists the `--write` gates) to note the refusal happens before the
scan; skip if it reads as noise. No ADR change: this aligns discover with the
existing ADR 022 preflight precedent rather than introducing a new model.

## Verify

```
just test-rust   # new + existing gate unit tests
just test-vm braid-discover braid-discover-empty-scan pool-lock-discover-contention pool-lock-precedes-state-read
```

- `braid-discover` -- behavioral (empty-stdout-on-refuse) + accept-path regression.
- `braid-discover-empty-scan` -- directly exercises the reordered dispatch arm:
  its `discover --write` subtest asserts the empty-scan refusal with exit 1 and
  no stdout preview. It stays green because its precondition pins pool.json
  absent (`test ! -e .../pool.json`), so the new preflight returns `Ok(Missing)`
  and the command still reaches the scan. Its `run_discover` helper is the
  single-invocation capture precedent noted above.
- `pool-lock-discover-contention` -- lock still fails before scan/write; its
  post-release subtest uses `2>&1` and only asserts the `ValidUuidKeyed` message
  is present, so F1's pending-op change does not touch it. No edit.
- `pool-lock-precedes-state-read` -- the direct guard that the pool lock
  precedes the new pre-scan pending-op + pool.json reads. Its
  "discover --write acquires before pending-op and probe reads" subtest needs
  **two coupled edits** (the FIFO `pool.json` subtest is unaffected):
  1. **Sentinel retarget (F1-driven).** Retarget the pending-op negative
     sentinel `"pending-op.json exists" not in out` to
     `"cannot read pending-op.json" not in out` -- the canonical guard's message
     for that subtest's deliberately-corrupt `{"op":"placeholder"}` seed (a
     pre-lock read now errors `"cannot read pending-op.json"`). Also update the
     priming comment just above it, which currently documents the old
     `"pending-op.json exists"` string -- so it is assertion **plus** comment,
     not quite single-site. Without the retarget the assertion passes vacuously
     and stops detecting a pre-lock journal read -- the same stale-string hazard
     that subtest's own comments flag for the probe sentinel.
  2. **Empty-scan baseline hardening (Edit-3-driven).** The subtest's
     no-contention baseline (`braid discover --write --expect-count 0`, asserting
     `"no braid-labeled LUKS2 devices found"` is present) reaches the empty-scan
     refusal *today* regardless of pool.json shape, because the current code
     scans before the `ValidUuidKeyed` gate. After Edit 3 the gate runs first, so
     the baseline only reaches that refusal when pool.json is not
     `ValidUuidKeyed`. Benign on the current diskless host (pool.json absent ->
     `Missing`), but add `machine.succeed("rm -f /var/lib/braid/pool.json")`
     right after the `mkdir -p /var/lib/braid` at the top of that subtest to keep
     the precondition robust against pool.json drift -- matching the defensive
     `rm -f` the FIFO subtest already uses. Without it, a stray pool.json makes
     the baseline refuse with "is already a healthy UUID-keyed membership" and
     fail on the misleading "did a discoverable member appear in the .nix?"
     assertion, sending the next maintainer after a phantom disk.

`just test-vm braid-discover-name-order` if touching the preview/scan path
beyond the gate (not expected here).

## Risk / blast radius

Narrow. The accept path (Missing/Corrupt pool.json, no pending-op) is
byte-identical to today: preflight returns `Ok`, scan + preview + write run as
before, and `write_discovered_membership` re-checks under the same held lock.
Only the refuse path changes -- it now exits before the scan with empty stdout.
The double-classify on accept (preflight in `main`, preflight again inside
`write_discovered_membership`) is two reads of a tiny local file, trivially
cheaper than the N `cryptsetup` shell-outs it removes on refuse.

## Implementation notes

- Extracted a `seed_valid_pending_journal(&StatePaths)` test helper in
  `discover.rs` instead of inlining the `build_journal`/`write_journal`
  boilerplate the plan showed per-test -- it is used by both new pending-op
  tests and the reseeded `discover_write_refuses_when_pending_op_exists`,
  so the helper avoids three identical copies.
- Both VM subtests use the single-invocation `>/tmp/out 2>/tmp/err` capture
  form (the plan's stated preference) rather than the double-invocation
  sketch, splitting stdout/stderr in one run.
