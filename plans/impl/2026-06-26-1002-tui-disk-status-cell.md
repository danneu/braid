# Plan: document the TUI disk-table status cell

## Context

`docs/commands/tui.md` is the page the README and `docs/index.md` point operators
to for the `braid tui` dashboard. Its "Disk table" bullet
(`docs/commands/tui.md`, "## What it shows" section) describes only the happy-path
columns of a row -- number, name, bus, SMART, temperature, btrfs errors, allocated.
It says nothing about the **status cell** the TUI renders in place of the
allocation columns when a declared disk is *not* assembled into the live pool.

`cli/src/tui/view/mod.rs#unpooled_disk_status_cell` renders seven distinct
status strings for those rows. Decision 024 introduced `offline` as a
first-class, cause-neutral disk state and its "Offline Disk State" section says
status *and TUI* surfaces render it -- but the TUI command doc was never kept in
sync. The result: an operator who sees a red `uuid mismatch` cell (the exact
swap/clone signal decision 024 exists to surface) has no explanation in the one
doc the README sends them to.

Two of the seven states -- `mapper conflict` and `LUKS<v> (unsupported)` -- are
**TUI-only as disk rows**. `braid status`'s `DiskStatus` enum has just six
variants and never renders them as a disk *state*: a hijacked mapper and a LUKS1
header surface in `status` only as a **config-disk probe-fault advisory**
(`status.md#advisories`: "a `braid-<name>` mapper hijacked ... a LUKS1 header"),
and `braid doctor`'s `check_declared_disks` does not probe the mapper or LUKS
version at all (`cli/src/doctor.rs`'s `DiskState` enum has no mapper-conflict or
wrong-version variant). So in the TUI an operator sees a `mapper conflict` /
`LUKS<v> (unsupported)` *cell* whose meaning is explained nowhere as the cell
itself -- only obliquely, as a status advisory.

Intended outcome: tui.md gains a terse, surface-specific legend mapping every
on-screen status string to a one-line meaning, cross-linking the authoritative
detail rather than duplicating it.

## The change

Single file: `docs/commands/tui.md`. Add one new bold-lead bullet immediately
**after** the existing `**Disk table**` bullet and **before** the `**Fans**`
bullet, matching the section's `**<Title>** -- <description>` style.

### Why list all seven inline (not cross-link the shared five)

The TUI cell is a glance-legend: the operator needs every exact on-screen string
glossed in one place. The TUI strings also differ in spelling from `status`'s
(`uuid mismatch` here vs `LUKS UUID MISMATCH` / `luks-uuid-mismatch` in
`status`), and color is a real TUI signal. Glosses are kept to one terse line
each and defer the fuller descriptions to their authoritative homes -- the five
shared states to `braid status`'s per-disk detail, the two probe-fault states to
its Advisories section, and recovery to `braid doctor` only for the states it
actually diagnoses -- so the authoritative description stays single-sourced (same
relationship `status.md` already has with decision 024). This is a
surface-specific rendering legend, not a second source of truth.

### Proposed wording (implementer may refine; keep strings verbatim)

```markdown
**Disk status cell** -- when a declared disk is not assembled into the live pool,
its row drops the allocation columns and shows a status cell instead. Cell color
reflects severity: red marks identity, header, or mapper faults that need
attention now; yellow marks the remaining diagnostic states, which can still need
follow-up -- `missing` in particular means a member is absent and the pool may be
running degraded. The five states `braid status` also reports per-disk (`missing`,
`offline`, `unknown`, `uuid mismatch`, `LUKS header unreadable`) carry fuller
descriptions under
[`braid status` per-disk detail](status.md#per-disk-detail), with an `Action:`
hint where applicable (`missing`, `uuid mismatch`, `LUKS header unreadable`;
`offline` and `unknown` get none); the two TUI-only probe faults
(`mapper conflict`, `LUKS<v> (unsupported)`) appear there only as
[config-disk probe-fault advisories](status.md#advisories). Run `braid doctor`
to diagnose the states it covers -- `uuid mismatch`, `LUKS header unreadable`,
`offline`, and `missing`.

  - `missing` (yellow) -- the device is absent at its by-id path.
  - `offline` (yellow) -- present and LUKS-identity-verified, but not assembled
    into the live pool. Cause-neutral (a locked member of a degraded mount, an
    interrupted post-commit step, etc.); see
    [decision 024](../design/decisions/024-luks-uuid-identity.md#offline-disk-state).
  - `unknown` (yellow) -- braid could not classify the disk's state.
  - `uuid mismatch` (red) -- the on-disk LUKS UUID differs from the recorded
    member: the disk was swapped, cloned, or reformatted. Run `braid doctor` for
    the expected vs observed UUID.
  - `mapper conflict` (red) -- the `braid-<name>` device-mapper node is open for
    the wrong backing device or LUKS UUID. Close it and unlock again.
  - `LUKS header unreadable` (red) -- the device is present but its LUKS header
    could not be read or validated.
  - `LUKS<v> (unsupported)` (red) -- the device holds a LUKS header of the wrong
    version (e.g. `LUKS1`; braid requires LUKS2). Back up its data and re-add the
    disk.
```

