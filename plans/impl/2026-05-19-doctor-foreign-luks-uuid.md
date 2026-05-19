# Plan: doctor surfaces foreign LUKS UUIDs in the live pool

## Context

`braid status` directs operators at `cli/src/status.rs:1175` to "run
'braid doctor' to investigate" when its verbose `Disks:` formatter
encounters a live pool device whose LUKS UUID is not present in pool
membership (the "foreign mapper" case introduced by
`plans/impl/2026-05-14-stop-foreign-mapper-replace-hint.md` to replace
the destructive `braid replace --old <mapper>` hint). The corresponding
`enrich_from_pool_state` (`cli/src/membership.rs:594-621`) already
detects foreign UUIDs and returns them via
`EnrichmentReport { foreign: BTreeMap<LuksUuid, MapperName> }`.

However, `run_doctor` (`cli/src/doctor.rs:881-913`) registers ten
checks, none of which probe the live pool for foreign UUIDs. The
`EnrichmentReport` plumbing is computed at every caller
(`unlock.rs:151`, `add.rs:1307`, `add.rs:1365`, `replace.rs:845`) and
discarded; `add.rs:1299-1303` and `replace.rs:842-844` carry explicit
"Phase 5" deferral comments naming doctor/status as the future
consumer (`unlock.rs` discards for a different reason -- its in-memory
membership is authoritative under the wrapper flock). The wrapper
`refresh_pool_metadata` + `RefreshOutcome`
(`cli/src/membership.rs:562-666`) was scaffolding for that wiring; the
unlock pivot in commit `2f03a00` removed its last non-test caller, so
the helper, its corruption-sidecar machinery, and its unit test are
now orphaned. Their docstrings still tell readers that doctor consumes
the report.

Result today: when a foreign LUKS UUID is admitted to the live btrfs
pool (e.g. a stray `cryptsetup open` + `btrfs device add -f`), the
operator sees a transient `eprintln!` warning that scrolls past on the
next `unlock` and a status hint pointing at doctor -- but doctor says
nothing.

Outcome: doctor reports the foreign UUID and the operator-facing
docstrings stop overpromising a consumer that does not exist.

## Approach

Add a `foreign_luks_uuid` doctor check that probes the live pool the
same way `unlock` does and calls a new pure helper
`membership::foreign_luks_uuids(&membership, &pool)` -- which returns
the `BTreeMap<LuksUuid, MapperName>` of UUIDs present in the live
pool but absent from membership, **without** mutating membership or
emitting `eprintln!`. Render the map as **Fail** when non-empty. Skip
when the pool is not mounted (no live `PoolState` to probe) or when
pool.json is absent. Warn on probe error. In the same commit:

- refactor `enrich_from_pool_state` to delegate its foreign-UUID
  detection to the new helper while retaining its existing per-UUID
  `eprintln!` warning and membership mutation (so mutating callers
  keep their behavior pinned by the existing
  `enrich_from_pool_state_foreign_live_uuid_does_not_admit` test);
- delete the orphan `refresh_pool_metadata`/`RefreshOutcome`
  scaffolding;
- correct the docstrings.

Doctor **must not** call `enrich_from_pool_state` itself -- doing so
would re-emit the per-UUID `eprintln!` on every `braid doctor` run.
The pure helper is the only doctor-facing entry point.

### Files to modify

- `cli/src/doctor.rs` -- add the check, thread `&dyn Filesystem`
  through `cmd_doctor` -> `run_doctor` -> `DoctorContext`.
- `cli/src/membership.rs` -- delete orphans, expose
  `foreign_luks_uuids` pure helper, correct docstrings on
  `EnrichmentReport` (the surviving type).
- `cli/src/add.rs` -- replace the stale "Phase 5" comment at lines
  1299-1303 (bootstrap branch) with the resolved wording. The second
  enrich call at line 1365 (live-pool branch) gains no Phase-5
  comment today but should get a matching one-liner so both arms
  agree.
- `cli/src/replace.rs` -- replace the stale "Phase 5" comment at
  lines 842-844 with the resolved wording.
