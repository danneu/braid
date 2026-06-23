# Plan: pin the post-commit close-skip warning wording at the helper boundary

## Context

A `/verify-issue` review of a Low / Testing finding asked to pin the
post-commit `Unverified` (UUID-mismatch) close-skip warning text on the
named `braid remove` path, mirroring how the inactive case is pinned in
`post_commit_close_inactive_warns_and_skips_close`.

Investigation showed the finding's specific proposal is misframed:

- The mismatch warning is emitted **inside the shared helper**
  `probe_observed_mapper_uuid` (`cli/src/probe_mapper_uuid.rs`), not by
  `remove`. `remove`'s mismatch arm is `MapperOwnership::Unverified => {}`
  -- it contributes zero mismatch-specific text. (`cmd_remove` in
  `cli/src/remove.rs`.)
- The inactive case is asymmetric *by design*: the helper returns
  `Inactive` silently and the **caller** emits that warning via
  `warn_close_skipped_inactive`, so pinning it at the call site is
  correct. The mismatch warning is the helper's responsibility -- the
  "pin it the way inactive is pinned" symmetry does not hold.
- The mismatch wording is in fact already pinned byte-for-byte through
  the real command path in `replace.rs` and `recover.rs`, so it cannot
  "regress unnoticed."

The real gap is broader and lives one layer down. `probe_observed_mapper_uuid`
is a user-visible boundary: its `Warning:` lines are the only signal the
operator gets that a foreign or unverifiable mapper is still open and must
be closed by hand. It has six emitting branches plus two silent branches.
The helper's own unit tests (added by commit `93424fa5`, itself a prior
`/verify-issue` pivot) deliberately pinned **routing only** -- the
`MapperOwnership` variant and `runner.requests()` shape -- and explicitly
deferred the emitted text. None of the per-branch tests assert the warning
wording, and the silent branches do not assert silence.

This is exactly the case `docs/dev/testing.md` legislates:

> Render or preview helpers that form a user-visible boundary need
> exact-output coverage for every branch, including no-op branches.

So the ideal change is to **complete** the helper's coverage: add
exact-output assertions to every branch of `probe_observed_mapper_uuid`
(and the sibling `warn_close_skipped_inactive` emitter), pinning each
operator warning at its single production source. This finishes what
`93424fa5` started and dissolves the whole class of "a close-skip warning
silently dropped or garbled" regressions, not just the mismatch one the
finding named.

## Approach

Additive change, one file. Extend the existing `#[cfg(test)] mod tests`
in `cli/src/probe_mapper_uuid.rs` so every branch of the boundary asserts
its rendered output, using the established capture seam. No production
code changes. No call-site test changes (see Non-goals).

Each emitting branch wraps the existing `probe_observed_mapper_uuid(...)`
call in `crate::status_tag::testing::capture_with_color(false, || { ... })`,
keeps its current ownership + request-count assertions, and adds an
assertion on the captured `Warning:` line. Each silent branch asserts the
captured output is empty.

The capture seam is thread-local (parallel-`cargo test` safe) and already
used to wrap small helpers in unit tests -- precedent: `close_mapper_best_effort`
(`cli/src/mapper_close.rs`), `wait_for_kernel_replace_to_finish`
(`cli/src/recover.rs`).

### Branch coverage to add

All branches are arms of `probe_observed_mapper_uuid` unless noted. The
"assert" column is the new output assertion; the existing ownership and
`runner.requests()` assertions stay.

