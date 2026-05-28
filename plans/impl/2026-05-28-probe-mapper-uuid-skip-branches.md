# Plan: pin the post-commit close probe-failure branches

## Context

A `/verify-issue` review of a Low / Testing finding flagged a gap in
post-commit close coverage for `cmd_remove`: the existing test
`post_commit_close_uuid_probe_demotes_to_skip_on_mismatch`
(`cli/src/remove.rs:3054`) only exercises the *UUID-mismatch* arm of
`probe_observed_mapper_uuid`. The finding's headline fix was a single
new test in `remove.rs` for the `cryptsetup status` runner-error case.

Looking at the helper directly
(`cli/src/probe_mapper_uuid.rs:30-117`), there are **seven** paths to
`return false` and one path to `true`. Only two of those eight paths
have coverage anywhere in the crate:

| Branch in `probe_observed_mapper_uuid` | Lines | Covered? |
| --- | --- | --- |
| `runner.run(CryptsetupStatus)` returns `Err` | 39-47 | no |
| `parse_cryptsetup_status` returns `Err` | 51-58 | no |
| backing device is `(null)` or empty string | 64-70 | no |
| backing device is `None` (mapper inactive) | 72-78 | no |
| `runner.run(CryptsetupLuksUuid)` returns `Err` | 86-94 | no |
| `parse_cryptsetup_luks_uuid` returns `Err` | 107-115 | no |
| parsed UUID does not equal `expected_uuid` | 98-106 | yes (remove.rs:3054, replace.rs:6359) |
| parsed UUID equals `expected_uuid` (→ true) | 96-97 | yes (remove.rs:2921, replace.rs:6413) |

Risk: any of the six untested branches getting flipped to `true` (or to
a hard error) by a future refactor would tear down a foreign dm slot
after the irreversible btrfs commit, or hard-error a command past the
point of safe retry. The existing tests would not catch it because they
all feed the probe a *successful* status + luksUUID round-trip whose
parse result happens to mismatch -- a single happy-path input shape.

The finding scoped the fix too narrowly (one test, in the wrong module).
The pivot: add unit tests directly to `probe_mapper_uuid.rs` covering
every early-return-`false` branch. The helper is now its own module
(extracted in commit `844ed0f`); the structurally correct place for its
tests is the module itself, matching the inline `#[cfg(test)] mod tests`
pattern already established by other small helpers in the crate
(`cli/src/util.rs:37-69`, `cli/src/state_paths.rs:53-104`).

## Outcome

After this change:

- Every `return false` branch in `probe_observed_mapper_uuid` is pinned
  by a unit test that calls the helper directly.
- A future refactor that flips any of those branches to `true` or
  propagates an `Err` instead of skipping fails a test by name.
- The existing integration tests at `remove.rs:3054` and
  `replace.rs:6359/6413` continue to pin the helper -> caller wiring
  (probe `false` -> command returns `Ok` with zero closes); they are
  unchanged.

## Test module

Add a `#[cfg(test)] mod tests { ... }` block at the bottom of
`cli/src/probe_mapper_uuid.rs` with one test per missing branch. Each
test follows the direct-call pattern from
`replace.rs:6359-6428`:

```rust
let runner = MockRunner::default().with_handler(/* per-scenario */);
let mapper = MapperName("braid-WRONG".into());
let expected = test_uuid(seed);
let matched = probe_observed_mapper_uuid(&runner, &mapper, &expected);
assert!(!matched, "<branch> must signal skip-close");
// optionally: assert runner.requests() shape to pin which probe ran
```

Use `with_handler` (not `with_output`) for the `Err`-returning scenarios.
`MockRunner::with_output` only injects success bodies; injecting
`Err(CmdError::Failed(_))` requires the handler form, demonstrated at
`cli/src/doctor.rs:1911-1916`. For the success-shaped-but-bad-content
scenarios (`(null)` backing, empty backing, inactive mapper, unparseable
luksUUID body), `with_output` returning a hand-rolled `mock_ok(...)` is
sufficient.

### Tests to add

1. `probe_returns_false_when_cryptsetup_status_runner_errs` -- handler
   returns `Some(Err(CmdError::Failed("cryptsetup status: not found")))`
   for `CryptsetupStatus`. Assert helper returns `false` and the runner
   recorded exactly one `CryptsetupStatus` and zero `CryptsetupLuksUuid`
   requests (proves the early-return short-circuits the second probe).

