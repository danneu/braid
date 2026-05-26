# Plan: fail-fast pool-lock docs sync (enroll + lock)

## Context

A review finding flagged that `docs/commands/enroll.md` never documents that
`braid enroll` acquires the pool lock (it also flagged a local header backup --
see "Not in scope"). Investigation (`/verify-issue` + plan review) showed the
gap is a *class*, not an enroll-only miss: command pages that fail fast on
pool-lock contention but don't document it. Two such pages are missing the note.

- **enroll** -- `lock_policy` maps `EnrollKeyFile` (non-dry-run) to
  `NonBlocking`; contention is fail-fast and pinned by
  `tests/module/pool-lock-enroll-contention.py`. enroll.md's `## Safety checks`
  has no pool-lock bullet (the only `NonBlocking` command page missing it).
- **lock** -- regular `braid lock` maps to `LockPlain`. `run_plain_lock`
  (`cli/src/main.rs:1149+`) acquires the stop coordinator, then the pool lock
  non-blocking, refusing fast with "another braid operation is already in
  progress" (exit 1). Pinned by `tests/module/pool-lock-lock-contention.py`.
  `LockPlain` returns `None` from `acquire_per_policy` only because lock owns its
  own acquisition order (stop coordinator first, then pool lock); the
  user-facing behavior is identical fail-fast contention. lock.md's
  `## Error handling` section doesn't mention it.

Both refuse with the same message and exit code, so both pages should carry the
same contention note. While mapping the enroll section, a second adjacent
divergence surfaced: enroll's pending-operation bullet is worded differently
from the siblings that share the journal-exists refusal -- five unconditional
siblings (`add`, `remove`, `remove-missing`, `replace`, `unlock`) plus `discover`
in its `--write` form. (`recover` is excluded: its bullet is the inverse,
"Refuses if no `pending-op.json` exists", because recover consumes the journal.)
Since the goal is the ideal codebase, this plan normalizes that bullet too.

Outcome: a reader of enroll.md or lock.md learns that a concurrent braid
mutation makes the command fail fast, and the contention wording reads
identically across every fail-fast pool-lock command page.

## The change (docs only -- two files)

### A. `docs/commands/enroll.md` -- `## Safety checks` (currently lines 70-78)

Two edits, each matching an established sibling convention: Edit 1 matches the
journal-exists pending-op wording (the five unconditional siblings plus
`discover --write`, excluding `recover`); Edit 2 matches the 7-sibling placement
convention (pool-lock bullet directly follows the pending-op bullet).

**Edit 1 -- normalize the pending-op bullet** (current line 72) to the shared
journal-exists wording used verbatim by `add.md:112`, `remove.md:62`,
`remove-missing.md:79`, `replace.md:117`, `unlock.md:91`:

- Before: `- Refuses if a pending operation exists (recovery mode).`
- After:  `- Refuses if a pending operation journal (\`pending-op.json\`) exists -- run \`braid recover\` to reconcile.`

**Edit 2 -- insert the pool-lock bullet immediately after the pending-op
bullet** (the invariant position across all 7 siblings):

- Insert: `- Refuses if another braid operation is in progress (pool lock \`/run/braid-pool.lock\` is held) -- retry once it finishes.`

Resulting section head:

```
## Safety checks

- Refuses if a pending operation journal (`pending-op.json`) exists -- run `braid recover` to reconcile.
- Refuses if another braid operation is in progress (pool lock `/run/braid-pool.lock` is held) -- retry once it finishes.
- With `--generate`, refuses unless the target directory is already a mount point.
- ... (remaining bullets unchanged)
```

### B. `docs/commands/lock.md` -- `## Error handling` (currently lines 56-60)

lock.md has no `## Safety checks` section; its `## Error handling` section is the
established home for its refusal/failure conditions. Add the contention bullet as
the **first** bullet there (contention is refused before any unmount work begins):

- Insert: `- Refuses if another braid operation is in progress (pool lock \`/run/braid-pool.lock\` is held) -- retry once it finishes.`

Wording is byte-identical to the six unconditional fail-fast siblings. Scope: this
documents the user-facing `braid lock`. The hidden `--systemd-stop` path
(`braid-online.service` ExecStop) has different deadline-poll semantics and stays
undocumented -- it is a hidden flag, not a user-facing invocation.

## Explicitly NOT in scope (deliberate non-changes)

- **Header-backup off-system pointer (enroll step 10).** Stays as-is -- it already
  matches `add.md:86` and `replace.md:89`. Off-system guidance is deliberately
  centralized in `docs/internals/luks-unlock.md` and surfaced at runtime via
  `status`/TUI/`doctor`; the messaging invariant there scopes that to runtime
  recovery/backup-status messages, not reference step-lists. Duplicating it onto
  a command page would diverge from the two sibling commands that do the same
  thing.
- **enroll numbered "What happens under the hood" list.** All siblings keep the
  pool lock out of their step lists; enroll matches by adding the bullet only.
- **Section-heading rename.** The project is split 5 (`Safety checks / refusal
  cases`) vs 4 (plain `Safety checks` -- ack, discover, recover, enroll). enroll
  already matches 4 siblings; unifying the heading is a separate project-wide
  taxonomy decision. No inbound links target the anchor (verified).
- **`ack` bounded-wait contention (noted, distinct gap).** `ack` uses
  `Timeout(10s)` (`lock_policy`: `Ack => Timeout(10s)`): it *waits* up to ten
  seconds for the holder to release, then refuses with the same "another braid
  operation is already in progress" message. The exclusion is purely a
  contract distinction -- ack is bounded-wait, not fail-fast -- so it belongs in a
  separate docs item with timeout-appropriate wording (e.g. "waits briefly for an
  in-progress operation, then refuses") rather than the verbatim "retry once it
  finishes" bullet. The behavior is already well-covered, so that follow-up is a
  low-risk docs-only addition: `tests/module/alert-state-lock.py` ("ack waits then
  fails without mutating alert state"; "ack re-acquires promptly when holder
  releases mid-wait") and `tests/module/pool-lock-precedes-state-read.py` ("ack
  waits then reports contention before broken config"). Flagged, not bundled.

## Critical files

- `docs/commands/enroll.md` -- two-bullet Safety-checks edit.
- `docs/commands/lock.md` -- one-bullet Error-handling edit.
- Reference (read-only): `docs/commands/unlock.md:91-92` (both enroll bullets
  verbatim); any of the six unconditional siblings for the lock bullet wording.

## No code or test changes

Both behaviors already exist and are regression-pinned:
`tests/module/pool-lock-enroll-contention.py` (enroll) and
`tests/module/pool-lock-lock-contention.py` (lock). This is a pure docs/behavior
sync; no Rust, Nix, or VM-test change is needed.

## Verification

1. `mdbook build docs` -- the CI gate (`mdbook-linkcheck`). Expected to pass: the
   edits add no links and change no headings/anchors.
2. enroll parity: diff enroll.md's two new refusal bullets against
   `unlock.md:91-92` -- pending-op and pool-lock bullets should read identically.
3. Cross-page parity grep -- confirm both pages now join the unconditional
   fail-fast siblings:
   `rg -n "Refuses if another braid operation is in progress" docs/commands/`
   should list `enroll.md` and `lock.md` alongside the other six with identical
   text.
