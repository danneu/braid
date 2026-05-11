# Plan: Migrate `cli/src/scrub_{cancel,needs_resume,resume_or_start}.rs` test scaffolding to `test_fixtures::scrub`

**Status: Draft**

## Goals

Simplify the `mod tests` blocks in

- `cli/src/scrub_cancel.rs`
- `cli/src/scrub_needs_resume.rs`
- `cli/src/scrub_resume_or_start.rs`

by moving the small set of scrub-shaped helpers each one re-defines into a
focused `cli/src/test_fixtures/scrub.rs` module. Preserve the contracts
that make those tests valuable:

### `scrub cancel`

- Dispatch is the numeric exit code, never the stderr substring.
- Exit 0 maps to `Cancelled`.
- Exit 2 maps to `NotRunning`, including the empty-stderr inverse case.
- Exit 1 with `"not running"` in stderr remains a real failure
  (`CancelFailed`).
- Other non-zero exits remain a real failure.
- A runner-layer failure (spawn error) remains `ScrubCancelError::Cmd` and
  keeps the `"btrfs scrub cancel command error: ..."` display framing.
- The runner-layer-failure test must still panic on any unexpected request:
  the strict failure contract is part of the assertion, not background noise.

### `scrub needs-resume`

- Aborted / Interrupted state -> `Yes`.
- Never / Finished / Running state -> `No`.
- Unknown state -> `Err(StatusUnknown)` (hard fail-closed against parser
  drift).
- Non-zero `btrfs scrub status` exit propagates as
  `ScrubNeedsResumeError::Parse(ParseError::CommandFailed { exit_code, .. })`.

### `scrub resume-or-start`

- Resume exit 0 -> `Resumed { uncorrectable_errors: false }`.
- Resume exit 3 -> `Resumed { uncorrectable_errors: true }`.
- Resume exit 2 is the only fallback-to-start condition.
- Resume real failures (any other non-zero) do not fall back to start.
- Start exit 0 -> `Started { uncorrectable_errors: false }`.
- Start exit 3 -> `Started { uncorrectable_errors: true }`.
- Start real failures propagate as `StartFailed`.

This is a test-side refactor only. Do not change `cmd_scrub_cancel`,
`cmd_scrub_needs_resume`, `cmd_scrub_resume_or_start`, `start_scrub`,
`parse_btrfs_scrub_status`, `ScrubState`, or any of the result/error
enums.

## Current-State Inventory

The three modules share the same shape and roughly the same per-test
scaffolding, but each has its own load-bearing contracts.

### `cli/src/scrub_cancel.rs::tests`

`scrub_cancel.rs` is 270 lines and has 6 tests plus about 75 lines of
local scaffolding.

| Helper | Lines | Role | Plan |
|---|---:|---|---|
| `mp` | 75-77 | Canonical `/mnt/storage` mount point. | Promote as `scrub_mp()`. |
| `scrub_cancel_ok` | 79-89 | `(CmdRequest, RawCommandOutput)` for cancel exit 0. | Promote as `scrub_cancel_ok()`. |
| `scrub_cancel_not_running` | 91-102 | `(CmdRequest, RawCommandOutput)` for cancel exit 2 with the canonical btrfs-progs `"not running"` stderr. | Promote as `scrub_cancel_not_running()`. The helper name documents the kernel state (ENOTCONN) rather than the stderr text; the helper still emits the stderr that real btrfs-progs renders. |
| `scrub_cancel_real_failure` | 104-114 | `(CmdRequest, RawCommandOutput)` for cancel exit 1 with a non-"not running" stderr. | Promote as `scrub_cancel_real_failure()`. The body keeps the `"Permission denied"` stderr so it remains visibly different from the not-running case. |
| `FailingCancelRunner` | 222-242 | Strict `CommandRunner` that returns `CmdError::Failed` only for `BtrfsScrubCancel` and **panics** on any other request, including `run_with_stdin`. | Keep **local** to `scrub_cancel.rs::tests`. The strict-unexpected-request behavior is part of what `cancel_command_failure_propagates` proves. |

Two tests in this module build their `RawCommandOutput` inline:

- `cancel_exit_two_with_empty_stderr_is_not_running` (179-192) keeps the
  empty-stderr exit-2 body inline because the empty stderr is the
  assertion's setup -- the point of the test is that the stderr does not
  matter.
