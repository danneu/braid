# Fix: header backup must follow keyfile enrollment in `add` and `replace`

## Context

`braid add` and `braid replace` produce a LUKS2 header backup as part of bringing up a freshly formatted disk, in both the dry-run plan compiler and the real execution path. In all four code paths, the backup currently runs between `luksFormat` and the optional `luksAddKey`:

- Dry-run preview: `cli/src/add.rs:993-1034` (`compile_add_steps`, `PresentNotLuks` arm).
- Dry-run preview: `cli/src/replace.rs:867-908` (`compile_replace_steps`, `PresentNotLuks` arm).
- Real run: `cli/src/add.rs:584-597` (`AddPlan::execute`, `PresentNotLuks` branch).
- Real run: `cli/src/replace.rs:305-318` (`ReplacePlan::execute`, `PresentNotLuks` branch).

`ReplacePlan::execute` discards the precompiled `steps` (`cli/src/replace.rs:115-117`, `steps: _`) and re-implements the sequence inline, so the dry-run and real-run paths diverge structurally and must be fixed together.

When `--enroll-key-file` is passed, the resulting backup file is captured before slot 1 is enrolled. Restoring that backup wipes the keyfile slot. The passphrase in slot 0 still unlocks the disk (the master volume key is unchanged, so the data remains decryptable), but auto-unlock via keyfile -- the typical NAS boot flow -- breaks until the user re-runs `braid enroll keyfile`. Degraded recovery, not data loss, but a real regression for an unattended-boot NAS.

The standalone `braid enroll keyfile` flow already orders correctly (`cli/src/enroll_key_file.rs:330-351`).

## Approach

Reorder so the header backup runs **immediately after the final keyslot mutation** and **before** `luksOpen`. The new fresh-disk sequence becomes:

1. `cryptsetup luksFormat`
2. `cryptsetup luksAddKey` (only when `--enroll-key-file` is set; does not require an open mapper -- consumes the existing passphrase and the new keyfile from the underlying device)
3. `cryptsetup luksHeaderBackup`
4. `cryptsetup luksOpen`
5. ... remaining btrfs / pool / mount / replace-resize-balance steps

Why this order, not "backup last":

- **Keeps the early-backup property for the no-keyfile path.** Today's no-keyfile flow backs up immediately after format; moving backup to the very end (after open / mkfs / mount / btrfs replace / soft balance) widens the no-backup window unnecessarily and churns dry-run line-index assertions in unrelated tests.
- **Captures every keyslot that exists on the device.** Slot 0 is always written by `luksFormat`; slot 1 is written by `luksAddKey` only when `--enroll-key-file` is set. Backup runs strictly after the last possible keyslot write.
- **Matches the `enroll_key_file.rs` reference pattern** (`luksAddKey` then `luksHeaderBackup`), now adapted to the fresh-disk flow where `luksFormat` precedes `luksAddKey`.

Backup is still always emitted for `PresentNotLuks` (regardless of `--enroll-key-file`), unchanged from today.

`Step::render_dry_run` (`cli/src/cmd.rs:266-278`) iterates `steps` in order, so reordering `steps.push(...)` is sufficient for the dry-run path. Real-run paths require reordering the inline calls.

## Files to modify

### Dry-run plan compilation

- `cli/src/add.rs`, `compile_add_steps` `PresentNotLuks` arm (~lines 993-1034): reorder so `CryptsetupLuksAddKeyFile` (when `enroll_key_file` is `Some`) is pushed **before** `CryptsetupLuksHeaderBackup`, and both are pushed before `CryptsetupLuksOpen`.
- `cli/src/replace.rs`, `compile_replace_steps` `PresentNotLuks` arm (~lines 867-908): same reorder.

### Real-run execution

- `cli/src/add.rs:584-597`: move `backup_luks_header(...)` to run after the optional `crate::luks::enroll_key_file(...)` block, and move both before `ensure_luks_open(...)`. The `luks_guard.track(mn.0.clone())` call must stay paired with `ensure_luks_open` (it tracks an opened mapper for rollback) -- keep the current pairing intact.
- `cli/src/replace.rs:305-318`: same reorder. Move `backup_luks_header(...)` to after the optional `crate::luks::enroll_key_file(...)` block, and both before `ensure_luks_open(...)`.

The `eprintln!` lines that announce each step move with their respective calls.

### Tests to update

- `cli/src/replace.rs:2710` `dry_run_render_fresh_disk_live_replace_with_keyfile` -- the only dry-run ordering test that changes. New layout: `lines[1]=luksFormat`, `lines[3]=luksAddKey`, `lines[5]=luksHeaderBackup`, `lines[7]=luksOpen`. Update the four indexed assertions; total line count unchanged.

The following dry-run tests do **not** change because they exercise no-keyfile paths:

- `cli/src/add.rs:3243` `dry_run_render_fresh_single_disk_bootstrap` -- still `format -> backup -> open -> ...`.
- `cli/src/replace.rs:2785` `dry_run_render_missing_path_ordering` -- still `luks_format < header_backup < luks_open < replace_start < resize < soft_balance`.

### Tests to add (regression coverage, both flows, real-run)

