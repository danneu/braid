# Plan: Enforce LUKS UUID match in `braid enroll`

## Context

`braid enroll` mutates LUKS slot 1 on every membership member without ever
comparing the live LUKS UUID at the by-id path to the membership UUID key.
This is the failure mode decision-024 explicitly enumerates: "UUID
mismatches catch disks that were swapped, cloned, or reformatted after
the original plan was made." Sibling mutating commands enforce this guard
(`mount.rs:274`, `replace.rs:79-86`, multiple `recover.rs` sites). Enroll
does not.

The single-passphrase principle (Principle 4) means
`verify_credential_for_targets` cannot stand in for the UUID check: a
swapped-in disk that was previously another braid pool's member will
accept the same passphrase, slot 1 is likely empty on it, and braid will
write the operator's keyfile into slot 1 of a foreign LUKS container
while the intended member's slot 1 stays empty. The user thinks
auto-unlock is set up; at 3 AM the real members reject the keyfile and
auto-unlock fails. Re-runs see `AlreadyEnrolled` on the foreign disk and
mask the problem.

Scope is isolated to standalone `braid enroll`: `add --enroll DIR` and
`replace --enroll DIR` only enroll on the disk they mutate, which is
covered by `classify_braid_disk_fsid` (add) and
`probe_existing_luks_new_target_uuid` /
`NewTargetUuidMismatchAtOpen` (replace).

## Fix

Add the UUID comparison inside the existing per-member loop in
`discover_enrollment_candidates`
(`cli/src/enroll_key_file.rs:77-142`). The probe already returns the
live UUID inside `ConfigDiskState::PresentLuks { uuid, .. }`; today the
binding `for (_, member) in membership.iter()` drops the membership UUID
key, and the match arm drops the live UUID via `..`. Capture both and
compare; on mismatch return
`EnrollKeyFileError::Validation` with the same wording shape as
`mount.rs:275-281` so operators see one consistent message across the
mount/enroll surfaces.

Fail fast on first mismatch (mirrors `mount.rs:274`). Discovery is the
right home for the check because it runs in both the dry-run path
(`plan_enroll` -> discovery -> compile_enroll_steps) and the real-run
path (`plan_enroll` -> discovery -> `EnrollPlan::execute` ->
`plan_enrollment` -> `apply_enrollment`), so dry-run preview also flags
swapped disks before any operator commits.

### Code change (sketch)

In `discover_enrollment_candidates`:

```rust
for (expected_uuid, member) in membership.iter() {
    let name = member.name.as_str();
    let probed = match probe::probe_config_disk(runner, fs, &member.name, &member.by_id) {
        Ok(p) => p,
        Err(e) => return (notes, Err(e.into())),
    };
    match &probed.state {
        ConfigDiskState::Absent => { /* unchanged */ }
        ConfigDiskState::PresentNotLuks => { /* unchanged */ }
        ConfigDiskState::PresentLuks { uuid, .. } => {
            if uuid != expected_uuid {
                return (
                    notes,
                    Err(EnrollKeyFileError::Validation(format!(
                        "disk '{}' LUKS UUID mismatch at {}:\n  \
                         expected  {}\n  \
                         found     {}",
                        name, member.by_id, expected_uuid, uuid
                    ))),
                );
            }
            candidates.push((name.to_owned(), member.by_id.clone()));
        }
    }
}
```

The candidate tuple shape (`(String, ByIdPath)`) is unchanged: once the
UUID is verified at the discovery boundary, downstream code can continue
operating on `(name, by_id)`. No threading required.

## Files to modify

- `cli/src/enroll_key_file.rs` — fix in `discover_enrollment_candidates`
  (lines 77-142). New unit test alongside the existing discovery tests.
- `cli/src/test_fixtures/enroll_key_file.rs` — extend
  `enroll_luks_uuid_ok` to take a `uuid: &str` argument, matching
  `mount::luks_uuid_ok(device, uuid)`'s signature. Update its doc
  comment to drop the "enroll tests never assert on the UUID value"
  claim, since after this fix the UUID is load-bearing.
