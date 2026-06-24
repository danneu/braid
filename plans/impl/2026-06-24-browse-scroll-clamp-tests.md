# Plan: dedup the Browse scroll clamp + pin it with tests

## Context

A review finding (Testing, Low) flagged that `BrowseState::page_down` /
`page_up` scroll clamping (`cli/src/tui/browse/state.rs:742-753`, the
documented Ctrl-D/Ctrl-U paging) has no unit test, so a regression that lets
`scroll_offset` run past the last full viewport (blank tail) or underflow
would go uncaught.

Investigation widened the picture:

- The clamp ceiling `output.len().saturating_sub(viewport_height)` is
  **duplicated** in two methods -- `page_down` (`state.rs:744`) and
  `content_down` (`state.rs:1273`). The two copies can drift independently.
- The finding assumed a keymap test proves `Ctrl-D`/`Ctrl-U` emit
  `BrowsePageDown`/`BrowsePageUp`. **No such test exists** -- the browse
  keymap test module (`browse/keymap.rs:32-128`) covers h/l, enter/esc, and
  j/k only. The behavior is a documented contract (`docs/commands/tui.md:49`:
  "Ctrl-D / Ctrl-U | Page Browse content down/up").
- `content_down`'s `+1` step is only indirectly hit by one assertion in
  `browse_scroll_down_does_not_emit_effect` (`app.rs:497-516`); its clamp is
  never reached.

**Outcome:** one source of truth for the clamp ceiling, plus behavioral tests
that pin every clamp endpoint and the missing keymap mapping. The tests
double as the safety net for the refactor (behavior is unchanged, so they
must stay green across it).

## Approach (chosen: dedup helper + behavioral tests)

Extract the duplicated ceiling into a single documented `max_scroll()` method
and route both "down" methods through it; leave the trivial "up" methods
(`saturating_sub` to floor 0) alone. Then add behavioral tests. Rejected
alternatives: tests-only (leaves the duplication the finding pointed at) and a
full `scroll_by(delta: isize)` clamp primitive (more machinery than a cosmetic
scroll offset warrants -- "simple" wins for a value whose worst case is a
redraw glitch, not data loss).

### 1. Production dedup -- `cli/src/tui/browse/state.rs`

Add a private accessor on `impl BrowseState` (carries a `///` per the
doc-comment convention since it encodes an invariant):

```rust
/// Largest `scroll_offset` that still fills the content viewport, so
/// paging and line scrolling clamp here instead of revealing a blank
/// tail past the last line.
fn max_scroll(&self) -> usize {
    self.output.len().saturating_sub(self.viewport_height.get() as usize)
}
```

Rewire the two call sites to use it (behavior identical):

- `page_down` (`state.rs:742-746`):
  ```rust
  pub(crate) fn page_down(&mut self) {
      let page = self.viewport_height.get() as usize;
      self.scroll_offset = (self.scroll_offset + page).min(self.max_scroll());
  }
  ```
- `content_down` else-branch (`state.rs:1272-1278`):
  ```rust
  } else {
      self.scroll_offset = (self.scroll_offset + 1).min(self.max_scroll());
  }
  ```

`page_up` / `content_up` are left unchanged.

### 2. Clamp tests -- `cli/src/tui/browse/state.rs` `mod tests` (~line 1308)

Each test opens with the required `// Intent: / // Why it exists: /
// Scenario:` preamble (`docs/dev/testing.md`). Setup mirrors the idiomatic
direct-field style already used at `state.rs:2316`: construct
`BrowseState::default()` (default command is `BtrfsFilesystem` usage -- not a
picker, so `content_down` takes the scroll else-branch), assign
`state.output`, and set the viewport via `set_viewport_height`. Assert through
the `scroll_offset()` getter.

**Test A -- `page_down`/`page_up` clamp.** With 10 lines and viewport 3
(`max_scroll` = 7):
- `page_down` walks 0 -> 3 -> 6 -> **7** (from 9, clamped) and **stays 7** on
  the next `page_down` (no blank tail / overrun).
