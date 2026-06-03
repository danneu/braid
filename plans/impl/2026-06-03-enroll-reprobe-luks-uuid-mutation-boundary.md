# Enroll: re-probe LUKS UUID at the mutation boundary

## Context

`braid enroll` writes a keyfile into LUKS slot 1 of each pool member. Today it
verifies each member's live LUKS UUID against the `pool.json` key **only** at
pre-passphrase discovery (`enroll_key_file.rs#discover_enrollment_candidates`,
the `expected_uuid != uuid` arm). After that check, `EnrollPlan::execute` reads
the passphrase, then `plan_enrollment` (passphrase verify + slot-1 inventory)
and `apply_enrollment` (the `cryptsetup luksAddKey` mutation via
`luks::enroll_key_file`) all operate purely on the persisted
`candidates: Vec<(DiskName, ByIdPath)>` -- never re-reading the live UUID.

The passphrase prompt is an operator-controlled window (seconds to minutes). A
disk swapped or reformatted in that window to a foreign LUKS container that
**shares the pool passphrase** passes the post-prompt passphrase verify and the
slot-1 check, so the keyfile lands in slot 1 of the wrong container while the
intended member's slot 1 stays empty -- silently breaking auto-unlock at boot.
This is exactly the "swapped/cloned disk silently takes the keyfile" failure
`tests/cli/enroll-uuid-mismatch.py` exists to prevent, just shifted past the
discovery check into the prompt window.

Decision-024 mandates re-checking live UUIDs "again at mutation boundaries where
a physical disk could have been swapped or reformatted" and `replace` already
pays this cost (item 10 + `replace.rs#probe_existing_luks_new_target_uuid`).
Enroll is a LUKS-mutating command with the same window and no such guard. This
plan closes that gap, mirroring replace's open-boundary re-probe, and folds the
twice-duplicated UUID-mismatch message into one shared helper.

Severity is Medium (not High): no journal, no btrfs topology change, blast
radius is one extra keyfile slot rather than data loss; and with serial-based
by-id paths a *different* physical disk yields a different by-id path (handled as
Absent). But the architecture has already decided this class is defended
uniformly at every mutating boundary, so the consistency gap is real.

## Design (decisions locked)

1. **Focused `luksUUID` re-probe, runner-only.** Mirror
   `replace.rs#probe_existing_luks_new_target_uuid`, *not* the full
   `probe::probe_config_disk`. `EnrollPlan::execute` has no `fs` handle (only
   `runner` + `params`), and `probe_config_disk` would re-run the LUKS2-version
   gate and mapper-backing checks that are irrelevant here: the `luksAddKey`
   mutation targets the raw by-id block device, so the only identity question is
   "is the live header UUID still the expected one." A single
   `CmdRequest::CryptsetupLuksUuid` parsed by `parse_cryptsetup_luks_uuid`
   answers that and **fails closed** on every swap variant: a different live
   UUID (incl. a LUKS1 swap -- `luksUUID` reads a LUKS1 header at exit 0) hits
   the mismatch arm; an absent or non-LUKS device exits non-zero ->
   `CommandFailed` -> the fail-closed arm. No swap is silently accepted.

2. **Carry the expected UUID on the candidate.** Widen `EnrollmentCandidate`
   from the tuple `(DiskName, ByIdPath)` to a small struct
   `#[derive(Debug, Clone)] struct EnrollmentCandidate { name: DiskName, by_id: ByIdPath, uuid: LuksUuid }`.
   (`Debug` is mandatory -- `EnrollPlan` derives `Debug` and holds
   `Vec<EnrollmentCandidate>`; `Clone` enables the dry-run `c.clone()` push in
   #4. `DiskName`/`ByIdPath`/`LuksUuid` all derive both.)
   Discovery already proves `expected_uuid == live uuid` and has the value in
   hand; carry it forward instead of re-deriving it. This removes a "can't
   happen" `None` branch (a `membership.by_name()` lookup at execute would return
   `None` for the existing empty-membership test fixture), and tightens the
   invariant: the re-probe compares against the exact UUID discovery validated.

3. **Placement: a batch loop in `EnrollPlan::execute`, immediately after
   `read_passphrase` and before `plan_enrollment`.** Re-probe every candidate up
   front. Doing it before the passphrase verify means a swap reports a clear
   "LUKS UUID mismatch" rather than a misleading "wrong passphrase", gates the
   mutation, and (in `--generate` mode) blocks keyfile creation too since the
   generate step runs later. `plan_enrollment`'s signature stays unchanged.

