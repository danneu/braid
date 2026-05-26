# Fix monitor.md "under the hood" step ordering

## Context

`docs/commands/monitor.md`'s "What happens under the hood" numbered list
(lines 42-51) misorders the self-heal/reconcile of stale ack state. It lists
"self-heals stale ack state" as the **last** step (step 8, *after* merging into
the alert latch), implying it happens after alert computation.

In the code (`cli/src/monitor.rs` `cmd_monitor`), the reconcile/self-heal runs
*before* alert causes are computed and feeds them:

- `reconcile_acked_stats` runs at code step 6 (`monitor.rs:100-105`), mutating
  the in-memory `acked` baseline (prune orphan devids + clear `missing_acked`
  for devices present again).
- If it changed anything, `save_acked_stats` persists immediately
  (`monitor.rs:106-109`); a write failure propagates via `?` and is folded into
  a fail-closed `ComputationError` (`monitor.rs:139-141`). This is deliberate
  (commit `3edfe1b7 fix(monitor): fail closed when acked-stats save fails`).
- The reconciled `acked` is then read by `compute_alert_state` at code step 7
  (`monitor.rs:113-120`), where `missing_acked` gates whether a `MissingDevice`
  cause fires (`alert.rs:132`). So the heal must precede cause computation, not
  follow the latch merge.

A reader debugging an ack-loop or a reconcile-save EROFS beep forms the wrong
mental model from the current ordering. The design authority
(`docs/design/decisions/014-alerts.md:155`) already documents this correctly in
prose; only the user-facing command doc drifted.

**Scope:** docs-only, single file. The Explore pass confirmed
`docs/commands/monitor.md` is the *only* doc with a numbered monitor-cycle step
list. `docs/guides/monitoring-and-alerts.md` has no such sequence, and ADR 014
is prose/defense-in-depth -- neither needs reordering.

## Change

### Primary edit -- `docs/commands/monitor.md:42-51`

Replace the 8-step list (steps reordered to mirror `cmd_monitor`; 8 -> 6 steps,
folding the three cause-checks into one and surfacing both fail-closed paths).

Current:

```
## What happens under the hood

1. Checks if the pool is mounted. If not, exits 0 (nothing to monitor).
2. Runs `btrfs device stats` on the pool mount point.
3. Loads the acknowledged-stats baseline (`acked-stats.json`) from a previous `braid ack`.
4. Computes which devices have new errors above the baseline.
5. Checks for missing/null-underlying devices.
6. Checks for a smartd alert flag.
7. Merges results into the alert latch (`alert-latch.json`). The latch is sticky: once an alert fires, it stays active until `braid ack` clears it.
8. Self-heals stale ack state: if a device was previously acknowledged as missing but is now present, the missing-acked flag is automatically cleared.
```

New:

```
## What happens under the hood

1. Checks if the pool is mounted. If not, exits 0 (nothing to monitor).
2. Runs `btrfs device stats` on the pool mount point.
3. Loads the acknowledged-stats baseline (`acked-stats.json`) from a previous `braid ack`. If the file is unreadable or unparseable, monitor fails closed -- it latches a `ComputationError` rather than firing every acknowledged cause against an empty baseline.
4. Self-heals stale ack state *before* computing alerts: prunes baseline entries for devices no longer in the pool, and clears the missing-acked flag for any device that was acknowledged missing but is now present again. If the baseline changed, the updated `acked-stats.json` is written immediately; a write failure (e.g. EROFS, ENOSPC) is itself a fail-closed `ComputationError`.
5. Computes alert causes against the reconciled baseline: btrfs device errors above the baseline, missing/null-underlying devices, and the smartd alert flag.
6. Merges the causes into the alert latch (`alert-latch.json`). The latch is sticky: once an alert fires, it stays active until `braid ack` clears it.
```

### Secondary edit (consistency) -- `docs/commands/monitor.md:36`

The "What triggers an alert" `Computation error` bullet enumerates the
`ComputationError` causes but (a) omits the acked-stats *save* failure now
surfaced in step 4, and (b) calls the acked-stats and alert-latch causes "read"
failures when each is really a *load* (read + parse) -- and the latch is
load-or-quarantine. `load_acked_stats_fallible` folds JSON parse errors into its
returned error (`alert.rs:219`); `load_alert_latch_or_quarantine` returns a
`ComputationError` detail for parse and quarantine failures too
(`alert.rs:336-371`, folded at `monitor.rs:135`). Tighten the wording so the
enumeration is complete and accurate. Keep the leading generic `parse` item in
the bullet -- it covers the distinct `parse_btrfs_device_stats` step
(`monitor.rs:84`); only the tail enumeration items change.

Current fragment:
`... acked-stats baseline read, or alert latch read failed.`

New fragment:
`... acked-stats baseline load, acked-stats save during self-heal, or alert-latch load/quarantine failed.`

## Style constraints

- Use `--` (ASCII), never em-dash -- matches the rest of the doc (e.g. line 28)
  and the repo's CLI/doc style rule.
- Keep ASCII throughout (`EROFS`, `ENOSPC` are fine).
- This is user-facing reference prose, not a literal code trace: internal-only
  inputs (membership views, smartd flag probe) stay folded into step 5 rather
  than getting their own steps.

## Re-verify before editing

Per repo planning hygiene, re-read these to confirm the order is still current
before applying (code may have changed):

- `cli/src/monitor.rs:43-159` -- `cmd_monitor` numbered-comment sequence.
- `cli/src/alert.rs:258-279` -- `reconcile_acked_stats` (prune + self-heal).
- `cli/src/alert.rs:102-143` -- `compute_alert_state` (reads reconciled `acked`).
- `docs/design/decisions/014-alerts.md:155` -- prose reference to mirror.

## Verification

Docs-only change; no Rust code touched, so no `just test-rust` / VM tests apply.

1. `mdbook build docs` -- validates all `docs/` cross-links via
   `mdbook-linkcheck` (a broken cross-link fails CI). The edit adds no new
   links, so this should pass; run it to confirm nothing regressed.
2. Read the rendered `## What happens under the hood` section and confirm: 6
   steps, self-heal (step 4) precedes cause computation (step 5), both
   fail-closed paths present (steps 3 and 4), `--` used throughout.
3. Confirm the step list now matches `cmd_monitor`'s order and ADR 014's prose
   (reconcile is per-cycle, before alerts; save failure -> `ComputationError`).
