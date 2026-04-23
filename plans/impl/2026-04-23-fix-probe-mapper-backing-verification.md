# Fix: probe `mapper_open` is path-existence only, permits wrong-backing

## Context

`probe_config_disk` (`cli/src/probe.rs:79-139`) computes `mapper_open` as pure
path existence at `cli/src/probe.rs:132`:

```rust
let mapper_open = fs.exists(&format!("/dev/mapper/{}", mn.0));
```

The returned `ConfigDiskState::PresentLuks { uuid, mapper_open }` then flows
into mount/add/replace. The probed `uuid` comes from
`cryptsetup luksUUID <by_id>` (lines 93-98) -- it describes the CONFIGURED
disk's LUKS UUID, not the container the live mapper is actually backed by.

The mount-side UUID guard at `cli/src/mount.rs:209-217` compares that probed
`uuid` to `member.luks_uuid`. It only catches "configured disk's UUID doesn't
match the enrolled UUID"; it never inspects the live mapper's backing. On
fresh pools `member.luks_uuid` is `None`, so even that guard is silent until
enrichment runs.

### Why this is a bug

If `/dev/mapper/braid-diskN` was opened externally (e.g. an unrelated LUKS
container aliased to the same mapper name), `probe_config_disk` reports
`mapper_open = true`. Blast radius:

- `plan_open_pool` (`mount.rs:153-266`) skips the LUKS open, sets
  `mount_device = /dev/mapper/braid-diskN`, and btrfs mounts from the wrong
  container -- possibly reading/writing a foreign filesystem.
- `add.rs:445-477` and `replace.rs` skip the open when `mapper_open=true`
  and then read the btrfs FSID via `classify_braid_disk_fsid` from
  `/dev/mapper/<name>` -- the superblock of a foreign disk.

This is exactly the kind of external-state anomaly braid's gateway probe
is supposed to reject. Fail-closed: each arm that consumes
`mapper_open=true` reads from `/dev/mapper/<name>` under the assumption it
is the configured disk; a mismatch can corrupt state or mount the wrong
filesystem.

## Fix

Make `cryptsetup status` the sole source of truth for `mapper_open`. Drop
the `fs.exists` gate entirely -- path existence is neither necessary nor
sufficient, and gating the status call behind a filesystem check reopens
the same TOCTOU window the bug already exemplifies. `cryptsetup status`
on a closed mapper cleanly reports inactive (parsed by
`parse_cryptsetup_status` at `cli/src/parse/cryptsetup_status.rs:26-45`),
so the single call covers both the closed case and the open case.

This matches the pattern already used in `probe_pool`
(`cli/src/probe.rs:212-248`): `cryptsetup status` -> underlying ->
`cryptsetup luksUUID` on underlying.

### Change 1: `cli/src/probe.rs`

After the LUKS2-version check, unconditionally run `cryptsetup status`
against the mapper name. Replace the line at 131-132:

```rust
let mn = mapper_name(name);
let mapper_open = fs.exists(&format!("/dev/mapper/{}", mn.0));
```

with:

```rust
let mn = mapper_name(name);
let mapper_open = probe_mapper_open(runner, name, &mn, &uuid)?;
```

Add the helper in the same module (no `fs` parameter -- the fix removes
`Filesystem` from the mapper-state decision):

```rust
fn probe_mapper_open<R: CommandRunner>(
    runner: &R,
    name: &str,
    mapper: &MapperName,
    expected_uuid: &LuksUuid,
) -> Result<bool, ProbeError> {
    let status_raw = runner.run(&CmdRequest::CryptsetupStatus {
        mapper: mapper.0.clone(),
    })?;
    let status = parse_cryptsetup_status(&status_raw)?;

    if !status.is_active {
        return Ok(false);
    }

    let underlying = match status.device.as_deref() {
        None | Some("") | Some("(null)") => {
            return Err(ProbeError::MapperConflict {
                name: name.to_owned(),
                expected: expected_uuid.clone(),
                found: None,
            });
        }
        Some(dev) => dev.to_owned(),
    };

    let uuid_raw = runner.run(&CmdRequest::CryptsetupLuksUuid {
        device: underlying,
    })?;
    let backing_uuid = parse_cryptsetup_luks_uuid(&uuid_raw)?.uuid;

    if &backing_uuid == expected_uuid {
        Ok(true)
    } else {
        Err(ProbeError::MapperConflict {
            name: name.to_owned(),
            expected: expected_uuid.clone(),
            found: Some(backing_uuid),
        })
    }
}
```