- `docs/decisions/017-runtime-disk-membership.md` -- the "See" list at
  line 104 still points readers at `refresh_pool_metadata`; replace
  with the surviving membership APIs.
- `flake.nix` -- register the new VM test (after `braid-doctor-beep`,
  near line 277 of the `checks` block).
- `tests/cli/braid-doctor-foreign-luks-uuid.py` (new).
- `tests/cli/braid-doctor-foreign-luks-uuid.nix` (new).

### Implementation steps

**1. Thread `Filesystem` into doctor.**
`probe::probe_pool` (`cli/src/probe.rs:371-375`) requires `&R: CommandRunner`,
`&F: Filesystem`, `&MountPoint`. Doctor currently has only a
`CommandRunner`. Smallest change:

- `cmd_doctor` constructs `RealFilesystem` alongside `RealRunner` and
  passes `&fs` into `run_doctor`.
- `run_doctor` gains a `fs: &dyn Filesystem` parameter (or
  `fs: &F: Filesystem`, matching the runner generic style).
- `DoctorContext` gains an `fs: &'a dyn Filesystem` field; the new
  check is the only consumer.
- Existing tests that call `run_doctor` (`cli/src/doctor.rs:975, 1074,
  1099, 1130, ...`) pass `&RealFilesystem` -- since none currently
  exercise the new check, a single `let fs = RealFilesystem;` at each
  call site is enough. (Existing tests don't need a mock `Filesystem`
  because `check_foreign_luks_uuid` short-circuits via the
  pool-mounted gate; live-pool tests gate on probe results, which run
  through `runner`.)

**2. Cache the probe.**
Add `pool_state: Option<Result<PoolState, ProbeError>>` to
`DoctorContext` (`cli/src/doctor.rs:102-110`). Add
`ensure_pool_state(ctx)` mirroring `ensure_df_snapshot` at
`cli/src/doctor.rs:498-531`: bail if config is `None`, bail if mount
gate fails, else probe and cache. Hold the `Result` (not just the
`Ok`) so probe errors stay reachable; `check_foreign_luks_uuid` is the
first non-stats consumer of the live pool, but seeding this now means
a future `check_pool_*` can piggyback on the same probe.

**3. Implement the check.**
Follow the exact pattern of `check_pool_missing_devices`
(`cli/src/doctor.rs:601-634`):

```rust
fn check_foreign_luks_uuid<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    const NAME: &str = "foreign_luks_uuid";
    if ctx.config.is_none() {
        return CheckResult::skip(NAME, "skipped (config not available)");
    }
    if ensure_mountpoint_is_mounted(ctx) != Some(true) {
        return CheckResult::skip(NAME, "skipped (pool not mounted)");
    }

    let membership = match membership::load_membership(ctx.paths) {
        Ok(m) => m,
        Err(membership::MembershipError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return CheckResult::skip(NAME, "skipped (no pool membership file)");
        }
        Err(e) => {
            return CheckResult::warn(NAME, format!("could not load pool membership: {e}"));
        }
    };

    ensure_pool_state(ctx);
    let pool = match ctx.pool_state.as_ref().expect("ensure_pool_state seeds the cache") {
        Ok(p) => p,
        Err(e) => {
            return CheckResult::warn(NAME, format!("could not probe pool: {e}"));
        }
    };

    // Pure helper -- no eprintln, no mutation. enrich_from_pool_state
    // is intentionally NOT called here (see Approach: doctor must not
    // double-emit the per-UUID warning on every braid doctor run).
    let foreign = membership::foreign_luks_uuids(&membership, pool);

    if foreign.is_empty() {
        return CheckResult::ok(NAME, "no foreign LUKS UUIDs in live pool");
    }

    let n = foreign.len();
    let entries: Vec<String> = foreign
        .iter()
        .map(|(uuid, mapper)| format!("{uuid} at mapper {mapper}"))
        .collect();
    CheckResult::fail(
        NAME,
        format!(
            "{n} foreign LUKS UUID{plural} in live pool: {body} -- restore with 'btrfs device remove /dev/mapper/<mapper> {mp}' then 'cryptsetup close <mapper>'",
            plural = if n == 1 { "" } else { "s" },
            body = entries.join("; "),
            mp = ctx.config.as_ref().unwrap().mount_point(),
        ),
    )
}
```

