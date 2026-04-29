# Plan: per-disk passphrase verification in `plan_enrollment`

## Context

The two-phase enroll refactor split planning (read-only preflight) from
applying (mutations) so that any preflight failure aborts before any disk is
mutated. That guarantee holds for the slot-1 conflict path because
`check_key_slot` runs per disk inside `plan_enrollment`. It does **not** hold
for a divergent-passphrase scenario.

`plan_enrollment` (`cli/src/enroll_key_file.rs:168`) currently calls
`verify_first_candidate_passphrase` exactly once, against `candidates[0]`
(`cli/src/enroll_key_file.rs:119-137`). braid does not produce divergent
passphrases on its own, but a user can break the single-passphrase invariant
out-of-band (`cryptsetup luksChangeKey` on disk2). When that happens:

1. Planner verifies passphrase on disk1 -- ok.
2. Planner runs slot-1 checks per disk -- ok.
3. Apply enrolls disk1 successfully (`luks::enroll_key_file`,
   `cli/src/luks.rs:638`).
4. Apply tries disk2; `cryptsetup luksAddKey` fails with "No key available
   with this passphrase".
5. Error propagates. disk1 is mutated. disk2 is not. Exactly the partial-
   mutation state the refactor exists to prevent.

In-tree VM regression coverage (`tests/cli/braid-enroll.py` Test 5,
lines 352-389) covers slot-1 conflict only. There is no divergent-passphrase
test.

**Outcome**: every disk that will be mutated has its passphrase verified
during planning. No partial-mutation window. The pre-loop first-candidate
verify is preserved so the existing wrong-passphrase semantics (the case
where every disk is `AlreadyEnrolled` -- the only path that does **not**
fall through to `NeedsEnroll`) keep working.

## Fix

### 1. Keep the pre-loop verify; add a per-disk verify on the `NeedsEnroll` path

**File:** `cli/src/enroll_key_file.rs`

**Preserve** `verify_first_candidate_passphrase` and its call at
`plan_enrollment` line 175. Rationale: when every candidate is
`AlreadyEnrolled` (for example, every disk in Test 4b's pool because Test 1
enrolled them), the keyfile probe authenticates, the loop body `continue`s,
and *no per-disk passphrase verify ever runs*. Removing the pre-loop check
would silently accept a wrong passphrase in that case and break Test 4b
(`tests/cli/braid-enroll.py:184`), which asserts `braid enroll` fails with
`"wrong passphrase"` on a fully-enrolled pool. The pre-loop check is the
only protection on the all-`AlreadyEnrolled` path -- keep it.

**Add** a per-disk passphrase verify inside the existing per-disk loop
(`cli/src/enroll_key_file.rs:178-208`), positioned **after** the keyfile
probe (so `AlreadyEnrolled` disks `continue` past it) and **before**
`check_slot_one_available` (so a wrong passphrase fails before slot-state
inspection -- preserves the pre-existing "passphrase before slot" error
precedence). Skip the per-disk verify on the *first* candidate to avoid a
duplicate cryptsetup call and a duplicate `[wait] passphrase: checking
against disk1...` line on stderr -- the pre-loop check already covered it.

Loop body shape (sketch):
```
for (i, (name, by_id)) in candidates.iter().enumerate() {
    if ExistingKeyfile mode {
        // existing keyfile probe -- on Authenticated, push AlreadyEnrolled and continue
    }
    // NEW: per-disk passphrase verify, only for to-be-enrolled disks
    // beyond the first (the first is covered by the pre-loop check).
    if i > 0 {
        emit_credential_wait_line(Passphrase, color, name);
        match luks::verify_passphrase(runner, &by_id.0, passphrase)? {
            Authenticated => {}
            Rejected => return Err(Validation(format!("wrong passphrase on {name}"))),
        }
    }
    check_slot_one_available(runner, name, by_id)?;
    // existing NeedsEnroll push
}
```

`emit_credential_wait_line` lives at `cli/src/status_tag.rs:74`, and the
existing per-disk keyfile-probe wait line at `enroll_key_file.rs:187`
demonstrates the pattern for in-loop emission.

**Error wording**: `"wrong passphrase on {name}"` -- `EnrollKeyFileError::
Validation` variant (matches the pre-loop check's error class so existing
caller-side error rendering does not change).

No deletion of `verify_first_candidate_passphrase`. No change to
`apply_enrollment`. No change to dry-run step compilation
(`compile_enroll_steps`).

### 2. Rust unit tests: two targeted divergent-passphrase tests