- `page_up` walks 7 -> 4 -> 1 -> **0** (saturated) and **stays 0** on the next
  `page_up` (no underflow).

**Test B -- `content_down`/`content_up` clamp (sibling formula).** Same 10/3
setup; set `state.focus = BrowseFocus::Content` and drive the real routing via
the `pub(crate)` `select_next()` / `select_prev()` (`state.rs:585,597`), which
dispatch to `content_down`/`content_up` for `Content` focus. Step `select_next`
past the end and assert `scroll_offset()` clamps at **7**; step `select_prev`
past the start and assert it returns to **0**. (Behavioral: exercises the same
entry point the live `j`/`k` keys use, not the private method directly.)

### 3. Keymap test -- `cli/src/tui/browse/keymap.rs` `mod tests` (~line 32)

Add the missing mapping test, mirroring `content_j_k_emit_scroll_messages`
and reusing the local `ctx(...)` helper (`keymap.rs:39-46`):

```rust
// Intent: Ctrl-D/Ctrl-U map to Browse full-page scroll messages.
// Why it exists: the documented (docs/commands/tui.md) page scroll must
//   keep emitting BrowsePageDown/BrowsePageUp; nothing else guards this
//   binding, so a keymap regression would silently break Ctrl-D/Ctrl-U.
// Scenario: user pages through long Browse output with Ctrl-D then Ctrl-U.
#[test]
fn ctrl_d_u_emit_page_messages() {
    let down = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
    let up = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert!(matches!(
        handle_key(down, &ctx(BrowseFocus::Content)),
        Some(Message::BrowsePageDown)
    ));
    assert!(matches!(
        handle_key(up, &ctx(BrowseFocus::Content)),
        Some(Message::BrowsePageUp)
    ));
}
```

## Files to modify

- `cli/src/tui/browse/state.rs` -- add `max_scroll()`, rewire `page_down` and
  `content_down`, add Test A and Test B in `mod tests`.
- `cli/src/tui/browse/keymap.rs` -- add `ctrl_d_u_emit_page_messages` in
  `mod tests`.

No `flake.nix` registration (that rule is for NixOS VM `.py` tests only); plain
`#[test]` fns are auto-discovered.

## Verification

1. `just test-rust` -- new tests pass; existing
   `browse_scroll_down_does_not_emit_effect` and the keymap tests stay green
   (proves the dedup is behavior-preserving).
2. **Confirm the tests actually guard the clamp** (the code is already correct,
   so prove the tests are meaningful by making them go red): temporarily drop
   `.min(self.max_scroll())` from `page_down` and from `content_down` and
   re-run -- Test A and Test B must fail on the clamp assertions; restore.
3. `just clippy` (= `cargo clippy --manifest-path cli/Cargo.toml --tests`,
   `justfile#clippy`) -- no new warnings. Use the recipe, not bare
   `cargo clippy`: it targets the `cli` manifest and its `--tests` flag lints
   the new unit-test code, which a repo-root `cargo clippy` would skip.
4. ASCII check: `scripts/docs/check-output-ascii.py` covers `cli/src/**/*.rs`
   but exempts comments and tests; the only non-comment/test addition is
   `max_scroll()`, which is pure code -- no user-facing strings. Keep the doc
   comment and any test strings ASCII regardless.

## Out of scope (noted, not addressed)

`page_down`/`page_up` mutate `scroll_offset` unconditionally while
`content_down`/`content_up` branch to list-selection in picker modes
(subvolume/smartctl/systemd). Whether Ctrl-D/Ctrl-U should move the picker
selection vs. scroll the raw viewport is a behavioral design question, not a
clamp-formula one -- left untouched here.

## Implementation notes

- The clamp tests use struct literals for `BrowseState` setup instead of
  post-`Default` field assignment so `just clippy` stays warning-free.
