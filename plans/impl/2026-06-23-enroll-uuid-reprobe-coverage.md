# Plan: end-to-end coverage + hardening for enroll's execute-time UUID re-probe

## Context

`braid enroll` carries a decision-024 mutation-boundary guard unique to it: after
the passphrase prompt and before `luksAddKey`, `EnrollPlan::execute` re-probes
every candidate's live LUKS UUID (`cli/src/enroll_key_file.rs#reprobe_member_luks_uuid`,
called in the re-probe loop in `cli/src/enroll_key_file.rs#EnrollPlan::execute`).
It closes the discovery -> execute window: a disk swapped or reformatted to a
foreign LUKS container during the operator-controlled passphrase prompt would
otherwise take the auto-unlock keyfile into slot 1 of the wrong container while
the real member's slot 1 stays empty -- silently breaking auto-unlock.

Two gaps motivate this change:

1. **No end-to-end coverage, and enroll is the lone outlier.** The guard's two
   closest analogs both prove their execute-boundary re-probe end-to-end:
   `tests/cli/replace-live-pool-collision-race-rejected.py` and
   `tests/cli/braid-add-uuid-swap-rejected.py`. enroll is the only command with a
   distinct mutation-boundary re-probe and no VM test of it. `unlock` is not a
   counterexample -- its UUID check is a single plan-time pass with no separate
   post-passphrase re-probe, so `tests/cli/unlock-uuid-mismatch.py` covering only
   the pre-command reformat is correct for unlock but not a model for enroll.
   Per [docs/dev/testing.md#live-tool-behavior-locks](docs/dev/testing.md#live-tool-behavior-locks),
   mocked unit tests prove the classifier given assumed `cryptsetup` behavior and
   given the loop is wired into `execute`; they do not prove the loop is reached
   in the real binary after a real stdin passphrase read, nor that
   `cryptsetup luksUUID` still exits 0 with a readable UUID for a real swapped
   header at that boundary.

2. **The unit tests carry a false rationale and a fragile mock.** Five comment
   blocks in `cli/src/enroll_key_file.rs` claim a mid-prompt swap is "not
   deterministically reproducible in a NixOS VM." That is refuted by the existing
   replace test, and
   [docs/dev/testing.md#vm-and-command-test-design](docs/dev/testing.md#vm-and-command-test-design)
   directs the next contributor to trust exactly such "infeasible" notes.
   Separately, the three execute-boundary tests place the swap at the right phase
   with `with_output_sequence(CryptsetupLuksUuid{d1}, vec![match, swapped])`,
   relying on discovery consuming the first element and the re-probe the second.
   That couples the test to discovery's probe count: if discovery's count for d1
   ever changes, the swapped element is consumed at the wrong phase and the
   failure surfaces as a `plan_enroll(...).expect(...)` panic or a `MissingMock`
   -- a confusing diagnostic that a bolt-on count assertion (which runs only after
   `execute`) could not improve, because the `expect` panic preempts it. The fix
   is to remove the count-coupling entirely, not to assert over it.

Intended outcome: bring enroll to parity with replace/add (a deterministic
end-to-end test of the execute-boundary guard), replace the false comments with a
pointer to that test, and re-base the three unit tests on a phase gate that is
immune to discovery's probe count -- with **no production change** (the guard is
correct; only its coverage, the comments, and the test mechanism are wrong).

## Approach

Three coordinated edits. The VM test is the load-bearing piece; the other two are
cheap and high-certainty.

### 1. New VM test: `tests/cli/enroll-uuid-mismatch-midprompt.py` (+ `.nix`)

Drives the real `braid enroll --generate --passphrase-stdin` and reproduces a
swap that **passes discovery but is caught at the execute re-probe**, deterministically.

**The sync problem and its solution.** Unlike replace/add, enroll's
`--passphrase-stdin` read is silent (no prompt --
`cli/src/luks.rs#read_passphrase_with_readers` shows the "LUKS passphrase:" prompt
is TTY-only), and for an all-present pool nothing is printed before it blocks
(`EnrollPlan::execute` emits only skip notes, which are empty). So the replace/add
trick (poll stdout for a confirmation prompt) does not transfer. Instead, exploit
that `cli/src/cmd.rs#apply_child_env` hands braid's **inherited PATH** to every
child: a `cryptsetup` shim placed first on the PATH that braid is launched with
intercepts braid's calls and logs each one **after** the real binary returns.

**Wrapper caveat (load-bearing).** The shipped `braid` is a makeWrapper script
(`flake.nix#braid`) that does `--prefix PATH : ${toolPath}`, prepending the Nix
`cryptsetup`/`btrfs`/... paths **ahead of** the caller's PATH before exec'ing the
real binary. So `PATH=/tmp/shim:$PATH braid enroll ...` would leave braid
inheriting `<nix-tools>:/tmp/shim:...` and resolving the Nix `cryptsetup` first --
the shim would never fire and the poll would time out. The fix (proven by
`tests/cli/replace-preview-warnings.py#replace_unwrapped_cmd`, which hits the same
wrapper) is to run the command-under-test against the **unwrapped** binary: resolve
it from the wrapper source (`readlink -f $(command -v braid)`, `cat` it, then
`re.search(r'(/nix/store/[^"\s]+/bin/braid)(?!\-)', ...)`), and launch only the
enroll-under-test as `PATH=/tmp/shim:$PATH <unwrapped_braid> enroll ...`. The
unwrapped ELF inherits the caller's PATH directly, so `/tmp/shim/cryptsetup` wins
lookup and `apply_child_env` then propagates it to braid's children. Setup commands
(`braid add`, `braid unlock`, lock) stay on the normal wrapped `braid` -- they need
the full Nix tool path and must not be shimmed. (The unwrapped binary still resolves
its **non-cryptsetup** tools via the ambient VM PATH. For enroll the one that matters
is `mountpoint` (util-linux): `enroll --generate` runs a **plan-time** mountpoint
check before any cryptsetup discovery --
`cli/src/enroll_key_file.rs#validate_generated_keyfile_target` issues a
`MountpointCheck` that `cli/src/cmd.rs#to_argv` maps to a bare `mountpoint -q`. enroll
shells out to no btrfs, so the replace test's btrfs resolution does not vouch for
`mountpoint`; instead it is grounded directly -- util-linux is a NixOS *required*
package (always on `/run/current-system/sw/bin`), and the sibling
`tests/cli/enroll-uuid-mismatch.py` already proves it by calling bare
`mountpoint -q /tmp/usb` via `machine.succeed`. If `mountpoint` failed to resolve,
braid would die at plan time with a spawn error instead of the expected
`LUKS UUID mismatch`, and the reused `assert_mismatch_output` would fail confusingly.)

Discovery probes each member with `luksUUID`, then `luksDump`, then
`cryptsetup status braid-<name>` (`cli/src/probe.rs#probe_config_disk` ->
`cli/src/luks.rs#classify_mapper_ownership`; a closed mapper returns `Inactive`
after that single status call -- no further cryptsetup calls). disk2 sorts last by
name, so `status braid-disk2` is discovery's **final** cryptsetup call for disk2,
emitted only after `luksUUID`/`luksDump` on disk2 have already completed. **Gate
the reformat on the `status braid-disk2` log entry** (not the earlier `luksUUID`
line): that guarantees discovery has fully probed disk2 and cannot race any
discovery read of it. Additionally assert the earlier `luksUUID <disk2 by-id>`
line was logged, pinning that discovery read disk2's real (matching) UUID before
the gate fired. `status braid-disk2` appears exactly once (the re-probe issues
only `luksUUID`, never `status`), so the gate is unambiguous.

**Why this cannot accidentally test discovery instead of execute.** The
`status braid-disk2` gate fires only after discovery finished reading disk2
(matching UUID captured), so discovery never observes the foreign UUID and always
classifies disk2 as a matching candidate; braid always proceeds to block on the
passphrase. The swap is therefore always caught by the post-passphrase re-probe.
What forces detection to the execute boundary is the **FIFO**: the coprocess holds
the only write end, so braid blocks in `read_passphrase` and cannot advance to the
re-probe until the coprocess releases it -- which it does only after the reformat.
The two `kill -0 $BRAID_PID` checks (after the gate, and again after the reformat
but before the passphrase is released) are liveness / premature-exit guards -- they
prove braid had not errored or exited early, so the reformat provably landed inside
the still-open discovery->execute window.

**Test body (mirroring `tests/cli/replace-live-pool-collision-race-rejected.py`
and `tests/cli/enroll-uuid-mismatch.py`):**

- Three-line preamble (Intent / Why it exists / Scenario) per `docs/dev/testing.md`.
- Setup (normal **wrapped** `braid`): `braid add disk1`, `braid add disk2`
  (low-iteration pbkdf2 args as in the sibling tests); `braid unlock` to enrich
  `pool.json` with UUID keys; capture disk2's old UUID; lock the pool (close all
  mappers); `mount -t tmpfs ... /tmp/usb`.
