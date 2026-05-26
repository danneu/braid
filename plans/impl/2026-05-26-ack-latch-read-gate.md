# Plan: pin the `Read` half of the ack `latch_corrupt` gate

## Context

`cmd_ack_impl` reads the alert latch once at entry and collapses *any*
`LatchLoadError` into a single "active for gating" flag:

```rust
// cli/src/ack.rs:42-49
let (causes, latch_corrupt) = match alert::load_alert_latch(paths) {
    Ok(Some(s)) => (s.causes, false),
    Ok(None)    => (Vec::new(), false),
    Err(e)      => { eprintln!("..."); (Vec::new(), true) }   // BOTH Read and Parse
};
```

`LatchLoadError` has two variants (`cli/src/alert.rs:307-313`): `Read`
(non-`NotFound` filesystem I/O failure, `alert.rs:327`) and `Parse`
(unparseable bytes). ADR 014 (`docs/design/decisions/014-alerts.md:118-122`)
makes the contract explicit: callers distinguish three outcomes
(`Ok(None)` / `Err(Read)` / `Err(Parse)`), and `cmd_ack` "treats a corrupt
latch as an active alert for gating ... otherwise a genuinely unmounted ack
would refuse with `PoolNotMounted` and the user would have no way to clear a
corrupt file with the pool offline."

**The gap:** every test that drives `latch_corrupt = true` writes
`b"not json"` -- exclusively the `Parse` branch (`ack.rs:353, 1281, 1461,
1755`). Nothing exercises the `Read` branch. A well-intentioned refactor that
narrowed the catch-all to `Err(LatchLoadError::Parse(_)) => true` and treated
`Read` as a transient fault (`Ok(None)` / not-corrupt) would silently regress
the offline recovery path to `PoolNotMounted`, and the full suite would stay
green. The whole `ack.rs` test module is built to pin exactly this class of
"future refactor narrows the gate" regression; the `Read`/`Parse` coverage
asymmetry is the smell this plan closes.

**Outcome:** add the missing `Read`-branch coverage so the offline gate's
contract is load-bearing in CI.

## Pivot from the original finding

The finding proposed triggering `Read` with "a latch path whose parent is
traversable but the file mode denies read" and asserting offline ack
"succeeds and clears." Two problems:

1. **Root-fragile.** A `chmod 000` read denial (`EACCES`) is bypassed by root.
   Under root -- a common CI uid -- `std::fs::read` *succeeds*, so the test
   would not exercise `Read` at all and would silently no-op exactly where the
   safety net matters most.
2. **"succeeds and clears" is unreachable for a robust `Read` trigger.** There
   is no portable, root-independent way to make `read` fail while `unlink`
   succeeds.

**Robust mechanism instead:** put a *directory* at the latch path. `std::fs::read`
on a directory fails with a non-`NotFound` error (`EISDIR`) on Linux and macOS
-> `LatchLoadError::Read`, deterministically and regardless of uid. This is the
idiom the file already uses 8x for forced `io::Error`s; `ack.rs:849-852`
documents the sibling fact that `remove_file` on a directory is a
platform-portable non-`NotFound` error (`EISDIR`/Linux, `EPERM`/macOS).

The same directory does double duty: it forces `Read` at load time, and it
makes cleanup's `remove_alert_latch` fail at the removal step. So the offline
ack reaches cleanup and returns `CleanupFailed` -- whereas a `Parse`-only
narrowing would short-circuit to `PoolNotMounted` *before* cleanup. Asserting
`CleanupFailed` (not `PoolNotMounted`) is the exact binary witness for "the
`Read` error drove `latch_corrupt = true` through the `has_alert` gate."

## Changes

### 1. Offline ack gate test (primary) -- `cli/src/ack.rs`

Add a `#[test]` in the existing `tests` module, modeled on
`ack_offline_cleanup_failure_after_missing_acked_returns_cleanup_failed`
(`ack.rs:1584`) but with a `Read`-erroring latch and empty causes. Prefix it
with a contiguous `//` line-comment preamble (`// Intent:` / `// Why it
exists:` / `// Scenario:`) per the literal form documented in
`docs/dev/testing.md:11` and `AGENTS.md` Test Conventions. (Some existing
neighbors drifted to `/* */` blocks; new tests use the `//` form.)

