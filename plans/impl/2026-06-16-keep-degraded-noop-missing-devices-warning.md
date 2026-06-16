# Plan: keep the degraded no-op missing-devices warning, document the rationale, pin it with tests

## Context

A testing finding (Low severity) observed that no regression test pins the note
output of a no-op `braid add` on a **degraded** pool. The existing no-op test
`no_journal_on_noop_add` (`cli/src/add.rs#no_journal_on_noop_add`) only checks
journal/inhibitor side effects, and its runner (`AddTestRunner`) cannot even
model a degraded pool. The only no-op *note* test
(`plan_add_already_in_pool_is_note_only_success`) runs on a healthy pool.

The finding proposed a test asserting the `pool has N missing device` warning is
**absent** on a degraded no-op. That assertion contradicts current code: the
warning is pushed at `cli/src/add.rs:1820` gated only on `pool.missing_count > 0`
(no `is_noop` guard), so today a degraded no-op renders **both** the warning and
the `Nothing to do` line. The finding's proposal therefore presumes an
undecided, separate behavior change (its referenced "finding 2": suppress the
warning on no-op). That test would fail on `master`.

**Decision (the pivot):** the warning is correct and stays. It is a *pool-health*
advisory about the pool's existing degraded state, categorically different from
the two *work-related* notes that are correctly no-op-gated (keyfile-asymmetry,
derived from work targets at `add.rs:1854`; and the RAID1 balance-skip note,
`!is_noop`-gated at `add.rs:1897`). On a degraded no-op the warning's
`braid replace` hint is exactly the guidance a confused operator needs, and
surfacing reduced redundancy aligns with the "never silently degraded" principle
(`docs/design/principles.md#1-resilient-by-default`). `add` is the only command that even reaches a
degraded no-op path (verified: `remove`/`remove-missing`/`replace` hard-reject in
degraded states or have no no-op; `enroll` emits only per-disk Skip notes), so
there is no cross-command convention to conform to.

The gating is principled but **undocumented**, which is why it keeps getting
flagged as an inconsistency. The intended outcome: write the rationale down at
the push site so the "why isn't this `is_noop`-gated like the other two?"
finding-class dissolves, and add the missing coverage in the *correct* direction
(warning present, balance-skip absent) -- a `plan_add`-level characterization
test plus a real-run no-op phase in the `braid-add-warnings` VM test.

## Changes

### 1. Document the intentional non-gating

File: `cli/src/add.rs`, the comment block above `if pool.missing_count > 0`
(currently lines ~1816-1824).

Expand the existing comment to state *why* the missing-devices warning is
intentionally **not** `is_noop`-gated, contrasting it with its two siblings.
Proposed comment (ASCII only, per project convention):

```rust
// Missing-devices warning: body-only, no legacy `warning:` prefix.
// Lives on `notes` so it surfaces on both dry-run stdout (via
// `Preview::render`) and real-run stderr (via `AddPlan::execute`
// using `preview::render_notes_for_stderr`).
//
// Intentionally NOT gated on `is_noop` -- unlike the keyfile-asymmetry
// warning (derived from `work_plan.targets`, so no-ops do not warn) and
// the balance-skip note (`!is_noop`-gated below). Those two describe the
// *work* (the new drive, the skipped balance step) and are meaningless
// when nothing is added. This warning describes the *pool's* existing
// health and is true whether or not work happens, so a degraded no-op
// re-add still surfaces it: the `braid replace` hint is the repair
// pointer an operator who ran `add` against a degraded pool needs, and
// staying quiet would run counter to "never silently degraded"
// (docs/design/principles.md#1-resilient-by-default). Pinned by the
// `plan_add_degraded_noop_keeps_missing_warning` unit test and the
// real-run no-op phase in tests/cli/braid-add-warnings.py.
if pool.missing_count > 0 {
    ...
}
```

### 2. Add the characterization test

File: `cli/src/add.rs`, immediately after `plan_add_degraded_preview_omits_balance_step`
(after the closing brace at ~line 9940, before the comment block at ~9942).

New test `plan_add_degraded_noop_keeps_missing_warning` (name adjustable). It
combines the already-in-pool harness from
`plan_add_keyfile_no_warn_when_target_already_in_pool_with_empty_slot_1` with the
`.with_missing(1)` knob from `plan_add_degraded_preview_omits_balance_step`. The
verified-constructible incantation:

```rust
let fixture = plan_add_fixture();
let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
let runner = AddPlanTestRunner::new()
    .with_keyfile_probes(vec![
        AddPlanKeyfileProbe::Occupied,
        AddPlanKeyfileProbe::Empty,   // len() must be >= 2 so disk2 is a real pool member
    ])
    .with_missing(1)
    .with_target_probe(
        "/dev/disk/by-id/virtio-disk2",
        AddPlanTargetProbe::AlreadyInPoolSlot1Empty,
    );
let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
let plan = plan_add(&runner, &fs, &fixture.params(&disk_specs, true))
    .expect("plan_add should succeed on a degraded no-op");
```

Assertions (reusing existing helpers `format_add_missing_devices_warning`,
`format_add_noop`, `format_add_degraded_balance_skip`):

- exactly one `PreviewNote::Warn`, equal to `format_add_missing_devices_warning(1)`;
- exactly one `PreviewNote::Info`, equal to `format_add_noop(&[DiskName::parse("disk2").unwrap()])`;
- `plan.preview().steps` is empty (no-op);
- rendered `plan.preview().render()` contains both the `[warn] pool has 1 missing device`
  line and the `Nothing to do -- disk2 already in pool.` line;
