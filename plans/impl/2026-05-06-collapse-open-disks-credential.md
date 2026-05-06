# Collapse `open_disks_with_*` into `open_disks_with_credential`

## Context

`cli/src/mount.rs` currently hosts two near-mirror copies of the
unlock-execute helper:

- `open_disks_with_passphrase` (lines 520-621, ~100 lines)
- `open_disks_with_key_file`   (lines 623-724, ~100 lines)

They differ in only five places: the credential parameter type, the
per-disk open call, and four narrative strings that swap the noun
"passphrase" <-> "keyfile". Both are private to `mount.rs` and only
called from `execute_unlock_and_mount`.

A `/verify-issue` investigation triggered this plan. The original
finding suggested two changes; one is rejected and one is kept:

- **Rejected (part 1):** "Fold `verify_credential_for_targets` into
  `mount.rs`." The finding's premise is wrong --
  `verify_credential_for_targets` has 8 production call sites across
  5 files (`mount.rs:528`, `mount.rs:631`, `add.rs:690`,
  `replace.rs:370`, `enroll_key_file.rs:161`,
  `enroll_key_file.rs:234`, `recover.rs:1878`, `recover.rs:2365`),
  and `replace.rs:374` passes a non-stderr `emit` closure
  (`emit_replace_stderr`). Inlining would either strand the other 4
  commands or re-duplicate the loop in 5 places. Leave
  `credential_verify.rs` alone.
- **Kept (part 2):** Collapse the two `open_disks_with_*` helpers
  into one parameterized over the existing `Credential<'_>` ADT.

Outcome: ~100 lines of duplication removed in `mount.rs` with no
behavior change; existing exit-2-name-the-disk regression tests pass
unchanged because all four narrative strings are reproduced verbatim
via a noun substitution.

## Design

### 1. Single helper signature

Replace both functions with one private helper:

```rust
fn open_disks_with_credential<R: CommandRunner>(
    runner: &R,
    to_unlock: &[(String, ByIdPath)],
    credential: Credential<'_>,
    color_enabled: bool,
    opened: &mut Vec<MapperName>,
) -> Result<(), MountError>
```

Use the borrowed `Credential<'_>` ADT from
`cli/src/credential_verify.rs:12-16` (already imported at
`cli/src/mount.rs:4`). It is `Copy`, already the verifier's
currency, and lifetime stays scoped to the caller's match arm so the
`OpenCredential::Passphrase(Zeroizing<String>)` keeps living through
the call -- no zeroization regression.

### 2. Noun substitution helper

Add a tiny private helper near the new function:

```rust
fn credential_noun(c: Credential<'_>) -> &'static str {
    match c {
        Credential::Passphrase(_) => "passphrase",
        Credential::KeyFile(_) => "keyfile",
    }
}
```

Bind `let noun = credential_noun(credential);` once at the top of
the unified body, then format the four narrative strings with
`{noun}`. This preserves every existing user-facing string verbatim:

- `"{noun} rejected on '{}'"` -- replaces lines 537 / 640
- `"wrong {noun} (rejected by {})"` -- replaces lines 539 / 642
- `"...after all planned-disk {noun} verification..."` -- replaces
  lines 588 / 691
- `"...even though the {noun} was just verified..."` -- replaces
  lines 591-592 / 694-695

Helper stays private to `mount.rs`. Do **not** promote it to
`credential_verify.rs` -- the verifier already has `CredentialKind`
for its own UI; this is a `mount.rs` error-wording detail.

### 3. Per-disk open dispatch -- match locally, do not push down

Inside the per-disk loop, dispatch to the right luks helper with a
single match:

```rust
let outcome = match credential {
    Credential::Passphrase(pp) =>
        luks::ensure_luks_open(runner, name, by_id, pp),
    Credential::KeyFile(kf) =>
        luks::ensure_luks_open_with_key_file(runner, name, by_id, kf),
};
```