- All existing call sites of `enroll_luks_uuid_ok` in
  `cli/src/enroll_key_file.rs` (~9 sites) and any other test files —
  thread the matching membership UUID (`shared::test_uuid(500 + idx)`)
  through.
- `tests/cli/enroll-uuid-mismatch.py` (new) — NixOS VM test.
- `tests/cli/enroll-uuid-mismatch.nix` (new) — VM test wrapper, mirror
  shape of `tests/cli/unlock-uuid-mismatch.nix`.
- `flake.nix` — register the new VM test (search for
  `unlock-uuid-mismatch` and add a sibling entry).

## Tests

### Rust unit test

Add to the existing `tests` module in `cli/src/enroll_key_file.rs`,
near the other discovery tests. Mirror the structure of
`mount_luks_uuid_mismatch_closed` (`cli/src/mount.rs:2118-2168`):

- Build a 2-disk membership with `enroll_make_membership` (which
  assigns UUIDs `test_uuid(500)` and `test_uuid(501)`).
- Wire `enroll_luks_uuid_ok` so disk1's probe returns
  `ffffffff-ffff-ffff-ffff-ffffffffffff` (divergent) while disk2's
  probe returns its correct fixture UUID. Use the new
  uuid-parameterized fixture.
- Invoke `discover_enrollment_candidates` and assert it returns
  `Err(EnrollKeyFileError::Validation(msg))` where `msg` contains
  "disk1", "LUKS UUID mismatch", the expected UUID string, and the
  observed UUID string.
- Crucially: assert this fires BEFORE any slot-1 check or mutation by
  asserting against the mock-runner request log:
  - The probe gateway `CmdRequest::CryptsetupLuksDumpText { device }`
    for the mismatched disk's by-id IS recorded (probe.rs:160-162
    requires it to return `PresentLuks` at all).
  - No slot-inventory `CmdRequest::CryptsetupLuksDump { device }`
    (cmd.rs:176, the JSON variant `check_key_slot` uses) is recorded
    for any disk.
  - No `CmdRequest::CryptsetupLuksAddKeyFile { .. }` mutation is
    recorded for any disk.
  This distinguishes "fired at the gateway before slot inventory"
  from "fired after slot inventory" and avoids pressuring the
  implementation to skip the LUKS2 gateway check.

Preamble for the test:

```
// Intent: discovery rejects a member whose live LUKS UUID at the
//   by-id path no longer matches the membership UUID key, before
//   any slot mutation or slot inventory probe runs.
// Why it exists: decision-024 mandates UUID re-checks at every
//   mutation boundary; mount/replace/recover enforce this and enroll
//   must too. Without it, a swapped or reformatted disk silently
//   takes the operator's keyfile into slot 1 of a foreign LUKS
//   container while the intended member's slot 1 stays empty,
//   defeating auto-unlock at boot.
// Scenario: operator's by-id stable path now points at a different
//   LUKS volume than the one captured in pool.json (swap, reformat,
//   or cloned disk). braid enroll fails before mutation, with the
//   same wording shape as braid unlock.
```

### NixOS VM test (`tests/cli/enroll-uuid-mismatch.py`)

Mirror the shape of `tests/cli/unlock-uuid-mismatch.py` and follow the
`braid enroll --generate` idiom established by
`tests/cli/braid-enroll-generate.py:74-83`. The enroll CLI takes a
positional directory (`braid enroll DIR ...`) that must be a mount
point when `--generate` is used (main.rs:253-268,
enroll_key_file.rs:517-553); there is no `--key-file` flag on the
enroll subcommand.

Setup (shared between dry-run and real-run phases):

1. Bring up a 2-disk pool (`braid add`), close it, re-unlock so
   `pool.json` is enriched with stable UUID keys.
2. Lock the pool.
3. Reformat disk2 with a fresh LUKS header (e.g.
   `cryptsetup luksFormat`), generating a new UUID.
4. Record disk2's old (membership) UUID and the new (live) UUID for
   the assertions.
5. Mount a tmpfs at the enroll target directory:
   ```
   mkdir -p /tmp/usb
   mount -t tmpfs -o size=1m,mode=700 tmpfs /tmp/usb
   mountpoint -q /tmp/usb
   ```

