# Plan: doctor declared_disks must catch LUKS UUID swaps

## Context

ADR 024 ([`docs/decisions/024-luks-uuid-identity.md`](../../docs/decisions/024-luks-uuid-identity.md):60-67) lists "earlier clone and swap detection" as a primary benefit of UUID-keyed identity: "UUID mismatches catch disks that were swapped, cloned, or reformatted after the original plan was made." That promise is delivered at *mutation* time -- `mount.rs:274` fails unlock with `MountError::Failed("disk '<name>' LUKS UUID mismatch ...")` and `replace.rs:913-940` re-probes at the open boundary -- but **not** at *read-only* time. `braid doctor`, the only read-only diagnostic command, currently passes a swapped/cloned/reformatted disk as healthy whenever the substituted volume happens to carry a valid LUKS2 header.

Concretely: [`cli/src/doctor.rs:275`](../../cli/src/doctor.rs) (`classify_disk_state`) calls `luks::probe_luks_header`, which only checks `cryptsetup isLuks` + `cryptsetup luksDump`. It never asks for the LUKS UUID. [`cli/src/doctor.rs:393-421`](../../cli/src/doctor.rs) (`check_declared_disks`) iterates the `(uuid, member)` pairs from `pool_membership.iter()` but discards the UUID before calling `classify_disk_state(ctx.runner, Path::new(&by_id))`. Result: a freshly-formatted blank LUKS volume on the same `by_id` slot lands in `DiskState::LuksHeaderOk` and `summarize_declared_disks` reports "all N declared disk(s) present." Operators learn about the mismatch only when `braid unlock` (or any other mutating command) fails. A green doctor on a swapped disk is the opposite of the ADR-promised early-warning surface.

Outcome: `braid doctor` reports a `Fail`-graded `declared_disks` entry naming both expected and observed UUIDs whenever a member disk's live LUKS UUID diverges from its pool.json key.

## Critical files

- [`cli/src/doctor.rs`](../../cli/src/doctor.rs) -- new `DiskState::LuksUuidMismatch` variant; new pure helper `classify_luks_identity`; `classify_disk_state` becomes the filesystem gate that delegates to the helper; `summarize_declared_disks` adds a mismatch group and escalates the rollup to `Fail`; `check_declared_disks` threads the UUID key.
- [`cli/src/luks.rs`](../../cli/src/luks.rs) -- consulted only, not modified. We deliberately do *not* widen `luks_uuid_for_device`'s visibility (it returns `OwnershipError`, which would couple doctor to ownership-classifier types unnecessarily).
- [`cli/src/parse/cryptsetup_luks_uuid.rs`](../../cli/src/parse/cryptsetup_luks_uuid.rs) -- reused as-is. `parse_cryptsetup_luks_uuid` returns `Result<CryptsetupLuksUuidOutput, ParseError>` and canonicalizes via `LuksUuid::parse`, matching how `add.rs:243`, `probe.rs:139`, and `replace.rs:927` already call it directly.
- [`tests/cli/braid-doctor-uuid-swap.py`](../../tests/cli/) -- new test script (the script body NixOS test framework reads).
- [`tests/cli/braid-doctor-uuid-swap.nix`](../../tests/cli/) -- new NixOS test wrapper; imports the `.py` via `builtins.readFile`. Models on [`tests/cli/braid-add-uuid-swap-rejected.nix`](../../tests/cli/braid-add-uuid-swap-rejected.nix).
- [`flake.nix`](../../flake.nix) -- register the new `.nix` wrapper in `checks.aarch64-darwin` (and `x86_64-linux`), following the `braid-doctor`/`braid-add-uuid-swap-rejected` entries at lines 126 and 246.
- [`manual/commands/doctor.md`](../../manual/commands/doctor.md) -- update line 55 (`declared_disks` description) and line 78 ("under the hood") to describe expected-vs-observed UUID checking.
- [`docs/decisions/024-luks-uuid-identity.md`](../../docs/decisions/024-luks-uuid-identity.md) -- append the new doctor test to the "Tests That Enforce This" list (currently lines 140-168).

## Code shape

### 1. Extend `DiskState`

[`cli/src/doctor.rs:243-263`](../../cli/src/doctor.rs): add a new variant alongside the existing five.

```rust
/// `cryptsetup isLuks` and `cryptsetup luksDump` succeeded, but the live
/// LUKS UUID does not match the pool.json key UUID. Operator-actionable:
/// a disk was swapped, cloned, or reformatted on the same by-id slot
/// since membership was journaled. Severity-graded as Fail in the rollup
/// because every subsequent mutating command will refuse this disk --
/// doctor is the early-warning surface ADR 024 promises.
LuksUuidMismatch { expected: LuksUuid, observed: LuksUuid },
```

