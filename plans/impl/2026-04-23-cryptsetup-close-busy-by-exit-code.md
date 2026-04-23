# Plan: classify `cryptsetup close` busy by exit status, not stderr

## Context

`close_mapper_with_retry` in `cli/src/lock.rs:30-64` classifies a cryptsetup
close failure as retry-eligible via a loose stderr substring match:

```rust
let is_busy = stderr.contains("busy") || stderr.contains("in use");
```

The review finding is that this English-wording match is wiring-risk: any
future release (or dm-level message bubbling up) that phrases busy without
the bigrams "busy" or "in use" -- e.g. "Target is still active and cannot
be removed" -- would make every busy close hard-fail without retry, even
though the retry loop is the whole reason the helper exists (kernel-async
release races that resolve in <=1.5 s are common and normal).

### Why exit code is the right signal for `cryptsetup close`

The vendored source shows that on the `cryptsetup close` path, exit status
5 is EBUSY-exclusive. No `-EEXIST` branch exists in the close flow:

- `reference/cryptsetup/src/cryptsetup.c:829-854` (`action_close`) calls
  `crypt_deactivate_by_name()` and returns its libcryptsetup result.
- `reference/cryptsetup/lib/setup.c:5763-5811` is the deactivate switch.
  Its return codes are:
  - `-EBUSY` -- `CRYPT_ACTIVE`/`CRYPT_BUSY` with holders (line 5770), and
    the post-deactivation recheck (line 5801).
  - `-EINVAL` -- OPAL+deferred conflict (line 5780), default/unknown status
    (line 5810).
  - `-ENODEV` -- `CRYPT_INACTIVE` (line 5806).
- `reference/cryptsetup/src/utils_tools.c:218-235` (`translate_errno`) maps
  `-EBUSY` (and `-EEXIST`, but `close` never produces EEXIST) to exit 5;
  `-ENODEV` to exit 4; `-EINVAL` to exit 1.

So for `cryptsetup close` specifically, exit 5 means "device held open".
Exit code is a stable, locale-independent, wording-independent signal that
matches the actual semantics of the retry loop -- retry when EBUSY, fail
fast for everything else.

### Side effect: existing tests need realistic exit codes

Six existing tests model a non-busy fatal error as `exit 5 +
"Device is not active."`. That pairing is impossible in real cryptsetup
(ENODEV translates to exit 4, not 5). The tests happen to pass today
because the current classifier looks only at stderr. Once the classifier
pivots to exit code, those fixtures become ambiguous and must be corrected
to exit 4 (the real ENODEV exit) so they keep exercising the non-busy
branch. Fixture occurrences: `cli/src/lock.rs:1027, 1060, 1158, 1202, 1234,
1273` (verify with `grep -n '"Device is not active' cli/src/lock.rs`).

## Files

- `cli/src/lock.rs` -- classifier change; fixture corrections; one
  regression test.
- `tests/repro/cryptsetup-close-mounted.py` -- extend existing repro to
  behavior-lock the exit-code contract that the classifier now depends on.

## Change 1: classifier

Replace `cli/src/lock.rs:40-41`:

```rust
// cryptsetup close (lib/setup.c:5763-5811) returns -EBUSY for a held
// mapper, translated to exit 5 by src/utils_tools.c translate_errno.
// On the close path exit 5 is EBUSY-exclusive (EEXIST has no close-path
// branch), so matching exit status is wording- and locale-agnostic and
// survives upstream phrasing drift.
let is_busy = result.exit_status == 5;
```

Drop the `result.stderr.to_lowercase()` line.

## Change 2: correct six unrealistic test fixtures

For each of the six `err_raw("cryptsetup close ...", 5, "Device is not active.")`
fixtures in `cli/src/lock.rs`, change the exit code from `5` to `4`. Lines
to change (contents-sensitive; verify by re-greping before editing):

