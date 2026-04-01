# Plan: SIGINT cleanup during `braid unlock`

## Context

When a user presses Ctrl-C during `braid unlock`, the process dies mid-loop.
LUKS disks are opened sequentially in `mount::open_and_mount_pool` (mount.rs:151-163).
If SIGINT arrives after disk 1 is opened but before disk 3, disk 1's mapper
(`/dev/mapper/braid-aaa`) stays decrypted. The user expects Ctrl-C to abort
cleanly — no stale decrypted mappers.

SIGINT is delivered to the entire foreground process group (parent + child
cryptsetup), so Ctrl-C can kill a running `cryptsetup open` child rather than
landing neatly between loop iterations. The cleanup logic must handle both cases.

## Approach

**Centralized cancellation gate** using `signal-hook` (already compiled as a
transitive dep via crossterm/ratatui — promote to direct).

1. Register SIGINT handler that sets an `Arc<AtomicBool>` flag
2. Also register `register_conditional_default` on the same flag — second
   Ctrl-C force-kills the process (standard double-Ctrl-C pattern)
3. Pass `&AtomicBool` into `open_and_mount_pool`
4. Track opened mappers in a `Vec<String>` during the unlock section
5. **Centralized gate after the unlock section:** on ANY error, if the flag is
   set, close all opened mappers and return `Interrupted`
6. Also check the flag between iterations (catches the clean-boundary case
   without waiting for the next open to fail)

This handles all interrupt scenarios:
- **Between iterations:** flag check catches it, cleanup runs
- **During child cryptsetup:** child killed → `ensure_luks_open` returns error →
  centralized gate sees error + flag → cleanup runs
- **During passphrase prompt:** flag set, rpassword keeps blocking (SA_RESTART),
  second Ctrl-C force-kills (no mappers open, clean)
- **During verify_passphrase:** child killed → error → gate sees flag → cleanup
  (no mappers open yet, returns `Interrupted { cleaned: 0 }`)

## Changes

### 1. `cli/Cargo.toml` — add direct dependency

```toml
signal-hook = "0.3"
```

### 2. `cli/src/mount.rs` — core logic

**New error variant:**
```rust
#[error("interrupted — cleaned up {cleaned} opened mapper(s)")]
Interrupted { cleaned: usize },
```

**New parameter** on `open_and_mount_pool`:
```rust
interrupt: &AtomicBool
```

**New helper** (private):
```rust
fn close_opened_mappers<R: CommandRunner>(runner: &R, mappers: &[String]) -> usize
```
Calls `CmdRequest::CryptsetupClose` for each. No retry — just-opened, never-
mounted mappers cannot be busy. Best-effort: logs warnings on failure, returns
count closed.

**Restructured unlock section** (step 4, lines 109-165):

```rust
let mut opened_mappers: Vec<String> = Vec::new();

// Wrap credential+open in a closure so errors fall through to the gate
let open_result: Result<(), MountError> = (|| {
    if to_unlock.is_empty() { return Ok(()); }

    match &credential {
        Credential::Passphrase { passphrase_stdin, passphrase_file } => {
            let passphrase = luks::read_passphrase(*passphrase_file, *passphrase_stdin)?;

            let (ref first_name, ref first_by_id) = to_unlock[0];
            let ok = luks::verify_passphrase(runner, &first_by_id.0, &passphrase)?;
            if !ok {
                return Err(MountError::Failed(format!(
                    "wrong passphrase (verified against {})", first_name
                )));
            }

            for (name, by_id) in &to_unlock {
                // Check between iterations
                if interrupt.load(Ordering::Relaxed) {
                    return Err(MountError::Interrupted { cleaned: 0 });
                }
                luks::ensure_luks_open(runner, fs, name, by_id, &passphrase)
                    .map_err(|_| MountError::Failed(format!(
                        "failed to open disk '{}': passphrase verified against \
                         '{}' but rejected here (single-passphrase invariant \
                         may be violated)", name, first_name
                    )))?;
                opened_mappers.push(config::mapper_name(name).0.clone());
                eprintln!("{}  disk: {:<10}unlocked", tag("ok"), name);
            }
        }
        Credential::KeyFile(kf) => {
            // Same pattern: verify, loop with flag check, track opened_mappers
        }
    }
    Ok(())
})();

// ── Centralized cancellation gate ──
// On ANY error, if the interrupt flag is set, clean up and return Interrupted.
// This catches: child killed by SIGINT, flag set between iterations, errors
// during verify, errors during read_passphrase — all of them.
if open_result.is_err() && interrupt.load(Ordering::Relaxed) {
    let cleaned = close_opened_mappers(runner, &opened_mappers);
    return Err(MountError::Interrupted { cleaned });
}
open_result?;
```

The closure borrows `&mut opened_mappers` during execution; after it returns,
the borrow is released and `close_opened_mappers` can read the vec.

