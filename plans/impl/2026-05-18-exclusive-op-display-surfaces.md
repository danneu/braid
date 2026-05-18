# Plan: Document the two `ExclusiveOp` display surfaces

## Context

A `/verify-issue` review flagged that the same kernel state renders two
different ways across braid's user-facing surfaces:

- `cli/src/preflight.rs:108` — `ExclusiveOp::Display::BalancePaused` =>
  `"balance (paused)"`. The only user-facing consumer of this variant's
  `Display` form is `braid lock`'s hard-error path:
  `"cannot lock: {op} is in progress. Wait for it to finish first."` at
  `cli/src/preflight.rs:165` (the `RejectAnyBusy` branch). The
  parenthesized form keeps that sentence readable. The
  `RejectPausedBalanceElseEnqueue` branch intercepts `BalancePaused`
  with its own hardcoded string (`cli/src/preflight.rs:168-170`) before
  it could feed the `"waiting for in-flight {op}..."` template at
  `cli/src/preflight.rs:513`, and the UPS guard's `refusing to start
  {op}` (`cli/src/preflight.rs:464`) takes a plain command-name `&str`
  -- neither path renders `ExclusiveOp::Display::BalancePaused`. The
  variant's display is also pinned by `exclusive_op_display`
  (`cli/src/preflight.rs:660-671`) and `lock_preflight_rejects_balance_paused`
  (`cli/src/preflight.rs:1346-1357`).
- `cli/src/idle.rs:39` — `BusyReason::Display` overrides
  `ExclusiveOp::Balance` to `"balance running"` and
  `ExclusiveOp::BalancePaused` to `"balance paused"`. This surface is
  `braid idle`'s stdout (`busy: balance paused`), pinned by both the
  manual (`manual/commands/idle.md:43`) and the
  `busy_reason_display_pins_cli_strings` test
  (`cli/src/idle.rs:164-206`).

The split is intentional -- the parenthesized form reads naturally
embedded in `braid lock`'s sentence (`"balance (paused) is in
progress"` vs the awkward `"balance paused is in progress"`), while
idle's status-line surface wants short standalone labels. But the
rationale isn't visible at either call site, so a reasonable reviewer
reading either file alone will keep filing this finding. The fix is
documentation, not code.

## Approach

Two short, surface-local code comments. No behavior change, no test
change, no string change, no rustdoc change.

### Change 1: `cli/src/preflight.rs`

Add a `//` line comment immediately above the
`Self::BalancePaused => write!(f, "balance (paused)")` arm at
`cli/src/preflight.rs:108`, inside the existing `impl fmt::Display for
ExclusiveOp` block. The rationale sits exactly where the surprising
string lives, so anyone editing the arm sees the reason in the same
hunk.

Target shape (final wording can be tightened during impl):

```rust
impl fmt::Display for ExclusiveOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Balance => write!(f, "balance"),
            // Parenthesized so `braid lock`'s `RejectAnyBusy` template
            // (`"cannot lock: {op} is in progress..."`) reads
            // naturally. `braid idle` has its own standalone-label
            // surface (`"balance paused"`) via `BusyReason::Display`
            // in `cli/src/idle.rs`.
            Self::BalancePaused => write!(f, "balance (paused)"),
            Self::DeviceAdd => write!(f, "device add"),
            ...
        }
    }
}
```

The comment cites grep-stable identities (the `RejectAnyBusy` policy
variant and the literal template string) rather than line numbers --
adding this comment itself shifts the lines of the template arm in
the same file, so a line reference would be stale on landing. The
cross-file pointer is a path (`cli/src/idle.rs`) not a line number,
so it stays stable too.

This is a code comment (not rustdoc), so it does not appear in the
public API surface and cannot drift from the string in a different
file -- a future edit to `write!(f, ...)` and the comment live in the
same code block.

### Change 2: `cli/src/idle.rs`

Add a one- to two-line inline comment above the override arms in
`BusyReason`'s Display impl (lines 38-39) explaining that idle uses
standalone status-line labels for the balance variants, while other
exclop variants fall through to the sentence-embedding noun phrase.
This dissolves the same finding from the other direction.

Target shape:

```rust
impl std::fmt::Display for BusyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BusyReason::Unknown(msg) => write!(f, "unknown ({msg})"),
            BusyReason::ScrubRunning { pct: Some(p) } => write!(f, "scrub running ({p}%)"),
            BusyReason::ScrubRunning { pct: None } => write!(f, "scrub running"),
            // `braid idle` renders standalone status-line labels for
            // balance variants; other ops fall through to
            // `ExclusiveOp`'s sentence-embedding noun phrase via the
            // `{op} in progress` arm.
            BusyReason::Exclop(ExclusiveOp::Balance) => write!(f, "balance running"),
            BusyReason::Exclop(ExclusiveOp::BalancePaused) => write!(f, "balance paused"),
            BusyReason::Exclop(op) => write!(f, "{op} in progress"),
        }
    }
}
```

## What this does NOT change

- No user-facing strings change. `braid idle`, `braid lock`, and
  mutating-command preflight all keep their current output verbatim.
- No tests change. `busy_reason_display_pins_cli_strings`
  (`cli/src/idle.rs:164-206`), `exclusive_op_display`
  (`cli/src/preflight.rs:660-671`), and
  `lock_preflight_rejects_balance_paused`
  (`cli/src/preflight.rs:1346-1357`) all keep passing unchanged.
- No manual change. `manual/commands/idle.md:43` already matches the
  pinned idle surface.
- No rustdoc surface change. Both new comments are plain `//` line
  comments inside the trait-impl bodies, which is consistent with
  AGENTS.md's "skip trait impls whose purpose is the trait (Display,
  Debug, ...)" guidance for `///` doc comments.

## Files modified

- `cli/src/preflight.rs` (one `//` comment above the `BalancePaused`
  arm in `impl fmt::Display for ExclusiveOp`)
- `cli/src/idle.rs` (one `//` comment above the two override arms in
  `impl Display for BusyReason`)

## Verification

- `cargo fmt --manifest-path cli/Cargo.toml -- --check` (no formatting
  drift from new comments).
- `just test-rust` (full Rust unit tests; comments-only change, expect
  zero diffs in behavior).
- Spot-read the final wording: each comment must point at the specific
  call-site coupling that justifies the surface choice -- preflight's
  comment names `braid lock`'s sentence template, and idle's comment
  names the fall-through arm.