Notes on the message body:
- The substring `foreign LUKS UUID` is the test pin (matches the plan
  wording at `plans/impl/2026-05-12-luks-uuid-as-identity/plan.md:1374`).
- Each entry renders the `LuksUuid` and `MapperName` via their existing
  `Display` impls (`cli/src/types.rs:60-64, 355-359`).
- Remediation order is **btrfs device remove first, then cryptsetup
  close** -- ADR 007 (`docs/decisions/007-disk-pool-management.md:36`,
  "After btrfs remove, `cryptsetup close` the mapper") and the
  in-tree `cli/src/remove.rs:201-218` already plan it this way. The
  inverse order can fail busy while btrfs still owns the device; the
  repo has a live repro at
  `tests/repro/cryptsetup-close-mounted.py:31` proving EBUSY (exit 5).
  Tests must pin the substring order `btrfs device remove` ... before
  `cryptsetup close` so a future docstring edit cannot silently
  invert it.
- The canonical doctor message style at `cli/src/doctor.rs:602-633`
  includes a `replace with: braid replace ...` suffix on the
  symmetric warn case; this check uses the same `-- <hint>` shape.
- `enrich_from_pool_state` already `eprintln!`s the per-foreign-UUID
  warning at `cli/src/membership.rs:612-616`. Doctor must NOT call it,
  or every `braid doctor` run would re-emit the warning for every
  foreign UUID. Pinned decision: expose
  ```rust
  pub fn foreign_luks_uuids(
      membership: &PoolMembership,
      pool: &PoolState,
  ) -> BTreeMap<LuksUuid, MapperName>
  ```
  in `cli/src/membership.rs`. The helper does the UUID join and
  nothing else -- no warning, no mutation, no `Result`. Refactor
  `enrich_from_pool_state` to call `foreign_luks_uuids` for the join,
  then iterate the returned map to emit the existing `eprintln!`
  warnings and (for known UUIDs in the mutating loop) update the
  membership devid/added_at fields. Doctor's check calls the pure
  helper directly. This keeps the warning behavior pinned by the
  existing `enrich_from_pool_state_foreign_live_uuid_does_not_admit`
  test (`cli/src/membership.rs:1308`) and avoids duplicating the join
  logic. Two complementary tests guard the silence invariant: the
  unit test `foreign_luks_uuids_lists_unknown_uuids_without_warning`
  pins that the *helper* itself is silent and non-mutating; the VM
  test's stderr-capture assertion (see Verification) pins that the
  *doctor call path* does not regress into `enrich_from_pool_state`.
  The unit test alone is not sufficient -- if a future edit accidentally
  routed `check_foreign_luks_uuid` through `enrich_from_pool_state`,
  the helper test would still pass while `braid doctor` re-emitted the
  warning for every foreign UUID. Only an end-to-end stderr check at
  the CLI boundary catches that regression.

**4. Insert in run_doctor.**
After `check_pool_missing_devices` (`cli/src/doctor.rs:902`), so the
pool-state checks stay grouped. Add the human label to the table at
`cli/src/doctor.rs:932-947`:

```rust
"foreign_luks_uuid" => "foreign uuids",
```

Label width is 13 chars; fits the existing `{label:<14}` padding.

**5. Delete orphans.**
After the new check is wired (which uses `foreign_luks_uuids` directly
rather than `refresh_pool_metadata`), remove from
`cli/src/membership.rs`:
- `pub enum RefreshOutcome` (lines ~562-581).
- `pub fn refresh_pool_metadata` (lines ~623-666).
- `struct CorruptSidecarError` (lines ~668-676).
- `fn write_corrupt_sidecar` (line ~684), `fn pick_unused_sidecar`
  (line ~711), and `fn format_rfc3339_utc_seconds` if and only if it
  is not used elsewhere (`grep -rn "format_rfc3339_utc_seconds" cli/src`
  to confirm).
