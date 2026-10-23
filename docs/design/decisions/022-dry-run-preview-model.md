---
intent: Capture the dry-run preview architecture for mutating CLI commands. Read before modifying dry-run output, preview rendering, command planning, typed work plans, or execution paths.
status: Active
---

# Dry-run preview model


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

Dispatch owns the read-side fences that must run under the pool lock before
the planner starts: pending-operation preflight and, for migrated mutating
commands, config loading. For every command recovery mode refuses (the `Gated`
rows of `cli/src/main.rs#dispatch_policy`), migrated or not, dispatch runs the
pending-operation preflight once, right after acquiring the pool lock and
before any command arm, so a recovery journal is never hidden behind a config
or `pool.json` load error. The planner
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
that require a passphrase or a mapper that was closed during planning. It may
also re-run its planner after the confirmation prompt, before the journal, to
validate live state the pool lock cannot pin, and refuse unless the new plan
equals the confirmed one. The new plan is only compared; execute still runs the
confirmed plan (`cli/src/remove.rs#RemovePlan::recheck_before_journal`,
`cli/src/remove_missing.rs#RemoveMissingPlan::recheck_before_journal`).

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

### Confirmation UI

Confirmation UI is not a preview note. The interactive `!params.yes` block --
the command summary, yes/no prompt, and go/no-go safety warnings attached to
that prompt -- is deliberately absent from both `--dry-run` and `--yes` output.
In `cli/src/remove.rs` and `cli/src/replace.rs`, the 1-disk redundancy warning
belongs to this class because it gates the operator's final decision about an
explicitly requested action, rather than reporting a discovered precondition.

For `remove` 2->1, dry-run still surfaces the redundancy-loss consequence as
the `RAID1 -> ...` balance step, labeled with the profiles
`cli/src/cmd.rs#ONE_DISK_PROFILES` converts to. For `replace`, the 1-disk
warning is confirmation-only context for a pool that is non-redundant before
and after; dry-run previews the replacement steps, and no redundancy-changing
step exists for that warning.

Long-running side-effect-free probes that run while building a preview may emit
`[wait]` / `[ok]` / `[skip]` status rows to stderr per
[Principle 13](../principles.md#13-announce-long-running-work). Those rows are
not part of the structured preview.

### Fresh-format identity placeholder

A fresh LUKS format mints its identity per-invocation at plan time (ADR-024),
so the UUID a real run will write does not yet exist when dry-run renders.
Showing the minted UUID would make the preview non-reproducible -- two dry-runs
of the same command would differ -- and misleading, since that value is
discarded when dry-run returns and a later real run mints a different one.

So the two fresh-format render sites (`cli/src/add.rs#AddWorkPlan::render_steps`
and `cli/src/replace.rs#ReplaceWorkPlan::render_steps`) emit a preview-only
`cli/src/cmd.rs#CmdRequest`, `CryptsetupLuksFormatPreview`, whose `to_argv`
renders a fixed `--uuid '<generated-at-format-time>'` placeholder (single-quoted
by `shell_words`). The real run uses `CryptsetupLuksFormat` with the journaled
identity. Both render through one shared `cli/src/cmd.rs#luks_format_argv`
builder, so a future luksFormat flag appears in both at once -- the "representative
commands" / "`Step` is output-only" rules in the Decision section still hold; this
is the one place the rendered command intentionally diverges from the real argv.
The preview variant is never executed: `cli/src/cmd.rs#RealRunner` hard-errors on
it via `cli/src/cmd.rs#CmdRequest::is_preview_only` before any spawn.

`recover` renders no luksFormat row: it never formats a disk.
`cli/src/recover.rs#render_add_pool_mutation_recovery_steps` renders a drop row
for a fresh target luksFormat never wrote, the step
`cli/src/recover.rs#drop_never_formatted_add_targets` runs.

## Scope

The typed work-plan preview model is the precedent for `add`, `replace`,
`remove`, `remove-missing`, and `recover`.

Recover is the one deliberate exception to the read-side planner rule, in two
places. The first: when
recovering an interrupted existing-pool add and the pool is not already mounted,
`plan_recover` reconciles the validated add-targets -- those present,
LUKS-openable, and not yet pool members -- before mount: it opens any whose
mapper is closed (resolving the unlock credential once, and only then), and
btrfs-scans a target only when its mapper shows a btrfs signature. All of this
is gated by `!dry_run` (`discover_add_targets_before_mount`, after an
already-mounted short-circuit). The preflight is non-destructive and exists for
two reasons: resolving the credential in the preflight window where an
interactive prompt belongs, then caching it so execute reuses it without a
second prompt (single passphrase, Principle 4); and making an
already-committed-but-closed target visible to the kernel before the initial
mount so the mount assembles it instead of recover re-adding it. It is not a
general license to mutate inside `plan_*()`.

The second: for an interrupted bootstrap add `PoolMutation` with nothing
mounted at the mount point, `cli/src/recover.rs#plan_bootstrap_recovery` opens
each closed target that luksFormat wrote, asks btrfs whether its mapper holds a
filesystem, and closes what it opened, all gated by `!dry_run`. The plan must
know whether mkfs.btrfs ran before it can choose between clearing the journal
and mounting, and only an open mapper answers that. When no target carries a
LUKS header, the plan needs no open at all, so `--dry-run` previews that clear
exactly.

The add and replace planners also run `cryptsetup luksFormat --test-args`
(`cli/src/luks.rs#check_format_args`), `--dry-run` included. That is a
read-side probe, not an exception: cryptsetup checks the options and exits
before it opens the device.

The LUKS-UUID-identity migration also gave `lock` a typed close set
(`LockCloseSet` carrying ordered `LockMapperClose` entries in
`cli/src/lock.rs`). Dry-run step compilation (`compile_lock_steps`),
`btrfs device scan --forget`, and `LockPlan::execute` all read from
that close set so preview and real execution share one identity
classification. `LockPlan::preview()` derives `Vec<Step>` on demand
from the close set rather than caching rendered steps.

The lock plan also owns the module-install unit stops (ADR 018 "On
`lock`"). `plan_lock` resolves the scrub units and the `BoundBy
braid-online.service` consumers into typed `LockUnitStop` entries, each
carrying its failure policy, only after every refusal check has passed.
The preview renders a `stop <unit>` step per entry, and
`LockPlan::execute` runs them right before unmount without looking the
list up again. So the lock wrapper calls the planner first, the planner
is lock's only refusal site, and planning mutates nothing. On a
lifecycle host, a dry-run therefore also spawns the read-only
`systemctl show -P BoundBy braid-online.service` lookup.

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
- `docs/design/decisions/012-intent-cli.md` -- intent-command safety model and
  dry-run probe constraints.
- `plans/impl/2026-05-06-unify-cli-plan-execution.md`
  -- historical implementation plan for the migration that introduced this
  typed work-plan preview model.
