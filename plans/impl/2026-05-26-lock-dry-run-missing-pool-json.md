# Plan: fix lock --dry-run membership-loader divergence + regression test

## Context

`braid lock` treats `pool.json` as **non-authoritative**: per-candidate
`cryptsetup luksUUID` probes are the real fail-closed guard, so a missing or
corrupt `pool.json` must not block teardown. Commit `8db7277`
("fix(lock): tolerate missing pool membership") encoded this by switching the
real lock paths to the lenient `load_membership_for_lock` helper -- but it
updated only two of the three lock dispatch arms.

The `--dry-run` arm was left on the strict `load_membership_or_exit`, which
prints an error and `exit(1)` on any `pool.json` load failure. The result is a
dry-run that **lies about what the real command would do**: with `pool.json`
moved aside or corrupt, `braid lock` (plain and ExecStop) warns and proceeds
with a UUID-verified orphan-mapper cleanup, while `braid lock --dry-run` aborts
with exit 1. This violates the dry-run/real parity contract in
`docs/design/decisions/022-dry-run-preview-model.md` (Active), whose entire
purpose is to stop "a dry-run preview [from drifting] from the work a real run
would perform, especially around ... cleanup."

No test catches this. The `load_membership_for_lock_*` unit tests
(`cli/src/main.rs:1407-1465`) prove the *helper* is lenient but never prove the
dispatch *calls* it; `tests/module/lock-tolerates-missing-pool-json.py`
exercises only the plain and `--systemd-stop` arms, never `--dry-run`. So the
divergence can persist or regress with all tests green.

This is the pivot the reviewer's testing finding pointed at: it is not a
test-only task. The proposed test asserts `exit 0`, which the current code does
not produce -- the production divergence must be fixed in the same change, or
the test would pin the bug.

## The fix (one line)

`cli/src/main.rs:759-769`, the `Commands::Lock(args)` dry-run arm. Swap the
strict loader for the lenient one already used by the sibling arms:

```rust
// before
let membership = load_membership_or_exit(&paths, 1);
// after
let membership = load_membership_for_lock(&paths);
```

After the swap, all three lock arms load membership identically:
`run_plain_lock` (`main.rs:1158`), `run_systemd_stop_lock` (`main.rs:1222`),
and the dry-run arm. `cmd_lock(..., dry_run=true)` is already shared with the
ExecStop path, so an empty membership produces the same orphan-mapper close
plan the real run performs (`cli/src/lock.rs:870-886`: live pool devices that
match no member become orphan closes).

### Why no production refactor and no Rust unit test

- **No "impossible-by-construction" refactor.** The three arms cannot share a
  single membership-load call site: the dry-run arm must load *without* the
  pool lock (`LockPolicy::None`), while the real arms must load *after*
  acquiring it (the "pool lock precedes state read" invariant, guarded by
  `tests/module/pool-lock-precedes-state-read.py`). Hoisting the load above the
  branch would break that ordering. The minimal correct fix is the loader swap.
- **A Rust unit test cannot catch this divergence.** The bug is in `main()`'s
  dispatch wiring (which loader the arm picks), which calls `std::process::exit`
  and constructs `RealRunner`/`RealFilesystem` inline -- not unit-testable
  without a subprocess harness or a refactor that fights the lock-ordering
  invariant above. `cmd_lock` already handles empty membership correctly
  (`cmd_lock_with_empty_membership_closes_observed_orphan_mappers`,
  `cli/src/lock.rs:2671`); the bug is that dry-run dispatch never *reaches*
  `cmd_lock` with empty membership today (it exits first). Only an end-to-end
  invocation of the real binary observes it -> a VM subtest is the right and
  sufficient guard.
- **Scope is exactly one line.** Other mutating commands (`add`, `remove`,
  `replace`, `recover`, `enroll`) use the strict loader for *both* dry-run and
  real, which is correct -- `lock` is the only command where `pool.json` is
  non-authoritative. There are no sibling divergences to fold in. Config
  loading in the dry-run arm (`load_config_or_exit`) already matches the
  sibling arms.

## The regression test (VM subtest)

Add one subtest to `tests/module/lock-tolerates-missing-pool-json.py`, inserted
**between** the existing "Unlock pool and remove pool.json" subtest and the
"Plain braid lock closes mappers without pool.json" subtest. At that point the
pool is mounted with open `braid-*` mappers and `pool.json` is moved aside --
exactly the state the dry-run must preview. Because dry-run is side-effect-free,
the existing plain-lock subtest that immediately follows still finds the mappers
to close, so the sequence directly proves dry-run/real parity.

New subtest `braid lock --dry-run previews cleanup without pool.json`:

- Run `braid lock --dry-run` redirecting stdout and stderr to separate files
  (the existing subtests use `2>&1`; split them here to assert the ADR-022
  output contract -- preview on stdout, loader warning on stderr, mirroring the
  pattern already in `tests/cli/braid-lock.py:168-175`).
- Assert `rc == 0`. **This is the heart of the regression** -- current code
  exits 1 here.
- Assert stderr contains `"pool.json unreadable"` (the lenient loader's
  `MembershipError::Io` warning for a moved-aside file; matches the grep the
  existing real-run subtests already use).
- Assert stdout shows a real cleanup plan: contains `"close LUKS mapper"` and
  does **not** contain `"nothing to do"`. Optionally also assert
  `"orphaned mapper"` (empty membership classifies every live device as an
  orphan via `cli/src/lock.rs:884`).
  - Do **not** assert the literal "UUID-scanned fallback" wording: that note
    fires only on probe *failure* (`Snapshot::ProbeFailed`), not for a healthy
    mounted pool, so it would be brittle/incorrect here.
- Assert side-effect-free: `ls /dev/mapper/braid-*` still succeeds,
  `mountpoint -q /mnt/storage` still succeeds, and
  `systemctl is-active --quiet braid-online.service` is still active after the
  dry-run.

Also update the file-level preamble (`Intent` / `Why it exists` / `Scenario`,
`tests/module/lock-tolerates-missing-pool-json.py:1-16`) to name the
`--dry-run` arm alongside plain and `--systemd-stop`. The current
"catches edits that update only one dispatch arm" rationale already describes
this exact bug; extend it to call out the dry-run arm explicitly.

No `flake.nix` `checks` change is needed -- this adds a subtest to an existing
registered test, not a new test file.

## Files to modify

- `cli/src/main.rs` -- one-line loader swap at the `Commands::Lock` dry-run arm
  (line ~762).
- `tests/module/lock-tolerates-missing-pool-json.py` -- new dry-run subtest +
  preamble update.

## Verification

1. **TDD order (prove the test guards the bug):** add the subtest first and run
   it against unmodified production code:
   `just test-vm lock-tolerates-missing-pool-json`. It must **fail** at
   `assert rc == 0` (dry-run exits 1 before printing any preview). This confirms
   the test catches the divergence.
2. Apply the one-line fix in `cli/src/main.rs`.
3. Re-run `just test-vm lock-tolerates-missing-pool-json` -- it must now pass,
   including the existing plain-lock and ExecStop subtests (proving the dry-run
   left state intact and the real run still cleans up).
4. `just test-rust` -- the existing `load_membership_for_lock_*` unit tests must
   still pass (the helper is unchanged).
5. This is a localized lock-dispatch change; a focused VM run is sufficient. Do
   not autonomously kick off the full suite -- hand back to the user for their
   full-suite rerun if they want broader coverage.
