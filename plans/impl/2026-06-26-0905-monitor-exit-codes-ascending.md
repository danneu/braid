# Plan: reorder the `braid monitor` exit-code table to ascending order

## Context

`docs/commands/monitor.md` documents `braid monitor`'s exit codes in a table
ordered **0, 1, 3, 2** (`docs/commands/monitor.md:30-35`). The user-facing table
copies the row order of the canonical table in
`docs/design/decisions/018-systemd-lifecycle.md:99-104`, but without the two
things that justify that order in the ADR: a "Wrapper action" column and a prose
note (`018-systemd-lifecycle.md:107`) explaining that the wrapper routes exit 3 to
the advisory unit *before* the `>= 2` failure branch. Stripped of that rationale, a
reader scanning the command doc sees `3` placed before `2` and reads it as a typo,
and the setup-error code (`2`) is buried in the last row instead of its natural
numeric slot.

This is a readability/consistency defect (Low severity, no behavior change). The
fix is to present the user-facing table in plain ascending numeric order, which is
the universal convention for an exit-code lookup reference and the established
house style for every other braid command doc.

**Why ascending, and why command-doc-only:**

- **House style.** Every other braid command doc with an exit-code table orders it
  ascending: `ack.md` (0,1,2), `idle.md` (0,1,2), `doctor.md` (0,1),
  `seal-mountpoint.md` (0 / non-zero). `monitor.md` is the lone outlier. Both
  `ack.md` and `idle.md` put "Setup error" as the highest code in numeric order --
  exactly the shape `monitor.md` should adopt.
- **No information lost.** The exit-2 row text already reads "Pre-monitor setup
  error (e.g. pool-lock I/O, config load failure)", so its "different category"
  signal survives the move regardless of row position.
- **ADR 018 must NOT change.** Its `0/1/3/2` order is load-bearing: it mirrors the
  wrapper's branch-check order (route Warning/3, then the `>= 2` failure branch),
  and its column + prose exist precisely to justify that order. The two tables
  serve different audiences and are *correctly* different presentations.

## The change

Single file, single table: `docs/commands/monitor.md`. Swap the last two rows so
exit `2` precedes exit `3`. No cell wording changes.

Current (`docs/commands/monitor.md:30-35`):

```
| Code | Meaning |
| --- | --- |
| **0** | Healthy, pool is offline, or another braid command holds the pool lock (cycle skipped, re-evaluated on the next timer tick) |
| **1** | Critical alert active -- a disk-health problem; the beeper fires |
| **3** | Warning-only alert active -- a proactive capacity (ENOSPC) risk; notifies via `alertCommand`, no beep |
| **2** | Pre-monitor setup error (e.g. pool-lock I/O, config load failure) |
```

After:

```
| Code | Meaning |
| --- | --- |
| **0** | Healthy, pool is offline, or another braid command holds the pool lock (cycle skipped, re-evaluated on the next timer tick) |
| **1** | Critical alert active -- a disk-health problem; the beeper fires |
| **2** | Pre-monitor setup error (e.g. pool-lock I/O, config load failure) |
| **3** | Warning-only alert active -- a proactive capacity (ENOSPC) risk; notifies via `alertCommand`, no beep |
```

That is the entire change.

## Out of scope (considered and rejected)

- **Adding an explanatory note instead of reordering.** The finding's alternative
  was to mirror ADR 018's prose ("exit 3 is handled before the `>= 2` branch").
  Rejected: that imports wrapper-internal routing detail into a user doc (wrong
  altitude), and only *explains* the apparent typo rather than removing it.
- **Touching ADR 018 (`018-systemd-lifecycle.md`).** Its order and prose are
  deliberate and correct for its audience -- leave them.
- **Unifying the two tables into a shared `{{#include}}`.** Rejected: they have
  different columns (the ADR adds "Wrapper action"), different orders, and even
  different cell wording for the same codes. They are intentionally distinct
  presentations; one include would degrade both.
- **No code change.** Behavior is unchanged. The exit codes themselves are correct
  and verified against `cli/src/main.rs:939-944` (Warning -> exit 3, Critical/None
  -> exit 1) and `cli/src/main.rs:949` (`load_config_or_exit(.., 2)`, the
  pre-`cmd_monitor` setup path).

## Verification

- **Read the rendered section.** Confirm the table reads 0, 1, 2, 3 top to bottom
  and the exit-2 / exit-3 cell text is byte-for-byte unchanged from before.
- **`just docs-build`.** Confirms the mdBook tree still builds and
  `mdbook-linkcheck2` passes. (No links or Unicode are added, so this is hygiene;
  the reorder cannot break a link.)
- **No test asserts table row order.** This is a pure documentation reorder: no
  Rust/VM test, and no docs check (`check-see-paths.py`, `check-output-ascii.py`,
  `mdbook-linkcheck2`) inspects exit-code-table ordering, so none needs updating.
- **Consistency spot-check.** Diff the four sibling exit tables (`ack.md`,
  `idle.md`, `doctor.md`, `seal-mountpoint.md`) mentally against the edited
  `monitor.md`: all five now read in ascending numeric order.

## Commit

Conventional Commit, lowercase first line, e.g.:

```
docs(monitor): order exit-code table ascending
```
