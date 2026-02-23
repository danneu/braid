# Rust CLI Migration Plan

## Context

Replacing `scripts/braid.sh` (~1,640 lines) with a Rust binary. The rewrite gains type-safe device identity, serde for all JSON, and fast unit tests via a CommandRunner trait. See `brainstorm/claude-rust-rewrite.md` for full rationale.

**This is not a compatibility shim.** We are building the most robust, simple, and correct implementation. Outputs should be what we actually want, not backward-compatible with the bash. JSON uses schema/semantic parity (same fields, same meaning). Human text stays stable only where VM tests assert exact strings.

Architecture decisions already locked in:
- Single crate with modules (not 4-crate workspace)
- CommandRunner trait for mockable testing
- Typestate for permanent invariants (`InitDiskOp` separate from reconciliation, executor accepts only `ApplicablePlan`)
- Runtime enums for serialized state (`ActionStatus`, `PlanOutcome`)
- LUKS UUID as canonical join key

Source lives in `cli/` (matching existing `daemon/` pattern).

---

## Phase 1: Crate Scaffold + Domain Types

**Create:**
- `cli/Cargo.toml` — deps: clap, serde, serde_json, thiserror, uuid
- `cli/src/main.rs` — clap skeleton with 4 subcommands, all print "not yet implemented"
- `cli/src/types.rs` — newtypes (`ByIdPath`, `LuksUuid`, `MapperName`), `ActionType` enum, `ActionStatus` enum with `transition_to()` validator, `PlanOutcome`, `ApplicablePlan` newtype, `Action`, `BlockedReason`, `Confirmation`, `Warning`
- `cli/src/config.rs` — `Config` struct with serde, `config_read()`, `config_hash()`
- `cli/src/cmd.rs` — `CommandRunner` trait, `RealRunner`, `MockRunner`

**Modify:**
- `flake.nix` — add `braid-rust` package via `rustPlatform.buildRustPackage` (not wired to tests yet)
- `Makefile` — add `make test-rust` target (`cd cli && cargo test`)

**Tests (`#[cfg(test)]` inline):**
- Config: deserialize valid JSON, reject missing fields, reject empty disks
- Types: ActionStatus transitions (valid + invalid), serde round-trips, newtype conversions
- Cmd: MockRunner returns expected outputs

**Commit point:** `cargo test` passes. `nix build .#braid-rust` produces a binary. `make test` still passes (bash untouched).

---

## Phase 2: Probe + Identity Resolution

**Create:**
- `cli/src/probe.rs` — `LiveState` struct, `discover_live_state(runner, config)`, parsers for `btrfs filesystem show`, `btrfs filesystem df`, `cryptsetup status`, `cryptsetup luksUUID`
- `cli/src/identity.rs` — `DeviceIdentity` struct, `resolve_config_identity()`, `resolve_pool_identity()`, UUID-based join logic

**Capture fixtures first:** Run bash in VMs, save raw command outputs to `cli/tests/fixtures/`. Each fixture file includes provenance metadata:

```json
{
  "_provenance": {
    "source_test": "tests/10-braid-plan.nix",
    "scenario": "healthy 2-disk RAID1",
    "captured": "2026-02-23",
    "command": "btrfs filesystem show /mnt/storage"
  },
  "stdout": "..."
}
```

Fixtures to capture:
- Raw `btrfs filesystem show` output (healthy, degraded, missing)
- Raw `cryptsetup status` output
- Raw `cryptsetup luksUUID` output
- Raw `lsblk -J` output
- End-to-end live state for: 2-disk RAID1, 3-disk RAID1, degraded, unmounted

**Tests:**
- `probe.rs`: Parse btrfs filesystem show (healthy, degraded, missing). Parse cryptsetup status. End-to-end `discover_live_state()` with MockRunner against fixtures.
- `identity.rs`: UUID join when mapper name differs from by-id basename. Absent device → None. Non-LUKS device → None.

**Commit point:** `cargo test` passes with probe/identity tests against fixtures. Binary still stubs. VM tests untouched.

