# Rust CLI Migration Plan

## Context

Replacing `scripts/braid.sh` (~1,640 lines) with a Rust binary. The rewrite gains type-safe device identity, serde for all JSON, and fast unit tests via a CommandRunner trait. See `brainstorm/claude-rust-rewrite.md` for full rationale.

**This is not a compatibility shim.** We are building the most robust, simple, and correct implementation. Define the outputs we actually want, then update VM test expectations to match. No concessions to the bash implementation's choices.

Architecture decisions already locked in:
- Single crate with modules (not 4-crate workspace)
- CommandRunner trait for mockable testing
- Typestate for permanent invariants (`InitDiskOp` separate from reconciliation, executor accepts only `ApplicablePlan`)
- Runtime enums for serialized state (`ActionStatus`, `PlanOutcome`)
- LUKS UUID as canonical join key

Source lives in `cli/` (matching existing `daemon/` pattern).

---

## Hard Rules

### 1. Structured output first, text parsing only when unavoidable

`command-capabilities.md` declares exactly which commands use JSON and which require text parsing for the pinned nixpkgs revision. Code implements only that contract.

**JSON (native):**
- `lsblk --json` — device tree, model, serial, UUID
- `findmnt --json` — mount state
- `btrfs --format json filesystem df <mount>` — RAID profile, usage

**Text (no JSON mode available):**
- `btrfs filesystem show` — device list, devids, missing count
- `cryptsetup status` — backing device, mapper state
- `cryptsetup luksUUID` — UUID extraction

### 2. Pin tool capabilities as deploy-time invariants

Document required versions and expected behavior in `cli/docs/command-capabilities.md`:
- btrfs-progs: version, which subcommands use `--format json`, which are text-only
- util-linux (lsblk, findmnt): version, JSON output fields relied upon
- cryptsetup: version, expected `status` output format

NixOS pins these via nixpkgs. The code always uses JSON flags where declared. No version detection, no runtime probing, no fallback parsing modes.

### 3. Fail hard on unexpected output

If a parser receives output that doesn't match the expected structure, return an explicit internal error. Never silently fall back to alternate parsing. If the toolchain drifts, we want a loud failure, not a subtle misparse.

### 4. Test the pinned contract

VM test fixtures must reflect exactly the Nix-provided toolchain. Rust unit test fixtures must use real outputs captured from that toolchain. If nixpkgs updates a tool version, fixtures get re-captured — provenance metadata makes this traceable.

### 5. Define canonical outputs, then update tests

Human and JSON outputs are designed for the Rust implementation, not preserved from bash. Define the output contract first (in Phase 3/5), then update VM test `.py` files to assert against the new contract. The VM tests validate the Rust binary's behavior, not backward compatibility with bash.

### 6. Structural safety boundaries, not string-grep tests

The `InitDiskOp` / reconciliation separation is enforced by module visibility and type boundaries, not by grepping source files. `exec.rs` cannot access formatting APIs because they are not exported from `init_disk.rs` (or vice versa). The compiler enforces Principle 3 — no test needed.

### 7. Typed command boundary — no raw string parsing in domain code

All external command interaction flows through a strict layered boundary. Domain modules never see raw stdout.

```
cmd.rs          CommandRunner trait — executes commands, returns raw Output
    ↓
parse.rs        All text/JSON parsing lives here — returns typed structs
    ↓
domain modules  probe.rs, plan.rs, exec.rs, status.rs — consume typed structs only
```

**Concrete rules:**
- No `split`, `regex`, `contains`, or stdout string manipulation in domain modules (probe, plan, exec, status, identity)
- `parse.rs` owns every parser. Each parser has its own unit tests at the parser layer.
- `cmd.rs` defines a typed request enum (`CmdRequest`) for every external command call and typed result structs (`CmdOutput`) returned to domain code
- Integration tests validate the full pipeline: `CmdRequest` → raw output → typed `CmdOutput`

This keeps parsing complexity localized, makes behavior deterministic, and prevents regression toward "shell script in Rust clothing."

---

## Phase 1: Crate Scaffold + Domain Types

**Create:**
- `cli/Cargo.toml` — deps: clap, serde, serde_json, thiserror, uuid
- `cli/src/main.rs` — clap skeleton with 4 subcommands, all print "not yet implemented"
- `cli/src/types.rs` — newtypes (`ByIdPath`, `LuksUuid`, `MapperName`), `ActionType` enum (no format variant — format ops are a separate type in `init_disk`), `ActionStatus` enum with `transition_to()` validator, `PlanOutcome`, `ApplicablePlan` newtype, `Action`, `BlockedReason`, `Confirmation`, `Warning`
- `cli/src/config.rs` — `Config` struct with serde, `config_read()`, `config_hash()`
- `cli/src/cmd.rs` — `CommandRunner` trait, `RealRunner`, `MockRunner`, `CmdRequest` enum (typed command variants), `CmdOutput` result structs
- `cli/src/parse.rs` — parser module (empty stubs, populated in Phase 2)

