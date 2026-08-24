# Plan: simplify the plain-lock orchestration seam

## Problem

The private plain-lock orchestrator forwards command dependencies and a
hardcoded `false` dry-run flag through its injected lock callback. The
orchestrator never supports preview mode, and its tests ignore every forwarded
callback argument.

## Decision

Replace the argument-forwarding callback with a zero-argument injected lock
operation. The production wrapper captures the command dependencies and keeps
the non-preview choice at the plain-lock boundary.

## Invariants

- No public API or user-visible behavior changes.
- Plain lock remains structurally non-preview; `cmd_lock` retains its reachable
  dry-run input for the separate preview path.
- Ordering remains lock success, coordinator completion, then online-service
  deactivation.
- The systemd-stop path is unchanged.

## Proof obligations

- Lock failure neither marks the coordinator done nor deactivates the online
  service.
- Successful lock marks the coordinator done before deactivation.
- Coordinator completion failure prevents deactivation.
- The full Rust test suite remains green.

## Non-goals

- No ADR, README, command-documentation, error, or output changes.
- No new behavioral tests unless implementation exposes behavior not covered by
  the existing orchestration tests.

## Delivery

Mark TASK-31 done in `scratch/2026-08-05-improvement-hunt.md` and record that the
complete pass-through callback seam was removed.
