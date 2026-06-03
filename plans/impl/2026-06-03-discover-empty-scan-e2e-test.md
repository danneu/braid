# Plan: end-to-end coverage for the discover empty-scan refusal

## Context

`braid discover`'s no-members refusal -- `members.is_empty()` ->
`print_cli_error(NoMembersDiscovered)` -> `exit(1)` in the `Commands::Discover`
arm of `cli/src/main.rs#main` -- is the one discover refusal with **no
end-to-end coverage**. Its four siblings (corrupt pool.json, expect-count
mismatch, already-exists, healthy-UUID `--write`) are each driven through the
real binary in `tests/cli/braid-discover.py`, but that test always boots a
two-disk LUKS fixture, so the scan can never come back empty. The only existing
coverage of the empty path is indirect: the unit test
`no_members_discovered_message_carries_remediation`
(`cli/src/discover.rs#no_members_discovered_message_carries_remediation`) calls
`NoMembersDiscovered.to_string()` and never touches the wiring, and the sentinel
at `tests/module/pool-lock-precedes-state-read.py:83` asserts the message is
*absent* under lock contention (discover never reaches the scan there).

A regression that printed the empty preview and exited 0, dropped the refusal,
or routed it to stdout would pass every existing test. The highest-value guard
is the `--write` path's post-refusal `pool.json`-absence check:
`write_discovered_membership` deliberately carries no empty-guard (caller-policy
design, per the predecessor impl plan), so the `main.rs` `is_empty()` gate is
the *sole* barrier against `discover --write` writing a 0-member `pool.json` --
and nothing exercises it today.