**Modify:**
- `flake.nix` — add `braid-rust` package via `rustPlatform.buildRustPackage` (not wired to tests yet)
- `Makefile` — add `make test-rust` target (`cd cli && cargo test`)

**Tests (`#[cfg(test)]` inline):**
- Config: deserialize valid JSON, reject missing fields, reject empty disks
- Types: ActionStatus transitions (valid + invalid), serde round-trips, newtype conversions
- Cmd: MockRunner returns expected outputs, CmdRequest covers all command variants

**Commit point:** `cargo test` passes. `nix build .#braid-rust` produces a binary. `make test` still passes (bash untouched).

---

## Phase 2: Probe + Identity Resolution

**Create:**
- `cli/src/parse.rs` — all parsers, organized by command:
  - **JSON parsers:** `parse_lsblk_json()`, `parse_findmnt_json()`, `parse_btrfs_df_json()` — serde deserialization into typed structs
  - **Text parsers:** `parse_btrfs_filesystem_show()`, `parse_cryptsetup_status()`, `parse_cryptsetup_luks_uuid()` — structured text extraction into typed structs
  - Every parser returns `Result<TypedOutput, ParseError>` — malformed input is always an explicit error
- `cli/src/probe.rs` — `LiveState` struct, `discover_live_state(runner, config)` — calls `CommandRunner`, feeds raw output to `parse.rs`, consumes only typed structs
- `cli/src/identity.rs` — `DeviceIdentity` struct, `resolve_config_identity()`, `resolve_pool_identity()`, UUID-based join logic — consumes typed structs from probe, no raw string handling
- `cli/docs/command-capabilities.md` — pinned tool versions, capability matrix (JSON vs text), expected output fields

**Capture fixtures first:** Run commands in VMs against the Nix-provided toolchain, save raw outputs to `cli/tests/fixtures/`. Each fixture file includes provenance metadata:

```json
{
  "_provenance": {
    "source_test": "tests/10-braid-plan.nix",
    "scenario": "healthy 2-disk RAID1",
    "captured": "2026-02-23",
    "command": "btrfs filesystem show /mnt/storage",
    "tool_version": "btrfs-progs v6.x (from nixpkgs <commit>)"
  },
  "stdout": "..."
}
```

Fixtures to capture:
- `lsblk --json` output (multi-disk, single-disk, missing disk)
- `findmnt --json` output (mounted, unmounted)
- `btrfs filesystem show` output (healthy, degraded, missing)
- `btrfs --format json filesystem df` output (RAID1, single)
- `cryptsetup status` output
- `cryptsetup luksUUID` output
- End-to-end live state for: 2-disk RAID1, 3-disk RAID1, degraded, unmounted

**Tests:**
- `parse.rs` (**parser layer, tested independently**): Each parser gets its own unit tests. JSON parsers: valid input → typed struct, malformed input → ParseError. Text parsers: healthy/degraded/missing variants for btrfs filesystem show, cryptsetup status/luksUUID. Every parser has a fail-hard test for garbage input.
- `probe.rs`: End-to-end `discover_live_state()` with MockRunner → `parse.rs` → typed LiveState. Integration tests validate the full pipeline: CmdRequest → raw fixture → typed CmdOutput.
- `identity.rs`: UUID join when mapper name differs from by-id basename. Absent device → None. Non-LUKS device → None. (Consumes typed structs, no parsing here.)

**Commit point:** `cargo test` passes with parse/probe/identity tests against fixtures. Binary still stubs. VM tests untouched.

---

## Phase 3: Planner + Golden Tests

**Create:**
- `cli/src/plan.rs` — `compute_plan(config, live_state, flags)` → `PlanOutcome`, `generate_plan_id()`, plan JSON serialization, human-readable formatting. Consumes `LiveState` (typed struct from probe), never raw command output.

**Define the canonical output contract first.** Design the plan JSON schema and human output format we actually want. Document it. Then:

**Capture reference fixtures:** Run `braid plan --json` in VMs for every scenario in `tests/braid-plan.py` — these are **reference inputs** to understand the bash behavior, not the target output. Each fixture gets provenance metadata (source test, scenario name, capture date). Scenarios:
- No-op, add, remove, replace
- Absent disk (DISK_ABSENT_SKIPPED)
- Non-LUKS disk (INIT_REQUIRED)
- Identity ambiguity (blocked + override)
- Missing device gate (blocked + override)
- Multiple missing (AMBIGUOUS_MISSING)
- Redundancy warning (2→1)
- Bootstrap (empty pool, single disk, two disks)

