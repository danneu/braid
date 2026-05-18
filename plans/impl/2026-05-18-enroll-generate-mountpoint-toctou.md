# Plan: close enroll `--generate` mountpoint TOCTOU

## Context

`braid enroll DIR --generate` writes 4096 bytes of LUKS keyfile material
into `DIR/braid.key`. The threat model at `docs/luks-unlock.md:50-66`
requires `DIR` to be an active mount point at write time so that "a
failed USB mount" cannot turn `/mnt/usb/braid.key` into persistent key
material on the host root filesystem. The gate was added in commit
`4adf3b7` ("fix(cli): require mountpoint for generated enroll keyfiles").

The current implementation runs the mountpoint probe only at plan time
inside `validate_generated_keyfile_target` (`cli/src/enroll_key_file.rs:549-558`),
called from `plan_enroll` at `:592-599`. The actual write happens much
later inside `EnrollPlan::execute` at `:479-481` via `generate_key_file`
(`:338-367`). Between the early check and the write, `execute` runs
`read_passphrase` (`:460`) plus `plan_enrollment` (`:471-477`), which
includes a batched Argon2 `--test-passphrase` verify against every pool
disk and a per-disk `luksDump` slot-1 inventory. With default Argon2id
parameters this window is seconds for a multi-disk pool.

If `DIR` is unmounted during that window (manual `umount`, USB pulled
with the mount entry still healthy, or `systemd-automount` idle-timeout),
the early gate's verdict no longer holds. `OpenOptions::create_new(...).mode(0o400).open(path)`
at `:346-350` will then create `braid.key` on whatever filesystem now
lives at `DIR`'s parent inode -- exactly the host-root leak the gate was
added to prevent. Today's test pin at `tests/cli/braid-enroll-generate.py:51-69`
covers the static "never mounted" refusal but not the mid-flight unmount
race.

This plan re-runs the same gate immediately before the write, shrinking
the race window from seconds to a single syscall pair. The change is
hermetic, mock-testable, consistent with decision-024's "re-verify at
every mutation boundary" precedent, and matches the documented threat
model exactly.

## Recommended fix: re-check before write

Re-call `validate_generated_keyfile_target` verbatim inside
`EnrollPlan::execute` between `plan_enrollment` and `generate_key_file`,
fork the function's mountpoint-failure wording on a new `recheck: bool`
flag so the post-passphrase user-facing error correctly says "was a
mount point but is no longer" rather than the misleading "you didn't
mount it" wording the plan-time call site emits.

### Files to modify

- `cli/src/enroll_key_file.rs` -- re-check call + forked error wording +
  new unit test.
- `cli/src/test_fixtures.rs` -- extend the `pub(crate) use enroll_key_file::{...}`
  re-export list at `:157-163` to include `enroll_mountpoint_ok` and
  `enroll_mountpoint_fail`. Today only the `enroll_with_mountpoint_*`
  wrappers are re-exported, but the new test needs the raw
  `(CmdRequest, RawCommandOutput)` pair so it can feed both outputs
  into `MockRunner::with_output_sequence` (the wrappers internally call
  `with_output`, which is the single-shot variant the test cannot use).
- `cli/src/test_fixtures/enroll_key_file.rs` -- no new fixtures needed;
  reuse `enroll_mountpoint_ok`, `enroll_mountpoint_fail`,
  `enroll_test_passphrase_ok`, `enroll_luks_dump_slot1_empty`,
  `enroll_luks_uuid_ok` already in `:142-216,229-240`.
- `docs/luks-unlock.md` -- strengthen the invariant text at `:50-66`.

No new helpers, no new dependency, no `unsafe`, no new abstraction seam.

### Code change

