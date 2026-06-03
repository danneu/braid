# Fix: render concrete per-mapper remediation in `foreign_luks_uuid` doctor check

_Date: 2026-06-03_

## Context

`braid doctor`'s `foreign_luks_uuid` check (`cli/src/doctor.rs#check_foreign_luks_uuid`)
**Fail**s when the live mounted btrfs pool contains a LUKS device whose UUID is
not declared in `pool.json` -- a foreign disk force-added outside braid. The
Fail message body names each foreign mapper concretely (`{uuid} at mapper
braid-stranger`), but the trailing remediation hint uses a literal `<mapper>`
placeholder:

> ... -- restore with 'btrfs device remove /dev/mapper/**\<mapper\>** /mnt/storage' then 'cryptsetup close **\<mapper\>**'

The system already knows every foreign mapper name -- `foreign` is a
`BTreeMap<LuksUuid, MapperName>` returned by `cli/src/membership.rs#foreign_luks_uuids`.
Printing `<mapper>` forces the operator to mentally substitute the real name
into a destructive recovery command (`btrfs device remove` + `cryptsetup
close`), in a high-stakes manual-recovery scenario, on a **Fail** message.

This is the exact anti-pattern braid avoids elsewhere: the SMART check's
operator hint substitutes the concrete device (`smartctl -t short {by_id}`,
`cli/src/doctor.rs#smart_selftest_hint`; the sibling `summarize_smart_selftest`
fail branch likewise emits a concrete `smartctl -l selftest {by_id}`), and
`cli/src/repair_hint.rs` exists explicitly "so operator hints do not drift",
shipping a concrete-when-known form
(`repair_hint::missing_replace_command_with_devid`).
Placeholders in braid are reserved for values the system genuinely cannot know
(e.g. the *new* replacement disk in `check_pool_missing_devices`). A foreign
mapper is always known, so the placeholder is unjustified.

**Outcome:** every foreign mapper gets a fully paste-ready remove+close recipe,
in both the common single-foreign case and the rare multi-foreign case. The
`<mapper>` placeholder is removed from the codebase entirely.

## Approach (Option A -- concrete for all, no placeholder)

Build one self-contained diagnosis+recipe unit per foreign mapper and join
them, instead of a shared trailing clause with a placeholder. This deletes the
placeholder, needs no `len() == 1` branch, collapses the old two parallel
`Vec`s into one, and pairs each foreign UUID with its own paste-ready recovery
(so the rare multi-foreign tail reads as N independent recoveries, not one long
sequence).

### 1. Code -- `cli/src/doctor.rs#check_foreign_luks_uuid`

Replace the single-clause remediation in the `CheckResult::fail` construction.
Representative shape (final tail of the function):

```rust
    let n = foreign.len();
    let mp = ctx.config.as_ref().unwrap().mount_point();
    // Pair each foreign mapper's diagnosis with its own paste-ready
    // remove+close recipe, so multi-foreign output reads as N independent
    // recoveries rather than one long sequence. Every mapper was observed
    // live, so no `<mapper>` placeholder is needed.
    let recoveries: Vec<String> = foreign
        .iter()
        .map(|(uuid, mapper)| {
            format!(
                "{uuid} at mapper {mapper} -- restore with 'btrfs device remove /dev/mapper/{mapper} {mp}' then 'cryptsetup close {mapper}'"
            )
        })
        .collect();
    CheckResult::fail(
        NAME,
        format!(
            "{n} foreign LUKS UUID{plural} in live pool: {body}",
            plural = if n == 1 { "" } else { "s" },
            body = recoveries.join("; "),
        ),
    )
```

Notes:
- `mount_point()` returns `&MountPoint` (Display); hoist it to a local `mp` so
  the per-mapper closure can reuse it. No borrow conflict -- `pool`'s borrow
  ends at its last use (the `foreign_luks_uuids` call) and `mp` borrows a
  different `ctx` field (`ctx.config`).
- `MapperName` and `LuksUuid` both implement `Display`
  (`cli/src/types.rs`), so interpolation is direct.
- Iteration order is by `LuksUuid` (the `BTreeMap` key), so tests must assert
  per-mapper substring presence, not positional order.
- Rendering: each unit is `<uuid> at mapper <mapper> -- restore with 'btrfs
  device remove /dev/mapper/<mapper> <mount>' then 'cryptsetup close <mapper>'`,
  units joined by `; `. Single-foreign renders byte-identical to the prior
  single-clause form (`1 foreign LUKS UUID in live pool: <uuid> at mapper
  braid-stranger -- restore with 'btrfs device remove
  /dev/mapper/braid-stranger /mnt/storage' then 'cryptsetup close
  braid-stranger'`); multi simply concatenates such units.
