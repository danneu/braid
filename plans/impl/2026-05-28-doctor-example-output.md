# Doctor example output: bring it in line with `run_doctor`

## Context

The illustrative output block at `docs/commands/doctor.md:21-34` shows 12
rows, but `run_doctor` (`cli/src/doctor.rs:1480-1497`) registers 16+ rows
that always render -- they may skip, but they always emit a line.

The finding flagged three missing rows (`foreign_luks_uuid`, `ups_daemon`,
`braid_online_active`). Investigation showed three more are also missing:
`enospc_risk`, `system_profile_mismatch`, and `paused_balance`. The narrow
fix would still leave the example diverging from reality, so the pivot is
to make the example reflect a complete real run, in the order
`format_doctor_human_with` prints.

The "What it checks" table at `docs/commands/doctor.md:60-77` was already
updated to cover every registered check (commit `e5c82df docs(doctor):
sync check table with registered checks`); only the example block was
left behind.

## Scope

- Single file: `docs/commands/doctor.md`.
- Doc-only change. No code, no tests, no commands modified.
- The "What it checks" table, "What happens under the hood" prose, and
  the warn-row callout below the example are already correct -- do not
  touch them.

## The example, after

Replace the contents of the fenced block at
`docs/commands/doctor.md:21-34` with the rows below, in this exact
order. Tags, labels, and messages are copied from the
`CheckResult::ok`/`::skip` literals at:

- `pool_missing_devices` ok: `cli/src/doctor.rs:746`
- `enospc_risk` ok: `cli/src/doctor.rs:830`
- `foreign_luks_uuid` ok: `cli/src/doctor.rs:896`
- `data_profile_mismatch` / `metadata_profile_mismatch` /
  `system_profile_mismatch` ok: `cli/src/doctor.rs:693`
  (via `check_profile_mismatch`)
- `metadata_enospc_pressure` ok: `cli/src/doctor.rs:1047`
- `paused_balance` ok: `cli/src/doctor.rs:858`
- `beep_path` skip: `cli/src/doctor.rs:1411`
- `ups_daemon` skip: `cli/src/doctor.rs:1239`
- `braid_online_active` skip: `cli/src/doctor.rs:1289`

Labels come from the match in `format_doctor_human_with`
(`cli/src/doctor.rs:1521-1542`). Column spacing matches the existing
block: tag column padded to 7 chars (`[ok]   ` / `[skip] `), label
column padded to 14 chars, then a two-space gap before the message.

Happy-path scenario: healthy mounted 3-disk pool, no UPS configured,
beep monitoring on but `--beep` not passed.

```
[ok]   config file     /etc/braid/config.json exists and is valid JSON
[ok]   config schema   required fields present and valid
[ok]   config perms    /etc/braid/config.json permissions ok
[ok]   declared disks  all 3 declared disks present
[ok]   missing devs    no missing devices
[ok]   enospc risk     per-device unallocated space healthy
[ok]   foreign uuids   no foreign LUKS UUIDs in live pool
[ok]   data profiles   data profile: RAID1
[ok]   meta profiles   metadata profile: RAID1
[ok]   system profiles  system profile: RAID1
[ok]   meta pressure   metadata pressure within bounds
[ok]   paused balance  no paused balance
[ok]   smart selftest disk1  passed ~2 days ago
[ok]   smart selftest disk2  passed ~12 days ago
[ok]   smart selftest disk3  passed ~30 days ago
[skip] alert beep      skipped (pass --beep to play the audible alert test beep)
[skip] ups daemon      skipped (braid.ups not enabled)
[skip] braid-online    skipped (braid.ups not enabled)
```

Note: `system profiles` is 15 chars, one wider than the 14-char column
floor, so its message column slips by one space. This matches what real
output does -- the formatter uses `:<14` (`cli/src/doctor.rs:1550`)
which pads short labels but does not truncate long ones. Keep the extra
space; do not contort the example for alignment.

## What not to do

- Do not invent additional warn/fail rows for the example -- the
  warn-row callout immediately below the block (`docs/commands/doctor.md:36-40`)
  already demonstrates that path with `smart selftest disk2`.
- Do not reorder rows. Output order is set by `run_doctor`'s
  registration list and `check_smart_selftests` extension, and the
  table prose above already lists checks in that same order.
- Do not change the label widths, tag widths, or message wording. The
  example's value is that a reader can `diff` it against their own
  `braid doctor` output and trust it.

## Verification

- Visual: re-read the block side-by-side with `run_doctor`'s checks vec
  (`cli/src/doctor.rs:1480-1497`) and the label match
  (`cli/src/doctor.rs:1521-1542`); every registered check name maps to
  exactly one row in the example, in declaration order.
- Build the book: `mdbook build docs` (per `AGENTS.md` -- broken cross-
  links fail CI). The example is plain text in a fenced block, so this
  only checks that no surrounding markdown structure was broken.
- Optional ground-truth check: on a NixOS VM test machine with a
  healthy 3-disk pool and `braid.ups` disabled, run `sudo braid doctor`
  and confirm the output matches line-for-line modulo the smart-selftest
  ages and the disk count.

No Rust tests are added or modified -- this change does not touch code
and there is no existing golden-output test for the formatter's full
happy-path layout to extend. If we ever want one, that is a separate
plan.