Use `MockRunner::requests()` (`cli/src/cmd.rs:985`) to capture the sequence of `CmdRequest`s during real execution, then pin the full fresh-disk LUKS chain. Each test must assert all three orderings together:

```
index(LuksFormat) < index(LuksAddKeyFile) < index(LuksHeaderBackup) < index(LuksOpen)
```

Asserting only the inner pair (`AddKeyFile < HeaderBackup`) would let a future change widen the no-backup window by opening before backing up while still passing.

- `cli/src/add.rs`: new `cmd_add_with_keyfile_orders_format_addkey_backup_open` -- drive `AddPlan::execute` with `enroll_key_file: Some(...)`, locate the four `CmdRequest` indices via `runner.requests()` and assert the full chain above. Use the existing add-execute test scaffolding (the test at line 3366 already drives `AddPlan::execute` against a `MockRunner` with a forced backup failure -- mirror that setup minus the failure).
- `cli/src/replace.rs`: new `cmd_replace_with_keyfile_orders_format_addkey_backup_open` -- same chain against `ReplacePlan::execute`. Mirror an existing replace-execute test as scaffolding.

These two tests are the load-bearing regression coverage for the bug and for the "backup before open" guarantee that keeps the no-backup window narrow.

Add a thin dry-run test for `add.rs` with `--enroll-key-file` (e.g. `dry_run_render_fresh_disk_with_keyfile_orders_backup_after_addkey`) that uses `find("$ cryptsetup luks...")` substring indices to assert `addKey < backup < open`. Pins the dry-run path symmetrically with `replace.rs:dry_run_render_fresh_disk_live_replace_with_keyfile`.

### VM-layer regression (recommended, behavioral)

Extend `tests/cli/braid-add-enroll.py` (~lines 52-93) to dump the post-add header backup and assert slot 1 is present. The backup file is named with the mapper prefix:

```
cryptsetup luksDump --dump-json-metadata /var/lib/braid/luks-headers/braid-disk2.luksheader
```

Assert the JSON contains a key `"1"` under `keyslots` (LUKS2 metadata schema). This catches the regression at the integration boundary and is independent of step-ordering specifics.

A symmetric VM test for `braid replace --enroll-key-file` against a `PresentNotLuks` new disk does not exist today (`tests/cli/replace-preview-warnings.*` is the closest hit but exercises the warning path, not enrollment). The unit-level `cmd_replace_with_keyfile_orders_header_backup_after_addkey` test above is the contract for that flow; a VM test can be added later if the team wants behavioral coverage there too, but it is not gating for this fix.

## Reused utilities

- `Step` and `Step::render_dry_run` (`cli/src/cmd.rs:266-278`): unchanged.
- `backup_luks_header`, `luks_format`, `ensure_luks_open`, `crate::luks::enroll_key_file` (`cli/src/luks.rs`): unchanged; only the call ordering moves.
- `mapper_name` (`cli/src/config.rs:71`): produces `braid-<name>`, used both for the mapper device and the backup filename `<mapper>.luksheader` (`cli/src/luks.rs:341`).
- `MockRunner::requests()` (`cli/src/cmd.rs:985`): the request log used by the new real-run regression tests.
- `compile_enroll_steps` (`cli/src/enroll_key_file.rs:313-354`): reference pattern; do not refactor.

## Verification

1. `just test-rust` -- exercises updated dry-run tests and the two new real-run regression tests (`cmd_add_*` and `cmd_replace_*`). The new tests fail before the fix and pass after.
2. `just test-vm braid-add-enroll` -- exercises the extended VM assertion; fails before the fix (slot 1 missing from backup), passes after.
3. `just test-vm` -- run the full VM suite to catch any unintended interaction with replace / add fast paths and the LUKS guard rollback in `AddPlan::execute`.
4. Manual sanity (optional): in a scratch VM, run `braid add disk2 --enroll /tmp/key`, then `cryptsetup luksDump --dump-json-metadata /var/lib/braid/luks-headers/braid-disk2.luksheader` and confirm both slot `"0"` and slot `"1"` are present.

## Out of scope

- `braid enroll keyfile` ordering (already correct; reused as the reference pattern).
- Existing on-disk backups produced by older braid against `add --enroll-key-file` / `replace --enroll-key-file`: this plan does not refresh them. `braid enroll keyfile` is **not** a recovery path -- `apply_enrollment` (`cli/src/enroll_key_file.rs:257-269`) only runs `luksAddKey` and `backup_luks_header_to` for `NeedsEnroll` disks; on `AlreadyEnrolled` disks it does nothing, and existing tests at `cli/src/enroll_key_file.rs:2096` pin that no-op behavior. Refreshing a stale backup requires a separate explicit header-backup regeneration path, which is not introduced here. braid is unreleased ("No backwards compatibility"), so this is acceptable for the current cohort, but anyone deciding to add a refresh path should treat it as its own follow-up (e.g. a `braid backup-headers` command, or making `enroll keyfile` re-backup `AlreadyEnrolled` disks behind an explicit flag).
- `doctor` / `status` recovery messaging: unchanged. The invariant in `docs/luks-unlock.md` ("never reference local `/var/lib/braid/luks-headers/*.luksheader` paths") is unaffected.
