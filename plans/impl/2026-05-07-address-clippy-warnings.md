# megaplan: address all clippy warnings in one commit

## Context

`cargo clippy --all-targets` currently emits **20 warnings across 11 distinct
lints** in `cli/`. None are blockers, but the noise hides future warnings and
some (e.g. the param-struct refactors) genuinely improve readability. The user
wants a single commit that brings clippy to zero warnings, with each fix
isolated as its own TODO so progress is trackable.

Two of the 11 lints (`result_large_err`, `large_enum_variant`) are not worth
the boxing churn in a CLI that holds at most one of these values at a time;
those are suppressed per-item via `#[allow(...)]` rather than restructured.

## Approach

- One commit at the end of the pass with all fixes squashed.
- Run `just test-rust` after the param-struct refactor (highest blast
  radius) and again at the very end.
- Final verification: `cargo clippy --all-targets` shows zero warnings.

## TODO checklist

Order is roughly low-risk → high-risk so the working tree stays green.

### Mechanical one-liners (no semantic change)

- [ ] **#4 -- `redundant_closure` at `cli/src/replace.rs:375`**
      Replace `|line| emit_replace_stderr(line)` with `emit_replace_stderr`.

- [ ] **#6a -- `manual_repeat_n` at `cli/src/confirm.rs:299`**
      Replace `std::iter::repeat(b' ').take(CONFIRM_MAX_BYTES + 1)` with
      `std::iter::repeat_n(b' ', CONFIRM_MAX_BYTES + 1)`.

- [ ] **#6b -- `manual_repeat_n` at `cli/src/confirm.rs:314`**
      Replace `std::iter::repeat(b' ').take(CONFIRM_MAX_BYTES)` with
      `std::iter::repeat_n(b' ', CONFIRM_MAX_BYTES)`.

- [ ] **#7 -- `io_other_error` at `cli/src/luks.rs:1314`**
      Replace `std::io::Error::new(std::io::ErrorKind::Other, "forced early return")`
      with `std::io::Error::other("forced early return")`.

- [ ] **#9a -- `useless_asref` at `cli/src/monitor.rs:400`**
      Replace `self.mountinfo.as_ref().map(|body| body.clone())` with
      `self.mountinfo.clone()`. Field is `Result<String, std::io::ErrorKind>`;
      `ErrorKind: Copy`, so `Result::clone` is equivalent.

- [ ] **#9b -- `useless_asref` at `cli/src/mount_check.rs:562`**
      Same fix as #9a; `MockMountInfoFs::mountinfo` is the same type.

- [ ] **#10a -- `manual_contains` at `cli/src/pool.rs:829`**
      Replace `sleeper.calls().iter().any(|d| *d == progress::HEARTBEAT_INTERVAL)`
      with `sleeper.calls().contains(&progress::HEARTBEAT_INTERVAL)`.

- [ ] **#10b -- `manual_contains` at `cli/src/progress.rs:866`**
      Replace `calls.iter().any(|d| *d == HEARTBEAT_INTERVAL)` with
      `calls.contains(&HEARTBEAT_INTERVAL)`.

- [ ] **#11 -- `missing_const_for_thread_local` at `cli/src/replace.rs:736`**
      Wrap initializer in `const { ... }`:
      `static CAPTURED_STDERR: RefCell<Option<String>> = const { RefCell::new(None) };`

### Test-only collapsible-if rewrites