### 3. `cli/src/unlock.rs` — thread parameter

Add `interrupt: &AtomicBool` to `cmd_unlock` signature, pass to
`open_and_mount_pool`.

### 4. `cli/src/recover.rs` — same treatment

Add `interrupt: &AtomicBool` to `cmd_recover` signature, pass to
`open_and_mount_pool`. Wire it from main.rs with the same signal registration.
Recover is the "interrupted operation" path — leaving mappers open here is
worse, not better.

### 5. `cli/src/main.rs` — signal registration

In both `Commands::Unlock` and `Commands::Recover` arms:

```rust
let interrupt = Arc::new(AtomicBool::new(false));
signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&interrupt))
    .expect("failed to register SIGINT handler");
signal_hook::flag::register_conditional_default(
    signal_hook::consts::SIGINT, Arc::clone(&interrupt),
).expect("failed to register conditional default handler");
```

- First Ctrl-C: sets flag, cleanup runs
- Second Ctrl-C: flag already set → default handler → process killed

New match arm for `Interrupted` → exit code 130 (128 + SIGINT).

### 6. Update existing tests

All calls to `open_and_mount_pool` (8 in mount.rs, 1 in recover.rs) and
`cmd_unlock` (4 in unlock.rs) get `&AtomicBool::new(false)` as the new arg.
Same for any `cmd_recover` test calls.

### 7. New unit tests in `mount.rs`

**Test A: interrupt before first open.** Flag set to `true` before call.
Pre-loop check catches it. `Interrupted { cleaned: 0 }`. No `CryptsetupClose`.

**Test B: interrupt between iterations.** Custom `CommandRunner` wrapper sets
the flag when `CryptsetupLuksOpen` for disk1 succeeds. Between-iteration check
catches it before disk2 opens. `Interrupted { cleaned: 1 }`. Assert
`CryptsetupClose` for `braid-disk1` called. Assert disk2/disk3 never attempted.

**Test C: child killed by SIGINT during disk2 open.** Custom runner: disk1 open
succeeds (flag stays false), disk2 open sets flag AND returns error (simulating
killed child). Centralized gate catches error + flag, cleans disk1.
`Interrupted { cleaned: 1 }`.

### 8. VM test: `tests/cli/unlock-sigint-cleanup`

**`unlock-sigint-cleanup.nix`:** 3 × 1024MB disks, braid + cryptsetup +
btrfs-progs + procps, config.json.

**`unlock-sigint-cleanup.py`:**

```
Setup:
  braid add × 3 (fast LUKS: pbkdf2, 1000 iterations)
  close_all()

  Re-key LUKS with slow params to widen the timing window:
    for each disk: cryptsetup luksConvertKey --pbkdf argon2id \
        --pbkdf-force-iterations 4 --pbkdf-memory 1048576

Subtest "SIGINT during unlock closes opened mappers":
  Run as a single native shell script (no Python roundtrip overhead).
  Launch unlock in its own process group (setsid) so we can signal the
  whole group — matching real Ctrl-C behavior where SIGINT hits both
  the parent braid process AND any running cryptsetup child:

    setsid sh -c 'printf "%s\n" "$PASSPHRASE" | braid unlock --passphrase-stdin' &
    PID=$!
    for i in $(seq 1 200); do
      test -e /dev/mapper/braid-disk1 && break
      sleep 0.02
    done
    kill -INT -- -$PID          # negative PID = signal the entire process group
    wait $PID || true

  Assert: no /dev/mapper/braid-* mappers exist
  Assert: /mnt/storage not mounted

Subtest "pool still usable after cleanup":
  Full unlock succeeds, pool mounts, all 3 mappers open
```

**`flake.nix`:** Register as `unlock-sigint-cleanup` in CLI test section
using `linuxCrane.braid`.

## Files to modify

| File | Change |
|---|---|
| `cli/Cargo.toml` | add `signal-hook = "0.3"` |
| `cli/src/mount.rs` | `Interrupted` variant, `interrupt` param, `close_opened_mappers`, centralized gate, update 8 test calls, 3 new tests |
| `cli/src/unlock.rs` | thread `interrupt` param, update 4 test calls |
| `cli/src/recover.rs` | thread `interrupt` param, update test calls if any |
| `cli/src/main.rs` | signal registration (unlock + recover), `Interrupted` match arms, exit 130 |
| `tests/cli/unlock-sigint-cleanup.nix` | new |
| `tests/cli/unlock-sigint-cleanup.py` | new |
| `flake.nix` | register new test |

## Verification

1. `just test-rust` — all unit tests pass (existing + 3 new)
2. `just test unlock-sigint-cleanup` — VM test passes
3. `just test braid-unlock` — existing unlock tests still pass
4. `just test braid-recover` — existing recover tests still pass
