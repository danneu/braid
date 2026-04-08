# Extend sleep inhibitor coverage to `add`, `remove`, `remove-missing`

## Context

`docs/decisions/inhibit-sleep.md` documents braid's rule for holding a logind sleep inhibitor across irreversible storage mutations. Today only `braid replace` implements that rule (`cli/src/replace.rs:213-233`, wired in `cli/src/main.rs:335`). Three other long-running mutating commands cross the same threshold but currently hold no inhibitor:

- **`braid remove`** — `cli/src/remove.rs`: writes a journal, then runs `evict_present_device`, which optionally rebalances RAID1→single and always runs `btrfs device remove` (data migration, hours on a real pool).
- **`braid remove-missing`** — `cli/src/remove_missing.rs`: writes a journal, removes the missing devid, and conditionally runs `maybe_restore_raid1` (a soft `-dconvert=raid1` balance) when clearing the last missing device on a multi-disk pool.
- **`braid add`** — `cli/src/add.rs`: writes a journal, formats/opens LUKS on the new disks, and either bootstraps the pool (mkfs) or adds to an existing pool followed by `pool_balance_raid1` when the post-add pool has ≥2 disks.

All three meet the decision-doc criterion: suspend inside the post-journal mutation window can corrupt topology, leave the journal pointing at unfinished work, or restart hours of relocation. This plan extends the existing `replace` pattern to all three commands.

## Scope and design rules

Mirror the `replace` pattern exactly:

1. Add `sleep_inhibitor: &'a dyn AcquireSleepInhibitor` to each command's params struct.
2. Acquire the inhibitor **unconditionally** for non-dry-run paths, **immediately before `journal::write_journal()`**, after all interactive work (confirmation, passphrase) and reversible preflight (probes, capacity checks, identity verification). This honors the `feedback_acquire_env_before_journal.md` memory rule.
3. Hold the guard via a `let _sleep_inhibitor_guard = …` binding scoped to the rest of the function so it covers journal write → mutation → membership persist → journal clear.
4. On failure to acquire, return the same `Validation`-style error that `replace` returns (`ReplaceError::Validation` analogue per command), so the user sees `could not acquire sleep inhibitor (is logind running?): {e}` and is not stranded in recovery mode for an environmental error.
5. Never acquire on the dry-run path.
6. Acquire at the `cmd_*` layer, never inside `cli/src/pool.rs` helpers (matches `feedback_caller_specific_gating_belongs_at_callsites.md`).

The `why` strings (visible to `systemd-inhibit --list` and asserted in VM tests):

- `remove`: `"removing disk from pool"`
- `remove-missing`: `"removing missing device from pool"`
- `add`: `"adding disk(s) to pool"`

## Files to modify

### `cli/src/remove.rs`

- Add `sleep_inhibitor: &'a dyn AcquireSleepInhibitor` to `RemoveParams` (currently `cli/src/remove.rs:29-36`).
- Insert the acquire call immediately before the existing `journal::write_journal()` near `cli/src/remove.rs:158`, after the confirmation block at `cli/src/remove.rs:118-143` and the dry-run early-return at `cli/src/remove.rs:107-110`.
- Map io::Error to `RemoveError::Validation` (or whatever error variant matches the existing reversible-preflight failure shape — confirm by reading the existing error enum).
- Guard binding lives until the function returns; mutation phase is `evict_present_device` (`cli/src/remove.rs:162`) → `membership::save_membership` (`cli/src/remove.rs:165-166`) → `journal::clear_journal` (`cli/src/remove.rs:167`).

### `cli/src/remove_missing.rs`

- Add `sleep_inhibitor: &'a dyn AcquireSleepInhibitor` to `RemoveMissingParams` (currently `cli/src/remove_missing.rs:59-66`).
- Insert the acquire call immediately before the existing `journal::write_journal()` near `cli/src/remove_missing.rs:177`, after the confirmation block at `cli/src/remove_missing.rs:147-164` and the dry-run early-return at `cli/src/remove_missing.rs:141-144`.
- Map io::Error to `RemoveMissingError::Validation` (verify variant name).
- Guard covers `pool_remove_devid` (`cli/src/remove_missing.rs:185`) → `maybe_restore_raid1` (`cli/src/remove_missing.rs:187-193`) → `membership::save_membership` (`cli/src/remove_missing.rs:196-197`) → `journal::clear_journal` (`cli/src/remove_missing.rs:199-200`).

