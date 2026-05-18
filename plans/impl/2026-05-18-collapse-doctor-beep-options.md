# Plan: collapse doctor beep-check options onto `DoctorOptions`

## Context

`cli/src/main.rs:361-371` rejects every command except `tui --demo` and
`help` when `geteuid() != 0`, exiting 1 before dispatch. `Commands::Doctor`
falls in the default needs-root arm, so `cmd_doctor` (and therefore
`check_beep_path` / `check_beep_path_inner`) can never run as a non-root
user in production.

Despite that, `BeepCheckOptions` carries an `is_root` field,
`check_beep_path` re-runs `geteuid()` to populate it, and
`check_beep_path_inner` has a skip arm that fires when `is_root == false`.
A dedicated unit test (`beep_path_skips_when_not_root`) and an explanatory
doc-comment block exist solely to cover that unreachable branch. The
NixOS test note at `tests/cli/braid-doctor-beep.py:72-81` already
acknowledges the branch is unreachable from the CLI and even cites a
stale main.rs line range (`244-251`, now `368`) -- a concrete instance
of the doc-drift cost.

Once `is_root` is gone, `BeepCheckOptions` carries the same two booleans
as `DoctorOptions` already does (`json_output` ~ `json`, `play_beep` ~
`beep`). The rename existed to distinguish a 3-axis internal struct
from the 2-axis public one. With only 2 axes left, the rename and the
struct both lose their reason to exist.

Outcome: a doctor that trusts the universal root gate, one fewer
`unsafe` syscall, one fewer adapter type, less test scaffolding to
maintain, and no comment block that has to be kept in sync with
`main.rs` -- but with the root-gate contract pinned for `doctor`
explicitly so a future `Commands::Doctor` exemption cannot silently
reach beep logic as non-root.

## Approach

Delete `BeepCheckOptions` entirely; pass `DoctorOptions` straight to
`check_beep_path_inner`. The inner function stays for `notifier_path`
injection in tests; only the options type changes. Add a doctor-
specific root-gate integration test so the premise the deletion
rests on is pinned.

### `cli/src/doctor.rs`

1. Delete the `BeepCheckOptions` struct (lines 118-126, including its
   `/// Triple-gate inputs` doc comment) and its `#[cfg(test)] impl
   BeepCheckOptions::for_test` block (lines 1084-1093).
2. In `check_beep_path` (line 668-682):
   - Rewrite the rustdoc (lines 653-668 region) so it no longer
     mentions `is_root`, the `geteuid()` adapter, the "real `geteuid()`
     syscall", "geteuid and json branches", or "`json_output = true`"
     framing. Keep only what is still true: that `--json` mode
     suppresses the audible side effect, and that the inner exists
     for `notifier_path` injection in tests.
   - Drop the `let is_root = unsafe { libc::geteuid() } == 0;` line.
   - Replace the `BeepCheckOptions { is_root, json_output, play_beep }`
     construction with a direct pass-through: forward `options:
     DoctorOptions` to `check_beep_path_inner`.