- Unit test `refresh_pool_metadata_corrupt_writes_sidecar_and_leaves_original`
  (line ~1351) and any sub-helpers.

Update the docstring on `EnrichmentReport`
(`cli/src/membership.rs:548-560`) to reflect the new consumer:
"`foreign` lists every UUID present in the live pool that membership
did NOT admit. The pure helper `foreign_luks_uuids` exposes the join
without mutating; `braid doctor`'s `foreign_luks_uuid` check renders
the result as Fail when non-empty."

**6. Update ADR 017.**
`docs/decisions/017-runtime-disk-membership.md:104` currently reads:

```
- `cli/src/membership.rs` -- load/save/validate membership, `DiskMember`, `refresh_pool_metadata`
```

Replace with the surviving membership API surface:

```
- `cli/src/membership.rs` -- load/save/validate membership, `DiskMember`, `PoolMembership`, `enrich_from_pool_state`, `foreign_luks_uuids` (pure helper consumed by `braid doctor`'s `foreign_luks_uuid` check)
```

This keeps the architecture doc aligned with the deletion in step 5;
without this, a reader following ADR 017's "See" list will look for a
function that no longer exists. No other ADR mentions
`refresh_pool_metadata` (confirmed via
`grep -rn refresh_pool_metadata docs/` -- only ADR 017 references it).

**7. Resolve stale "Phase 5" comments in mutating commands.**
After the new doctor check lands, the deferral comments at
`cli/src/add.rs:1299-1303` and `cli/src/replace.rs:842-844` become
incorrect: they tell readers that doctor/status wiring is future work
when in fact this commit delivers it. Replace each with a one-liner
matching the new reality:

- `cli/src/add.rs:1299-1303` (bootstrap branch) -- replace the
  multi-line "Phase 5" block with:
  ```
  // EnrichmentReport.foreign is intentionally discarded here:
  // braid doctor's foreign_luks_uuid check probes the live pool
  // on demand and surfaces foreigners persistently, so the
  // per-command report does not need its own consumer.
  ```
- `cli/src/add.rs:1365` (live-pool branch) -- add the same one-liner
  above the `let _ = membership::enrich_from_pool_state(...)` call so
  the two arms agree.
- `cli/src/replace.rs:842-844` -- replace the existing "doctor/status
  wiring is Phase 5" block with the same wording.

These edits are comment-only. No behavior change; no test changes.
They keep the source-tree narrative consistent with the new
`foreign_luks_uuid` check and prevent future readers from
re-introducing duplicate plumbing in the mistaken belief that doctor
still needs the report routed through each command.

### Reused utilities

- `probe::probe_pool` (`cli/src/probe.rs:371-375`): canonical live
  pool probe.
- `RealFilesystem` (`cli/src/probe.rs:25`): satisfies the
  `Filesystem` trait for `cmd_doctor`.
- `membership::load_membership` (`cli/src/membership.rs:425-427`):
  matches `check_declared_disks`'s skip-on-NotFound + warn-on-other
  pattern at `cli/src/doctor.rs:444-458`.
- `CheckResult::ok/warn/fail/skip` (`cli/src/doctor.rs:52-84`).
- `ensure_mountpoint_is_mounted` (`cli/src/doctor.rs:482-496`) for the
  pool-not-mounted skip gate.
- `ensure_df_snapshot` (`cli/src/doctor.rs:498-531`) as the pattern
  template for `ensure_pool_state`.

## Verification

### Rust unit test

A naive `MockRunner`-only fixture is insufficient because
`probe::probe_pool` (`cli/src/probe.rs:376`) calls
`mount_check::fstype_at_mount_via_fs(fs, ...)`
(`cli/src/mount_check.rs:185`) which reads `/proc/self/mountinfo`
through the `Filesystem` trait *before* any runner-mediated probe. If
the test passes `RealFilesystem`, mountinfo on a build host will not
contain the configured pool mount and `probe_pool` returns
`PoolState { mounted: false, devices: vec![] }` -- the foreign-UUID
detection never fires and the Fail/Ok tests silently degrade to
nothing-to-see.