### `cli/src/add.rs`

- Add `sleep_inhibitor: &'a dyn AcquireSleepInhibitor` to `AddParams` (currently `cli/src/add.rs:230-240`).
- Insert the acquire call immediately before the existing `journal::write_journal()` near `cli/src/add.rs:485`, after the dry-run early-return at `cli/src/add.rs:359-362`, the confirmation prompt at `cli/src/add.rs:375-391`, the passphrase read at `cli/src/add.rs:394-395`, the passphrase verification at `cli/src/add.rs:397-413`, and the PresentLuks identity check at `cli/src/add.rs:415-468`.
- Map io::Error to `AddError::Validation` (verify variant name).
- Guard covers LUKS format/backup/open of fresh disks (`cli/src/add.rs:488-516`) → bootstrap-or-add path (`cli/src/add.rs:521-554`, including `pool_balance_raid1` at `cli/src/add.rs:551`) → membership enrichment + persist (`cli/src/add.rs:557-577`) → `journal::clear_journal` (`cli/src/add.rs:578`).

### `cli/src/main.rs`

- Hoist the existing `RealSleepInhibitor` instance currently constructed at `cli/src/main.rs:335` so that all four call sites (`cmd_replace` at ~`cli/src/main.rs:335`, `cmd_remove` at ~`cli/src/main.rs:283`, `cmd_remove_missing` at ~`cli/src/main.rs:307`, `cmd_add` at ~`cli/src/main.rs:256`) can pass `&sleep_inhibitor`. A single top-of-`main` instance is fine — `RealSleepInhibitor` is a unit struct.
- Wire the field into each command's params struct construction at the existing call site.

### `cli/src/inhibit.rs`

No changes — `AcquireSleepInhibitor`, `RealSleepInhibitor`, `SleepInhibitor`, and `RecordingInhibitor` are already in place and reusable as-is (`cli/src/inhibit.rs:59-97`).

## Unit test updates

Each command's existing test module already passes a params struct to its `cmd_*` function. Add a `RecordingInhibitor` to each test setup and assert acquire counts. The pattern is established in `cli/src/replace.rs` tests (around `cli/src/replace.rs:1529` and `:1600`).

- **`cli/src/remove.rs`** test module (`cli/src/remove.rs:700-944`): add `RecordingInhibitor::new()` to each `cmd_remove` test, assert `acquire_count() == 1` for non-dry-run paths and `== 0` for dry-run / preflight-failure paths.
- **`cli/src/remove_missing.rs`** test module (`cli/src/remove_missing.rs:322-1180`): same. Cover both `maybe_restore_raid1` triggered (last-missing-cleared on ≥2-disk pool) and not-triggered cases — both should still report `acquire_count() == 1`.
- **`cli/src/add.rs`** test module (`cli/src/add.rs:1625-2119`): same. Cover bootstrap path and add-to-existing-pool path — both should report `acquire_count() == 1` on non-dry-run.

Negative coverage (per `feedback_test_at_failure_layer.md`): each command needs at least one test that would fail if the acquire call were removed. `acquire_count() == 1` already provides that.

## VM tests

Mirror `tests/cli/replace-inhibits-suspend.{nix,py}`. Three new VM tests, each verifying:

1. No braid inhibitor is held before the command runs.
2. The inhibitor (`who=braid`, `what=sleep`, `mode=block`, with command-specific `why` substring) is held while the command is in flight.
3. The inhibitor is released after the command completes.
4. The systemd-inhibit + sh + sleep process group is reaped — no orphan PIDs remain (the existing `pgrep -g <pid>` pattern at `tests/cli/replace-inhibits-suspend.py:241-269`).
5. Pool integrity post-operation (matching the existing pattern of writing a payload before the operation and verifying the sha256 unchanged after).

### New helper file: `tests/cli/inhibitor_helpers.py`

Extract from `tests/cli/replace-inhibits-suspend.py:64-132`:

- `list_inhibitors(machine)` — runs `busctl call … ListInhibitors`, parses the output into structured records.
- `find_braid_sleep_inhibitor(machine)` — filters for `who=="braid" && what=="sleep" && mode=="block"`, returns the matching record or `None`.

**Important — harness wiring**: NixOS VM tests run their `testScript` as a single string passed to a Python interpreter; there is no module path on the test runner so a sibling Python file cannot be `import`ed. The current harness in `tests/cli/replace-inhibits-suspend.nix:40` uses `testScript = builtins.readFile ./replace-inhibits-suspend.py;` for exactly this reason.

The fix is to concatenate the helper file at Nix-eval time so the helper definitions land in the test script's global namespace before the test code runs. Each new `.nix` file (and the existing `replace-inhibits-suspend.nix` after the helper extraction) does:

```nix
testScript = builtins.readFile ./inhibitor_helpers.py
  + "\n\n"
  + builtins.readFile ./<test-name>.py;
```

After extraction, `tests/cli/replace-inhibits-suspend.py` no longer defines `list_inhibitors` / `find_braid_sleep_inhibitor` itself but calls them as bare names; they are provided by the helper file prepended to the script. No `import` statement is added (and would not work). The existing `replace-inhibits-suspend` VM test must still pass after this change — it is the regression check for the helper extraction.

### New VM tests

Each test must force enough real work that the protected window outlives the polling interval used by `wait_until_succeeds`-style probes. Pool members sized 1024 MiB (matching `replace-inhibits-suspend.nix:22-27`); payload sizes chosen to make the long-running phase observable on the slow CI VMs.

