# Plan: Split conflated pool-lock vs pending-op refusal bullets across command manuals

## Context

Users who hit the wrapper-level error
`braid: another braid operation is already in progress (pool lock /run/braid-pool.lock is held); retry once it finishes`
(emitted from `modules/braid/braid-wrapper.sh:53-59,62-70` when the non-blocking
`flock -n` fails) cannot find that wording in `manual/commands/*.md`. The
manuals collapse two genuinely different refusal cases into a single bullet
or, in `recover.md`, omit the pool-lock case entirely:

| Refusal | Source | Remediation |
| --- | --- | --- |
| Wrapper-level `flock -n` failure on `/run/braid-pool.lock` | `modules/braid/braid-wrapper.sh:57` | Transient -- retry once the holding command finishes. The kernel releases the lock when its holder exits. |
| CLI-level pending-op journal preflight | `cli/src/preflight.rs:44-57` -- error string `interrupted operation detected (pending-op.json exists, started ...)` | Persistent -- run `braid recover` to reconcile. |

The wrong bullet sends the user to the wrong remediation. A user whose
sibling `braid remove` is still running would, reading `remove.md:62`
("Refuses if another braid operation is pending"), wrongly conclude that
`braid recover` is the fix. The reverse confusion is also possible.

`manual/commands/discover.md:71,73` already splits these into two clear
bullets, added in commit `905e9ca fix(discover): serialize discovery with
pool lock` -- but the same update was not back-propagated to the older
wrapper-locked manuals. The result is inconsistency across the six other
pages that share the wrapper-lock acquisition list at
`modules/braid/braid-wrapper.sh:53`.

Goal: every wrapper-locked command manual lists the same two bullets, in
the same wording, in the same order, with self-contained remediation
hints so a user reading just the bullet knows what to do.

Out of scope: `ack` (acquires the lock with a 10s blocking wait, not
non-blocking -- different semantics; its manual currently lists no
lock/pending bullets and a separate decision); `enroll` (does not acquire
the wrapper lock -- correctly does not need the pool-lock bullet);
`monitor` (non-blocking silent exit 0 on contention, never user-visible).

## Canonical wording

Use these two bullets verbatim everywhere they apply:

```
- Refuses if a pending operation journal (`pending-op.json`) exists -- run `braid recover` to reconcile.
- Refuses if another braid operation is in progress (`/run/braid-pool.lock` is held by another wrapper) -- retry once it finishes.
```

Rationale for divergence from current `discover.md` text:

- Adds the inline remediation hint (`run braid recover`, `retry once it
  finishes`) so the bullet is self-contained -- this is the whole point
  of the fix.
- Names `pending-op.json` parenthetically so users grepping for the
  error string land on the right bullet.
- Mirrors the wrapper's actual user-facing phrasing ("retry once it
  finishes" verbatim from `braid-wrapper.sh:57,66`).

`discover.md` also gets rewritten to match so the canonical wording is
truly canonical.

## Files to edit (manual only -- no code, no tests)

1. **`manual/commands/discover.md:71,73`** -- replace both existing
   bullets with the canonical pair so this page becomes the
   reference. Order: pending-op bullet first, then pool-lock bullet.

2. **`manual/commands/add.md:112`** -- replace
   `Refuses to proceed if another braid operation is pending (pending-op.json exists)`
   with the canonical pair. Keep the `btrfs exclusive operation` bullet
   that follows.

3. **`manual/commands/remove.md:62`** -- replace
   `Refuses if another braid operation is pending` with the canonical
   pair.

4. **`manual/commands/remove-missing.md:79`** -- replace
   `Refuses if another braid operation is pending` with the canonical
   pair.

5. **`manual/commands/replace.md:115`** -- replace
   `Refuses if another braid operation is pending` with the canonical
   pair.

6. **`manual/commands/unlock.md:85`** -- replace
   `Refuses if another braid operation is pending` with the canonical
   pair. Note: line 63 (`Checks that no other braid operation is
   pending`) is in the "What happens under the hood" section and refers
   only to the CLI-level pending-op preflight -- leave it as-is; it is
   describing internal behavior, not the user-facing refusal.

7. **`manual/commands/recover.md`** -- the page currently has
   `Refuses if no pending-op.json exists.` at line 85 (the inverse --
   correct, leave it). Add a new bullet immediately after for the pool
   lock: `Refuses if another braid operation is in progress
   (`/run/braid-pool.lock` is held by another wrapper) -- retry once
   it finishes.` `recover` does acquire the wrapper lock per
   `modules/braid/braid-wrapper.sh:53`, so the case is real.

No edits to:

- `manual/commands/enroll.md` -- does not acquire wrapper lock.
- `manual/commands/ack.md` -- different lock semantics (10s blocking).
- `manual/commands/monitor.md` -- silent exit on contention.
- `manual/commands/status.md`, `lock.md`, `doctor.md`, `tui.md`,
  `ups-status.md`, `idle.md` -- do not acquire the wrapper lock.
- `manual/guides/troubleshooting.md:132-144` -- already documents the
  pool-lock symptom correctly. No cross-link is added from the command
  pages because the new bullets are self-contained.
- Any Rust source (`cli/src/preflight.rs`, `cli/src/main.rs`, etc.) --
  Explore confirmed no clap `#[command]`/`#[arg]` help text duplicates
  these refusal-list claims, so the manual is the only surface that
  needs to change.
- `modules/braid/braid-wrapper.sh` -- error string is already
  user-clear; no need to alter behavior.

## Verification

This is a documentation-only change. No automated tests cover manual
prose. Verification is by inspection:

1. `git diff manual/commands/` -- confirm exactly 7 files changed and
   the two-bullet block is byte-identical across all of them (a `grep
   -A1 "Refuses if a pending operation journal" manual/commands/`
   should show the same two lines in each file).
2. Walk through each of the 7 manuals and confirm the surrounding
   "Safety checks" / "Safety checks / refusal cases" list still reads
   coherently (the bullets are inserted in roughly the original
   position; ordering relative to the `btrfs exclusive operation`
   bullet should remain consistent -- the two new bullets immediately
   precede the btrfs-exclop bullet where one exists).
3. Sanity check that the wording in each new bullet is byte-identical
   to the wrapper's user-facing error string for the lock case
   (`/run/braid-pool.lock`, "retry once it finishes") and to the
   CLI's user-facing error string for the pending-op case
   (`pending-op.json`, "Run 'braid recover'"). The bullet does not
   need to quote the full sentence -- it needs to share enough tokens
   that a user who pasted the error into a search would land on the
   bullet.
4. No test or build command is required; the manual is plain markdown
   not consumed by build tooling. `just test-rust` and `just test-vm`
   remain green by definition since no code changes.

## Commit shape

Single commit, conventional-commit style:

```
docs(manual): split pool-lock and pending-op refusals
```

Body: explain that the wrapper-level flock failure and the CLI-level
pending-op preflight are two distinct refusal cases with different
remediations, that `discover.md` already split them, and that this
commit propagates the same pattern to `add`, `remove`, `remove-missing`,
`replace`, `unlock`, and `recover`, with `discover.md` rewritten to the
new canonical wording.
