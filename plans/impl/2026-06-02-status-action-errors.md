# Plan: document `braid status` per-disk `Action:` / `Errors: unknown` lines

## Context

`braid status` prints, for each disk that needs attention, an `Action:` line
naming the next command to run, plus an `Errors: unknown (<reason>)` line for
every disk that is not a live present member. The "Per-disk detail" section of
`docs/commands/status.md` documents neither. Worse, its single `MISSING`
example is itself inaccurate: it stops at the `Device:` line, omitting the
`Errors: unknown (device absent)` and `Action: braid replace ...` lines the
code actually emits. A reader hitting a degraded or swapped disk sees output
the page never describes and cannot tell that status already hands them the
remediation command.

This is a Completeness fix in the same spirit as today's commit `e5c69b82`
("align errors example with status output"), which corrected the *present*-disk
`Errors:` rendering in this exact section. Outcome: the page shows the real
degraded-disk output and documents the full `Action:` / `Errors: unknown`
vocabulary, cross-linked to the commands that perform the repair.

Docs-only change. No code, no tests, no behavior change.

## Scope

Single file: `docs/commands/status.md`. Three touch points:

- the `### Per-disk detail` lead-in sentence (status.md:142) and example block
  (status.md:144-156) -- Edit A;
- new `Errors:` / `Action:` reference text after the "Disk states" table
  (status.md:158-167) -- Edit B;
- the "Related commands" list (status.md:391-396) -- Edit C.

`README.md` is cookbook-style and carries only the bare `sudo braid status`
invocation, not a per-disk output block, so the AGENTS.md "keep both in sync"
rule needs no README edit.

## Ground truth (diff the new text against these; do not paraphrase from memory)

All literal strings come from `cli/src/status.rs#format_status_human`:

- Per-disk header line per state (status.rs:1387-1407): `present` renders
  `<name> devid <n> present`; the rest render the bare label `MISSING`,
  `OFFLINE`, `LUKS HEADER UNREADABLE`, `LUKS UUID MISMATCH`, `UNKNOWN`.
- `Errors:` block (status.rs:1429-1459): present member ->
  `read N / write N / flush N / corruption N / generation N`; every other
  state -> `unknown (<reason>)` with reason one of `device absent`,
  `LUKS header unreadable`, `LUKS UUID mismatch`,
  `disk offline -- not in pool`, `metadata unavailable`.
- `Action:` block (status.rs:1461-1483). Emitted only for:
  - missing member, or present member with nonzero counters, *with* a
    membership match -> `repair_hint::missing_replace_command(Some(name))`,
    i.e. `braid replace --old <name> --new <new-name>=/dev/disk/by-id/<...>`
    (verbatim shape in `cli/src/repair_hint.rs#missing_replace_command`).
  - same trigger but *no* membership match (foreign mapper) ->
    `foreign mapper detected -- run 'braid doctor' to investigate`.
  - `LUKS UUID MISMATCH` -> `luks::luks_uuid_mismatch_guidance()` +
    ` -- run 'braid doctor' for the expected vs observed UUID`.
  - `LUKS HEADER UNREADABLE` -> `run 'braid doctor' for recovery guidance`.
  - `OFFLINE`, `UNKNOWN`, and healthy present disks get NO `Action:` line.

- `LUKS:` line (status.rs:1425, `if !d.luks_uuid.is_empty()`): shown for any
  disk with a non-empty `luks_uuid`. Among non-live disks, only the
  `LUKS UUID MISMATCH` row carries one -- the observed on-disk UUID
  (status.rs:1078-1079); missing/offline/header-unreadable/unknown leave it
  blank (status.rs:1067/1084/1087/1110) and render no `LUKS:` line. The page's
  own JSON-field doc (status.md:266-267) already states this for JSON. So the
  live-member vs not split -- not present vs non-present -- is the right axis
  for the lead-in (offline/mismatch/header-unreadable disks are device-present;
  see the states table at status.md:164-166).

Confirmed not gated by any flag: `StatusArgs` exposes only `--json`
(no `--verbose`); the `Disks:` detail always renders for a mounted pool
(`cli/src/status.rs#cmd_status`). So the section needs no verbosity caveat.

## Edits

### Edit A -- rewrite the lead-in sentence and example block (status.md:142-156)

First, replace the lead-in sentence (status.md:142), which currently reads
"Each disk shows its device path, model, serial, LUKS UUID, and I/O error
counts:" -- it describes only the present-disk case and is contradicted by the
degraded disks in the example directly below it. New lead-in:

> What each disk shows depends on whether it is a live pool member. A live pool
> member shows its device path, model, serial, LUKS UUID, and btrfs I/O error
> counters. Any other disk -- missing, offline, UUID mismatch,
> header-unreadable, or unknown -- shows a reduced set: its device path and an
> `Errors: unknown (<reason>)` line in place of counters; a UUID-mismatch disk
> also shows its observed `LUKS:` UUID so the divergence is visible. Separately,
> any disk that needs attention -- for example a missing disk, or a present
> member with nonzero error counters -- gets an `Action:` line naming the next
> command (detailed below).

Then replace the example block. Show three disks so every distinct rendering
appears once: healthy present (zero counters, no `Action:`), errored present
(nonzero counters + replace `Action:` -- the least predictable case, since a
*present* disk still gets an action), and missing (the `unknown (...)` form +
replace `Action:`). Use a distinct `by-id` suffix per disk.

