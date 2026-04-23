---
name: Split execute_open_plan into two entry points
status: Draft
---

## Context

`mount::execute_open_plan` (cli/src/mount.rs:461) takes `credential: Option<&OpenCredential>` and strict-validates at runtime that `credential.is_some() == !plan.to_unlock.is_empty()`. The `(false, false)` and `(true, true)` arms (mount.rs:473-486) return `MountError::Failed("internal: ...")` because the signature cannot express the "credential required iff to_unlock is non-empty" precondition.

Every production caller already gates on `plan.to_unlock.is_empty()` before calling:

- `cmd_unlock` (unlock.rs:73-82) resolves a credential only when needed.
- `cmd_recover` initial mount (recover.rs:267-271) passes `None` or `Some(credential.as_ref())` based on the same check.
- `cmd_recover` cycle remount (recover.rs:684) always passes `Some(credential)` because the cycle just closed every mapper and guarantees `to_unlock` is non-empty.

The runtime checks are defensive asserts against a caller bug, not load-bearing decisions. Splitting the function into two entry points lets callers express their intent in the signature and deletes the defensive arms entirely.

Finding source: verification of "strict cross-caller validation (mount.rs:463-479)" earlier this session.

## Target shape

Two public entry points in `cli/src/mount.rs`:

```rust
/// Execute an `OpenPlan` whose `to_unlock` is empty (all mappers already open).
/// Phases: reject non-empty `to_unlock` -> btrfs device scan -> mkdir + mount.
/// Non-empty `to_unlock` is a caller-contract violation and returns
/// `MountError::Failed("internal: execute_mount_only called with non-empty
/// plan.to_unlock")` in all builds. Callers that might hold a plan with
/// locked disks must dispatch to `execute_unlock_and_mount` instead.
pub fn execute_mount_only<R, F>(runner: &R, fs: &F, config: &Config, plan: &OpenPlan)
    -> Result<bool, MountError>;

/// Execute an `OpenPlan` that has disks to unlock.
/// Phases: reject empty `to_unlock` -> verify credential -> open LUKS ->
/// btrfs device scan -> mkdir + mount.
/// Empty `to_unlock` is a caller-contract violation and returns
/// `MountError::Failed("internal: execute_unlock_and_mount called with empty
/// plan.to_unlock")` in all builds. Callers that might hold an empty plan
/// must dispatch to `execute_mount_only` instead.
pub fn execute_unlock_and_mount<R, F>(
    runner: &R, fs: &F, config: &Config,
    plan: &OpenPlan, credential: &OpenCredential,
) -> Result<bool, MountError>;
```

Shared tail factored into a private helper (scan + mkdir + mount, roughly 37 lines of the current body):

```rust
fn scan_and_mount<R, F>(runner: &R, fs: &F, config: &Config, plan: &OpenPlan)
    -> Result<bool, MountError>;
```

`execute_open_plan` is deleted.

## Why this shape, not a type-encoded non-empty plan

A `struct OpenPlanWithUnlock(OpenPlan)` with a constructor that rejects empty `to_unlock` would also make the invariant type-enforced, but it pushes a new type through the call graph (`plan_open_pool` return, all callsite match arms, test helper) for one validation site. Keeping a hard runtime check inside `execute_unlock_and_mount` costs one `if` and one error string while the signature still does the primary work of eliminating the `(false, false)` case. If we later find a second place that needs the same guarantee we can revisit.

**Both runtime checks stay hard (not `debug_assert!`) and are symmetric.** The old `execute_open_plan` validated in both directions: `(false, false)` and `(true, true)`. After the split, the type system kills `(false, false)` (no credential argument on `execute_mount_only` means "credential missing" is unrepresentable), but nothing in the signature prevents a caller from routing a plan with locked disks into `execute_mount_only` or an empty plan into `execute_unlock_and_mount`. Both entry points therefore keep a hard runtime check on `plan.to_unlock`, and both return an `Err(...)` in all builds. A `debug_assert!` would let a caller-wiring regression silently succeed in release builds, which weakens the invariant the split is supposed to encode.

## Changes

### 1. cli/src/mount.rs

- Introduce private `scan_and_mount(runner, fs, config, plan)` containing lines currently at mount.rs:591-627 (btrfs device scan, create_dir_all, degraded-aware mount, success print).
- Introduce `execute_mount_only(runner, fs, config, plan)` whose first statement is a hard runtime check that returns `MountError::Failed("internal: execute_mount_only called with non-empty plan.to_unlock")` if `!plan.to_unlock.is_empty()`, then calls `scan_and_mount`. Its rustdoc names the precondition (`plan.to_unlock` is empty) and states it is enforced by the runtime check (since the type system can't express it directly on `&OpenPlan`).
- Introduce `execute_unlock_and_mount(runner, fs, config, plan, credential)` containing the current lines 489-589 (credential verification + LUKS opens) followed by `scan_and_mount(...)`. First statement is a hard runtime check that returns `MountError::Failed("internal: execute_unlock_and_mount called with empty plan.to_unlock")` if `plan.to_unlock.is_empty()` -- this fires in all builds.
- Delete `execute_open_plan` and its `"internal: credential required ..."` / `"internal: credential provided ..."` strings. The new empty-plan check replaces the latter and there is no longer any way to hit the former (the type system makes it unrepresentable).
- Update rustdoc on `OpenCredential` (mount.rs:31), `plan_open_pool` (mount.rs:148), and `open_disks_with_passphrase` (mount.rs:379) to reference the two new entry points.

### 2. cli/src/unlock.rs:73-84

Replace the `credential` let + single call with a branch:

```rust
let mounted = if plan.to_unlock.is_empty() {
    mount::execute_mount_only(runner, fs, params.config, &plan)?
} else {
    let source = mount::CredentialSource { ... };
    let credential = mount::resolve_credential(&source)?;
    mount::execute_unlock_and_mount(runner, fs, params.config, &plan, &credential)?
};
```

The UX rule "no prompt when every mapper is already open" is preserved because `resolve_credential` only runs in the else arm. No other unlock.rs changes.

### 3. cli/src/recover.rs:264-272 (initial mount)

The eager credential resolution at lines 245-258 stays as-is (the cycle below always needs it). Replace the `cred_for_initial` match arm with:

```rust
Some(p) => {
    let res = if p.to_unlock.is_empty() {
        mount::execute_mount_only(runner, fs, params.config, p)
    } else {
        mount::execute_unlock_and_mount(runner, fs, params.config, p, &credential
            .as_ref()
            .expect("credential resolved above when plan is Some"))
    };
    match res { Ok(b) => b, Err(e) => { ... existing error handling ... } }
}
```

The `expect` is load-bearing: the outer match at lines 245-258 guarantees `credential` is `Some` whenever `plan` is `Some`. Leave a one-line comment pointing to that guarantee.

### 4. cli/src/recover.rs:684 (cycle remount)

Swap directly to `execute_unlock_and_mount`:

```rust
mount::execute_unlock_and_mount(runner, fs, config, &cycle_plan, &credential)
    .map_err(|e| RecoverError::Failed(format!("recover remount cycle: re-mount: {e}")))?;
```

The existing comment at recover.rs:674-676 ("the cycle's plan ALWAYS has `to_unlock` non-empty") already justifies the choice of entry point -- keep it.

### 5. cli/src/mount.rs test helper

`open_and_mount_for_test` at mount.rs:661-681 stays as a single helper for test ergonomics. Update its body to dispatch:

```rust
if plan.to_unlock.is_empty() {
    execute_mount_only(runner, fs, config, &plan)
} else {
    let credential = credential.as_ref()
        .expect("test passed empty credential with non-empty plan");
    execute_unlock_and_mount(runner, fs, config, &plan, credential)
}
```

Keep the existing rustdoc accurate post-split.

### 6. Out of scope

- `feature-findings/*` and `plans/impl/2026-04-07-*` reference `execute_open_plan` historically. Those are frozen records; no updates.
- No `MountError` variant changes. The deleted arms used `MountError::Failed(String)`; no variant becomes unused.
- No changes to `plan_open_pool`, `resolve_credential`, `CredentialSource`, `OpenPlan`, or `OpenCredential`.

## Files modified

- `cli/src/mount.rs` -- split function, factor shared tail, update rustdoc, update `open_and_mount_for_test`.
- `cli/src/unlock.rs` -- branch at the credential-resolution site.
- `cli/src/recover.rs` -- branch at the initial-mount callsite; swap the cycle remount callsite.

## New boundary tests

The existing mount-layer tests go through `open_and_mount_for_test`, which will also dispatch on `plan.to_unlock.is_empty()` post-refactor. That means a bad caller wiring could still mount successfully through the helper and pass the suite. Four focused tests pin the new contract directly:

1. **`execute_unlock_and_mount_rejects_empty_plan`** (cli/src/mount.rs test module).
   Build an `OpenPlan` with empty `to_unlock` (e.g. via `plan_open_pool` on a fixture where all mappers are already open), call `execute_unlock_and_mount` directly with an arbitrary credential, and assert the result is `Err(MountError::Failed(msg))` where `msg` contains `"execute_unlock_and_mount called with empty plan.to_unlock"`. This test must call the production function directly -- NOT through `open_and_mount_for_test` -- otherwise the helper's dispatch masks the check.

2. **`execute_mount_only_rejects_non_empty_plan`** (cli/src/mount.rs test module).
   Symmetric to test 1. Build an `OpenPlan` with non-empty `to_unlock` (fixture with at least one locked disk), call `execute_mount_only` directly, and assert the result is `Err(MountError::Failed(msg))` where `msg` contains `"execute_mount_only called with non-empty plan.to_unlock"`. Again, call the production function directly -- NOT through `open_and_mount_for_test`.

3. **`cmd_unlock_skips_credential_resolution_when_nothing_to_unlock`** (cli/src/unlock.rs test module, or mount.rs if easier to fixture).
   Fixture: all mappers already open. Use a `CredentialSource` whose `passphrase_stdin` / `passphrase_file` / `key_file` are all `None` or point at nonexistent paths. Drive `cmd_unlock` end-to-end. Assert it succeeds without attempting to resolve a credential -- the test passes because `resolve_credential` is never reached, so absent credential sources cause no error. Guards against a future refactor where someone hoists `resolve_credential` above the `to_unlock.is_empty()` branch.

4. **`cmd_recover_resolves_credential_for_cycle_even_if_initial_mount_needs_none`** (cli/src/recover.rs test module).
   Fixture: all mappers open for the initial mount (so `p.to_unlock.is_empty()` at the initial call), but the remount cycle will close them and need to reopen. Assert the recovery succeeds end-to-end. This pins the recover.rs:245-258 eager-resolution rule -- if a future edit moves credential resolution into the initial-mount branch, the cycle loses its credential and this test fails. There is already one adjacent test at recover.rs:1458+ (`recover_with_all_mappers_open_still_resolves_credential_for_cycle`); verify that test covers exactly this case before adding a new one, and extend/rename it if so rather than duplicating.

Tests 1, 2, and 4 directly pin the invariants most at risk from this refactor. Test 3 is a lightweight add-on that captures the UX rule the split preserves.

## Verification

1. `cargo check -p braid` -- signatures wire up.
2. `cargo clippy -p braid --all-targets` -- no new warnings.
3. `just test-rust` -- all existing mount/unlock/recover unit tests plus the four new boundary tests from the previous section (`execute_unlock_and_mount_rejects_empty_plan`, `execute_mount_only_rejects_non_empty_plan`, `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock`, `cmd_recover_resolves_credential_for_cycle_even_if_initial_mount_needs_none`). If test 4 is folded into the existing `recover_with_all_mappers_open_still_resolves_credential_for_cycle` at recover.rs:1458+ rather than added anew, treat that extension as satisfying test 4 and keep the count at four.
4. `just test-vm unlock` and any `recover` VM tests touching mount flow -- end-to-end confirmation that both entry points work in their intended call paths.
5. Grep `rg 'execute_open_plan' cli/src/` returns zero hits post-refactor.
6. Grep `rg '"internal: credential required' cli/src/` returns zero hits post-refactor. The two replacement strings are `"execute_mount_only called with non-empty plan.to_unlock"` and `"execute_unlock_and_mount called with empty plan.to_unlock"`.

Skip the fixture-capture obligations (no parser-critical tool version change).