- `cli/src/lock.rs:1027` -- `lock_umount_fails_unexpected_mapper_error_is_fatal`
- `cli/src/lock.rs:1060` -- `lock_mapper_close_fatal_when_umount_succeeded`
- `cli/src/lock.rs:1158` -- `lock_umount_fails_orphan_unexpected_error_is_fatal`
- `cli/src/lock.rs:1202` -- `lock_orphan_close_failure_is_fatal`
- `cli/src/lock.rs:1234` -- `lock_continues_closing_after_mapper_error`
- `cli/src/lock.rs:1273` -- `lock_collects_first_mapper_error`

Each of these tests pins that a non-busy cryptsetup close error is fatal
(not retried, not suppressed by the umount-busy warning branch). Exit 4
keeps their intent intact and aligns them with real ENODEV behavior.

The existing `exit 5 + "Device ... is still in use."` fixtures (e.g.
`cli/src/lock.rs:688, 697, 986, 996, 1112`) remain unchanged -- those are
real EBUSY and keep working under the new classifier.

## Change 3: regression test

Add one test to the `tests` module in `cli/src/lock.rs`. It pins both the
*mechanism* (exit 5 -> retry -> `DeviceBusy`) and the *wording independence*
(uses a wording the old classifier would have missed).

```rust
// Intent: cryptsetup close with exit status 5 goes through the retry
//   loop and surfaces as LockError::DeviceBusy, regardless of the
//   specific English phrase in stderr.
// Why it exists: the classifier at lock.rs:40-41 is what distinguishes
//   "kernel-async release race, retry wins" from "every close hard-
//   fails on first attempt". Before this test, the classifier matched
//   stderr substrings -- a future cryptsetup phrasing change would
//   silently disable retry. This test uses a wording the old
//   substring classifier would have missed ("still active and cannot
//   be removed", not "in use" / "busy") so a regression to stderr-
//   based matching fails here.
// Scenario: umount succeeds; braid-aaa close returns exit 5 every
//   attempt with non-canonical busy wording; braid-bbb closes cleanly.
//   Lock must retry braid-aaa CLOSE_RETRY_ATTEMPTS times, then return
//   LockError::DeviceBusy.
#[test]
fn lock_mapper_close_exit5_is_busy_regardless_of_wording() {
    let inner = mounted_runner()
        .with_output(
            CmdRequest::CryptsetupClose { mapper: "braid-aaa".into() },
            err_raw(
                "cryptsetup close braid-aaa",
                5,
                "Target braid-aaa is still active and cannot be removed.",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupClose { mapper: "braid-bbb".into() },
            ok_raw("cryptsetup close braid-bbb"),
        );
    let runner = RecordingRunner::new(inner);
    let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
    let config = test_config();
    let membership = test_membership();

    let err = cmd_lock(&runner, &fs, &config, &membership, false)
        .expect_err("busy close should bubble up after retries exhaust");
    assert!(
        matches!(err, LockError::DeviceBusy(_)),
        "expected LockError::DeviceBusy, got: {err:?}"
    );
    let aaa_attempts = runner
        .close_calls()
        .iter()
        .filter(|m| m.as_str() == "braid-aaa")
        .count();
    assert_eq!(
        aaa_attempts, CLOSE_RETRY_ATTEMPTS as usize,
        "expected {} retry attempts for busy wording, got {}",
        CLOSE_RETRY_ATTEMPTS, aaa_attempts
    );
}
```

The assertion pair -- `matches!(err, LockError::DeviceBusy(_))` plus
attempts == `CLOSE_RETRY_ATTEMPTS` -- pins both the type-level contract
(caller-visible variant) and the behavioral contract (retry loop engaged),
so a refactor that collapses DeviceBusy into Failed or short-circuits
retries will fail this test.

Before editing, confirm `MockRunner` re-serves the registered output on
repeated identical `CmdRequest`s (read `cli/src/cmd.rs` around the
`MockRunner::run` impl). If it one-shots, the test needs three separate
`with_output(CryptsetupClose{braid-aaa}, ...)` registrations. This is
mechanical, not a design change.

## Change 4: behavior-lock the exit-code contract in the repro