---

## Phase 3: Planner + Golden Tests

**Create:**
- `cli/src/plan.rs` — `compute_plan(config, live_state, flags)` → `PlanOutcome`, `generate_plan_id()`, plan JSON serialization, human-readable formatting

**Capture fixtures first:** Run `braid plan --json` in VMs for every scenario in `tests/braid-plan.py`. Each fixture gets provenance metadata (source test, scenario name, capture date). Scenarios:
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
- Golden tests: compare Rust planner output against captured fixtures on **schema/semantic level** (same action types in same order, same warning codes, same blocked reason codes), not byte-identical JSON
- Property: `compute_plan` never emits a format action (safe-by-construction)
- Property: `PlanOutcome::Blocked` cannot produce an `ApplicablePlan`

**Commit point:** `cargo test` passes all planner tests. This is a major milestone — the planner has unit-level coverage matching every VM scenario.

---

## Phase 3.5: Early VM Planner Validation

**Goal:** Catch contract drift early. The planner is where most semantic differences between bash and Rust will surface. Validate it against VMs now, before building the executor.

**Create:**
- `tests/rust-vm/10-braid-plan-rust.nix` — same as `10-braid-plan.nix` but injects Rust binary

**Modify:** `flake.nix` — add `braid-plan-rust` check entry.

Wire `braid plan` (and only plan) in `main.rs` — the other subcommands remain stubs.

**Process:** Run `make test-one t=braid-plan-rust`. Fix output differences. This catches:
- JSON field naming/structure mismatches
- Warning/blocked reason code differences
- Action ordering differences
- Human-readable text differences where tests assert exact strings

**Commit point:** `braid-plan-rust` VM test passes alongside original bash test.

---

## Phase 4: Executor + Checkpoints + Init-disk

**Create:**
- `cli/src/exec.rs` — `execute(plan: ApplicablePlan, runner, config)`, action handlers (open_luks, btrfs_add, balance_raid1, remove_graceful, remove_missing, close_luks, verify_health, verify_diskset), checkpoint lifecycle (init, update, finalize), resume logic with config hash validation, confirmation phrase parsing, `BRAID_TEST_FAIL_AFTER_ACTION` hook
- `cli/src/init_disk.rs` — **separate module by design**, `cmd_init_disk()`, validation chain (declared-disk, pool-membership refusal, LUKS probe, force gate, passphrase match), `cryptsetup luksFormat` call

**Architectural enforcement:** `exec.rs` does not import `init_disk.rs`. `ActionType` enum has no format variant. The typestate wall: `InitDiskOp` is structurally unreachable from the apply path.

**Invariant test (Principle 3):** Explicit test that `exec.rs` source does not contain `luksFormat` — either grep-based (`include_str!` the file and assert no match) or compile-time boundary check. This mirrors the VM test in `braid-apply.py` but enforces it at the Rust level.

**Tests:**
- `exec.rs`: Execute multi-action plan with MockRunner (verify commands issued in order). Checkpoint create/update/finalize. Resume skips completed. Resume rejects stale config hash. Resume fails on missing target. Confirmation: correct/wrong/missing/multi-phrase. Test hook stops after specified action. No-op prints clean message. Blocked plan rejected at compile time.
- `init_disk.rs`: Format declared non-LUKS disk. Refuse undeclared. Refuse pool member. Refuse already-LUKS without --force. --force needs confirmation. Passphrase check against existing member.
- **Invariant**: `exec.rs` does not reference `luksFormat` paths.

**Commit point:** `cargo test` passes all executor and init-disk tests. Both operational paths covered against mocks.

---

## Phase 5: Status + CLI Wire-Up

**Create:**
- `cli/src/status.rs` — `cmd_status(runner, config, verbose, json)`, human output formatting (Pool/Status/Drives/Profile/Capacity/Last scrub), JSON output (schema_version 1), verbose per-disk detail, `format_bytes()`