- `cancel_not_running_stderr_with_exit_one_is_failure` (194-220) keeps the
  exit-1 + `"not running"` body inline for the same reason: the bad
  exit-code-plus-stderr pairing is the test's setup.

Migration must not collapse those inline bodies into named helpers. They
are the inverse pin to `scrub_cancel_not_running()` and
`scrub_cancel_real_failure()`; renaming them as helpers would let a
future refactor accidentally substitute the canonical body and weaken the
proof that the dispatch is exit-code-only.

### `cli/src/scrub_needs_resume.rs::tests`

`scrub_needs_resume.rs` is 255 lines and has 7 tests plus about 115 lines
of local scaffolding.

| Helper | Lines | Role | Plan |
|---|---:|---|---|
| `mp` | 48-50 | Canonical `/mnt/storage` mount point. | Reuse promoted `scrub_mp()`. |
| `scrub_status_running` | 52-70 | `(CmdRequest, RawCommandOutput)` for state `running`, fixed 10% body. | Promote as `scrub_status_running()`. |
| `scrub_status_never` | 72-84 | `(CmdRequest, RawCommandOutput)` for state `Never`, "no stats available" body. | Promote as `scrub_status_never()`. |
| `scrub_status_finished` | 86-103 | `(CmdRequest, RawCommandOutput)` for state `Finished`, zero errors. | Promote as `scrub_status_finished()`. |
| `scrub_status_aborted` | 105-122 | `(CmdRequest, RawCommandOutput)` for state `Aborted`. | Promote as `scrub_status_aborted()`. |
| `scrub_status_interrupted` | 124-141 | `(CmdRequest, RawCommandOutput)` for state `Interrupted`. | Promote as `scrub_status_interrupted()`. |
| `scrub_status_unknown` | 143-153 | `(CmdRequest, RawCommandOutput)` with exit 0, empty stdout, empty stderr -- forces `ScrubState::Unknown`. | Promote as `scrub_status_unknown()`. |

The `status_command_failure_propagates` test (230-254) keeps its
`RawCommandOutput` inline:

```rust
RawCommandOutput {
    cmd: "btrfs scrub status --raw /mnt/storage".into(),
    stdout: String::new(),
    stderr: "ERROR: not a btrfs filesystem".into(),
    exit_status: 1,
}
```

Migration should keep that body inline. The exit-1 + stderr shape is the
assertion's setup, and the `ParseError::CommandFailed { exit_code: 1, ... }`
match arm depends on the exact exit code.

### `cli/src/scrub_resume_or_start.rs::tests`

`scrub_resume_or_start.rs` is 215 lines and has 6 tests plus about 35
lines of local scaffolding.

| Helper | Lines | Role | Plan |
|---|---:|---|---|
| `mp` | 72-74 | Canonical `/mnt/storage` mount point. | Reuse promoted `scrub_mp()`. |
| `resume_output(exit_status)` | 76-90 | `(CmdRequest, RawCommandOutput)` for `BtrfsScrubResume`, parameterised by exit code; emits `"ERROR: resume failed\n"` stderr only when exit code is 1. | Promote as `scrub_resume_output(exit_status: i32)`. Preserve the exit-1-only stderr so a real-failure test continues to assert against a specific stderr if it chooses. |
| `start_output(exit_status)` | 92-106 | `(CmdRequest, RawCommandOutput)` for `BtrfsScrubStart`, parameterised by exit code; emits `"ERROR: start failed\n"` stderr only when exit code is 1. | Promote as `scrub_start_output(exit_status: i32)`. Same stderr-only-on-exit-1 behavior. |

All six tests in this module compose their `MockRunner` from at most one
`resume_output(...)` plus optionally one `start_output(...)`. There are
no inline `RawCommandOutput` bodies to preserve.

### Behavior families across the three modules

