# Rust Rewrite: Analysis and Recommendations

Review of `codex-proposal-rust-cli.md` against the actual codebase.

## Current Codebase Scale

- `scripts/braid.sh`: ~1,640 lines (the main CLI)
- `scripts/braid-remove-disk.sh`: ~261 lines
- `scripts/braid-status.sh`: ~182 lines
- NixOS modules: ~400 lines across 6 files
- VM tests: 29 files, 40+ tests, ~2,813 lines
- Total shell: ~2,087 lines

The bash is sophisticated but not large. The Rust version will likely be 3-5K lines.

## The Rewrite is Justified

### Device identity is the scariest part

Misidentifying a disk means formatting the wrong drive. Three identifier layers (by-id path, mapper name, btrfs devid) must be reconciled correctly every time. Typed newtypes (`ByIdPath`, `LuksUuid`, `MapperName`) make it impossible to accidentally pass a mapper name where a by-id path is expected.

### JSON handling is fragile in bash

Hand-assembling JSON with `jq -n` and parsing btrfs/lsblk output with awk/grep chains is brittle. Serde with `lsblk -J` and structured parsing eliminates this class of bugs.

### Checkpoint serialization needs types

The atomic tmp+mv pattern works, but serde + typed checkpoint structs eliminate corruption and deserialization bugs.

### Fast planner tests are a real workflow improvement

Currently the only safety net is 40+ VM tests that take minutes. Testing "config says 3 disks, live state shows 2 with these UUIDs, plan should be X" in milliseconds is a significant feedback loop improvement.

## Recommended Changes to the Proposal

### Use a single crate, not a 4-crate workspace

The proposal calls for `braid-core`, `braid-probe`, `braid-exec`, `braid-cli`. That's too much structure for a 3-5K line codebase. Four crates add compile-time boundaries, `pub` visibility ceremony, and cross-crate dependency management that doesn't pay for itself at this scale.

Single crate with modules instead:

```
braid/
  src/
    main.rs          // clap CLI
    config.rs         // Config types, deserialization
    identity.rs       // DeviceIdentity, probe chains, UUID join
    probe.rs          // Live state discovery (lsblk, cryptsetup, btrfs)
    plan.rs           // Planner (pure functions, easy to test)
    exec.rs           // Action execution, checkpoint lifecycle
    status.rs         // Status formatting
    types.rs          // Shared domain types, newtypes
```

Same logical separation. No cross-crate overhead. If a crate boundary becomes necessary later (e.g., daemon imports the library), split then — it's a straightforward refactor.

### Pragmatic type safety: typestate where it's permanent, enums where it's runtime

The original proposal used typestate everywhere. Plain enums everywhere swings too far the other way. The right split: **typestate for things the compiler should prevent forever, enums for things that change at runtime and cross serialization boundaries.**

#### Compile-time walls (typestate)

These are permanent architectural invariants — not runtime state. Making them compile-time walls means no refactor or future contributor can accidentally violate them.

1. **`InitDiskOp` is a separate type from reconciliation ops.** The apply path physically cannot express a format operation. This is the most important safety contract in the system.

2. **Executor only accepts `ApplicablePlan`.** Once plan applicability is computed, it doesn't change. A newtype or distinct type is cheap and prevents passing an unchecked plan to apply.

```rust
// The executor signature enforces this at compile time
fn execute(plan: ApplicablePlan, runner: &dyn CommandRunner) -> Result<()>;

// ApplicablePlan can only be constructed by the planner
// when no blocked reasons exist — no other code path can create one
```

#### Runtime-validated enums (action lifecycle)

Action state (`Pending → InProgress → Completed | Failed`) changes at runtime and must survive serialization for checkpoints. Typestate adds conversion boilerplate at every serde boundary for no benefit.

```rust
#[derive(Serialize, Deserialize)]
enum ActionStatus {
    Pending,
    InProgress,
    Completed,
    Failed { error: String },
}

impl ActionStatus {
    fn transition_to(&self, next: ActionStatus) -> Result<ActionStatus> {
        match (self, &next) {
            (Pending, InProgress) => Ok(next),
            (InProgress, Completed | Failed { .. }) => Ok(next),
            _ => Err(InvalidTransition { from: self, to: next }),
        }
    }
}
```

Plan outcome also uses enums — the plan is computed and consumed in a single invocation:

```rust
enum PlanOutcome {
    Applicable(ApplicablePlan),
    Blocked { actions: Vec<Action>, reasons: Vec<BlockedReason> },
    NoOp,
}
```

### Focus on the command runner abstraction

This is what actually unlocks fast testing. Define a trait for shelling out:

```rust
trait CommandRunner {
    fn run(&self, cmd: &str, args: &[&str]) -> Result<Output>;
}
```

Production implementation calls real binaries. Test implementation returns recorded outputs. This lets you test the planner, executor, and identity resolution against fixtures without VMs.

The command runner trait is more valuable than typestate — it's what makes the entire test strategy work.

## Test Layout

Keep Rust tests separate from the existing NixOS VM tests so neither interferes with the other.

**Unit tests** live inline as `#[cfg(test)]` modules — idiomatic Rust, access to private internals, run with `cargo test`:

```
braid-rust/
  src/
    plan.rs           // #[cfg(test)] mod tests { ... }
    identity.rs       // #[cfg(test)] mod tests { ... }
    exec.rs           // #[cfg(test)] mod tests { ... }
    ...
```

**Integration tests** live in `tests/rust/` — exercise the CLI binary end-to-end against fixtures and recorded command outputs:

```
tests/
  rust/               // Rust integration tests (fixture-based, golden tests)
    plan_fixtures/    // recorded plan --json outputs
    apply_traces/     // recorded apply sequences
    ...
  *.nix              // existing VM tests, untouched
```

This way `cargo test` runs the fast layer, `make test` runs the VM layer, and neither touches the other's files.

## What Stays the Same

These parts of the proposal are solid as-is:

- **CLI-first, no daemon** — correct migration strategy
- **LUKS UUID as canonical join key** — strictly better than path-based matching
- **DeviceIdentity struct** — right fields, right concept (just lives in a module, not a crate)
- **Command contracts** — init-disk only formats, plan is read-only, apply never formats
- **Implementation phases** — fixtures first, planner second, executor third, CLI fourth
- **Testing strategy** — fast Rust tests as primary, VM tests retained for full behavior coverage
- **Packaging** — static Rust binary via Nix, replaces `writeShellApplication`

## What Actually Simplifies Things

The biggest complexity in the bash isn't the language — it's device identity reconciliation and plan computation. Rust helps with both, but the real simplification comes from:

1. **Parsing structured output** (`lsblk -J`, `cryptsetup status` fields) instead of text munging
2. **A command runner trait** so planner + executor can be tested against recorded outputs
3. **Serde for all JSON** (config, plan output, checkpoints, status) instead of hand-assembly

## Summary

Do the rewrite. Single crate, compile-time walls for permanent invariants (InitDiskOp separation, ApplicablePlan gate), runtime enums for serialized state (action lifecycle), command runner trait for testability. The type safety for device identity and the fast test feedback loop are the primary wins. Don't over-engineer the structure — let it grow organically from a well-organized single crate.
