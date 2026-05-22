# Plan: drop bespoke unlock stderr-capture seam

## Context

`cli/src/unlock.rs` carries two parallel stderr-capture mechanisms. One
is the canonical project seam, `crate::status_tag::emit_status` plus
`crate::status_tag::testing::capture_with_color`, used by three sibling
unlock tests (`unlock.rs:620`, `:1091`, `:1195`) and explicitly
documented at `status_tag.rs:130` as "the" way unit tests capture stderr
rows. The other is a one-off thread-local module
(`unlock_stderr_capture`) plus a wrapper helper
(`emit_post_mount_enrichment_warning`) at `unlock.rs:259-308`, used by
exactly one test (`unlock_tolerates_post_mount_save_membership_failure`
at `unlock.rs:1486`).

The bespoke seam was introduced in `2f03a00 fix(unlock): enrich from
in-memory membership` alongside the test it serves. The immediately
adjacent probe-error branch in the same `match` block already routes
through `status_tag::emit_status` (`unlock.rs:166-168`), so the
inconsistency is visible at first read inside a single 20-line span.

Removing the bespoke seam removes ~50 lines, deletes a `#[cfg(test)]`
branch from a production-only code path, and leaves one canonical
stderr-capture mechanism for the crate. Behavior is byte-identical: the
production paths both emit the same bytes, and the test captures the
same string from the same call sites.

## Change

### 1. Reroute the two warning emission sites in `cli/src/unlock.rs:154-164`

Replace:

```rust
emit_post_mount_enrichment_warning(format_args!(
    "Warning: failed to save enriched membership: {e}"
));
```

with:

```rust
crate::status_tag::emit_status(&format!(
    "Warning: failed to save enriched membership: {e}\n"
));
```

And similarly for the sibling call at `unlock.rs:160-162`:

```rust
crate::status_tag::emit_status(&format!(
    "Warning: failed to enrich pool membership: {e}\n"
));
```

This mirrors the existing probe-error emission at `unlock.rs:166-168`
verbatim in shape.

### 2. Delete the bespoke seam at `cli/src/unlock.rs:259-308`

Remove:
- The `emit_post_mount_enrichment_warning` helper (lines 259-267).
- The `unlock_stderr_capture` module (lines 269-308).

### 3. Switch the one test consumer to the canonical seam

In `unlock_tolerates_post_mount_save_membership_failure`
(`unlock.rs:1364`), replace the
`super::unlock_stderr_capture::capture(...)` wrapper at line 1486 with
the same `crate::status_tag::testing::capture_with_color(false, ||
{...})` pattern already used by the three sibling tests. Rename the
local from `stderr` to `captured` for consistency with the other tests
and update the two assertions that reference it (the
`Warning: failed to save enriched membership:` count check and its
debug-format message).

Behavioral pin remains "exactly one save-membership warning line is
emitted"; only the capture transport changes.

## Why this approach

- Single canonical seam already exists, documented as such, and is
  used by three sibling tests in the same file. Unification eliminates
  the "which one is canonical?" question for future maintainers.
- Production emission is byte-identical: `eprintln!("{args}")` vs.
  `eprint!("...\n")` produce the same bytes.
- Test capture is semantically identical: both seams capture the same
  emitted line text from the same call sites.
- The bespoke seam has no external consumers (confirmed by grep --
  `unlock_stderr_capture` and `emit_post_mount_enrichment_warning` are
  used only inside `unlock.rs`).

## Files modified

- `cli/src/unlock.rs` -- the only file touched. Two emission sites,
  one helper + module deletion, one test wrapper swap.

## Reused helpers

- `crate::status_tag::emit_status` (`cli/src/status_tag.rs:66`) --
  canonical stderr-capture-aware emitter.
- `crate::status_tag::testing::capture_with_color`
  (`cli/src/status_tag.rs:143`) -- canonical test capture seam.

## Verification

- `just test-rust` -- runs the full Rust unit test suite. The three
  tests pinning the unlock post-mount warning paths
  (`unlock_warns_when_post_mount_probe_errors`,
  `unlock_tolerates_post_mount_probe_mounted_false`,
  `unlock_tolerates_post_mount_save_membership_failure`) must all
  pass. The last one exercises the rerouted code path; the first two
  guard the sibling paths that already use the canonical seam, so
  they regress-check that the change does not break the surrounding
  match arm.
- `cargo check -p braid-cli` (or `just test-rust`, which compiles
  first) confirms no leftover references to `unlock_stderr_capture`
  or `emit_post_mount_enrichment_warning`.
- No VM tests are needed: the change is internal to a `#[cfg(test)]`
  capture mechanism and the in-process stderr emission shape, with no
  effect on systemd units, mount/unlock behavior, or any
  CLI-reachable output bytes.
