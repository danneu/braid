# Fail fast on pool-lock contention

## Context

`modules/braid/braid-wrapper.sh:38` calls `@flockBin@ 9` with no flag,
which blocks indefinitely waiting for `/run/braid-pool.lock`. The
original plan that introduced the flock
(`plans/wip/flickering-greeting-clarke.md`) did not specify acquisition
behavior; it shipped a blocking call.

The problem is two-fold: a wedged holder hangs any subsequent
mutating command silently with no feedback, and the wrapper offers no
fast signal that "another braid operation is already in progress".
The realistic scenario is post-boot concurrent admin operations: an
admin runs `braid add` (e.g. a long-running balance), then a second
shell runs `braid unlock` and silently waits forever.

The fix is to make the wrapper **non-blocking**: braid does not queue
pool operations. A concurrent `unlock` / `add` / `recover` fails
immediately with a clear message and the user retries after the
active operation completes. This is simpler than any timeout-based
design — there is no "how long is too long" question and no
test-only knobs.

## Change

### 1. `modules/braid/braid-wrapper.sh:38`

Replace:

```sh
      @flockBin@ 9
```

with:

```sh
      if ! @flockBin@ -n 9; then
        echo "braid: another braid operation is already in progress (pool lock /run/braid-pool.lock is held); retry once it finishes" >&2
        exit 1
      fi
```

Notes:
- `-n` is non-blocking: `flock` returns 1 immediately if the lock is
  held by another process.
- The post-flock `mountpoint -q` recheck at lines 46–55 stays. It
  still matters for the *sequential* case — a script that runs
  `braid unlock` twice in a row should see the second invocation
  acquire the lock cleanly and exit with "pool already mounted",
  not error.
- The wrapper currently has no `|| { echo ... ; exit N }` precedent
  (the only `>&2` lines are non-fatal `WARNING:` messages — see lines
  81/84/88), so this introduces a new pattern. The wording matches
  the existing `braid:`-prefixed style.
- The flock comment block above (lines 30–33) does not need to
  change — it already describes *why* we hold the lock. The new
  failure stanza is self-explanatory.

### 2. Behavior at each entry point — verify, no code change

- **`braid-auto-unlock.service`** (`modules/braid/storage.nix:148`):
  the script's flow at lines 216–226 captures `braid unlock`'s exit
  code into `ret` and logs `"unlock failed (exit $ret), skipping"`
  for any non-2 failure, then falls through to umount. A wrapper
  exit 1 from contention will be logged and the service will still
  exit 0 — auto-unlock must never block boot. **No change needed.**
- **`braid-unlock.service`** (`storage.nix:103`): runs the wrapper
  as its sole `ExecStart`. Wrapper exit 1 marks the unit failed,
  which is correct — manual unlock failure should be visible to the
  user via `systemctl status braid-pool.target`.
- **`braid-pool.target`** (`storage.nix:126`): unaffected; just
  inherits `braid-unlock.service`'s status.

### 3. New focused contention test — `tests/module/pool-lock-contention.{nix,py}`

A dedicated minimal test that asserts the failure layer
deterministically. Modeled on `tests/module/systemd-lifecycle.nix`
but trimmed to a single 2-disk pool.

#### `tests/module/pool-lock-contention.nix`

```nix
# Test: pool-lock-contention
#
# What: Verifies the wrapper's flock acquisition is non-blocking and
# fails fast when another process holds /run/braid-pool.lock.
#
# Why: Without -n on the flock call, a wedged holder would silently
# hang any concurrent `braid unlock` invocation forever. This test
# guards the failure layer — it must fail if the wrapper regresses
# to a blocking flock.
{ braid }:
{ pkgs, lib, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [ "disk1" "disk2" ];
in
{
  name = "pool-lock-contention";

  nodes.machine = { pkgs, lib, ... }: {
    imports = [
      ../../modules/braid
      (import ./lib/initrd-fixture.nix {
        inherit passphrase diskNames;
        description = "Prepare LUKS + btrfs fixture for lock-contention test";
      })
    ];

    braid = {
      enable = true;
      package = braid;
    };

    systemd.tmpfiles.rules = [
      "d /var/lib/braid 0755 root root -"
      ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"disk1":{"by_id":"/dev/disk/by-id/virtio-disk1"},"disk2":{"by_id":"/dev/disk/by-id/virtio-disk2"}}}''
    ];

    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];
    virtualisation.memorySize = 1024;
  };

  testScript = builtins.readFile ./pool-lock-contention.py;
}
```