**Tests:**
- `plan.rs`: One test per scenario — construct LiveState + Config, assert PlanOutcome variant, action types/count, warnings, blocked_reasons, confirmations
- Golden tests: compare Rust planner output against the **new canonical schema** (same action types in same order, same warning codes, same blocked reason codes)
- Property: `compute_plan` never emits a format action (safe-by-construction — enforced by `ActionType` not having a format variant)
- Property: `PlanOutcome::Blocked` cannot produce an `ApplicablePlan`

**Commit point:** `cargo test` passes all planner tests. This is a major milestone — the planner has unit-level coverage matching every scenario.

---

## Phase 3.5: Early VM Planner Validation

**Goal:** Catch contract drift early. The planner is where most semantic differences will surface. Validate against VMs before building the executor.

**Create:**
- `tests/rust-vm/10-braid-plan-rust.nix` — same as `10-braid-plan.nix` but injects Rust binary
- **Updated `tests/braid-plan-rust.py`** — fork of `braid-plan.py` with assertions rewritten to match the new canonical output contract

**Modify:** `flake.nix` — add `braid-plan-rust` check entry.

Wire `braid plan` (and only plan) in `main.rs` — the other subcommands remain stubs.

**Process:** Run `make test-one t=braid-plan-rust`. This validates the new output contract against real system state. Catches:
- JSON schema mismatches against real btrfs/LUKS state
- Warning/blocked reason semantic correctness
- Action ordering correctness
- Parser errors against real tool output (first time parsers see non-fixture data)

**Commit point:** `braid-plan-rust` VM test passes.

---

## Phase 4: Executor + Checkpoints + Init-disk

**Create:**
- `cli/src/exec.rs` — `execute(plan: ApplicablePlan, runner, config)`, action handlers (open_luks, btrfs_add, balance_raid1, remove_graceful, remove_missing, close_luks, verify_health, verify_diskset), checkpoint lifecycle (init, update, finalize), resume logic with config hash validation, confirmation phrase parsing, `BRAID_TEST_FAIL_AFTER_ACTION` hook. All command output goes through `parse.rs` — no raw string handling here.
- `cli/src/init_disk.rs` — **separate module by design**, `cmd_init_disk()`, validation chain (declared-disk, pool-membership refusal, LUKS probe, force gate, passphrase match), `cryptsetup luksFormat` call. Formatting types and functions are private to this module — not exported, not reachable from `exec.rs`.

**Structural safety boundary (Principle 3):** `init_disk.rs` keeps formatting APIs private (`pub(super)` at most for `main.rs` dispatch, not `pub`). `exec.rs` does not import `init_disk.rs`. `ActionType` has no format variant. The compiler enforces the wall — formatting is structurally unreachable from the apply path.

**Tests:**
- `exec.rs`: Execute multi-action plan with MockRunner (verify commands issued in order). Checkpoint create/update/finalize. Resume skips completed. Resume rejects stale config hash. Resume fails on missing target. Confirmation: correct/wrong/missing/multi-phrase. Test hook stops after specified action. No-op prints clean message. Blocked plan rejected at compile time.
- `init_disk.rs`: Format declared non-LUKS disk. Refuse undeclared. Refuse pool member. Refuse already-LUKS without --force. --force needs confirmation. Passphrase check against existing member.

**Commit point:** `cargo test` passes all executor and init-disk tests. Both operational paths covered against mocks.

---

## Phase 5: Status + CLI Wire-Up

**Create:**
- `cli/src/status.rs` — `cmd_status(runner, config, verbose, json)`, human output formatting (Pool/Status/Drives/Profile/Capacity/Last scrub), JSON output (schema_version 1), verbose per-disk detail, `format_bytes()`. Consumes typed structs from `probe.rs`/`parse.rs` — no raw string handling.

**Define the canonical status output contract first.** Design the status JSON schema and human format we want. Then implement to that spec.

**Modify:**
- `cli/src/main.rs` — wire all 4 subcommands to real handlers, global `--config` flag, env var reading (`BRAID_PASSPHRASE`, `BRAID_CONFIRM`, `BRAID_LUKS_OPTS`, `BRAID_TEST_FAIL_AFTER_ACTION`)

**Tests:**
- `status.rs`: Human output contains expected fields. JSON has correct schema. Verbose includes per-disk detail. Degraded shows correct status. Unmounted handled. `format_bytes()` correctness.
- `main.rs`: CLI arg parsing (correct dispatch, flag parsing, unknown flag error)

**Commit point:** The Rust binary is functionally complete. All 4 subcommands work. `cargo test` passes.

---

## Phase 6: Full VM Validation

