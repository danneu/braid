# Plan: make the discover probe-before-lock sentinel self-priming

## Context

A Low/Testing review finding flagged the `assert "no braid-labeled LUKS2
devices found" not in out` check in the `discover --write acquires before
pending-op and probe reads` subtest of
`tests/module/pool-lock-precedes-state-read.py` as "structurally weak ...
cannot fail when its target regression is reintroduced."

**The finding's headline claim is factually wrong.** It assumed the test host
has a braid-labeled `disk1`, so a pre-lock probe would print a preview (not the
empty-scan refusal). Verification disproves this:

- The test's `.nix` (`tests/module/pool-lock-precedes-state-read.nix`) sets up
  **no disks at all** -- no `emptyDiskImages`, no LUKS, no `add`/`enroll`.
  Sibling tests that need real braid disks use `emptyDiskImages`; this one does
  not. The braid module creates no disks.
- On a diskless host, a probe leaked above the lock finds zero members and hits
  `print_cli_error(NoMembersDiscovered)` (`cli/src/main.rs#main`, the
  `Commands::Discover` arm), emitting exactly `no braid-labeled LUKS2 devices
  found`. So the current assertion **does** trip under its target regression --
  it is sound, and the finding's proposed "assert preview rows absent"
  replacement would be the actually-vacuous one on this host.

