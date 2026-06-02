# TUI Snapshot Testing with Ratatui + Insta

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

## The cargo insta workflow

1. **`cargo test`** — runs tests normally. New/changed snapshots fail and produce `.snap.new` files alongside the existing `.snap` files.
2. **`cargo insta review`** — interactive TUI that walks through each pending change with diffs. Keys: `a` accept, `r` reject, `s` skip.
3. **`cargo insta accept`** — bulk-accepts all pending `.snap.new` files without review.

**Shortcut:** `cargo insta test --review` runs tests then immediately opens the review TUI.

## Typical cycle

```
# Write or change a test → run tests
just test-rust

# Tests fail because snapshot is new/different → .snap.new files appear
# Review the diffs interactively
cargo insta review

# Or if you trust the output, bulk accept
cargo insta accept

# Commit the .snap files
```

For first-time snapshots (no `.snap` file yet), `cargo test` will always fail — run `cargo insta review` or `cargo insta accept` to create the initial `.snap`.

## What ratatui recommends

- **`TestBackend` + insta** for integration-level view tests (what we do)
- **`Buffer::empty()` + direct render** for unit-testing individual widgets in isolation, asserting on buffer contents without a full terminal
- Consistent terminal dimensions (e.g., 80x20) for reproducible snapshots