**Modify:**
- `cli/src/main.rs` — wire all 4 subcommands to real handlers, global `--config` flag, env var reading (`BRAID_PASSPHRASE`, `BRAID_CONFIRM`, `BRAID_LUKS_OPTS`, `BRAID_TEST_FAIL_AFTER_ACTION`)

**Tests:**
- `status.rs`: Human output contains expected fields. JSON has correct schema. Verbose includes per-disk detail. Degraded shows correct status. Unmounted handled. `format_bytes()` correctness.
- `main.rs`: CLI arg parsing (correct dispatch, flag parsing, unknown flag error)

**Commit point:** The Rust binary is functionally complete. All 4 subcommands work. `cargo test` passes.

---

## Phase 6: Full VM Parity Validation

**Goal:** Run remaining VM test scripts against the Rust binary.

**Create thin wrapper .nix files** in `tests/rust-vm/` (plan already done in 3.5):

```
tests/rust-vm/
  8-braid-status-rust.nix
  11-braid-apply-rust.nix
  12-braid-unified-rust.nix
  13-braid-bootstrap-rust.nix
  14-braid-init-disk-rust.nix
```

Each reuses the **same .py test script** as the original. Only the package injection changes.

**Modify:** `flake.nix` — add remaining `-rust` check entries.

**Process:** Start with status (read-only), then init-disk, bootstrap, apply, unified. Fix differences as they surface.

**Commit point:** All `-rust` VM tests pass alongside original bash tests.

---

## Phase 7: The Swap

**Modify:**
- `modules/braid/cli.nix` — replace `writeShellApplication` with Rust package
- `flake.nix` — point original test entries at Rust binary, remove `-rust` parallel entries
- All test `.nix` files that inject the CLI — update to use Rust package
- Delete `tests/rust-vm/`

**Legacy scripts:**
- `scripts/braid-status.sh` — delete (no test uses it, fully replaced)
- `scripts/braid-add-disk.sh` — delete (already a deprecation stub)
- `scripts/braid.sh` — delete
- `scripts/braid-remove-disk.sh` — delete. Its interactive confirmation UX (`read -r`) is not replicated in `braid apply`, but it should be. Add a `braid remove-disk` subcommand to the Rust binary that provides the same interactive workflow. Update tests 9 and 12 to use it. This is the clean end-state — one binary, no shell scripts in the operational path.

**Commit point:** `make test` passes with the Rust binary as production CLI. All bash scripts removed from the active path.

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
Phase 1 (scaffold, types, config, cmd)
  → Phase 2 (probe, identity)
    → Phase 3 (planner + golden tests)
      → Phase 3.5 (early VM planner validation)
      → Phase 4 (executor + init-disk)
        → Phase 5 (status + CLI wire-up)
          → Phase 6 (full VM parity validation)
            → Phase 7 (the swap)
              → Phase 8 (cleanup + docs)
```

Phase 3.5 and Phase 4 can proceed in parallel — 3.5 validates the planner against VMs while 4 builds the executor using the planner's types.

## Risk Points

1. **Human text differences** — VM tests assert some exact strings. Schema/semantic parity is the goal for JSON; match exact strings only where tests require them. Surfaces in Phases 3.5 and 6.
2. **btrfs/cryptsetup parsing** — bash uses awk/grep tuned to specific formats. Fixtures captured in Phase 2 de-risk this.
3. **Env var handling** — `BRAID_PASSPHRASE`, `BRAID_CONFIRM`, `BRAID_LUKS_OPTS`, `BRAID_TEST_FAIL_AFTER_ACTION` must behave identically.
4. **jq removal** — Rust binary is self-contained (serde, not jq). Nix package must not require jq as runtime dep. Tests may still use jq independently — that's fine.

## Key Files

- `scripts/braid.sh` — the 1,640-line source being replaced
- `modules/braid/cli.nix` — Nix packaging glue (swap point)
- `tests/10-braid-plan.nix` + `tests/braid-plan.py` — most comprehensive planner test
- `tests/11-braid-apply.nix` + `tests/braid-apply.py` — most comprehensive executor test
- `flake.nix` — build system, test discovery