Required fixture surface for every live-pool doctor test:

1. **`Filesystem` mock** seeded with a btrfs mountinfo line for the
   pool's `mount_point` (e.g. `/mnt/storage`). Match the body shape
   already pinned in
   `cli/src/test_fixtures/idle.rs:21-22` and the pattern in
   `cli/src/preflight.rs:570-596` -- the body is one line:
   ```
   36 35 0:32 / /mnt/storage rw,noatime shared:1 - btrfs /dev/mapper/braid-disk1 rw\n
   ```
   Either reuse `IdleMockFs::mounted_btrfs_only()` (already
   `pub(crate)` in `test_fixtures/idle.rs:46-48`) if its surface is
   acceptable, or add a doctor-local `DoctorMockFs` mirroring the
   preflight pattern. The plan recommends a doctor-local fixture so
   doctor tests can evolve independently of idle's fixture; place it
   alongside the existing `doctor::tests` module or under
   `crate::test_fixtures::doctor`.
2. **Runner mocks** (via `MockRunner`) for:
   - `CmdRequest::MountpointCheck { path: /mnt/storage }` -> exit 0
     (drives `ensure_mountpoint_is_mounted` -> true).
   - `CmdRequest::BtrfsFilesystemShow { mount_point }` -> raw
     `btrfs filesystem show` output naming two devices (the member
     mapper and the foreign mapper). Reuse the existing
     `status_btrfs_show_*` helpers in
     `cli/src/test_fixtures/status.rs` if their shape matches; else
     hand-roll a two-disk show body.
   - `CmdRequest::CryptsetupStatus { mapper: braid-disk1 }` and
     `{ mapper: braid-stranger }` -> active status with their
     respective backing-device paths.
   - `CmdRequest::CryptsetupLuksUuid { device: <backing> }` -> `U1`
     for `braid-disk1`'s backing, `U_F` for `braid-stranger`'s.
3. **Persisted membership.** Construct `StatePaths` over a tempdir
   (`isolated_paths()` in `cli/src/test_fixtures/mod.rs`), build a
   `PoolMembership` with one member at `U1`
   (`crate::test_fixtures::disk_member_with`), and call
   `membership::save_membership(&m, &paths)` so
   `check_foreign_luks_uuid` can `load_membership` it back. The
   check loads from disk (not from a parameter), matching the
   `check_declared_disks` pattern; an in-memory-only membership
   would not be visible.
4. **Config file** at the same `config_path` doctor tests already
   use, with `mount_point = "/mnt/storage"`. Existing doctor tests
   (`cli/src/doctor.rs:1074, 1099, 1130, ...`) already write a
   tempfile config -- reuse that helper.

Concrete tests to add in `cli/src/doctor.rs#tests`:

- **`check_foreign_luks_uuid_fails_when_pool_has_unknown_uuid`** --
  fixture above; foreign mapper present in the canned
  `BtrfsFilesystemShow` output; assert the `foreign_luks_uuid` check
  has `status == Fail`, `report.status == Fail`, and message contains
  `"foreign LUKS UUID"`, the foreign UUID (lowercase-hyphenated), and
  `"braid-stranger"`. Also assert the substring order:
  `msg.find("btrfs device remove").unwrap() < msg.find("cryptsetup close").unwrap()`
  (pins the F2 invariant against future docstring edits).
- **`check_foreign_luks_uuid_ok_when_membership_admits_all_uuids`** --
  same fixture but the only pool device's UUID is `U1` (no foreign);
  assert `status == Ok` and message contains
  `"no foreign LUKS UUIDs"`.
- **`check_foreign_luks_uuid_skips_when_pool_not_mounted`** --
  `MockRunner` returns non-zero for `MountpointCheck`; assert
  `status == Skip` with message
  `"skipped (pool not mounted)"`. The `Filesystem` mock can be
  default/empty here since the check should bail before probing.