Add `crate::types::LuksUuid` to the imports. Update the enum-level doc to mention six reachable outcomes (was five).

### 2. Split `classify_disk_state` into a filesystem gate and a pure LUKS-identity helper

Today `classify_disk_state` does both filesystem-existence checks and runner calls in one function. The runner-driven half is unreachable from unit tests because `std::fs::metadata` runs first against a non-existent fake path; that is why the existing file pins testing on the *pure* `summarize_declared_disks`. To make the new UUID branch unit-testable, factor the runner-driven half into its own pure-of-filesystem helper.

New helper, runner-only, no filesystem touches:

```rust
/// Pure-of-filesystem LUKS-identity classifier. Given a device path that
/// the caller has already confirmed is a real block device, run the
/// shared LUKS header probe and verify the live UUID equals
/// `expected_uuid`. Returns one of the four LUKS-side `DiskState`
/// outcomes (LuksHeaderUnreadable, LuksHeaderDamaged, ProbeFailed,
/// LuksHeaderOk, LuksUuidMismatch). Pure with respect to the
/// filesystem; all I/O flows through `runner`, so MockRunner unit tests
/// can pin every branch.
fn classify_luks_identity<R: CommandRunner>(
    runner: &R,
    device: &str,
    expected_uuid: &LuksUuid,
) -> DiskState {
    match luks::probe_luks_header(runner, device) {
        luks::LuksHeaderState::Unreadable => return DiskState::LuksHeaderUnreadable,
        luks::LuksHeaderState::Damaged => return DiskState::LuksHeaderDamaged,
        luks::LuksHeaderState::ProbeFailed(err) => return DiskState::ProbeFailed(err),
        luks::LuksHeaderState::Ok => {}
    }

    let raw = match runner.run(&CmdRequest::CryptsetupLuksUuid {
        device: device.to_owned(),
    }) {
        Ok(r) => r,
        Err(e) => return DiskState::ProbeFailed(e.to_string()),
    };
    let observed = match parse_cryptsetup_luks_uuid(&raw) {
        Ok(out) => out.uuid,
        // probe_luks_header::Ok already proved isLuks + luksDump
        // succeeded; a luksUUID parse failure here is a tool anomaly,
        // not a corrupted header. Classify as ProbeFailed so no "repair
        // the header" guidance is emitted.
        Err(e) => return DiskState::ProbeFailed(e.to_string()),
    };

    if observed == *expected_uuid {
        DiskState::LuksHeaderOk
    } else {
        DiskState::LuksUuidMismatch {
            expected: expected_uuid.clone(),
            observed,
        }
    }
}
```

`classify_disk_state` retains the filesystem gate and delegates:

```rust
fn classify_disk_state<R: CommandRunner>(
    runner: &R,
    path: &Path,
    expected_uuid: &LuksUuid,
) -> DiskState {
    match std::fs::metadata(path) {
        Err(_) => return DiskState::Missing,
        Ok(meta) if !meta.file_type().is_block_device() => return DiskState::NotBlock,
        Ok(_) => {}
    }
    let device = path.to_string_lossy().into_owned();
    classify_luks_identity(runner, &device, expected_uuid)
}
```

Add the import next to the existing parser import at [`cli/src/doctor.rs:24`](../../cli/src/doctor.rs) -- doctor.rs imports parsers directly (`use crate::parse::parse_btrfs_df_json;`), so the new import should be `use crate::parse::parse_cryptsetup_luks_uuid;` and the helper should call `parse_cryptsetup_luks_uuid(&raw)` directly. `crate::cmd::CmdRequest` is already in scope.

### 3. Update `check_declared_disks` to thread the UUID

[`cli/src/doctor.rs:411-418`](../../cli/src/doctor.rs):

```rust
let mut members: Vec<(&LuksUuid, &DiskMember)> = pool_membership.iter().collect();
members.sort_by(|(_, a), (_, b)| a.name.cmp(&b.name));
let classifications: Vec<(String, String, DiskState)> = members
    .into_iter()
    .map(|(uuid, member)| {
        let by_id = member.by_id.as_str().to_owned();
        let state = classify_disk_state(ctx.runner, Path::new(&by_id), uuid);
        (member.name.as_str().to_owned(), by_id, state)
    })
    .collect();
```

### 4. Render the mismatch and escalate severity

[`cli/src/doctor.rs:301-391`](../../cli/src/doctor.rs) (`summarize_declared_disks`): add a `uuid_mismatch` accumulator alongside `missing` / `not_block` / `probe_failed` / `header_unreadable` / `header_damaged`. Append a match arm:

