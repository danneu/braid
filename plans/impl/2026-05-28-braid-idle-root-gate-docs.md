# Plan: clarify root-gate exit in `braid idle` docs

## Context

The exit-code table at `docs/commands/idle.md:30-36` claims:

| Exit code | Meaning |
|---|---|
| **0** | Pool is idle, or pool is offline |
| **1** | Pool is busy (running op) or pool state could not be determined |
| **2** | Setup error -- config could not be read |

But `cli/src/main.rs:476-485` exits 1 with stderr `error: braid must be
run as root (try: sudo braid ...)` *before* config loading or any
mount/sysfs/scrub probe runs when invoked without root. `Idle` falls in
the `_ => true` arm of the `needs_root` match, and `load_config_or_exit`
runs inside the `Commands::Idle =>` arm at `main.rs:787` -- after the
gate -- so `braid idle --config /tmp/missing.json` as non-root exits 1
with the root-gate stderr, not 2 with the documented config-read
classification.

In production this never fires -- autosuspend's `ExternalCommand`
daemon runs as root, and `docs/design/decisions/016-auto-suspend.md`'s
inversion table only documents the production path. The audience that
hits it is a human or script author debugging without `sudo`. The
stderr message identifies the cause, but the bare exit code reads as
"pool busy or undetermined" under the current table, which is
misleading.

This is the one braid command whose exit codes are script-consumed
(autosuspend, custom wait loops), so precision in this one doc pays
off. Peer command docs (`lock.md`, `doctor.md`, `unlock.md`, ...) signal
the root requirement implicitly via `sudo` in examples and do not have
detailed exit-code tables the gate could contradict, so they are out
of scope.

## Change 1: doc note

**File:** `docs/commands/idle.md`
**Location:** After the existing `## Exit codes` block (i.e. after the
`When the pool is offline ...` line at `idle.md:61`, before the
`## Autosuspend integration` heading at `idle.md:63`).

Add a single short paragraph in the same prose-after-table style the
section already uses for the busy-reason and pool-offline clarifications:

```markdown
`braid idle` must run as root. A non-root invocation exits 1 with
`error: braid must be run as root` on stderr before config loading or
any probe runs, with no stdout output. The streams disambiguate this
from the documented exits above: exit 0 prints `idle:` on stdout,
busy/probe-failure exit 1 prints `busy:` on stdout, and config-load
exit 2 emits a config-error diagnostic.
```

No code change. No change to `docs/design/decisions/016-auto-suspend.md`
(the inversion table there describes the production autosuspend path,
which always runs as root; documenting the gate there would be
misleading scope).

## Change 2: regression test for the root-gate contract

**File:** `tests/cli/braid-idle.py`

The doc note codifies dispatch-level behavior that lives in `main.rs`,
outside `cmd_idle`. Without a test, a future refactor of the root gate
(removing it for `Idle`, moving config loading ahead of it, swapping
the stderr message) could stale the new docs without failing any test.
Add a focused subtest that pins the contract.

**Placement:** Immediately after the existing
`braid idle exits 2 on config-load failure ...` subtest (around line 41
of the current file), before the `Create 2-disk RAID1 pool` block. This
groups the three pre-pool exit-code subtests together (offline,
config-load, non-root) and avoids depending on pool setup, since the
root gate fires before any pool state is observed.

**Behavior to assert** (using `runuser -u nobody --` from util-linux,
which is in the base NixOS image; `nobody` is a default system user).
Streams must be captured *separately* so a regression that swapped the
root-gate message from stderr to stdout would fail the test -- the doc
contract is specifically about stream routing, not just the exit code:

- Run via `machine.execute(...)` with explicit redirection:
  `runuser -u nobody -- braid idle >/tmp/idle.stdout 2>/tmp/idle.stderr`.
- Assert exit status is 1.
- Read both files (`machine.succeed("cat /tmp/idle.stdout")` and
  `... idle.stderr`) and assert:
  - stderr contains the literal `error: braid must be run as root`.
  - stdout contains neither `idle:` nor `busy:` (the gate fires before
    `cmd_idle` runs, so no operational classification is emitted).
- Clean up the two `/tmp/idle.*` files at the end of the subtest so
  later subtests start from a known state.

This is a tighter capture pattern than the existing `2>&1`-style
`machine.execute(...)` calls in this file. The merged-stream pattern
is fine for subtests that only check exit code and a substring match
(e.g. the config-load subtest at line 30, where any stream carrying
the diagnostic is acceptable); the non-root subtest is stricter
because the doc paragraph it pins explicitly promises stream routing.

**Nix node:** No change to `tests/cli/braid-idle.nix` -- `runuser` and
the `nobody` user are present in the default NixOS toolchain; the
existing `environment.systemPackages` list does not need additions.

**Test preamble update:** Extend the `# Intent:` and `# Scenario:`
lines of the test header to mention the non-root path, in keeping with
braid's test-preamble convention
(see `docs/dev/testing.md`).

## Non-changes (explicit)

- **Do not expand the exit-1 row.** The table classifies operational
  outcomes; the root gate is an infrastructure gate that fires before
  probing. Folding it into the row blurs the semantic.
- **Do not touch peer command docs.** The root requirement is
  project-wide, not idle-specific. Documenting it per-command in this
  PR would invent a doc structure braid doesn't have. If a future
  change wants a central "all braid commands require root" note, that
  is a separate effort.
- **Do not touch ADR 016.** Its inversion table is scoped to the
  autosuspend daemon path (always root). Adding the root-gate case
  would imply it is a production concern, which it is not.

## Verification

- `just test-vm braid-idle` passes (covers all existing exit-code
  subtests plus the new non-root subtest pinning the doc claim).
- `mdbook build docs` succeeds (validates Markdown and cross-links via
  `mdbook-linkcheck`).
- Visual check that the new paragraph renders below the exit-code
  table and above the "Autosuspend integration" heading, in the same
  prose style as the surrounding clarifications.
- No code touched; no other `just test-*` runs required (the gate
  itself is exercised end-to-end by `braid-idle`).