The unit test and fixture corrections assume `cryptsetup close` returns exit
5 for busy and exit 4 for inactive. That is a claim about the live tool,
not about braid's code, so it needs a live-tool guard. The existing
`tests/repro/cryptsetup-close-mounted.py` already exercises both states
(close-while-mounted and close-after-mapper-gone would be a small addition
to the latter). Tighten its assertions from "any non-zero exit + English
substring" to the specific exit codes:

In `tests/repro/cryptsetup-close-mounted.py:31-38` (the busy subtest):

```python
with subtest("cryptsetup close fails while mounted"):
    exit_code, stderr = machine.execute("cryptsetup close disk1 2>&1")
    print(f"Exit code: {exit_code}")
    print(f"Stderr: {stderr}")
    # EBUSY -> translate_errno -> exit 5. Busy detection in
    # cli/src/lock.rs close_mapper_with_retry relies on this exact code.
    assert exit_code == 5, \
        f"Expected exit 5 (EBUSY) while mounted, got {exit_code}. " \
        f"stderr: {stderr}"
```

In the final subtest (currently lines 40-43), add a third close attempt
after the mapper is gone and pin its exit code:

```python
with subtest("After umount, cryptsetup close succeeds"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close disk1")
    machine.fail("test -e /dev/mapper/disk1")

with subtest("cryptsetup close on already-closed mapper returns ENODEV (exit 4)"):
    # Pins the non-busy distractor exit code that lock.rs unit tests
    # model (see lock.rs `lock_mapper_close_fatal_when_umount_succeeded`
    # and siblings). If cryptsetup ever started returning exit 5 here,
    # close_mapper_with_retry would misclassify a fatal error as busy
    # and spin three retries before surfacing it.
    exit_code, stderr = machine.execute("cryptsetup close disk1 2>&1")
    print(f"Exit code: {exit_code}")
    print(f"Stderr: {stderr}")
    assert exit_code == 4, \
        f"Expected exit 4 (ENODEV) for already-closed mapper, " \
        f"got {exit_code}. stderr: {stderr}"
```

The existing stderr substring assertion (`"busy" in stderr_lower or ...`)
can stay as a descriptive sanity check or be dropped -- it's no longer
load-bearing for braid's classifier. Keep it, since a wording change is
still worth noticing in the repro log output even when it no longer
breaks braid.

The repro is already registered at `flake.nix:464` as
`repro-cryptsetup-close-mounted` and runs under `just test-repro
repro-cryptsetup-close-mounted` (the `repro-` prefix is required --
see memory `reference_just_test_repro_prefix`).

## Tests that already cover the busy path (unchanged, still valid)

- `lock_umount_busy_fails` (`cli/src/lock.rs:667-708`) -- exit 5 + "still
  in use" -> DeviceBusy -> lock reports umount error.
- `lock_umount_busy_includes_hint` (lines 716-760) -- same path, pins the
  lsof/fuser hint.
- `lock_umount_fails_busy_mapper_is_warning` (lines 977-1011) -- pins
  DeviceBusy suppression when umount already failed.
- `lock_umount_fails_orphan_busy_is_warning` (lines 1090-1131) -- same for
  the orphan branch.

All four use exit 5 + canonical busy wording. Classifier change does not
affect them.

## Verification

1. `cargo test -p braid-cli --lib lock::` -- new regression test passes;
   all six fixture-updated tests still exercise the non-busy-fatal path
   they were written to cover; all four existing busy tests still pass.
2. `just test-rust` -- full Rust unit test suite. No fixture capture or
   parser canary needed: cryptsetup version is unchanged and no parser
   surface changed.
3. `just test-repro repro-cryptsetup-close-mounted` -- gates the
   exit-code claims (busy=5, inactive=4) against live cryptsetup. Must
   pass for the plan to land: if this repro fails, the classifier
   change is wrong for the current pinned tool version and the plan
   needs to revisit.

## Non-goals

- No new `CryptsetupError::Busy` variant. `LockError::DeviceBusy` already
  exists and carries the message; callers that need to distinguish already
  can.
- No change to the umount-busy message at `cli/src/lock.rs:184-195`; that
  string comes from `umount(8)` and has a separate drift profile.
- No change to `just test-parsers` or fixtures: parser-critical tool
  versions are unchanged.