```rust
DiskState::LuksUuidMismatch { expected, observed } => {
    uuid_mismatch.push(format!(
        "{name} ({by_id}): expected {expected}, observed {observed} -- \
         disk was swapped, cloned, or reformatted; detach the foreign \
         disk and reattach the original, or run 'braid replace' if the \
         swap was intentional"
    ));
}
```

Reuse the "detach the foreign disk" wording from [`cli/src/replace.rs:80`](../../cli/src/replace.rs) so doctor / replace / unlock messages read consistently.

Add a `parts.push(...)` block for the mismatch group, then change the final rollup so a mismatch promotes to `Fail`:

```rust
if !uuid_mismatch.is_empty() {
    CheckResult::fail("declared_disks", format!(...))
} else if problem_count > 0 {
    CheckResult::warn("declared_disks", format!(...))
} else {
    CheckResult::ok(...)
}
```

`overall_status()` ([`cli/src/doctor.rs:833`](../../cli/src/doctor.rs)) already cascades `Fail > Warn > Ok`, so a single mismatched disk promotes the whole `DoctorReport.status` to `Fail`, and `cmd_doctor` at [`cli/src/doctor.rs:953-956`](../../cli/src/doctor.rs) returns `Err(DoctorError::Failed)` -- the process exits non-zero.

## Tests

### Rust unit tests (`cli/src/doctor.rs` `#[cfg(test)] mod tests`)

Target the pure `classify_luks_identity` helper, not `classify_disk_state`. Three new tests using `MockRunner` -- match the existing fixture pattern in the file:

1. **`classify_luks_identity_returns_luks_uuid_mismatch_when_observed_diverges`** -- pin the new branch. MockRunner returns `isLuks` ok, `luksDump` ok, `luksUUID` with a UUID that differs from `expected_uuid`. Assert `DiskState::LuksUuidMismatch { expected, observed }` with both fields naming the canonical lowercase-hyphenated forms.

2. **`classify_luks_identity_returns_luks_header_ok_when_uuid_matches`** -- pin the negative. Same stub chain but `luksUUID` returns the expected UUID. Assert `DiskState::LuksHeaderOk`.

3. **`summarize_declared_disks_promotes_to_fail_on_uuid_mismatch`** -- pin the rollup escalation (this one tests the existing pure function `summarize_declared_disks` with a slice that includes the new variant). Assert `CheckResult { status: Fail, .. }` and that the rendered message contains the expected UUID, the observed UUID, and "detach the foreign disk".

The mismatch helper for `CryptsetupLuksUuid` can stay inline -- existing UUID-mismatch tests in `add.rs:3119-3121` and `enroll_key_file.rs:1408-1409` inline their MockRunner arms rather than sharing a fixture, so no `test_fixtures/mount.rs` change is required.

### NixOS VM test (`tests/cli/braid-doctor-uuid-swap.py` + `.nix` wrapper)

New `.py` script with three-section preamble per [`docs/testing.md`](../../docs/testing.md):

```
# Intent: braid doctor's declared_disks check fails when a member disk's
#   live LUKS UUID no longer matches its pool.json key UUID.
# Why it exists: ADR 024 commits to "earlier swap detection" as a primary
#   doctor surface, but classify_disk_state previously stopped at LUKS
#   header presence and never verified UUID identity. A swapped disk
#   passed silently until the next mutating command (unlock, replace).
# Scenario: a 2-disk RAID1 pool is unlocked and mounted. Operator powers
#   down, swaps disk1 for a different LUKS2 volume in the same physical
#   slot (or reformats disk1 by mistake). On reboot, before any unlock
#   attempt, braid doctor must surface the mismatch with Fail severity.
```

Test flow:

1. Build a 2-disk pool with deterministic UUIDs (use the `braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000` pattern from `braid-add-uuid-swap-rejected.py:24-29`).
2. Capture `initial_uuid = machine.succeed("cryptsetup luksUUID /dev/disk/by-id/virtio-disk1").strip()`.
3. Lock the pool: `umount /mnt/storage`, `cryptsetup close braid-disk1`, `cryptsetup close braid-disk2`.
4. Swap disk1's UUID:
   ```
   printf '%s' "$pass" | cryptsetup luksFormat \
       --batch-mode --label=braid-disk1 --key-file=- \
       --pbkdf pbkdf2 --pbkdf-force-iterations 1000 \
       /dev/disk/by-id/virtio-disk1
   ```
