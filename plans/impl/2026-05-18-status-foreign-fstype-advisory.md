# Plan: surface foreign fstype in `braid status`

## Context

`braid status` is the read-only diagnostic surface. Today, if the
configured mount point is occupied by a non-btrfs filesystem (e.g. a
leftover `tmpfs`, `ext4`, or anything else), the probe layer correctly
distinguishes that condition from "mount point empty" via
`ProbeError::NotBtrfs { mount_point, fstype }`, but `build_status`
collapses both cases into `StatusCode::NotMounted` with no extra
information. The `fstype` field is silently dropped.

This is a diagnostic dead-end for the operator:

1. `braid status` reports `Status: not mounted`.
2. The natural follow-up `braid unlock` calls `mountpoint -q` and sees
   a mount exists at the path, so it reports `pool already mounted at
   /mnt/storage` (`cli/src/mount.rs:215-224`).
3. The two messages contradict each other and neither names the actual
   issue. The operator has no clear path forward without inspecting
   `/proc/self/mountinfo` by hand.

`probe_pool` already carries the right information; the fix is to stop
discarding it on the way to the report. The closely related commands
(`add`, `lock`, `ack`) already surface the fstype -- this brings
`status` into the same pattern. The latched-alert pipeline (ADR 014)
is unaffected.

Intended outcome: when a foreign filesystem occupies the mount point,
`braid status` keeps reporting `not mounted` (the pool genuinely is not
mounted) but additionally emits a `warning:` line naming the fstype,
through the existing `advisories` channel. JSON consumers see the same
text in `report.advisories`.

## Critical files

- `cli/src/status.rs:392-400` -- `build_status`'s `NotBtrfs` arm; the
  one-line fix lives here.
- `cli/src/status.rs:361-383` -- `not_mounted_status`; already accepts
  an `advisories: Vec<String>` argument. No signature change.
- `cli/src/status.rs:972-974` -- human renderer iterates
  `report.advisories` and prefixes each with `warning: `. No change.
- `cli/src/probe.rs:62-113` -- `ProbeError::NotBtrfs` whose `Display`
  format (`"{mount_point} is mounted but fstype is {fstype}, not
  btrfs"`) we will reuse verbatim via `e.to_string()`. This is the
  canonical Display for the error variant; `status`'s job here is to
  surface that text intact rather than invent a new phrasing.
- `cli/src/test_fixtures/status.rs:78-83` -- existing
  `status_fs_ext4(&[])` fixture; reused for the new test.
- `cli/src/status.rs:2765-2782` -- existing test
  `status_not_btrfs_maps_to_not_mounted`; renamed and tightened to
  pin the new behavior.
- `manual/commands/status.md:144-161` -- Advisories section; extend
  to document the new advisory case.

## Implementation

### 1. `cli/src/status.rs` (`build_status`)

Replace the `NotBtrfs` arm so the advisory is appended before the
not-mounted body is built. Reuse `ProbeError::NotBtrfs`'s `Display`
impl so the wording is a single source of truth.

```rust
let mut advisories = luks::header_backup_advisories(paths);

let pool = match probe_pool(runner, fs, config.mount_point()) {
    Ok(p) => p,
    Err(e @ ProbeError::NotBtrfs { .. }) => {
        advisories.push(e.to_string());
        return Ok(not_mounted_status(config, paths, advisories));
    }
    Err(e) => return Err(e.into()),
};
```

Notes:

- `advisories` becomes `mut`. Header-backup advisories (if any) come
  first, then the wrong-fstype advisory. Ordering is incidental but
  this minimizes diff churn vs. the current line.
- The status code stays `StatusCode::NotMounted`. We do not add a new
  variant -- the JSON contract documented in `manual/commands/status.md`
  (`"intact"` / `"degraded"` / `"not_mounted"`) is preserved.
- The `alert_active` / latch handling inside `not_mounted_status` is
  untouched.

### 2. `cli/src/status.rs` (test module)

Rename and tighten the existing test. Today it only asserts
`cmd_status` returns `Ok` -- it does not catch the silent-drop. New
form pins both the status code and the advisory text via
`build_status` directly (which avoids capturing stdout).

