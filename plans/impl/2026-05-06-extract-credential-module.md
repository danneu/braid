# Move `OpenCredential` + `resolve_credential` out of `mount.rs`

## Context

`OpenCredential` and `resolve_credential` live at `cli/src/mount.rs:32-81`
today. They're a credential abstraction (a 2-variant owned enum with a
zeroize-on-drop contract, plus an 11-line flag-router that reads
file/stdin/TTY) marooned in a 4148-line mount module. The function
returns `MountError` purely because `luks::read_passphrase` returns
`LuksError` and `MountError::Luks` happens to be in scope -- there is no
real coupling to mount logic.

Three production call sites already cross the module boundary to use
them: `unlock.rs:116`, `recover.rs:829`
(`execute_recover_initial_open`), and `recover.rs:1820`
(`discover_add_targets_before_mount`). `recover.rs` additionally threads
`OpenCredential` through 14+ type positions and pattern-matches on it in
two helpers (`recover_passphrase`, `open_credential_passphrase`). The
type is genuinely a credential primitive, not a mount one.

The project has clear precedent for hiving credential code off mount:
`cli/src/credential_verify.rs` already owns the borrowed-credential
verification surface (`Credential<'a>`, `CredentialVerifyTarget`,
`CredentialVerifyError`, `verify_credential_for_targets`). A sibling
`credential.rs` for the owned-credential resolution surface fits the
same pattern.

Outcome: `mount.rs` sheds a credential primitive it never owned the
intent of; `credential.rs` becomes the single home for resolving raw
credential flags into a typed, zeroizing value. No behavior change, no
test rewrites beyond import paths.

## Scope

Two logically separable commits, in order:

1. **Move** -- relocate the type and function as-is, fix imports.
2. **Tighten** -- add an `OpenCredential::as_borrowed()` method and
   collapse the inline conversion at `mount.rs:695-697`.

Commit 2 is a small follow-on; if it falls out cleanly during commit 1
they can ship together, but split if review surface gets noisy.

## Files modified

- `cli/src/lib.rs` -- add `pub mod credential;` between
  `credential_verify` (line 8) and `discover` (line 9), keeping the two
  credential modules adjacent. Prefix it with a `///` doc comment per
  the project's "doc-comment new public boundaries" rule (AGENTS.md
  "Doc Comments" section).
- `cli/src/credential.rs` -- new file. Owns `OpenCredential` (with
  `Debug` impl) and `resolve_credential`.
- `cli/src/mount.rs` -- delete the moved items (lines 32-81). Adjust
  top-level imports: `use std::path::{Path, PathBuf};` becomes
  `use std::path::Path;` (no remaining `PathBuf` use after the move),
  and `use zeroize::Zeroizing;` moves *into* the `#[cfg(test)]` block
  because the only remaining users are tests at lines 968 and 3617.
  Update the inline conversion at lines 695-697 (commit 2 only).
  Update in-module test imports (8 sites -- see below).
- `cli/src/unlock.rs` -- swap `mount::resolve_credential(...)` ->
  `credential::resolve_credential(...)` at line 116; wrap the result
  with `.map_err(MountError::from)?` so the error continues to flow
  through `UnlockError::Mount(MountError::Luks(...))`.
- `cli/src/recover.rs` -- swap two call sites (lines 829, 1820); update
  the `use crate::mount::{...OpenCredential...}` import at line 9 to
  pull `OpenCredential` from `crate::credential` instead. Existing
  `.map_err(|e| RecoverError::Failed(format!("recover: {e}")))?`
  wrappers continue to work because `LuksError`'s `Display` is the same
  string.
- `cli/src/luks.rs:222` -- update the doc-comment cross-reference from
  `mount::resolve_credential` to `credential::resolve_credential`.

### Final import map

For unambiguity, the post-refactor import lines are:

- `cli/src/lib.rs` (around line 8-9):
  ```rust
  pub mod credential_verify;
  /// Owned, fully-resolved credential values (`OpenCredential`) shared
  /// by `unlock` and `recover` plus the flag-router that produces them.
  /// Sibling to `credential_verify` (borrowed-credential verification).
  pub mod credential;
  pub mod discover;
  ```
- `cli/src/mount.rs` top-of-file:
  ```rust
  use std::path::Path;            // was: use std::path::{Path, PathBuf};
  // delete: use zeroize::Zeroizing;
  ```
  Inside the `#[cfg(test)]` block (around line 875, alongside other
  test-only imports):
  ```rust
  use crate::credential::OpenCredential;
  use zeroize::Zeroizing;
  ```
- `cli/src/unlock.rs` top-of-file (no `use crate::credential;` line --
  call site uses fully-qualified `crate::credential::resolve_credential`):
  ```rust
  // unchanged top imports
  ```
  Call site at line 116:
  ```rust
  let credential = crate::credential::resolve_credential(
      params.passphrase_stdin,
      params.passphrase_file,
      params.key_file,
  )
  .map_err(MountError::from)?;
  ```