- **`tests/cli/remove-inhibits-suspend.{nix,py}`** — 3-disk pool, write a ~400 MiB payload (matching `replace-inhibits-suspend.py`'s payload size), run `braid remove` on one disk so the kernel performs a real `btrfs device remove` data migration with measurable runtime. Assert inhibitor held throughout the device-remove phase, released after, process group cleaned, payload sha256 unchanged.

- **`tests/cli/remove-missing-inhibits-suspend.{nix,py}`** — `maybe_restore_raid1` only triggers a soft balance when (a) the pool was degraded before the operation, (b) the operation cleared the last missing device, and (c) the post-op pool has ≥2 present devices (`pool.rs:138-148`). And the soft balance only has work when there are single-profile chunks to convert — those only exist if the pool was actually written to *while degraded*.

  **Reuse the canonical missing-disk pattern from the existing suite — do not invent a new one.** The pattern is established and proven in:
  - `tests/cli/braid-remove-disk.py` — disk-missing simulation via `umount` → `cryptsetup close braid-disk<N>` → `mount -o degraded`, plus the `get_missing_devid()` helper that parses `braid status --json` for `missing_devids[0]`.
  - `tests/repro/degraded-soft-balance.py` — the "write while degraded so single-profile chunks exist" sequence, which is exactly what makes `pool_balance_raid1_soft` have observable work.

  Test sequence (lifted from those two tests):
  1. Build a 3-disk pool via `braid add`, mount, write a baseline payload. Sized to ensure RAID1 chunks exist.
  2. `umount /mnt/storage` → `cryptsetup close braid-disk3` to make one device unreachable to braid (matches `braid-remove-disk.py`).
  3. `mount -o degraded …` so the pool comes up with 2 present + 1 missing.
  4. Write an additional payload *while degraded* — this is what creates the single-profile chunks the soft balance exists to convert. Lift the size and write pattern from `degraded-soft-balance.py`.
  5. Use the `get_missing_devid()` helper from `braid-remove-disk.py` to read the missing devid out of `braid status --json`.
  6. Run `braid remove-missing --missing-id <devid> --yes` asynchronously. Poll for an in-flight inhibitor observation while the soft balance progresses (`btrfs balance status` non-zero).
  7. Wait for completion, assert inhibitor released, process group cleaned, both payloads' sha256 unchanged.

  Do not introduce a new "make a disk missing" mechanism (e.g. QEMU hot-unplug, `device_del`, deleting the disk image file). The `cryptsetup close` + degraded mount pattern is the canonical one and is already exercised by the test suite.

- **`tests/cli/add-inhibits-suspend.{nix,py}`** — `pool_balance_raid1` only has measurable work when the pre-add pool already contains substantial single-profile data. Test sequence:
  1. Build a 1-disk pool (single profile, by definition).
  2. Mount and write a ~400 MiB payload — all single-profile chunks.
  3. Run `braid add` on a second disk. The post-add path (`cli/src/add.rs:548-554`) runs `pool_balance_raid1` because total devices ≥ 2, and the balance has real work converting the 400 MiB of single-profile data to RAID1.
  4. Assert inhibitor is present and held continuously while the balance progresses (poll for at least one in-flight observation), release after, process group cleaned, payload sha256 unchanged.

For all three tests, follow the existing `replace-inhibits-suspend.py` pattern of starting the braid command asynchronously, polling for in-flight progress before asserting the inhibitor exists, and only declaring success after the command exits cleanly and the inhibitor is gone.

### `flake.nix`

Add three new entries matching the existing `replace-inhibits-suspend` registration pattern:

```nix
remove-inhibits-suspend = pkgs.testers.nixosTest (
  import ./tests/cli/remove-inhibits-suspend.nix { braid = linuxCrane.braid; }
);
remove-missing-inhibits-suspend = pkgs.testers.nixosTest (
  import ./tests/cli/remove-missing-inhibits-suspend.nix { braid = linuxCrane.braid; }
);
add-inhibits-suspend = pkgs.testers.nixosTest (
  import ./tests/cli/add-inhibits-suspend.nix { braid = linuxCrane.braid; }
);
```

## Documentation

Update `docs/decisions/inhibit-sleep.md` "Current application" section (`docs/decisions/inhibit-sleep.md:56-77`) to document the three new commands alongside `replace`, with the same structure: protected scope (journal write through journal clear, including the long-running phases) and excluded scope (dry-run, confirmation, passphrase, reversible validation). Add to the "Consequences" section that `add`/`remove`/`remove-missing` now follow the same boundary rule.

`docs/index.md` does not need an update — the decision doc is already indexed there.

## Reusable code (no new abstractions)

- `cli/src/inhibit.rs:59-70` — `AcquireSleepInhibitor` trait + `RealSleepInhibitor` impl. Reused verbatim.
- `cli/src/inhibit.rs:74-97` — `RecordingInhibitor` for unit tests. Reused verbatim.
- `cli/src/replace.rs:226-233` — acquire-and-bind pattern. Mirror line-for-line per command, only the `why` string and error variant change.
- `tests/cli/replace-inhibits-suspend.py:64-132` — D-Bus inhibitor query helpers. Move into the new shared helper module rather than copying.
- `tests/cli/replace-inhibits-suspend.py:241-269` — process group reap assertion. Mirror per command.

## Verification

1. `just test-rust` — unit tests pass with `RecordingInhibitor` integrated. Each command has at least one test where `acquire_count() == 1` and at least one dry-run test where `acquire_count() == 0`. Refactored `replace-inhibits-suspend.py` import does not break existing tests.
2. `just test-vm replace-inhibits-suspend` — existing replace VM test still passes after the helper extraction (regression check; this is the test that catches the helper-move bug if it exists).
3. `just test-vm remove-inhibits-suspend remove-missing-inhibits-suspend add-inhibits-suspend` — three new VM tests pass: each observes a held braid inhibitor mid-operation, sees it released afterward, and confirms no orphaned systemd-inhibit / sh / sleep PIDs.
4. `just test-vm` — full VM suite green (no regression in `add`, `remove`, `remove-missing`, `replace`, or any other command from the params-struct signature changes).
5. `just test-rust` again after the `docs/decisions/inhibit-sleep.md` update lands (cheap, catches typos in any doc test that may exist).

## Out of scope

- `unlock`, `lock`, `enroll-key-file`, `ack`, `status`, `doctor`, `recover`, `monitor`, `idle` — none cross the "interruption corrupts state or wastes hours" line per the prior triage.
- Any change to the `RealSleepInhibitor` / `SleepInhibitor` plumbing in `cli/src/inhibit.rs` — already correct.
- Any rework of the existing `replace` inhibitor wiring beyond the helper-import change.