| Family | Tests | Migration concern |
|---|---|---|
| Cancel dispatch by exit code | `cancel_running_returns_cancelled`, `cancel_idle_returns_not_running`, `cancel_real_failure_propagates` | Use promoted factories. |
| Cancel dispatch ignores stderr | `cancel_exit_two_with_empty_stderr_is_not_running`, `cancel_not_running_stderr_with_exit_one_is_failure` | Keep inline `RawCommandOutput` bodies. Do not name them. |
| Cancel runner-layer failure framing | `cancel_command_failure_propagates` | Keep `FailingCancelRunner` local; the panic-on-unexpected-request behavior is load-bearing. |
| Needs-resume mapping | `aborted_needs_resume`, `interrupted_needs_resume`, `never_does_not_need_resume`, `finished_does_not_need_resume`, `running_does_not_need_resume` | Promoted `scrub_status_*()` factories. |
| Needs-resume parser drift | `unknown_is_hard_error` | Promoted `scrub_status_unknown()` factory. |
| Needs-resume command failure | `status_command_failure_propagates` | Keep the failing `RawCommandOutput` inline; the exit code is part of the matcher. |
| Resume-or-start happy paths | `resume_succeeds_returns_resumed`, `resume_uncorrectable_propagates` | Promoted `scrub_resume_output(0|3)`. |
| Resume-or-start fallback to start | `resume_nothing_to_resume_falls_back_to_start`, `start_uncorrectable_after_fallback` | Promoted `scrub_resume_output(2)` plus `scrub_start_output(0|3)`. |
| Resume-or-start real failures | `resume_real_failure_propagates`, `start_real_failure_propagates` | Promoted `scrub_resume_output(1)` and `scrub_start_output(1)`. |

## Existing Fixture Modules

Each candidate for reuse was evaluated against the constraint that the
output shape must be semantically correct for the command under test, not
merely parse-compatible.

- **`test_fixtures::status::status_btrfs_scrub_{never,finished,aborted,interrupted,finished_with_errors}`.**
  - Pros: already cover four of the five non-Running scrub states. Already
    composed with `mock_ok` and contain `Error summary` + timestamps.
  - Cons:
    1. They return only `RawCommandOutput`, not the
       `(CmdRequest, RawCommandOutput)` pair shape that every
       `scrub_needs_resume` test uses.
    2. They omit `BtrfsScrubStatus { mount_point: ... }` -- the caller
       supplies it. In status's own tests the request is paired with
       `status_mp()`, but reusing the helpers in scrub-needs-resume
       tests would force every test to wrap the response in
       `.with_output(CmdRequest::BtrfsScrubStatus { ... }, ...)`, which
       is no shorter than today.
    3. The status helpers use UUID `aaaaaaaa-...`, while the existing
       needs-resume helpers use UUID `12345678-...`. Either UUID parses
       identically, but status's body is shaped like a real
       `btrfs filesystem show` UUID + scheduled-status read; the
       needs-resume body is a synthetic per-command witness. The
       difference is small but the helpers belong to two different
       semantic scopes (status's full report vs. scrub's per-command
       triggers), so promoting them to a single shared name would
       conflate the contracts.
    4. There is no `status_btrfs_scrub_running` -- status's tests do
       not currently need a running scrub output, so reusing the
       status family would still leave one gap that needs a new factory.
    5. `status_btrfs_scrub_never` uses the
       `Scrub started:    no stats available` framing, and the existing
       needs-resume helper uses a bare `no stats available` line.
       Neither matches real btrfs-progs output. The pinned source at
       `reference/btrfs-progs/cmds/scrub.c:320-321` prints
       `pr_verbose(LOG_DEFAULT, "\tno stats available\n")` only -- no
       `Scrub started:` prefix -- and the committed golden fixture at
       `cli/tests/fixtures/nixos-25.11/btrfs-scrub-never.txt` matches:
       `UUID:` line + tab-indented `no stats available` + `Total to
       scrub:` + `Rate:` + `Error summary:` lines. Both existing
       fixtures parse to `ScrubState::Never`, but neither is the real
       shape; reusing either would mean copying an artificial body into
       a new module while claiming it is faithful.
  - Decision: **do not re-export** `status_btrfs_scrub_*`. Define a
    fresh `scrub_status_*()` family that returns
    `(CmdRequest, RawCommandOutput)` pairs paired with `scrub_mp()`.
    Use the real `--raw` never-scrubbed shape from the golden fixture
    family for `scrub_status_never()`: UUID line + tab-indented
    `no stats available` + raw summary lines.

