# Plan: cite the TUI offline render-string test in decision 024

## Context

A review finding claimed the TUI disk-table has no test pinning that a
verified-present-but-unpooled member renders as the `offline` cell, so the
render string could regress to the `missing` fallback undetected.

Investigation (see prior verify-issue run) showed the **test already exists and
passes**: `cli/src/tui/view/mod.rs#unpooled_disk_status_cell_renders_each_variant`
maps `delta -> UnpooledDiskRender::Offline` and asserts both
`cell("delta") == "offline"` and the span color is `Yellow`. Git history confirms
the assertion shipped in the same commit as the feature (`ffcdc222`). So the
headline (add a test) is **not** the work.

The real, narrow problem is what *generated* the false positive: decision 024's
"Tests That Enforce This" offline bullet cites only the **classifier**
(`cli/src/tui/probe.rs`, which pins the `UnpooledDiskRender::Offline` enum) and
`cli/src/status.rs`. It omits the **renderer** (`cli/src/tui/view/mod.rs`, which
pins the literal `offline` string). A reviewer reading that bullet sees the
classifier test, finds no render-string test cited, and concludes one is missing.

Intended outcome: make the citation accurate so this class of false-positive
review finding stops recurring. Docs-only change; no code or test change.

## The change

Single bullet edit in `docs/design/decisions/024-luks-uuid-identity.md`
(the offline entry under `## Tests That Enforce This`, currently lines 253-255).

**Before:**

```
- `cli/src/status.rs` and `cli/src/tui/probe.rs` unit tests pin that a present,
  LUKS-identity-verified member absent from the live pool renders `offline`, not
  `missing` or `unknown`.
```

**After:**

```
- `cli/src/status.rs` and `cli/src/tui/probe.rs` unit tests pin that a present,
  LUKS-identity-verified member absent from the live pool is classified offline
  (`UnpooledDiskRender::Offline` in the TUI), not `missing` or `unknown`.
  `cli/src/tui/view/mod.rs#unpooled_disk_status_cell_renders_each_variant` pins
  that the TUI renders that classification as the literal yellow `offline` cell
  rather than collapsing to the `missing` fallback.
```

## Why this wording

- **Separates classify from render.** `probe.rs` pins the enum; `view/mod.rs`
  pins the string. Collapsing them is exactly what misled the reviewer; naming
  both pinning points dissolves the confusion.
- **Names the `missing` fallback** -- the precise regression the finding feared,
  now visibly covered.
- **`path#symbol` citation form** matches `doc-citations.md`: a plain code span
  (not a link, no line number), naming the drift-proof greppable test symbol.
  Bare-path "X unit tests" reads fine for a cluster of tests in a file; here a
  single specific test is the referent, so the symbol anchor is the correct form.

## Files

- `docs/design/decisions/024-luks-uuid-identity.md` -- the one bullet above. No
  sibling bullet in 024 describes the offline render string (the TUI bullets near
  the Bus-column / LUKS-metadata section concern other columns), so this is the
  only edit site.

## Out of scope

- No change to `cli/src/tui/view/mod.rs` -- the test it would add already exists
  and passes (`cargo test --lib unpooled_disk_status_cell_renders_each_variant`).
- No change to `cli/src/tui/probe.rs` or its offline-classification test.
- No behavior or invariant change, so no other ADR / `principles.md` / `README.md`
  update is triggered.

## Verification

1. Symbol resolves (citation is greppable, not stale):
   `rg unpooled_disk_status_cell_renders_each_variant cli/src/tui/view/mod.rs`
   returns the test definition.
2. The cited test still passes:
   `cd cli && cargo test --lib unpooled_disk_status_cell_renders_each_variant`.
3. Docs build clean (mdBook render + linkcheck, confirms the prose code span is
   not mistaken for a broken link): `just docs-build`.
