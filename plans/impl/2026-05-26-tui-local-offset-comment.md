# Document why `current_local_offset()` is sound in the TUI render loop

## Context

A code review flagged `cli/src/tui/mod.rs:79` as a Medium correctness bug:
"`current_local_offset()` always fails in the threaded TUI, so every timestamp
renders in UTC instead of local time." The claim is that once
`InputHandler::new()` and the probe effects spawn threads, the `time` crate
refuses to resolve the local offset (`IndeterminateOffset`), the
`.unwrap_or(UtcOffset::UTC)` branch is always taken, and the Scrub tab's
absolute/relative timestamps skew by the host offset.

Investigation showed the claim is **stale, not real**, for the pinned
`time` 0.3.47:

- The multithreaded restriction was removed in `time` 0.3.37. The thread gate
  (`num_threads::is_single_threaded()`) now lives only in
  `sys/refresh_tz/unix.rs`, reachable solely via the public
  `time::util::refresh_tz()` -- which braid never calls.
- The internal `sys/local_offset_at/unix.rs::local_offset_at` (shared by Linux
  and macOS, no `#[cfg]`/thread branch) calls `localtime_r` directly and never
  consults `refresh_tz`. So `current_local_offset()` cannot return
  `IndeterminateOffset` merely because the process is multithreaded.
- The crate confirms the cutover: `local_offset::set_soundness` is
  `#[deprecated(since = "0.3.37", note = "no longer needed; TZ is refreshed
  manually")]`.
- Empirically reproduced with a standalone `=0.3.47` binary (same features):
  with 4 extra threads live, `current_local_offset()` returned
  `Ok(-05:00:00)`.

So `now` is already correct naive-local time, it shares a basis with
`parse_ctime`'s naive-local scrub `ctime` (`cli/src/parse/helpers.rs:7`), and
`timeago`/`format_timestamp` (`cli/src/tui/view/mod.rs:109`, `:26`) are correct.
No behavioral change is needed.

The remaining problem is **readability**: the
`current_local_offset().unwrap_or(UTC)` idiom reads exactly like the famous
pre-0.3.37 `time` footgun, so it will keep getting re-flagged. The fix is a
single clarifying comment. Intended outcome: a future reader (or reviewer/agent)
can see from the code why the offset resolution is sound and why a UTC `now`
would be wrong, without re-deriving it from `time` crate internals.

Explicitly out of scope: the finding's proposed "capture the offset once in
`main.rs` while single-threaded and cache it on the `Model`" fix. There is no
real bug to fix, so it is unnecessary -- and it would add `Model` state plus a
startup-ordering constraint (the offset must be captured before any thread
spawns) for no benefit.

## Change

Single file: `cli/src/tui/mod.rs`. Add a `//` comment block immediately above
the `let now = {` block at line 78 (inside `run_loop`). No code lines change.

Proposed comment text:

```rust
// `now` is naive-LOCAL on purpose: it must share a time basis with the scrub
// `ctime`, which parse_ctime returns as a naive-local PrimitiveDateTime. A UTC
// `now` here would skew the relative timeago text by the host's offset.
//
// current_local_offset() is sound despite the multithreaded TUI: time >= 0.3.37
// dropped the old "fail when multithreaded" rule and calls localtime_r directly,
// so unwrap_or(UTC) guards only a genuine localtime failure, not thread count.
let now = {
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let local = time::OffsetDateTime::now_utc().to_offset(offset);
    time::PrimitiveDateTime::new(local.date(), local.time())
};
```

Each clause carries information a reader cannot recover from braid's own code:

1. **naive-local basis** -- the `parse_ctime` <-> `now` coupling is implicit and
   split across two files; this is the real correctness invariant.
2. **sound when multithreaded** -- the rationale lives in `time` crate
   internals; without it, `unwrap_or(UTC)` looks like a constant-UTC trap.

Both clauses are load-bearing; neither states more than the code guarantees.
Note the comment deliberately makes no claim about elapsed-time correctness
across a DST boundary: `timeago` subtracts two naive `PrimitiveDateTime`s
(`cli/src/tui/view/mod.rs:110`) and never consults an offset, so re-resolving
the offset per frame would not make that diff DST-correct.

Style constraints (per repo conventions): ASCII only, `--` not em-dash, `>=`
not the Unicode glyph.

## Verification

This is a comment-only change, so correctness is "still compiles, no behavior
change":

- `just test-rust` -- confirms the crate still builds and unit tests pass
  (comment-only, so this is a smoke check, not new coverage).
- `git diff cli/src/tui/mod.rs` -- confirm the diff is purely the added comment
  lines and that the four `let now` / offset lines are byte-for-byte unchanged.

No new tests: the offset-resolution path is now just a `localtime_r` call with a
genuine-failure fallback; there is no structure-insensitive behavioral assertion
worth adding, and the existing snapshot tests (fixed `now`) already cover the
rendering. Adding a TZ-dependent test would be environment-coupled and brittle.
