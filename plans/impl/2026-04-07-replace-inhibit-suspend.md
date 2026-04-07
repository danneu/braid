# replace: inhibit suspend during the operation

Fixes [danneu/braid#45](https://github.com/danneu/braid/issues/45). Tracked by [#48](https://github.com/danneu/braid/issues/48).

## Context

`braid replace` runs `btrfs replace start -B` and blocks until it finishes — typically several minutes to hours. While the host is mid-replace, two distinct failure modes can be triggered by a suspend:

- **Path A (any kernel):** an unclean wake/interruption mid-replace leaves the kernel's resume-on-mount path to pick the replace back up later, but it does **not** perform the post-completion devid swap. The pool ends up with five device entries — replace source still present, target added at temporary devid 0, plus a phantom `MISSING devid 0` placeholder — and `braid status` reports DEGRADED. This is the bug pinned by `tests/repro/btrfs-replace-interrupted-mid-flight.py`.
- **Path B (v6.19+ kernels):** systemd's cgroup freeze around suspend triggers the new freeze/signal cancellation path inside the scrub worker loop, transitioning the replace to `CANCELED` (terminal). The user has to start over.

Upstream btrfs explicitly recommends inhibiting suspend during replace — `reference/btrfs-progs/Documentation/btrfs-replace.rst:49-50`. braid enables `autosuspend` by default, so this interaction is reachable in normal operation.

The fix is no-regret: a thin RAII helper around the long-running portion of the replace intent flow + one VM test, eliminating the most plausible real-world trigger of the broken-degraded topology bug on every kernel and preempting the v6.19+ Path B trigger.

## Approach

Add a small `SleepInhibitor` RAII helper that holds a `What=sleep, Who=braid, Mode=block` logind inhibitor for the lifetime of a Rust value, and drop it into `cmd_replace` so the inhibitor covers the entire long-running portion of the intent command — including the `maybe_restore_raid1` soft balance that runs after a missing-path replace ([cli/src/replace.rs:339-347](/Users/dan/Code/braid/cli/src/replace.rs#L339)).

Why a helper rather than mutating `BtrfsReplaceStart::to_argv()`:

- **The soft balance tail is in scope.** A missing-path replace runs `maybe_restore_raid1` at `replace.rs:339-347` after `pool_replace_device` returns. That soft balance is itself long-running and equally suspend-vulnerable. Wrapping only the BtrfsReplaceStart subcommand would leave a documented long tail of the same intent command exposed and weaken Principle 3's end-to-end safety story. A helper scoped to the intent function covers both.
- **Keeps systemd-specific behavior out of `cmd.rs` argv rendering.** `cmd.rs` stays a pure parser/renderer of subprocess invocations.
- **RAII handles every error path.** Drop fires on early `?`-returns from any of the steps inside the scope, so the inhibitor is always released even on partial failure.
- **`systemd-inhibit` is already on PATH.** `modules/braid/wrapper.nix:11` already includes `pkgs.systemd` in `toolPackages`. No NixOS module change required. `busctl` (used by the test below) ships from the same package.

`--mode=block` (hard refuse) is the right choice over `--mode=delay` per the issue description and the upstream btrfs doc recommendation.

**Failure-to-acquire is a hard error, and acquisition happens *before* the journal is written.** Inhibitor acquisition is side-effect-free (process-scoped — Drop kills the systemd-inhibit child and logind releases the lock with it). Acquiring it before `journal::write_journal` means a logind failure cleanly errors out without stranding the user in recovery mode for a pure preflight/environment failure. Soft-warn would silently re-expose the bug we are trying to prevent.

## File changes

### 1. `cli/src/inhibit.rs` (new file, ~30 lines) — `SleepInhibitor` helper

Tiny module exporting a single type. Sketch:

```rust
//! Hold a logind sleep inhibitor for the lifetime of a value.
//!
//! Used during long-running braid operations (currently `replace`) where
//! suspend mid-flight produces kernel-level topology corruption — see
//! issues #45 and #48 and the upstream warning at
//! reference/btrfs-progs/Documentation/btrfs-replace.rst:49-50.

use std::io::{self, Read};
use std::process::{Child, Command, Stdio};

pub struct SleepInhibitor {
    child: Child,
}

impl SleepInhibitor {
    /// Acquire a `What=sleep, Who=braid, Mode=block` inhibitor lock from logind.
    /// Blocks until the inhibitor is registered with logind.
    pub fn acquire(why: &str) -> io::Result<Self> {
        // Spawn `systemd-inhibit ... sh -c 'printf READY; exec sleep infinity'`.
        // systemd-inhibit acquires the inhibitor lock from logind BEFORE
        // exec'ing its child argv, so reading "READY" from the child's stdout
        // is a race-free handshake that the lock is held.
        let mut child = Command::new("systemd-inhibit")
            .args(["--what=sleep", "--who=braid", "--mode=block"])
            .arg(format!("--why={why}"))
            .args(["sh", "-c", "printf READY; exec sleep infinity"])
            .stdout(Stdio::piped())
            .spawn()?;
        let mut buf = [0u8; 5];
        child
            .stdout
            .as_mut()
            .expect("piped stdout")
            .read_exact(&mut buf)?;
        if &buf != b"READY" {
            // Defensive — should be unreachable.
            return Err(io::Error::other("systemd-inhibit handshake failed"));
        }
        Ok(Self { child })
    }
}

impl Drop for SleepInhibitor {
    fn drop(&mut self) {
        // SIGTERM the systemd-inhibit child; logind releases the inhibitor
        // when the holding process exits. Reap to avoid a zombie.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
```

Notes:

- The handshake (`printf READY` + `read_exact(5)`) is not paranoia: without it, there is a window between `Command::spawn` returning and `systemd-inhibit` actually registering with logind, during which a suspend could slip through.
- `Read::read_exact` returns `UnexpectedEof` if the pipe closes before 5 bytes are read — i.e. if `systemd-inhibit` exited early because logind isn't running. That naturally surfaces as the io error returned by `acquire`.
- No unit tests for this module; the VM regression test in step 4 below is the contract. Mocking `systemd-inhibit` at unit level would be more lines than the helper itself.

### 2. `cli/src/lib.rs` — register the new module

Add `pub mod inhibit;` alongside the other module declarations (e.g. after `pub mod idle;` at line 12, since the file uses rough alphabetical grouping).

### 3. `cli/src/replace.rs` — acquire the inhibitor inside `cmd_replace`

Place the `SleepInhibitor::acquire` call **immediately before** `journal::write_journal` at line 222-223. Acquisition is side-effect-free, so failing it cleanly aborts without leaving a recover-eligible journal behind. The journal write then happens just before the first irreversible disk operation, as the principles already require.

RAII ensures the inhibitor stays held through:

- The journal write (line 222-223) — fast
- LUKS format/open of the new disk (lines 225-257) — fast, but free coverage doesn't hurt
- The `match &replace_source` block with `pool_replace_device` (lines 262-333) — long-running
- `maybe_restore_raid1` soft balance for the missing-path case (lines 339-347) — potentially long-running
- `pool.json` write and journal clear (post line 349) — fast

…and is automatically released on every early `?` return inside the scope.

Sketch (insert immediately before the existing `journal::write_journal` call at line 222):

```rust
// Hold a logind sleep inhibitor for the rest of the replace operation.
// Suspending mid-replace produces kernel-level topology corruption on every
// kernel — see issues #45 and #48. Acquired BEFORE the journal write so a
// logind failure does not strand the user in recovery mode.
let _sleep_inhibitor = crate::inhibit::SleepInhibitor::acquire("replace in progress")
    .map_err(|e| ReplaceError::Validation(format!(
        "could not acquire sleep inhibitor (is logind running?): {e}"
    )))?;

journal::write_journal(params.paths, &journal)
    .map_err(|e| ReplaceError::Validation(e.to_string()))?;

// Step 1: Init new disk (LUKS format/open) — irreversible from here.
match new_probed.state {
    ...
}
```

The exact `ReplaceError` variant the implementer maps to is a judgment call — `Validation` is the closest match in the existing variants but a new variant or `Pool` may fit better; the implementer should pick based on what's already in `ReplaceError`.

### 4. `tests/cli/replace-inhibits-suspend.{nix,py}` — new VM regression test

Create the pair following the established `tests/cli/replace-*.{nix,py}` pattern.

#### `replace-inhibits-suspend.nix`

**Four** `emptyDiskImages` (disk1/2/3 build the pool, disk4 is the replacement target) — model the existing `tests/repro/btrfs-replace-interrupted-mid-flight.nix` exactly:

```nix
virtualisation.emptyDiskImages = [
  { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
  { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
  { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
  { size = 1024; driveConfig.deviceExtraOpts.serial = "disk4"; }
];
```

`environment.systemPackages = [ braid pkgs.cryptsetup pkgs.btrfs-progs ];`. Same `environment.etc."braid/config.json"` as the other tests. (Cloning from `replace-2disk-pool.nix` would be wrong — it only provisions three disks.)

#### `replace-inhibits-suspend.py`

Standard `# Test:` block comment with Intent / Why / Scenario, referencing #45 and #48. Then:

1. **Build a 3-disk pool** via `braid add disk1`, `braid add disk2`, `braid add disk3` (reuse the `add_cmd` helper pattern from `tests/repro/btrfs-replace-interrupted-mid-flight.py:43-48`).
2. **Write a payload** large enough that the replace takes measurable time. Use the proven sizing: `dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=400 status=none && sync` (same as `tests/repro/btrfs-replace-interrupted-mid-flight.py:73`).
3. **Background `braid replace --old disk2 --new disk4=...`** using the same `replace_cmd_bg` helper pattern as `tests/repro/btrfs-replace-interrupted-mid-flight.py:51-60`.
4. **Poll for in-flight state** by checking `btrfs replace status -1 /mnt/storage` for `"Started on"` or `"% done"` (same loop as `btrfs-replace-interrupted-mid-flight.py:99-117`). Hard-fail if never observed (the test has degraded to a no-op).
5. **Assert the inhibitor lock exists** while the replace is in flight. Query logind directly via D-Bus with:
   ```sh
   busctl call org.freedesktop.login1 /org/freedesktop/login1 \
     org.freedesktop.login1.Manager ListInhibitors
   ```
   Use `busctl` rather than `systemd-inhibit --list` because `--list` depends on TTY/terminal context that the NixOS VM test environment does not provide. The output is a `busctl` response containing tuples of `(what, who, why, mode, uid, pid)`. Assert that one of the returned tuples has `who="braid"` and `what` containing `"sleep"` and `mode="block"`. Hard-fail otherwise. (Look at `reference/systemd/` if the parser shape is non-obvious; the implementer can also use `--json=short` if available on the pinned systemd, which produces a JSON-shaped response.)
6. **Wait for the replace to finish** — poll `btrfs replace status -1 /mnt/storage` until output contains `"finished on"`. (Confirmed against the pinned `reference/btrfs-progs/cmds/replace.c:460` — the FINISHED state prints `"Started on %s, finished on %s"`. Lowercase `f`, not `Ended on`.) Bound the wait around `300s`.
7. **Assert the inhibitor lock is released** after replace completion — re-run the `busctl ListInhibitors` query and verify no tuple has `who="braid"`. Allow a brief settle (e.g. `wait_until_succeeds` style) since logind may take a moment to release after the systemd-inhibit child exits.
8. **Assert pool integrity post-replace**: `mountpoint -q /mnt/storage` succeeds, payload sha256 unchanged from the pre-replace value, `btrfs filesystem show /mnt/storage` reports 3 devices with no `MISSING` entries.

Place the test in `tests/cli/` (functional test of normal braid behavior), not `tests/repro/` (which is for kernel/edge-case reproductions).

### 5. `flake.nix` — register the new check

Add a `replace-inhibits-suspend` entry to the `checks` attribute set alongside the other `replace-*` entries (currently at `flake.nix:192-228`). Use the same `pkgs.testers.nixosTest (import ./tests/cli/replace-inhibits-suspend.nix { braid = linuxCrane.braid; })` pattern.

## Critical files

- `cli/src/replace.rs:222` — insertion point for `SleepInhibitor::acquire` (immediately before `journal::write_journal`)
- `cli/src/replace.rs:339-347` — `maybe_restore_raid1` soft-balance tail (the reason for scoping at the intent-command level)
- `cli/src/lib.rs:1-38` — module declarations (`pub mod inhibit;` slots in here)
- `cli/src/cmd.rs:798-809` — read-only confirmation that `RealRunner::exec` is unaffected; the helper rides on its own `Command::new` site
- `cli/src/progress.rs:208-259` — read-only; the unchanged thread+poll loop that drives the blocking replace
- `tests/repro/btrfs-replace-interrupted-mid-flight.nix` — template for the 4-disk `.nix` shape (NOT `replace-2disk-pool.nix`, which only has 3)
- `tests/repro/btrfs-replace-interrupted-mid-flight.py` — template for the background-replace + in-flight-poll pattern (lines 43-48, 51-60, 73, 99-117)
- `flake.nix:192-228` — checks registration
- `modules/braid/wrapper.nix:11` — read-only confirmation that `pkgs.systemd` (and thus `systemd-inhibit` + `busctl`) is on PATH
- `reference/btrfs-progs/cmds/replace.c:450-475` — read-only confirmation of `"finished on"` literal in the FINISHED state

## Verification

```sh
# Rust build + lints + unit tests.
cargo clippy --manifest-path cli/Cargo.toml --tests
just test-rust

# All existing replace-path VM tests — should pass unchanged because the
# helper does not touch any existing code path.
just test-vm replace-2disk-pool replace-live-disk replace-dead-disk \
  replace-larger-disk replace-luks-label replace-new-already-in-pool \
  replace-new-already-luks replace-new-in-pool-guard replace-passphrase-mismatch \
  replace-preserves-devid replace-sequential

# The new regression test — fails before the helper lands, passes after.
just test-vm replace-inhibits-suspend

# The interrupted-mid-flight repro test — must still pass; the helper is
# transparent to machine.crash() (qemu SIGKILL kills the entire process tree
# including the systemd-inhibit child).
just test-repro btrfs-replace-interrupted-mid-flight
```

## Follow-ups to flag at the end of implementation

- **No fixture capture is required.** The golden parser fixtures in `cli/tests/fixtures/nixos-25.11/` capture tool stdout (e.g. `btrfs replace status` output), not subprocess argv or process trees. `systemd-inhibit` is invisible to those parsers. Skip `just capture-all-fixtures`.
- **Other long-running braid operations are still suspend-vulnerable.** Standalone scrub (`braid-scrub.service`) and any future explicit `braid balance` command would benefit from the same `SleepInhibitor` wrapping. Out of scope for #45 — file as a sibling issue under #48 if not already tracked.
- **`docs/principles.md` does not need an update.** The helper is an implementation detail of principle 3 (Safe-by-construction operations), not a new principle. If the implementer wants to document the decision, a short entry under `docs/decisions/` pointing at #45/#48 and `btrfs-replace.rst:49-50` would be the natural place — optional.
- **Commit message** should call out that the inhibitor scope intentionally covers the post-replace `maybe_restore_raid1` soft balance, not just the BtrfsReplaceStart subcommand, so the rationale is greppable later.
