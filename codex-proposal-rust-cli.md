# Proposal: Bash to Rust Migration (CLI-First, No Daemon Dependency)

## Goal
Replace `scripts/braid.sh` with a Rust CLI that preserves current command contracts:
1. `braid init-disk`
2. `braid plan`
3. `braid apply`
4. `braid status`

No daemon is required for normal operation. A daemon can be added later on top of the same core library if needed.

## Why CLI-First
1. Smaller migration surface.
2. Fewer moving parts (no socket protocol, no service lifecycle).
3. Easier rollout and debugging.
4. Still gains strong typing, safer state modeling, and fast non-VM tests.

## Target Architecture
Rust workspace with:
1. `braid-core`
   - Domain types (`Config`, `LiveState`, `Plan`, `Action`, `Checkpoint`)
   - Planner and invariant checks (pure functions)
2. `braid-probe`
   - Reads live system state from tools/sysfs
3. `braid-exec`
   - Executes actions and checkpoint lifecycle
4. `braid-cli`
   - User-facing command binary (`clap`)

## Command Contracts (Must Preserve)
1. `init-disk` is the only destructive formatting path.
2. `plan` is read-only.
3. `apply` never formats, supports resume/checkpoint.
4. Missing-device removal is explicitly gated.
5. Warnings/blocked reasons remain machine-parseable.

## Type-Level Safety Model
Use Rust typestate for lifecycle correctness.

### Plan Typestate
- `Plan<Applicable>`
- `Plan<Blocked>`

Only `Plan<Applicable>` can be executed.

### Action Typestate
- `Action<Pending> -> Action<InProgress> -> Action<Completed | Failed>`

No API supports invalid transitions.

### Hard Boundary Types
- Formatting operations live only in `InitDiskOp`.
- Apply action enum excludes format variants.

## Implementation Phases
1. Build fixtures from current `plan --json` and representative `apply` traces.
2. Implement `braid-core` planner with parity tests.
3. Implement `braid-exec` with atomic checkpoint writes and resume checks.
4. Implement `braid-cli` subcommands and output formatting parity.
5. Switch Nix packaging to Rust CLI.
6. Keep Bash wrapper temporarily as a forwarding stub (optional), then remove.

## Testing Strategy
### Fast Rust Tests (primary)
1. Planner unit tests:
   - add/remove/replace/no-op
   - missing-device gate behavior
   - mapper alias identity (`disk1` vs `virtio-disk1`)
2. Property tests:
   - no destructive apply actions
   - blocked plans are non-executable
   - action transition monotonicity
3. Executor integration tests with mocked command runner:
   - checkpoint resume
   - target missing on resume
   - confirmation gates

### VM Tests (reduced but essential)
Keep only true integration boundaries:
1. remote unlock and degraded boot
2. first-disk bootstrap
3. failed-disk replacement
4. one checkpoint/resume end-to-end path

## CLI UX Compatibility
1. Preserve existing flags and output semantics where practical.
2. Preserve warning/error codes used by tests.
3. Keep `--json` schemas stable (version if changes are needed).

## Packaging and Deployment
1. Build static or mostly-static Rust binary via Nix.
2. Replace `writeShellApplication` for `braid` with Rust package.
3. Keep standalone legacy scripts only as explicit stubs (or remove entirely if no compatibility desired).

## Optional Future Daemon
If later needed for TUI streaming/concurrency:
1. Add `braid-daemon` using `braid-core` and `braid-exec`.
2. Keep CLI working in direct mode even if daemon is unavailable.
3. Add `--via-daemon` mode as opt-in before making it default.

## Risks and Mitigations
1. Behavior drift from Bash:
   - Mitigate with fixture parity and golden tests.
2. Hidden shell command differences:
   - Mitigate with explicit probe/exec adapters and integration tests.
3. Over-retaining VM test load:
   - Mitigate by migrating planner/transition checks to Rust tests first.

## Expected Outcome
A safer, typed CLI implementation with faster feedback loops, lower operational complexity than daemon-first, and a clean path to add a daemon later if needed.
