# Plan: make the `braid idle` sysfs-exclop coverage discoverable

## Context

A testing finding claimed there is "no end-to-end VM coverage that `braid idle`
itself exits 1 during a btrfs kernel exclusive operation," and proposed adding a
live-`btrfs balance` subtest to `tests/cli/braid-idle.py`.

Investigation showed the **headline claim is false**. `tests/cli/replace-inhibits-suspend.py`
(subtest "braid idle returns promptly and detects in-flight replace via sysfs",
the Phase 3a block) already drives `braid idle` -> exit 1 through the sysfs
`exclusive_operation` branch against a **live kernel**: a `btrfs replace` is a
member of the kernel exclop set (`BTRFS_EXCLOP_DEV_REPLACE`), and the test
asserts `idle_exit == 1` plus `"device replace"` in the output. That string can
only originate from `cmd_idle` step 2 -> `IdleResult::Busy(BusyReason::Exclop(DeviceReplace))`
-> `main.rs` -> `busy: device replace in progress`. The test is registered in
`flake.nix` and runs in CI. Git history confirms the pairing was deliberate:
commit `64232791` ("detect every btrfs exclusive op in `braid idle`") added the
detection, and `185a7f10` + the replace Phase 3a landed as its live coverage.

The busy arm of `cmd_idle` is **op-agnostic**
(`Err(ExclusiveOpError::Busy(op)) => IdleResult::Busy(BusyReason::Exclop(op))`,
`cli/src/idle.rs`), so a balance subtest would re-exercise the *same* live branch
the replace test already covers; the only per-op deltas (parser, `Display`,
mapping) are already pinned for all seven ops by the `idle.rs` unit tests
`busy_exclop_short_circuits_scrub_probe` and `busy_reason_display_pins_cli_strings`.

So the proposed VM subtest is **near-redundant and slow** (a balance needs its own
dm-delay *write*-throttle cycle + large payload), and adds permanent CI
wall-clock for ~zero marginal regression-catching value. The real problem the
finding surfaced is **discoverability**: the canonical live proof lives in a file
named for inhibitor behavior, so an auditor reading `braid-idle.py` concludes the
branch is uncovered and re-files this same finding.

**Intended outcome:** make the existing coverage self-documenting so this false
"gap" cannot recur, without adding any redundant test. Documentation-only.

## The change

Three cross-reference comments forming a self-documenting triangle, so the
coverage is discoverable from whichever file a future reader/auditor opens, and
the load-bearing assertion is protected from deletion. No code or test-runtime
changes.

### (a) `tests/cli/braid-idle.py` -- the audit surface (essential)

Add a "Coverage boundary" note to the top-of-file preamble, immediately after the
`Scenario:` block (before the blank line preceding `import base64`). This is
where someone auditing "what exit-1 paths does idle cover" looks -- and where the
finding's author looked. Match the file's `#   ` continuation indent and `--`
dashes:

```python
# Coverage boundary: the exit-1 cases here are the root gate, a forced probe
#   failure, and a live scrub. The sysfs exclusive-operation branch of cmd_idle
#   (step 2 -> Busy(Exclop) -> exit 1) is proven end-to-end against a live
#   kernel by tests/cli/replace-inhibits-suspend.py, where an in-flight `btrfs
#   replace` (a kernel exclop) makes `braid idle` exit 1. That branch is
#   op-agnostic, so a balance-specific subtest would be redundant; the per-op
#   parse/Display/mapping deltas are pinned by cli/src/idle.rs unit tests
#   (busy_exclop_short_circuits_scrub_probe, busy_reason_display_pins_cli_strings).
```

### (b) `cli/src/idle.rs` -- reciprocal pointer (recommended)

Append one sentence to the existing doc-comment block directly above
`fn busy_exclop_short_circuits_scrub_probe` (the last line before `#[test]`), so a
Rust dev reading the structure-level test finds its live counterpart. ASCII (it
is also inside `mod tests`, which `check-output-ascii.py` skips):

```rust
    // Live end-to-end counterpart: tests/cli/replace-inhibits-suspend.py drives
    // this branch through a real `btrfs replace` exclop -> `braid idle` exit 1.
```

### (c) `tests/cli/replace-inhibits-suspend.py` -- protect the load-bearing assertion (recommended)

Add a sentence to the existing Phase 3a comment block (the "Why it exists"
paragraph, around the `braid idle returns promptly...` subtest) flagging that the
exit-1 / "device replace" assertions double as the canonical live proof of the
idle exclop branch, so a future editor does not weaken or delete them thinking
they are redundant with the inhibitor checks:

```python
# This subtest is also the canonical live proof of the `braid idle` exit-1
# exclusive-operation branch (cmd_idle step 2); tests/cli/braid-idle.py points
# here for that path. Do not weaken the exit-1 / "device replace" assertions
# below without relocating that coverage.
```

## Out of scope (explicitly rejected)

- **The finding's proposed balance VM subtest in `braid-idle.py`.** Rejected: it
  re-exercises the op-agnostic live branch already covered by the replace test;
  its only deltas are pure functions already unit-tested for all seven ops; and
  it would add a slow dm-delay write-throttle cycle to CI forever. This conflicts
  with the project's demonstrated preference for de-duplicated test logic (e.g.
  the `balance_helpers.py` extraction).
- **Any behavior/ADR/doc change.** No runtime behavior changes; ADR 016
  (auto-suspend) and `idle.md` already document the any-busy semantic and exit
  codes correctly.

## Files modified

- `tests/cli/braid-idle.py` (preamble comment)
- `cli/src/idle.rs` (one comment line in `mod tests`)
- `tests/cli/replace-inhibits-suspend.py` (one sentence in the Phase 3a comment)

## Verification

This is a documentation-only change with no runtime effect, so verification is
about reference accuracy, not behavior:

1. **References resolve:** confirm `tests/cli/replace-inhibits-suspend.py` still
   contains the subtest asserting `idle_exit == 1` and `"device replace"` in
   output, and that `cli/src/idle.rs` still defines
   `busy_exclop_short_circuits_scrub_probe` and `busy_reason_display_pins_cli_strings`.
   (All confirmed present at plan time.)
2. **Rust still builds (comment sanity):** `just test-rust` -- comments cannot
   change behavior, this only confirms no accidental syntax damage in `idle.rs`.
3. **No ASCII regression:** the `idle.rs` comment is plain ASCII (and test-gated,
   so `scripts/docs/check-output-ascii.py` skips it regardless).
4. **No Python tooling to satisfy:** the repo has no ruff/black/flake8 config, so
   there is no formatter to re-run on the `.py` edits.

A reviewer confirms correctness by reading the three touch points; running the
slow VM lanes is unnecessary because nothing executable changed.
