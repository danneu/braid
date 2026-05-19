# Plan: Document the `braid enroll` sleep-inhibitor exclusion in decision 019

## Context

Decision 019 (`docs/decisions/019-inhibit-sleep.md`) is the authority for
when braid acquires a logind sleep inhibitor. It enumerates the commands
that DO acquire one (`add`, `remove`, `remove-missing`, `replace`, plus
`recover`'s replayed destructive paths) and includes one worked
exclusion -- `### Excluded: braid lock` -- explaining why lock
deliberately does not, framed through Principle 19's deciding question
("would suspend make the operation incorrect, unsafe, or expensive to
restart?").

`braid enroll` is also a mutating command -- it writes LUKS slot 1 on
each pool disk via `cryptsetup luksAddKey` (`cli/src/enroll_key_file.rs`'s
`apply_enrollment`, lines 285-334) -- but does not acquire an inhibitor,
and 019 does not mention it. A future contributor reading 019 cannot
tell whether the omission is intentional or an oversight. The lock
exclusion established a project pattern that mutating commands which
opt out of the inhibitor get an explicit deciding-question
justification; enroll is the only remaining mutating command outside
that pattern.

This plan is primarily a docs addition: insert a short `### Excluded:
braid enroll` subsection in 019 explaining why enroll passes the
deciding question without an inhibitor. The plan also adds one Rust
regression test that backs the doc's central no-journal claim with a
direct behavioral assertion (see "Regression test" below).

## Why this is docs, not code

The current behavior (no inhibitor on standalone `braid enroll`) is
correct on the deciding question. Each leg:

- **No journal write, no recovery-mode lockout.** Standalone enroll
  writes no operation journal of its own. `EnrollPlan::execute`
  (`cli/src/enroll_key_file.rs:450-493`) only checks for a pending
  journal during preflight (`check_no_pending_operation` at line 588)
  and never calls `journal::write_journal`. So an environmental
  inhibitor failure cannot strand the user in recovery mode, which is
  the failure mode 019's "Failure to acquire the inhibitor returns a
  `Validation`-shaped error before the journal is written" sentence
  protects against -- but enroll has nothing to protect there.
- **Recoverability (with a caveat for `--generate`).** `plan_enrollment`
  calls `probe_keyfile_enrollment` per candidate and short-circuits
  disks whose slot 1 already verifies the keyfile (`AlreadyEnrolled`
  at `cli/src/enroll_key_file.rs:211-219`). A suspend mid-loop leaves
  disks N+1..M un-enrolled; re-running `braid enroll DIR` (existing-
  keyfile path) skips the already-done disks and advances on the rest.
  The `--generate` retry path is *not* same-command idempotent:
  `validate_key_file_path` (`cli/src/enroll_key_file.rs:499-514`)
  refuses `--generate` when `DIR/braid.key` already exists, and that
  refusal is documented in the user-facing error message
  ("remove it manually if you want to generate a new one", line 506).
  A partial `braid enroll --generate` is therefore recovered by
  re-running `braid enroll DIR` (without `--generate`) against the
  now-generated keyfile -- the same advance-on-partial-state property
  that justifies `lock`'s exclusion, just routed through a different
  command line.
- **Bounded mutation window.** Per-disk cost is one Argon2-bounded
  `cryptsetup luksAddKey` (~2-3 sec on default parameters) plus a fast
  `cryptsetup luksHeaderBackup` (already classified as "fast
  bookkeeping that completes well under a second" in
  `docs/principles.md:107`). Three-disk pool: single-digit seconds
  total, no btrfs work in the same window.
- **No btrfs topology mutation; LUKS2 writes use cryptsetup metadata
  locking.** Enroll does not touch btrfs membership or chunk
  allocation -- the topology-corruption risk surface 019 was
  originally written to protect. Concurrent LUKS2 metadata writes
  are serialized by cryptsetup's flock()-based metadata locking
  (`reference/cryptsetup/docs/LUKS2-locking.txt`), not by kernel-
  level keyslot atomicity. After each successful `cryptsetup
  luksAddKey`, `apply_enrollment` writes a local `.luksheader` file
  via `luks::backup_luks_header_post_mutation`
  (`cli/src/enroll_key_file.rs:316`). Per `docs/luks-unlock.md:118-134`,
  that local file is a transient byproduct of a *successful*
  mutation -- it feeds the operator's off-system backup workflow,
  and `backup_luks_header_post_mutation`
  (`cli/src/luks.rs:547-557`) only runs after the mutation
  completes. The plan does NOT claim kernel-level slot atomicity,
  and it does NOT claim the local `.luksheader` recovers an
  interrupted write (an interrupted write produces no local
  backup, and `docs/luks-unlock.md`'s messaging invariant forbids
  pointing users at the local file as a recovery target anyway).

Adding an inhibitor to standalone `braid enroll` would be inconsistent
with 019's "acquire before journal write" boundary rule and would buy
no protection commensurate with the added complexity. The same
`luks::enroll_key_file` call IS held under an inhibitor when invoked
from `braid add --enroll` (`cli/src/add.rs`) or `braid replace --enroll`
(`cli/src/replace.rs:636,706`), but that is incidental -- those
commands already hold an inhibitor for their journal-protected btrfs
work, not for the keyfile call itself.

## Change

The implementation has two parts:

1. **Docs (primary).** Insert a new subsection in
   `docs/decisions/019-inhibit-sleep.md` between the current
   `### Excluded: braid lock` block (ends at line 172) and the
   `## Consequences` heading (begins at line 174).
2. **Regression test (secondary).** Add one new behavioral test to
   the existing `#[cfg(test)]` module in
   `cli/src/enroll_key_file.rs` that locks in the no-journal
   premise the doc relies on. Details in the "Regression test"
   section below.

Proposed text (final wording TBD during the implementation pass, but
this is the target shape and content):

```markdown
### Excluded: braid enroll

`braid enroll` does not acquire the sleep inhibitor despite mutating
LUKS slot 1 on each pool disk. Applying the deciding question to
standalone enroll specifically:

- **No journal, no recovery-mode lockout.** Standalone enroll writes
  no operation journal (`EnrollPlan::execute` in
  `cli/src/enroll_key_file.rs`). Suspend mid-loop cannot strand the
  operator in recovery mode, which is the failure surface 019's
  "Validation error before journal write" promise protects against
  for the four inhibitor-using commands.
- **Recoverability.** `plan_enrollment` probes each candidate via
  `probe_keyfile_enrollment` and short-circuits disks whose slot 1
  already verifies the keyfile (`AlreadyEnrolled`). A partial enroll
  leaves only the un-enrolled disks for the next invocation:
  re-running `braid enroll DIR` (existing-keyfile mode) advances on
  partial state, the same property that justifies `lock`'s exclusion.
  Note that `braid enroll --generate` is not same-command idempotent
  -- a partial `--generate` run leaves `DIR/braid.key` on disk, and
  `validate_key_file_path` refuses a second `--generate` against an
  already-present keyfile. Recovery for an interrupted `--generate`
  run is to drop `--generate` and re-run as a regular enroll against
  the now-existing keyfile.
- **Bounded mutation window.** Each disk pays one Argon2-bounded
  `cryptsetup luksAddKey` (~2-3 sec on default parameters) plus a
  sub-second `cryptsetup luksHeaderBackup`. A three-disk pool's total
  enroll window is single-digit seconds with no long-running btrfs
  work to protect.
- **No btrfs topology mutation; LUKS2 writes use cryptsetup
  metadata locking.** Enroll does not touch btrfs membership or
  chunk allocation, which is the topology-corruption risk surface
  this doc was written to protect. LUKS2 metadata writes are
  serialized by cryptsetup's own metadata locking. After each
  successful `cryptsetup luksAddKey`, `apply_enrollment` writes a
  local `.luksheader` as input to the existing off-system backup
  workflow (see `docs/luks-unlock.md`); the local file is a
  transient byproduct of a successful mutation, not a recovery
  mechanism for an interrupted one. Recovery from actual header
  damage uses the operator's off-system backup, identical to every
  other LUKS-mutating command in braid.

The same `luks::enroll_key_file` call is held under an inhibitor when
invoked from `braid add --enroll` or `braid replace --enroll`, but
that is incidental: those commands already hold an inhibitor for their
journal-protected btrfs work, and the keyfile call happens inside that
existing window. Standalone `braid enroll` has no btrfs work to protect
and no journal seam to guard, so an inhibitor would buy nothing.

If a future change adds long-running follow-up work to `braid enroll`
(e.g. a pool-wide rekey or a balance after enrollment), revisit this
exclusion under the same deciding question.
```

Notes on shape:

- Mirror the `### Excluded: braid lock` heading style and section
  level (`###`).
- Do NOT copy lock's four-bullet structure verbatim. Lock's `Shutdown-
  driven ExecStop`, `Manual stop and user-lock reentry`, and `ExecStop
  budget` bullets have no counterpart in standalone `braid enroll`
  (enroll has no systemd unit, no ExecStop seam, no
  `TimeoutStopSec` budget). The bullets above are the ones that
  actually apply.
- Include the closing "revisit if scope grows" sentence to match the
  lock exclusion's final paragraph -- this keeps the doc's
  living-document tone consistent.

## Regression test

The doc's central claim is that standalone `braid enroll` cannot
strand the user in recovery mode because it writes no pending-op
journal. The existing test
`cmd_enroll_blocked_in_recovery_mode`
(`cli/src/enroll_key_file.rs:2978`) only covers the inverse direction
(enroll *refuses* when a pending journal already exists). It does
not assert that enroll never *writes* one.

Add one new behavioral test alongside it (in the same `#[cfg(test)]`
module) that:

- builds a valid two-disk membership and an existing keyfile fixture
  (using the same `enroll_make_membership` / `enroll_make_existing_keyfile`
  helpers the surrounding tests use),
- configures `MockRunner` so that the apply phase fails partway
  through (e.g. the second `CryptsetupLuksAddKeyFile` returns a
  non-zero exit, or the header backup after the first successful
  enrollment fails),
- invokes `cmd_enroll_key_file` expecting an error,
- asserts the call did NOT create `paths.pending_op_json()` (use
  `assert!(!paths.pending_op_json().exists())` or the equivalent
  via the test's filesystem seam).

Intent / Why / Scenario preamble (per `docs/testing.md` test
conventions):

- **Intent:** standalone `braid enroll` never writes the pending-op
  journal, so an interrupted apply phase cannot strand the user in
  recovery mode.
- **Why:** this property is the justification recorded in
  `docs/decisions/019-inhibit-sleep.md`'s "Excluded: braid enroll"
  subsection for not acquiring a sleep inhibitor; a regression that
  silently introduces a journal write to enroll would invalidate that
  justification without failing any existing test.
- **Scenario:** an operator runs `braid enroll`, the second disk's
  cryptsetup invocation fails mid-loop, and `pending-op.json` must
  still not exist afterward so the operator is not pushed into
  recovery mode for a non-recovery error.

The test is structure-insensitive: it does not pin the internal
control flow, just the externally observable "no journal file on
disk" invariant. It is small and uses fixtures already shared with
the surrounding test module.

## Critical files

- `docs/decisions/019-inhibit-sleep.md` -- new `### Excluded: braid
  enroll` subsection.
- `cli/src/enroll_key_file.rs` -- one new test added to the existing
  `#[cfg(test)]` module, near
  `cmd_enroll_blocked_in_recovery_mode` (line 2984+).

Referenced from the new section (not modified):

- `cli/src/enroll_key_file.rs:211-219` -- `probe_keyfile_enrollment`
  short-circuit, basis of the recoverability bullet.
- `cli/src/enroll_key_file.rs:285-334` -- `apply_enrollment`'s per-disk
  mutation loop.
- `cli/src/enroll_key_file.rs:450-493` -- `EnrollPlan::execute`, the
  no-journal entry point.
- `cli/src/add.rs` (`cmd_add`) and `cli/src/replace.rs:636,706`
  (`luks::enroll_key_file` call sites) -- evidence for the
  "incidental under add/replace" sentence.
- `docs/principles.md:107` -- justification for treating
  `cryptsetup luksHeaderBackup` as fast bookkeeping.
- `docs/luks-unlock.md:118-134` -- header-backup workflow and the
  messaging invariant forbidding local `.luksheader` files as a
  user-visible recovery target. Grounds the corrected LUKS recovery
  bullet.
- `cli/src/luks.rs:547-557` --
  `backup_luks_header_post_mutation`'s doc comment and behavior
  (only runs after a successful mutation). Grounds the corrected
  LUKS recovery bullet.

## Out of scope

- No production code changes in `cli/src/enroll_key_file.rs` --
  the new regression test lives in the existing `#[cfg(test)]`
  module and does not modify `cmd_enroll_key_file`, `EnrollPlan::execute`,
  or `apply_enrollment`.
- No code-side doc comments in `cli/src/enroll_key_file.rs` pointing
  to 019. Lock's exclusion in 019 has no corresponding code comment
  in `cli/src/lock.rs`; that precedent is preserved.
- No sleep inhibitor added to enroll. The whole point of the
  exclusion section is that the deciding question answers "no
  inhibitor needed" for this command.
- Do not generalize the doc into a broad "excluded commands" parent
  section. `unlock` and `mount` are not mutating commands in the same
  sense (they open mappers / mount filesystems, both reversible
  without disk-level mutation); `doctor` and `status` are read-only.
  Enroll is the only remaining mutating command worth a worked
  exclusion alongside lock.
- No update to `docs/index.md` -- 019 is already listed there and the
  one-line summary is unaffected.

## Verification

- Read the resulting `docs/decisions/019-inhibit-sleep.md` end-to-end
  once edited. The new subsection should sit at section level `###`,
  immediately after the lock exclusion and before `## Consequences`,
  with no broken links or duplicated content.
- Sanity grep: `grep -n "enroll" docs/decisions/019-inhibit-sleep.md`
  should now return matches inside the new subsection.
- `git status` should show exactly two modified files:
  `docs/decisions/019-inhibit-sleep.md` and
  `cli/src/enroll_key_file.rs` (test-only addition).
- `just test-rust` must pass, including the new no-journal regression
  test. To prove the test is meaningful, the impl pass should also
  confirm it FAILS on a stub variant that mutates `enroll_key_file.rs`
  to write a pending-op journal before apply -- this is a one-line
  local sanity check during implementation, not a committed change.
- No VM tests or parser canaries are affected (no Nix-side changes,
  no tool-output parsers modified, no fixtures touched).