**File:** `cli/src/enroll_key_file.rs` (existing `#[cfg(test)] mod tests`).

Add **two** tests, modeled on `plan_slot1_conflict_errors`
(`enroll_key_file.rs:1200-1241`). Both use a 2-disk membership (`disk1`,
`disk2`).

`MockRunner` is **lookup-based**: orphaned `with_output` entries are silent,
and an absent mock surfaces as `MissingMock` only when actually queried (per
`feedback_mockrunner_absent_mocks.md`). The targeted test design exploits
this -- mock **only** the path up to and including disk2's passphrase
rejection, and **omit** every mock that the planner would only reach if the
fix regressed (e.g., to "verify against disk1 only"). A regressed planner
would then advance into an unmocked call site and fail with `MissingMock`,
not pass silently.

**Test A: `plan_divergent_passphrase_existing_keyfile_errors_on_disk2`**

Mode: `ExistingKeyfile`. Mocks:
- `CryptsetupTestPassphrase` disk1 -> exit 0 (Authenticated) -- the pre-loop
  first-candidate verify
- `CryptsetupTestKeyFile` disk1 -> exit 2 (Rejected) -- the loop's keyfile
  probe on disk1
- `CryptsetupLuksDump` disk1 -> slot 1 empty -- disk1's slot-1 check
- `CryptsetupTestKeyFile` disk2 -> exit 2 (Rejected) -- the loop's keyfile
  probe on disk2
- `CryptsetupTestPassphrase` disk2 -> exit 2 (Rejected) -- the new per-disk
  verify on disk2

Assert `Err(Validation)` whose message contains both `"disk2"` and
`"wrong passphrase"`. Do **not** add a `CryptsetupLuksDump` mock for disk2
-- if the planner reaches disk2's slot-1 check, the fix regressed and the
test should fail loudly with `MissingMock`.

**Test B: `plan_divergent_passphrase_generate_new_errors_on_disk2`**

Mode: `GenerateNew`. Mocks:
- `CryptsetupTestPassphrase` disk1 -> exit 0 (Authenticated)
- `CryptsetupLuksDump` disk1 -> slot 1 empty
- `CryptsetupTestPassphrase` disk2 -> exit 2 (Rejected)

No keyfile-probe mocks for either disk (`GenerateNew` skips the probe per
`enroll_key_file.rs:179`). No `CryptsetupLuksDump` mock for disk2. Assert
the same `Err(Validation)` shape as Test A.

**Existing tests to inspect (not extend for regression-pinning).** Per the
MockRunner rule, adding extra `with_output` entries to existing success
tests does **not** pin the regression -- those orphaned entries would be
silent if the planner stopped calling them. The two targeted tests above
are the regression contract.

Independently of regression coverage, every existing planner test that
exercises a `NeedsEnroll` candidate beyond `candidates[0]` must gain a
`CryptsetupTestPassphrase` mock for that candidate, or `just test-rust`
will fail with `MissingMock` once the per-disk verify lands. Find the
full set with:
```
rg -nB2 -A40 'fn plan_[a-z0-9_]+\b' cli/src/enroll_key_file.rs
```
and update every test whose membership has two or more disks **and**
whose flow reaches at least one `NeedsEnroll` (not all-`AlreadyEnrolled`).
This includes -- but is not limited to -- the `_slot1_conflict_*`
tests, since they reach disk2's slot check after passing the per-disk
passphrase verify. This is mechanical compilation-of-the-fix work, not
regression coverage. Treat the grep as the authoritative list, not any
named subset in this plan.

### 3. VM regression test: divergent passphrase = no mutation on disk1

**File:** `tests/cli/braid-enroll.py`

Add Test 5b after Test 5 (after line 389), before `machine.shutdown()`. The
test must carry the same `# Intent: / # Why it exists: / # Scenario:`
Python-comment header that the existing tests in the file use (the
`AGENTS.md` test-conventions block comment is conceptual; the file's
literal style is Python `#` lines -- match that).

Setup pattern:
1. `close_all()`
2. Slot-1 cleanup must be idempotent: Test 5 already guarantees disk1's
   slot 1 is empty when entering 5b, so an unconditional `luksKillSlot` on
   disk1 would error (`cryptsetup` rejects killing an inactive slot).
   Disk2's slot 1 is occupied by the random key from Test 5 and must be
   killed. Use a single idempotent form for both. Note: only the parts
   with `{...}` placeholders are f-strings -- per `docs/testing.md:62`,
   placeholder-free f-strings fail the lint:
   ```python
   for dev in ["virtio-disk1", "virtio-disk2"]:
       machine.execute(
           f"cryptsetup luksKillSlot --batch-mode /dev/disk/by-id/{dev} 1 "
           "2>/dev/null || true"
       )
   ```
   Then explicitly assert the intended starting state -- both disks'
   slot 1 empty, disk2's slot 0 still holding the original passphrase --
   before proceeding.