#### `tests/module/pool-lock-contention.py`

```python
# Test: pool-lock-contention
#
# Intent: When another process holds /run/braid-pool.lock, the
# wrapper must fail fast (exit 1) with a clear "another braid
# operation is already in progress" message — never hang.
#
# Why it exists: Without -n on the flock call, a wedged holder
# (e.g. a long-running `braid add` balance) would silently hang any
# concurrent `braid unlock` invocation forever. A blocking-flock
# regression must fail this test.
#
# Scenario: Admin starts `braid add` in one shell (modeled here as a
# background flock holder), then opens a second shell and runs
# `braid unlock` — the second invocation should fail immediately.

import shlex

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

with subtest("Precondition: pool offline"):
    machine.fail("mountpoint -q /mnt/storage")

with subtest("braid unlock fails fast when pool lock is held"):
    # Start the holder. The holder ONLY writes /tmp/holder.ready
    # AFTER flock has actually acquired the lock — without this
    # readiness signal the test would race the holder and could
    # falsely take the normal unlock path before the lock is held.
    machine.succeed(
        "rm -f /tmp/holder.ready /tmp/holder.pid; "
        "( flock -x 9 sh -c 'touch /tmp/holder.ready; sleep 60' "
        "9>/run/braid-pool.lock ) & "
        "echo $! >/tmp/holder.pid"
    )
    # Block until the holder confirms it owns the lock.
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)

    # Wall-clock cap of 5s — non-blocking flock should fail in
    # well under a second. The cap exists so the test fails (not
    # hangs) if the wrapper regresses to blocking flock.
    rc, out = machine.execute(
        f"timeout 5 sh -c 'printf %s\\\\n {pq} | "
        f"braid unlock --passphrase-stdin' 2>&1"
    )

    # Always tear down the holder, even if assertions below fail.
    machine.succeed(
        "kill $(cat /tmp/holder.pid) 2>/dev/null; "
        "wait $(cat /tmp/holder.pid) 2>/dev/null; true"
    )

    assert rc != 0, f"expected unlock to fail, got rc=0; out={out}"
    assert rc != 124, (
        f"unlock hung past 5s wall-clock cap — wrapper regressed "
        f"to blocking flock; out={out}"
    )
    assert "another braid operation is already in progress" in out, (
        f"expected contention message; out={out}"
    )

    # The contention failure must not have mounted the pool.
    machine.fail("mountpoint -q /mnt/storage")
```

#### Register in `flake.nix`

Add immediately after the existing `systemd-lifecycle` entry at
`flake.nix:451-455`:

```nix
          pool-lock-contention = pkgs.testers.nixosTest (
            import ./tests/module/pool-lock-contention.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
```

### 4. Update existing `tests/module/systemd-lifecycle.py` subtests for the new contract

The fail-fast change breaks two existing subtests because they
assume queueing semantics. These updates are *contract* changes,
not timing-budget changes:

#### Subtest 6 (lines 125–159) — "Concurrent unlock attempts serialize via flock"

The current assertion uses `machine.succeed("... wait")` (both
processes must exit 0) and asserts one says "pool already mounted".
Under fail-fast, the loser may exit 1 with the contention message
instead. Update to:

- Use `machine.execute(...)` for the launch line so a non-zero loser
  doesn't break the test invocation itself.
- Capture per-process exit codes via `pid_a` / `pid_b` like
  subtest 9 already does.
- Assert: exactly one process exits 0 (the winner unlocks the pool),
  and the other either exits 0 with `"pool already mounted"` (won
  the lock sequentially after the winner released) **or** exits 1
  with `"another braid operation is already in progress"` (lost the
  flock race).
- Update the subtest's intent comment to describe the new mutual-
  exclusion contract.
- Rename the subtest title to "Concurrent unlocks: one wins, the
  other fast-fails or sees mounted".

#### Subtest 9 (lines 284–346) — "Concurrent add attempts serialize via flock"

The current assertion is `exit_a == 0 && exit_b == 0` plus
`pool.json contains all 5 disks`. This is *fundamentally
incompatible* with fail-fast: only one of disk4/disk5 gets added.
Rewrite to:

- New intent: "Concurrent adds reject the loser cleanly; pool.json
  reflects the winner only and is not corrupted by the rejected
  attempt."