- **`check_foreign_luks_uuid_skips_when_membership_missing`** --
  same `Filesystem` + `MockRunner` fixture, but membership is NOT
  persisted (`load_membership` raises `Io::NotFound`); assert
  `status == Skip`.

Also add a `cli/src/membership.rs#tests` unit test
`foreign_luks_uuids_lists_unknown_uuids_without_warning`:

- Build a `PoolMembership` with one member at `U1`.
- Build a `PoolState` with `U1` (known) and `U_F` (foreign).
- Call the new pure helper `membership::foreign_luks_uuids(&m, &pool)`.
- Assert the returned `BTreeMap` contains exactly `U_F` -> the
  foreign mapper, and that the call did NOT mutate `m` (clone-and-
  compare).
- This is the regression pin for the
  "doctor must not double-emit the per-foreign-UUID `eprintln!`"
  invariant: the pure helper is silent; `enrich_from_pool_state`
  retains its warning behavior pinned by existing tests
  (`enrich_from_pool_state_foreign_live_uuid_does_not_admit` at
  `cli/src/membership.rs:1308`).

### NixOS VM test

Add `tests/cli/braid-doctor-foreign-luks-uuid.py` + `.nix`. Three
disks (disk1, disk2 for the RAID1 pool; disk3 as the foreign source).
The .py preamble in the project's three-line form
(`docs/testing.md`), e.g.:

```python
# Intent: braid doctor's foreign_luks_uuid check fails when the live btrfs
#   pool admits a LUKS device whose UUID is not in pool.json.
# Why it exists: enrich_from_pool_state detects foreign live UUIDs but the
#   eprintln warning scrolls off-screen; doctor must surface the structured
#   diagnosis the status hint already promises (status.rs:1175).
# Scenario: A healthy 2-disk RAID1 pool is mounted. The operator (or a
#   stray cryptsetup session) freshly luksFormats disk3, opens it as a
#   mapper, and force-adds it to btrfs. braid doctor --json must report
#   the foreign_luks_uuid check as fail and exit non-zero.
```

Test body (kebab-case command names; `--` per AGENTS.md CLI style):

```python
import json, shlex
start_all()
machine.wait_for_unit("multi-user.target")
pp = shlex.quote("testpassphrase")

def add_cmd(name, disk):
    return (
        f"printf '%s\\n' {pp} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        f"--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"{name}=/dev/disk/by-id/virtio-{disk} --passphrase-stdin --yes"
    )

with subtest("Build a 2-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1", "disk1"))
    machine.succeed(add_cmd("disk2", "disk2"))

with subtest("doctor is clean before foreign UUID is injected"):
    raw = machine.succeed("braid doctor --json")
    rep = json.loads(raw)
    checks = {c["name"]: c for c in rep["checks"]}
    assert "foreign_luks_uuid" in checks, f"check missing: {list(checks)}"
    assert checks["foreign_luks_uuid"]["status"] == "ok", checks["foreign_luks_uuid"]

with subtest("luksFormat disk3 with a fresh UUID (NOT a clone of disk1/disk2)"):
    machine.succeed(
        f"printf '%s' {pp} | "
        "cryptsetup luksFormat --batch-mode --pbkdf pbkdf2 "
        "--pbkdf-force-iterations 1000 --key-file=- "
        "/dev/disk/by-id/virtio-disk3"
    )
    machine.succeed(
        f"printf '%s' {pp} | "
        "cryptsetup open --key-file=- /dev/disk/by-id/virtio-disk3 braid-stranger"
    )

with subtest("Force-add foreign mapper into the live pool"):
    machine.succeed("btrfs device add -f /dev/mapper/braid-stranger /mnt/storage")
    fi = machine.succeed("btrfs filesystem show /mnt/storage")
    assert "braid-stranger" in fi, fi

with subtest("braid doctor fails with foreign_luks_uuid Fail"):
    # Split stdout and stderr at the shell so we can assert on each
    # independently. The JSON report belongs on stdout; the
    # per-foreign-UUID eprintln from enrich_from_pool_state would
    # appear on stderr if doctor regressed into the warning path.
    exit_code, stdout = machine.execute(
        "braid doctor --json 2>/tmp/braid-doctor.err"
    )
    assert exit_code != 0, f"doctor must exit non-zero on Fail:\n{stdout}"
    rep = json.loads(stdout)
    assert rep["status"] == "fail", rep["status"]
    checks = {c["name"]: c for c in rep["checks"]}
    chk = checks["foreign_luks_uuid"]
    assert chk["status"] == "fail", chk
    msg = chk["message"]
    for needle in ["foreign LUKS UUID", "braid-stranger"]:
        assert needle in msg, f"missing {needle!r} in:\n{msg}"

    # Silence regression guard: doctor must NOT call
    # enrich_from_pool_state (the warning-emitting helper). The pure
    # helper foreign_luks_uuids never eprintln!s, so doctor's stderr
    # must be free of the per-foreign-UUID warning string pinned at
    # cli/src/membership.rs:612-616. If a future edit silently routes
    # doctor's call back through enrich_from_pool_state, every
    # `braid doctor` run would re-emit this line for every foreign
    # UUID and this assertion would fail. Substring match on the
    # stable prefix only -- the {uuid}/{mapper} interpolation is not
    # load-bearing for the check.
    stderr = machine.succeed("cat /tmp/braid-doctor.err")
    assert "Warning: live LUKS UUID" not in stderr, (
        "doctor regressed into the enrich_from_pool_state warning path; "
        f"stderr was:\n{stderr}"
    )

machine.shutdown()
```

