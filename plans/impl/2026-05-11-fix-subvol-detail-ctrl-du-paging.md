# Fix SubvolDetail Ctrl-D/Ctrl-U Paging

## Summary

Restore advertised page scrolling in `braid browse` subvolume detail mode
by routing Ctrl-D and Ctrl-U through the existing `PageDown` and `PageUp`
messages. This is a key dispatch fix only; app state, rendering, and help
text already support the intended behavior.

## Key Changes

- Update `cli/src/browse/keymap.rs` so `ViewMode::SubvolDetail` handles
  `(KeyCode::Char('d'), KeyModifiers::CONTROL)` as `Message::PageDown`.
- Handle `(KeyCode::Char('u'), KeyModifiers::CONTROL)` as
  `Message::PageUp`.
- Preserve existing detail-mode allow-list behavior:
  - `Esc`/`Backspace` still go back.
  - `q`, `r`, `j`/Down, `k`/Up, and `?` still work.
  - `Tab`, `Enter`, `h`, and `l` remain ignored in detail mode.
- Do not change the help overlay, footer, `Message` enum, or
  update/render logic.

## Test Plan

- Add keymap unit tests in `cli/src/browse/keymap.rs`:
  - Ctrl-D in `SubvolDetail` emits `Some(Message::PageDown)`.
  - Ctrl-U in `SubvolDetail` emits `Some(Message::PageUp)`.
- Add app-level unit tests in `cli/src/browse/app.rs` that pin the
  page-scroll contract detail mode relies on:
  - With `mode = ViewMode::SubvolDetail`, multi-line `output`, and a
    fixed `viewport_height`, `Message::PageDown` advances
    `scroll_offset` by one viewport.
  - A second `Message::PageDown` clamps `scroll_offset` at
    `output.len().saturating_sub(viewport_height)`.
  - `Message::PageUp` subtracts one viewport from a nonzero
    `scroll_offset`.
  - A second `Message::PageUp` saturates `scroll_offset` at `0`.
- Keep the existing `tab_in_detail_is_ignored` test as the guard that
  detail mode remains an allow-list.
- Run `just test-rust` after the change.

## Assumptions

- Match the existing Normal-mode semantics exactly: Ctrl-D/U require
  `KeyModifiers::CONTROL`, not additional modifier combinations.
- Existing `PageDown`/`PageUp` app handling is sufficient because it
  already updates `scroll_offset` against `model.output` and
  `viewport_height`; the app-level tests above pin that contract.
- Ignore unrelated dirty worktree files. Production implementation should
  only touch `cli/src/browse/keymap.rs`; tests may also touch
  `cli/src/browse/app.rs`.