1. `validate_generated_keyfile_target` (`:523-559`) gains a `recheck: bool`
   parameter. The mountpoint-failure branch at `:552-556` selects one of
   two messages:
   - `recheck = false` (plan-time, today's wording, byte-for-byte):
     `"keyfile directory is not a mount point: {dir} -- mount the USB device there before running braid enroll --generate"`
   - `recheck = true` (execute-time, new wording):
     `"keyfile directory {dir} was a mount point at plan time but is no longer mounted -- the USB device may have been unmounted or disconnected during enrollment; remount and re-run braid enroll --generate"`

   The existence / is-dir / non-overwrite checks at `:530-547` and `:558`
   keep their existing wording -- those failure modes are too unlikely
   mid-run to need bespoke phrasing.

2. The single plan-time call site in `plan_enroll` at `:593` passes
   `recheck: false`.

3. `EnrollPlan::execute` (`:450-493`) gains, between `plan_enrollment`
   at `:471-477` and the `if self.generate` block at `:479`:

   ```rust
   if self.generate {
       validate_generated_keyfile_target(runner, params.key_file_path, /*recheck=*/ true)?;
       generate_key_file(params.key_file_path)?;
       eprintln!("ok: generated {}", params.key_file_path.display());
   }
   ```

   Re-using the bundled function (not factoring out a tiny mountpoint
   helper) is deliberate: the existence and is-dir preconditions can
   also be violated mid-run (admin races, concurrent process), and
   re-running all three checks together is cheap. The `generate=true`
   variant of `validate_key_file_path` invoked from `:558` correctly
   asserts `!path.exists()`, which is still true at this point because
   `generate_key_file`'s `create_new(true)` hasn't run yet.

4. The dry-run preview is unaffected. `compile_enroll_steps`
   (`:370-408`) does not model `MountpointCheck` as a `Step`, so no
   change to the step list is needed.

### Unit test

Fork `cmd_generate_wrong_passphrase_no_keyfile_created`
(`cli/src/enroll_key_file.rs:3040-3085`). Place the new test adjacent so
reviewers find both regression tests together.

Skeleton (mock the happy real-run path that previously did not exist in
the test suite, then make the second mountpoint call fail):

```rust
// Intent: --generate refuses to write braid.key if DIR was a mount
//   point at plan time but no longer is when execute reaches the
//   write -- and the re-check fires AFTER the slow planning window
//   (passphrase verify + slot inventory), not at the top of execute.
// Why it exists: pins the TOCTOU fix at the execute-time re-check.
//   The plan-time mountpoint gate alone cannot prevent a key leak
//   onto the host root filesystem if DIR is unmounted between the
//   early check and OpenOptions::create_new -- the window includes
//   passphrase read, Argon2 --test-passphrase verify, and per-disk
//   luksDump. A re-check positioned at the top of execute (before
//   plan_enrollment) would silently leave the same window open;
//   the ordering assertion below pins that the re-check is placed
//   immediately before generate_key_file. See docs/luks-unlock.md
//   "Keyfile creation target invariant".
// Scenario: operator mounted /tmp/usb correctly, ran enroll, but
//   systemd-automount timed out (or admin manually unmounted)
//   between passphrase prompt and key generation.
#[test]
fn cmd_generate_mountpoint_revoked_between_plan_and_write() {
    let (tmp, paths) = isolated_paths();
    let kf = tmp.path().join("braid.key");
    let pass_path = tmp.path().join("pass");
    std::fs::write(&pass_path, "rightpass\n").unwrap();

    let d1 = "/dev/disk/by-id/d1";
    let (uuid_req, uuid_out) = enroll_luks_uuid_ok(d1, test_uuid(500).as_str());
    let (tp_req, tp_stdin, tp_out) = enroll_test_passphrase_ok(d1, "rightpass");
    let (slot_req, slot_out) = enroll_luks_dump_slot1_empty(d1);

    // Sequential mountpoint outputs: pass first (plan_enroll), fail
    // second (EnrollPlan::execute pre-write recheck).
    let (mp_req, mp_ok_out) = enroll_mountpoint_ok(tmp.path());
    let (_, mp_fail_out) = enroll_mountpoint_fail(tmp.path());

    let runner = MockRunner::default()
        .with_output_sequence(mp_req, vec![mp_ok_out, mp_fail_out])
        .with_output(uuid_req, uuid_out)
        .with_luks_dump_text_luks2(d1)
        .with_mappers_closed(&["braid-disk1"])
        .with_output_stdin(tp_req, tp_stdin, tp_out)
        .with_output(slot_req, slot_out);

    let fs = enroll_fs(&[d1]);
    let membership = enroll_make_membership(&[("disk1", d1)]);

    let err = cmd_enroll_key_file(
        &runner, &fs,
        &EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: Some(&pass_path),
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        },
    ).expect_err("re-check must fail when mountpoint disappears mid-run");

    // 1. Execute-time variant of the error fires (not the plan-time wording).
    assert!(
        err.to_string().contains("was a mount point at plan time but is no longer mounted"),
        "expected execute-time mountpoint error, got: {err}"
    );
    // 2. No keyfile on disk.
    assert!(!kf.exists(), "braid.key must not be created when re-check fails");

    let reqs = runner.requests();

    // 3. apply_enrollment did not run -- no LUKS mutations attempted.
    assert!(
        !reqs.iter().any(|r| matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { .. })),
        "no luksAddKey calls expected; got requests: {reqs:?}"
    );
    assert!(
        !reqs.iter().any(|r| matches!(r, CmdRequest::CryptsetupLuksHeaderBackup { .. })),
        "no header backup expected; got requests: {reqs:?}"
    );

    // 4. Exactly two MountpointCheck calls (plan-time + execute-time).
    let mp_positions: Vec<usize> = reqs
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(r, CmdRequest::MountpointCheck { .. }))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        mp_positions.len(),
        2,
        "expected two MountpointCheck calls (plan + execute), got {} in {reqs:?}",
        mp_positions.len()
    );

    // 5. Critical ordering: the second MountpointCheck must come AFTER
    //    the passphrase verify and slot inventory -- otherwise the fix
    //    would only narrow the window without closing it, and a re-check
    //    placed at the top of execute (before plan_enrollment) would
    //    still pass this test's count-only assertion. The TestPassphrase
    //    and CryptsetupLuksDump requests mark the slow window's two
    //    boundaries; pin that the re-check fires after both.
    let tp_position = reqs
        .iter()
        .position(|r| matches!(r, CmdRequest::CryptsetupTestPassphrase { .. }))
        .expect("CryptsetupTestPassphrase must run as part of verify");
    let dump_position = reqs
        .iter()
        .position(|r| matches!(r, CmdRequest::CryptsetupLuksDump { .. }))
        .expect("CryptsetupLuksDump (slot inventory) must run as part of plan_enrollment");
    assert!(
        mp_positions[0] < tp_position,
        "plan-time mountpoint check must precede passphrase verify; got mp_positions={mp_positions:?} tp={tp_position} in {reqs:?}"
    );
    assert!(
        mp_positions[1] > tp_position,
        "execute-time mountpoint re-check must follow passphrase verify (not be at top of execute); got mp_positions={mp_positions:?} tp={tp_position} in {reqs:?}"
    );
    assert!(
        mp_positions[1] > dump_position,
        "execute-time mountpoint re-check must follow slot inventory; got mp_positions={mp_positions:?} dump={dump_position} in {reqs:?}"
    );
}
```

Key reuses from existing fixtures:
- `MockRunner::with_output_sequence` at `cli/src/cmd.rs:1314` -- pops
  front-to-back, falls back to static map then `MissingMock`. In-tree
  precedent at `cmd.rs:2024-2027`. `MockRunner::requests()` at
  `cmd.rs:1395` returns a `Vec<CmdRequest>` in insertion order, which
  the ordering assertions rely on.
- `enroll_mountpoint_ok` / `enroll_mountpoint_fail` at
  `test_fixtures/enroll_key_file.rs:198-216` -- both return the same
  `CmdRequest::MountpointCheck` shape; only the output differs, so the
  sequence wires up cleanly. NB: these helpers are `pub(crate)` inside
  the `enroll_key_file` fixture module but are NOT currently re-exported
  through `cli/src/test_fixtures.rs` -- the `:157-163` re-export list
  only surfaces the `enroll_with_mountpoint_*` wrappers. Adding the two
  raw helpers to that list (per "Files to modify" above) is a
  one-line change and unblocks the test as written.
- `enroll_test_passphrase_ok`, `enroll_luks_dump_slot1_empty`,
  `enroll_luks_uuid_ok`, `enroll_make_membership`, `enroll_fs`,
  `isolated_paths` -- all already present and in use by sibling tests.
- The deliberate omission of any `CmdRequest::CryptsetupLuksAddKeyFile`
  / `CryptsetupLuksHeaderBackup` mock turns `apply_enrollment` into a
  load-bearing trip-wire: if the re-check fails to fire, the runner
  returns `CmdError::MissingMock` (`cmd.rs:1372`) and the test fails
  with the wrong error -- the explicit assertions on `runner.requests()`
  pin the intent.

### Skipped: VM test

No VM test is added. The race window the production code suffers
(seconds of default Argon2id) does not exist in the VM test suite,
which uniformly forces `--pbkdf pbkdf2 --pbkdf-force-iterations 1000`
(verified across `tests/cli/*.py`). Reproducing the race in a VM would
require synthetic slowdowns (drop the fast pbkdf flag, hook-inject a
sleep, or add a test-only env var) and timing tolerances that risk
flakiness -- a flaky pass/fail oracle on a security-critical regression
is worse signal than a 100%-reliable unit test. The hermetic unit test
above pins the exact code path the production race exploits. Existing
`braid-enroll-generate.py` Test 1 (`:71-92`) continues to cover the
end-to-end happy path and proves `mountpoint(1)` is wired through
correctly at runtime.

### Docs

`docs/luks-unlock.md:50-66` ("Keyfile creation target invariant") is
updated to make the "must hold at write time" requirement explicit.
Today's text reads "must first verify ..." -- the word "first" implies a
single entry-time check. Replace with wording that names the race
window and the re-check, e.g.:

> Any braid command path that creates or overwrites `braid.key` in a
> user-supplied directory must verify that directory exists, is a
> directory, and is an active mount point both at plan time and again
> immediately before writing `braid.key`. The plan-time check alone is
> insufficient: the seconds-long window between planning and the actual
> write (passphrase prompt, Argon2 `--test-passphrase` verify against
> every pool disk, per-disk `luksDump` slot inventory) lets a USB device
> be unmounted (manual `umount`, hot-unplug, `systemd-automount` idle
> timeout) after the gate passes, which would otherwise let the keyfile
> land on the host root filesystem.

`manual/commands/enroll.md:60` is left as-is. The user-visible step list
correctly states that the target directory must already be a mount
point; the re-check is a defense-in-depth implementation detail that
does not change what the operator needs to do (mount the USB stick and
leave it mounted).

No edit to `docs/decisions/024-luks-uuid-identity.md`: that decision is
scoped specifically to LUKS UUID identity (`:153-199` "Tests That
Enforce This" are all UUID-axis tests), and adding a mountpoint test
entry would be off-topic. The "re-verify before mutation" principle the
fix follows is generic but already documented in spirit at `024:208-210`
and at the same-file precedent in `enroll_key_file.rs:763-775` -- no
new decision doc is warranted for a single-paragraph addition to the
threat-model file.

## Verification

1. `just test-rust` -- new unit test passes; existing tests (especially
   `cmd_generate_wrong_passphrase_no_keyfile_created`,
   `cmd_generate_dry_run_short_circuits`, and the mountpoint-step tests
   around `enroll_key_file.rs:1305,1356,1402,1444` that assert exact
   `runner.requests()` shapes) still pass. Tests at `:1305,1356,1402`
   may need updates if their `runner.requests()` length assertion
   counts the plan-time mountpoint call only -- check during impl that
   none of them exercise a real-run code path that now fires a second
   mountpoint check (none should: `:1305,1356` are dry-run / plain-dir
   paths that bail before `execute`; `:1402` is the existing-keyfile
   path that doesn't take the `--generate` branch; `:1444` asserts the
   non-generate path makes zero mountpoint calls).

2. `just test-vm braid-enroll-generate` -- end-to-end behavior is
   unchanged on the happy path (Test 1, `:71-92`) and on the static
   "never mounted" refusal (Test 0, `:51-69`). The new re-check fires
   on every `--generate` invocation but is silent when the mount
   survives, so no test output changes.

3. Manual smoke (optional, not blocking): on a VM, mount tmpfs at
   `/tmp/usb`, start `braid enroll /tmp/usb --generate --passphrase-stdin`
   under default Argon2id, `umount /tmp/usb` from a second shell during
   the prompt, send the passphrase, confirm braid errors out with the
   new wording and `/tmp/usb/braid.key` exists nowhere on the host root.
