# TUI Snapshot Testing with Ratatui + Insta

## Rendering for snapshots

Create a `Terminal` with `TestBackend`, draw into it, then assert:

```rust
let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
terminal.draw(|frame| frame.render_widget(&app, frame.area())).unwrap();
assert_snapshot!(terminal.backend());
```

`TestBackend` implements `Display`, so insta captures the text grid. Styles/colors are **not** captured — text only.

Our codebase uses a custom `buffer_to_string` helper in `cli/src/tui/view.rs` that trims trailing whitespace per line. Either approach works; ours produces slightly cleaner diffs.

## The cargo insta workflow

1. **`cargo test`** — runs tests normally. New/changed snapshots fail and produce `.snap.new` files alongside the existing `.snap` files.
2. **`cargo insta review`** — interactive TUI that walks through each pending change with diffs. Keys: `a` accept, `r` reject, `s` skip.
3. **`cargo insta accept`** — bulk-accepts all pending `.snap.new` files without review.

**Shortcut:** `cargo insta test --review` runs tests then immediately opens the review TUI.

## Typical cycle

```
# Write or change a test → run tests
cargo test -p braid-cli

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
