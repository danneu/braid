# Rust CLI Migration Plan

## Context

Replacing `scripts/braid.sh` (~1,640 lines) with a Rust binary. The bash is working and has 40+ passing VM tests. The rewrite gains type-safe device identity, serde for all JSON, and fast unit tests via a CommandRunner trait. See `brainstorm/claude-rust-rewrite.md` for full rationale.

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

**Capture fixtures first:** Run bash `discover_live_state` in VMs, save JSON outputs to `cli/tests/fixtures/`:
- `live_state_healthy_raid1.json` — 2-disk RAID1
- `live_state_3disk_raid1.json` — 3-disk RAID1
- `live_state_degraded.json` — 1 missing device
- `live_state_unmounted.json` — pool not mounted

Also capture raw command outputs (`btrfs filesystem show`, `cryptsetup status`, etc.) for parser unit tests.

**Tests:**
- `probe.rs`: Parse btrfs filesystem show (healthy, degraded, missing). Parse cryptsetup status. End-to-end `discover_live_state()` with MockRunner against fixtures.
- `identity.rs`: UUID join when mapper name differs from by-id basename. Absent device → None. Non-LUKS device → None.

**Commit point:** `cargo test` passes with probe/identity tests against fixtures. Binary still stubs. VM tests untouched.

---

## Phase 3: Planner + Golden Tests

**Create:**
- `cli/src/plan.rs` — `compute_plan(config, live_state, flags)` → `PlanOutcome`, `generate_plan_id()`, plan JSON serialization, human-readable formatting

**Capture fixtures first:** Run `braid plan --json` in VMs for every scenario in `tests/braid-plan.py`:
- No-op, add, remove, replace
- Absent disk (DISK_ABSENT_SKIPPED)
- Non-LUKS disk (INIT_REQUIRED)
- Identity ambiguity (blocked + override)
- Missing device gate (blocked + override)
- Multiple missing (AMBIGUOUS_MISSING)
- Redundancy warning (2→1)
- Bootstrap (empty pool, single disk, two disks)

Save to `cli/tests/fixtures/plan_*.json`.

**Tests:**
- `plan.rs`: One test per scenario — construct LiveState + Config, assert PlanOutcome variant, action types/count, warnings, blocked_reasons, confirmations
- Golden tests: serialize Rust planner output, compare against captured bash JSON (ignoring plan_id)
- Property: `compute_plan` never emits a format action (safe-by-construction)
- Property: `PlanOutcome::Blocked` cannot produce an `ApplicablePlan`

**Commit point:** `cargo test` passes all planner tests with golden parity. This is a major milestone — the planner has unit-level coverage matching every VM scenario.

---

## Phase 4: Executor + Checkpoints + Init-disk

**Create:**
- `cli/src/exec.rs` — `execute(plan: ApplicablePlan, runner, config)`, action handlers (open_luks, btrfs_add, balance_raid1, remove_graceful, remove_missing, close_luks, verify_health, verify_diskset), checkpoint lifecycle (init, update, finalize), resume logic with config hash validation, confirmation phrase parsing, `BRAID_TEST_FAIL_AFTER_ACTION` hook
- `cli/src/init_disk.rs` — **separate module by design**, `cmd_init_disk()`, validation chain (declared-disk, pool-membership refusal, LUKS probe, force gate, passphrase match), `cryptsetup luksFormat` call

**Architectural enforcement:** `exec.rs` does not import `init_disk.rs`. `ActionType` enum has no format variant. The typestate wall: `InitDiskOp` is structurally unreachable from the apply path.

**Tests:**
- `exec.rs`: Execute multi-action plan with MockRunner (verify commands issued in order). Checkpoint create/update/finalize. Resume skips completed. Resume rejects stale config hash. Resume fails on missing target. Confirmation: correct/wrong/missing/multi-phrase. Test hook stops after specified action. No-op prints clean message. Blocked plan rejected at compile time.
- `init_disk.rs`: Format declared non-LUKS disk. Refuse undeclared. Refuse pool member. Refuse already-LUKS without --force. --force needs confirmation. Passphrase check against existing member.

**Commit point:** `cargo test` passes all executor and init-disk tests. Both operational paths covered against mocks.

---

## Phase 5: Status + CLI Wire-Up