Do **not** add a `luks::ensure_luks_open_with_credential` wrapper.
`ensure_luks_open` (the passphrase variant) has independent
production callers in `add.rs:755,916`, `replace.rs:499,519`, and
`recover.rs:1813,2058,2107,2165` that pass a `&str` directly --
none of them need a unified credential entry point.
`ensure_luks_open_with_key_file` is currently mount-only
(`mount.rs:678` is the sole production call site). A
credential-dispatch wrapper would therefore exist solely to serve
this one refactor while adding API surface to `luks.rs`. Keep the
match local to `mount.rs`.

### 4. Caller collapse in `execute_unlock_and_mount`

Replace the two-arm `match credential` block at
`cli/src/mount.rs:788-816` with a single call:

```rust
let cred = match credential {
    OpenCredential::Passphrase(pp) => Credential::Passphrase(pp.as_str()),
    OpenCredential::KeyFile(kf)   => Credential::KeyFile(kf.as_path()),
};
open_disks_with_credential(
    runner,
    &plan.to_unlock,
    cred,
    color_enabled,
    &mut opened_mappers,
)
.map_err(|error| UnlockAndMountFailure {
    error,
    opened_mappers: opened_mappers.clone(),
})?;
```

`OpenCredential` (`cli/src/mount.rs:39-42`) stays unchanged -- it
remains the owned, `Zeroizing`-wrapped representation that flows
through `resolve_credential` and into `execute_unlock_and_mount`.

### 5. Error shapes are byte-identical

The two existing functions construct the same `MountError` variants
in the same branches:

- `CredentialVerifyError::Rejected` -> `explain_open_failure(...)`
  with `MountError::Failed("wrong {noun} (rejected by ...)")`
  fallback.
- `CredentialVerifyError::Luks { source: OpenFailed{..} }` ->
  `explain_open_failure(...)` with `MountError::Luks(e)` fallback.
- `CredentialVerifyError::Luks { .. }` (catch-all) ->
  `MountError::Luks(source)`.
- Per-disk open exit-2 -> `MountError::Failed(...)` with the
  "...{noun} verification..." narrative.
- Per-disk open other failure -> `MountError::Luks(e)` with
  `"cryptsetup open failed on '{name}': {e}"` summary.

The unified function reproduces all five branches with `noun`
substituted; no ordering or branch shape changes.

## Files modified

