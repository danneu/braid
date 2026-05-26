# Document the ENOSPC advisory's count invariant + lock it with a test

## Context

A code-review finding claimed the ENOSPC risk advisory could print a
self-contradicting `"0 of N devices have less than ..."` message: it counts
devices against the larger pre-loss `current_threshold` while the 3+ disk
`at_risk` predicate triggers against each survivor set's smaller
`survivor_threshold` (`cli/src/capacity.rs:38-86`).

Investigation showed the contradiction is **mathematically impossible**, not
merely rare. Because `survivor_threshold <= current_threshold` (threshold is
monotonic in pool size) and any survivor set whose members are all
`>= current_threshold` has RAID1 chunk-pair capacity `>= current_threshold`,
the predicate can only fire when at least one device is below
`current_threshold`. So `at_risk` implies `count_below >= 1`. The feature's own
impl plan (`git show 57d1d78e:plans/impl/2026-05-23-raid1-enospc-risk-advisory.md`)
already documents this reasoning -- the threshold choice was deliberate.

The real gap is that this invariant lives only in a historical plan doc, not in
the code, so a careful reviewer reading `capacity.rs` re-derives the same worry
(as the finding demonstrates). And the existing 3-disk firing test never checks
the rendered count, so the invariant is not regression-locked for the 3+ disk
path.

**Outcome:** make the threshold choice read as deliberate and safe in-code, and
pin the non-zero count in a test. No behavior or output change.

## Non-goals

- No change to the advisory message text, trigger logic, or thresholds.
- Not addressing the separate (unraised) present-tense-vs-post-loss wording
  imprecision for 3+ disk pools -- that is a deliberate message redesign with
  doc + fixture blast radius, explicitly out of scope for this update.
- No doc (`docs/commands/status.md`) or VM-fixture changes; output is unchanged.

## Change 1: in-code invariant comment

File: `cli/src/capacity.rs`, at the `count_below` binding (currently lines
48-51).

Add a `//` comment immediately above `let count_below = ...` explaining why the
displayed count uses `current_threshold` while the predicate uses
`survivor_threshold`, and that the two can never disagree. Keep it ~6 lines,
self-contained (do not reference the plan doc -- `plans/impl/` is historical and
non-authoritative per AGENTS.md). Substance to capture:

- `count_below` is counted against the pre-loss `current_threshold`, not the
  per-survivor `survivor_threshold` the 3+ disk predicate uses below.
- Safe because `survivor_threshold <= current_threshold`, and any survivor set
  whose members are all `>= current_threshold` has chunk-pair capacity
  `>= current_threshold` (so `>= survivor_threshold`).
- Therefore `at_risk` can only fire when some device is below
  `current_threshold`; i.e. `at_risk` implies `count_below >= 1`, and the
  rendered `"K of N"` message never reads `"0 of N"`.

This is the artifact that dissolves future findings of this exact kind.

## Change 2: lock the count across a threshold gap in the existing firing test

File: `cli/src/capacity.rs`, test `enospc_risk_advisory_fires_on_3_disk_loss_simulation`
(preamble + fn, currently lines 242-258). Keep the test name; replace its
geometry in place -- do not add a new test.

Replace the current 3x100 GiB `[10 GiB, 10 GiB, 50 MiB]` geometry with three
4 GiB disks unallocated `[3 GiB, 3 GiB, 700 MiB]`. The 100 GiB case is a
same-threshold path -- its `current_threshold` and each 2-disk
`survivor_threshold` both pin to 1 GiB -- so it never exercises the
display-count-vs-survivor-threshold mismatch this test is meant to guard. The
4 GiB case still fires via loss simulation (serving the original intent) and
makes the two thresholds differ:

- 4 GiB disks, unallocated `[3 GiB, 3 GiB, 700 MiB]`; `current_total = 12 GiB`
  -> `current_threshold = min(1 GiB, 1.2 GiB) = 1 GiB`.
- Losing one 4 GiB disk -> `survivor_total = 8 GiB` ->
  `survivor_threshold = min(1 GiB, 8 * GIB / 10) = 858,993,459 bytes`
  (~819.20 MiB; the 10% term wins here, so the 1 GiB cap does not bind --
  this is exactly why it differs from `current_threshold`).
- Losing a 3 GiB disk -> survivors `[3 GiB, 700 MiB]`, chunk-pair capacity
  `700 MiB < ~819.20 MiB` (the survivor threshold) -> `at_risk` fires.
- `count_below` counts devices `< current_threshold` (1 GiB): only the 700 MiB
  device qualifies -> `count_below == 1`; the rendered byte value stays
  `format_bytes(current_threshold) = "1.00 GiB"`.

Then:

- Add an assertion that the rendered message contains the non-zero count:
  `assert!(advisories[0].contains("1 of 3 devices"));` alongside the existing
  `len() == 1` / `starts_with("ENOSPC risk:")` checks.
- Rewrite the `//` preamble (Intent/Why/Scenario) to the new 4 GiB scenario and
  to state it now pins the rendered count across a `current_threshold` (1 GiB)
  vs `survivor_threshold` (~819.20 MiB) gap -- guarding the `count_below >= 1`
  invariant (a firing advisory never reports `"0 of N"`).

This geometry is the firing mirror of the existing non-firing test
`enospc_risk_advisory_uses_survivor_threshold_not_pre_loss`
(`cli/src/capacity.rs:281-289`): same three 4 GiB disks, where `700 MiB` fires
and `900 MiB` stays silent, bracketing the ~819.20 MiB (`8 * GIB / 10` =
`858,993,459 bytes`) survivor threshold from both sides. Keep the two tests
adjacent and coherent.

Reuse the existing `device(...)` helper and `GIB`/`MIB` consts already in the
module (`cli/src/capacity.rs:92-105`).

## Verification

- `just test-rust` -- runs the CLI crate unit tests, including the strengthened
  `enospc_risk_advisory_fires_on_3_disk_loss_simulation`. This is the only
  validation needed: the comment is non-functional and no output, parser, or
  systemd behavior changes, so no VM tests or fixture refresh apply.
- Do not run `cargo fmt` / `just fmt`; keep the diff to the two narrow edits.