| Branch (condition) | Existing test | Emits | New output assertion |
| --- | --- | --- | --- |
| `CryptsetupStatus` runner `Err` | `probe_returns_unverified_when_cryptsetup_status_runner_errs` | yes (`probe failed ({err})`) | framing pin (see below) |
| status parses to `Inactive` | `probe_returns_inactive_when_mapper_is_inactive` | no (silent) | captured output is empty |
| active, backing `(null)` | `probe_returns_unverified_when_backing_device_is_null` | yes (literal) | full exact line |
| status parse `Err` | `probe_returns_unverified_when_status_parse_fails` | yes (`probe failed ({err})`) | framing pin (swap input to malformed `device:`; see below) |
| `CryptsetupLuksUuid` runner `Err` | `probe_returns_unverified_when_luks_uuid_runner_errs` | yes (`probe failed ({err})`) | framing pin |
| backing UUID == expected (`Owned`) | none -- add `probe_returns_owned_when_backing_uuid_matches` | no (silent) | captured output is empty |
| backing UUID != expected (mismatch) | `probe_returns_unverified_when_uuid_value_differs` | yes (literal) | full exact line |
| luksUUID parse `Err` | `probe_returns_unverified_when_luks_uuid_parse_fails` | yes (`probe failed ({err})`) | framing pin |
| inactive emitter (caller path) | none -- add `warn_close_skipped_inactive_renders_expected_line` | yes (literal) | full exact line |

Production warning strings to pin (from `probe_mapper_uuid.rs`), `{mapper}`
is `braid-WRONG`, `{expected}`/`{observed}` are the test UUIDs:

- null backing: `Warning: post-commit close skipped for mapper braid-WRONG: probe failed (mapper backing device is unavailable (cryptsetup reports null)); expected LUKS UUID <expected>\n`
- mismatch: `Warning: post-commit close skipped for mapper braid-WRONG: expected LUKS UUID <expected> but observed <observed>\n`
- inactive emitter (`warn_close_skipped_inactive`): `Warning: post-commit close skipped for mapper braid-WRONG: probe failed (mapper is inactive); expected LUKS UUID <expected>\n`
- `{err}` branches: `Warning: post-commit close skipped for mapper braid-WRONG: probe failed (<err>); expected LUKS UUID <expected>\n`

### Pinning `{err}` branches without coupling to a downstream Display

The four `probe failed ({err})` branches embed the `Display` of a
downstream `CmdError` or parser error. Pin the **helper's own framing**
exactly and treat `{err}` as a hole -- do not re-assert the downstream
type's `Display` (that is a different unit's contract; `docs/dev/testing.md`:
"Test the layer where production failed, not a downstream ... helper").

Concretely, assert that the captured line:
- starts with `Warning: post-commit close skipped for mapper braid-WRONG: probe failed (`, and
- ends with `); expected LUKS UUID <expected>\n`, and
- contains the diagnostic substring the test injected (e.g. the
  `CmdError::Failed("...")` message), proving the error is passed through.

For the `CryptsetupStatus`/`CryptsetupLuksUuid` runner-`Err` cases, where
the test fully controls the `CmdError`, the equivalent exact form is
acceptable and cleaner: build the expected string with
`format!("...probe failed ({e})...", e = CmdError::Failed(msg.clone()))`
so the `{err}` segment is sourced from the same `Display` production uses,
never hand-built (`docs/dev/testing.md`: "production and tests should call
the same mapping helper; do not hand-build the target variant").

**Parse-`Err` inputs must echo a controllable substring.** The
"contains the injected diagnostic" check only works when the parser's
error `Display` actually carries the injected text, which not every
`ParseError` does:

- Status parse (`probe_returns_unverified_when_status_parse_fails`): the
  current input `garbage\n` parses to `ParseError::MissingField`
  (renders `missing field `device` in output of ...`), which does **not**
  echo the body -- no substring is assertable. Swap the input to a
  well-formed active status whose `device:` value is non-absolute: mirror
  the null-backing test's status shape but use `device:  dev/vda` in place
  of `(null)`. That routes through the same `parse_cryptsetup_status`
  `Err(e)` arm in the helper (branch under test unchanged) and yields
  `ParseError::InvalidValue`, whose `Display` echoes `dev/vda` verbatim
  (behavior already pinned by `parse/cryptsetup_status.rs`'s
  `cryptsetup_status_invalid_device_is_invalid_value`). Assert the framing
  plus the `dev/vda` substring; update the test's Scenario preamble (it no
  longer "emits garbage"); keep the existing `Unverified` and
  status-probe short-circuit assertions.