Add the new variant to `ProbeError`:

```rust
#[error(
    "disk '{name}' mapper '/dev/mapper/braid-{name}' is open but not \
     backed by the configured disk. Expected LUKS UUID {expected}, found \
     {found_display}. Close the conflicting mapper with \
     'sudo cryptsetup close braid-{name}' and re-run."
)]
MapperConflict {
    name: String,
    expected: LuksUuid,
    found: Option<LuksUuid>,
},
```

(`found_display` is rendered by a helper that prints the UUID or
`"no backing (stale mapper)"` for `None`.)

Rationale for returning `ProbeError` rather than widening
`ConfigDiskState::PresentLuks`:

- A new `MapperState::OpenMismatch` variant would ripple into
  `add.rs:445-477`, `replace.rs`, `enroll_key_file.rs`, `status.rs`, and
  `tui/probe.rs`, each of which would have to re-decide "is this safe to
  act on?". That trap is exactly
  `feedback_no_diagnostic_refinements_in_mutation_paths.md`.
- TUI already degrades gracefully on `Err(_)` from `probe_config_disk`
  (`tui/probe.rs:219`). `status.rs:365` propagates probe errors via `?`.
- Every downstream mutation path (mount, add, replace) has catastrophic
  blast radius on wrong-backing; fail-closed is the right stance
  (`feedback_fail_closed_by_downstream_blast_radius.md`).

### Change 2: no callers need to change

`cli/src/mount.rs`, `cli/src/add.rs`, `cli/src/replace.rs`,
`cli/src/enroll_key_file.rs`, `cli/src/status.rs`, and
`cli/src/tui/probe.rs` already propagate `ProbeError`. The new variant
surfaces automatically with its message.

## Tests

Four probe-level tests plus one caller-level test. Every test (including
the updated existing one) uses the repo block-comment convention:
`Intent / Why it exists / Scenario`.

### 1. `probe_config_disk_mapper_backing_mismatch_errors` (primary regression, in `cli/src/probe.rs`)

```
Intent: when /dev/mapper/braid-<name> is open but backed by a LUKS
  container with a different UUID than the configured disk, the probe
  must surface ProbeError::MapperConflict instead of reporting
  mapper_open=true.
Why it exists: this is the failure-layer test for the path-existence
  regression. Reverting probe_mapper_open back to fs.exists makes this
  test fail (mapper_open would become true), per
  feedback_test_at_failure_layer.md. Parser canaries cannot catch this
  wiring bug.
Scenario: a user or systemd-cryptsetup has opened an unrelated LUKS
  container under the name braid-toshiba before running braid unlock.
```

Expected: `ProbeError::MapperConflict { expected: aaaa..., found: Some(bbbb...) }`.

### 2. `probe_config_disk_mapper_status_inactive_is_closed`

```
Intent: when cryptsetup status reports the mapper as inactive, the probe
  must report mapper_open=false without error so the normal unlock flow
  opens the LUKS container fresh.
Why it exists: cryptsetup status is the sole source of truth for mapper
  state; the probe must handle the inactive case as "not open", not as
  an error.
Scenario: a fresh boot where no mapper has been opened yet; also the
  race where /dev/mapper path was briefly present but has since been
  torn down.
```

Expected: `Ok(ConfigDiskState::PresentLuks { mapper_open: false, ... })`.

### 3. `probe_config_disk_mapper_backing_null_errors`

