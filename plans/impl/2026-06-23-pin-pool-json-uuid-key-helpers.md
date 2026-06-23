# Plan: pin post-replace pool.json UUID-key identity, and unify the key-blind test helpers

## Context

Decision 024 ([`024-luks-uuid-identity.md`](../../docs/design/decisions/024-luks-uuid-identity.md))
makes the LUKS UUID the single persistent disk identity: `pool.json` membership is a map
**keyed by canonical LUKS UUID**, with `name`/`by_id`/`devid` as value-side metadata. For
`replace`, that key is load-bearing -- `derive_replace_target_membership`
(`cli/src/replace.rs#derive_replace_target_membership`) drops the old UUID and inserts the new
member under `new_uuid`.

**The gap (verified):** no VM test checks that a completed `braid replace` writes a member whose
**object key** equals the new disk's real, live `cryptsetup luksUUID`. The three replace tests that
inspect membership (`replace-preserves-devid.py`, `replace-2disk-pool.py`, `replace-larger-disk.py`)
assert only via `member_names`/`member`, which read `entry["name"]` and **cannot see the key**. A
regression that committed the new member under the wrong key (old UUID, name, or unenriched/garbage
UUID) would leave the value-side `name` correct and pass every existing replace test. The Rust seam
cannot close this: `derive_replace_target_membership`'s unit tests use synthetic fixture UUIDs, so
they pin the drop-old/insert-new derivation but never prove `new_uuid` equals what `cryptsetup`
actually wrote to (or read from) the header. Only the VM lane observes that round-trip.