Phase 1 -- dry-run (must run BEFORE real-run, so a regression that
unsafely renders a preview is caught even if real-run rejection still
works). Run:
```
printf '%s\n' "$PASSPHRASE" | braid enroll /tmp/usb --generate --dry-run --passphrase-stdin
```
Assert:
- exit code != 0
- combined output contains `"LUKS UUID mismatch"`, `"disk2"`, and
  both UUIDs (old membership UUID and new live UUID)
- stdout contains NO `enroll keyfile` step (mismatch fires inside
  `discover_enrollment_candidates`, which is upstream of
  `compile_enroll_steps`, so the dry-run preview must not advertise
  enrollment)
- `/tmp/usb/braid.key` does NOT exist (`--dry-run` must not create
  the keyfile; this also defends against any leak from the generate
  side effect)
- `cryptsetup luksDump <disk1-byid>` shows slot 1 still empty
- `cryptsetup luksDump <disk2-byid>` shows slot 1 still empty

Phase 2 -- real-run. Run:
```
printf '%s\n' "$PASSPHRASE" | braid enroll /tmp/usb --generate --passphrase-stdin
```
Assert:
- exit code != 0
- combined output contains `"LUKS UUID mismatch"`, `"disk2"`, and
  both UUIDs
- `cryptsetup luksDump <disk1-byid>` shows slot 1 is empty (the
  discovery error fires before any disk is mutated)
- `cryptsetup luksDump <disk2-byid>` shows slot 1 is empty
- `/tmp/usb/braid.key` does NOT exist (generate is gated behind
  planning, so the file must not be created)

This separation pins the dry-run planner contract from
decision-022:30 -- planning is the boundary that decides what would
happen, and a dry-run preview must not promise behavior the real run
would refuse.

Preamble:

```
# Intent: braid enroll refuses with non-zero exit when any membership
#   disk's live LUKS UUID at its by-id path no longer matches the UUID
#   captured in pool.json, before slot 1 is mutated on any disk.
# Why it exists: decision-024 mandates UUID re-checks at every mutation
#   boundary. mount/replace/recover already enforce this; enroll did
#   not, so a swapped or reformatted disk would silently take the
#   operator's keyfile into slot 1 of a foreign LUKS container while
#   the real member's slot 1 stays empty -- breaking auto-unlock.
# Scenario: operator sets up a 2-disk braid pool, locks it, then
#   reformats one member out-of-band (or hot-swaps a foreign LUKS disk
#   onto the same by-id slot). The next `braid enroll --generate` must
#   abort before any slot 1 mutation and surface the same wording shape
#   as `braid unlock` does for the same scenario.
```

### Flake registration

Add to `flake.nix` alongside the `unlock-uuid-mismatch` entry. Confirm
the test runs under `just test-vm enroll-uuid-mismatch`.

## Verification

1. `just test-rust` — new unit test passes; existing enroll unit tests
   still pass after fixture/call-site refactor.
2. `just test-vm enroll-uuid-mismatch` — new VM test passes.
3. `just test-vm unlock-uuid-mismatch` — regression check on sibling.
4. `just test-vm` — full suite passes.
5. The new VM test's Phase 1 (dry-run) and Phase 2 (real-run) both
   exercise the shared `discover_enrollment_candidates` boundary, so
   no separate manual sanity check is needed -- the automated
   coverage pins the dry-run/real-run parity.

## Out of scope

- Refactoring UUID-check logic into a shared helper across
  mount/replace/enroll/recover. Each caller has a different error type
  and different per-state handling around the check; extracting would
  add indirection without collapsing meaningful duplication.
- Adding UUID checks to `add --enroll DIR` or `replace --enroll DIR`:
  both paths only enroll on the mutating target, which already has
  separate UUID verification.
- Reworking the credential-verify layer to know about UUIDs. The UUID
  check is conceptually a preflight invariant on the disk-by-by-id
  identity, not a credential property; placing it in discovery keeps
  the responsibilities separate.