### Correctness anchors for the implementer

- **Labels: six fixed strings + one versioned pattern.** Six cells render fixed
  strings (`missing`, `offline`, `unknown`, `uuid mismatch`, `mapper conflict`,
  `LUKS header unreadable`); `WrongLuksVersion(v)` renders the pattern
  `format!("LUKS{v} (unsupported)")`, so the doc lists it as `LUKS<v> (unsupported)`
  (the displayed `<v>` is the detected version, e.g. `LUKS1`), not a fixed string.
  Keep all labels and their colors verbatim against
  `cli/src/tui/view/mod.rs#unpooled_disk_status_cell`; the unit test
  `unpooled_disk_status_cell_renders_each_variant` pins each, using
  `WrongLuksVersion(1)` -> `LUKS1 (unsupported)` as the version exemplar.
- **Gloss semantics** mirror the `UnpooledDiskRender` variant doc comments in
  `cli/src/tui/model.rs` (e.g. `MapperHijacked` recovery = "close the mapper and
  unlock again"; `WrongLuksVersion` recovery = re-add via `braid add`) and the
  `offline` cause-neutral wording in
  `docs/design/decisions/024-luks-uuid-identity.md#offline-disk-state`.
- **Cross-link anchors (verified to exist; mdbook-linkcheck2 will fail CI if
  wrong):**
  - `status.md#per-disk-detail` (the `### Per-disk detail` section holding the
    disk-states table) -- for the five shared states.
  - `status.md#advisories` (the `### Advisories` section, whose config-disk
    probe-fault paragraph documents the mapper hijack and LUKS1 header) -- for
    the two TUI-only states.
  - `../design/decisions/024-luks-uuid-identity.md#offline-disk-state` (relative
    path form already used by `docs/commands/doctor.md`) -- for `offline`.
- **`braid doctor` scope (do not over-promise):** for the TUI's status-cell
  states, `check_declared_disks` (`cli/src/doctor.rs`, via `classify_disk_state`)
  diagnoses `offline`, `uuid mismatch` (`LuksUuidMismatch`), `missing`, and
  `LUKS header unreadable`. (Its `DiskState` enum has other variants --
  `LuksHeaderOk`, `NotBlock`, `ProbeFailed` -- that don't map to TUI cells.) It
  has no mapper-conflict or wrong-version variant and does not probe the mapper or
  LUKS version, so doctor guidance must not be attached to `mapper conflict` or
  `LUKS<v> (unsupported)`.
- **ASCII only** per project convention: ` -- ` separators, plain quotes, no
  Unicode dashes. Wrap each literal string in backticks so `<name>` / `<v>`
  render literally and aren't parsed as HTML tags.

## Out of scope (deliberate)

- **No `README.md` / `SUMMARY.md` change.** This is a doc-only elaboration with no
  behavior change; the README already says the TUI shows "disk status," and no
  page is added. (AGENTS.md's README-sync rule triggers on behavior changes.)
- **No code change to unify the surfaces.** The TUI renders `mapper conflict` /
  `LUKS<v> (unsupported)` as first-class per-disk *cells*, whereas `braid status`
  surfaces the same faults only as advisories and `braid doctor` does not probe
  for them at all (the TUI's `probe_config_disk` probes more than `status`'s /
  `doctor`'s classifiers). Whether to unify them is a separate design question;
  this finding is a doc gap, and widening it to a code refactor would be
  speculative. Noted here only so the divergence is on record.
- **No doc-vs-code string test.** No such harness exists; pinning seven prose
  glosses to code would be brittle and disproportionate. The existing
  `unpooled_disk_status_cell_renders_each_variant` test plus the labels rule
  above are the safeguard.

## Verification

1. `just docs-build` -- runs mdbook + `mdbook-linkcheck2`; confirms all three new
   cross-links (`status.md#per-disk-detail`, `status.md#advisories`, and the
   decision-024 offline anchor) resolve. A broken link fails the build.
2. Manual diff: the six fixed labels in the new bullet (`missing`, `offline`,
   `unknown`, `uuid mismatch`, `mapper conflict`, `LUKS header unreadable`) match
   `cli/src/tui/view/mod.rs#unpooled_disk_status_cell` verbatim; the seventh is
   the versioned pattern `LUKS{v} (unsupported)` (test exemplar
   `LUKS1 (unsupported)`), not a literal `LUKS<v>` string.
3. Eyeball the rendered page section ordering: the new `**Disk status cell**`
   bullet sits between `**Disk table**` and `**Fans**` and reads in the same
   bold-lead style as its neighbors.

(Note: `braid tui --demo` shows three healthy fake disks and does **not**
exercise these unpooled states, so it is not a useful check here.)
