# Consolidate recovery-plan predicates

## Problem

`plan_recover` repeatedly classifies the same journal operation and separately
tests the same open-plan state while assembling consecutive recovery actions.
The repetition obscures that replace-specific actions belong only to recovery
sessions that braid itself plans to mount.

## Decision

Classify replace pool mutation once in the planner and structurally group all
open-plan actions. Replace recovery still plans the initial open, kernel wait,
and remount cycle in that order; other recovery paths retain their current
actions and refusals.

## Invariants

- An already-mounted replace pool mutation is refused before action assembly.
- A replace pool mutation that braid plans to mount orders the initial open,
  kernel wait, remount cycle, and completion without changing failure behavior.
- Non-replace and already-mounted non-replace recovery paths retain their
  existing actions, previews, and execution behavior.
- Recovery error wording, public interfaces, action types, and runtime safety
  guards remain unchanged.

## Proof obligations

- Planner tests cover mounted and unmounted replace and non-replace paths.
- Execution tests prove successful replace recovery traverses the remount cycle
  and a remount failure aborts before persistent recovery state is changed.
- The full Rust suite remains green.

## Non-goals

- Do not cache replace classification in the work plan or consolidate the
  separate execution-time credential decision.
- Do not change recovery documentation or the audit record.

## Implementation discretion

- Internal binding names and nesting details are left to implementation so long
  as the invariants and ordering above remain explicit.
