# Plan: clarify pre-condition in `busy_when_scrub_running`

## Context

`cli/src/idle.rs` was reordered in commit `872e8ef` ("fix(idle): check sysfs
exclusive ops before scrub") so `cmd_idle` now checks
`/sys/fs/btrfs/*/exclusive_operation` *before* spawning `btrfs scrub status`.
That same commit changed `busy_when_scrub_running`'s fixture from
`IdleMockFs::mounted_btrfs_only()` (which carried a comment explaining the
deliberate skip) to `IdleMockFs::with_exclop("none")` -- but the explanatory
comment was dropped and never replaced.

The test still passes and still proves the right thing, but a reader scanning
the preamble can mistake it for "scrub running implies busy" as a top-level
invariant. The dependency on sysfs being seeded clean (so the probe order
actually reaches the scrub branch) is invisible. The complementary order
test `busy_exclop_short_circuits_scrub_probe` -- which proves sysfs wins
when both are busy -- is not cross-referenced from the running-scrub test,
so the two-test matrix isn't obvious either.

Intended outcome: a reader of `busy_when_scrub_running` can see at a glance
(a) that the test depends on a clean sysfs scan and (b) where the
complementary "sysfs wins" case is pinned. Code behavior is unchanged.

## Scope

- One file: `cli/src/idle.rs`.
- One test: `busy_when_scrub_running` (currently at lines 142-157).
- Add a `Pre-condition:` line to its preamble; do not touch the test body.
- Sibling scrub-layer tests (`busy_unknown_on_scrub_probe_failure`,
  `busy_unknown_on_scrub_parse_failure`,
  `busy_unknown_on_scrub_state_unknown`) are intentionally **out of scope**
  -- their names already announce "scrub" failures, making the layer
  dependency self-evident.

## Change

Edit the preamble of `busy_when_scrub_running` in
`cli/src/idle.rs:142-146`.

Before:

```rust
// Intent: Scrub running -> Busy with percentage from subprocess parser.
// Why: Scrub is not in the kernel exclop set; only `btrfs scrub status`
//   sees it. Suspending mid-scrub interrupts data integrity verification.
// Scenario: Monthly auto-scrub is in progress when autosuspend checks.
#[test]
fn busy_when_scrub_running() {
```

After:

```rust
// Intent: Scrub running -> Busy with percentage from subprocess parser.
// Why: Scrub is not in the kernel exclop set; only `btrfs scrub status`
//   sees it. Suspending mid-scrub interrupts data integrity verification.
// Scenario: Monthly auto-scrub is in progress when autosuspend checks.
// Pre-condition: sysfs is seeded clean so the scrub probe is actually
//   reached -- the sysfs-first order is exercised by
//   `busy_exclop_short_circuits_scrub_probe`.
#[test]
fn busy_when_scrub_running() {
```

Notes on form:

- Plain ASCII `--` (per the global writing-style rule and the project's CLI
  output style); no em-dashes.
- The new line is a fourth labeled section after the project's required
  three (Intent / Why / Scenario). The three-section contract in
  `AGENTS.md` and `docs/testing.md:11-22` does not forbid additional
  labeled lines, and other tests in `idle.rs` already use ad-hoc inline
  comments to explain fixture-state choices (e.g.
  `idle_skips_features_and_debug_pseudo_dirs` at line 484-498).
- Cross-reference to `busy_exclop_short_circuits_scrub_probe` makes the
  two-test matrix explicit: this test pins "sysfs clean, scrub running ->
  Busy::ScrubRunning"; the named test pins "sysfs busy short-circuits the
  scrub probe entirely."

## Critical files

- `cli/src/idle.rs` -- only file modified.

## Files / utilities reused (for context, not modified)

- `IdleMockFs::with_exclop(body)` -- `cli/src/test_fixtures/idle.rs:64-67`.
  Sets the fixture so `cmd_idle` clears the mountinfo check, walks
  `/sys/fs/btrfs/` with one seeded fsid dir, and reads `body` from its
  `exclusive_operation` file. Seeding `"none"` is precisely how a test
  reaches the scrub probe past a clean sysfs scan.
- `busy_exclop_short_circuits_scrub_probe` -- `cli/src/idle.rs:208-225`.
  The complementary test the new preamble line references.

## Verification

- `just test-rust` -- the only behavioural surface that could regress is
  the test module itself; this confirms the file still compiles and every
  existing test still passes.
- Visual: re-read the four-line preamble for `busy_when_scrub_running` to
  confirm the ASCII `--` rendering and that the cross-reference name
  matches `busy_exclop_short_circuits_scrub_probe` exactly.

No fixture refresh, VM test, or parser-canary work is required -- this is
a pure comment edit with zero runtime effect.
