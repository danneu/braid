# plan-the-ideal-refactor-rustling-pond

## Context

A code-review finding at `cli/src/unlock.rs:128` flagged that
`UnlockPlan::execute` silently swallows every `ProbeError` variant from
its post-mount `probe::probe_pool` call. The doc-comment justifies one
case (a benign `mounted=false` race), but the `if let Ok(...)` arm
absorbs the rest -- `BtrfsFilesystemShow` non-zero exit, `cryptsetup
status` parse failure, command spawn failure, etc. When that happens
in production, `pool.json` enrichment skips silently, no warning is
emitted, and `refresh_pool_metadata`'s own per-failure `Warning:`
lines (`cli/src/membership.rs:651, 658, 663`) never get a chance to
fire because the outer probe never reached them.

Investigating the finding shows the same swallow shape lives at two
sibling callsites:

- `cli/src/add.rs:1244` -- bootstrap-add post-mkfs enrichment.
- `cli/src/replace.rs:806` -- post-`btrfs replace` enrichment.

The `2026-04-23` plan that pinned `unlock`'s `mounted=false` tolerance
(`plans/impl/2026-04-23-pin-unlock-post-mount-probe-mounted-false.md`)
explicitly carved this consistency sweep out of scope: *"`add.rs` and
`replace.rs` share the same shape, so any restructuring should sweep
all three callers and is beyond this finding."* This plan is that
sweep.