- **`test_fixtures::idle::idle_scrub_finished` / `idle_scrub_running`.**
  - Pros: pair-shape `(CmdRequest, RawCommandOutput)`. Use
    `BtrfsScrubStatus { mount_point: idle_mp() }`.
  - Cons:
    1. They are bound to `idle_mp()`, not a scrub-scope mount point.
       Sharing one mount-point constant across `idle` and `scrub` would
       conflate two scopes whose only overlap is "default test mount
       point happens to be `/mnt/storage`."
    2. `idle_scrub_running(pct)` computes scrubbed bytes from `pct` to
       exercise the parser's percent math, which is `idle`'s contract,
       not `scrub_needs_resume`'s. Needs-resume only asks whether the
       state is Running; a fixed 10% body is enough.
    3. `idle_scrub_finished` carries idle-specific framing
       (UUID `12345678-...`, the same shape the needs-resume tests
       happen to use today). Reusing it would couple the two scopes.
  - Decision: **do not re-export** `idle_scrub_*`. Keep the scrub-scope
    helpers self-contained.

- **`shared::mock_ok`.**
  - Pros: it is the canonical success builder, used by every other
    promoted fixture.
  - Decision: use it **internally** in `test_fixtures::scrub` for the
    exit-0 status bodies. No facade re-export is required from this
    migration.

- **`shared::MockFs`** and the other filesystem mocks (`monitor_fs_*`,
  `lock_fs`, `ack_fs_*`, etc.).
  - None of the three scrub command tests touch a `Filesystem`. The
    `scrub` fixture module does not need a filesystem mock at all.

- **`doctor::isolated_paths`** and the `StatePaths` family.
  - None of the three scrub command tests read or write state files.
    Skip.

## Proposed Fixture Shape

Create `cli/src/test_fixtures/scrub.rs` as a flat scrub-scoped module.
Register it in `cli/src/test_fixtures.rs` with `mod scrub;` and facade
re-exports.

Do not create a `ScrubPool`, topology installer, params builder, or a
broad scrub runner that answers every scrub command. Each command's
tests intentionally compose only the requests they expect (`scrub
cancel` tests only seed `BtrfsScrubCancel`; `scrub needs-resume` tests
only seed `BtrfsScrubStatus`; `scrub resume-or-start` tests seed at most
`BtrfsScrubResume` plus `BtrfsScrubStart`). A multi-command runner
would silently resolve cross-command probes that the current tests
prove absent through `CmdError::MissingMock`.

### Public Fixture Surface

```rust
// Mount point
pub(crate) fn scrub_mp() -> MountPoint;

// scrub cancel: exit-code-shaped factories.
//
// Helper names document the kernel state (Cancelled, ENOTCONN, "real
// failure") rather than the stderr substring, so the numeric-exit-code
// contract stays visible at call sites and a future "scrub_cancel_with_
// not_running_stderr" name is not tempting.
pub(crate) fn scrub_cancel_ok() -> (CmdRequest, RawCommandOutput);
pub(crate) fn scrub_cancel_not_running() -> (CmdRequest, RawCommandOutput);
pub(crate) fn scrub_cancel_real_failure() -> (CmdRequest, RawCommandOutput);

// scrub status by state. Each returns (BtrfsScrubStatus { scrub_mp() },
// RawCommandOutput). Bodies are scrub-scope shapes, distinct from the
// status-scope status_btrfs_scrub_* family.
pub(crate) fn scrub_status_running() -> (CmdRequest, RawCommandOutput);
pub(crate) fn scrub_status_never() -> (CmdRequest, RawCommandOutput);
pub(crate) fn scrub_status_finished() -> (CmdRequest, RawCommandOutput);
pub(crate) fn scrub_status_aborted() -> (CmdRequest, RawCommandOutput);
pub(crate) fn scrub_status_interrupted() -> (CmdRequest, RawCommandOutput);
pub(crate) fn scrub_status_unknown() -> (CmdRequest, RawCommandOutput);

// scrub resume / start: exit-code-parameterised factories. Stderr stays
// populated only when exit_status == 1, matching the current locals so
// real-failure tests continue to assert against a specific stderr if
// they choose.
pub(crate) fn scrub_resume_output(exit_status: i32) -> (CmdRequest, RawCommandOutput);
pub(crate) fn scrub_start_output(exit_status: i32) -> (CmdRequest, RawCommandOutput);
```

