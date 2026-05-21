---
intent: Capture the dry-run preview architecture for mutating CLI commands. Read before modifying dry-run output, preview rendering, command planning, typed work plans, or execution paths.
---

# Dry-run preview model

Status: Active

> Principles:
> - [Safe-by-construction operations](../principles.md#3-safe-by-construction-operations)
> - [Single passphrase](../principles.md#4-single-passphrase)
> - [One pool operation at a time](../principles.md#12-one-pool-operation-at-a-time)
> - [Announce long-running work](../principles.md#13-announce-long-running-work)

## Context

Intent commands originally mixed dry-run rendering seams with execution
planning. Some commands compiled `Vec<Step>` directly for preview tests, while
execution consumed separate command-specific state. That made it too easy for a
dry-run preview to drift from the work a real run would perform, especially
around LUKS preparation, btrfs mutations, journals, cleanup, and follow-up
maintenance such as resize or balance.

The current model keeps dry-run preview and execution tied to the same typed
semantic decision. `Step` is only the output shape used to show a preview; it is
not the plan.

## Decision

For migrated mutating commands, dispatch owns the read-side fences that must
run under the pool lock before the planner starts: pending-operation preflight
and config loading. The pending-operation preflight must run before config load
so a recovery journal is never hidden behind a config parse error. The planner
then owns pool state loading, live probes, accumulated preview notes, and
construction of a typed work plan. This split finishes the Rust-owned pool-lock
migration: the lock boundary and the config/journal reads it protects now live
above `plan_*()`, while dry-run and real execution still share the same typed
plan. The command wrapper calls the planner first. On `--dry-run`, it prints
`plan.preview()` to stdout. On a real run, it passes the same plan to
`execute()`.

A successful command plan carries:

- accumulated `PreviewNote`s, in the order they must render;
- a typed `WorkPlan` containing the semantic choices execution needs.

`preview()` is the public dry-run boundary. It constructs a `Preview` whose
steps come from `work_plan.render_steps()`. Notes render first, then steps. A
plan struct must not cache a rendered `Vec<Step>` alongside its work plan.

`execute()` consumes the same typed `WorkPlan`. It must not rediscover or
reinterpret semantic choices already made during planning. It may still perform
execution-time validation that dry-run intentionally cannot do, such as checks
that require a passphrase or a mapper that was closed during planning.

`Step` is output-only. It may describe risk, human text, and representative
commands for dry-run rendering, but it must not become an execution source, a
planning cache, or a second semantic model.

When planning accumulates notes and then fails later, use a report shape that
returns both the error and the accumulated notes. The command wrapper renders
those notes to stderr before returning the error, using the same preview note
renderers that dry-run stdout uses. This preserves context without duplicating
wording.

## Output contract

The structured dry-run preview lives on stdout. Preview notes are part of that
stdout preview. Real-run notes, and notes preserved on a later planning error,
render to stderr through the shared preview renderers so warning and info
wording stays byte-compatible across modes.

Long-running side-effect-free probes that run while building a preview may emit
`[wait]` / `[ok]` / `[skip]` status rows to stderr per
[Principle 13](../principles.md#13-announce-long-running-work). Those rows are
not part of the structured preview.

## Scope

The typed work-plan preview model is the precedent for `add`, `replace`,
`remove`, `remove-missing`, and `recover`.

The LUKS-UUID-identity migration also gave `lock` a typed close set
(`LockCloseSet` carrying ordered `LockMapperClose` entries in
`cli/src/lock.rs`). Dry-run step compilation (`compile_lock_steps`),
`btrfs device scan --forget`, and `LockPlan::execute` all read from
that close set so preview and real execution share one identity
classification. `LockPlan::preview()` derives `Vec<Step>` on demand
from the close set rather than caching rendered steps.

Older dry-run seams in `unlock` and `enroll` may remain until those
commands are intentionally migrated. Do not use their older helpers or
cached step fields as precedent for commands already on the typed
work-plan model.

## Consequences

- Tests about user-visible dry-run output should prefer `plan_*()` followed by
  `plan.preview().render()`.
- Tests about the step list should use `plan.preview().steps`.
- Narrow leaf-renderer tests may call `work_plan.render_steps()` directly when
  reaching the case through `plan_*()` would require noisy unrelated setup.
- New migrated command plans should store semantic work, not rendered steps.

## See

- `cli/src/preview.rs` -- `Preview`, `PreviewNote`, and canonical rendering.
- `cli/src/cmd.rs` -- `Step` and dry-run command rendering.
- `docs/decisions/012-intent-cli.md` -- intent-command safety model and
  dry-run probe constraints.
- [plans/impl/2026-05-06-unify-cli-plan-execution.md](../../plans/impl/2026-05-06-unify-cli-plan-execution.md)
  -- historical implementation plan for the migration that introduced this
  typed work-plan preview model.