- Resolve the **unwrapped** braid binary (per the wrapper caveat above), via the
  `tests/cli/replace-preview-warnings.py#replace_unwrapped_cmd` pattern:
  `readlink -f $(command -v braid)` -> `cat` the wrapper -> `re.search` for the
  `/nix/store/.../bin/braid` path. Only the enroll-under-test runs against it.
- Build the shim. Capture the absolute real binary **before** the shim is on PATH
  (`real_cs=$(command -v cryptsetup)`) -- an absolute path, captured pre-shadow so the
  shim delegates to the real binary, not to itself. braid execs `cryptsetup` directly
  via `std::process::Command` (`cli/src/cmd.rs#RealRunner`, **no shell**), and
  `cli/src/cmd.rs#apply_child_env` `env_clear()`s the child env (only PATH + LC_ALL
  survive). So the shim **must** (a) carry a `#!/bin/sh` shebang -- a shebang-less
  script fails ENOEXEC at `execvp` before it can log anything; and (b) embed the
  captured path **literally** at write time, never reference a runtime `$REAL` (the
  cleared env would leave it unset). Write `/tmp/shim/cryptsetup` by substituting
  `real_cs` into the body (exactly as `tests/cli/replace-preview-warnings.py`
  substitutes `__REAL_CRYPTSETUP__`):

  ```sh
  #!/bin/sh
  <real_cs> "$@"; rc=$?; printf '%s\n' "$*" >> /tmp/cs.log; exit "$rc"
  ```

  then `chmod +x`. The body is POSIX, so `/bin/sh` suffices. **No `set -e`**
  (deliberately unlike the replace test's `set -eu` template): the gated
  `cryptsetup status braid-disk2` exits **4** for the inactive/closed mapper (pinned
  by braid's own `cli/src/cmd.rs#MockRunner` `with_mapper_closed` fixture, and matching
  `action_status`'s `CRYPT_INACTIVE` arm in vendored cryptsetup), so an errexit shim
  would abort at the delegate and never reach the `printf` -- the gate line would never
  be logged and the poll would time out. `rc=$?` is captured immediately after the
  delegate (before `printf`, which clobbers `$?`) and re-raised via `exit "$rc"` so
  braid sees cryptsetup's real exit code.
- Coprocess (a `/tmp/*.sh` script invoked once, like the replace test):
  - `mkfifo` the passphrase FIFO; truncate `/tmp/cs.log` and the out/exit files.
  - Background, in a `set +e` subshell capturing `$?` to an exit file (the errexit
    idiom in `docs/dev/testing.md`, "NixOS test driver wraps every command with
    `set -euo pipefail`"):
    `PATH=/tmp/shim:$PATH <unwrapped_braid> enroll /tmp/usb --generate --passphrase-stdin < FIFO > OUT 2>&1`
    (the **unwrapped** binary, per the wrapper caveat). Record `BRAID_PID`.
  - `exec 3>FIFO` to open the write end.
  - Poll `/tmp/cs.log` for a `status braid-disk2` line (discovery's final disk2
    call), with a `kill -0 "$BRAID_PID"` premature-exit guard and a bounded loop
    (300 x 0.1s, copied from the replace test).
  - Assert `/tmp/cs.log` already contains a `luksUUID` line naming disk2's by-id
    (discovery read disk2's real UUID before the gate).
  - `kill -0 "$BRAID_PID"` (discovery done; braid is blocked on the passphrase).
  - Reformat disk2 to a fresh foreign LUKS2 container, same passphrase
    (`cryptsetup luksFormat --batch-mode --key-file=- ...`, as in the disk2 reformat
    in `tests/cli/enroll-uuid-mismatch.py`).
  - `kill -0 "$BRAID_PID"` again (reformat not yet seen; still blocked on passphrase).
  - `printf '%s\n' "$PASS" >&3; exec 3>&-` to release the passphrase; `wait` braid.
- Assertions (reuse `tests/cli/enroll-uuid-mismatch.py`'s `assert_mismatch_output`
  and `assert_slot1_empty` shapes): braid exit != 0; OUT contains `LUKS UUID mismatch`,
  `disk2`, the old and new UUIDs, `detach the foreign disk`, `braid replace`;
  `/tmp/usb/braid.key` does **not** exist; slot 1 empty on **both** disk1 and
  disk2. Also assert `/tmp/cs.log` shows two `luksUUID <disk2>` entries (discovery
  + re-probe), pinning that the re-probe actually ran.

**`.nix` module** -- copy `tests/cli/enroll-uuid-mismatch.nix` verbatim, changing
only `name` and the `testScript` path. Two disks (disk1, disk2) suffice; no third
disk needed. The `cryptsetup`/`btrfs-progs` packages and `config.json` are already
in that template. No util-linux entry is needed -- `mountpoint` resolves via the
ambient system path (per the wrapper caveat above); adding `pkgs.util-linux` to the
template's `systemPackages` is optional, self-documenting insurance.

**Registration** -- `flake.nix` registration is explicit (no directory scan). Add,
immediately after the `enroll-uuid-mismatch` check entry in `flake.nix`:

```nix
enroll-uuid-mismatch-midprompt = pkgs.testers.nixosTest (
  import ./tests/cli/enroll-uuid-mismatch-midprompt.nix {
    braid = linuxCrane.braid;
  }
);
```

### 2. Re-base the three execute-boundary unit tests on a phase gate

The three tests -- `execute_rejects_swapped_disk_before_mutation`,
`execute_rejects_swapped_disk_existing_keyfile_before_mutation`, and
`execute_rejects_swapped_already_enrolled_disk_before_mutation` in
`cli/src/enroll_key_file.rs` -- currently use
`with_output_sequence(CryptsetupLuksUuid{d1}, vec![match, swapped])`. Replace that
sequence mock with a **phase-gated handler** that decouples *which* UUID is
returned from *how many times* discovery calls. Use the existing
`MockRunner::with_handler` API (`cli/src/cmd.rs#MockRunner`) plus a file-local
phase cell, keeping the change test-local per
[docs/dev/testing.md#vm-and-command-test-design](docs/dev/testing.md#vm-and-command-test-design)
("prefer a file-local runner or wrapper over widening the shared `MockRunner`").
`with_output_sequence` stays in the wider test suite; only these three call sites
change.

```rust
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

let phase = Arc::new(AtomicU8::new(0)); // 0 = discovery, 1 = execute
let (_, d1_match) = enroll_luks_uuid_ok(d1, test_uuid(500).as_str());
let (_, d1_foreign) = enroll_luks_uuid_ok(d1, foreign);
let runner = MockRunner::default()
    .with_handler({
        let phase = phase.clone();
        move |req| match req {
            CmdRequest::CryptsetupLuksUuid { device } if device == d1 => Some(Ok(
                if phase.load(Ordering::SeqCst) == 0 {
                    d1_match.clone()
                } else {
                    d1_foreign.clone()
                },
            )),
            _ => None,
        }
    })
    // ... existing d2 / luksDump / mappers-closed / mountpoint / keyfile mocks
    //     unchanged; the handler only intercepts luksUUID for d1 ...
    ;

let plan =
    plan_enroll(&runner, &fs, &params).expect("discovery must succeed with matching UUID");
let discovery_d1 = count_d1_luksuuid(&runner); // filter requests() for CryptsetupLuksUuid{device == d1}
phase.store(1, Ordering::SeqCst);
let err = plan
    .execute(&runner, &params)
    .expect_err("execute re-probe must reject the swapped disk")
    .to_string();
let total_d1 = count_d1_luksuuid(&runner);

assert!(discovery_d1 >= 1, "discovery must probe disk1's UUID");
assert_eq!(
    total_d1 - discovery_d1, 1,
    "the execute re-probe must issue exactly one luksUUID for disk1"
);
```

The remaining assertions (mismatch wording names disk1, no `CryptsetupLuksAddKeyFile`,
keyfile state, and -- for the already-enrolled test -- no `CryptsetupTestKeyFile`
before the re-probe) stay as-is. This makes the tests immune to a future discovery
probe-count change: discovery always reads the match (any number of times), execute
always reads the foreign value, and the per-phase delta pins the re-probe to exactly
one execute-time probe with a named message instead of a `MissingMock`/`expect` panic.
The delta is pinned to exactly `1` (not `>= 1`) deliberately: decision-024 specifies
one re-probe per candidate at the mutation boundary, so `== 1` documents that contract
and yields a clear diagnostic. It is not the sole safety net -- a reverted guard is
caught independently by `expect_err` (execute returns `Ok` with no re-probe loop) --
so the exact count is a diagnostic refinement, not load-bearing.

Each test's in-body comment that today explains the sequence trick ("Mappers closed
=> discovery issues exactly one luksUUID per disk, so the 2nd sequence element ... a
mapper-open disk would pop both at discovery and silently invert the test") must be
rewritten to explain the phase gate -- the foot-gun those comments document no longer
exists once the sequence mock is gone.

### 3. Correct the five overstated comments

Replace the "not deterministically reproducible in a NixOS VM" / "No VM test"
rationale with an accurate pointer to the new VM test, in these test functions in
`cli/src/enroll_key_file.rs`:

- `cli/src/enroll_key_file.rs#reprobe_member_luks_uuid_mismatch_rejects`
- `cli/src/enroll_key_file.rs#reprobe_member_luks_uuid_probe_failure_fails_closed`
- `cli/src/enroll_key_file.rs#execute_rejects_swapped_disk_before_mutation`
- `cli/src/enroll_key_file.rs#execute_rejects_swapped_disk_existing_keyfile_before_mutation`
- `cli/src/enroll_key_file.rs#execute_rejects_swapped_already_enrolled_disk_before_mutation`

(The last three overlap the tests rewritten in step 2, so their comment correction
and phase-gate rewrite land together; the two `reprobe_member_luks_uuid_*` tests get
only the comment correction.)

New wording (per comment): the mismatch/fail-closed arms are unit-pinned here;
`tests/cli/enroll-uuid-mismatch-midprompt.py` covers the post-passphrase swap
end-to-end (deterministic via a `cryptsetup` PATH-shim that gates the reformat on
discovery completion), and `tests/cli/enroll-uuid-mismatch.py` covers the
pre-command discovery case.

### Out of scope (deliberately)

- **No production change.** The guard is correct; a status line in `execute`
  printed only to give the test something to poll would be a production change made
  solely for testability -- counter to the repo's testing discipline (e.g.
  `docs/dev/testing.md`'s rule against zeroing production timing constants under
  `#[cfg(test)]`) and ADR 034's subprocess discipline.
- **decision-024 doc** (`docs/design/decisions/024-luks-uuid-identity.md`) is
  already accurate -- it attributes the window closure to unit tests and lists the
  VM tests for the general re-check. Optionally, add
  `tests/cli/enroll-uuid-mismatch-midprompt.py` to
  [its "Tests That Enforce This" list](docs/design/decisions/024-luks-uuid-identity.md#tests-that-enforce-this)
  alongside the other UUID re-check VM tests; low priority.

## Critical files

- NEW `tests/cli/enroll-uuid-mismatch-midprompt.py` -- the VM test.
- NEW `tests/cli/enroll-uuid-mismatch-midprompt.nix` -- copy of
  `tests/cli/enroll-uuid-mismatch.nix` with `name` + script path changed.
- `tests/cli/replace-preview-warnings.py` -- **read-only pattern source**: the
  unwrapped-braid resolution (`#replace_unwrapped_cmd` and the wrapper-source
  `readlink`/`cat`/`re.search` block above it) the new VM test copies to bypass the
  `flake.nix#braid` `--prefix PATH` wrapper so the shim wins lookup.
- `flake.nix` -- new check entry after the `enroll-uuid-mismatch` entry.
- `cli/src/enroll_key_file.rs` -- rewrite three tests onto the phase-gated handler
  (with updated in-body comments + per-phase count assertions); correct five
  "No VM test" comment blocks.

## Verification

- **TDD confirmation (do first).** With the new VM test in place, temporarily
  delete the re-probe loop in `cli/src/enroll_key_file.rs#EnrollPlan::execute` and
  run the test; it must fail for the right reason -- braid proceeds past the
  passphrase, enrolls the keyfile into the reformatted disk2, exits 0, and creates
  `/tmp/usb/braid.key`. Restore the loop; the test must pass. (`AGENTS.md`: write
  failing tests first, confirm they fail for the right reason.)
- **VM test:** `just test-vm enroll-uuid-mismatch-midprompt` (test-vm names carry
  no `repro-` prefix, per `docs/dev/testing.md`). Re-run a few times to rule out a
  lucky pass, per
  [docs/dev/testing.md#vm-and-command-test-design](docs/dev/testing.md#vm-and-command-test-design)
  -- though the `status braid-disk2` gate + `kill -0` assertions make it
  deterministic by construction.
- **Unit tests:** `just test-rust` -- the three rewritten tests still pass, the
  phase gate returns match then foreign, and the per-phase delta assertion holds.
- **Lint gates:** the `.py` test must avoid `f"..."` without placeholders (per
  [docs/dev/testing.md#python-f-strings-without-placeholders-fail-the-build-time-linter](docs/dev/testing.md#python-f-strings-without-placeholders-fail-the-build-time-linter));
  comment edits are Rust comments (exempt from `check-output-ascii.py`).
  `nix flake check` builds the newly registered test.

## Implementation notes

- The negative `just test-vm enroll-uuid-mismatch-midprompt` probe with the
  execute-time re-probe temporarily disabled could not reach the VM stage:
  `linuxCrane.braid` runs Rust tests first, and the strengthened phase-gated
  enroll unit tests failed on the missing re-probe before the VM derivation
  could run. The guard was restored before all passing validation.