Implementation notes:

- All helpers are `pub(crate)` and test-only (`#[cfg(test)]`).
- Use `super::shared::mock_ok` privately for exit-0 bodies, mirroring
  the pattern in `idle.rs` and `status.rs`.
- All `(CmdRequest, RawCommandOutput)` pairs use `scrub_mp()` so the
  request matches the call site without the call site re-supplying the
  mount point.
- `scrub_status_never()` uses the real `--raw` never-scrubbed shape
  modelled on `cli/tests/fixtures/nixos-25.11/btrfs-scrub-never.txt`:

  ```
  UUID:             12345678-1234-1234-1234-123456789abc
  \tno stats available
  Total to scrub:   33914880
  Rate:             0/s
  Error summary:    no errors found
  ```

  Source-pinned in `reference/btrfs-progs/cmds/scrub.c:320-321`: when
  `t_start == 0` (never scrubbed), btrfs-progs prints
  `pr_verbose(LOG_DEFAULT, "\tno stats available\n")` and skips the
  `Scrub started:` line. Both the existing status fixture's
  `Scrub started:    no stats available` and the existing needs-resume
  local's bare `no stats available\n` are artificial; this helper uses
  the real shape so it stays aligned with the golden fixture family on
  future btrfs-progs bumps.
- `scrub_status_unknown()` produces exit 0, empty stdout, empty stderr
  -- the only documented way the parser falls into `ScrubState::Unknown`
  -- so the helper name remains a state name, not a stderr description.
- `scrub_cancel_not_running()` keeps the canonical btrfs-progs
  `"ERROR: scrub cancel failed on /mnt/storage: not running\n"` stderr
  so the helper still matches the real exit-2 surface. The promoted
  helper does not change the contract that the dispatch ignores the
  stderr -- the stderr is in the body because btrfs-progs emits it, not
  because braid reads it.
- `scrub_cancel_real_failure()` keeps the
  `"Permission denied"` stderr so it remains visibly different from the
  not-running case.
- No helper called `scrub_cancel_*_stderr_only` or
  `scrub_cancel_stderr_says_not_running` exists. Those names would
  imply stderr matching and would invert the contract the cancel tests
  pin. The inverse cases stay as inline `RawCommandOutput` bodies in
  the test.
- `scrub_resume_output` / `scrub_start_output` take the exit code as a
  positional `i32` to preserve the cross-test ergonomic that the
  current locals already have. Do not split into per-exit-code variants
  (`scrub_resume_ok` / `scrub_resume_nothing_to_resume` /
  `scrub_resume_fail`). The exit code is the contract; a single
  parameterised helper keeps that visible.

### Facade Exports

Add a scrub block to `cli/src/test_fixtures.rs`:

```rust
mod scrub;

#[allow(unused_imports)]
pub(crate) use scrub::{
    scrub_cancel_not_running, scrub_cancel_ok, scrub_cancel_real_failure, scrub_mp,
    scrub_resume_output, scrub_start_output, scrub_status_aborted, scrub_status_finished,
    scrub_status_interrupted, scrub_status_never, scrub_status_running, scrub_status_unknown,
};
```

Update the module-level comment in `cli/src/test_fixtures.rs` with one
scrub bullet:

> `scrub` -- flat scrub-shaped helpers for `cmd_scrub_cancel`,
> `cmd_scrub_needs_resume`, and `cmd_scrub_resume_or_start`. Ships
> exit-code-shaped factories for cancel and resume/start, plus
> per-state scrub-status factories. Names document kernel state, not
> stderr text, so the numeric-exit-code dispatch contract for
> `scrub cancel` stays visible. No broad scrub runner: cross-command
> probes still surface as `MissingMock`.

### Why `scrub_` is the prefix

Every newly-exported helper carries a `scrub_` prefix.

1. Facade collisions across fixture modules. `status` already exports
   `status_btrfs_scrub_never`, `status_btrfs_scrub_finished`,
   `status_btrfs_scrub_aborted`, `status_btrfs_scrub_interrupted`, and
   `status_btrfs_scrub_finished_with_errors`. The scrub-scope helpers
   intentionally have shorter names (no `status_btrfs_` prefix) because
   they belong to per-command tests, not to status assembly. The
   `scrub_` prefix keeps the two families distinct and self-documenting
   at the call site.