2. `probe_returns_false_when_status_parse_fails` -- handler returns
   `Ok(mock_ok(...))` with body that
   `parse_cryptsetup_status` rejects (e.g. an empty string, or
   "garbage\n"). Assert helper returns `false`; no `CryptsetupLuksUuid`
   recorded.

3. `probe_returns_false_when_backing_device_is_null` -- handler returns
   a well-formed active status whose `device:` line is `(null)`. Assert
   helper returns `false`; no `CryptsetupLuksUuid` recorded.

4. `probe_returns_false_when_mapper_is_inactive` -- handler returns a
   status body that parses to `is_active: false` / `device: None`.
   Assert helper returns `false`; no `CryptsetupLuksUuid` recorded.

5. `probe_returns_false_when_luks_uuid_runner_errs` -- handler returns
   a well-formed active status pointing at `/dev/vdc`, and returns
   `Some(Err(CmdError::Failed(...)))` for
   `CryptsetupLuksUuid { device: "/dev/vdc" }`. Assert helper returns
   `false`; exactly one `CryptsetupStatus` and one `CryptsetupLuksUuid`
   recorded.

6. `probe_returns_false_when_luks_uuid_parse_fails` -- handler returns
   a well-formed active status, then returns
   `Ok(mock_ok("cryptsetup luksUUID ...", "not-a-uuid\n"))` for the
   luksUUID probe. Assert helper returns `false`; one of each probe
   recorded.

Each test gets the project-standard three-section preamble (Intent /
Why / Scenario) per `AGENTS.md` "Test Conventions".

### Helpers

The new tests use plain `MockRunner::default().with_handler(...)`
closures and the `mock_ok` re-export from `crate::test_fixtures` (the
same path replace.rs uses at `cli/src/replace.rs:3107`). No new
shared helper is needed:

- `runner_with_active_mapper_uuid` and `runner_with_luks_uuid_probe`
  in `replace.rs` are test-module-private and only cover the
  success-shaped happy path; they do not fit any of the six branches
  here (Err returns, null backing, inactive, unparseable bodies). No
  promotion or duplication is warranted.
- A small per-test ad-hoc closure is clearer than threading branch
  selection through a parameterized helper.

A short helper inside the new `mod tests` -- `fn test_uuid(seed: u64)
-> LuksUuid` -- can mirror the convention used elsewhere in the crate
(`remove.rs:2920` area; `cmd.rs:1613`); the per-file seed range
should sit above any range already used in `probe_mapper_uuid.rs` --
suggest 700-799 (no existing probe seeds; verify with `rg
'test_uuid\(7[0-9][0-9]\)' cli/src` before committing).

## Critical files

- `cli/src/probe_mapper_uuid.rs` -- add `#[cfg(test)] mod tests` block
  at the bottom. This is the only file touched.

## Coordination with active plan `plan-the-ideal-pivot-eager-spindle.md`

That plan refactors three of the six branches (parse-status-err,
backing-null, backing-inactive) into typed enum arms over
`CryptsetupStatusOutput::{Inactive, Active { backing: BackingDevice }}`.

The behavioral assertions in tests 2, 3, 4 (above) survive that
refactor unchanged -- the helper still returns `false` for the same
inputs. Only the per-test handler bodies need light shape updates
because the underlying parsed enum changes. Tests 1, 5, 6 are
unaffected: they exercise runner-Err paths and luksUUID parsing,
neither of which the eager-spindle refactor touches.

Either plan can land first. If `eager-spindle` lands first, write the
new tests against the new enum shape. If this plan lands first,
`eager-spindle`'s implementer adjusts the three affected test bodies as
part of its consumer-update sweep.

## Verification

- `just test-rust` -- runs the new unit tests alongside the rest of
  the CLI suite.
- Filter to just the new tests during iteration:
  `cargo test -p braid-cli --lib probe_mapper_uuid`.
- No VM tests change; no fixtures change; no parser-canary impact.

## Commit shape

One commit, scoped to `cli/src/probe_mapper_uuid.rs`. Suggested
Conventional Commits message (lowercased first word per `AGENTS.md`
"Git Commits"):

```
test(probe-mapper-uuid): pin every probe-failure skip branch
```