4. **Extract the shared mismatch-message helper.** The full multi-line template
   (not just the already-shared `luks_uuid_mismatch_guidance()` hint) is
   duplicated byte-for-byte in `enroll_key_file.rs#discover_enrollment_candidates`
   and `mount.rs`. Add `luks::format_luks_uuid_mismatch` next to
   `luks::luks_uuid_mismatch_guidance` and route all three sites (enroll
   discovery, mount, the new re-probe) through it. `doctor.rs` uses a different
   one-line shape -- leave it.

### Known limit (documented, not closed)

A sub-second TOCTOU window remains between the re-probe and `luksAddKey`,
identical in kind to replace's probe -> open window. The batch placement makes
each disk's window slightly wider than replace's single-target case (re-probe
all, then apply one-by-one), but it closes the *dominant* threat -- the human
passphrase-prompt window. Per-disk re-probe inside `apply_enrollment` would
shave the negligible residual at the cost of worse diagnostics and a guard
spread across two functions; explicitly out of scope. Note this in the new fn's
doc-comment.

A related, intended behavioral change: a member *present at discovery but
disconnected or reformatted by execute* now hard-fails the whole batch (the `?`
aborts before any disk is enrolled, including healthy disks earlier in the
loop), whereas discovery *skips* an already-absent disk and proceeds on the
rest. This skip-on-absent vs. hard-fail-on-vanish asymmetry is deliberate: a
mid-prompt disappearance is indistinguishable from a swap in progress, so
fail-closed is the correct choice. The fail-closed message tells the operator to
re-run, and the re-run re-discovers and cleanly skips a genuinely-absent member.

## Implementation

### Production

1. **`cli/src/luks.rs`** -- add `pub(crate) fn format_luks_uuid_mismatch(name: &str, by_id: &ByIdPath, expected: &LuksUuid, found: &LuksUuid) -> String` next to `luks_uuid_mismatch_guidance`. It must render exactly today's string and end by calling `luks_uuid_mismatch_guidance()`:
   ```
   disk '<name>' LUKS UUID mismatch at <by_id>:
     expected  <expected>
     found     <found>
   hint: <guidance>
   ```
   (i.e. `"disk '{name}' LUKS UUID mismatch at {by_id}:\n  expected  {expected}\n  found     {found}\nhint: {}"`). Add a one-line doc-comment.

2. **`cli/src/mount.rs`** (`plan_open_pool_inner`, the `PresentLuks` UUID-mismatch arm) -- replace the inline `format!` with `luks::format_luks_uuid_mismatch(display_name, &member.by_id, expected_uuid, uuid)` inside `MountError::Failed(...)`.

3. **`cli/src/enroll_key_file.rs#discover_enrollment_candidates`** -- replace the inline `format!` with `luks::format_luks_uuid_mismatch(name.as_str(), &member.by_id, expected_uuid, uuid)`, and at the candidate push carry the UUID: `candidates.push(EnrollmentCandidate { name: name.clone(), by_id: member.by_id.clone(), uuid: expected_uuid.clone() })`.

4. **`cli/src/enroll_key_file.rs`** -- type + ripple:
   - Add `LuksUuid` to `use crate::types::{...}` (currently absent).
   - Change `EnrollmentCandidate` (the `pub type` alias) into the struct in Design #2, with a doc-comment. `EnrollmentCandidateDiscovery`, `EnrollPlan.candidates`, and `compile_enroll_steps`'s `&[EnrollmentCandidate]` flow through automatically.
   - **Destructure sites** (read `name`/`by_id`): `plan_enrollment`'s `verify_targets` map and per-candidate loop, and `compile_enroll_steps`. Rebind `for c in &candidates` (or `for c in candidates`) and use `&c.name`/`&c.by_id`. (`plan_single_disk_enrollment` already takes `name`/`by_id` params -- callers pass `&c.name, &c.by_id`.)
   - **Construction site -- do NOT rebuild from the action.** The dry-run loop in `plan_enroll` does `needs_enroll.push((name, by_id))` (the `NeedsEnroll { name, by_id }` arm carries no `uuid`, so a struct literal there would be `error[E0063]: missing field uuid`). Rebind the loop `for c in &candidates`, keep matching on `plan_single_disk_enrollment(...)`'s action, and in the `NeedsEnroll` arm push the original candidate: `Ok(DiskEnrollAction::NeedsEnroll { .. }) => needs_enroll.push(c.clone())`. The action's name/by_id are clones of `c`'s, so this is semantically identical and carries the validated uuid; the dry-run path never re-probes, so the carried uuid is inert. (The `AlreadyEnrolled { name, .. }` arm keeps binding `name` for its skip note.)

