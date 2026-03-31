# Deduplicate `now_iso()` into a `util` module

## Context

`now_iso()` is implemented identically in 4 places:
- `cli/src/membership.rs:175` — private `fn now_iso()`
- `cli/src/journal.rs:81` — private `fn now_iso()` (exact duplicate)
- `cli/src/add.rs:549` — inlined same logic
- `cli/src/replace.rs:352` — inlined same logic

All four do: `time::OffsetDateTime::now_utc().format(&Iso8601::DEFAULT).expect(...)`.

## Plan

1. **Create `cli/src/util.rs`** with a single `pub fn now_iso() -> String`
2. **Add `pub mod util;`** to `cli/src/lib.rs`
3. **Delete `fn now_iso()`** from `cli/src/membership.rs:175-180`, replace call sites with `crate::util::now_iso()`
4. **Delete `fn now_iso()`** from `cli/src/journal.rs:81-86`, replace call site with `crate::util::now_iso()`
5. **Replace inline formatting** in `cli/src/add.rs:547-551` with `crate::util::now_iso()`
6. **Replace inline formatting** in `cli/src/replace.rs:350-354` with `crate::util::now_iso()`

## Files modified

- `cli/src/util.rs` (new)
- `cli/src/lib.rs`
- `cli/src/membership.rs`
- `cli/src/journal.rs`
- `cli/src/add.rs`
- `cli/src/replace.rs`

## Verification

`just test-rust` — confirms compilation and existing unit tests pass.
