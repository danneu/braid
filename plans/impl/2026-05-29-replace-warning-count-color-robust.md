# Fix color-fragile assertion in `cmd_replace_live_source_io_warning_renders_once_on_real_run`

> On promotion, conventional name: `2026-05-28-replace-warning-count-color-robust.md`.

## Context

A single Rust unit test fails deterministically on the Linux nix builder while
passing on darwin, and because every VM test depends on the `braid-cli`
derivation (whose `checkPhase` runs `cargo test`), this one failure **blocks the
entire VM suite** on Linux.

Failing test: `replace::tests::cmd_replace_live_source_io_warning_renders_once_on_real_run`
(`cli/src/replace.rs:6290`). The failing assertion (`cli/src/replace.rs:6308`):

```rust
assert_eq!(
    stderr
        .matches("[warn] source device (devid 2) has I/O errors")
        .count(),
    1,
    "real-run stderr must render source I/O warning exactly once, got:\n{stderr}"
);
```

It panics with `left: 0, right: 1` -- zero matches against a string that visibly
contains the warning.

### Root cause (confirmed against the raw build log)

braid colorizes the `[warn]` status tag when stderr is interactive.
`status_tag::color_enabled_for_stderr()` (`cli/src/status_tag.rs:83-93`) returns
`std::io::stderr().is_terminal() && !NO_COLOR`. When true,
`render_status_tag(StatusTag::Warn, true)` (`cli/src/status_tag.rs:29`) emits
`"\x1b[33m[warn]\x1b[0m"`, and `status_line` (`:43-49`) prepends it to the
message body. Only the **tag** is wrapped in ANSI; the body is never colorized.

- On the Linux nix builder the build's stderr is a TTY, so the warning renders
  as `\x1b[33m[warn]\x1b[0m source device (devid 2) has I/O errors. ...`. The
  literal substring `[warn] source device ...` no longer matches (the escapes
  sit between `[warn]` and the space), so `.count()` is 0.
- On darwin's native `cargo test`, stderr is piped (not a TTY), so the tag is
  plain `[warn]` and the match succeeds.

Confirmed via the untruncated build log (`nix-store -l <braid-cli drv>`, line
~2222): `[33m[warn][0m source device (devid 2) has I/O errors...`. (Nix's
"Last 25 log lines" error tail strips the ESC bytes, which is why a casual
`cat -v`/`rg` of that tail shows a clean `[warn]` and can mislead.)

### Why the fix belongs in the test, not production

braid's color gating (`is_terminal() && !NO_COLOR`) is the standard, correct
policy and is independently pinned (`cli/src/status_tag.rs:301-313`,
`:321-328`). A build sandbox presenting a TTY on stderr is unusual but not
braid's bug. The defect is that this one test asserts against the **colorized
presentation** without controlling color -- unlike its sibling
`cli/src/replace.rs:3785`, which already forces color off via
`status_tag::testing::capture_with_color(false, ...)`.

## Decision

**Match the warning's message body (which is never colorized) instead of the
`[warn]`-prefixed literal, reusing the `format_source_io_error_warning`
formatter.** The behavior under test is "the source I/O warning is emitted
exactly once on a real run"; the warning's stable identity is its body, while
the `[warn]` bracket and its color are presentation. The warn tag/level is
already pinned separately, so matching the body loses no coverage.

Rejected alternatives:
- **Force color off in the test** (add a `with_color_override` helper; wrap the
  capture). Matches the `3785` sibling pattern but adds machinery to keep a
  presentation-coupled assertion alive.
- **Force color off inside `replace_stderr_capture::capture`** for all callers.
  Fixes the class but makes color-off implicit and adds cross-module test-infra
  coupling.

Both are defensible; the chosen fix is the smallest, most behavioral, and most
structure-insensitive.

## Change

Single edit in `cli/src/replace.rs`, in
`cmd_replace_live_source_io_warning_renders_once_on_real_run`
(the `.matches(...)` argument at `:6310`):

```rust
 stderr
-    .matches("[warn] source device (devid 2) has I/O errors")
+    .matches(&format_source_io_error_warning(2))
     .count(),
 1,
```

- `format_source_io_error_warning` (`cli/src/replace.rs:1079`) is the single
  source of the warning body and is already in scope in this test module (used
  at `:6136`). The returned body is exactly what `status_line` appends after the
  tag, so it appears verbatim once in the captured stderr regardless of color.
- This mirrors the sibling planning assertion at `:6136`
  (`assert_eq!(warns[0], &format_source_io_error_warning(2))`), so the test now
  stays in sync with the warning wording automatically (DRY).
- Leave the assertion message (`"...got:\n{stderr}"`) and the companion
  `stats_requests == 1` assertion (`:6320`) unchanged -- the latter remains the
  structural backstop for the documented double-probe regression.
- Implementation detail: `&String` satisfies `std::str::pattern::Pattern`, so
  the form above compiles. If the pinned toolchain disagrees, bind
  `let warning = format_source_io_error_warning(2);` and match `warning.as_str()`.

### Coverage (no loss)

The warn tag/level is pinned independently of this test:
- `cli/src/replace.rs:6135-6136` -- planning emits exactly one `PreviewNote::Warn`
  whose body equals `format_source_io_error_warning(2)`.
- `cli/src/replace.rs:6140` -- the dry-run render includes the `[warn]` prefix.
- `cli/src/status_tag.rs:206-244` -- both plain `[warn]` and colored
  `\x1b[33m[warn]\x1b[0m` tag bytes are pinned.

This test's unique contribution is the end-to-end emission **count** on a real
run, which the body match captures exactly (a double-emit regression still
yields `count == 2`).

### Out of scope

- Production color logic (`status_tag.rs`) -- correct as-is; do not change.
- Other tests -- none other are at risk. The `3785` sibling already forces color
  off; the only other `replace_stderr_capture::capture` caller
  (`cmd_replace_old_equals_new_aborts_before_any_probe`, `:6342`) asserts
  `stderr.is_empty()` and is color-independent.
- No new test helpers, no parser/fixture refresh, no NixOS module changes.

## Verification

1. `just test-rust` -- compiles; the test still passes on darwin. (Necessary but
   not sufficient: darwin stderr is non-TTY, so this exercises the plain path.
   The new assertion is color-independent, so it passes on either path.)
2. **Decisive check (exercises the colored path):**
   `just test-vm braid-status-rust`. This rebuilds `braid-cli` in the Linux nix
   sandbox where stderr is a TTY (color on), runs its `cargo test` checkPhase,
   and -- with the fix -- the body match passes despite the colored `[warn]`,
   the build completes, and the VM boots. This is the proof the blocker is gone.
3. With the build unblocked, the originally intended VM verification for the
   devid change (the prior task's `braid-status-rust` / `braid-monitor` /
   `monitor-hot-unplug` runs) can finally execute end-to-end.

Scope is one assertion in one unit test (no systemd/lifecycle/pool-lock blast
radius), so the focused runs above suffice; hand back to the user for any full
`just test-vm` rerun.
