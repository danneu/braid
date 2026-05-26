# Fix stale "first disk" / "both disks" verification prose in unlock docs + test comment

## Context

A review finding flagged `docs/commands/unlock.md:65` for claiming `braid
unlock` "Verifies the passphrase against the first unlockable disk." The code
verifies the credential against **every** to-unlock disk
(`credential_verify_targets` at `cli/src/mount.rs:486` feeds **all** disks into
`verify_credential_for_targets`, which loops over every target at
`cli/src/credential_verify.rs:46`). This contradicts Principle 4
(`docs/design/principles.md:34`, single passphrase) and the internals doc, which
already says "verify against every pool disk" (`docs/internals/luks-unlock.md:61-62`).

Investigating the fix surfaced **two more stale artifacts with the same root
cause**. Commit `1825997 fix(cli): verify credentials across all relevant LUKS
disks` (2026-04-30) moved unlock from "open disks sequentially" to "preflight
`--test-passphrase` against every disk, then open." After that change the error
names **only the failing disk**, but prose written for the old behavior still
says the error "names both disks":

1. `docs/commands/unlock.md:94` (safety-checks list).
2. `cli/src/mount.rs:1650` -- the `/// Intent` doc-comment of test
   `mount_passphrase_mismatch_names_disk`, which **contradicts its own
   assertions** (`msg.contains("disk2")` + `!msg.contains("disk1")` at
   `cli/src/mount.rs:1708-1715`).

The code is correct. The two pinned tests' *assertions* are correct
(`passphrase_mismatch_names_failing_disk` at `cli/src/unlock.rs:446`,
`mount_passphrase_mismatch_names_disk` at `cli/src/mount.rs:1658`). Only prose is
stale. Outcome: the unlock page and the test intent tell the same story as the
code, so this class of finding stops recurring.

## Scope

Four prose edits across two files (`docs/commands/unlock.md` x3,
`cli/src/mount.rs` x1), zero behavior change. Two drivers, both pointing toward
generic, accurate prose:

- **The cited finding:** step 3 says the credential is verified against the
  "first" disk; the code verifies *every* disk.
- **Follow-up review finding (credential-generic):** the page documents
  `braid unlock --key-file` and the keyfile path shares the same verify/open
  code, so passphrase-only wording (steps 3-4 and the safety bullet) is wrong
  for keyfile users -- the prose must be generic over the *selected credential*.
- **Same-root-cause siblings (`1825997` preflight refactor):** the "names both
  disks" claim (safety bullet + a stale test doc-comment) describes pre-refactor
  sequential-open behavior; the error now names only the failing disk.

The user asked for the *ideal* fix, not the minimum-diff patch, so all four are
included.

## Changes

### 1. `docs/commands/unlock.md:65-66` -- steps 3 and 4 (the cited line + its pair)

Both steps live under "What happens under the hood" and describe the shared
verify-then-open flow, so both go credential-generic.

- Step 3 before: `3. Verifies the passphrase against the first unlockable disk`
- Step 3 after:  `3. Verifies the selected credential against every disk it will unlock`
- Step 4 before: `4. Opens LUKS mappers for all locked disks using the verified passphrase`
- Step 4 after:  `4. Opens LUKS mappers for all locked disks using the verified credential`

Use "selected credential", not "passphrase": `braid unlock --key-file` is
documented on this same page (lines 30-34, 57) and routes through the identical
path -- `resolve_credential(...)` at `cli/src/unlock.rs:103` yields a
`Credential` (passphrase *or* keyfile) that `open_disks_with_credential` verifies
and opens the same way (`credential_noun` at `cli/src/mount.rs:498` prints
"passphrase" or "keyfile"). "every disk it will unlock" matches the `to_unlock`
set and forecloses the finding's misreading (that a divergent slot-0 on a
non-first disk could pass silently). Principle 4's "single passphrase" rule
explicitly extends to keyfile credentials (`docs/design/principles.md:34`), so
generic wording stays faithful to it.

### 2. `docs/commands/unlock.md:94` (sibling stale line, "Safety checks")

- Before: `- If a disk rejects the passphrase after another disk accepted it, the error names both disks (indicates someone changed a disk's passphrase outside braid)`
- After:  `- If any disk rejects the selected credential during verification, unlock fails before opening any mapper and names the failing disk. If another disk already accepted the same credential, that points to disk-specific credential drift outside braid.`

Three fixes in one line: (a) credential-generic noun -- the error message itself
is generic (`credential_noun` prints "passphrase" or "keyfile",
`cli/src/mount.rs:498` / `:525`); (b) "names the failing disk", not "both disks";
(c) the "outside braid" inference is now *conditioned* on an earlier disk having
accepted the same credential. A first-disk rejection is just a wrong credential
-- the common typo case, pinned as `wrong passphrase (rejected by disk1)` by
`unlock_passphrase_verify_fails_ok_header_preserves_wrong_passphrase`
(`cli/src/mount.rs:3328` / `:3360`) -- so the divergence hint must not fire
unconditionally. The original line carried this condition ("after another disk
accepted it") but welded it to the stale "both disks"; the rewrite keeps the
condition and drops the stale claim.
Accurate: preflight verifies every disk and, on rejection, returns before the
open loop (`cli/src/mount.rs:518-533` runs before the `for ... in to_unlock`
open loop at `:552`), naming only `target.name`
(`"wrong {noun} (rejected by {})"`, `cli/src/mount.rs:525`). The rare
post-preflight open rejection (race/TOCTOU, `cli/src/mount.rs:582-588`) also
names a single disk, so "both disks" is wrong on every path.

### 3. `cli/src/mount.rs:1649-1650` (stale test doc-comment)

- Before:
  ```
  /// Intent: When a passphrase is verified against disk1 but rejected by
  /// disk2, the error must name both disks.
  ```
- After:
  ```
  /// Intent: When a passphrase verifies on disk1 but is rejected on disk2
  /// during preflight, the error must name only the failing disk (disk2),
  /// not the disk that already verified.
  ```

This makes the `/// Intent` match the test's own assertions
(`cli/src/mount.rs:1708-1715`). Comment-only; the test name
(`..._names_disk`, singular) and assertions are already correct and unchanged.
Stays passphrase-worded on purpose -- this test is passphrase-specific
(`test_passphrase()` fixtures); the credential-generic wording belongs on the
user-facing page, not on a per-credential test.

## Do NOT change

- Any Rust logic, error strings, or test assertions -- all correct.
- `docs/internals/luks-unlock.md` -- already says "verify against every pool
  disk."

## Verification

- Re-read assertions at `cli/src/mount.rs:1708-1715` and
  `cli/src/unlock.rs:496-509` and confirm the revised safety-bullet (edit 2) and
  test-comment (edit 3) prose matches them in meaning (names the failing disk,
  not the disk that already verified).
- `just test-rust` -- confirms `mount_passphrase_mismatch_names_disk` and
  `passphrase_mismatch_names_failing_disk` still pass (comment-only change cannot
  break them; this is confirmation the prose now matches green tests).
- `mdbook build docs` -- confirms the unlock page still builds and
  mdbook-linkcheck passes (no links touched, but it's the project's doc gate).
- Optional manual read of the rendered "What happens under the hood" and "Safety
  checks" sections to confirm the page no longer self-contradicts (old 65 vs 94)
  and reads correctly for a `--key-file` user.
- No new behavioral test: these are prose/comment-only edits. Existing coverage
  is adequate -- the credential verifier tests already pin all-target
  verification and first-rejection (`cli/src/credential_verify.rs`), and the
  mount/unlock tests pin failing-disk naming. `mdbook build docs` is the doc
  gate.
