# Plan: document why unlock re-probes the LUKS header at failure time

## Context

A code-review finding (Low/Simplicity) claimed that `explain_open_failure`'s
`probe_luks_header` calls in `open_disks_with_credential` are redundant with
the planner's probing, and that the planner's classification should be threaded
in instead -- and separately, that two header-damage classifiers
(`ConfigDiskState`/`MissingReason` vs `LuksHeaderState`) exist and can drift.

Verification showed both premises are wrong, and the proposed "fix" would be a
correctness regression:

- The disks reaching `open_disks_with_credential` are exactly the `to_unlock`
  disks, which `plan_open_pool` classified as `ConfigDiskState::PresentLuks`
  (header intact -- both `luksUuid` and `luksDump` succeeded at plan time). The
  planner's own `probe_luks_header` call (`mount.rs:259`) fires **only** for
  `PresentNotLuks` disks, which go to `missing` and never reach unlock. So the
  planner holds no damage observation to thread in; threading plan-time state
  would feed `LuksHeaderState::Ok` every time and dead-code the damage guidance.
- The re-probe is load-bearing: the header can change between plan and open
  (external `dd`, hardware fault, swapped device in the credential window). The
  feature commit is literally `eaeec7c feat(unlock): probe LUKS headers on
  open-failure to stop misdiagnosis`, and test
  `unlock_passphrase_verify_exit_1_unreadable_header_emits_guidance`
  (`mount.rs:3764`) pins the "header fine at plan, wiped by the time we open"
  scenario.
- There is exactly **one** header-damage classifier: `probe_luks_header` ->
  `LuksHeaderState`. `MissingReason`'s damage labels are *derived* by calling
  it; `ConfigDiskState` is a separate, deliberately coarse membership gateway
  with no damage variants. No second classifier exists to drift against.

The finding is real evidence of one thing: this rationale lives only in test
preambles, not in the code a reviewer reads, and not in the internals docs. The
three re-probe callsites are bare calls with zero inline comment. **Outcome:**
add documentation -- code comments + an internals-doc subsection -- so future
readers see the re-probe is intentional and the classifier is singular,
dissolving both misreadings. No behavior change.

## Scope

Documentation only: Rust doc/inline comments + one mdBook subsection. Zero code
logic changes, zero behavior changes, no new tests.

## Changes

### 1. `cli/src/mount.rs` -- failure-time re-probe rationale

**1a. Extend the `open_disks_with_credential` doc comment** (currently
`mount.rs:506-508`, the 3-line "Keeps header-state classification shared..."
block). Append a paragraph stating the failure-time-re-probe contract:

- On any verify-rejection or open failure it re-probes the header at failure
  time rather than reusing plan state.
- The `to_unlock` disks were `PresentLuks` (header intact) at plan time, so the
  planner holds no damage observation to thread in; its `probe_luks_header`
  call only runs for `PresentNotLuks` disks that go to `missing`.
- The header can change between plan and open; the re-probe captures fresh
  state so `explain_open_failure` emits damage guidance instead of a misleading
  "wrong passphrase".
- Cross-reference `docs/internals/luks-unlock.md`.

**1b. Add two short inline breadcrumbs** at the re-probe regions so the
rationale is visible at the call, not just on the function:

- One comment above the `match verify_credential_for_targets(...)` block
  (covers the two re-probe arms at `mount.rs:527` and `:541`).
- One comment at the open-loop `Err(e) =>` arm (`mount.rs:574`).

Each is 1-2 lines and points to the function doc, e.g. "Re-probe at failure
time: plan-time state for these `PresentLuks` disks is always healthy, so the
live header is the only available diagnosis -- see fn doc." Keep the full
rationale in one place (1a); the breadcrumbs are pointers, not duplicates.

### 2. `cli/src/luks.rs` -- single-classifier clarifier

**Extend the `LuksHeaderState` doc comment** (`luks.rs:649-655`). Add ~3 lines
stating that this enum is the single source of header-damage classification
(doctor, status, TUI, and unlock's failure path all consume it), and that
`ConfigDiskState` is a separate coarse membership gateway (Absent /
PresentNotLuks / PresentLuks) carrying no damage variants -- so there is no
second classifier to drift against. This builds on the existing rationale at
`mount.rs:253-258` ("do NOT propagate this back into ConfigDiskState").

### 3. `docs/internals/luks-unlock.md` -- new subsection

**Insert a `## Open-failure header diagnosis` section** immediately after the
"Header backup workflow and messaging" section (after `luks-unlock.md:145`,
before `## Unparseable state-file reconciliation` at `:147`). This is the right
neighbor: that section governs *what* the unlock recovery messages say; the new
one explains *how/when* unlock selects them. Content (short, conceptual --
the durable maintainer-facing home, kept tight to avoid drift):

- The two-phase model: `plan_open_pool` probes, then
  `execute_unlock_and_mount` verifies + opens; on failure
  `open_disks_with_credential` re-probes and routes through
  `explain_open_failure` (Damaged -> repair, Unreadable -> off-system-backup
  per the messaging invariant above, Ok -> original error verbatim,
  ProbeFailed -> diagnosis incomplete).
- Why the re-probe is deliberate, not redundant: `to_unlock` disks were
  `PresentLuks` (intact by construction), so there is nothing to reuse; the
  header can change in the plan->open window, and the failure-time probe is
  what prevents a "wrong passphrase" misdiagnosis of a wiped header.
- One line: `probe_luks_header` -> `LuksHeaderState` is the single
  header-damage classifier; `ConfigDiskState` is a separate coarse gateway, so
  the two neither duplicate nor drift.

Keep references in-prose / within-file (e.g. "the messaging invariant above")
to avoid introducing new cross-file links. No `docs/SUMMARY.md` change -- this
is a subsection within an existing page, not a new page.

## Files

- `cli/src/mount.rs` -- doc comment + 2 inline comments (no code change).
- `cli/src/luks.rs` -- doc comment on `LuksHeaderState` (no code change).
- `docs/internals/luks-unlock.md` -- one new subsection.

## What NOT to do

- Do **not** thread the planner's per-disk state into
  `execute_unlock_and_mount`. That is the finding's proposed fix and it would
  dead-code the Damaged/Unreadable/ProbeFailed guidance -- a correctness
  regression. The plan exists to document why that approach is wrong.
- Do not touch the test preambles (`mount.rs:2472-2569`, `:3689-3763`); they
  already articulate the rationale and the new comments mirror, not replace,
  them.
- Do not add header-damage variants to `ConfigDiskState`.

## Verification

1. `just test-rust` -- confirms the crate still compiles (doc comments are part
   of compilation) and no test asserts against text we changed. Expected: pass,
   unchanged.
2. `nix develop .#docs -c mdbook build docs` -- builds the unified docs and runs
   `mdbook-linkcheck`; a broken cross-link fails. The build must run inside the
   flake docs shell: `mdbook` and `mdbook-linkcheck` are provided only by
   `devShells.<system>.docs` (flake.nix:112-125), not the default workspace
   PATH, so a bare `mdbook build docs` would fail before checking the change.
   Expected: pass (new content uses in-prose / within-file references only).
3. `just check-docs` (complementary; runs the repo's SUMMARY parity,
   link-escape, and table-parity gates). Expected: pass unchanged -- the change
   adds a subsection to an existing page, so it touches no SUMMARY entry, doc
   table, or cross-file link.
4. `git diff` read-through: confirm the diff is comments + one markdown
   subsection only, no logic lines touched.

No VM or parser-canary runs needed: this change has zero behavioral blast
radius (comments + prose), so `just test-vm` / `just test-parsers` are not
required.