**Create:**
- `cli/src/status.rs` — `cmd_status(runner, config, verbose, json)`, human output formatting (Pool/Status/Drives/Profile/Capacity/Last scrub), JSON output (schema_version 1), verbose per-disk detail, `format_bytes()`

**Modify:**
- `cli/src/main.rs` — wire all 4 subcommands to real handlers, global `--config` flag, env var reading (`BRAID_PASSPHRASE`, `BRAID_CONFIRM`, `BRAID_LUKS_OPTS`, `BRAID_TEST_FAIL_AFTER_ACTION`)

**Capture fixtures:** `braid status --json` and `braid status --json --verbose` for healthy, degraded, unmounted.

**Tests:**
- `status.rs`: Human output format, JSON fields, verbose per-disk detail, degraded output, unmounted output, `format_bytes()` correctness
- `main.rs`: CLI arg parsing (correct dispatch, flag parsing, unknown flag error)
- Golden: status JSON matches bash fixtures

**Commit point:** The Rust binary is functionally complete. All 4 subcommands work. `cargo test` passes. Can be manually tested in playground VM. VM tests still untouched (bash).

---

## Phase 6: VM Parity Validation

**Goal:** Run the existing VM test scripts against the Rust binary to prove behavioral parity.

**Create thin wrapper .nix files** in `tests/rust-vm/` — identical to originals except they inject the Rust binary instead of the bash script:

```
tests/rust-vm/
  8-braid-status-rust.nix
  10-braid-plan-rust.nix
  11-braid-apply-rust.nix
  12-braid-unified-rust.nix
  13-braid-bootstrap-rust.nix
  14-braid-init-disk-rust.nix
```

Each reuses the **same .py test script** as the original. The only change is the package injection:
```nix
# instead of writeShellApplication { text = builtins.readFile ../scripts/braid.sh; }
braid-cli = pkgs.callPackage ../../cli { };
```

**Modify:** `flake.nix` — add `-rust` test entries alongside originals.

**Process:** Start with `braid-status-rust` (read-only, lowest risk). Progress through plan, init-disk, bootstrap, apply, unified. Fix output format discrepancies as they surface.

**Commit point:** All `-rust` VM tests pass alongside original bash tests. Both sets run in `nix flake check`.

---

## Phase 7: The Swap

**Modify:**
- `modules/braid/cli.nix` — replace `writeShellApplication` with Rust package
- `flake.nix` — point original test entries at Rust binary, remove `-rust` parallel entries
- All test `.nix` files that inject the CLI — update to use Rust package
- Delete `tests/rust-vm/` (no longer needed)

**Legacy scripts:**
- `scripts/braid-status.sh` — delete (not used by any test, fully replaced)
- `scripts/braid-add-disk.sh` — keep as-is (already a deprecation stub)
- `scripts/braid-remove-disk.sh` — keep as-is (interactive UX that `braid apply` doesn't replicate; used by tests 9 and 12)
- `scripts/braid.sh` — archive or delete

**Commit point:** `make test` passes with the Rust binary as production CLI. Bash script removed from the active path.

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
      → Phase 4 (executor + init-disk)
        → Phase 5 (status + CLI wire-up)
          → Phase 6 (VM parity validation)
            → Phase 7 (the swap)
              → Phase 8 (cleanup + docs)
```

## Risk Points

1. **Output format parity** — VM tests assert exact strings. Rust must produce byte-identical output. Surfaces in Phase 6.
2. **btrfs/cryptsetup parsing** — bash uses awk/grep tuned to specific formats. Fixtures captured in Phase 2 de-risk this.
3. **Env var handling** — `BRAID_PASSPHRASE`, `BRAID_CONFIRM`, `BRAID_LUKS_OPTS`, `BRAID_TEST_FAIL_AFTER_ACTION` must behave identically.
4. **jq removal** — Rust binary is self-contained (serde, not jq). Nix package must not require jq as runtime dep.

## Key Files

- `scripts/braid.sh` — the 1,640-line source being replaced
- `modules/braid/cli.nix` — Nix packaging glue (swap point)
- `tests/10-braid-plan.nix` + `tests/braid-plan.py` — most comprehensive planner test
- `tests/11-braid-apply.nix` + `tests/braid-apply.py` — most comprehensive executor test
- `flake.nix` — build system, test discovery
