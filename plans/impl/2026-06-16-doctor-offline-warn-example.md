# Plan: illustrate the declared_disks offline-warn path in doctor.md

## Context

`docs/commands/doctor.md` is the canonical reference for `braid doctor`. Its
"Basic example" section shows an all-ok output snapshot, and separately
illustrates exactly one *warn* variant -- the SMART self-test warn -- as a short
fenced block with explanatory prose (`docs/commands/doctor.md:47-53`).

Commit `2796fbd4` ("fix(doctor): warn on offline declared disks") taught the
`declared_disks` check to cross-check each identity-verified member against live
btrfs assembly and *warn* when a member is present and LUKS-verified but absent
from the mounted device set (a degraded mount or interrupted reconciliation).
That commit updated the authoritative table row (`docs/commands/doctor.md:78`)
but touched only those two lines -- the example block was never updated. As a
result the headline example still shows `declared disks` purely as a presence
check (`all 3 declared disks present`), with no illustration of the new
live-pool cross-check. A reader skimming the example can miss that doctor now
warns on an offline member.

This is doc drift, not a behavior bug. The fix is purely additive: add one
illustrative offline-warn snippet to the example section, mirroring the existing
SMART warn precedent, so the example matches the already-correct table row.

## Source of truth (the wording must match the code exactly)

The warn message is produced by `summarize_declared_disks`
(`cli/src/doctor.rs:412-534`). For one offline member of three the rendered
message is, literally:

```
1/3 disks have problems: 1 present but not in the live pool: disk2 (/dev/disk/by-id/...)
```

- aggregate prefix `N/M disks have problems:` -- `cli/src/doctor.rs:509-516`
- offline part `K present but not in the live pool: <name> (<by_id>)` --
  `cli/src/doctor.rs:494-499`
- the offline classification (LUKS-verified but not in the live device set) --
  `reconcile_with_live_pool`, `cli/src/doctor.rs:387-400`

The literal `present but not in the live pool` substring is pinned by the unit
test `summarize_warn_offline_member_not_in_live_pool`
(`cli/src/doctor.rs:3613-3631`), so the example wording is anchored in code.

## Change

Single file: `docs/commands/doctor.md`. Additive only -- no existing lines
change.

Insert a new illustration **after** the SMART self-test by-id note paragraph
(current `docs/commands/doctor.md:53`) and **before** "To test the real alert
sound:" (current line 55). This placement keeps the existing snapshot ->
SMART-prose adjacency intact (the snapshot shows the three `smart selftest`
rows, so that explanation belongs directly under it) and appends the
declared-disks warn as a second, parallel illustration.

Text to insert is shown below inside a four-backtick fence; the literal bytes to
add to `doctor.md` are everything between the outer ```` ```` ```` markers --
a prose paragraph followed by a normal three-backtick fenced block, all pure
ASCII (no zero-width or other non-ASCII characters):

````markdown
When the pool is mounted, the `declared disks` check also confirms each member
is assembled into the live btrfs pool. A member that passes its LUKS identity
checks but is missing from the live device set -- e.g. after a degraded mount or
an interrupted reconciliation -- warns as `offline` rather than counting as
present:

```
[warn] declared disks  1/3 disks have problems: 1 present but not in the live pool: disk2 (/dev/disk/by-id/...)
```
````

Notes for the implementer:
- Use a 3-disk framing (`1/3`, `disk2`) so it stays continuous with the all-ok
  example's "all 3 declared disks present" pool.
- Row prefix is `[warn] declared disks  ` (two spaces before the message),
  matching the column alignment of the example block (`[ok]   ` and `[warn] `
  are both 7 chars wide).
- The abbreviated by-id path `(/dev/disk/by-id/...)` mirrors the SMART snippet's
  abbreviation style; the real message prints the member's full persisted by-id
  path in that parenthetical.
- The inserted block must be pure ASCII (the repo convention; `--` not an
  em-dash, plain `'`/`"`). After editing, confirm with
  `LC_ALL=C grep -nP '[^\x00-\x7F]' docs/commands/doctor.md` returning no hits on
  the new lines.

## Out of scope (verified, do not touch)

- **Table row** `docs/commands/doctor.md:78` -- already documents the
  offline/UUID-mismatch/topology-unavailable semantics correctly. No change.
- **`README.md` and the guides** -- `docs/commands/doctor.md` is the only file
  that shows `braid doctor` output; README only lists the command and the guides
  show no declared-disks output. No sync needed.
- **The all-ok snapshot block** itself -- do *not* inject a `[warn]` line into
  it. It is a single coherent snapshot of one healthy 3-disk pool; a warn row
  inside it would contradict the `[ok] declared disks` row. The new illustration
  is a separate fenced block, exactly as the SMART warn is.

## Verification

- `python3 scripts/docs/check-doctor-table-parity.py` -- confirms the doctor
  table still matches the registered checks (unaffected by the example-block
  edit, but cheap to confirm it stays green).
- `just docs-build` -- builds the mdBook and runs `mdbook-linkcheck2`; confirms
  no markdown/link breakage (the change adds no links).
- `LC_ALL=C grep -nP '[^\x00-\x7F]' docs/commands/doctor.md` -- confirms the
  inserted lines added no non-ASCII bytes.
- Confirm fidelity: the inserted message string must be a substring-faithful
  render of `summarize_declared_disks` (`cli/src/doctor.rs:494-516`); cross-check
  against the pinned unit test `summarize_warn_offline_member_not_in_live_pool`
  (`cli/src/doctor.rs:3613-3631`).
- Eyeball the rendered "Basic example" section: healthy snapshot -> SMART warn
  illustration -> new declared-disks offline-warn illustration -> `--beep`.