- `cli/src/recover.rs` line 9 area:
  ```rust
  use crate::credential::{self, OpenCredential};
  use crate::mount::{self, MountError, OpenPlan};
  ```
  Call sites at lines 829 and 1820: `mount::resolve_credential(...)` ->
  `credential::resolve_credential(...)`.
- `cli/src/luks.rs:222` doc:
  ```text
  /// Used by `replace`, `enroll-key-file`, and
  /// `credential::resolve_credential`
  ```

## Detailed steps

### Commit 1: move

1. **Create `cli/src/credential.rs`** with the moved items. Adjust the
   doc comments to drop mount-internal references (the current docs
   mention `execute_unlock_and_mount`, `execute_mount_only`,
   `cmd_unlock`, `cmd_recover` -- those are still relevant context for
   *callers* deciding when to invoke, so keep that explanation; just
   reword the type-level doc to not assert "produced by
   resolve_credential" / "passed to execute_unlock_and_mount" since
   those are call-site concerns, not type invariants).

   Skeleton:

   ```rust
   use crate::luks::{self, LuksError};
   use std::path::{Path, PathBuf};
   use zeroize::Zeroizing;

   /// Owned, fully-resolved credential ready to drive `cryptsetup
   /// open`. Plaintext is scrubbed on drop via `Zeroizing`.
   ///
   /// Owned (no lifetime parameter) because callers hold the resolved
   /// value across multiple operations -- e.g. `cmd_recover` reuses
   /// the same credential for the initial mount and the post-resume
   /// relock cycle.
   pub enum OpenCredential {
       Passphrase(Zeroizing<String>),
       KeyFile(PathBuf),
   }

   impl std::fmt::Debug for OpenCredential { /* unchanged */ }

   /// Resolve credential flag inputs into an owned, fully-resolved
   /// `OpenCredential`. ALWAYS reads -- callers decide whether to
   /// invoke this, because the "should we prompt now?" rule differs
   /// by command:
   ///
   /// - `cmd_unlock` skips this call entirely when `plan.to_unlock`
   ///   is empty (the no-prompt-when-all-mappers-open UX rule).
   /// - `cmd_recover` calls this whenever the pool is not yet
   ///   mounted, even if the initial plan's `to_unlock` is empty,
   ///   because the post-mount relock cycle will close every mapper
   ///   and need to reopen them.
   ///
   /// Resolution order: `key_file` (if provided) -> passphrase
   /// (file/stdin/TTY).
   pub fn resolve_credential(
       passphrase_stdin: bool,
       passphrase_file: Option<&Path>,
       key_file: Option<&Path>,
   ) -> Result<OpenCredential, LuksError> {
       if let Some(kf) = key_file {
           return Ok(OpenCredential::KeyFile(kf.to_path_buf()));
       }
       let pp = luks::read_passphrase(passphrase_file, passphrase_stdin)?;
       Ok(OpenCredential::Passphrase(pp))
   }
   ```

   Note the return type changed from `Result<_, MountError>` to
   `Result<_, LuksError>` -- this is the only way to break the
   credential -> mount back-edge. Both call sites already convert
   appropriately (see step 4-5).

2. **Add `pub mod credential;`** to `cli/src/lib.rs` immediately after
   `pub mod credential_verify;` (alphabetical order would put it
   *before*; we deliberately group siblings by keeping
   `credential` adjacent to `credential_verify`. The existing file
   already breaks alphabetic order in two places -- the blank line
   before `lock`, and `mount_check` after `mount` -- so this is
   consistent with project style).

3. **Delete the moved items from `mount.rs`** (lines 32-81 inclusive).
   Also remove the top-level `use zeroize::Zeroizing;` (line 15) and
   trim `use std::path::{Path, PathBuf};` (line 14) to
   `use std::path::Path;` -- both top-level imports lose their
   only normal-build users when `OpenCredential` moves out. The
   `Zeroizing` import gets re-added inside the `#[cfg(test)]` block in
   step 6; the test sites at lines 968 and 3617 are the only remaining
   users.

4. **Fix `unlock.rs:116-120`**: route through the new module via the
   fully-qualified path (no new top-level `use`) and convert the error:

   ```rust
   let credential = crate::credential::resolve_credential(
       params.passphrase_stdin,
       params.passphrase_file,
       params.key_file,
   )
   .map_err(MountError::from)?;
   ```