**Goal:** Run remaining VM test scripts against the Rust binary with updated expectations.

**Create thin wrapper .nix files** in `tests/rust-vm/` (plan already done in 3.5):

```
tests/rust-vm/
  8-braid-status-rust.nix
  11-braid-apply-rust.nix
  12-braid-unified-rust.nix
  13-braid-bootstrap-rust.nix
  14-braid-init-disk-rust.nix
```

Each reuses or forks the `.py` test script with assertions updated to the new canonical output contracts.

**Modify:** `flake.nix` — add remaining `-rust` check entries.

**Process:** Start with status (read-only), then init-disk, bootstrap, apply, unified. Update test assertions to match the designed output contracts.

**Commit point:** All `-rust` VM tests pass alongside original bash tests.

---

## Phase 7: The Swap

**Modify:**
- `modules/braid/cli.nix` — replace `writeShellApplication` with Rust package
- `flake.nix` — point original test entries at Rust binary, remove `-rust` parallel entries
- All test `.nix` files that inject the CLI — update to use Rust package
- Update original `.py` test scripts to match new output contracts (replacing the `-rust` forks)
- Delete `tests/rust-vm/`

**Legacy scripts:**
- `scripts/braid-status.sh` — delete (no test uses it, fully replaced)
- `scripts/braid-add-disk.sh` — delete (already a deprecation stub)
- `scripts/braid.sh` — delete
- `scripts/braid-remove-disk.sh` — delete. Add `braid remove-disk` subcommand to the Rust binary with the interactive workflow. Update tests 9 and 12 to use it. One binary, no shell scripts in the operational path.

**Commit point:** `make test` passes with the Rust binary as production CLI. All bash scripts removed.

---

## Phase 8: Cleanup + Docs

- Update `AGENTS.md` with Rust build/test commands
- Update `README.md` if CLI usage changed
- Add `docs/decisions/rust-migration.md` (Active)
- Clean up any TODO comments in Rust source
- Remove `make test-rust` if integrated into `make test`

**Commit point:** Clean repo. All tests pass. Documentation current.

---

## Dependency Graph

```
Phase 1 (scaffold, types, config, cmd, parse stubs)
  → Phase 2 (parse, probe, identity, command-capabilities.md)
    → Phase 3 (planner + golden tests)
      → Phase 3.5 (early VM planner validation)
      → Phase 4 (executor + init-disk)
        → Phase 5 (status + CLI wire-up)
          → Phase 6 (full VM validation)
            → Phase 7 (the swap)
              → Phase 8 (cleanup + docs)
```

Phase 3.5 and Phase 4 can proceed in parallel — 3.5 validates the planner against VMs while 4 builds the executor using the planner's types.

## Risk Points

1. **Tool output drift** — if nixpkgs updates btrfs-progs/util-linux/cryptsetup, outputs may change. Pinned capabilities doc + fail-hard parsing catches this immediately. Fixture provenance metadata makes re-capture traceable.
2. **Env var handling** — `BRAID_PASSPHRASE`, `BRAID_CONFIRM`, `BRAID_LUKS_OPTS`, `BRAID_TEST_FAIL_AFTER_ACTION` must behave identically.
3. **jq removal** — Rust binary is self-contained (serde, not jq). Nix package must not require jq as runtime dep. Tests may still use jq independently — that's fine.

## Module Boundary Summary

```
cli/src/
  cmd.rs           CommandRunner trait, CmdRequest enum, raw Output
  parse.rs         ALL parsing (JSON + text) → typed CmdOutput structs
  ─────────────────────────────────────────────────────────────────
  probe.rs         LiveState discovery — calls cmd, feeds to parse, returns typed structs
  identity.rs      DeviceIdentity — consumes typed structs, no parsing
  plan.rs          Planner — consumes LiveState + Config, returns PlanOutcome
  exec.rs          Executor — consumes ApplicablePlan, calls cmd+parse for verification
  status.rs        Status — consumes typed probe output, formats for display
  ─────────────────────────────────────────────────────────────────
  init_disk.rs     LUKS formatting — private APIs, not importable by exec.rs
  ─────────────────────────────────────────────────────────────────
  config.rs        Config deserialization
  types.rs         Domain types, newtypes
  main.rs          CLI dispatch
```

## Key Files

- `scripts/braid.sh` — the 1,640-line source being replaced
- `modules/braid/cli.nix` — Nix packaging glue (swap point)
- `tests/10-braid-plan.nix` + `tests/braid-plan.py` — most comprehensive planner test
- `tests/11-braid-apply.nix` + `tests/braid-apply.py` — most comprehensive executor test
- `flake.nix` — build system, test discovery
- `cli/docs/command-capabilities.md` — pinned tool versions and capability matrix (created in Phase 2)
