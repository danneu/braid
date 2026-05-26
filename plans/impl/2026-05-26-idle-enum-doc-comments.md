# Plan: type-level doc comments for `idle.rs` enums

## Context

A `/ultrareview` finding (Low / project-fit) flagged that `IdleResult`
(`cli/src/idle.rs:9`) and `BusyReason` (`cli/src/idle.rs:18`) are top-level
`pub` enums with no type-level `///` doc, which the CLAUDE.md "Doc Comments"
convention asks for on new public types.

Verification confirmed the finding is substantively correct and its proposed
wording is accurate:

- **Exit-code intent is non-local.** `IdleResult` is the `braid idle`
  autosuspend gate; the tri-state-to-exit mapping lives in the consumer
  (`cli/src/main.rs:785-796`): `PoolOffline => exit(0)`, `Idle => exit(0)`,
  `Busy => exit(1)`. A reader of `idle.rs` alone cannot recover the
  "two states allow suspend, one blocks, fail-closed" framing from the
  variant docs.
- **`BusyReason::Display` deliberately diverges from `ExclusiveOp::Display`.**
  `ExclusiveOp::Display` emits `"balance (paused)"` (`cli/src/preflight.rs:124`);
  `BusyReason::Display` emits `"balance paused"` / `"balance running"`
  (`cli/src/idle.rs:41-42`). `preflight.rs:119-123` already documents this split
  from the other side, so a `BusyReason` type-doc closes the loop.
- **The sibling it wraps is already documented.** `ExclusiveOp` carries a
  multi-line type-doc (`cli/src/preflight.rs:78-82`). Documenting the idle enums
  brings the idle/preflight pair into consistency -- the concrete, bounded win.

Intended outcome: the two enums (and the one undocumented variant) gain tight
type-level docs that capture the fail-closed gate intent and the intentional
`Display` divergence, with no behavioral change.

## Scope boundary (deliberate)

A survey of `cli/src/` found dozens of `pub`/`pub(crate)` enums with no
type-level doc (e.g. `cmd.rs:14/21`, `config.rs:10/140`, `status.rs:32/121/142`,
`probe.rs:64`, `mount_check.rs:8`). This plan does **not** sweep them. The
convention's own test -- "if removing the comment would not lose any
information a reader could not recover from the code, do not write it" -- argues
against a mechanical project-wide pass. We fix `idle.rs` because it is the
fail-closed autosuspend gate where the intent is genuinely non-obvious and its
direct sibling is already documented.

## Edits (single file: `cli/src/idle.rs`)

All text uses `--` (not em-dash), matching the file's existing doc style.

1. **`IdleResult` type doc** -- insert above `pub enum IdleResult` (currently
   `cli/src/idle.rs:8-9`, between the `#[derive]` and the enum is fine; place it
   above the `#[derive]` to match `ExclusiveOp`):

   ```rust
   /// Tri-state result of the `braid idle` autosuspend gate: `PoolOffline`
   /// and `Idle` both allow suspend (exit 0), `Busy` blocks it (exit 1).
   /// Fail-closed -- any unknowable probe maps to `Busy`, never to idle.
   ```

2. **`Busy` variant doc** -- the variant at `cli/src/idle.rs:14` lacks a doc
   while its siblings `Idle` and `PoolOffline` have one. Add for consistency:

   ```rust
   /// Pool is busy -- block suspend. Carries the reason for status output.
   Busy(BusyReason),
   ```

3. **`BusyReason` type doc** -- insert above `pub enum BusyReason`
   (currently `cli/src/idle.rs:17-18`):

   ```rust
   /// Why `braid idle` reports busy. Its `Display` is the idle-specific
   /// status-line surface and intentionally diverges from
   /// `ExclusiveOp::Display` (e.g. "balance paused" vs "balance (paused)").
   ```

Each is within the convention's "one to three lines" guidance.

## Out of scope

- No change to `cmd_idle`, the variant set, `Display`, or any behavior.
- No new or modified tests -- this is doc-only; existing `idle.rs` tests
  (including `busy_reason_display_pins_cli_strings`) already pin the `Display`
  strings the `BusyReason` doc references.
- No project-wide enum-doc sweep (see Scope boundary).

## Verification

Doc-only, so the bar is "still builds, nothing regressed":

1. `just test-rust` -- the crate's unit tests (incl. `idle.rs` tests) pass.
2. `cargo build -p braid-cli` (implied by the above) -- confirms the `///`
   blocks parse. No intra-doc `[link]` syntax is used (only backticked code
   spans), so there is no rustdoc link-resolution risk.

No VM tests needed -- this touches no module, unit, or runtime behavior.
