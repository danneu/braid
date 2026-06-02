# Fix the TUI snapshot guide + unify the duplicated snapshot harness

## Context

`docs/dev/tui-snapshots.md` is the contributor guide for TUI snapshot
testing, but its central example never matched the code. It shows
`assert_snapshot!(terminal.backend())` with a bare `assert_snapshot!`,
while every real view test does `snap!(buffer_to_string(&render(...)))`
through a local `snap!` macro that sets `prepend_module_to_snapshot =>
false`. insta's default for that setting is `true` (verified in
`insta-1.47.2/src/settings.rs:209` and its doc comment at `:263`), so a
contributor copying the doc would assert on the wrong value AND get
module-prefixed snapshot filenames (`braid_cli__tui__view__tests__*.snap`)
instead of the bare names the repo actually commits. Commit `335beb61`
("remove snapshot prefix") is the proof: it renamed
`braid_cli__tui__view__tests__snapshot_*.snap` -> `snapshot_*.snap`
*by adding* the `snap!` macro, and in the same commit created this doc
with the wrong example. `403d1b07` later moved the doc into the mdbook
tree without fixing it. The doc also points at a stale path
(`cli/src/tui/view.rs`; the file is `cli/src/tui/view/mod.rs`).

While verifying, the real harness turned out to be duplicated
byte-for-byte: `buffer_to_string` and the `snap!` macro are identical in
`cli/src/tui/view/mod.rs:1567-1587` and `cli/src/tui/browse/view.rs:364-384`.
The doc has to describe this pattern, so the ideal end state is one
canonical shared helper that the doc points at -- not two copies plus a
caveat.

**Outcome:** one shared `#[cfg(test)]` TUI snapshot helper, both view
modules using it, and a doc whose example is copy-paste-correct against
the real code. Snapshot files must not move or change.

## Part A -- Unify the harness (refactor, code)

Extract the two identical pieces into a new TUI-scoped test module. Keep
each module's `render` local, since they diverge (view calls
`view(model, frame, now)` with a fixed `now`; browse calls
`view_browse(model, frame, frame.area())`).