The corresponding `.nix` mirrors
`tests/cli/braid-add-cloned-luks-header-rejected.nix` with three
disks of 1024 MiB and `braid` + `cryptsetup` + `btrfs-progs` in
`environment.systemPackages`. **Important:** use a fresh
`luksFormat` (not `luksHeaderBackup`/`luksHeaderRestore`); a cloned
header would make disk3's UUID equal disk2's, which is the
*duplicate*-UUID scenario (already covered by
`tests/cli/braid-add-cloned-luks-header-rejected.py`), not the
foreign-UUID scenario this check targets.

### Commands

- `just test-rust` -- run the new unit tests.
- `just test-vm braid-doctor-foreign-luks-uuid` -- run the new VM
  test.
- `just test-vm` -- full VM suite (existing doctor tests must still
  pass; the only signature change is `run_doctor` accepting a `&dyn
  Filesystem`).
- `cargo check` -- catch the threading of `Filesystem` through call
  sites.

### Manual smoke

In a dev VM with a mounted braid pool:

```
cryptsetup luksFormat /dev/<spare-block>
cryptsetup open /dev/<spare-block> braid-stranger
btrfs device add -f /dev/mapper/braid-stranger /mnt/storage
braid doctor   # expect Fail + "foreign LUKS UUID ... at mapper braid-stranger"
btrfs device remove /dev/mapper/braid-stranger /mnt/storage
cryptsetup close braid-stranger
braid doctor   # expect Ok
```

## Out of scope

- The TUI surface. The original finding alleges the TUI also redirects
  to `braid doctor` for the foreign-mapper diagnosis, but no string
  in `cli/src/tui/` does so (`grep -rn "foreign mapper\|braid doctor"
  cli/src/tui/` returns nothing). No TUI-side change is needed.
- Status's inline foreign-mapper hint at `cli/src/status.rs:1175`
  remains unchanged; it already points operators at doctor, which now
  honours the redirect.
- Routing the `EnrichmentReport.foreign` return value into a
  per-command surface in `add.rs`/`replace.rs`/`unlock.rs`. Those
  code paths still emit the per-UUID `eprintln!` and the next `braid
  doctor` run picks up the same foreign UUIDs from the live pool, so
  no separate plumbing is owed there. (The stale "Phase 5" comments
  on those discards ARE updated in this plan -- see step 7 -- but
  the discards themselves stay.)
