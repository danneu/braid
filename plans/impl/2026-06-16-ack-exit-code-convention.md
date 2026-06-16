# Align `braid ack` exit codes with the detector/alert convention

## Context

braid has a settled three-tier exit-code convention for its machine-readable
commands:

- **0** = success / expected state
- **1** = operational, retryable, or "negative-but-normal" outcome
- **2** = setup error -- "could not even attempt"; config load or pool-lock I/O,
  never emitted by the command's core logic

Three commands document and follow it: `idle` (`docs/commands/idle.md` exit table,
ADR 016), `wol-ready` (ADR 016, has a `WolReadyOutcome::SetupError` -> exit 2),
and `monitor` (`docs/commands/monitor.md` table, ADR 014, ADR 018, principles.md).
ADR 014 is the authority for the alert pair and states exit 2 is "Reserved for
'could not even attempt to detect'; never emitted by `cmd_monitor` itself."

`braid ack` -- monitor's sibling in the same alert pipeline -- violates this. It
collapses **every** non-success outcome to exit 1: config load (`main.rs:919`),
pool-lock I/O (`acquire_pool_with_timeout_or_exit`, `main.rs:1145-1156`), and all
`cmd_ack` errors (`main.rs:924`). It is also the only command in the family whose
clap help documents no exit codes at all, and `docs/commands/ack.md` has no exit-code
table (only scattered prose). A wrapper that keys off exit 2 for "setup is broken,
do not retry" cannot distinguish a config/lock fault from an ordinary ack failure.

**Outcome:** ack joins the convention. config-load and pool-lock-I/O faults exit 2;
contention and operational ack errors stay exit 1. The codes are documented in ack's
clap help, in `ack.md`, and in ADR 014 (which currently specifies them only for
monitor). New VM subtests pin ack's two exit-2 paths (config-load and pool-lock I/O);
monitor and idle config-load exit 2 are already covered (see Tests).

## Design

