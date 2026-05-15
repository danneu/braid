# Plan: Add canonical remediation hint to unlock/enroll LUKS UUID mismatch errors

## Context

A review noted that `cli/src/mount.rs:278-285` emits a LUKS UUID mismatch
error with no remediation hint -- the operator gets the discrepancy but
no follow-up sentence. Decision 024 (`docs/decisions/024-luks-uuid-identity.md`)
frames UUID mismatch as the highest-blast-radius identity guard, so the
diagnostic density should match the structured `DegradedRefused` path
which closes with `hint: braid unlock --allow-degraded` and a doctor footer.

Two facts shape the right fix:

1. **Sibling instance.** `cli/src/enroll_key_file.rs:121-125` emits the
   identical hint-less format string (`disk '{}' LUKS UUID mismatch at
   {}:\n  expected ...\n  found ...`). Fixing only mount.rs leaves the
   same diagnostic gap one command over.

2. **Canonical wording already exists.** `cli/src/doctor.rs:354-359`
   already emits the operator-facing UUID-mismatch hint for the same
   condition: `disk was swapped, cloned, or reformatted; detach the
   foreign disk and reattach the original, or run 'braid replace' if
   the swap was intentional`. This wording is anchored by
   `doctor.rs:1804` (unit test asserts `msg.contains("detach the
   foreign disk")`) and by `tests/cli/braid-doctor-uuid-swap.py:84`.
   The review's proposed hint mentioned `braid discover --write`, but
   `cli/src/discover.rs:155-171` proves that route fails closed when
   pool.json already exists -- so adopting the doctor.rs phrasing is
   both right and already-tested.

Outcome: lift doctor.rs's hint string into a shared
`luks::luks_uuid_mismatch_guidance()` helper alongside the existing
`luks_header_unreadable_guidance()` and `luks_header_damaged_guidance()`,
then append it as a `hint:` footer at both mount.rs and
enroll_key_file.rs. Out of scope: `recover.rs`'s six UUID-mismatch sites
(distinct recovery context, already reference `manual/guides/recovery-scenarios.md`),
and the `add.rs`/`replace.rs` planning-time variants (already carry
`-- detach the foreign disk and retry`).

## Files to modify

- `cli/src/luks.rs` -- add `pub(crate) fn luks_uuid_mismatch_guidance() -> &'static str` next to the existing `luks_header_*_guidance` helpers; add a substring-anchored unit test mirroring the `luks_header_damaged_guidance` test pattern (`luks.rs:3521-3541`).
- `cli/src/doctor.rs:354-359` -- replace the inline hint string with `luks::luks_uuid_mismatch_guidance()`. Existing test at `doctor.rs:1775-1807` continues to pass because it asserts on substrings the helper still produces.
- `cli/src/mount.rs:278-285` -- after the `expected/found` block, append `\nhint: {luks::luks_uuid_mismatch_guidance()}` to the `MountError::Failed` payload.
- `cli/src/enroll_key_file.rs:121-125` -- same footer append on the `EnrollKeyFileError::Validation` payload.

## Helper shape

Mirror `luks_header_unreadable_guidance()` (`cli/src/luks.rs`, current
device-agnostic variant returning `&'static str`):

```rust
/// Canonical operator-facing remediation hint for a LUKS UUID mismatch
/// (live disk's UUID does not match pool.json membership). Used by
/// `unlock`, `enroll` standalone, and `doctor` so the three surfaces
/// give the operator one consistent next step. The wording is
/// intentional: "detach + reattach original" is the safe default;
/// "braid replace" is the destructive alternative for intentional swap.
pub(crate) fn luks_uuid_mismatch_guidance() -> &'static str {
    "disk was swapped, cloned, or reformatted; detach the foreign \
     disk and reattach the original, or run 'braid replace' if the \
     swap was intentional"
}
```

Doctor folds it inline after `--`; mount/enroll prefix with `hint: ` on
a new line so the multi-line `expected/found` block stays readable.
The "hint: " prefix matches the existing `format_degraded_refused`
convention (`mount.rs:88`).

## Caller wording

**mount.rs:278-285** becomes:

