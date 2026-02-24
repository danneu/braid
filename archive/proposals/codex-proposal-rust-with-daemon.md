# Proposal: Bash to Rust Migration with Daemon-First Architecture

## Goal
Migrate safety-critical `braid` logic from Bash to Rust, with a Unix-socket daemon as the primary control plane for CLI and future TUI clients.

## Design Objectives
1. Preserve safe-by-construction boundary: only `init-disk` may format (`luksFormat`).
2. Make illegal action/state transitions unrepresentable.
3. Centralize coordination (locking, checkpointing, progress, multi-client safety).
4. Keep NixOS integration clean and declarative.
5. Move most behavior testing out of slow VM tests into fast Rust tests.

## Target Architecture
Rust workspace with:
1. `braid-core`: pure domain + planner + invariants.
2. `braid-probe`: live state discovery (`btrfs`, `cryptsetup`, `/sys`, by-id mapping).
3. `braid-exec`: action execution, checkpoints, resume semantics.
4. `braid-daemon`: NDJSON RPC over Unix socket, event streaming.
5. `braid-cli`: thin client to daemon.

## Migration Phases
1. Freeze behavior with fixtures and current VM tests.
2. Implement typed planner in `braid-core`.
3. Implement typed executor + checkpoint transitions in `braid-exec`.
4. Build daemon read-only methods (`ping`, `status.get`, `plan.compute`).
5. Build daemon mutable methods (`init_disk.start`, `apply.start`, `apply.resume`, `apply.status`).
6. Switch CLI to daemon-backed calls.
7. Replace Nix packaging to ship Rust binaries.
8. Remove Bash core logic after parity validation.

## Type-Level Safety Model
Use typestate + enums to prevent invalid flows.

### Plan State
- `Plan<Applicable>`
- `Plan<Blocked>`

Only `Plan<Applicable>` can become an apply session.

### Action Lifecycle
- `Action<Pending>`
- `Action<InProgress>`
- `Action<Completed>`
- `Action<Failed>`

Allowed transitions only:
- `Pending -> InProgress`
- `InProgress -> Completed | Failed`

No API for illegal transitions (for example, `Completed -> InProgress`).

### Destructive Boundary
Separate operation domains:
- `InitDiskOp` may include formatting.
- `ApplyActionKind` has no format variant.

This makes formatting-from-apply structurally impossible.

### Gated Dangerous Ops
Explicit gate tokens:
- `ConfirmedRemoveMissing`

Planner/executor APIs requiring this token cannot emit/run missing-device removal without validated operator intent.

## Daemon API (NDJSON over Unix Socket)
Request envelope:
```json
{"v":1,"id":"req-1","method":"plan.compute","params":{"config_path":"/etc/braid/config.json"}}
```

Success response:
```json
{"v":1,"id":"req-1","ok":true,"result":{"status":"applicable","actions":[]}}
```

Error response:
```json
{"v":1,"id":"req-1","ok":false,"error":{"code":"INIT_REQUIRED","message":"...","data":{}}}
```

Rules:
1. Exactly one terminal response per request.
2. Optional progress events for long-running operations.
3. Structured machine-parseable errors only.

## Test Strategy
Shift most testing to Rust:
1. Unit tests (`braid-core`) for planner behavior and invariants.
2. Property tests for state machine safety and forbidden action classes.
3. Executor integration tests with fake runners/filesystems.
4. Daemon protocol contract tests.

Retain VM tests for:
1. Real `cryptsetup` + `btrfs` behavior.
2. initrd remote unlock + degraded boot.
3. failed-disk replacement end-to-end.
4. one checkpoint/resume e2e path.

## Rollout
1. Run Rust planner in shadow mode against Bash outputs.
2. Compare plans in CI fixture corpus.
3. Flip CLI to Rust/daemon when parity is met.
4. Remove Bash implementation.

## Risks and Controls
1. Behavior drift:
   - Control via fixture parity + shadow comparisons.
2. Protocol lock-in:
   - Control via versioned envelopes and compatibility tests.
3. Operational complexity:
   - Control via systemd hardening, health checks, and clear fallbacks.

## Expected Outcome
Safer reconciliation engine, stronger invariants at compile time, and a stable control plane for future TUI/automation.