The clean invariant, mirroring ADR 014's monitor language: **exit 2 = pre-command
setup failure (config load, pool-lock I/O), never emitted by `cmd_ack` itself.**
Everything `cmd_ack` returns is "attempted and failed" -> exit 1. Contention is the
retryable "system busy" outcome -> exit 1 (ack is interactive, so exit 1, not
monitor's exit-0-skip; principles.md:70 and `ack.md:73` already establish this).

`AckError` variants (`cli/src/ack.rs:254-288`) -- `PoolNotMounted`,
`OfflineBtrfsErrorsRefused`, `Probe`, `Cmd`, `Parse`, `Io`, `CleanupFailed` -- are all
operational and correctly stay exit 1. None map to exit 2.

## Changes

### 1. Code -- `cli/src/main.rs`

**a. Config-load (line 919).** Change the exit code `1` -> `2`:

```rust
let config = load_config_or_exit(Path::new(&config_path), 2);
```

(ack already uses the plain `load_config_or_exit`, same as monitor/idle/wol-ready;
only the code differs.)

**b. Pool-lock I/O split -- `acquire_pool_with_timeout_or_exit` (lines 1145-1156).**
This helper is used **only** by ack (`Timeout` is the sole ack lock policy,
`main.rs:170`), so the change is isolated. It currently exits 1 for every error.
Split it to match the `MonitorSilent` arm (`main.rs:1124-1131`): I/O -> exit 2,
contention -> exit 1. `acquire_with_timeout` returns `PoolLockError::AlreadyHeld` on
contention and `PoolLockError::Io` on a lock-file fault (`pool_lock.rs:57-61, 73-80`),
so the match is exhaustive:

```rust
fn acquire_pool_with_timeout_or_exit(
    pool_lock: &RealPoolLock,
    timeout: Duration,
) -> RealPoolLockGuard {
    match pool_lock.acquire_with_timeout(timeout) {
        Ok(guard) => guard,
        Err(e) => {
            // Lock I/O is a setup fault ("could not even attempt"): exit 2, matching
            // monitor's MonitorSilent arm. Contention is retryable: exit 1.
            // Classify by borrow (`&e`) so `e` is still owned for the handler.
            let exit_code = if matches!(&e, PoolLockError::Io(_)) { 2 } else { 1 };
            handle_pool_lock_error(e);
            std::process::exit(exit_code);
        }
    }
}
```

`cmd_ack` operational errors (line 924) stay exit 1 -- no change.

**c. Clap help (line 80).** ack's `///` is the only one in the family without exit
codes. Match the one-line style of `monitor`/`idle` (ASCII only, per
`check-output-ascii.py`):

```rust
/// Acknowledge current alerts and silence notifications: exit 0 = acknowledged or nothing to ack, exit 1 = lock contention or ack failure, exit 2 = setup error (config load, pool-lock I/O)
```

### 2. Docs

**a. `docs/commands/ack.md`** -- add an `## Exit codes` section after "Basic example"
and before "What happens under the hood" (the placement monitor.md/idle.md use):

| Exit code | Meaning |
|---|---|
| **0** | Alerts acknowledged, or nothing to acknowledge |
| **1** | Lock contention (retry once the other op finishes), or an ack failure (offline btrfs-error refusal, probe/fstype error, cleanup I/O) |
| **2** | Setup error -- config could not be read, or pool-lock I/O error |

Tighten the existing prose for consistency: line 57 "exits non-zero" -> "exits 1"
(line 73 already says exit 1; line 42's "exits 0" stays).

**b. `docs/design/decisions/014-alerts.md`** -- after the monitor exit-codes block
(ends line 87), add a short `braid ack` exit-codes subsection so the authority for the
alert pair documents both. State the same invariant: exit 2 is a pre-`cmd_ack` setup
failure (config load, pool-lock I/O) and is never emitted by `cmd_ack` itself; ack
contention exits 1 (interactive: a missed run must report failure, unlike monitor's
harmless timer skip).

**c. No change expected** (verify during impl): `docs/guides/monitoring-and-alerts.md`
mentions exit codes only in a monitor-specific diagram; ADR 016/018 are idle/monitor
scoped; `README.md` has no ack exit-code claims.

### 3. Tests

**Already covered (cite, do not duplicate):** monitor config-load exit 2 lives in
`tests/cli/braid-monitor.py` ("braid monitor exits 2 on config-load failure" --
asserts exit 2 + the config-path substring for both an unparseable and a missing
config); idle config-load exit 2 in `tests/cli/braid-idle.py` (same shape). New
coverage targets only the genuinely-untested paths.

`tests/cli/braid-monitor.py` is the canonical "monitor + ack lifecycle" test (it
already builds a 3-disk pool and exercises ack repeatedly), so ack's new exit-2
subtests belong there, beside the existing monitor config-load subtest and reusing its
pool:

- **ack config-load exit 2** (gates the line-919 change; exits 1 today) -- mirror the
  monitor subtest: `braid ack --config /tmp/bad.json` and `--config /tmp/nonexistent.json`
  each assert `status == 2` and the config path appears in output (proving it is the
  `config_read` path, not a clap usage error -- which also exits 2). Place it right after
  the monitor config-load subtest, where the pool is healthy and the lock is acquirable,
  so config-load is the only failing variable.

- **ack pool-lock I/O exit 2** (gates the F1 / `Io -> 2` branch) -- poison the lock path
  itself so `open_lock_file` (`O_RDWR|O_CREAT`, `pool_lock.rs:208`) fails *before* config
  load: `rm -f /run/braid-pool.lock && mkdir /run/braid-pool.lock`, run `braid ack` (valid
  default config), assert `status == 2`, then `rmdir /run/braid-pool.lock` to restore for
  later subtests. A directory opened `O_RDWR` returns EISDIR -> `PoolLockError::Io` ->
  exit 2. **Assertion caveat:** `handle_pool_lock_error` prints the *inner* io error, not
  the `"pool lock I/O error"` wrapper, so assert exit 2 plus `"directory" in output.lower()`
  (the EISDIR text), which also separates it from contention (exit 1, "already in
  progress"). Reverting the `Io -> 2` branch flips this to exit 1, so the subtest pins the
  behavioral change.

- **wol-ready config-load exit 2** (closes a *pre-existing* gap, not part of the ack
  change) -- `tests/module/braid-auto-suspend.py` asserts wol-ready exit 0/1 (lines
  234-246) but not config-load. Add a subtest after line 246 mirroring the idle one:
  `braid wol-ready --config /tmp/bad.json` -> exit 2 + path. wol-ready already exits 2
  here (`main.rs:832`); this just records the contract.

No new test file or `flake.nix` registration is needed -- all three subtests extend
tests already registered under `checks`.

Coverage notes:
- The **ack contention exit-1** path is already exercised
  (`alert-state-lock.py:315-349` asserts ack succeeds after the holder releases).
- All three subtests are behavioral and structure-insensitive (observe exit code + a
  diagnostic substring); the two ack subtests fail before the change and pass after.

### Rejected alternative

Documenting ack's collapse-to-exit-1 "by design" (the finding's fallback): rejected --
it leaves ack the lone violator of a four-command convention that ADR 014 already
establishes for its own pipeline pair, for no benefit. Refactoring dispatch to return
an exit code from a pure function (to enable Rust-level exit-code unit tests): rejected
as out of scope -- it would touch every command's dispatch arm; the codebase tests
exit codes at the VM level (`machine.execute` -> rc), and the new subtests match that.

## Verification

1. `just test-rust` -- Rust units still pass (no unit-level exit-code change).
2. `just test-vm braid-monitor` -- the new ack config-load and ack pool-lock-I/O
   subtests assert exit 2 (both fail before the change, pass after); monitor's existing
   exit-2 subtest still passes.
3. `just test-vm braid-auto-suspend` -- the new wol-ready config-load exit-2 subtest passes.
4. `just test-vm alert-state-lock` -- ack contention behavior unchanged (exit 0 after
   holder release).
5. `just docs-build` -- mdBook link/anchor check for the `ack.md` table and ADR 014 edit.
6. `just check-output-ascii` -- the new clap help line is ASCII-clean.
7. Manual smoke: `braid --config /nonexistent/x.json ack; echo $?` -> `2`; then poison
   the lock (`mkdir /run/braid-pool.lock`) and `braid ack; echo $?` -> `2`
   (`rmdir` after).

## Critical files

- `cli/src/main.rs` -- exit-code edits (config-load line 919, lock split lines 1145-1156, clap help line 80)
- `cli/src/pool_lock.rs` -- read-only reference for the `PoolLockError` split + `open_lock_file`
- `cli/src/ack.rs` -- read-only reference for `AckError` variants (all stay exit 1)
- `docs/commands/ack.md` -- new exit-code table + prose tidy
- `docs/design/decisions/014-alerts.md` -- ack exit-codes subsection
- `tests/cli/braid-monitor.py` -- add ack config-load + ack pool-lock-I/O exit-2 subtests
- `tests/module/braid-auto-suspend.py` -- add wol-ready config-load exit-2 subtest (pre-existing gap)