- Assert: exactly one exit code is 0 and the other is 1 (use a
  symmetric `(exit_a == 0) ^ (exit_b == 0)` check so the test
  doesn't depend on which one wins).
- Assert: the failing process's stderr contains
  `"another braid operation is already in progress"`.
- Assert: pool.json has 4 disks (`disk1`, `disk2`, `disk3`, plus
  exactly one of `disk4` or `disk5`) — and the surviving disk
  matches the winning process.
- Assert: `btrfs fi show /mnt/storage` reports 4 devices, not 5.
- Assert: no residual `pending-op.json` from the rejected attempt
  (the wrapper's flock check fires *before* the CLI writes the
  journal, so this should hold trivially — but assert it explicitly
  as a regression guard).

Both updates are contract-driven, not timing-driven. They do not
shrink any timeout or tighten any flake-prone assertion — they just
swap the expected outcome to match the new behavior.

### 5. `docs/decisions/systemd-lifecycle.md` — rewrite "Unlock path mutual exclusion"

The current "Unlock path mutual exclusion" section
(`systemd-lifecycle.md:133–135`) describes the flock as queueing
("acquired before the CLI runs and held through post-processing").
Rewrite to:

> Pool-mutating commands (`unlock`, `add`, `recover`) acquire an
> exclusive non-blocking `flock` on `/run/braid-pool.lock` in the
> wrapper before invoking the CLI. **braid does not queue pool
> operations** — if the lock is already held by another braid
> process, the wrapper exits 1 immediately with
> `"braid: another braid operation is already in progress"` and
> the user must retry after the active operation completes. The
> lock is held through post-processing (permissions,
> `braid-online` activation). After acquiring the lock, `unlock`
> re-checks `mountpoint -q` and exits cleanly if the pool was
> already mounted by a prior winner that finished sequentially —
> `add` and `recover` do not fast-exit because they operate on
> mounted pools. See [Principle 12](../principles.md#12-one-pool-operation-at-a-time).

Also update key constraint #5 in the "Key design constraints"
section (`systemd-lifecycle.md:153`) to reflect that exclusion is
non-blocking (e.g. "One pool operation at a time. Enforced by a
non-blocking `flock` in the wrapper — concurrent attempts are
rejected, not queued.").

No README.md change. The new failure mode is self-explanatory from
the error message itself; no new command, flag, or option is
added.

## Out of scope (deliberate non-changes)

- **`docs/principles.md` Principle 12.** The principle says one
  pool operation at a time. That is still true; non-blocking is the
  enforcement mechanism, not a principle change. (If the agreed
  rewrite of systemd-lifecycle.md exposes a wording mismatch in
  Principle 12, fix it then — but no proactive principle edit is
  planned.)
- **NixOS option for the lock behavior.** No knob is added.

## Critical files

| File                                    | Change                                                              |
| --------------------------------------- | ------------------------------------------------------------------- |
| `modules/braid/braid-wrapper.sh`        | Replace `flock 9` with non-blocking `flock -n 9` + fail-fast exit   |
| `tests/module/pool-lock-contention.nix` | New — minimal 2-disk fixture                                        |
| `tests/module/pool-lock-contention.py`  | New — holder-with-readiness + fail-fast assertions                  |
| `tests/module/systemd-lifecycle.py`     | Update subtests 6 and 9 for the new contract (one wins, one fails)  |
| `flake.nix`                             | Register `pool-lock-contention` next to `systemd-lifecycle`         |
| `docs/decisions/systemd-lifecycle.md`   | Rewrite "Unlock path mutual exclusion" + key-constraint #5          |

## Verification

1. `just test-vm pool-lock-contention` — runs the new dedicated
   test; ~30s VM boot + sub-second contention assertion. Must
   pass.
2. `just test-vm systemd-lifecycle` — runs the updated lifecycle
   suite. Subtests 6 and 9 must pass under their new assertions;
   all other subtests must be unaffected.
3. Confirm the regression direction by hand: temporarily revert
   the wrapper change (drop `-n`), re-run `just test-vm
   pool-lock-contention`, observe the assertion `unlock hung past
   5s wall-clock cap` fire. Restore the change.
4. Manual smoke in a VM: `( flock -x 9 sleep 30 9>/run/braid-pool.lock ) &`
   then `printf %s\\n testpassphrase | braid unlock --passphrase-stdin` —
   should print "braid: another braid operation is already in
   progress …" and exit 1 in well under a second.