```rust
// Intent: status keeps reporting `NotMounted` for a foreign-fstype
//   mount, AND surfaces the actual fstype to the operator via the
//   existing `advisories` channel, using `ProbeError::NotBtrfs`'s
//   canonical Display text verbatim.
// Why it exists: prior behavior dropped `ProbeError::NotBtrfs`'s
//   `fstype` field on the floor, leaving operators with a
//   contradictory "not mounted" / "pool already mounted" pair across
//   `status` / `unlock` and no path to diagnose the foreign mount.
// Scenario: operator left an `ext4` partition mounted at
//   `/mnt/storage`. `braid status` must keep `Status: not mounted`
//   but add the exact `warning: /mnt/storage is mounted but fstype
//   is ext4, not btrfs` line -- the verbatim Display of
//   `ProbeError::NotBtrfs`.
#[test]
fn build_status_not_btrfs_surfaces_fstype_advisory() {
    let runner = MockRunner::default();
    let fs = status_fs_ext4(&[]);
    let config = status_config();
    let (_tmp, paths) = isolated_paths();

    let built = build_status(
        &runner,
        &fs,
        &config,
        &paths,
        crate::test_fixtures::mock_virtio_backing_path_resolver(),
    )
    .expect("build_status should succeed for foreign-fstype mount");

    assert_eq!(built.report.status, StatusCode::NotMounted);
    assert_eq!(
        built.report.advisories,
        vec!["/mnt/storage is mounted but fstype is ext4, not btrfs"],
    );
    assert!(built.mounted_extras.is_none());
}
```

The exact-string assertion pins the canonical `ProbeError::NotBtrfs`
Display wording. If a future implementation drops `mount_point` from
the message, paraphrases it, or routes through a different error
variant, the test fails -- which is the point: `status`'s advisory is
defined as the verbatim Display of `ProbeError::NotBtrfs`, not a
free-form description, so the channel and the wording must stay
coupled.

Delete the old `status_not_btrfs_maps_to_not_mounted` -- the new test
strictly subsumes it (same fixture, same NotMounted assertion, plus
the advisory pin).

`not_mounted_status_envelope_is_minimal` (lines 1305-1325) is not
affected -- it calls `not_mounted_status` directly with an empty
advisories vector and pins the no-advisories envelope.

### 3. `manual/commands/status.md` (Advisories section)

Restructure the Advisories section to list both cases. Suggested
shape:

```markdown
### Advisories

`braid status` may print one or more `warning:` lines above the pool
summary. Each warning corresponds to an entry in the JSON
`advisories` array.

**Foreign filesystem at the mount point.** When something other than
the braid pool is mounted at the configured mount point (e.g. a stale
`tmpfs` or `ext4` mount left by another tool), `braid status` reports
`Status: not mounted` and names the actual filesystem type:

    warning: /mnt/storage is mounted but fstype is ext4, not btrfs

Unmount the foreign filesystem before retrying `braid unlock` --
otherwise `unlock` reports "pool already mounted" because something is
in fact mounted at that path.

**Pending LUKS header backups.** When a header-mutating operation
(`braid add`, `braid replace`, `braid enroll`) writes a local LUKS
header backup to `/var/lib/braid/luks-headers/<disk>.luksheader`,
`braid status` prints a warning until those files are removed:

    warning: LUKS header backups exist in /var/lib/braid/luks-headers -- copy offsite and delete local copies

The local copy is a transient byproduct of the header-mutating
operation, not the intended backup target. Copy each `.luksheader`
file to an off-system location (USB, another machine, cloud key
storage), then remove the local copy to silence the warning.

See [LUKS header backup workflow](../../docs/luks-unlock.md#header-backup-workflow-and-messaging)
for the full rationale.
```

The JSON-fields bullet ("`advisories`: array of human-readable
advisory strings...") already covers both cases -- no JSON section
change needed.

## Out of scope

- `cli/src/remove.rs:497`, `cli/src/replace.rs:1172`,
  `cli/src/remove_missing.rs:377` collapse `NotBtrfs` to "pool is not
  mounted. Nothing to remove." / "Cannot replace." -- same root cause,
  same dropped diagnostic, but those are mutating commands where the
  correct user response (abort) is unambiguous. Not fixing them now;
  worth a follow-up sweep if a future reviewer flags them. The fix
  shape would be analogous (push `e.to_string()` into the validation
  message) but each command's `*Error::Validation(String)` channel is
  a single string, not a `Vec`, so the wording would need a small
  in-line concatenation.
- No change to `StatusCode`. Adding a `WrongFilesystem` variant was
  considered and rejected: it would change the documented JSON enum,
  touch `display_human`, the human/JSON formatters, and the manual's
  status table, all to express something the existing `advisories`
  channel already expresses.
- No change to `monitor.rs` (`probe_pool_alerts` returning `Ok(None)`
  on `NotBtrfs`). That is the deliberate ADR 014 offline policy for
  the headless alert pipeline; it is not the diagnostic surface this
  plan is fixing.

## Verification

1. Rust unit tests:
   - `just test-rust` -- the new
     `build_status_not_btrfs_surfaces_fstype_advisory` must pass; the
     existing `not_mounted_status_envelope_is_minimal` and the rest
     of the `status` test module must continue to pass.
2. Manual sanity-check on the VM:
   - In a fresh test VM, before unlocking, mount tmpfs over the
     configured mount point:
     `sudo mount -t tmpfs none /mnt/storage`
   - Run `sudo braid status` and confirm the output order is:
     1. `warning: /mnt/storage is mounted but fstype is tmpfs, not btrfs`
        (advisories render above the pool summary -- see
        `cli/src/status.rs:972-982`).
     2. `Pool:     /mnt/storage`
     3. `Status:   not mounted`
   - Run `sudo braid status --json` and confirm
     `.advisories` contains the exact string
     `"/mnt/storage is mounted but fstype is tmpfs, not btrfs"`
     and `.status` is `"not_mounted"`.
   - Run `sudo braid unlock` and observe the existing "pool already
     mounted" message -- now the operator has both halves of the
     picture and can `umount /mnt/storage` to proceed.
3. No new VM test is required: the bug is in pure assembly logic that
   the Rust unit test covers end-to-end against the `Filesystem`
   trait. The existing `tests/cli/braid-status-rust.py` suite is not
   the right lane (no live tool output is parsed by this code path).