1. **New file `cli/src/tui/test_support.rs`** (entire module is `#[cfg(test)]`):
   - `pub(crate) fn buffer_to_string(terminal: &Terminal<TestBackend>) -> String`
     -- moved verbatim from `view/mod.rs:1567-1579` (identical to browse's copy).
   - `macro_rules! snap { ... }` -- moved verbatim, followed by
     `pub(crate) use snap;` so it is path-addressable as
     `crate::tui::test_support::snap` (`macro_rules!` re-export via
     `pub(crate) use` -- verified to compile under this crate's edition
     2024; see "Macro sharing (verified)" under Risks).
   - `use ratatui::Terminal; use ratatui::backend::TestBackend;` for the
     function signature.
   - Per the repo's doc-comment rule (`AGENTS.md` "Doc Comments"): a
     module-level `//!` line and a `///` on `buffer_to_string` stating
     *why* it is shared (e.g. "Shared TUI snapshot helpers so `view` and
     `browse` tests render and assert through one canonical path.").

2. **`cli/src/tui/mod.rs`** -- add `#[cfg(test)] pub(crate) mod test_support;`
   (mirrors the `pub(crate) mod test_fixtures;` precedent at `cli/src/lib.rs:62`).

3. **`cli/src/tui/view/mod.rs`** -- delete the local `buffer_to_string`
   (1567-1579) and `snap!` (1581-1587); add
   `use crate::tui::test_support::{buffer_to_string, snap};` to the
   `#[cfg(test)] mod tests` use-block (1547-1558). Keep `render` and the
   `ratatui::{Terminal, backend::TestBackend}` imports (still used by `render`).

4. **`cli/src/tui/browse/view.rs`** -- same deletion (364-376, 378-384) and
   same `use` addition in the test module (345-353). Keep `render` and its
   ratatui imports.

**Out of scope (do not touch):** the `snap!` in `cli/src/ups.rs:787/795` is
a different harness (asserts JSON, no `render`/`buffer_to_string`); leave it.
Do not try to share `render` -- its divergence is intentional.

### Why snapshots will not move

`snap!` expands `insta::assert_snapshot!` at the *call site*, so `file!()`
and insta's function-name auto-detection resolve to each test's own file
(`view/mod.rs`, `browse/view.rs`) regardless of where the macro is defined.
Combined with `prepend_module_to_snapshot => false`, `.snap` files stay in
`cli/src/tui/view/snapshots/` and `cli/src/tui/browse/snapshots/` with bare
names. Moving only the *definition* changes nothing observable. Verification
(below) confirms it.

## Part B -- Rewrite the doc (docs)

Rewrite the top of `docs/dev/tui-snapshots.md` to the real pattern, and fix
the stale path and misleading framing. Proposed replacement for the
"Rendering for snapshots" section (lines 3-15):

```markdown
## Rendering for snapshots

Each TUI view module's `#[cfg(test)]` block defines a small `render` that
draws the view into a `TestBackend`, then asserts via the shared `snap!`
helper. `render` is per-module (it calls that module's own view function);
`buffer_to_string` and the `snap!` macro are shared from
`cli/src/tui/test_support.rs`.

```rust
use crate::tui::test_support::{buffer_to_string, snap};

// Per-module: calls this view's draw fn with a fixed `now` for determinism.
fn render(model: &Model, width: u16, height: u16) -> Terminal<TestBackend> {
    let now = time::macros::datetime!(2026-02-24 02:12:00);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| view(model, frame, now)).unwrap();
    terminal
}

#[test]
fn snapshot_with_pool() {
    let model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
    snap!(buffer_to_string(&render(&model, 60, 24)));
}
```

`snap!` wraps `insta::assert_snapshot!` in
`insta::with_settings!({ prepend_module_to_snapshot => false }, ...)`.
That setting defaults to `true`; we force it off so snapshot files are
named after the test alone (`snapshot_with_pool.snap`), not
`braid_cli__tui__view__tests__snapshot_with_pool.snap`. Always go through
`snap!` -- a bare `insta::assert_snapshot!` would reintroduce the prefix
and write to a different filename.

insta could snapshot the `TestBackend` directly (it implements `Display`),
but `buffer_to_string` trims trailing whitespace per line for cleaner
diffs, so all view tests assert on its `String`. Styles/colors are not
captured -- text only.
```

Also in the same file:
- **"The cargo insta workflow" / "Typical cycle":** keep the
  `cargo insta review` / `cargo insta accept` guidance (insta-specific),
  but change the plain test-run command from `cargo test -p braid-cli` to
  `just test-rust`, per `AGENTS.md` ("prefer `just test-rust` over
  `cargo test -p <name>`").
- Leave "What ratatui recommends" as-is.

## Part C -- Cross-link (docs, minor)

`docs/dev/testing.md` (the main testing guide) never mentions TUI snapshot
testing. Add a one-line pointer to `tui-snapshots.md` near where unit tests
/ `just test-rust` are discussed. Use a same-directory relative link
(`tui-snapshots.md`) so `mdbook-linkcheck2` passes. Verify the exact anchor
location when implementing.

## Files to modify

- `cli/src/tui/test_support.rs` -- new shared `#[cfg(test)]` module.
- `cli/src/tui/mod.rs` -- declare the module.
- `cli/src/tui/view/mod.rs` -- drop dup, add `use`.
- `cli/src/tui/browse/view.rs` -- drop dup, add `use`.
- `docs/dev/tui-snapshots.md` -- rewrite example + fix path/commands.
- `docs/dev/testing.md` -- one-line cross-link (Part C).

## Commits (two, in order)

1. `refactor(tui): share snapshot test harness across view modules` -- Part A.
2. `docs(tui): correct snapshot guide to match real test pattern` -- Parts B + C.

(Refactor first so the doc can reference the now-existing `test_support`.)

## Verification

1. `just test-rust` -- all Rust unit tests incl. insta snapshots pass.
2. **Prove snapshots did not move:** after the run, `git status --short`
   shows *no* changes under `cli/src/tui/view/snapshots/` or
   `cli/src/tui/browse/snapshots/`, and there are no `*.snap.new` files
   (`git status` should list only the 4 source files in commit 1). This is
   the load-bearing check that the macro move was behavior-preserving.
3. `mdbook build docs` -- passes `mdbook-linkcheck2` (validates the Part C
   cross-link and that the doc still builds).
4. Eyeball the rendered doc example against `cli/src/tui/view/mod.rs` tests
   -- the `render`/`snap!` shapes should match line-for-line in spirit.

This is a localized change (test scaffolding + one doc); no VM tests needed.
After `just test-rust` is green, hand back for any full-suite rerun the user
wants.

## Risks / non-goals

- **Macro sharing (verified):** the plan's `pub(crate) use snap;` re-export
  + `use crate::tui::test_support::{buffer_to_string, snap};` at call sites
  compiles under edition 2024 -- reproduced against the exact
  `tui`/`test_support`/`view`/`browse` module structure with
  `rustc --edition 2024 --test`. A negative control that drops the
  `pub(crate) use snap;` line (but keeps the path import) fails with
  `E0432: unresolved import ...snap`, confirming the re-export is the
  load-bearing piece -- so it must textually follow the `macro_rules!`.
  The `#[macro_use] mod test_support;` + unqualified-`snap!` alternative
  also compiles, but is declined: the crate has zero `#[macro_use]` today,
  and explicit path imports match its modern-edition style and keep each
  test's dependency on `snap!` visible.
- **Non-goal:** unifying `render` (intentionally per-module) or folding in
  the unrelated `ups.rs` `snap!`.
- **Non-goal:** any change to snapshot content, dimensions, or the
  `buffer_to_string` algorithm -- it moves verbatim.

## Implementation notes

- The plan specified only a module-level `//!` and a `///` on
  `buffer_to_string`. I also added a `///` to the `snap!` macro (the macro
  *body* still moved verbatim) because the `pub(crate) use snap;` re-export
  makes it a new shared `pub(crate)` boundary, which the `AGENTS.md`
  "Doc Comments" rule covers; the comment records the
  `prepend_module_to_snapshot => false` invariant.
- Part C anchor: `docs/dev/testing.md` has no "unit tests / `just test-rust`"
  section to attach to, so the one-line `tui-snapshots.md` pointer went into
  the intro paragraph beside the existing `tests/module/systemd-lifecycle.py`
  cross-link -- the doc's established place for cross-doc navigation.
- Per the plan's scope, only the `cargo test -p braid-cli` command in the
  "Typical cycle" block became `just test-rust`; the bare `cargo test`
  mentions in "The cargo insta workflow" stayed, since they describe insta's
  generic `cargo test` hook, not braid's run command.