```
Intent: when cryptsetup status reports active but device = (null), the
  probe must surface ProbeError::MapperConflict with found=None so
  downstream mutations do not operate on a stale mapper whose backing
  disk is gone.
Why it exists: hot-unplug leaves the mapper structure present but
  unusable; mount/add/replace reading from this mapper would see a
  detached block device.
Scenario: user hot-unplugs the backing disk during active use; the
  kernel removes the block device but the dm-crypt mapper is not yet
  removed by udev.
```

Expected: `ProbeError::MapperConflict { expected: aaaa..., found: None }`.

### 4. Updated `probe_config_disk_present_luks_open`

Add the convention block comment and stub `CryptsetupStatus` +
`CryptsetupLuksUuid` on the underlying device so the match path is
covered:

```
Intent: when the mapper is open AND cryptsetup status reports a backing
  device whose LUKS UUID matches the configured disk, the probe must
  report mapper_open=true.
Why it exists: regression guard that the new source-of-truth shift from
  fs.exists to cryptsetup status still admits the healthy already-open
  case.
Scenario: braid status run after a successful unlock; the mapper is
  open and backed by the correct disk.
```

Expected (unchanged): `Ok(ConfigDiskState::PresentLuks { mapper_open: true, uuid })`.

### 5. `status_surfaces_mapper_conflict` (caller-level, in `cli/src/status.rs`)

```
Intent: a ProbeError::MapperConflict raised by probe_config_disk must
  surface through build_status_report as a StatusError, not be swallowed
  or remapped.
Why it exists: a future regression in status-path error handling could
  swallow MapperConflict (e.g. via a .or_else that filters probe
  errors), leaving the safety fix invisible at the non-mutating
  command boundary. Probe-level tests alone do not cover propagation.
Scenario: braid status run on a host where an external mapper has been
  aliased over braid-diskN.
```

Build a 1-disk membership. Stub `probe_config_disk`'s dependencies so
that the mapper reports active with a mismatched backing UUID. Call
`build_status_report` and assert on the typed error shape:

```rust
match result {
    Err(StatusError::Probe(ProbeError::MapperConflict {
        expected,
        found,
        ..
    })) => {
        assert_eq!(expected, LuksUuid("aaaa...".into()));
        assert_eq!(found, Some(LuksUuid("bbbb...".into())));
    }
    other => panic!("expected StatusError::Probe(MapperConflict), got: {other:?}"),
}
```

The typed-shape assertion is what proves propagation (and resists
wording drift). If later work wants to lock the user-facing remediation
string as well, that belongs in a separate `Display`-targeted unit
test on `ProbeError` itself.

## Verification

1. `just test-rust` -- the four probe tests and the status caller test
   pass; existing `probe_config_disk_*` / `status_*` tests still pass.
2. Reversion check: temporarily restore the
   `let mapper_open = fs.exists(...)` line and confirm
   `probe_config_disk_mapper_backing_mismatch_errors` AND
   `status_surfaces_mapper_conflict` both fail. Restore.
3. `just test-vm` -- full VM suite; no behavior change for healthy
   paths, only the error surface on externally-aliased mappers.

No VM repro test is needed: the bug is pure wiring/dispatch at the
probe layer with no kernel/async state dimension; the failure-layer
unit test plus the caller-boundary status test cover it end-to-end.

## Critical Files

- `cli/src/probe.rs` -- new `probe_mapper_open` helper, remove
  `fs.exists` gate at line 132, new `ProbeError::MapperConflict`
  variant, three new unit tests + one updated unit test.
- `cli/src/status.rs` -- one new caller-boundary propagation test
  (no production code changes).

## Out of Scope

- Collapsing `luksUUID` + `luksDump` into a single `luksDump` call.
  Separate clean-up on the same function; bundling couples unrelated
  work. Land this safety fix first.
- Enriching `member.luks_uuid` on fresh pools so the mount-side guard
  at `mount.rs:209-217` lights up earlier. The probe-level fix already
  covers the fresh-pool case (it uses the probed `uuid`, not
  `member.luks_uuid`), so the fresh-pool hole closes automatically.
- Changing the `Filesystem` trait. The UUID-based backing check uses
  only the runner; no new filesystem primitives needed.