This reverses the "Explicitly out of scope" note in
`plans/impl/2026-06-02-discover-empty-scan-refusal.md` ("No VM/integration test
positively exercising the empty-scan exit"). That deferral rested on two
premises that don't hold: (1) "no cheap host emits the empty-scan path without
holding the pool lock" -- false: a diskless `braid.enable` node reaches it
trivially when discover runs with no holder (bare discover takes no lock per
`main.rs` `lock_policy`, and `check_pool_json_for_bare_discover` passes on a
missing pool.json); (2) "every sibling refusal is tested only at the lib level"
-- false: `braid-discover.py` drives the other four refusals end-to-end. The
empty-scan refusal is an asymmetric gap, not a uniformly-deferred one.

**Intended outcome:** one minimal, single-responsibility VM test pins the
empty-scan *wiring* (exit code, stream routing, no pool.json write) for both
bare and `--write`, closing the asymmetry. Test-only change; no production code
and no docs change (`docs/commands/discover.md` already documents the refusal).

## Why a VM test (not a Rust integration test)

The binary hardcodes both paths the empty-scan path depends on, with no flag or
env override: the state dir via `StatePaths::production()`
(`cli/src/state_paths.rs`, pinned to `/var/lib/braid`; `--config` only moves the
braid.json config, not the state dir) and the scan dir via
`discover_pool_members` (`cli/src/discover.rs`, pinned to `/dev/disk/by-id`; the
`discover_from_dir` dir-arg seam is reachable only from unit tests). A committed
`assert_cmd` test would read the host's real `/var/lib/braid` and
`/dev/disk/by-id` -- non-hermetic and side-effecty. A VM gives an isolated
filesystem where "zero braid-labeled LUKS2 disks" is the natural boot state.

## Approach

Add one new test pair under `tests/cli/` on a **minimal diskless node** (no
`virtualisation.emptyDiskImages`, no initrd-fixture), register it in the flake
`checks`. On a node with no braid-labeled LUKS2 disks, the by-id scan returns
empty (non-LUKS host disks are silently filtered), so both invocations hit the
shared `is_empty()` gate.

The test owns the **wiring**, deliberately complementing the unit test (which
owns the **message wording**: it already pins `LUKS1`, `attached and readable`,
` -- `). So this VM test matches only the `error:` prefix + lead clause, keeping
it structure-insensitive to remediation-tail edits.

## Critical files

### 1. New: `tests/cli/braid-discover-empty-scan.nix`

Mirror `tests/module/pool-lock-precedes-state-read.nix`'s diskless node and
`tests/cli/braid-discover.nix`'s `{ braid }` / `../../modules/braid` shape:

```nix
# Test: braid-discover-empty-scan
#
# What: Boots a minimal diskless braid node (no disks, no LUKS fixture) and
# drives bare `braid discover` and `braid discover --write`. With zero
# braid-labeled LUKS2 disks attached, both must exit non-zero with the
# no-members refusal on stderr, print no preview rows on stdout, and `--write`
# must not create pool.json.
#
# Why: the empty-scan refusal is the only discover refusal with no end-to-end
# coverage -- braid-discover.py always boots two labeled disks, so it can never
# reach members.is_empty(). The unit test only pins the message string and the
# pool-lock sentinel only asserts the string's absence under contention.
{ braid }:
{
  name = "braid-discover-empty-scan";

  nodes.machine =
    { ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };
    };

  testScript = builtins.readFile ./braid-discover-empty-scan.py;
}
```

### 2. New: `tests/cli/braid-discover-empty-scan.py`

Use the `start_all()` / `wait_for_unit` preamble, the
`>/tmp/x.out 2>/tmp/x.err` stream-separation idiom from `single-disk.py:13`, and
`machine.execute` (returns `(rc, _)`) for the intentionally-failing commands.
Avoid placeholder-free f-strings (testing.md lint gotcha):

```python
# Intent: bare `braid discover` and `braid discover --write` refuse with a
#   non-zero exit and the no-members message, printing no preview rows, when
#   zero braid-labeled LUKS2 disks are attached; `--write` writes no pool.json.
# Why it exists: the members.is_empty() -> print_cli_error(NoMembersDiscovered)
#   -> exit(1) wiring in main.rs's Discover arm is the only discover refusal
#   with no end-to-end test. Its siblings are each driven through the real
#   binary in braid-discover.py, but that test always boots two labeled disks.
#   The unit test only checks NoMembersDiscovered.to_string(); the pool-lock
#   sentinel only asserts the string's ABSENCE under contention. A regression
#   that printed the empty preview and exited 0, dropped the refusal, or routed
#   it to stdout would pass every existing test.
# Scenario: an operator rebuilding a lost pool.json runs `braid discover` with
#   the array's disks momentarily detached/mislabeled and must get a clear
#   exit-1 refusal -- not a silent empty preview.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

ERR = "error: no braid-labeled LUKS2 devices found"
POOL_JSON = "/var/lib/braid/pool.json"


def run_discover(label, command):
    """Assert one discover invocation refuses an empty scan: exit 1, no stdout
    preview, no-members message on stderr. Asserts internally; returns None."""
    rc, _ = machine.execute(command + " >/tmp/out 2>/tmp/err")
    out = machine.succeed("cat /tmp/out")
    err = machine.succeed("cat /tmp/err")
    # Exit 1 exactly, not just non-zero: a panic (rc 101) or a reroute to
    # another code (e.g. exit 2) must not pass as a clean refusal. The plain
    # redirect preserves braid's own exit status, so the precise code is
    # observable. The documented contract (docs/commands/discover.md) is "exits 1".
    assert rc == 1, label + ": expected exit 1 refusal; rc=" + str(rc) + " err=" + err
    assert out.strip() == "", label + ": printed preview rows on stdout:\n" + out
    assert ERR in err, label + ": missing no-members refusal on stderr:\n" + err


with subtest("precondition: no pool.json so bare discover reaches the scan"):
    # check_pool_json_for_bare_discover passes on a Missing pool.json, so bare
    # discover proceeds past the gate to the (empty) by-id scan.
    machine.succeed("test ! -e " + POOL_JSON)

with subtest("bare discover refuses empty scan with exit 1 and no preview"):
    run_discover("bare discover", "braid discover")

with subtest("discover --write hits the same gate and writes no pool.json"):
    run_discover("discover --write", "braid discover --write")
    machine.succeed("test ! -e " + POOL_JSON)
```

### 3. Edit: `flake.nix` (`checks` block)

Insert directly after the `braid-discover-name-order` entry (`flake.nix:217-221`,
before `multi-add`), keeping the discover tests grouped and using the wrapped
`linuxCrane.braid` like its CLI-test siblings:

```nix
braid-discover-empty-scan = pkgs.testers.nixosTest (
  import ./tests/cli/braid-discover-empty-scan.nix {
    braid = linuxCrane.braid;
  }
);
```

## Reused patterns

- Diskless node: `tests/module/pool-lock-precedes-state-read.nix` (`braid.enable`,
  no `emptyDiskImages`, no fixture).
- CLI-test nix shape (`{ braid }`, `../../modules/braid`, `linuxCrane.braid`):
  `tests/cli/braid-discover.nix` + its flake entry at `flake.nix:212-216`.
- Stream separation: `tests/module/single-disk.py:13`
  (`>/tmp/d.out 2>/tmp/d.err`, then `cat`).
- Non-zero-with-message + `(rc, out)` from `machine.execute`:
  `tests/module/pool-lock-precedes-state-read.py:40-45`.
- File-absence assertion: `tests/module/single-disk.py:18`
  (`test ! -e /var/lib/braid/pool.json`).

## Out of scope

- **No message-wording assertions** (`LUKS1`, `attached and readable`, ` -- `):
  owned by `no_members_discovered_message_carries_remediation` in `discover.rs`.
  This test pins only the `error:` prefix + lead clause so remediation edits
  don't disturb it.
- **No LUKS1 / unreadable-disk warning coverage**: would need a LUKS1 fixture;
  the empty-scan *wiring* doesn't require it, and the warning-drain path is a
  separate concern.
- **No production-code or docs change**: the wiring already behaves correctly;
  `docs/commands/discover.md` already documents the refusal. This is pure
  regression coverage.
- **The historical impl plan stays untouched**: `plans/impl/2026-06-02-...md` is
  a frozen point-in-time record; its scoping note is superseded by this plan's
  Context, not rewritten in place.

## Verification

1. `just test-vm braid-discover-empty-scan` -- new test passes (and the flake
   `checks` entry evaluates).
2. Confidence check (manual, optional, revert after): temporarily change the
   refusal in `main.rs`'s Discover arm -- e.g. `std::process::exit(1)` ->
   `exit(0)`, or comment out the `if members.is_empty()` block -- and confirm
   `just test-vm braid-discover-empty-scan` now **fails** on the `rc == 1` /
   stdout-empty assertion. This proves the test guards the wiring, not just the
   message. Revert.
3. No Rust source changes, so `just test-rust` is not required; no new
   cross-links, so no `mdbook build docs` needed.