5. Capture `swapped_uuid` and assert it differs from `initial_uuid`.
6. Run doctor and capture both exit code and stdout (Fail status causes `cmd_doctor` to return `Err`, which makes main.rs exit 1; `machine.succeed` would abort here):
   ```python
   exit_code, raw = machine.execute("braid doctor --json 2>/tmp/doctor.err")
   assert exit_code != 0, f"doctor must exit non-zero on Fail: {raw}"
   report = json.loads(raw)
   ```
   (`2>/tmp/doctor.err` keeps `print_cli_error("Error: doctor found failures")` from polluting the JSON stdout.)
7. Assert:
   - `report["status"] == "fail"`
   - The `declared_disks` check has `status == "fail"`
   - Its `message` contains `disk1`, `expected <initial_uuid>`, `observed <swapped_uuid>`, and `detach the foreign disk`
   - disk2's row (UUID unchanged) is *not* listed as problematic in the message

New `.nix` wrapper at `tests/cli/braid-doctor-uuid-swap.nix` modeled on `tests/cli/braid-add-uuid-swap-rejected.nix`:

```nix
{ braid }:
{
  name = "braid-doctor-uuid-swap";
  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];
    environment.systemPackages = [ braid pkgs.cryptsetup pkgs.btrfs-progs ];
    environment.etc."braid/config.json".text = builtins.toJSON { mount_point = "/mnt/storage"; };
  };
  testScript = builtins.readFile ./braid-doctor-uuid-swap.py;
}
```

Register in [`flake.nix`](../../flake.nix) alongside the existing `braid-doctor` and `braid-add-uuid-swap-rejected` entries (lines 126 and 246):

```nix
braid-doctor-uuid-swap = pkgs.testers.nixosTest (
  import ./tests/cli/braid-doctor-uuid-swap.nix {
    braid = linuxCrane.braid;
  }
);
```

(Existing entries at `flake.nix:122-130` use `braid = linuxCrane.braid;`; there is no in-scope `braid` binding at the registration site, so `{ inherit braid; }` would fail Nix eval.)

## Doc updates

Both are short edits, not new docs:

- [`manual/commands/doctor.md`](../../manual/commands/doctor.md): change line 55 from "Every UUID-keyed pool.json member is present and has a readable LUKS header" to something like "Every UUID-keyed pool.json member is present, has a readable LUKS header, and its live LUKS UUID matches the pool.json key". Update line 78 ("probes each declared disk via `cryptsetup isLuks` and `cryptsetup luksDump`") to add "and verifies the live LUKS UUID with `cryptsetup luksUUID`".

- [`docs/decisions/024-luks-uuid-identity.md`](../../docs/decisions/024-luks-uuid-identity.md): append to the "Tests That Enforce This" list (after current line 168):
  ```
  - `tests/cli/braid-doctor-uuid-swap.py` verifies `braid doctor` fails
    closed when a member's live LUKS UUID diverges from its pool.json
    key, surfacing the swap before any mutating command runs.
  ```

## Verification

End-to-end:

```sh
just test-rust                                  # unit tests, including the three new ones
just test-vm braid-doctor-uuid-swap             # the new VM test
just test-vm braid-doctor                       # regression: existing doctor tests still pass
just test-vm braid-add-uuid-swap-rejected       # regression: sibling swap test still passes
```

Manual smoke (optional, on a VM with real cryptsetup):

```sh
braid doctor --json | jq '.checks[] | select(.name=="declared_disks")'
# Expect: status="ok", "all 2 declared disk(s) present"

# (lock + swap a disk's UUID -- see VM test step 4)
braid doctor --json 2>/dev/null | jq '.checks[] | select(.name=="declared_disks")'
echo "exit=$?"
# Expect: status="fail", message names expected vs observed UUID and
# "detach the foreign disk"; exit code 1.
```

## Out of scope

- Re-grading `LuksHeaderUnreadable` / `LuksHeaderDamaged` / `Missing` / `ProbeFailed` to `Fail`. The current rollup grades them as `Warn`; this fix only escalates the new variant. Uniform severity-rebalancing for severe per-disk states belongs in its own ticket.
- The post-mount `foreign-luks-uuid` check planned in [`plans/impl/2026-05-12-luks-uuid-as-identity/plan.md`](../../plans/impl/2026-05-12-luks-uuid-as-identity/plan.md):1330-1367. That check fires against live mappers in a mounted pool whose backing UUID isn't in membership; the present fix runs against the by_id surface before unlock. Complementary, not overlapping.
- Widening `luks::luks_uuid_for_device` visibility. Calling `parse_cryptsetup_luks_uuid` directly matches the pattern already used by `add.rs`, `probe.rs`, and `replace.rs` and avoids dragging `OwnershipError` into the doctor module.
