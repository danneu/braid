# Plan: inline the dead `STDERR_STYLE` Bracketed constants

## Context

A review finding flagged `RemoveMissingPlan::STDERR_STYLE` as dead structure:
`remove-missing` never emits a `PerDisk` note, so its `Bracketed` value is
unobservable and the constant is justified only by analogy ("keeps the
stderr-note contract uniform with the other migrated commands").

Investigation showed the finding's *claim* is correct but its *scope* is wrong.
Six command `Plan` types each define `pub const STDERR_STYLE: PerDiskStyle =
PerDiskStyle::Bracketed` (`add`, `remove`, `unlock`, `replace`, `remove_missing`,
`recover`). In all six the value is a non-choice:

- `add`, `remove`, `replace`, `remove_missing` never construct a `PerDisk` note
  in production, so `Bracketed` vs `Plain` is indistinguishable.
- `unlock`, `recover` do emit `PerDisk` notes (via `ProbeEvent::to_preview_note`),
  but `Bracketed` is forced anyway: `Preview::render` hardcodes `Bracketed` for
  dry-run stdout (`preview.rs`, pinned by `render_per_disk_note_uses_bracketed_style`),
  so any other stderr value would break cross-mode wording uniformity.

The finding's own fix -- inline at `remove-missing` only -- would *fragment* the
convention by singling out one of six identical commands. The ideal pivot is the
uniform one: delete all six dead `Bracketed` consts and inline the
`PerDiskStyle::Bracketed` literal at their call sites, exactly as `main.rs` and
`mount.rs` already do.

`enroll` is the sole genuine exception and stays untouched: `EnrollPlan::STDERR_STYLE
= PerDiskStyle::Plain` is observable and tested -- it preserves the legacy
`skip: <name> not present` stderr wording while dry-run stdout still renders
`Bracketed` (pinned by `plan_skip_note_renders_bracketed_in_preview_and_plain_in_stderr`).
Its `///` documents a real invariant, so it earns its name under braid's
doc-comment convention (AGENTS.md: a `///` must say *why an item exists at that
boundary*, not restate the signature). The six Bracketed consts fail that rule
("stays uniform with..."); enroll's passes it.

**Outcome:** six dead consts + their analogy doc-comments removed; every
stderr-note call site either inlines `PerDiskStyle::Bracketed` or keeps enroll's
documented `Plain` const. Net: less surface, and the one surviving distinction is
meaningful and self-documenting.

**No behavior change.** The inline literal equals each deleted const's former
value at every site, so all existing byte-exact render tests must pass unchanged.
The `style` *parameter* on the preview helpers stays (enroll needs `Plain`). No
user-facing output strings change, so `check-output-ascii.py` is unaffected. ADR
022 mandates only "shared renderers," not per-command consts -- nothing is
contradicted.

## Approach

### 1. Delete the six Bracketed consts (and their `///` doc comments)

| File | Const | Approx. line |
|------|-------|------|
| `cli/src/remove.rs` | `RemovePlan::STDERR_STYLE` | 234 (doc 230-233) |
| `cli/src/add.rs` | `AddPlan::STDERR_STYLE` | 1018 (doc 1014-1017) |
| `cli/src/unlock.rs` | `UnlockPlan::STDERR_STYLE` | 60 (doc 56-59) |
| `cli/src/replace.rs` | `ReplacePlan::STDERR_STYLE` | 400 (doc 396-399) |
| `cli/src/remove_missing.rs` | `RemoveMissingPlan::STDERR_STYLE` | 144 (doc 139-143) |
| `cli/src/recover.rs` | `RecoverPlan::STDERR_STYLE` | 1193 (doc 1189-1192) |

### 2. Inline the literal at every reference

Pattern: `Self::STDERR_STYLE` / `<Plan>::STDERR_STYLE` -> `PerDiskStyle::Bracketed`.
Each command has a production `execute()` site, a `PlanFailure` error-arm site in
`cmd_*`, and (most) a test-assertion site:

- `remove.rs`: 257 (`execute`), 656 (`cmd_remove` err arm), 2456 (test)
- `add.rs`: 1042, 1937, 9859
- `unlock.rs`: 80, 247
- `replace.rs`: 1167 (inside the `render_replace_notes_for_stderr` capture wrapper)
- `remove_missing.rs`: 164, 503, 2686
- `recover.rs`: 1214, 1568, 17745 (this one calls `render_notes_for_stderr_with(..., false)`)

Line numbers are anchors and will drift as edits land; the symbol->literal pattern
is what matters. Every one of these modules already imports `PerDiskStyle` in its
`use crate::preview::{...}` line -- confirm the import stays referenced after the
change (it will, via the inline literal) so there is no unused-import warning.

### 3. Reword orphaned prose doc-comments that name the deleted const

- `unlock.rs:42` and `recover.rs:212` struct docstrings ("renders `notes` to
  stderr with `STDERR_STYLE`...") -> name `PerDiskStyle::Bracketed`, or drop the
  symbol reference.
- Test docstrings `remove.rs:2448`, `add.rs:9845`, `remove_missing.rs:2678` ->
  replace `<Plan>::STDERR_STYLE` with `PerDiskStyle::Bracketed`.

### 4. Leave `enroll` untouched

Keep `EnrollPlan::STDERR_STYLE = PerDiskStyle::Plain` (`enroll_key_file.rs:512`),
its doc comment, and its three references (530, 799, 1309).

Optional separable tidy (not required by this pivot): nothing outside the module
references the const, so `pub const` -> `const` would tighten visibility -- the
`#[cfg(test)] mod tests` child can still read it. Skip unless doing a visibility
pass anyway; keep this diff minimal.

## Reuse / no new code

- Preview helpers `emit_notes_to_stderr(notes, style)`,
  `render_notes_for_stderr(notes, style)`, `render_notes_for_stderr_with(notes,
  style, color)` are unchanged -- they already take `style`.
- The inline literal is the exact precedent already in `main.rs` (lock/stop
  load-note emission, ~1272/1342) and `mount.rs` (`render_probe_events` /
  `print_probe_events`, ~329/339).

## Verification

- **`just test-rust` -- required gate.** This compiles and runs the
  `#[cfg(test)]` modules, so it is the check that catches a stale *test-only*
  reference to a deleted const (`remove.rs` ~2456, `add.rs` ~9859,
  `remove_missing.rs` ~2686, `recover.rs` ~17745). Plain `cargo build` does
  **not** compile test code and would silently miss these, so the test run -- not
  the build -- is the safety net. All existing tests must pass **unchanged**,
  because the inline literal equals each former const value. Behavioral anchors
  that pin the output contract this refactor must not move:
  - `cli/src/preview.rs#render_per_disk_note_uses_bracketed_style` -- dry-run
    stdout always renders `PerDisk` notes in `Bracketed` style.
  - `cli/src/mount.rs#probe_event_to_preview_note_preserves_byte_format` -- the
    probe-event note bytes feeding `unlock` / `recover`.
  - `cli/src/enroll_key_file.rs#plan_skip_note_renders_bracketed_in_preview_and_plain_in_stderr`
    -- enroll's `Plain` stderr (`skip: disk1 not present`) vs `Bracketed` stdout,
    proving the kept const still works.
  - plus `remove_warn_notes_render_canonical_bracketed_form`,
    `add_warn_notes_render_canonical_bracketed_form`,
    `remove_missing_warn_notes_render_canonical_bracketed_form`.
- **`cargo build -p braid-cli` -- optional.** A fast production-only compile that
  catches the non-test call-site conversions (each `execute()` + `cmd_*` error
  arm), but not the test references above. A convenience, not the gate.
- **`cargo fmt` + `just clippy`** -- the repo lint recipe is `just clippy` (clean,
  modulo the pre-existing large-err / too-many-args allowances).
- No new behavioral test is warranted: the change is provably output-preserving
  (literal == former const value at every site), and the byte-exact tests above
  already cover the stderr/stdout note contract.
