# Plan: inline the single-variant `MissingReason::is_luks_header_state` predicate

## Context

`cli/src/mount.rs` defines `MissingReason` (two variants: `Unplugged`,
`LuksHeaderUnreadable`) and a one-method `impl` block exposing
`is_luks_header_state(self) -> bool`, whose body is
`matches!(self, MissingReason::LuksHeaderUnreadable)`. Its only caller is the
doctor-footer gate in `format_degraded_refused`
(`cli/src/mount.rs#format_degraded_refused`).

This method is residue from a refactor, not an intended abstraction:

- Introduced in `15186f05` covering **two** variants --
  `matches!(self, LuksHeaderUnreadable | LuksHeaderDamaged)` -- a real
  "is this any LUKS-header-problem state?" category that earned its name.
- `a9ab6d51` ("collapse the unreachable Damaged header state") deleted the
  `Damaged` variant and rewrote the body to the single-variant form, but left
  the method and its category name in place.

The name now implies a "header state" category with exactly one member, inviting
a future reader to assume more variants belong to it. The collapse commit
deliberately declared `Damaged` unreachable, so there is no anticipated
re-expansion to preserve the abstraction for.

**Outcome:** make the single-variant reality explicit at the callsite, delete
the misleading named boundary, and purge the two active comments that still name
it -- all with zero behavioral change.

## Change

Four edits across two files (`cli/src/mount.rs`, `tests/cli/braid-unlock.py`).
Edits 1-2 are the code change; edits 3-4 retire the two comments that still name
the deleted predicate, so the scoped active-source search in Verification step 2
comes back clean (committed `plans/impl/` records keep their historical
references untouched).

1. **Inline the predicate at the footer gate.** Replace the method call with the
   `matches!` it wraps:

   ```rust
   if missing.iter().any(|(_, r)| matches!(r, MissingReason::LuksHeaderUnreadable)) {
       lines.push("run 'braid doctor' for recovery guidance".to_owned());
   }
   ```

   `r` is `&MissingReason`; the unit-variant pattern matches through the
   reference via Rust match ergonomics, so this typechecks without a deref.

2. **Delete the whole `impl MissingReason` block** (the method is its only
   member):

   ```rust
   impl MissingReason {
       fn is_luks_header_state(self) -> bool {
           matches!(self, MissingReason::LuksHeaderUnreadable)
       }
   }
   ```

   The enum (ending at the `LuksHeaderUnreadable` doc comment) is then followed
   directly by the `format_degraded_refused` doc comment.

3. **Retire the stale name in the `braid-unlock.py` composition-pin comment**
   (`tests/cli/braid-unlock.py`, in the degraded-refusal footer test). The
   comment narrates the live per-disk reason text and the footer gate "staying in
   agreement"; only the gate's name is stale. Rename the concrete condition,
   leave the surrounding prose and every `assert` untouched:

   - From: `... reason text and the is_luks_header_state() footer gate stay in agreement (a`
   - To:   `... reason text and the LuksHeaderUnreadable footer gate stay in agreement (a`

4. **Retire the stale name in the mixed-footer test doc comment**
   (`cli/src/mount.rs#format_degraded_refused_mixed_includes_doctor_footer_once`).
   Name the concrete variant instead of the deleted category; no assertion change:

   - From: `/// most once, even when multiple LUKS-header-state disks are present`
   - To:   `/// most once, even when multiple LuksHeaderUnreadable disks are present`

## Explicitly out of scope (do not gold-plate)

- **Do not touch `MissingReason`'s derives.** `#[derive(Debug, Clone, Copy,
  PartialEq, Eq)]` stays. `Copy` was relied on by the by-value `self` method, but
  the enum and its tests still want `Debug`/`PartialEq`/`Eq` (for `assert_eq!`)
  and the derives are harmless; removing `Copy` is unrelated scope and risks
  disturbing implicit copies elsewhere.
- **Do not add or change tests.** Behavior is already pinned, structure-
  insensitively, by three footer tests that drive `format_degraded_refused` and
  assert on rendered substrings -- none reference the method:
  - `format_degraded_refused_unreadable_includes_doctor_footer` (footer present
    for an `LuksHeaderUnreadable` disk),
  - `format_degraded_refused_unplugged_only_omits_doctor_footer` (footer absent
    for `Unplugged`-only),
  - `format_degraded_refused_mixed_includes_doctor_footer_once` (footer once for
    a mixed slice).
  (The finding said "two existing footer unit tests"; there are three. Either
  way, coverage of the gate survives the change untouched.)
- **Leave the `MissingReason` enum doc and `format_degraded_refused` doc
  unaffected** (the deleted method itself carries no `///`). The only comment
  changes are edits 3-4.
- **Do not over-correct the other "header state" mentions.** The only stale
  references are the two in edits 3-4: the deleted symbol `is_luks_header_state`
  and the category phrase "LUKS-header-state". Every other "header state" /
  `header_state` mention in active source describes a *living* abstraction and
  must be left exactly as-is -- correct, not residue (enumerate them with
  `git grep -ni "header.state" -- cli/src tests docs README.md`):
  - `cli/src/mount.rs#explain_open_failure` -- its doc, its `header_state`
    bindings, and its helper tests classify the real `LuksHeaderState` probe
    enum (luks.rs), which still exists.
  - `cli/src/status.rs` describes `DiskStatus`'s unreadable state.
  - `cli/src/tui/model.rs` describes the TUI's `LuksHeader*` render enum.

## Verification

No behavioral change: edits 1-2 are a pure Rust-internal refactor, and edits 3-4
touch only comment text (no assertions), so the `tests/cli/braid-unlock.py` edit
is comment-only and needs no VM run. Rust unit tests are the appropriate and
sufficient gate; no `just test-vm` needed.

1. `just test-rust` -- compiles the crate (proving the inlined `matches!`
   typechecks and the deleted method has no stray reference) and runs the three
   footer tests above. Optionally narrow to the touched path with
   `cargo test format_degraded_refused`.
2. `git grep -n "is_luks_header_state\|LUKS-header-state" -- cli/src tests docs README.md`
   returns no hits -- the deleted symbol and its category phrasing are gone from
   active source. Scope to active source on purpose: committed historical records
   under `plans/impl/` (e.g. `2026-06-03-pin-unlock-doctor-footer-e2e.md`)
   legitimately still quote `is_luks_header_state()` as a point-in-time snapshot
   and must not be edited, so an unscoped `rg` would falsely fail this check.
3. Confirm the diff is exactly the four edits above across the two files and
   nothing else: no derive or assertion churn, and no living-enum "header state" /
   `header_state` mention touched.