The intended outcome: when any of the three best-effort post-mount
probes fails, the operator sees one `Warning:` line on stderr telling
them enrichment was skipped and why, while the command still succeeds
(today's tolerance contract is preserved). Behavior on the `Ok` path
-- including the `mounted=false` no-op race already pinned by
`unlock_tolerates_post_mount_probe_mounted_false` -- is unchanged.

## Scope

Three callsites, all best-effort post-mount enrichment. No other
swallow sites exist (Explore agent confirmed via grep for
`if let Ok(.*probe_pool`, `let _ = probe_pool`, and `.ok()` chains in
`cli/src/`).

| Site | File:line | Today on Err | Today on Ok |
| --- | --- | --- | --- |
| `unlock` | `cli/src/unlock.rs:128-130` | silent skip | `membership::refresh_pool_metadata(&pool_after, params.paths)` |
| `add` (bootstrap) | `cli/src/add.rs:1244-1246` | silent skip | `membership::enrich_from_pool_state(&mut final_membership, &pool_after)?` |
| `replace` | `cli/src/replace.rs:806-811` | silent skip | `membership::enrich_from_pool_state(&mut target_membership, &pool_after)?` |

Out of scope:

- The non-bootstrap `add.rs:1258` and `add.rs:1282` probes already
  propagate `Err` with `?` -- they're fail-closed by design and stay
  that way.
- The `Ok(mounted=false)` race already documented at `unlock.rs:124`
  and pinned by `unlock_tolerates_post_mount_probe_mounted_false`.
  This plan does not touch that path.
- Other `if let Ok(...)`-style swallow patterns elsewhere in the CLI
  (TUI probes, parser fallbacks). Those have different semantics.

## Change

At each of the three sites, replace
```rust
if let Ok(pool_after) = probe_pool(...) {
    /* enrich */
}
```
with
```rust
match probe_pool(...) {
    Ok(pool_after) => { /* enrich */ }
    Err(e) => crate::status_tag::emit_status(&format!(
        "Warning: failed to probe pool for metadata refresh: {e}\n"
    )),
}
```

Wording rationale: matches the three existing `Warning: failed to ...
for metadata refresh: {e}` / `Warning: failed to enrich ...` /
`Warning: failed to save ...` lines in `membership.rs:651, 658, 663`
(same shape, same capitalization, no command-name prefix, in line with
the convention across the codebase -- the Explore pass confirmed no
existing warning prefixes the command name).

Emitter rationale: route through `crate::status_tag::emit_status`
rather than raw `eprintln!`. `emit_status` (`cli/src/status_tag.rs:66`)
falls through to `eprint!("{line}")` outside test capture, so the
runtime user-visible bytes are byte-for-byte identical to a raw
`eprintln!`; the difference matters only inside `#[cfg(test)]`, where
`emit_status` routes the line into the thread-local capture buffer
that `status_tag::testing::capture_with_color` reads. The membership.rs
warnings stay on raw `eprintln!` because no test pins them; this plan
introduces three new tests that pin the new warning, so it must use
the capturable emitter. The `cleanup failed: ...` warning at
`cli/src/mount.rs:764-767` already uses the same `emit_status` shape
and is pinned by `cmd_unlock_preserves_mount_error_when_cleanup_close_fails`
(`cli/src/unlock.rs:522-538`); we are following that precedent, not
inventing a new one.

The text is identical at all three sites so the warning is greppable
as one string. No helper extraction: the project convention (from
CLAUDE.md and the `membership.rs` precedent above) is to inline these
short `if let Err(e) = ...` arms. Three nearly-identical `emit_status`
calls is below the abstraction threshold; a `probe_pool_or_warn`
helper would only save one line per site.

### Per-site edits

**1. `cli/src/unlock.rs:122-130`**

Before:
```rust
// Enrich pool.json with live metadata (devid, added_at) -- best-effort.
// A rare race where probe_pool sees mounted=false after a successful
// mount leaves `pool_after.devices` empty, so refresh_pool_metadata
// no-ops. That is acceptable: correctness never depends on this write
// (see contract above). Pinned by
// unlock_tolerates_post_mount_probe_mounted_false.
if let Ok(pool_after) = probe::probe_pool(runner, fs, mount_point) {
    membership::refresh_pool_metadata(&pool_after, params.paths);
}
```
After:
```rust
// Enrich pool.json with live metadata (devid, added_at) -- best-effort.
// A rare race where probe_pool sees mounted=false after a successful
// mount leaves `pool_after.devices` empty, so refresh_pool_metadata
// no-ops. That is acceptable: correctness never depends on this write
// (see contract above). Pinned by
// unlock_tolerates_post_mount_probe_mounted_false. A real probe error
// (command failure, parse failure, or mountinfo error) emits one
// Warning line and skips enrichment without failing unlock, pinned
// by unlock_warns_when_post_mount_probe_errors.
match probe::probe_pool(runner, fs, mount_point) {
    Ok(pool_after) => {
        membership::refresh_pool_metadata(&pool_after, params.paths);
    }
    Err(e) => crate::status_tag::emit_status(&format!(
        "Warning: failed to probe pool for metadata refresh: {e}\n"
    )),
}
```

**2. `cli/src/add.rs:1237-1247`**

Before:
```rust
// Bootstrap post-commit persist: write pool.json after mkfs + mount.
// Enrich with live metadata (devid) from pool probe.
// `enrich_from_pool_state` correlates by LUKS UUID only and
// surfaces any foreign UUIDs through `EnrichmentReport`;
// downstream consumption of `foreign` lives in doctor/status
// (Phase 5), so the report is discarded here.
let mut final_membership = journal.target_membership.clone();
if let Ok(pool_after) = probe_pool(runner, fs, mount_point) {
    let _ = membership::enrich_from_pool_state(&mut final_membership, &pool_after)?;
}
membership::save_membership(&final_membership, params.paths)?;
```
After:
```rust
// Bootstrap post-commit persist: write pool.json after mkfs + mount.
// Enrich with live metadata (devid) from pool probe.
// `enrich_from_pool_state` correlates by LUKS UUID only and
// surfaces any foreign UUIDs through `EnrichmentReport`;
// downstream consumption of `foreign` lives in doctor/status
// (Phase 5), so the report is discarded here.
// A probe failure here emits one Warning line and skips enrichment,
// pinned by cmd_add_bootstrap_warns_when_post_mount_probe_errors.
let mut final_membership = journal.target_membership.clone();
match probe_pool(runner, fs, mount_point) {
    Ok(pool_after) => {
        let _ = membership::enrich_from_pool_state(&mut final_membership, &pool_after)?;
    }
    Err(e) => crate::status_tag::emit_status(&format!(
        "Warning: failed to probe pool for metadata refresh: {e}\n"
    )),
}
membership::save_membership(&final_membership, params.paths)?;
```

**3. `cli/src/replace.rs:800-811`**

Before:
```rust
// Membership committed by btrfs replace. Enrich with kernel-assigned
// devid + observed luks_uuid from a fresh probe, then persist before
// the post-replace cleanup, resize, and (missing-path) soft balance.
// The journal still covers maintenance, so recovery can replay it if
// we crash before clear_journal.
let mut target_membership = target_membership;
if let Ok(pool_after) = probe_pool(runner, fs, config.mount_point()) {
    // Discard `EnrichmentReport.foreign` for now; doctor/status
    // wiring is Phase 5. The function is fail-closed on
    // foreign-UUID admission (no insert), so the discard is safe.
    let _ = membership::enrich_from_pool_state(&mut target_membership, &pool_after)?;
}
```
After:
```rust
// Membership committed by btrfs replace. Enrich with kernel-assigned
// devid + observed luks_uuid from a fresh probe, then persist before
// the post-replace cleanup, resize, and (missing-path) soft balance.
// The journal still covers maintenance, so recovery can replay it if
// we crash before clear_journal. A probe failure here emits one
// Warning line and skips enrichment, pinned by
// cmd_replace_warns_when_post_mount_probe_errors.
let mut target_membership = target_membership;
match probe_pool(runner, fs, config.mount_point()) {
    Ok(pool_after) => {
        // Discard `EnrichmentReport.foreign` for now; doctor/status
        // wiring is Phase 5. The function is fail-closed on
        // foreign-UUID admission (no insert), so the discard is safe.
        let _ = membership::enrich_from_pool_state(&mut target_membership, &pool_after)?;
    }
    Err(e) => crate::status_tag::emit_status(&format!(
        "Warning: failed to probe pool for metadata refresh: {e}\n"
    )),
}
```

## Test coverage

Three new unit tests, one per site. Each pins:

1. `Err` from `probe_pool` does not turn the command into a hard
   failure (preserves today's tolerance).
2. The exact `Warning:` line lands on stderr.

Behavior-only assertions; no structural inspection. All three tests
use `crate::status_tag::testing::capture_with_color(false, || ...)`
to capture stderr, matching the existing
`cmd_unlock_preserves_mount_error_when_cleanup_close_fails` capture
pattern at `cli/src/unlock.rs:522-538`.

### Test 1: `unlock_warns_when_post_mount_probe_errors`

Location: `cli/src/unlock.rs::tests`, sibling to
`unlock_tolerates_post_mount_probe_mounted_false`.

Builds a runner identical to the happy-path unlock plus a
`.with_output(CmdRequest::BtrfsFilesystemShow { mount_point }, err_raw("btrfs filesystem show", 1, "no devices found"))`
so `probe_pool` errors after the successful mount. Filesystem must
have `/mnt/storage` present in mountinfo so the probe gets past the
`mounted=false` short-circuit and reaches `BtrfsFilesystemShow`. Use
`mount_fs(&[...])` plus a `with_mountinfo_for(...)` helper if one
exists, or model on
`unlock_warns_on_paused_balance` (`cli/src/unlock.rs:602`) which
already produces a successful mount + mountinfo state.

Asserts:
- `cmd_unlock(...)` returns `Ok(())`.
- Captured stderr contains exactly one occurrence of
  `"Warning: failed to probe pool for metadata refresh: "`.
- Captured stderr also contains `"no devices found"` -- the seeded
  stderr from the `err_raw(...)` mock, which `ParseError::CommandFailed`
  Display (`cli/src/parse/mod.rs:50`) renders into the `{e}` interpolation.
  Pinning this substring guards against an implementation that drops
  `{e}` and still passes the prefix check.

The redundant negative assertion against `refresh_pool_metadata`'s own
warnings (which use raw `eprintln!` and would never appear in the
capture buffer regardless) is intentionally omitted.

Preamble: standard Intent / Why it exists / Scenario block per
`docs/testing.md`.

### Test 2: `cmd_add_bootstrap_warns_when_post_mount_probe_errors`

Location: `cli/src/add.rs::tests`, sibling to
`cmd_add_bootstrap_clears_acked_stats_before_probe_enrich` (line
~4178).

Reuses the existing `AddFullPathRunner::bootstrap()` fixture +
`.with_bootstrap_post_mount_probe_failure()` builder
(`cli/src/add.rs:3727, 3734`). That builder already injects a probe
failure via `fail_bootstrap_post_mount_probe = true`
(`cli/src/add.rs:3920-3922` returns
`Err(CmdError::Failed("post-mount probe failed".into()))`).

Asserts:
- `cmd_add(...)` returns `Ok(())`.
- Captured stderr contains
  `"Warning: failed to probe pool for metadata refresh: "`.
- Captured stderr also contains `"post-mount probe failed"` -- the
  literal text the bootstrap fixture returns inside
  `CmdError::Failed(...)` at `cli/src/add.rs:3920-3922`. Pins that
  `{e}` survives the warning interpolation.
- `pool.json` is still written (membership saved before `clear_journal`).

The existing
`cmd_add_bootstrap_clears_acked_stats_before_probe_enrich` test stays
unchanged: its assertion is about acked-stats deletion ordering, not
about warning emission. Two siblings, two distinct Why-it-exists
preambles.

### Test 3: `cmd_replace_warns_when_post_mount_probe_errors`

Location: `cli/src/replace.rs::tests`.

The existing replace tests build runners via
`crate::test_fixtures::ReplacementPool` (`cli/src/test_fixtures/replace.rs`)
-- e.g. `ReplacementPool::two_disk_healthy().install(MockRunner::default(), replace_done)`.
Its `BtrfsFilesystemShow` arm at `cli/src/test_fixtures/replace.rs:172`
already flips state-dependent outputs based on the `replace_done`
`AtomicBool`. Add a post-replace probe-failure injector minimally:

- Add a `fail_post_replace_probe: bool` field (default `false`) to
  the `ReplacementPool` struct, plus a
  `with_post_replace_probe_failure()` builder, plus one conditional
  arm in the existing `BtrfsFilesystemShow` handler that returns
  `Err(CmdError::Failed("post-replace probe failed".into()))` once
  `replace_done` is set AND the new flag is true. Mirror the shape of
  `add.rs:3734` / `add.rs:3920-3922` exactly.

Test scaffolding: `ReplacementPool::install` does NOT handle
`CmdRequest::BtrfsReplaceStart` (its `match` arms cover only
`BtrfsFilesystemShow`, `BtrfsDeviceUsageRaw`, `CryptsetupStatus`,
`CryptsetupLuksUuid`, `CryptsetupLuksDumpText`, `BtrfsBalanceStatus`,
`BtrfsDeviceStatsJson`, `CryptsetupTestPassphrase`); existing
success-path tests like `live_replace_old_close_failure_emits_warn_row`
(`cli/src/replace.rs:3170-3218`) chain a per-test `.with_handler(...)`
that flips `replace_done` and stubs the post-commit commands. The new
test must do the same. Required handler arms (mirroring the model
test exactly):

- `CmdRequest::BtrfsReplaceStart { .. }` -> set
  `replace_done.store(true, std::sync::atomic::Ordering::Relaxed)`
  and return `Some(Ok(mock_ok("btrfs replace start", "")))`. This
  flip is what makes the `ReplacementPool` `BtrfsFilesystemShow` arm
  read post-state -- and, with the new `fail_post_replace_probe`
  flag set on the fixture, what makes it return the seeded `Err`.
- `CmdRequest::CryptsetupClose { .. }` -> `Some(Ok(mock_ok("cryptsetup close", "")))`
  (live-only old-mapper close at `cli/src/replace.rs:863-876`).
- `CmdRequest::BtrfsFilesystemResize { .. }` -> `Some(Ok(mock_ok("btrfs filesystem resize", "")))`
  (the resize at `cli/src/replace.rs:878`).
- `_ => None` to fall through to the `ReplacementPool` install arms
  for everything else (`probe_observed_mapper_uuid` reads route through
  `CryptsetupStatus` + `CryptsetupLuksUuid`, both handled by the
  install arms).

Use `PoolFixture::two_disk_healthy()` and the same `MockFs::storage(...)`
and `f.replace_params().build()` plumbing the model test uses.

Asserts:
- `cmd_replace(...)` returns `Ok(...)`.
- Captured stderr contains
  `"Warning: failed to probe pool for metadata refresh: "`.
- Captured stderr also contains `"post-replace probe failed"` -- the
  literal returned by the new fixture arm. Pins that `{e}` survives
  the warning interpolation.
- `pool.json` is still written (membership saved after the match
  block at `replace.rs:817`).

If extending `ReplacementPool` in this way crosses a complexity
boundary worth its own commit, split it: commit 1 = warning + unlock
test + add test, commit 2 = `ReplacementPool` extension + replace
test. Decide at implementation time based on diff size.

### Negative coverage already in place

- `unlock_tolerates_post_mount_probe_mounted_false`
  (`cli/src/unlock.rs:955`) -- pins that `Ok(mounted=false)` continues
  to no-op. Today it asserts only success and non-enrichment; it does
  NOT capture stderr, so a regression that emitted the new warning on
  the benign `mounted=false` race would silently pass. Extend this
  test to wrap `cmd_unlock(...)` in
  `crate::status_tag::testing::capture_with_color(false, || ...)`
  (mirroring the wrapper at
  `cli/src/unlock.rs:522-538`) and assert that the captured string
  does NOT contain `"Warning: failed to probe pool for metadata
  refresh: "`. Update the test's Why-it-exists preamble to name the
  no-warning invariant explicitly.
- `cmd_add_bootstrap_clears_acked_stats_before_probe_enrich`
  (`cli/src/add.rs:4178`) -- pins that bootstrap survives a probe
  failure. Must still pass; the new warning does not affect its
  acked-stats assertion.

## Critical files

- `cli/src/unlock.rs` -- one production-site edit + one new test +
  one extension to `unlock_tolerates_post_mount_probe_mounted_false`.
- `cli/src/add.rs` -- one production-site edit + one new test
  (reuses existing fixture).
- `cli/src/replace.rs` -- one production-site edit + one new test.
- `cli/src/test_fixtures/replace.rs` -- adds the
  `fail_post_replace_probe` field, builder, and conditional arm on
  `ReplacementPool` (the `BtrfsFilesystemShow` handler at line 172).
  This is where the replace probe-failure injector belongs -- the
  test lives in `replace.rs::tests` but the fixture machinery is in
  the shared module.

No production code outside `unlock.rs`, `add.rs`, and `replace.rs`
changes. No principles doc, decision record, or README update needed
-- the change is a behavior tightening within the existing
"best-effort post-mount enrichment" contract, not a new invariant.

## Verification

1. `cargo fmt` -- no formatting regressions.
2. `just test-rust` -- full Rust suite passes, including the two
   pre-existing tolerance tests above (the
   `unlock_tolerates_post_mount_probe_mounted_false` extension is
   covered here).
3. `cargo test --lib warns_when_post_mount_probe_errors` -- substring
   filter that matches all three new test names
   (`unlock_warns_when_post_mount_probe_errors`,
   `cmd_add_bootstrap_warns_when_post_mount_probe_errors`,
   `cmd_replace_warns_when_post_mount_probe_errors`). The
   `just test-rust` recipe takes no arguments, so use the raw cargo
   form for targeted runs.
4. `grep -rn 'Warning: failed to probe pool for metadata refresh' cli/src/`
   -- expect exactly 3 production hits (one per site) plus 4
   test-side string-literal hits: one positive assertion in each of
   the three new `*_warns_when_post_mount_probe_errors` tests, plus
   one negative assertion in the extended
   `unlock_tolerates_post_mount_probe_mounted_false`.
5. `just test-vm` -- the NixOS VM tests for `unlock`, `add`, and
   `replace` continue to pass; no behavior change expected at the VM
   level because the probe `Err` arm is rare in real runs and the
   warning is informational only.