- [ ] **#8a -- `collapsible_if` at `cli/src/monitor.rs:271`**
      Collapse `if matches!(*guard, Some(Override::BtrfsShowPayload(_)))`
      with the inner `if let` into a let-chain (clippy's suggested form).

- [ ] **#8b -- `collapsible_if` at `cli/src/monitor.rs:281`**
      Same collapse for the `BtrfsShowResult` variant.

- [ ] **#8c -- `collapsible_if` at `cli/src/monitor.rs:291`**
      Same collapse for the `StatsResult` variant.

### Test module reordering

- [ ] **#5a -- `items_after_test_module` at `cli/src/main.rs:855`**
      Move the `#[cfg(test)] mod tests { ... }` block to the bottom of the
      file, after `disk_name_candidates()` (currently at line 947).
      Pure reordering; no code changes inside the module.

- [ ] **#5b -- `items_after_test_module` at `cli/src/tui/event.rs:48`**
      Move the `#[cfg(test)] mod tests { ... }` block to the bottom of the
      file, after `InputHandler` and its `Drop` impl (lines 91-136).

### Per-item allow suppressions (size lints not worth boxing)

- [ ] **#1 -- `result_large_err` at `cli/src/credential_verify.rs:36`**
      Add `#[allow(clippy::result_large_err)]` directly above
      `pub fn verify_credential_for_targets`. Add a one-line `//` comment
      noting the rationale: CLI-level call site, not a hot path.

- [ ] **#2 -- `large_enum_variant` at `cli/src/journal.rs:94`**
      Add `#[allow(clippy::large_enum_variant)]` directly above
      `pub enum OpKind`. Add a one-line `//` comment noting the rationale:
      one in-flight `OpKind` per CLI invocation; boxing
      `ReplaceJournalTarget` would litter the codebase for ~200 bytes.

### Param-struct refactors (highest blast radius)

For each of the four sub-tasks below:
- Define the new struct as a **private** `struct` (no visibility modifier)
  inside the same module as the helper fn.
- Per AGENTS.md the rule applies to "top-level function, type, module,
  trait, OR pub/pub(crate) item" -- top-level types need a one-line `///`
  doc comment regardless of visibility. Capture intent, not the signature.
- Update every call site identified in exploration. Update the helper
  fn signature to take the new struct (by value, ref, or `&mut` per
  ownership notes).

- [ ] **#3a -- `too_many_arguments` (9) at `cli/src/recover.rs:2030`**
      Function: `execute_add_pool_mutation_recovery`.
      New struct (private to `recover`, contains the per-replay state):
      ```rust
      struct AddPoolReplayCtx<'a> {
          credential: Option<&'a OpenCredential>,
          journal: &'a Journal,
          union: &'a PoolMembership,
          targets: &'a std::collections::BTreeMap<String, journal::AddJournalTarget>,
          pool: PoolState, // moved in; helper currently takes `mut pool`
      }
      ```
      New signature: `fn execute_add_pool_mutation_recovery<R, F>(runner, fs,
      by_id_resolver, params, ctx: AddPoolReplayCtx<'_>) -> Result<...>`.
      Call sites to update: **1 production + 13 tests = 14 total**:
      - Production: `recover.rs:552`
      - Tests: `recover.rs:5341, 5413, 5487, 5730, 5909, 6020, 6091, 6219,
        6481, 6594, 6799, 6892, 7043`

- [ ] **#3b -- `too_many_arguments` (8) at `cli/src/recover.rs:2309`**
      Function: `execute_remove_missing_post_maintenance_recovery`.
      New struct:
      ```rust
      struct RemoveMissingPostCtx<'a> {
          journal: &'a Journal,
          pool: PoolState,
          devid: u64,
          restore_raid1_after_commit: bool,
          inhibitor_already_held: bool,
      }
      ```
      New signature: `fn execute_remove_missing_post_maintenance_recovery<R>(
      runner, by_id_resolver, params, ctx: RemoveMissingPostCtx<'_>) -> ...`.
      Call sites to update: 2 production (`recover.rs:588, 2297`) + 2 tests
      (`recover.rs:7254, 7294`).

- [ ] **#3c -- `too_many_arguments` (8) at `cli/src/recover.rs:2428`**
      Function: `finish_uncommitted_replace_recovery`.
      New struct:
      ```rust
      struct ReplaceFinishCtx<'a> {
          credential: Option<&'a OpenCredential>,
          journal: &'a Journal,
          pool: &'a PoolState,
          new_name: &'a str,
          new_target: &'a journal::ReplaceJournalTarget,
      }
      ```
      New signature: `fn finish_uncommitted_replace_recovery<R, F>(runner, fs,
      params, ctx: ReplaceFinishCtx<'_>) -> ...`.
      Call sites to update: 1 production (`recover.rs:2571`).

- [ ] **#3d -- `too_many_arguments` (8) at `cli/src/lock.rs:98`**
      Function: `close_one_mapper`.
      Split args by lifetime: loop-invariant inputs go in the struct;
      per-iteration values stay as method args.
      New struct (private to `lock`):
      ```rust
      struct CloseMapperCtx<'a, R, S>
      where
          R: CommandRunner,
          S: Sleeper,
      {
          runner: &'a R,
          sleeper: &'a S,
          color_enabled: bool,
          umount_error: &'a Option<LockError>,
          first_mapper_error: &'a mut Option<LockError>,
      }
      ```
      Convert `close_one_mapper` to a method on `CloseMapperCtx`:
      `fn close_one(&mut self, mapper: &str, disk_label: &str, is_orphan: bool)`.
      The `&mut self` covers the existing `&mut Option<LockError>` borrow;
      the two close loops in `LockPlan::execute` build one `CloseMapperCtx`
      per loop and call `.close_one(...)` per mapper.
      Call sites to update: the membership and orphan close loops in
      `LockPlan::execute` (the only callers of `close_one_mapper`).

## Critical files

- `cli/src/credential_verify.rs` (#1)
- `cli/src/journal.rs` (#2)
- `cli/src/recover.rs` (#3a, #3b, #3c -- by far the largest diff)
- `cli/src/lock.rs` (#3d)
- `cli/src/replace.rs` (#4, #11)
- `cli/src/main.rs` (#5a)
- `cli/src/tui/event.rs` (#5b)
- `cli/src/confirm.rs` (#6a, #6b)
- `cli/src/luks.rs` (#7)
- `cli/src/monitor.rs` (#8a, #8b, #8c, #9a)
- `cli/src/mount_check.rs` (#9b)
- `cli/src/pool.rs` (#10a)
- `cli/src/progress.rs` (#10b)

## Reused patterns / existing conventions

- `RecoverParams<'a>` (`cli/src/recover.rs:163`) is the existing
  cross-cutting param struct in this module; the three new recover
  `Ctx` structs follow the same `'a`-lifetime, borrow-heavy shape. The
  fourth (`CloseMapperCtx` in `lock.rs`) is generic over `R: CommandRunner,
  S: Sleeper` because `close_one_mapper` is generic over those.
- AGENTS.md doc-comment rule covers "top-level function, type, module,
  trait, OR pub/pub(crate) item" -- the four new private context structs
  are top-level types so each gets a one-line `///` doc capturing intent.
- Conventional Commits + lowercase first letter convention (AGENTS.md):
  commit message subject `chore(clippy): silence all clippy warnings`
  or similar.

## Verification

1. `cargo clippy --all-targets` -- expect zero warnings.
2. `just test-rust` -- exercises the recover.rs param-struct refactor
   through its 13 test call sites and the journal/replace/credential_verify
   /lock paths through unit tests.
3. `just test-vm braid-recover` -- representative recover VM smoke test
   (defined in `flake.nix` as the `braid-recover` check); confirms the
   param-struct refactor preserves runtime behavior end-to-end.
   (`just test-rust` covers logic; the VM test covers integration.)
4. `git diff --stat` review -- confirm no unintended files touched
   beyond the list above.

## Out of scope

- Boxing `CredentialVerifyError::Luks.source` or `OpKind::Replace.new_target`
  (we explicitly chose `#[allow]` for these).
- Any code beyond what clippy currently flags. The 20 warnings are the
  full scope.
- Touching `cli/Cargo.toml` to set crate-wide lint levels -- per-item
  `#[allow]` is the chosen mechanism.