**But the finding has a legitimate underlying signal.** A `assert "X" not in
out` negative sentinel is inherently weak: it passes vacuously unless something
guarantees `X` would otherwise appear. Here that guarantee is the zero-members
precondition (the `.nix` is diskless) -- which lives off-screen in the `.nix` and
is neither stated nor asserted in the `.py`. That is exactly why a careful
reviewer misread it. And it is genuinely fragile: if a discoverable braid-labeled
member ever appears (a disk added to this test's `.nix`), the sentinel goes
vacuous **silently** -- the same "passes silently" failure mode the existing
coupling comment (tracking `NoMembersDiscovered`'s wording) was written to
prevent on a different axis.

**Intended outcome:** resolve the real signal the way braid's own mutation-safety
heuristic prescribes -- "query the authoritative source of state directly; do not
pre-gate it with a cheaper but weaker observable." Convert the zero-members
precondition from an *unstated assumption* into an *observed, asserted fact*, so
the negative sentinel is primed by construction and any future discoverable
member fails loudly instead of silently retiring the guard. Document the whole
mechanism in the house Intent/Why/Scenario style the sibling subtest already
uses.

## Why this shape (not just a comment)

A comment documents the precondition but does not protect it. braid invests in
correctness over minimal diff (cf. the 221-line plan behind the one-line
`eprintln!` -> `print_cli_error` swap that introduced `NoMembersDiscovered`) and
guards explicitly against silent degradation. The strongest, in-character fix
adds a **positive baseline**: run the same command *without* contention, assert
it prints the empty-scan refusal, then assert that string is *absent* under
contention. That before/after differential is what actually proves the lock
gated the probe -- it is the negative sentinel's missing positive half.

Confirmed viable and side-effect-free (`cli/src/main.rs`, `Commands::Discover`
arm): on `--write`, `check_pool_json_for_bare_discover` is skipped (the
`!args.write` gate), the flow is probe -> `drain_warnings` -> `members.is_empty()`
-> `NoMembersDiscovered` + `exit(1)`, and `write_discovered_membership` (the only
pool.json / pending-op writer) is never reached. So the baseline invocation on
the diskless host writes nothing.

This baseline is a *same-host precondition guard*, not new empty-scan coverage.
The empty-scan refusal contract (exit 1, no-members message on stderr, no
preview, no pool.json write) is already pinned end-to-end by
`tests/cli/braid-discover-empty-scan.py` (registered in `flake.nix`) -- but that
is a *separate, dedicated VM*. The baseline here instead confirms that *this*
test's own host (`pool-lock-precedes-state-read.nix`) is diskless, so the
negative probe sentinel below is primed on the host it actually runs against. The
two are complementary: one pins the refusal behavior, the other proves this host
triggers it.

## The change (one subtest, documentation + one baseline assertion)

Edit only the first subtest in `tests/module/pool-lock-precedes-state-read.py`
("discover --write acquires before pending-op and probe reads"). Add a
house-style preamble and a no-contention baseline assertion ahead of the existing
contention case. Collapse the probe sentinel's substring into one local variable
used by both the baseline (asserted present) and the contention case (asserted
absent), and move the `NoMembersDiscovered` coupling comment onto that variable.
The pending-op sentinel and the contention assertions are unchanged.

Target shape:

```python
with subtest("discover --write acquires before pending-op and probe reads"):
    # Intent: under contention, discover --write must exit at the central lock
    #   acquire (cli/src/main.rs#acquire_per_policy, before the dispatch match)
    #   before it reads the pending-op journal or probes /dev/disk/by-id/.
    # Why: ADR 018 / principle 12
    #   (`docs/design/principles.md#12-one-pool-operation-at-a-time`) make lock
    #   acquire the serialization boundary. The two negative sentinels below each
    #   catch a DIFFERENT pre-lock leak, primed differently:
    #     - pending-op: primed by the planted placeholder journal -- a pre-lock
    #       read errors "pending-op.json exists".
    #     - probe: primed by this host discovering ZERO braid-labeled LUKS2
    #       members (the .nix is diskless; see pool-lock-precedes-state-read.nix).
    #       The baseline below proves that precondition by observation rather than
    #       assuming it, so a discoverable member appearing later fails here loudly
    #       instead of silently neutering the guard.
    # Scenario: external holder holds /run/braid-pool.lock; discover --write runs
    #   with a placeholder pending-op.json planted. Nonblocking flock fails fast
    #   with the contention message before either read.
    machine.succeed("mkdir -p /var/lib/braid")
    # Single source of truth for the probe sentinel's substring -- asserted
    # PRESENT in the baseline, ABSENT under contention. Tracks the lead clause of
    # cli/src/discover.rs#NoMembersDiscovered (not the remediation tail, which may
    # reword freely). Because the baseline asserts it PRESENT, a stale value here
    # -- renamed in NoMembersDiscovered but not updated -- now fails the baseline
    # loudly instead of silently retiring the negative sentinel.
    refusal = "no braid-labeled LUKS2 devices found"
    # Baseline (no contention): the probe runs, finds zero discoverable
    # braid-labeled LUKS2 members, and discover --write prints the empty-scan
    # refusal, exiting at the is_empty gate before writing anything. This positive
    # half is what makes the negative probe sentinel under contention meaningful.
    # --expect-count 0 keeps the baseline fail-closed against fixture drift: if a
    # discoverable member ever appears, write_discovered_membership refuses with
    # ExpectCountUnmet (count != 0) before save_membership -- so no pool.json is
    # written, and base_out carries that error instead of the refusal, tripping
    # the "precondition broken" assertion below rather than silently writing state.
    base_rc, base_out = machine.execute("braid discover --write --expect-count 0 2>&1")
    assert base_rc != 0, "baseline should exit nonzero (empty-scan refusal); out=" + base_out
    assert refusal in base_out, (
        "precondition broken: expected the empty-scan refusal without contention "
        "(did a discoverable braid-labeled LUKS2 member appear in the .nix?); "
        "out=" + base_out
    )
    machine.succeed("printf '{\"op\":\"placeholder\"}' > /var/lib/braid/pending-op.json")
    rc, out = with_holder("braid discover --write --expect-count 0")
    machine.succeed("rm -f /var/lib/braid/pending-op.json")
    assert rc != 0, "discover --write should fail under contention; out=" + out
    assert "another braid operation is already in progress" in out, (
        "expected contention message; out=" + out
    )
    assert "pending-op.json exists" not in out, (
        "discover read pending-op before acquiring lock; out=" + out
    )
    assert refusal not in out, (
        "discover probed devices before acquiring lock; out=" + out
    )
```

Notes:
- Use `machine.execute` (not `succeed`/`fail`) for the baseline: it exits 1, and
  `execute` matches the file's `(rc, out)` idiom. `2>&1` is required because the
  refusal goes to stderr via `print_cli_error`.
- The baseline runs before the holder is started, so its lock is acquired and
  released within that one process -- no interference with the contention case.
- Symbol references (`cli/src/main.rs#acquire_per_policy`,
  `cli/src/discover.rs#NoMembersDiscovered`) follow the AGENTS.md `path#symbol`
  convention; the ADR/principle link matches the sibling subtest.

## Out of scope

- **No production-code change.** The assertion under fire is sound; the
  `Commands::Discover` arm and `NoMembersDiscovered` are unchanged.
- **No change to the negative assertions or the contention assertions.** They
  stay; the baseline only adds the positive half.
- **No sibling-file sweep.** Exploration confirmed this is the only VM-suite
  negative sentinel primed by ambient/off-screen state; every other
  `... not in out` is explicitly primed in-line (planted journal, seeded
  `pool.json`, `--config /nonexistent`, command shape).
- **The second (FIFO) subtest is untouched** -- already sound and documented.

## Critical files

- `tests/module/pool-lock-precedes-state-read.py` -- the only file edited (first
  subtest: add preamble + baseline assertion).
- `tests/module/pool-lock-precedes-state-read.nix` -- read-only reference; named
  in the new comment as the home of the zero-members precondition (it is diskless).
- `cli/src/main.rs` (`Commands::Discover` arm) and
  `cli/src/discover.rs#NoMembersDiscovered` -- read-only references the comment
  cites; not modified.

## Verification

1. `just test-vm pool-lock-precedes-state-read` -- end-to-end. Confirms the new
   baseline assertion passes (diskless host prints the empty-scan refusal without
   contention) and all existing sentinels still pass. This is a comment +
   test-assertion change to one VM test with no production-code change, so blast
   radius is this single check; the unscoped suite is not required.
2. Manual one-off (optional, not committed) to prove the baseline is
   load-bearing: temporarily add an `emptyDiskImages` + braid-labeled disk to the
   `.nix`, rerun, and confirm the baseline fails with the "precondition broken"
   message -- and, because `--expect-count 0` makes `write_discovered_membership`
   refuse with `ExpectCountUnmet` before `save_membership`, that it writes no
   `pool.json` (fail-closed, not silently vacuous). Revert afterward.
3. No `just test-rust` / `mdbook build` needed: no Rust or docs change.
