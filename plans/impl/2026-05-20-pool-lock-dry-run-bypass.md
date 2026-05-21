# Plan: pin the dry-run-bypasses-lock invariant with a unified VM test

## Context

The `ff6f766 fix(lock): move pool lock ownership into rust dispatch` refactor centralized pool-lock acquisition into one site (`cli/src/main.rs:489 acquire_per_policy(&pool_lock, lock_policy(&cli.command))`), driven by an exhaustive `lock_policy()` table at `cli/src/main.rs:100`. For every mutator (`add`, `remove`, `remove-missing`, `replace`, `unlock`, `enroll`, `recover`, `lock`), `lock_policy` returns `None` when `dry_run` is set, so dispatch never touches `/run/braid-pool.lock`. This is the "safe to preview at any time" UX contract: a user can run `braid <cmd> --dry-run` while another braid operation is in flight without the preview blocking or failing.

What exists today:

- **Classification unit test:** `lock_policy_classifies_every_command_and_branch` at `cli/src/main.rs:1359` pins the `Commands -> LockPolicy` mapping, including all 8 dry-run cases (the 7 mutators above plus `lock --dry-run` at `cli/src/main.rs:163-173`).
- **One end-to-end `None`-policy case:** `tests/module/pool-lock-discover-contention.py:54-58` proves that bare `discover` (also `None` policy) runs under a held external flock without printing the contention message.
- **Real-run contention cases:** `tests/module/pool-lock-precedes-state-read.py:56-68` and the per-command `pool-lock-<cmd>-contention.py` tests assert each mutator's `NonBlocking` policy fails fast with the contention message.

What is missing: no integration test proves the 8 dry-run cases actually bypass the lock at runtime. The original finding observed this for `enroll` specifically; the same gap applies to all 8. A regression that re-introduces a per-arm `acquire_pool_or_exit()` call inside a dispatch arm (the pre-refactor pattern), or that special-cases dry-run inside `acquire_per_policy`, would silently break the UX contract for any of the 8 commands. The classification unit test would not catch this; only an integration test that holds the lock and observes the absence of the contention message would.

The pivot from the original finding is structural: one unified VM test that pins the invariant for all 8 dry-run cases at once, mirroring the shape of `pool-lock-precedes-state-read.py` (a `cases` dict with a shared assertion helper).

## Files to create

### `tests/module/pool-lock-dry-run-bypass.py`

Mirror `tests/module/pool-lock-precedes-state-read.py` shape with inverted assertion. Concretely:

- **Preamble** (literal form per `docs/testing.md:13-22`):
  - Intent: every `--dry-run` mutator bypasses `/run/braid-pool.lock` -- dispatch must not acquire under dry-run, so preview is safe to run while another operation holds the lock.
  - Why it exists: `lock_policy` (`cli/src/main.rs:100`) centralizes the dispatch-time policy decision and returns `None` for each mutator under `--dry-run`. The classification is unit-tested at `cli/src/main.rs:1359` but the runtime bypass behavior is not exercised end-to-end for any of the 8 dry-run cases (only for bare `discover`). A regression that re-introduces per-arm `acquire_pool_or_exit()` calls inside dispatch arms, or that special-cases dry-run inside `acquire_per_policy`, would silently break the "safe to preview at any time" UX contract.
  - Scenario: admin starts a long-running pool mutation, then runs `braid <mutator> --dry-run` from another shell. The preview should not block on the held flock and should not print "another braid operation is already in progress".
- **VM bring-up:** `start_all()` + `wait_for_unit("multi-user.target", timeout=120)`.
- **Holder helper:** copy the `with_holder(command, timeout, hold_secs)` body from `pool-lock-precedes-state-read.py:23-36` (background `flock -x /run/braid-pool.lock` + `sleep`), but **invert the default arithmetic so `hold_secs > timeout`**. Call sites in this test use `with_holder(command, timeout=2, hold_secs=8)`. The asymmetry is load-bearing: it is the only thing that distinguishes "dry-run never tried to acquire" from "dry-run blocked on acquire and got the lock after the holder released". If `hold_secs <= timeout` (the precedes-state-read default of `timeout=5, hold_secs=4`) and a regression made dry-run acquire the lock, the command would wait ~4s, the holder would release, the command would proceed and complete inside the 5s timeout, and the negative assertion would pass vacuously. With `hold_secs=8` and `timeout=2`, any blocking acquire is guaranteed to still be blocked when `timeout(1)` fires, producing `rc=124` -- which the assertion rejects.
- **Sanity probe inside the holder block:** `assert "FLOCK" in machine.succeed("cat /proc/locks")` (matches `pool-lock-discover-contention.py:43-44`) -- proves the holder actually holds before the negative assertions run, so the test cannot pass vacuously if the holder fails to start.
- **Assertion helper:**
  ```python
  def assert_no_contention(name, command):
      rc, out = with_holder(command, timeout=2, hold_secs=8)
      assert rc != 124, (
          f"{name}: dry-run blocked on the held lock for >2s "
          "(holder held for 8s, so the command was demonstrably waiting "
          f"on acquire, not running); out={out}"
      )
      assert "another braid operation is already in progress" not in out, (
          f"{name}: dry-run acquired the pool lock; out={out}"
      )
      # Reject pre-dispatch failure modes so a malformed invocation cannot
      # silently make the negative assertion vacuous. The cases below run
      # against /nonexistent/braid.json and are crafted to parse cleanly;
      # if either sentinel appears, the command never reached
      # `acquire_per_policy` at cli/src/main.rs:489 and the test is not
      # exercising what it claims to.
      assert "Usage:" not in out, (
          f"{name}: clap rejected the invocation before dispatch -- "
          f"fix the command shape; out={out}"
      )
      assert "must be run as root" not in out, (
          f"{name}: root check at cli/src/main.rs:480 fired before "
          f"dispatch (test VM should run as root); out={out}"
      )
  ```
  Note: still no assertion on `rc != 0` or on a specific downstream failure message. With `--config /nonexistent/braid.json` and `acquire_per_policy` running at `cli/src/main.rs:489` before any config or membership load, the negative-plus-pre-dispatch-rejection assertion is meaningful regardless of what each command does after dispatch. A command may exit with config-not-found, membership-not-loadable, or any other downstream error -- the guarantees being pinned are (a) no contention message, (b) no `timeout(1)` kill at 2s, and (c) the command reached past the two pre-dispatch gates (clap parse, root check).