3. In `check_beep_path_inner` (line 796):
   - Change the parameter from `options: BeepCheckOptions` to `options:
     DoctorOptions`.
   - Delete the doc-comment block at lines 828-838 ("Lack of root is an
     INVOCATION CONTEXT issue..." through the `is_root flag is computed
     by the public wrapper" line).
   - Delete the `if !options.is_root { ... }` skip arm (lines 836-838).
   - Rewrite `options.json_output` -> `options.json` and
     `options.play_beep` -> `options.beep` in the remaining gates
     (lines 844, 853).
4. Delete the `beep_path_skips_when_not_root` test (lines 2422-2443).
5. Update the seven remaining `beep_check_options(is_root, json_output,
   play_beep)` call sites in the test module (lines 2349, 2377, 2400,
   2470, 2497, 2529, 2564) to construct `DoctorOptions { json, beep }`
   directly. Drop the first argument; map `json_output` -> `json` and
   `play_beep` -> `beep`. Update the inline `// is_root:` /
   `// json_output:` / `// play_beep:` comments at lines 2504-2506 and
   2625-2627 so the surviving fields are documented by their new
   names, and rewrite the test-intent comment at line 2601 that reads
   "when is_root=true, play_beep=true" to just "when --beep is set
   and a real-looking probe path is configured".
6. In the `valid_config_parses_ok_disks_warn` comment (lines 1126-1128),
   rewrite the geteuid mention: "beep_path is intentionally not asserted
   here: it depends on real host state (/etc/braid/notifier-config.json).
   Deterministic coverage lives in the check_beep_path_inner tests."
7. In the "Test-only constructors" section header (lines 1028-1037),
   drop `BeepCheckOptions` from the prose; the surviving constructors
   are `DoctorContext::for_test_*` only.
8. In the doctor-tests `use` import (line 1105), remove
   `beep_check_options` from the `crate::test_fixtures` use list.
9. In the test-block section header (lines 2482-2483), rewrite to drop
   the `is_root flag` and `geteuid()` references; the new framing is
   that beep_path tests target the inner helper directly to inject the
   notifier-config path.

### `cli/src/test_fixtures/doctor.rs`

10. Update the module-doc comment (lines 8-10) to remove the
    `BeepCheckOptions` reference; the surviving statement is that
    `DoctorContext` is field-constructed via the `#[cfg(test)]
    pub(crate)` constructor on that type in `doctor.rs`.
11. In the `use crate::doctor::{...}` import (line 15), drop
    `BeepCheckOptions`. The surviving imports are `DiskState`,
    `DoctorContext`, `DoctorOptions`.
12. Delete the `beep_check_options` helper (lines 50-56). Tests will
    construct `DoctorOptions { json, beep }` inline; named fields keep
    call-site readability without the helper.

### `cli/src/test_fixtures.rs`

13. Remove `beep_check_options` from the `pub(crate) use doctor::{ ... }`
    re-export at line 152.

### `tests/cli/braid-doctor-beep.py`

14. Delete the entire `NOTE on the missing non-root subtest` block (lines
    72-81). The branch it discusses no longer exists.

### `cli/tests/root_check.rs`

15. Add a new doctor-specific root-gate test that pins the contract the
    code now trusts:

    ```rust
    // Intent: `braid doctor --beep` is rejected as non-root by the
    //   universal root gate in main.rs, never reaching cmd_doctor or
    //   the beep-check logic.
    // Why: check_beep_path / check_beep_path_inner no longer carry a
    //   defense-in-depth `is_root` skip arm; that branch was deleted
    //   because main.rs already gates every command except `tui --demo`
    //   and `help`. A future `Commands::Doctor` exemption that bypassed
    //   the gate would silently reach beep logic as non-root. This test
    //   makes such a regression fail loudly.
    // Scenario: an unprivileged user runs `braid doctor --beep` to
    //   preview the alert beep without sudo.
    #[test]
    fn non_root_doctor_exits_with_error() {
        if is_root() {
            return;
        }
        let output = braid()
            .args(["doctor", "--beep"])
            .output()
            .expect("failed to execute braid");
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("must be run as root"), "got: {stderr}");
    }
    ```

    This sits next to `non_root_exits_with_error` (lines 12-24) and
    reuses the existing `is_root()` and `braid()` helpers. No new
    imports needed.

### Final sweep

16. Before declaring done, run:

    ```
    rg -n 'BeepCheckOptions|beep_check_options|json_output|play_beep|\bis_root\b' cli/src/ cli/tests/ tests/cli/braid-doctor-beep.py
    rg -n 'geteuid' cli/src/doctor.rs
    ```

    Expected post-fix surface:
    - `BeepCheckOptions`, `beep_check_options`, `json_output`,
      `play_beep`: no matches in `cli/src/doctor.rs`,
      `cli/src/test_fixtures/`, `cli/src/test_fixtures.rs`, or
      `tests/cli/braid-doctor-beep.py`. (Unrelated `json_output_*`
      test function names in `cli/src/ups.rs` are out of scope and
      stay.)
    - `is_root`: only the `is_root()` helper and call sites in
      `cli/tests/root_check.rs`.
    - `geteuid`: no matches in `cli/src/doctor.rs`. (Survives in
      `cli/src/main.rs:367-368` -- the universal gate -- and in
      `cli/tests/root_check.rs:4-5` -- the gate test helper. Both are
      intentional.)

### Open question deliberately resolved

Whether to keep `BeepCheckOptions` minus the `is_root` field, or
collapse onto `DoctorOptions` directly: collapse. Two positional bools
would hurt readability, but `DoctorOptions`'s named fields keep
call-site clarity (`DoctorOptions { json: false, beep: true }`).

## Critical files

- `cli/src/doctor.rs` -- struct definition, both `check_beep_path*`
  functions, `for_test` constructor, all seven surviving unit-test
  call sites, the `beep_path_skips_when_not_root` test, several doc
  comments and inline comments referencing `is_root` / `geteuid()` /
  `json_output` / `play_beep`, plus the test-mod section headers and
  test-mod `use` import.
- `cli/src/test_fixtures/doctor.rs` -- the `beep_check_options`
  helper, the module-doc-comment reference to `BeepCheckOptions`, and
  the `use crate::doctor::{BeepCheckOptions, ...}` import.
- `cli/src/test_fixtures.rs` -- the helper's re-export.
- `cli/tests/root_check.rs` -- new `non_root_doctor_exits_with_error`
  test pinning the doctor side of the universal root gate.
- `tests/cli/braid-doctor-beep.py` -- the now-obsolete note.

`cli/src/main.rs` is unchanged: the universal root gate keeps doing
its job. The existing `non_root_exits_with_error` integration test
already pins the `status` side of that gate; the new test pins the
`doctor --beep` side.

## Reuse

- `DoctorOptions` (`cli/src/doctor.rs:113-116`) already has the two
  booleans we need (`json`, `beep`) and is already passed into
  `check_beep_path` from `run_doctor`. Forward it directly; do not
  introduce a parallel struct.
- The remaining beep_path tests already use `MockRunner::default()` /
  `MockRunner::with_output` and `beep_ctx`; those stay. Only the
  options builder changes.
- `cli/tests/root_check.rs::is_root()` and `braid()` helpers (lines
  3-10) are reused by the new test verbatim; no new helpers needed.

## Verification

1. `cargo test --test root_check non_root_doctor_exits_with_error`
   -- pins the new doctor-side root gate. `just test-rust` does not
   run the `root_check` integration target (`justfile:104` only
   selects `--lib --bin braid --test golden_nixos_25_11 --test
   tty_guard`), so this command is explicit and required.
2. `cargo test --test root_check` -- full root_check integration
   suite continues to pass.
3. `just test-rust` -- doctor unit tests still pass with seven
   beep_path tests (the not-root case is removed).
4. `cargo build -p braid-cli` -- no `unsafe` regression; `libc` stays
   in `Cargo.toml` (still used by `cmd.rs`, `hdparm.rs`, `luks.rs`,
   `inhibit.rs`, plus `main.rs` and `root_check.rs`).
5. Run the sweep commands from step 16 of Approach; confirm the
   expected post-fix surface.
6. `just test-vm braid-doctor-beep` -- existing NixOS scenarios (beep
   disabled, JSON mode, --beep success, --beep broken speaker) still
   pass. The deleted python note has no live assertion behind it, so
   removing it cannot fail the VM test.
7. Manual: `sudo braid doctor --beep` on a NixOS host with monitor
   enabled still emits the beep; `sudo braid doctor --beep --json` still
   skips with the JSON-mode message; `sudo braid doctor` still skips
   with the opt-in message. Non-root `braid doctor` still exits 1 with
   "braid must be run as root" -- enforcement moves entirely to the
   main.rs gate, with the new integration test as its doctor-side pin.