2. Identifier continuity at call sites. The local helpers
   `scrub_status_*`, `scrub_cancel_*`, and `resume_output` /
   `start_output` cover most of the helper surface; promoting them with
   the `scrub_` prefix keeps the cancel and status call sites
   character-for-character identical (`scrub_status_running()`,
   `scrub_cancel_ok()`, ...) and only changes the resume/start helpers
   from `resume_output(0)` to `scrub_resume_output(0)`. The local
   `mp()` becomes `scrub_mp()`.

The `scrub_` prefix is **not** a staged-migration-safety device. Most
local helpers already share the prefix (`scrub_cancel_ok`,
`scrub_status_running`, ...) and therefore already collide with the
promoted name. The migration handles that head-on: every sub-commit
that imports a promoted facade name also deletes the same-named local
in the same commit (see Staged Migration below).

The local `mp()` function in each of the three test modules has a
different identifier from the promoted `scrub_mp()` and does not
collide; each migration sub-commit replaces all `mp()` call sites in
that module with `scrub_mp()` and deletes the local `mp` so the module
ends with a single mount-point identifier.

### What Stays Local

- `FailingCancelRunner` stays in `scrub_cancel.rs::tests`. It is used by
  exactly one test, and its panic-on-any-other-request behavior is part
  of the assertion: a future regression that adds a probe before
  `BtrfsScrubCancel` should fail this test loudly. Promoting it to the
  fixture module would either weaken that contract (if a sibling test
  ever needs a less strict variant) or duplicate it (if the fixture
  copy diverges).
- The two inline `RawCommandOutput` bodies in `scrub_cancel.rs::tests`
  (`cancel_exit_two_with_empty_stderr_is_not_running` and
  `cancel_not_running_stderr_with_exit_one_is_failure`) stay inline.
  They are the inverse pins to the canonical
  `scrub_cancel_not_running()` and `scrub_cancel_real_failure()`
  bodies; their stderr shapes are the assertion's setup, not reusable
  fixtures.
- The inline `RawCommandOutput` in
  `status_command_failure_propagates` in
  `scrub_needs_resume.rs::tests` stays inline. The exit-1 + stderr
  pairing is the matcher's assertion setup.
- The `mod tests` doc-style intent / why / scenario comments in each
  test stay local. The fixture module is for reusable bodies, not for
  rewriting test prose.

### What Does Not Go in `shared`

No new `shared` helper is required for this migration.

- The scrub fixtures are tied to scrub `CmdRequest` variants
  (`BtrfsScrubCancel`, `BtrfsScrubStatus`, `BtrfsScrubResume`,
  `BtrfsScrubStart`).
- The cancel-vs-status-vs-resume separation is scrub-scope, not a
  cross-command primitive.
- `mock_ok` already covers the only cross-command primitive
  (exit-0 builder).

## Staged Migration

Each sub-commit must compile and keep

- `cargo test --manifest-path cli/Cargo.toml --lib scrub_cancel::tests`
- `cargo test --manifest-path cli/Cargo.toml --lib scrub_needs_resume::tests`
- `cargo test --manifest-path cli/Cargo.toml --lib scrub_resume_or_start::tests`

and `just test-rust` green.

The local helpers `scrub_cancel_*` and `scrub_status_*` share their
identifiers with the promoted facade names. A sub-commit that adds
`use crate::test_fixtures::scrub_cancel_ok;` while the same-named
local `fn scrub_cancel_ok` is still defined in the module would fail
to compile (`E0252` / `E0255`). Each migration sub-commit therefore
deletes the same-named local helpers in the same commit. The local
`mp()` does not collide with the promoted `scrub_mp()`, but every
migration sub-commit renames all `mp()` call sites in the touched
module and deletes the local `mp` in the same commit so the module
ends with a single mount-point identifier.