5. **Fix `recover.rs`**:
   - Line 9 import: change
     `use crate::mount::{self, MountError, OpenCredential, OpenPlan};`
     to:
     ```rust
     use crate::credential::{self, OpenCredential};
     use crate::mount::{self, MountError, OpenPlan};
     ```
   - Line 829 call site: `mount::resolve_credential(...)` ->
     `credential::resolve_credential(...)`. The existing
     `.map_err(|e| RecoverError::Failed(format!("recover: {e}")))?`
     wrapper continues to work because `LuksError: Display` produces
     the same string `MountError::Luks(_)` did.
   - Line 1820 call site: same change.

6. **Fix `mount.rs` test imports** (8 sites referencing
   `OpenCredential` inside the `#[cfg(test)]` block, lines 968,
   2589, 3360, 3560, 3617, 3787, 4059, 4124, plus 2 sites referencing
   `Zeroizing::new(...)` at lines 968 and 3617). Add to the top of the
   `#[cfg(test)]` block (around line 875):
   ```rust
   use crate::credential::OpenCredential;
   use zeroize::Zeroizing;
   ```
   No per-site changes needed.

7. **Fix `luks.rs:222` doc cross-reference**: change
   `mount::resolve_credential` to `credential::resolve_credential`
   in the `///` comment on `read_passphrase`.

### Commit 2: tighten conversion

The inline conversion at `mount.rs:695-697`:

```rust
let cred = match credential {
    OpenCredential::Passphrase(pp) => Credential::Passphrase(pp.as_str()),
    OpenCredential::KeyFile(kf) => Credential::KeyFile(kf.as_path()),
};
```

is a candidate for an `OpenCredential::as_borrowed()` method on the new
type. It introduces a one-way dependency from `credential.rs` to
`credential_verify.rs::Credential`, which is fine -- "the borrowed view
of a resolved credential" is a credential concern, not a mount one.

Add to `credential.rs`:

```rust
use crate::credential_verify::Credential;

impl OpenCredential {
    /// Borrowed view for callers (verify, cryptsetup open) that take a
    /// `Credential<'_>` without taking ownership.
    pub fn as_borrowed(&self) -> Credential<'_> {
        match self {
            OpenCredential::Passphrase(pp) => Credential::Passphrase(pp.as_str()),
            OpenCredential::KeyFile(kf) => Credential::KeyFile(kf.as_path()),
        }
    }
}
```

Then `mount.rs:695-697` becomes one line: `let cred = credential.as_borrowed();`.

Quick scan during implementation: check whether `recover.rs` has an
inline copy of the same conversion that should also collapse. (None
spotted in the verify-issue read; if found, fold them into this commit.)

## Verification

Behavior is unchanged -- this is a pure relocation plus an error-type
narrowing. The verification budget is:

1. **Build**: `cargo check -p braid-cli` (or via the workspace) -- must
   compile clean. Catches missing imports, the `MountError` -> `LuksError`
   change at the unlock call site, and any test import I missed.
2. **Rust unit tests**: `just test-rust`. The 8 in-module mount tests
   that construct `OpenCredential` exercise the post-rename type path.
   Golden-fixture tests are unaffected (no parser surface touched).
3. **VM tests covering credential flow** (single-shot, no `-v`):
   ```
   just test-vm braid-unlock braid-unlock-key-file braid-recover
   ```
   - `braid-unlock` -- exercises the passphrase/stdin path end-to-end;
     `just test-rust` covers `luks::read_passphrase` file/stdin/TTY
     branch selection.
   - `braid-unlock-key-file` -- exercises the early-return keyfile
     branch.
   - `braid-recover` -- exercises the recover-eager-resolve path
     (`execute_recover_initial_open` at the new `credential::` import).
4. **No fixture refresh needed** -- parser-critical tool versions
   (`btrfs-progs`, `cryptsetup`, `util-linux`, `nut`) are untouched.

If `cargo check`, `just test-rust`, and the three VM checks pass, the
refactor is safe to ship.

## Out of scope / non-goals

- **Do NOT merge `credential_verify.rs` into `credential.rs`.** They
  have distinct responsibilities (verify vs resolve) and the existing
  module is fine where it is. Renaming would create churn without
  payoff. A future reader who finds both modules will see the clean
  semantic split (`credential.rs` = "what is a credential", `credential_verify.rs` =
  "is this credential authentic").
- **Do NOT touch `RecoverPassphrase` or `recover_passphrase` /
  `open_credential_passphrase`** in `recover.rs`. Those are
  recover-internal helpers for "borrowed-or-owned passphrase, with
  recover-specific error wording" -- moving them broadens scope and
  doesn't share a root cause with this finding.
- **Do NOT add a `CredentialError` wrapper** around `LuksError`. One
  variant is overengineered; `LuksError` is the only error path
  through `resolve_credential` and both call sites already convert it
  cleanly.
- **Do NOT renumber or alphabetize `lib.rs` module declarations.** The
  file already has a few intentional groupings; insert `credential`
  next to `credential_verify` and stop there.