- `cli/src/mount.rs` -- main refactor.
  - Delete `open_disks_with_passphrase` (lines 520-621).
  - Delete `open_disks_with_key_file` (lines 623-724).
  - Add `open_disks_with_credential` and `credential_noun`.
  - Update doc comment near the new function (replace the
    "Mirrors `open_disks_with_key_file`" cross-reference at
    line 517-519, since there's only one helper now).
  - Collapse the two `match credential` arms in
    `execute_unlock_and_mount` (lines 788-816) to a single call.

- `tests/cli/braid-unlock-key-file.py` -- pin keyfile rejection
  wording (covers a test-coverage gap for the keyfile branch of
  `credential_noun`).
  - Test 2 (lines 99-105) currently only asserts
    `ret[0] == 1`. After the wrong-keyfile run, also assert that
    `ret[1]` (combined stdout+stderr from `2>&1`) contains
    `wrong keyfile (rejected by disk1)`. Without this, a buggy
    `credential_noun(Credential::KeyFile(_))` returning
    `"passphrase"` would silently produce wrong wording and every
    Rust unit test plus the existing exit-code assertion would
    still pass.

- `docs/decisions/021-wait-in-unlock.md` -- replace stale helper
  names so future readers find the unified helper.
  - Line 95-96 currently reads
    `cli/src/mount.rs -- open_disks_with_passphrase,
    open_disks_with_key_file, and scan_and_mount host the new rows.`
    Update to
    `cli/src/mount.rs -- open_disks_with_credential and
    scan_and_mount host the new rows.`

No changes to:
- `cli/src/credential_verify.rs` (kept shared as-is)
- `cli/src/luks.rs` (no new wrapper)
- `cli/src/unlock.rs`, `add.rs`, `replace.rs`, `recover.rs`,
  `enroll_key_file.rs` (untouched)

## Reused symbols (do not redefine)

- `Credential<'a>` -- `cli/src/credential_verify.rs:12-16`
- `CredentialVerifyError`, `CredentialVerifyTarget`,
  `verify_credential_for_targets` --
  `cli/src/credential_verify.rs:18-72`
- `credential_verify_targets` (the targets builder) --
  `cli/src/mount.rs:505-513`
- `explain_open_failure` -- `cli/src/mount.rs:478-503`
- `luks::ensure_luks_open`, `luks::ensure_luks_open_with_key_file`,
  `luks::probe_luks_header`, `luks::LuksError`, `OpenOutcome`,
  `LuksHeaderState` -- `cli/src/luks.rs`
- `status_line`, `StatusTag`, `mapper_name`, `color_enabled_for_stderr`
  -- existing `mount.rs` neighbors
- `OpenCredential` -- `cli/src/mount.rs:39-42` (unchanged)

## Verification

1. `just test-rust` -- runs all Rust unit tests for the `braid-cli`
   crate. Specifically must pass:
   - `mount::tests::mount_passphrase_mismatch_names_disk`
     (`cli/src/mount.rs:1818-1912`) -- asserts `msg.contains("disk2")`,
     `!msg.contains("disk1")`. Exit-2 narrative must keep naming the
     failing disk.
   - `unlock::tests::passphrase_mismatch_names_failing_disk`
     (`cli/src/unlock.rs:652-794`) -- same naming guarantee plus
     `!msg.contains("Wrong passphrase?")`.
   - All `mount::tests::*` keyfile tests -- e.g.
     `mount_non_auth_open_failure_propagates_keyfile`
     (`cli/src/mount.rs:2618`) and any test that asserts on
     `"wrong keyfile"` in either positive or negative form
     (e.g. `cli/src/mount.rs:3687`, `4247`).
2. `just test-vm braid-unlock-key-file` -- runs the NixOS VM
   keyfile unlock test, which now pins the wrong-keyfile wording
   (`wrong keyfile (rejected by disk1)`). This is the assertion
   that catches a `credential_noun(Credential::KeyFile(_))` typo
   slipping through Rust unit tests.
3. `just test-vm` -- run the broader NixOS VM tests that exercise
   unlock end-to-end (passphrase + keyfile paths, plus add /
   replace / enroll / recover paths that share
   `verify_credential_for_targets`) to confirm no behavior
   regression at the integration level.
4. Source sanity check: after the refactor, the four narrative
   strings live as single noun-templated format strings, not as
   per-credential literals. Grep `cli/src/mount.rs` for the
   templated fragments -- each should appear exactly once:
   - `wrong {noun} (rejected by`
   - `planned-disk {noun} verification`
   - `the {noun} was just verified`
   - `{noun} rejected on`
   The full literals (`"wrong passphrase"`, `"wrong keyfile"`,
   `"passphrase verification"`, `"keyfile verification"`, `"the
   passphrase was just verified"`, `"the keyfile was just
   verified"`) should NOT appear in `cli/src/mount.rs` source
   anymore -- they are produced at runtime via `{noun}`. They should
   still appear in expected-output assertions in `cli/src/mount.rs`,
   `cli/src/unlock.rs`, and `tests/cli/braid-unlock-key-file.py`.

## Out of scope

- **Inlining `verify_credential_for_targets`** -- rejected; 8
  production call sites across 5 files. Leave
  `credential_verify.rs` alone.
- **New Rust-level keyfile mirror unit test** (e.g. a
  `mount_keyfile_mismatch_names_disk` parallel to the passphrase
  one) -- not added. The keyfile-wording assertion lives in the
  VM test (`tests/cli/braid-unlock-key-file.py` Test 2) where it
  pins end-to-end output, which is the right layer for catching
  noun-substitution typos. A Rust-level mirror would be a
  near-duplicate of the existing passphrase test for marginal
  added coverage.
- **Pushing dispatch into `luks.rs`** (a new
  `ensure_luks_open_with_credential`) -- rejected; helps only this
  call site while adding API surface for no other caller.
- **Changing `OpenCredential`** -- it is the right shape (owned,
  `Zeroizing`) for the resolve-credential flow.