5. **`cli/src/enroll_key_file.rs`** -- add command-local `fn reprobe_member_luks_uuid<R: CommandRunner>(runner: &R, name: &DiskName, by_id: &ByIdPath, expected: &LuksUuid) -> Result<(), EnrollKeyFileError>`, mirroring `replace.rs#probe_existing_luks_new_target_uuid`:
   - `runner.run(&CmdRequest::CryptsetupLuksUuid { device: by_id.as_str().to_owned() })?` (CmdError propagates -> fail closed).
   - `match parse_cryptsetup_luks_uuid(&raw)`: `Ok(p) if p.uuid == *expected => Ok(())`; `Ok(p) => Err(Validation(luks::format_luks_uuid_mismatch(name.as_str(), by_id, expected, &p.uuid)))`; `Err(_) => Err(Validation(<fail-closed: "disk '{name}': cannot confirm LUKS UUID at {by_id} before enrolling -- device may have been swapped or disconnected after planning; re-run `braid enroll`">))`.
   - Use `--` not em-dash. Add the AGENTS.md doc-comment incl. the residual-window note. Confirm the `parse_cryptsetup_luks_uuid` import path (match `replace.rs`).

6. **`cli/src/enroll_key_file.rs#EnrollPlan::execute`** -- between `read_passphrase` and `plan_enrollment`, add: `for c in &self.candidates { reprobe_member_luks_uuid(runner, &c.name, &c.by_id, &c.uuid)?; }`. No membership lookup needed.

### Tests (`enroll_key_file.rs` `mod tests`, fixtures in `test_fixtures/enroll_key_file.rs`)

