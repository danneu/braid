# Plan: scope recover cycle name computation to its consumer

## Context

`plan_recover` in `cli/src/recover.rs` computes two values eagerly today:

- `cycle_reopen_names: Vec<DiskName>` -- built from a `DiskName::parse` loop
  over `report.events` (`cli/src/recover.rs:1260-1278`).
- `cycle_close_names: Vec<DiskName>` -- built by iterating the membership
  union and probing `/dev/mapper/<name>` via `fs.exists` for each member
  (`cli/src/recover.rs:1279-1289`).

Both are read only inside the `if let Some(initial_open_plan) = &open_plan
&& is_replace_pool_mutation(&journal.op)` block at
`cli/src/recover.rs:1385-1411`. Confirmed by grep: the only reads are at
`:1388`, `:1398`, `:1407`, `:1408` -- all inside that gate.

For every non-Replace recover plan (`Add`, `RemoveMissing`, post-maintenance
phases, `Replace::PostReplaceMaintenance`) we therefore:

1. Run an unconditional parse loop with a fallible
   `DiskName::parse(name).map_err(...)` whose only consumer is dead code on
   this path.
2. Allocate and populate two `Vec<DiskName>` that will be dropped unread.
3. Force a reader to chase `cycle_close_names` / `cycle_reopen_names` ~125
   lines down to the gate at `:1385` to learn that they are unused on the
   common path.

The fix is local: move both computations inside the
`is_replace_pool_mutation` branch so the parse-failure path is reachable
only when the cycle is actually planned, and non-Replace plans drop both
Vec allocations entirely. Behavior is preserved because the values have no
other readers.

This is a Simplicity-tier change, not a correctness fix. There is no user-
visible regression to repair; the goal is to make `plan_recover` cheaper to
read and slightly cheaper to run on the dominant non-Replace paths.

## Recommended approach

In `cli/src/recover.rs`, delete the current eager computation block at
`:1260-1289` and reinsert both computations at the top of the
`is_replace_pool_mutation` gated block at `:1385-1411`, immediately before
the existing `for name in &cycle_reopen_names` membership check.

Concretely, the gated block becomes:

```rust
if let Some(initial_open_plan) = &open_plan
    && is_replace_pool_mutation(&journal.op)
{
    let mut cycle_reopen_names: Vec<DiskName> = Vec::new();
    for event in &report.events {
        let Some(name) = (match event {
            mount::ProbeEvent::DiskAvailable { name }
            | mount::ProbeEvent::DiskAlreadyOpen { name } => Some(name),
            _ => None,
        }) else {
            continue;
        };
        let parsed = DiskName::parse(name).map_err(|e| {
            PlanFailure::with_notes(
                notes.clone(),
                RecoverError::Failed(format!(
                    "recover remount cycle preview: invalid disk name \
                     from mount planner '{name}': {e}"
                )),
            )
        })?;
        cycle_reopen_names.push(parsed);
    }
    let cycle_close_names: Vec<DiskName> = union
        .iter()
        .filter_map(|(_, member)| {
            let name = &member.name;
            if cycle_reopen_names.contains(name) {
                return Some(name.clone());
            }
            let mapper_path = format!("/dev/mapper/{}", config::mapper_name(name).0);
            fs.exists(&mapper_path).then(|| name.clone())
        })
        .collect();

    for name in &cycle_reopen_names {
        // ... existing membership check
    }
    if cycle_reopen_names.is_empty() {
        // ... existing empty check
    }
    actions.push(RecoverWorkAction::RemountCycle {
        close_names: cycle_close_names,
        reopen_names: cycle_reopen_names,
        any_missing_member: initial_open_plan.any_missing_member,
    });
}
```

Notes:

- `notes.clone()` stays in the parse-error arm because `notes` is still
  borrowed by the subsequent `completion` match (`:1413+`) and the final
  plan assembly. No ownership win is available here.
- `report.events`, `union`, and `fs` are all in scope at the new site;
  the move is a pure rewrap with no helper extraction.
- Keep the existing membership check, empty check, and `RemountCycle`
  push exactly as written -- only the two computations move.
- Do not extract a helper. The block is ~30 lines and has a single call
  site; a helper would obscure the now-tight scoping.

## Critical files

- `cli/src/recover.rs` -- only file modified. Delete `:1260-1289`,
  reinsert at the top of the gated block at `:1385`.

## Verification

Existing tests cover the moved code densely; no new tests are needed.

Rust unit tests in `cli/src/recover.rs` that exercise the cycle path:

- `recover_remount_cycle_umount_failure_aborts_before_pool_json`
- `recover_remount_cycle_honors_close_names_over_membership`
- `recover_remount_cycle_skips_disappeared_planned_mapper`
- `recover_remount_cycle_mount_failure_closes_reopened_mappers`
- `plan_recover_dry_run_cycle_close_set_includes_absent_open_mapper`
- `plan_recover_dry_run_cycle_reopen_set_excludes_damaged_header_disk`
- `plan_recover_dry_run_cycle_mount_uses_first_reopen_not_initial_mount_device`

NixOS VM tests under `tests/cli/`:

- `recover-replace-completed.py`
- `recover-replace-not-started.py`

Steps:

1. `just test-rust` -- catches any local regression in cycle computation,
   close-name selection, reopen ordering, or parse-error wrapping.
2. `just test-vm recover-replace-completed recover-replace-not-started`
   -- end-to-end Replace recovery on a live VM, confirms the gated cycle
   still fires correctly under the kernel-resumed-replace path.

Non-goals:

- No change to `RecoverWorkAction::RemountCycle`'s shape or semantics.
- No change to error messages, preview rendering, or note ordering.
- No change to non-Replace ops' plans beyond removing two unused Vec
  allocations and an unreachable parse-error arm.
