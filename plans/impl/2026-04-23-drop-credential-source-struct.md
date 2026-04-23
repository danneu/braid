---
status: Draft
---

# Plan: drop `CredentialSource`, pass flag fields directly to `resolve_credential`

## Context

`feature-findings/unlock.md` flags an "OpenCredential/CredentialSource split that only one production command uses both halves of" at `cli/src/mount.rs:28-68`. Verification (just performed):

- `CredentialSource` has exactly two construction sites:
  - `cli/src/unlock.rs:76-80` -- direct passthrough of all three flag fields from `UnlockParams`.
  - `cli/src/recover.rs:247-251` -- passes two flag fields from `RecoverParams`; `key_file` is hardcoded `None` because `recover` does not expose `--key-file`.
- The type is never used in tests. Tests build `OpenCredential` directly (e.g. `test_passphrase()` at `cli/src/mount.rs:720-721`; direct construction at `mount.rs:2016, 2378, 2435, 2599, 2872, 2937`) and pass them to `execute_open_plan`.
- `CredentialSource<'a>` carries a lifetime that exists purely to borrow the same `&Path` fields already borrowed by `UnlockParams<'a>`/`RecoverParams<'a>`.

The unlock.md entry's proposed fix ("have `UnlockParams`/`RecoverParams` own an `Option<OpenCredential>` directly (constructed by `main.rs`)") would regress the "no prompt when every mapper is already open" UX rule at `cli/src/unlock.rs:70-73`, because `main.rs` would have to resolve the credential before `plan_open_pool` runs. The callsite-level gate must stay; only the intermediate `CredentialSource` bag should be removed.

Intended outcome: delete `CredentialSource` and inline its three fields as parameters of `resolve_credential`. Zero behavior change; simpler surface; one fewer type with a lifetime parameter.

## Approach

Change `resolve_credential`'s signature from a `&CredentialSource<'_>` argument to three parameters matching the fields it already reads, then update the two callsites and delete `CredentialSource`.

New signature:

```rust
pub fn resolve_credential(
    passphrase_stdin: bool,
    passphrase_file: Option<&Path>,
    key_file: Option<&Path>,
) -> Result<OpenCredential, MountError>
```

The body is unchanged (same resolution order: `key_file` -> passphrase file/stdin/TTY).

## Unlock-side regression coverage

The unlock-side UX rule ("never read a credential when `plan.to_unlock` is empty") is already pinned by `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` at `cli/src/unlock.rs:1128` (added by commit `c4a32d4`, the `execute_open_plan` split). That test points `passphrase_file` at a nonexistent path and proves `cmd_unlock` returns `Ok(())` without ever attempting to read the file; if a regression hoists `resolve_credential` above the `plan.to_unlock.is_empty()` branch, `read_passphrase` surfaces a "failed to read passphrase file" error and the test fails. That is a stronger signal than the test the earlier draft of this plan proposed, and no new test is needed.

Symmetric partner on the recover side: `recover_with_all_mappers_open_still_resolves_credential_for_cycle` at `cli/src/recover.rs:1471` (HEAD line number; was 1459 in the earlier draft).

## Changes

### `cli/src/mount.rs`

- Delete the `CredentialSource<'a>` struct and its doc comment (`mount.rs:38-46`).
- Update `resolve_credential` (`mount.rs:48-68`) to the signature above; replace `source.key_file` / `source.passphrase_file` / `source.passphrase_stdin` with the plain parameters.
- Update the doc comment on `OpenCredential` (`mount.rs:26-32`) to remove the `CredentialSource` reference; keep the note about `execute_open_plan`'s strict validation.

### `cli/src/unlock.rs`

- At `unlock.rs:73-82`, replace the `CredentialSource { ... }` construction + `resolve_credential(&source)` call with a single direct call:

  ```rust
  let credential = if plan.to_unlock.is_empty() {
      None
  } else {
      Some(mount::resolve_credential(
          params.passphrase_stdin,
          params.passphrase_file,
          params.key_file,
      )?)
  };
  ```

### `cli/src/recover.rs`

- At `recover.rs:245-258`, replace the `CredentialSource { ... }` construction + `resolve_credential(&source)` call with:

  ```rust
  let credential = match plan.as_ref() {
      Some(_) => Some(
          mount::resolve_credential(
              params.passphrase_stdin,
              params.passphrase_file,
              None, // recover does not expose --key-file today
          )
          .map_err(|e| RecoverError::Failed(format!("recover: {e}")))?,
      ),
      None => None,
  };
  ```

  Keep the existing "eager resolve for the cycle" comment above the block verbatim.

## Non-goals

- Do NOT change `UnlockParams` / `RecoverParams`. Their `'a` lifetimes are justified by `config: &'a Config`, `paths: &'a StatePaths`, `membership: &'a PoolMembership`, and the `Option<&'a Path>` passphrase/key fields; removing `CredentialSource` does not eliminate them.
- Do NOT change `OpenCredential`, `execute_open_plan`, or any test construction of `OpenCredential`. Tests bypass `CredentialSource` entirely today and continue to.
- Do NOT move credential resolution into `main.rs`. The callsite gates at `unlock.rs:73` ("no prompt when every mapper is already open") and `recover.rs:245` ("eager resolve for cycle") are load-bearing and must stay where they are.

## Critical files

- `cli/src/mount.rs` -- delete struct, change function signature.
- `cli/src/unlock.rs` -- update single callsite.
- `cli/src/recover.rs` -- update single callsite.

## Verification

- `cargo check -p braid-cli` -- compile.
- `just test-rust` -- Rust unit tests. Two load-bearing regression tests must pass:
  - `recover_with_all_mappers_open_still_resolves_credential_for_cycle` at `recover.rs:1471` -- guards against dropping the eager read in recover (and thus breaking the post-mount relock cycle).
  - `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` at `unlock.rs:1128` -- guards against moving unlock toward eager resolution. Already present at HEAD; must continue to pass after the refactor.
- `grep -rn CredentialSource cli/src/` -- should return zero matches after the refactor.
- `just test-vm unlock recover` (or equivalent scenario subset) -- confirm the end-to-end unlock and recover paths are unchanged. Optional if `just test-rust` is green and the grep is clean, since the change is purely a signature refactor with no behavior change.

## Risks

Low. The refactor is mechanical: one struct deleted, one function signature changed, two callsites updated. No test touches `CredentialSource`, and the UX-critical gates at both callsites are preserved verbatim.