3. Diverge disk2's passphrase. Use `--key-slot 0` explicitly so cryptsetup
   replaces the original passphrase in slot 0 rather than allocating a free
   slot (which on this VM is slot 1 and would silently make this a slot-
   conflict test instead of a divergent-passphrase test). Same f-string
   rule -- continuation lines without placeholders are plain strings:
   ```python
   new_pass = "differentpassphrase"
   machine.succeed(
       f"printf '%s\\n%s\\n' {shlex.quote(passphrase)} {shlex.quote(new_pass)} "
       "| cryptsetup luksChangeKey --key-slot 0 --batch-mode "
       "/dev/disk/by-id/virtio-disk2"
   )
   ```
   Verify the divergence is real before running braid: assert disk1 still
   accepts the original passphrase (`machine.succeed` on `printf '%s\n'
   ... | cryptsetup open --test-passphrase ... virtio-disk1`) and disk2
   rejects it (`machine.fail` on the same against virtio-disk2). This
   guards against `luksChangeKey` having silently no-opped or having
   operated on a different slot.

Run + assert (capture combined stdout+stderr through `machine.execute`,
which returns `(status, output)` and does **not** abort on nonzero exit
the way `machine.succeed` does):
```python
pq = shlex.quote(passphrase)
status, output = machine.execute(
    f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin 2>&1"
)
assert status != 0, f"expected nonzero exit on divergent passphrase; got status={status}, output={output!r}"
assert "wrong passphrase" in output, f"expected wrong-passphrase error in output, got: {output!r}"
assert "disk2" in output, f"expected disk2 to be named in error, got: {output!r}"
machine.fail(
    "cryptsetup open --test-passphrase --key-file /tmp/braid.key "
    "/dev/disk/by-id/virtio-disk1"
)
```
The final `machine.fail` mirrors Test 5's lines 387-389 -- disk1's slot 1
is still empty -- proving no partial mutation occurred.

Revert at the end of the subtest so the per-VM-test state is consistent if
any future test is appended after 5b. Same f-string rule:
```python
machine.succeed(
    f"printf '%s\\n%s\\n' {shlex.quote(new_pass)} {pq} "
    "| cryptsetup luksChangeKey --key-slot 0 --batch-mode "
    "/dev/disk/by-id/virtio-disk2"
)
```

## Critical files

- `cli/src/enroll_key_file.rs` -- add per-disk verify in
  `plan_enrollment`'s loop (after keyfile probe, before slot-1 check, skip
  on `i == 0`); add Tests A and B; update existing planner unit tests'
  mock surfaces to mock `CryptsetupTestPassphrase` on every `NeedsEnroll`
  candidate beyond the first.
- `tests/cli/braid-enroll.py` -- add Test 5b with `--key-slot 0` divergent-
  passphrase setup and revert.

## Verification

1. `just test-rust` -- new Tests A and B pass; the existing planner unit
   tests pass after their mock surfaces are updated for the per-disk
   verify on the second candidate. The targeted tests pin the regression:
   any implementation that skips disk2's passphrase verify will fail with
   `MissingMock` on disk2's slot-1 dump (Test A) or disk2's
   `LuksDump`/keyfile-probe (Test B) rather than passing silently.
2. `just test-vm braid-enroll` -- Tests 1-5 pass unchanged. Test 4b
   (`tests/cli/braid-enroll.py:184`) keeps passing because the pre-loop
   first-candidate verify is preserved -- on a fully-enrolled pool, the
   loop short-circuits via `AlreadyEnrolled` and the only passphrase check
   is the pre-loop one against disk1, which rejects the wrong passphrase
   exactly as today. Test 5b passes.
3. `just test-vm braid-enroll-generate` -- regression check on
   `GenerateNew` mode under the new per-disk verify path. Both
   `braid-enroll.py` and `braid-enroll-generate.py` need a substring sweep
   for `"[wait] passphrase: checking against"` -- the loop now emits one
   such line per `NeedsEnroll` candidate beyond the first, in addition to
   the pre-loop emit on disk1. If any test pins the *count* of those
   lines (rather than just a substring presence), update the assertion to
   match the per-disk fan-out. The grep step is a non-skippable part of
   the implementation work, not a verification afterthought.
