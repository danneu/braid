# Plan: add `replace` to the wrapper pool lock

## Context

A code-review finding claimed two concurrent `braid remove` invocations
race past `check_no_pending_operation`, clobber `pending-op.json.tmp`,
and corrupt journal state. Investigation shows the finding is stale on
the `remove`/`remove-missing` part -- commit `3ee1674`
(`fix(wrapper): serialize alert-state mutators with pool lock`,
2026-05-05) already added both to the wrapper allowlist and updated
principle 12, with a regression test at
`tests/module/alert-state-lock.py:244-316`.

The finding's prescribed fix also names `replace`, and that part *is* a
real, currently-unfixed gap. `replace` is structurally identical to
`remove` for race purposes:

- `cli/src/replace.rs:877` -- `check_no_pending_operation` (pure read).
- `cli/src/replace.rs:476` -- `journal::write_journal` via
  `state_io::atomic_write`, which uses a deterministic
  `.pending-op.json.tmp` path (`cli/src/state_io.rs:62`) and so cannot
  tolerate concurrent writers in the same directory.
- `btrfs replace start` does reject a second concurrent ioctl
  kernel-side with `BTRFS_IOCTL_DEV_REPLACE_RESULT_ALREADY_STARTED`
  (`reference/btrfs-progs/cmds/replace.c:57`), but that is too late --
  by the time the kernel rejects the second `replace start`, the loser
  has already clobbered `pending-op.json` belonging to the winner.
  Recovery via `braid recover` works, but the failure mode is far more
  violent than principle 12's "fails fast with a clear message".

The wrapper lock is the only gate that prevents the race from firing in
the first place. Adding `replace` to the existing allowlist matches the
shape of the 2026-05-05 fix exactly.

`replace` does NOT write `acked-stats.json` or `alert-latch.json`, so
the alert-state-mutator paragraph in `docs/decisions/014-alerts.md:76`
stays as-is. Only the pool-mutator enumerations need widening.

## Changes

### 1. Wrapper allowlist

`modules/braid/braid-wrapper.sh:52` -- add `replace` to the non-blocking
fail-fast case:

```diff
-  unlock|add|recover|remove|remove-missing)
+  unlock|add|recover|remove|remove-missing|replace)
```

`modules/braid/braid-wrapper.sh:42-44` -- update the comment block to
list `replace` alongside the other interactive pool mutators:

```diff
-# - unlock/add/recover/remove/remove-missing: non-blocking fail-fast for
+# - unlock/add/recover/remove/remove-missing/replace: non-blocking fail-fast for
 #   interactive pool mutation; the user retries once the active operation
 #   finishes.
```

### 2. Principle 12

`docs/principles.md:67` -- add `replace` to both the mutator list and
the fail-fast list, and to the "do not fast-exit" tail clause:

- `(`unlock`, `add`, `recover`, `remove`, `remove-missing`, `ack`, `monitor`)`
  -> `(`unlock`, `add`, `recover`, `remove`, `remove-missing`, `replace`, `ack`, `monitor`)`
- `unlock, add, recover, remove, and remove-missing acquire the lock non-blocking`
  -> `unlock, add, recover, remove, remove-missing, and replace acquire the lock non-blocking`
- `add, recover, remove, and remove-missing do not fast-exit`
  -> `add, recover, remove, remove-missing, and replace do not fast-exit`

### 3. Decision doc 018 (systemd lifecycle)

`docs/decisions/018-systemd-lifecycle.md:140` -- same widening as
principle 12. Both the leading enumeration and the
"unlock, add, recover, remove, and remove-missing are non-blocking
fail-fast commands" clause need `replace`. The trailing
"add, recover, remove, and remove-missing do not fast-exit" clause
also needs `replace`.

### 4. Decision doc 014 (alerts) -- NO CHANGE