| # | Commit subject | Scope | Focused verification |
|---:|---|---|---|
| 1 | `test(scrub): add scrub fixture module` | Add `cli/src/test_fixtures/scrub.rs`, register facade exports, update the `test_fixtures.rs` module doc comment with the new scrub bullet. No `scrub_*.rs` call sites change yet; no locals are deleted yet. | `cargo check --manifest-path cli/Cargo.toml --tests`; `cargo test --manifest-path cli/Cargo.toml --lib scrub_cancel::tests`; `cargo test --manifest-path cli/Cargo.toml --lib scrub_needs_resume::tests`; `cargo test --manifest-path cli/Cargo.toml --lib scrub_resume_or_start::tests`; `just test-rust` |
| 2 | `test(scrub_cancel): migrate scrub_cancel tests to scrub fixtures` | In `scrub_cancel.rs::tests`: import `scrub_cancel_not_running`, `scrub_cancel_ok`, `scrub_cancel_real_failure`, `scrub_mp` from the facade. Migrate `cancel_running_returns_cancelled`, `cancel_idle_returns_not_running`, and `cancel_real_failure_propagates` to the promoted factories. Rename every remaining `mp()` call site in the module (the two inverse-pin tests `cancel_exit_two_with_empty_stderr_is_not_running` and `cancel_not_running_stderr_with_exit_one_is_failure`, and `cancel_command_failure_propagates`) to `scrub_mp()`. Delete local `mp`, `scrub_cancel_ok`, `scrub_cancel_not_running`, and `scrub_cancel_real_failure` in the same commit. Keep `FailingCancelRunner` local (panic-on-any-other-request behavior is load-bearing for `cancel_command_failure_propagates`). Keep the two inline `RawCommandOutput` bodies (inverse pins to the canonical not-running and real-failure helpers). | `cargo check --manifest-path cli/Cargo.toml --tests`; run all six `scrub_cancel::tests::*` by name; `cargo test --manifest-path cli/Cargo.toml --lib scrub_cancel::tests`; `just test-rust` |
| 3 | `test(scrub_needs_resume): migrate status-by-state tests to scrub fixtures` | In `scrub_needs_resume.rs::tests`: import the six `scrub_status_*` helpers and `scrub_mp` from the facade. Migrate `aborted_needs_resume`, `interrupted_needs_resume`, `never_does_not_need_resume`, `finished_does_not_need_resume`, `running_does_not_need_resume`, and `unknown_is_hard_error` to the promoted factories. Rename the remaining `mp()` call site in `status_command_failure_propagates` to `scrub_mp()` (the test keeps its inline `RawCommandOutput` body; only the mount-point identifier changes). Delete local `mp`, `scrub_status_running`, `scrub_status_never`, `scrub_status_finished`, `scrub_status_aborted`, `scrub_status_interrupted`, and `scrub_status_unknown` in the same commit. | `cargo check --manifest-path cli/Cargo.toml --tests`; run all seven `scrub_needs_resume::tests::*` by name; `cargo test --manifest-path cli/Cargo.toml --lib scrub_needs_resume::tests`; `just test-rust` |
| 4 | `test(scrub_resume_or_start): migrate resume/start tests to scrub fixtures` | In `scrub_resume_or_start.rs::tests`: import `scrub_resume_output`, `scrub_start_output`, and `scrub_mp` from the facade. Migrate every test to the promoted factories. Rename remaining `mp()` call sites to `scrub_mp()`. Delete local `mp`, `resume_output`, and `start_output` in the same commit. (The local names `resume_output` / `start_output` do not collide with the promoted `scrub_resume_output` / `scrub_start_output`, but every call site is migrated in this commit so the locals become unused and are dropped together.) | `cargo check --manifest-path cli/Cargo.toml --tests`; run all six `scrub_resume_or_start::tests::*` by name; `cargo test --manifest-path cli/Cargo.toml --lib scrub_resume_or_start::tests`; `just test-rust` |

There is no separate final-cleanup commit. Every migration sub-commit
deletes the locals it obsoletes in-place, so after sub-commit 4 the
three test modules retain only the helpers and inline bodies that must
stay local (`FailingCancelRunner`, the two cancel inverse-pin inline
bodies, and the `status_command_failure_propagates` inline body).
Run `cargo check --manifest-path cli/Cargo.toml --tests` as part of
each migration sub-commit's verification to catch any dead `use`
imports or stranded helpers.

## Risks