```rust
return Err(MountError::Failed(format!(
    "disk '{}' LUKS UUID mismatch at {}:\n  \
         expected  {}\n  \
         found     {}\n\
     hint: {}",
    name, member.by_id, expected_uuid, uuid,
    luks::luks_uuid_mismatch_guidance()
)));
```

**enroll_key_file.rs:121-125** becomes identical (same format, same
helper call).

**doctor.rs:354-359** becomes:

```rust
uuid_mismatch.push(format!(
    "{name} ({by_id}): expected {expected}, observed {observed} -- {}",
    luks::luks_uuid_mismatch_guidance()
));
```

## Tests

Substring assertions only -- match the project's existing diagnostic-helper
test discipline so wording can evolve without churn.

**New unit tests:**
- `cli/src/luks.rs` -- `luks_uuid_mismatch_guidance_includes_canonical_remediation()`: assert the returned string contains `"detach the foreign disk"`, `"braid replace"`, and `"swap was intentional"`. Mirrors the `luks_header_damaged_guidance` test at `luks.rs:3521-3541`.
- `cli/src/mount.rs` near line 2208 -- extend `mount_luks_uuid_mismatch_closed` (or add a sibling test `mount_luks_uuid_mismatch_includes_remediation_hint`) to assert `msg.contains("detach the foreign disk")` and `msg.contains("braid replace")`. Keep the existing disk-name / UUID-fragment assertions.
- `cli/src/enroll_key_file.rs` near line 800 -- extend `discover_rejects_luks_uuid_mismatch_before_slot_inventory` with the same two `contains` checks.

**Pre-existing tests that must keep passing:**
- `cli/src/doctor.rs:1804` -- already asserts `contains("detach the foreign disk")`; survives the refactor because the helper still emits that substring.
- `cli/src/mount.rs:2196-2207, 2271-2282` -- anchor on disk name and UUID fragments only; the footer addition does not invalidate them.
- `cli/src/enroll_key_file.rs:799` -- anchors on `"LUKS UUID mismatch"`; preserved.
- VM tests `tests/cli/unlock-uuid-mismatch.py:113-124`, `tests/cli/enroll-uuid-mismatch.py:66-73` -- anchor on the header phrase + disk name + UUIDs only; preserved.

**VM test extensions:**
- `tests/cli/unlock-uuid-mismatch.py` -- after the existing `"LUKS UUID mismatch" in ret[1]` assertion, add `"detach the foreign disk" in ret[1]` and `"braid replace" in ret[1]`. Mirrors `tests/cli/braid-doctor-uuid-swap.py:84`.
- `tests/cli/enroll-uuid-mismatch.py` -- same two assertions on `output`.

## Explicitly out of scope

- `cli/src/recover.rs:2078, 2216, 2439, 2532, 3004, 3091` -- six UUID-mismatch sites in recovery context. One (2078) already factors a helper and references `manual/guides/recovery-scenarios.md`. The bare ones (2216/2439/2532/3091) have the same hint-less gap, but recovery surface needs the guide reference instead of the unlock-time hint, and recover.rs:3004 has a state-preservation note (`-- preserving pending-op.json`). Leave a comment in those sites or file a follow-up; do not co-opt this change.
- `cli/src/add.rs:75`, `cli/src/replace.rs:81` -- already emit `-- detach the foreign disk and retry`. Different phrasing is intentional (planning-time / pre-journal-write, retry-focused). Anchored by `tests/cli/braid-add-uuid-swap-rejected.py:163-168`. Leave alone.

## Verification

1. `just test-rust` -- the new `luks.rs` unit test, the updated mount.rs and enroll_key_file.rs unit tests, and the unchanged doctor.rs test all pass.
2. `just test-vm unlock-uuid-mismatch enroll-uuid-mismatch braid-doctor-uuid-swap braid-add-uuid-swap-rejected` -- targeted check that the unlock + enroll VM tests now anchor on the hint, doctor's existing hint anchor still passes, and the add/replace VM test (which already anchors on `"detach the foreign disk"`) is unaffected.
3. Spot-check end-to-end: read the final error message produced by `mount.rs` for a swap scenario from the VM test output to confirm the rendered footer reads cleanly over SSH and uses ASCII (no em-dash; `--` and `hint:` only).