`docs/decisions/014-alerts.md:76` lists "monitor, ack, add, remove,
remove-missing" specifically as the set that writes `acked-stats.json`
or `alert-latch.json`. `replace` does not write either (verified by
grep in `cli/src/replace.rs`). The lock is *shared* with these
alert-state mutators, but `replace` is not itself one of them. Leaving
this paragraph alone preserves the precise meaning ("every alert-state
writer holds the lock") rather than blurring it into "every wrapper-
locked command writes alert state".

### 5. Regression test

Add a new VM test `tests/module/pool-lock-replace-contention.{nix,py}`
modeled on `tests/module/pool-lock-contention.{nix,py}` (the existing
`braid unlock` contention test). A new file is preferred over extending
`pool-lock-contention.nix` because `replace` needs a mounted multi-disk
pool plus a spare disk, while the existing test only seeds a 2-disk
offline pool.

**Fixture (`.nix`) -- explicit minimal shape:** This test bootstraps
the pool entirely from the python testScript via `braid add`, so the
`.nix` must NOT import `./lib/initrd-fixture.nix` and must NOT seed
`/var/lib/braid/pool.json`. Both of those are present in
`pool-lock-contention.nix:27,40-43` for that test's offline-pool
`unlock` scenario and would force a pre-formatted pool state that
bypasses the bootstrap path we want to exercise. The braid module
asserts `cfg.package != null` when `cfg.enable` is true
(`modules/braid/options.nix:81-82`), so `package = braid;` is
mandatory, not optional.

Write the fixture out in full (do not "copy and modify"
`pool-lock-contention.nix`):

```nix
{ braid }:
{ pkgs, lib, ... }:
{
  name = "pool-lock-replace-contention";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };

      virtualisation.emptyDiskImages = [
        { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
        { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
        { size = 512; driveConfig.deviceExtraOpts.serial = "disk3"; }
        { size = 512; driveConfig.deviceExtraOpts.serial = "disk4"; }
      ];
      virtualisation.memorySize = 1024;
    };

  testScript = builtins.readFile ./pool-lock-replace-contention.py;
}
```

512 MiB per disk is enough -- this test does not need the multi-second
replace-in-flight window that `ups-lb-during-replace.nix:41-58` uses,
because the wrapper fail-fast happens long before any btrfs ioctl.

**Script (`.py`) -- two phases:**

*Phase 1: bootstrap a real mounted 3-disk pool.* Use the same
passphrase-piped, fast-LUKS-args `braid add` pattern as
`tests/module/ups-lb-during-replace.py:42-68`:

Per `docs/testing.md:58-62`, the build-time linter rejects any
f-string without at least one `{placeholder}`. Only fragments that
substitute a variable (`pq`, `key`, `holder_pid`, `locks`, `out`) are
written as f-strings below; literal-only fragments are plain strings.
Python concatenates adjacent string literals at compile time, so
mixing the two forms within a parenthesised group is fine.

```python
import shlex

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        "braid add "
        "--luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"{key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )

with subtest("Build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("mountpoint -q /mnt/storage")
```

The fast LUKS args (`--pbkdf pbkdf2 --pbkdf-force-iterations 1000`)
are mandatory -- without them Argon2 makes each `braid add` take
tens of seconds. The disk4 entry is intentionally NOT added; it stays
as the unenrolled spare.

*Phase 2: hold the lock externally and prove `replace` fails fast.*
Reuse the background-flock-holder + readiness signal + `/proc/locks`
defense-in-depth pattern from
`tests/module/pool-lock-contention.py:42-56`, then run the *valid*
replace command shape with `--old NAME --new NAME=PATH` and the
passphrase piped via stdin:

```python
with subtest("braid replace fails fast when pool lock is held"):
    holder_pid = machine.succeed(
        "rm -f /tmp/holder.ready; "
        "nohup flock -x /run/braid-pool.lock "
        "sh -c 'touch /tmp/holder.ready; sleep 60' "
        ">/dev/null 2>&1 & echo $!"
    ).strip()
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)
    locks = machine.succeed("cat /proc/locks")
    assert "FLOCK" in locks, "no flock in /proc/locks: " + locks

    rc, out = machine.execute(
        f"timeout 5 sh -c \"printf '%s\\n' {pq} | "
        "braid replace "
        "--luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        "--old disk2 --new disk4=/dev/disk/by-id/virtio-disk4 "
        "--passphrase-stdin --yes\" 2>&1"
    )
    machine.execute(f"kill {holder_pid} 2>/dev/null || true")

    assert rc != 0, "expected rc != 0; out=" + out
    assert rc != 124, "replace hung past 5s cap; out=" + out
    assert "another braid operation is already in progress" in out, (
        "expected contention message; out=" + out
    )
    machine.fail("test -e /var/lib/braid/pending-op.json")
```

The `--old disk2 --new disk4=/dev/disk/by-id/virtio-disk4` shape
matches the clap definition at `cli/src/main.rs:200-204` (both `old`
and `new` are required `#[arg(long)]` strings, not positionals) and
mirrors the working invocation in
`tests/module/ups-lb-during-replace.py:55`.

The `machine.fail("test -e /var/lib/braid/pending-op.json")` assertion
at the end is the load-bearing check: it proves the wrapper-level
flock fail-fast happened *before* `journal::write_journal`
(`cli/src/replace.rs:476`). Without that assertion the test could
pass against a regression that moved the flock into the Rust CLI past
the journal seam.

Preamble (per `AGENTS.md` Test Conventions): Intent / Why it exists /
Scenario, citing the race window (preflight at `cli/src/replace.rs:877`
to journal write at `:476`) and the deterministic
`.pending-op.json.tmp` path (`cli/src/state_io.rs:62`) as the concrete
risk.

Register in `flake.nix` alongside the existing entries (the explorer
located the block at `flake.nix:599-608`):

```nix
pool-lock-replace-contention = pkgs.testers.nixosTest (
  import ./tests/module/pool-lock-replace-contention.nix {
    braid = linuxCrane.braid-cli-unwrapped;
  }
);
```

## Critical files

- `modules/braid/braid-wrapper.sh` (lines 42-52)
- `docs/principles.md` (line 67)
- `docs/decisions/018-systemd-lifecycle.md` (line 140)
- `tests/module/pool-lock-replace-contention.nix` (new)
- `tests/module/pool-lock-replace-contention.py` (new)
- `flake.nix` (lines ~599-608, sibling registration)

## Verification

1. `just test-rust` -- no Rust code changes, so this is a sanity pass
   (must still be green).
2. `just test-vm pool-lock-contention` -- existing test must still
   pass (proves the unlock subtest is undisturbed; the wrapper change
   is additive).
3. `just test-vm pool-lock-replace-contention` -- the new test should
   pass. Then revert the `braid-wrapper.sh` allowlist change locally
   and re-run; the test must FAIL (confirms it actually catches the
   regression). Re-apply the fix and confirm green.
4. `just test-vm alert-state-lock` -- prior coverage for `remove`,
   `remove-missing`, `add`, `ack`, `monitor` must still pass; the
   wrapper change is additive.
5. Spot-read `docs/principles.md:67` and
   `docs/decisions/018-systemd-lifecycle.md:140` after edits to
   confirm internal consistency (both enumerations match the wrapper
   case statement and each other).

## Out of scope

- Any change to `cli/src/replace.rs` or `cli/src/state_io.rs`. The
  race is closed at the wrapper layer; making `atomic_write` pick a
  per-process tmp name would be defense-in-depth but is unnecessary
  and would obscure the principle 12 invariant that the wrapper is
  the single mutual-exclusion gate.
- Any change to `docs/decisions/014-alerts.md`. `replace` does not
  write alert state.
- Any refactor of `pool-lock-contention.{nix,py}` to consolidate with
  the new replace test. The two have different fixture shapes; mixing
  them adds setup complexity for no behavioral gain.
- Adding `--enqueue`-style queuing for `replace`. Principle 12 is
  explicitly "do not queue interactive pool operations -- fail fast
  and let the user retry."