- **Cases:** one `with subtest("dry-run mutators bypass /run/braid-pool.lock"):` block with a `cases` dict matching all 8 dry-run cases:
  ```python
  cases = {
      "add": "printf x | braid --config /nonexistent/braid.json add disk1=/dev/disk/by-id/virtio-disk1 --passphrase-stdin --yes --dry-run",
      "remove": "braid --config /nonexistent/braid.json remove disk1 --yes --dry-run",
      "remove-missing": "braid --config /nonexistent/braid.json remove-missing --missing-id 1 --yes --dry-run",
      "replace": "printf x | braid --config /nonexistent/braid.json replace --old disk1 --new disk2=/dev/disk/by-id/virtio-disk2 --passphrase-stdin --yes --dry-run",
      "unlock": "printf x | braid --config /nonexistent/braid.json unlock --passphrase-stdin --dry-run",
      "enroll": "printf x | braid --config /nonexistent/braid.json enroll /nonexistent/keydir --passphrase-stdin --dry-run",
      "recover": "printf x | braid --config /nonexistent/braid.json recover --passphrase-stdin --dry-run",
      "lock": "braid --config /nonexistent/braid.json lock --dry-run",
  }
  for name, command in cases.items():
      assert_no_contention(name, command)
  ```

### `tests/module/pool-lock-dry-run-bypass.nix`

Near-copy of `tests/module/pool-lock-precedes-state-read.nix`. Minimal VM:

```nix
# Test: pool-lock-dry-run-bypass
#
# What: --dry-run mutators must not acquire /run/braid-pool.lock.
#
# Why: `lock_policy` returns None for every dry-run mutator, so dispatch must
# pass through `acquire_per_policy` without acquiring. Pins the runtime side
# of the classification unit test.
{ braid }:
{
  name = "pool-lock-dry-run-bypass";

  nodes.machine =
    { ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };
    };

  testScript = builtins.readFile ./pool-lock-dry-run-bypass.py;
}
```

No disks, no pool, no pool.json -- the cases use `/nonexistent/braid.json` and lock acquisition runs before any state load.

### `flake.nix` registration

Add one entry immediately after the existing `pool-lock-precedes-state-read` block at `flake.nix:702-706`:

```nix
pool-lock-dry-run-bypass = pkgs.testers.nixosTest (
  import ./tests/module/pool-lock-dry-run-bypass.nix {
    braid = linuxCrane.braid-cli-unwrapped;
  }
);
```

No unstable-lane registration needed: `--unstable` works per-test via `--override-input nixpkgs ...` (see `justfile:16,152`).

## What is NOT changing

- `cli/src/main.rs` -- the centralized `lock_policy()`/`acquire_per_policy()` mechanism is correct; the test only pins behavior.
- The existing `lock_policy_classifies_every_command_and_branch` unit test at `cli/src/main.rs:1359` -- it continues to pin the classification layer; the new VM test pins the runtime translation.
- `tests/module/pool-lock-precedes-state-read.py` -- intent is "lock precedes state read"; inverting it in-place would muddy semantics. Sibling file is the right shape.
- The per-command `pool-lock-<cmd>-contention.py` tests -- they remain the authoritative real-run contention coverage.

## Verification

End-to-end:

```
just test-vm pool-lock-dry-run-bypass
```

Expected: VM boots, 8 cases all assert (no contention message, no 2s timeout, no pre-dispatch failure). Each case waits for the holder to release before the next starts, so per-case wall time is bounded by `hold_secs=8` plus small overhead (~9s). Total in-VM runtime ~75s for the 8 cases; full `just test-vm` runtime additionally includes VM boot.

Sanity that nothing else regressed:

```
just test-rust              # confirms lock_policy unit test still passes
just test-vm pool-lock-precedes-state-read pool-lock-enroll-contention pool-lock-discover-contention
```

Optional unstable-lane forecast (catches upstream `flock`/wrapper drift):

```
just test-vm pool-lock-dry-run-bypass --unstable
```

Failure-mode check (manual, only if doubting the test): temporarily edit `cli/src/main.rs:100` to make `Add(args)` return `NonBlocking` even under `--dry-run`, then `just test-vm pool-lock-dry-run-bypass` -- the `add` case should fail with the contention-message assertion. Revert the edit.