- **Do not** route through `repair_hint`: this is the only callsite that builds
  the btrfs+cryptsetup pair, so there is no cross-callsite drift to centralize
  away. Keep it inline.

### 2. Doc -- `docs/commands/doctor.md`

Update the `foreign_luks_uuid` row in the checks table (currently describes the
`<mapper>` placeholder form). New wording, e.g.:

> **Fail** when the live (mounted) pool contains a btrfs device whose LUKS UUID
> is not declared in `pool.json` (a foreign disk). The message pairs each
> foreign UUID and its mapper with a paste-ready `btrfs device remove
> /dev/mapper/<mapper> <mount>` then `cryptsetup close <mapper>` recipe -- the
> observed mapper name and pool mount point are substituted in, and multiple
> foreign disks each get their own recipe. Skipped when the pool is not mounted.

### 3. Tests

Behavioral, structure-insensitive assertions only (the operator-facing
guarantee: the real mapper name appears in the command; no placeholder leaks).

- **Strengthen** `cli/src/doctor.rs#check_foreign_luks_uuid_fails_when_pool_has_unknown_uuid`
  (single-foreign): add assertions that the message contains the concrete
  `btrfs device remove /dev/mapper/braid-stranger` and `cryptsetup close
  braid-stranger`, and does **not** contain `<mapper>`. (Existing needle/order
  assertions still hold.)
- **Add** a multi-foreign unit test (e.g.
  `check_foreign_luks_uuid_emits_concrete_command_per_foreign_mapper`): model
  one known member plus two foreign mappers via `pool_state_runner` (add a
  third device tuple `("braid-other", 3, "/dev/vdd", <2nd foreign uuid>)`
  alongside the existing `braid-stranger`), membership declaring only the known
  member. Assert status `Fail`; the pluralized `2 foreign LUKS UUIDs` (with the
  trailing `s` -- `2 foreign LUKS UUID` is a prefix of the plural and would not
  prove the `s` rendered); concrete remove+close commands for **both**
  `braid-stranger` and `braid-other`; per-mapper ordering (each mapper's own
  `btrfs device remove .../<mapper>` precedes its own `cryptsetup close
  <mapper>`, which the interleaved form guarantees); and no `<mapper>`. This is
  the first multi-foreign coverage in the suite and locks the Option A
  contract.
- **Strengthen** the VM test `tests/cli/braid-doctor-foreign-luks-uuid.py`: add
  `btrfs device remove /dev/mapper/braid-stranger` to the needle list and
  assert `<mapper>` is absent, locking the concrete form end-to-end. (Existing
  `braid-stranger` + remove-before-close assertions still hold.)

## Critical files

- `cli/src/doctor.rs#check_foreign_luks_uuid` -- the fix.
- `cli/src/doctor.rs#check_foreign_luks_uuid_fails_when_pool_has_unknown_uuid` -- strengthen; add sibling multi-foreign test.
- `tests/cli/braid-doctor-foreign-luks-uuid.py` -- strengthen needles.
- `docs/commands/doctor.md` -- update the `foreign_luks_uuid` checks-table row.

## Reuse / no-touch notes

- Reuse `cli/src/membership.rs#foreign_luks_uuids` (already the read-only source
  of the `BTreeMap<LuksUuid, MapperName>`); no membership changes.
- `cli/src/status.rs` only points operators at doctor
  (`"foreign mapper detected -- run 'braid doctor' to investigate"`) and does
  **not** duplicate the recipe -- no status/TUI change needed.
- No parser/tool-output surface is touched, so **no fixture refresh** is
  required.
- Per repo policy, make narrow hand edits; do not run `cargo fmt`/formatters.

## Verification

1. `just test-rust` -- runs the strengthened single-foreign test and the new
   multi-foreign test (and the rest of the doctor unit suite).
2. `just test-vm braid-doctor-foreign-luks-uuid` -- focused VM run proving the
   concrete recipe end-to-end (blast radius is a single check, so a focused run
   is appropriate; no full-suite run needed).
3. Manual eyeball of the rendered Fail message for the single case:
   `... -- restore with 'btrfs device remove /dev/mapper/braid-stranger
   /mnt/storage' then 'cryptsetup close braid-stranger'` (no `<mapper>`).