- rendered render does **NOT** contain `format_add_degraded_balance_skip()` --
  pins that the *work* note stays suppressed on a no-op while the *health*
  warning fires (this is the principled distinction the comment documents).

Open the test with the required `//` preamble per `docs/dev/testing.md`:
- **Intent:** a degraded no-op `braid add` surfaces the pool-health missing-devices
  warning AND the no-op Info line, but omits the work-only balance-skip note.
- **Why it exists:** guards the deliberate "health warning fires on no-op,
  work notes do not" split. A regression that `is_noop`-gates the missing-devices
  warning (i.e. implements the rejected "finding 2") or a note-ordering refactor
  that drops it must consciously update this test rather than silently going
  quiet about a degraded pool.
- **Scenario:** the pool is degraded (one member missing); the operator runs
  `braid add disk2` to repair it but disk2 is already a member, so the add is a
  no-op.

Scope of the `plan_add`-level test: it pins the note-accumulation gating --
`plan.notes` content, dry-run `Preview::render`, and the warning / no-op /
balance-skip split. It does NOT exercise the real-run operator path:
`AddPlan::execute` calls `emit_notes_to_stderr` at `add.rs:1047` BEFORE the no-op
early-return at `add.rs:1053`, and `emit_notes_to_stderr` writes to real stderr
via `eprint!` (`preview.rs:230`) with no capturable seam, so a Rust unit test
cannot assert that ordering. `add_warn_notes_render_canonical_bracketed_form`
only pins the renderer in isolation, not that `execute` calls it before
returning. Change #3 closes that gap end-to-end at the VM level.

### 3. Pin the real-run operator-visible path (VM test)

File: `tests/cli/braid-add-warnings.py` (registered as the `braid-add-warnings`
flake check, `flake.nix:229`).

That VM test already builds a 2-disk RAID1 pool (Phase 0), kills disk2 so the
pool is degraded (Phase 1), and asserts real-run `braid add disk3` stderr on the
degraded pool (Phase 3). Add a new subtest immediately after Phase 3 that re-adds
an already-in-pool, present disk on the still-degraded pool -- the real-run
**no-op**:

- run the real (non-dry-run) `add_cmd("disk1")` (disk1 has been a pool member and
  present since Phase 0; disk3 works too), capturing stdout/stderr to temp files
  as the other phases do;
- assert exit 0 and stdout empty (notes route to stderr on a real run);
- assert stderr CONTAINS the canonical missing-devices `[warn] pool has 1 missing
  device ...` line (reuse Phase 3's `expected_line` literal);
- assert stderr CONTAINS `Nothing to do -- disk1 already in pool.` (the no-op
  Info line, rendered bare);
- assert stderr does NOT contain the `[skip] pool: RAID1 balance skipped ...`
  line -- the operator-visible counterpart of Change #2's Rust assertion that the
  work note stays suppressed while the health warning fires;
- mirror Phase 3's hygiene guards: no legacy `warning:` prefix, no ANSI escape.

This pins that `AddPlan::execute` emits notes to real stderr BEFORE the no-op
early-return -- the operator path the unit test cannot reach. Open the subtest
with the Intent/Why/Scenario preamble per `docs/dev/testing.md`; the Why names
the regression guarded: moving the `is_noop` return above `emit_notes_to_stderr`,
or re-`is_noop`-gating the missing-devices warning, would make the real
`braid add` go silent about a degraded pool on the no-op path.

## Out of scope / explicitly rejected

- **Suppressing the warning on no-op (the referenced "finding 2").** Rejected: it
  removes the operator's repair pointer in the highest-stakes scenario, cuts
  against "never silently degraded", and carries note-ordering + preserved-context
  Err-path risk (the push would have to move below `build_add_work_plan`) for a
  debatable noise reduction. The new test's "Why it exists" preamble records this
  decision so a future reviewer re-raising it sees the rationale.
- Doc edit to `docs/commands/add.md`: optional polish only. It already says
  "Warns if the pool has missing devices"; a one-line clarification that this
  holds even for a no-op re-add could be added but is not required by this pivot.

## Verification

- `just test-rust` (or `cargo test -p braid plan_add_degraded`): the new test
  passes, and these existing tests still pass --
  `plan_add_degraded_preview_omits_balance_step`,
  `plan_add_already_in_pool_is_note_only_success`,
  `plan_add_keyfile_no_warn_when_target_already_in_pool_with_empty_slot_1`,
  `no_journal_on_noop_add`, `add_warn_notes_render_canonical_bracketed_form`,
  `plan_add_render_emits_warn_above_steps`.
- `just test-vm braid-add-warnings` (or
  `nix build .#checks.{system}.braid-add-warnings -L`): the new real-run no-op
  subtest passes alongside the existing Phases 0-4. Per AGENTS.md the NixOS VM
  checks run on macOS via `nix.linux-builder` (aarch64-darwin).
- Sanity check the test has teeth: temporarily flip the balance-skip assertion to
  `assert!(rendered.contains(...))` and confirm it fails (then revert); and
  temporarily delete the `if pool.missing_count > 0` push and confirm the
  warn assertion fails (then revert). Confirms the test exercises the real gates.
- `scripts/docs/check-output-ascii.py`: no new user-facing output strings are
  introduced (test + comment only; comments/tests are exempt anyway), but keep
  the new comment ASCII per `CLAUDE.md`.
- No `///` doc comment needed on the test fn (tests are exempt from the
  `pub`-item rule), but the `//` Intent/Why/Scenario preamble is mandatory.

## Implementation notes

- The current repo already had a degraded no-op characterization in `cli/src/add.rs` from the prior suppress-warning change, so the implementation converted that test to the pivot's warning-present contract instead of adding a parallel conflicting test.