- luksUUID parse (`probe_returns_unverified_when_luks_uuid_parse_fails`):
  the current input `not-a-uuid\n` already parses to
  `ParseError::InvalidText` whose detail is
  `not a valid UUID: "not-a-uuid" -- ...`, so `not-a-uuid` is already a
  valid injected substring -- no input change needed.

The two fully-literal branches (null backing, mismatch) and the inactive
emitter assert the complete line byte-for-byte -- no hole.

### New tests

- `probe_returns_owned_when_backing_uuid_matches` -- status resolves
  `braid-WRONG` to `/dev/vdc`; `cryptsetup luksUUID /dev/vdc` returns
  `<expected>`. Assert `MapperOwnership::Owned`, both probes ran, and
  captured output is empty (the owned path must not warn; the caller
  closes). Use a fresh seed (existing helper seeds are 710-716; use 717).
- `warn_close_skipped_inactive_renders_expected_line` -- call
  `warn_close_skipped_inactive(&mapper, &expected)` directly inside the
  capture and assert the full inactive line. This pins the only emitter in
  the module not reachable through `probe_observed_mapper_uuid`, giving the
  module self-contained output coverage.

Each new test carries the standard three-section preamble (Intent / Why it
exists / Scenario) per `AGENTS.md`.

## Reuse (no new helpers)

- `crate::status_tag::testing::capture_with_color` (`cli/src/status_tag.rs`)
  -- the capture seam. `capture_with` (no color override) is the sibling
  used by `recover.rs`; either fits since these lines carry no color.
- `mock_ok` from `crate::test_fixtures` -- already imported in the module.
- `MockRunner::with_handler` for the `Err`-returning runner cases;
  `with_output` only injects success bodies (the module already uses both).
- The module-local `test_uuid(seed)` helper -- reuse as-is.

No shared test helper is warranted: the per-branch handler closures already
exist in each test; the change only wraps the call and adds an assertion.

## Non-goals (and why)

- **No changes to call-site tests** in `remove.rs`, `replace.rs`,
  `recover.rs`. Their full-wording assertions prove a *different* layer --
  that the warning surfaces to the operator through the real command --
  which `docs/dev/testing.md` explicitly endorses ("prefer a CLI/VM test
  that drives the real command"). The wording appearing in both the helper
  test and a command test is two layers pinning one operator contract, not
  redundant production wording (ADR 022's single-source rule governs
  production renderers; production already has one `emit_status` per
  branch). Slimming them would trade end-to-end robustness for less
  reword-churn, which is not braid's priority.
- **No `remove.rs` mismatch-wording assertion** (the finding's literal
  ask). `remove`'s mismatch arm emits nothing of its own; such an
  assertion would re-test the helper through a heavier fixture. The
  existing `probe_for_foreign_backing == 1` / `closes_for_wrong == 0`
  assertions already pin remove's routing.
- **No production changes.**

## Critical files

- `cli/src/probe_mapper_uuid.rs` -- the only file modified. Extend the
  `#[cfg(test)] mod tests` block: wrap seven existing tests with capture +
  output assertions, add two new tests.

## Verification

- `cargo test -p braid-cli --lib probe_mapper_uuid` -- iterate on the
  added/changed tests.
- `just test-rust` -- full CLI suite stays green.
- Confirm each new output assertion *fails for the right reason* before it
  passes: temporarily garble or drop the corresponding `emit_status` line
  in `probe_observed_mapper_uuid` / `warn_close_skipped_inactive`, run the
  filtered test, see it fail, revert. This is the TDD "fail first" check
  required by `AGENTS.md`.
- No VM tests, fixtures, or parser canaries change; no `flake.lock` /
  `braid.packages` change, so no fixture-refresh event.

## Commit shape

One commit scoped to `cli/src/probe_mapper_uuid.rs`. Suggested message
(lowercase first word per `AGENTS.md`):

```
test(probe-mapper-uuid): pin close-skip warning wording on every branch
```