```
Disks:

  toshiba1          devid 1   present
    Device:  /dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234
    Model:   TOSHIBA MN07ACA12T
    Serial:  1234ABC
    LUKS:    aaaaaaaa-1111-2222-3333-444444444444
    Errors:  read 0 / write 0 / flush 0 / corruption 0 / generation 0

  toshiba2          devid 2   present
    Device:  /dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_5678
    Model:   TOSHIBA MN07ACA12T
    Serial:  5678DEF
    LUKS:    bbbbbbbb-1111-2222-3333-444444444444
    Errors:  read 12 / write 0 / flush 0 / corruption 3 / generation 0
    Action:  braid replace --old toshiba2 --new <new-name>=/dev/disk/by-id/<...>

  toshiba3          MISSING
    Device:  /dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_9ABC  (not found)
    Errors:  unknown (device absent)
    Action:  braid replace --old toshiba3 --new <new-name>=/dev/disk/by-id/<...>
```

### Edit B -- add `Errors:` and `Action:` reference after the existing "Disk states" table (status.md:167)

Keep the existing `| State | Meaning |` table as the header-label reference.
Append two short blocks. The `Action:` table is keyed by *condition*, not
state, because the action does not map 1:1 to a state (an errored present disk
gets one; a missing disk with no membership gets the foreign-mapper variant).

```markdown
**`Errors:` line.** A live, present pool member shows real btrfs counters
(`read / write / flush / corruption / generation`). Every other disk shows
`Errors: unknown (<reason>)`, where `<reason>` names why counters are
unavailable: `device absent`, `LUKS header unreadable`, `LUKS UUID mismatch`,
`disk offline -- not in pool`, or `metadata unavailable`.

**`Action:` line.** When a disk needs attention, `braid status` appends an
`Action:` line naming the next command, so you do not have to look it up:

| Condition | `Action:` line |
|---|---|
| Missing member, or a present member with nonzero error counts | `braid replace --old <name> --new <new-name>=/dev/disk/by-id/<...>` |
| Missing or errored device with no pool membership (foreign mapper) | `foreign mapper detected -- run 'braid doctor' to investigate` |
| LUKS UUID mismatch | `disk was swapped, cloned, or reformatted; detach the foreign disk and reattach the original, or run 'braid replace' if the swap was intentional -- run 'braid doctor' for the expected vs observed UUID` |
| LUKS header unreadable | `run 'braid doctor' for recovery guidance` |

Healthy present disks and disks in the `OFFLINE` or `UNKNOWN` state get no
`Action:` line. These hints are human-output only; `--json` consumers derive
their own remediation from the `status` and `errors` fields (the JSON
`disks[]` element has no action field).

See [braid replace](replace.md) to rebuild a missing or failing disk and
[braid doctor](doctor.md) for the guided recovery path.
```

All four `Action:` cells are verbatim copies of the emitted line -- no
ellipsis. The mismatch cell is long, but the page already carries comparably
long table cells (the Profile "single, RAID1" row at status.md:68), and the
clause an ellipsis would drop is the actual remediation the `Action:` line
exists to surface, so render it in full. The mismatch text is
`luks::luks_uuid_mismatch_guidance()` (`cli/src/luks.rs`) plus the
` -- run 'braid doctor' for the expected vs observed UUID` suffix from
status.rs:1477.

### Edit C -- add `braid doctor` to "Related commands" (status.md:391-396)

After Edit B the page references `braid doctor` three times (the Profile
"single, RAID1" row at status.md:68, the `LUKS UUID MISMATCH` states-table row
at status.md:166, and Edit B's `Action:` section with the first actual
`[braid doctor](doctor.md)` link), yet doctor is absent from the command nav
while the less-referenced `idle` is present. Add one line to the existing list,
grouped with the repair commands (after `remove-missing`, before `idle`):

```markdown
- [braid doctor](doctor.md) -- diagnose pool/disk health and get recovery guidance
```

## Out of scope (do not do)

- No `--verbose` caveat -- the section is not flag-gated.
- No restructuring of the existing "Disk states" table -- it stays as the
  header-label reference; the new `Action:` table is the condition reference.
- No new JSON field -- `Action:` is a human-output convenience, correctly
  absent from JSON.
- No README change.
- No links into `docs/internals/` -- keep the cross-links at operator-facing
  altitude. `replace.md` and `doctor.md` are existing sibling pages, so the
  links resolve; `replace.md` is already in this page's "Related commands",
  but `doctor.md` is not -- Edit C adds it.

## Verification

- `mdbook build docs` -- exercises `mdbook-linkcheck2`; confirms the new
  `replace.md` / `doctor.md` relative links resolve (a broken cross-link fails
  the build). This is the only automated gate for the change.
- Diff-check each literal in Edits A/B against the format strings in
  `cli/src/status.rs` (Errors block 1429-1459, Action block 1461-1483) and
  `cli/src/repair_hint.rs#missing_replace_command` -- every quoted line must be
  copy-exact, including the `--`-not-em-dash CLI style.
- No `cargo test` / VM tests: docs-only, no code path touched.
- Optional manual confirmation (not required for merge): on a scratch VM pool,
  hot-unplug a member and run `sudo braid status`; the rendered `MISSING`
  block should match Edit A line-for-line.