7. **Standalone fn tests** -- mirror `replace.rs`'s `runner_with_luks_uuid_probe` + seed-630 pattern:
   - `reprobe_member_luks_uuid_mismatch_rejects`: runner returns a different UUID; assert the error contains "LUKS UUID mismatch", the disk name, both UUIDs, and "detach the foreign disk"; assert exactly one `CryptsetupLuksUuid` ran. (Optional cheap pin: one exact-string assertion on `format_luks_uuid_mismatch`'s output if the `  expected  ` / `  found     ` layout is to be treated as a contract -- otherwise the whitespace is cosmetic and below the test bar.)
   - `reprobe_member_luks_uuid_probe_failure_fails_closed`: runner returns a non-zero output (reuse the `enroll_luks_uuid_not_luks` shape); assert the fail-closed wording and that no mutation request followed.

8. **End-to-end window test** `execute_rejects_swapped_disk_before_mutation` -- prove the discovery->execute window is closed. Run it in `--generate` mode (with the mountpoint-ok fixture) so the "keyfile not created" assertion actually bites. Use `MockRunner::with_output_sequence(CryptsetupLuksUuid{device: d1}, vec![matching_uuid, mismatched_uuid])` (discovery pops #1 and passes; execute re-probe pops #2 and fails), matching-only for d2, plus `with_mappers_closed(...)`, `with_luks_dump_text_luks2`, and the standard fixtures. Supply the passphrase via `passphrase_file: Some(&pass_path)` (write it to a temp file, as the sibling `execute_generate_partial_failure_reports_recovery_hint` test does) -- **not** `passphrase_stdin`: `EnrollPlan::execute` calls the public `luks::read_passphrase`, which has no reader-injection seam and would `dup` the empty real process stdin under `cargo test` (rejected per `luks.rs#read_passphrase_stdin_from_empty_rejected`), failing before the re-probe is ever reached. Drive `plan_enroll` then `plan.execute`. Assert: the mismatch error surfaces; **no** `CryptsetupLuksAddKeyFile` in `runner.requests()`; keyfile not created (`!kf.exists()`). **Pin with a comment**: "mappers closed => discovery issues exactly one luksUUID per disk, so the 2nd sequence element is consumed by the execute re-probe" (a mapper-open disk would pop both at discovery and silently invert the test).

9. **Repair existing execute-path tests.** Adding the re-probe means every test that reaches `execute` with a *directly constructed* `EnrollPlan` (no discovery luksUUID mock) now needs an execute-time `CryptsetupLuksUuid` mock returning the carried `candidate.uuid`. Full-flow tests that call `plan_enroll` first are unaffected (the static mock serves both probes). Audit: `rg 'EnrollPlan \{|\.execute\(' cli/src/enroll_key_file.rs` in the test module. Known case: `execute_generate_partial_failure_reports_recovery_hint` -- add the `uuid` field to its directly-constructed candidate literals and a matching `CryptsetupLuksUuid` mock per by-id so the re-probe passes and the test still reaches its `luksAddKey` apply-failure assertion.
   - Separately (struct migration, not the re-probe), the four `plan.candidates[..].0.as_str()` read-backs live in the three discovery tests (`plan_discover_two_present_luks_disks` has two, `plan_discover_absent_disk_accumulates_skip_note`, `plan_discover_non_luks_disk_accumulates_skip_note`) -- *not* in the execute test. Update `.0.as_str()` -> `.name.as_str()`.

10. **No new VM test.** An in-window physical swap during the passphrase prompt is not deterministically reproducible in a NixOS VM; `tests/cli/enroll-uuid-mismatch.py` already covers the pre-command reformat (discovery) case for both dry-run and real-run. The new execute-boundary guard is covered by the Rust tests above. State this in the test preambles.

### Docs

11. **`docs/design/decisions/024-luks-uuid-identity.md`** -- under "Tests That Enforce This", extend the `enroll_key_file.rs` bullet (currently only the discovery-time rejection) to add the execute-boundary re-probe coverage, and add a short note that enroll re-probes member UUIDs at its mutation boundary. Leave item 10 (replace-specific) as-is; the general principle is already in Consequences.

12. **`docs/commands/enroll.md`** -- add one sentence (near the "What happens under the hood" step 4 or "Safety checks") noting enroll re-checks each member's UUID again at the mutation boundary *after* the passphrase is read. Do not reword the existing step that accurately describes the pre-prompt discovery check.

## Verification

- `just test-rust` -- the new standalone + E2E tests pass; the repaired execute-path tests pass; `discover_rejects_luks_uuid_mismatch_before_slot_inventory` and the mount UUID-mismatch tests still pass. These assert *substrings*, so they confirm the extracted helper preserves the asserted tokens, not its exact whitespace; routing all three sites through one helper keeps them identical to each other, and the only residual risk is helper-vs-today layout (covered by the optional exact-string pin in test #7a if desired).
- `just test-vm enroll-uuid-mismatch` -- the existing VM test still passes (the shared helper preserves every asserted substring: "LUKS UUID mismatch", "detach the foreign disk", "braid replace", disk name, both UUIDs).
- `just test-vm braid-enroll` (and any other enroll VM test) -- happy-path enroll still succeeds end-to-end (the re-probe sees a matching UUID and is a no-op).
- `mdbook build docs` -- doc cross-links validate.
- Scope check: focused run only (enroll + mount touched); not a broad-blast-radius change, so the full suite is the user's call, not an autonomous rerun.

## Implementation notes

- Test ripple was wider than item #9's audit. The `EnrollmentCandidate`
  tuple->struct migration breaks every test that builds a
  `vec![(disk(..), enroll_by_id(..)), ..]` candidate literal and passes it to
  `plan_enrollment` / `compile_enroll_steps` (~14 sites in the
  `plan_enrollment` and `compile_enroll_steps` test cohorts). Item #9's audit
  (`rg 'EnrollPlan \{|\.execute\('`) only covers directly-built `EnrollPlan`s,
  not these. Migrated them via a new local test helper
  `enroll_candidate(name, by_id)` carrying an inert `test_uuid(500)` (these
  paths never re-probe, so the uuid is unused there), which keeps the call
  sites terse. The execute test from item #9 keeps explicit struct literals
  with distinct per-disk uuids since its re-probe mock must match them.
- Made `EnrollmentCandidate` `pub` (not `pub(crate)`) to match the visibility
  of the original `pub type` alias: `compile_enroll_steps` and
  `EnrollPlan::candidates` are `pub`, so a `pub(crate)` type there trips
  `private_interfaces` warnings.