- **Hiding the numeric-exit-code contract behind a stderr-shaped name.**
  A helper called `scrub_cancel_not_running_stderr()` would imply that
  the dispatch is the stderr substring; the contract is the opposite.
  Mitigation: helper names document kernel state (`Cancelled`,
  `NotRunning`, `RealFailure`), not stderr text. The two
  inverse-stderr tests keep their inline bodies so the inverse pin
  cannot be replaced by the canonical body.
- **Weakening the cancel runner-layer failure contract.** Promoting
  `FailingCancelRunner` to the fixture module risks a future test
  weakening the panic to "return MissingMock" or "accept arbitrary
  requests," which would mask a regression that adds an unintended probe
  ahead of `BtrfsScrubCancel`. Mitigation: keep `FailingCancelRunner`
  local. Its only caller is one test.
- **Cross-command probe leakage.** A broad scrub runner that seeds
  `BtrfsScrubStatus`, `BtrfsScrubResume`, and `BtrfsScrubCancel`
  together would resolve probes the per-command tests prove absent
  through `MissingMock`. Mitigation: do not ship a runner helper. Each
  test still composes a `MockRunner::default().with_output(...)` chain.
- **Semantic shape drift from reusing `status_btrfs_scrub_*` or
  `idle_scrub_*`.** Status's scrub helpers belong to status's full
  report and idle's scrub helpers bake in idle's percent math. Reusing
  either family would couple scrub's per-command tests to an unrelated
  scope and reduce clarity at the call site. Mitigation: define a
  scrub-scope `scrub_status_*` family with bodies modelled on
  btrfs-progs's real output and parsed identically to the existing
  local bodies.
- **`scrub_status_never` body drift.** The current needs-resume local
  uses a bare `no stats available\n` line; the status fixture uses
  `Scrub started:    no stats available\n`. Neither matches real
  btrfs-progs output. The promoted helper switches to the real
  `--raw` never-scrubbed shape modelled on
  `cli/tests/fixtures/nixos-25.11/btrfs-scrub-never.txt`: UUID line +
  tab-indented `no stats available` + `Total to scrub:` + `Rate:` +
  `Error summary:` lines, matching
  `reference/btrfs-progs/cmds/scrub.c:320-321`. All three shapes parse
  to `ScrubState::Never`, so the change is invisible to assertions,
  but the framing should be the real one. Mitigation: call this out
  in the helper doc comment and in the sub-commit 3 commit message
  so a reviewer doesn't think the body was changed by accident.
- **Overprescribing the test structure.** The implementation may choose
  to leave the per-state needs-resume tests as separate tests or
  table-drive them. The plan requires preserving behavior and strict
  fixtures, not a specific assertion layout.
- **Facade churn.** Adding 12 new re-exports is a noticeable surface
  change. Mitigation: gate every new export with
  `#[allow(unused_imports)]`, batch the additions in one block in the
  module doc comment, and group the names in the `pub(crate) use`
  block by sub-family (`scrub_cancel_*` / `scrub_status_*` /
  `scrub_resume_output` / `scrub_start_output` / `scrub_mp`).

## Verification

Use filtered Rust tests during each sub-commit:

```sh
cargo test --manifest-path cli/Cargo.toml --lib scrub_cancel::tests::<test_name>
cargo test --manifest-path cli/Cargo.toml --lib scrub_cancel::tests
cargo test --manifest-path cli/Cargo.toml --lib scrub_needs_resume::tests::<test_name>
cargo test --manifest-path cli/Cargo.toml --lib scrub_needs_resume::tests
cargo test --manifest-path cli/Cargo.toml --lib scrub_resume_or_start::tests::<test_name>
cargo test --manifest-path cli/Cargo.toml --lib scrub_resume_or_start::tests
```

Run the full Rust gate at every sub-commit boundary:

```sh
just test-rust
```

Run `cargo check --manifest-path cli/Cargo.toml --tests` as part of
every sub-commit boundary -- not only after adding the fixture module
(sub-commit 1) but also after each migration sub-commit (2, 3, 4),
because each migration sub-commit deletes locals in-place and must
leave the module free of unused imports, dead references, and facade
wiring errors.

No VM fixture capture is required. This migration does not change
parser fixtures, nixpkgs inputs, or production parser behavior.