```rust
let (_dir, paths) = isolated_paths();
// A directory at the latch path forces LatchLoadError::Read (EISDIR) at
// load time -- root-independent, unlike chmod-based read denial -- and also
// makes remove_alert_latch fail during cleanup.
std::fs::create_dir(paths.alert_latch_json()).unwrap();
let beeper_calls = std::cell::Cell::new(0u32);
let beeper = || beeper_calls.set(beeper_calls.get() + 1);

let err = cmd_ack_impl(&AckPanicRunner, &ack_fs_not_mounted(), &ack_mp(), &paths, &beeper)
    .expect_err("unreadable latch on an offline pool must not silently no-op");

// CleanupFailed (not PoolNotMounted) proves the Read error gated as active:
// it passed has_alert and reached cleanup, which then failed removing the
// latch directory. A Read->not-corrupt regression returns PoolNotMounted
// before cleanup -- the exact ADR 014:122 recovery-contract violation.
assert!(
    matches!(err, AckError::CleanupFailed(_)),
    "Read-error latch must gate as active and reach cleanup, got {err:?}"
);
assert_eq!(beeper_calls.get(), 1, "stop_beeper fires before the failed removal");
assert!(paths.alert_latch_json().exists(), "latch directory cannot be removed by remove_file");
assert!(paths.alert_cleanup_pending().is_file(), "sentinel marked before the failed removal");
assert!(!paths.acked_stats_json().exists(), "no MissingDevice cause -> no acked-stats write");
```

Reused helpers (all `pub(crate)` in `cli/src/test_fixtures/`): `isolated_paths`
(`doctor.rs:26`), `ack_fs_not_mounted` / `ack_mp` / `AckPanicRunner`
(`ack.rs:150/129/20`). No new fixtures needed.

### 2. Loader-level companion test (required) -- `cli/src/alert.rs`

Add a `#[test]` beside `load_alert_latch_corrupt_returns_parse_err`
(`alert.rs:592`), mirroring its structure (`tempfile::tempdir()` + `.join(...)`
+ `load_alert_latch_at`) with a contiguous `//` line-comment preamble (same
documented form as test 1).

**Required, not optional.** The primary ack test asserts only `CleanupFailed`,
which *any* `latch_corrupt = true` path satisfies -- it cannot distinguish
`Read` from `Parse`. This loader test is the sole guard that a directory (a
non-`NotFound` I/O failure) maps to `LatchLoadError::Read` *specifically*,
completing ADR 014's three-way loader contract (`Ok(None)` / `Read` / `Parse`,
`alert.rs:307`) alongside the existing absent (`alert.rs:568`) and Parse
(`alert.rs:592`) tests. Without it, a regression that misclassified `Read` as
`Parse` -- or folded the two -- would pass the entire suite silently.

```rust
let dir = tempfile::tempdir().unwrap();
let path = dir.path().join("latch-as-dir");
// A directory makes std::fs::read fail with a non-NotFound io::Error
// (EISDIR), which must surface as Read -- not Parse, not Ok(None).
std::fs::create_dir(&path).unwrap();
let result = load_alert_latch_at(&path);
assert!(matches!(result, Err(LatchLoadError::Read(_))), "got {result:?}");
```

## Out of scope (considered, deliberately omitted)

- **Mounted `Read` variant.** The mounted gate (`ack.rs:80`) and offline gate
  (`ack.rs:128`) both read the *same* `latch_corrupt` from the *same* match arm
  (`ack.rs:42-49`). The catch-all narrowing this plan guards against breaks both
  at once, and the offline test catches it. A mounted-`Read` test would be
  redundant for guarding that arm.
- **`status.rs:607` / `monitor.rs:135`.** Both also fold `Read`+`Parse`
  identically, but their regressions are benign (status surfaces a
  `ComputationError` either way; monitor quarantines either way). Only ack's
  offline gate has the silent `PoolNotMounted` failure mode, so it is the only
  high-value pin.
- No production-code change. Current behavior is correct per ADR 014; this is
  pure test coverage.

## Verification

1. `just test-rust` -- confirm green (runs `cargo test` for `braid-cli`).
2. Optional load-bearing check (TDD confidence): temporarily change `ack.rs:45`
   to `Err(LatchLoadError::Parse(_)) => (Vec::new(), true), Err(_) => (Vec::new(), false)`,
   confirm the offline test fails with `PoolNotMounted` (and the loader test
   still passes -- it targets the loader, not the gate), then revert.