**Root cause, not just a missing line:** the reason this class of gap was easy to miss is that the
universal test vocabulary -- the inline `member_names` / `member` / `member_entry` / `member_uuid`
family, duplicated across **33 test files** -- is overwhelmingly name-centric (`member_names` and
`member` read `entry["name"]`), structurally projecting the UUID key away. The test suite is blind to
the exact axis Decision 024 makes load-bearing. Per AGENTS.md ("reach for the ideal, robust, simple, most
correct solution -- regardless of scope, refactor cost"), the fix dissolves that root cause, not just
the one assertion.

**Provenance confirmed (both replace modes), so the assertion holds end-to-end:**
- **FreshLuks** (blank/non-LUKS target): `new_uuid = LuksUuid::new_v4()` (`cli/src/replace.rs:1501`),
  journaled, written to the header via `cryptsetup luksFormat --uuid`, and used as the `pool.json`
  key. So `cryptsetup luksUUID <new device>` == key.
- **ExistingLuks** (pre-formatted target): `new_uuid` = the probed `cryptsetup luksUUID` of the
  existing header (`cli/src/probe.rs` -> `PresentConfigDiskState::PresentLuks { uuid }` ->
  `cli/src/replace.rs:1502`), used as the key. So `cryptsetup luksUUID <new device>` == key.

The two modes derive `new_uuid` differently (generated vs adopted), so covering both is meaningful,
not redundant.

## Decision

**Scope: unify** (user-chosen), structured as **two coherent commits** so the load-bearing fix lands
independently of the mechanical churn and `git bisect` stays meaningful.

- **Commit 1 (the fix):** Create the shared helper module `tests/cli/member_helpers.py` with the new
  key-aware assertion as its centerpiece, wire it into the 3 tests that gain coverage, and add the
  UUID-key assertions. Self-contained and verifiable by running 3 VM tests.
- **Commit 2 (the refactor):** Migrate the remaining 30 consumers to the shared module and delete
  their inline duplicate helpers. Pure dedup, no behavior change.

End state: one blessed source for membership helpers, including the key-aware assertion that pins
Decision 024 identity in the VM lane.

---

## Commit 1 -- `test(replace): pin post-replace pool.json key == live LUKS UUID`

### New file: `tests/cli/member_helpers.py`

Final content: 4 existing helpers consolidated from their inline copies, plus 1 genuinely new
(`assert_member_keyed_by_uuid`). `member_names`/`member`/`member_entry` are byte-compatible with the
duplicated copies. `member_uuid` already exists inline in 4 files (`braid-unlock.py`,
`enroll-uuid-mismatch.py`, `enroll-uuid-mismatch-midprompt.py`, `unlock-uuid-mismatch.py`) as a direct `for uuid, entry in
pool["disks"].items()` loop; the module version below is a behavior-identical refactor that delegates
to `member_entry` (same lookup, same returned key) -- so migrating those 4 files swaps their inline
loop for the delegating version with no behavior change. Reuse the established Nix-concat preamble from
[`inhibitor_helpers.py`](../../tests/cli/inhibitor_helpers.py) (the "NOT a Python module" note).
`member_entry`'s loop variable is renamed `entry` (was `member`) so it no longer shadows the
module-level `member()` now that both live in one module. The key variable is `luks_uuid`, not `uuid`,
so concatenated consumers that import the `uuid` module do not trip the NixOS test-script linter.

```python
# Shared helpers for VM tests that inspect braid's pool.json membership.
# (Same NOT-a-module / Nix-concat preamble as inhibitor_helpers.py.)

def member_names(pool):
    return {member["name"] for member in pool["disks"].values()}


def member(pool, name):
    for entry in pool["disks"].values():
        if entry["name"] == name:
            return entry
    raise AssertionError(f"{name} missing from pool.json: {pool}")


def member_entry(pool, name):
    # Decision 024: the dict KEY is the member's persistent LUKS UUID identity,
    # distinct from the value-side display name.
    for luks_uuid, entry in pool["disks"].items():
        if entry["name"] == name:
            return luks_uuid, entry
    raise AssertionError(f"{name} missing from pool.json: {pool}")


def member_uuid(pool, name):
    uuid, _ = member_entry(pool, name)
    return uuid


def assert_member_keyed_by_uuid(pool, name, expected_uuid):
    # Pin Decision 024 end-to-end: the member named `name` is stored under the
    # object key `expected_uuid` -- the disk's real, live cryptsetup LUKS UUID.
    key = member_uuid(pool, name)
    assert key == expected_uuid, (
        f"member '{name}' keyed by {key}, expected live LUKS UUID {expected_uuid}: {pool}"
    )
```

### Wire 3 tests to the module + add assertions

For each of the 3 tests below: in its paired `.nix`, prepend
`builtins.readFile ./member_helpers.py + "\n\n" +` to the `testScript` expression; in its `.py`,
delete the inline `member_names`/`member`/`member_entry` defs (now sourced from the module).

1. **`tests/cli/replace-preserves-devid.py`** -- FreshLuks (disk2 -> disk3, fresh format).
   - In the "Record disk2 devid" subtest, also capture the old UUID while disk2 is a live member:
     `disk2_uuid = machine.succeed("cryptsetup luksUUID /dev/disk/by-id/virtio-disk2").strip()`
   - In the closing "Pool membership updated" subtest, after the existing name asserts:
     ```python
     disk3_uuid = machine.succeed("cryptsetup luksUUID /dev/disk/by-id/virtio-disk3").strip()
     assert_member_keyed_by_uuid(pm, "disk3", disk3_uuid)
     assert disk2_uuid not in pm["disks"], f"old disk2 UUID key {disk2_uuid} still present: {pm}"
     ```
   - Catches: new member keyed by anything other than disk3's real generated-and-written UUID, and
     failure to drop the old key.

2. **`tests/cli/replace-new-already-luks.py`** -- ExistingLuks (disk2 -> pre-formatted disk4, adopt).
   - This test already captures `luks_uuid_after` (disk4's live UUID, already proven unchanged by the
     "LUKS UUID unchanged" subtest). In Phase 0 setup, also capture
     `disk2_uuid = machine.succeed("cryptsetup luksUUID /dev/disk/by-id/virtio-disk2").strip()`.
   - In the "Pool membership updated" subtest, after the existing name asserts:
     ```python
     assert_member_keyed_by_uuid(pm, "disk4", luks_uuid_after)
     assert disk2_uuid not in pm["disks"], f"old disk2 UUID key {disk2_uuid} still present: {pm}"
     ```
   - Catches the adopt-path regression: member keyed by a freshly-generated/garbage UUID instead of
     the adopted live-header UUID.

3. **`tests/cli/recover-replace-completed.py`** -- recover-rebuild-write (distinct `cli/src/recover.rs`
   path). Already captures `new_uuid` (disk4, line ~91) and `old_uuid` (disk2, line ~110); it
   currently discards the recovered key via `_, recovered_member = member_entry(...)`.
   - In the "braid recover rebuilds pool.json" subtest, after the existing by_id loop:
     ```python
     assert_member_keyed_by_uuid(recovered, "disk4", new_uuid)
     assert old_uuid not in recovered["disks"], f"old disk2 UUID key {old_uuid} still present: {recovered}"
     ```
   - Catches a recovery that rebuilds membership under the wrong key. This is a genuinely separate
     code path from `replace.rs` execute, adjacent to the finding's class -- cheap to pin since the
     UUIDs are already in hand.

Update each modified test's `# Intent` / `# Why it exists` preamble to note it now pins the UUID-key
identity (per the testing-doc preamble contract).

---

## Commit 2 -- `test(cli): dedup membership helpers into member_helpers.py`

Migrate the remaining **30 consumers** that still define the helpers inline. Mechanical, no behavior
change.

For each consumer:
- `.py`: delete every inline helper def block it carries -- any of `member_names` / `member` /
  `member_entry` / `member_uuid` (the four files with an inline `member_uuid` are
  `braid-unlock.py`, `enroll-uuid-mismatch.py`, `enroll-uuid-mismatch-midprompt.py`,
  `unlock-uuid-mismatch.py`). Leave `members_except`
  alone -- it is NOT part of the shared module (see the verification grep note).
- `.nix`: prepend `builtins.readFile ./member_helpers.py + "\n\n" +` to the `testScript` expression.

Consumer set: 33 files define at least one of the four migrated helpers, minus the 3 done in Commit 1
= **30**. The authoritative full list is the single enumeration grep (the old two-grep form,
`def member_names` + `def member_entry`, undercounts because it misses `enroll-uuid-mismatch.py`
and `enroll-uuid-mismatch-midprompt.py`, which define only `member_uuid`):
```
rg -l '^def (member_names|member|member_entry|member_uuid)\(' tests/cli/*.py
```
Grouped for orientation:
- **`member_names`/`member` group** (replace/add/misc), e.g. `replace-2disk-pool.py`,
  `replace-larger-disk.py`, `replace-live-disk.py`, `replace-dead-disk.py`, `replace-sequential.py`,
  `braid-add-persists-before-balance.py`, `braid-remove-disk.py`, `braid-unlock.py`,
  `config-name-immutability.py`, `unlock-uuid-mismatch.py`, ...
- **`member_entry` group** (recover), e.g. `recover-replace-not-started.py`,
  `recover-replace-existing-luks-enroll.py`, `recover-replace-existing-luks-uuid-mismatch.py`,
  `recover-remove-missing-completed.py`, `recover-add-mixed-batch.py`, `braid-recover-remove.py`.
- **`member_uuid`-only files**: `enroll-uuid-mismatch.py` and
  `enroll-uuid-mismatch-midprompt.py` -- the consumers the old two-grep enumeration missed; they
  define `member_uuid` inline but neither `member_names` nor `member_entry`.

**One caveat:** `tests/cli/braid-unlock.nix` already chains a helper via `readFile`. Add
`member_helpers.py` to its existing concat chain (prepend), do not replace the chain. Every other
consumer's `.nix` is a single `testScript = builtins.readFile ./X.py;` (confirmed) -- straight
prepend.

**Note on namespace harmlessness:** after migration, every consumer gets all five helpers in scope
even if it only uses one. That matches the existing `inhibitor_helpers.py` precedent (a consumer pulls
the whole helper file) and is harmless.

---

## Files

**Commit 1:**
- New: `tests/cli/member_helpers.py`
- Edit (`.py` + paired `.nix`): `replace-preserves-devid`, `replace-new-already-luks`,
  `recover-replace-completed`

**Commit 2:**
- Edit (`.py` + paired `.nix`): the 30 remaining consumers enumerated above (including
  `enroll-uuid-mismatch` and `enroll-uuid-mismatch-midprompt`). `braid-unlock.nix` is the lone
  prepend-to-existing-chain case.

No Rust changes. No doc/ADR changes required: Decision 024's "Tests That Enforce This" inventory may
optionally gain a bullet noting the replace VM tests now pin the post-replace UUID-key round-trip, but
that is a docs nicety, not load-bearing.

## Verification

**Commit 1 (the fix) -- run the 3 modified VM tests:**
```
just test-vm replace-preserves-devid replace-new-already-luks recover-replace-completed
```
All three must pass. To prove the assertions are real (not vacuous), temporarily perturb
`derive_replace_target_membership` to insert under `old_uuid` (or a fresh `LuksUuid::new_v4()`) and
confirm `replace-preserves-devid` / `replace-new-already-luks` FAIL on the new key assertion (revert
after).

**Commit 2 (the refactor) -- cheap structural checks, no full VM suite needed.** Two distinct
failure modes, each with its own guard:

1. **Forced-instantiation gate (catches a bad `readFile` path).** A bad
   `builtins.readFile ./member_helpers.py` only errors when a check's `testScript` is *forced* --
   `builtins.attrNames` lists names without forcing check values, so it would miss it (and
   `--apply 'cs: builtins.attrNames cs' --raw` is itself invalid: `--raw` prints a string, not a
   list). Instead, instantiate every check (eval-only, no build) so each `testScript` readFile is
   forced. This repo's interactive shell is zsh, but `mapfile` is a Bash builtin -- run the gate
   under an explicit fail-fast Bash heredoc so it is copy-paste safe regardless of the login shell:
   ```bash
   bash <<'EOF'
   set -euo pipefail
   sys=$(nix eval --impure --expr builtins.currentSystem --raw)
   mapfile -t attrs < <(nix eval ".#checks.$sys" \
     --apply 'cs: builtins.concatStringsSep "\n" (builtins.attrNames cs)' --raw)
   nix build --dry-run --no-link "${attrs[@]/#/.#checks.$sys.}"
   EOF
   ```
   (Per-test form, shell-agnostic: `nix build --dry-run --no-link .#checks.$(nix eval --impure --expr
   builtins.currentSystem --raw).<migrated-test>`. The enumeration mirrors the `concatStringsSep`
   apply the justfile's `_build-checks` already uses; the full sweep instantiates all ~180 check
   attrs.)

2. **Wiring grep invariants (catch a forgotten `readFile` line).** The gate above cannot catch a
   `.nix` that *dropped* the `member_helpers.py` line while its `.py` deleted the inline defs -- that
   instantiates fine and only `NameError`s at runtime. Two greps guard it:
   - No consumer defines the migrated helpers anymore -- this grep returns only `member_helpers.py`:
     ```
     rg -l '^def (member_names|member|member_entry|member_uuid|assert_member_keyed_by_uuid)\(' tests/cli/
     ```
     Use this exact alternation, NOT a loose `^def member` prefix: the prefix form false-positives
     `def members_except(`, a separate inline helper that 6 recovery tests legitimately keep (it is
     not part of `member_helpers.py`), so it would flag those files forever. The trailing `\(` pins
     each name to a real call-shape def and keeps `member` from swallowing `member_entry`/`member_uuid`.
   - Every `.py` that *calls* a helper has the wiring in its paired `.nix` (empty output == all wired):
     ```
     for py in $(rg -l '\b(member_names|member|member_entry|member_uuid|assert_member_keyed_by_uuid)\(' \
         tests/cli/*.py | grep -v member_helpers.py); do
       nix="${py%.py}.nix"
       rg -q member_helpers "$nix" || echo "MISSING WIRING: $nix"
     done
     ```

3. **Spot-run** a representative migrated few -- the ultimate runtime check that the namespace resolves
   end-to-end: `just test-vm replace-2disk-pool braid-add-persists-before-balance recover-replace-not-started braid-unlock`

## Out of scope / notes

- No change to `replace.rs` / `recover.rs` logic -- the code is correct; this closes a test-coverage
  gap and removes the duplication that hid it.
- `balance_helpers.py` (a pre-existing unused helper found in `tests/module/`) is unrelated; leave it.
- If preferred, Commit 2 can be deferred to a separate follow-up PR -- Commit 1 is fully self-contained
  and the suite stays consistent (member_helpers.py simply has 3 consumers until the migration lands).
