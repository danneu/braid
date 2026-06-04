# Plan: cover the `Snapshot::ProbeFailed` lock arm

## Context

`braid lock`'s planner (`cli/src/lock.rs#plan_lock`) has three snapshot arms.
The `Snapshot::ProbeFailed` arm is the most delicate: the pool is mounted but
the per-device `probe_pool` failed, so lock falls back to `probe_fsid` +
the exclusive-op preflight + UUID-scanned mapper cleanup
(`build_close_sets_uuid_scanned_fallback`).

A coverage audit (Low/Testing finding) flagged that this arm has **only
mock-based Rust coverage**. Investigation confirmed the gap and refined it:

- The two adjacent VM tests cover *other* arms. `luks-mapper-drift.py` mounts on
  a drifted mapper that `probe_pool` **succeeds** on (`Snapshot::Probed`);
  `luks-lock-skipped-no-false-closed.py` locks an **unmounted** pool
  (`Snapshot::Unmounted`). Neither reaches `ProbeFailed`.
- The finding's suggested triggers are inaccurate. Null-underlying is handled
  gracefully (`cli/src/probe.rs#probe_pool`, `continue`, no error -> `Probed`);
  `MapperConflict`/`MapperBacking*` are **unreachable** from `probe_pool` (it
  does no ownership validation -- the twin comment in
  `cli/src/monitor.rs` confirms "unreachable ... but listed for the gate"). The
  only reproducible way to make `probe_pool` fail while `probe_fsid` succeeds on
  a mounted pool is a **btrfs device path that is not `/dev/mapper/`-prefixed**
  (`probe_pool` errors `PoolDevice`; `probe_fsid` stops at the FSID and returns
  it). Such a path can be introduced into braid's **own** mounted pool (a raw
  device manually `btrfs device add`-ed), so the trigger stays braid-owned and
  never raises the foreign-mount ownership question that ADR 024 governs.
- A second gap surfaced: **nothing at any layer** pins "ProbeFailed arm + busy
  `exclusive_operation` -> refuse". The existing busy-preflight unit tests
  (`lock_refuses_when_exclusive_op_active`, `lock_refuses_when_balance_paused`)
  all run through the **Probed** arm, because the shared fixture
  `lock_with_fsid_probe_mocks` seeds a *successful* probe by default. The one
  `ProbeFailed` unit test (`mounted_probe_failure_fallback_closes_uuid_verified_member`)
  uses an **idle** FSID.

Intended outcome: pin the `ProbeFailed` arm at the right layers -- a VM test for
the real-tool-output behavior a mock can't prove, and a unit test for the
gate-wiring a VM test shouldn't fragile-ly chase -- while staying strictly
within ADR 024's active policy. The plan also surfaces (for a separate ADR-024
follow-up, **not edited here**) the gap between ADR 024's "the FSID proves braid
owns the mount" invariant and what the lock code implements: `probe_fsid`
extracts the FSID for the exclusive-op preflight without comparing it to any
recorded pool FSID.

## Why this shape (and not the finding's proposal)

The finding asked for a single VM test that also asserts "still honors the
exclusive-op gate". Splitting by the layer that owns each invariant
(AGENTS.md Mutation Safety) is stronger and less fragile:

- **Real-output divergence** (`probe_pool` fails *and* `probe_fsid` succeeds on
  the same real `btrfs filesystem show` output; arm proceeds against real
  `/sys/fs/btrfs/<fsid>/exclusive_operation`) is an integration invariant no
  mock can prove -> **VM test**.
- **Gate-wiring** (the `require_lock_preflight` call at the `ProbeFailed`/User
  arm refuses on a busy FSID) is a logic invariant -> **deterministic unit
  test** with the existing `.with_excl_op(...)` fixture. A VM blocked-preflight
  subtest would add fill+balance+poll-pause fragility and merely re-prove (via a
  second arm) what `braid-exclop-paused-balance.py` already covers through the
  same `require_lock_preflight`.

## Deliverables

### 1. VM test -- `tests/cli/braid-lock-probe-failed.{nix,py}`

Models a **braid-owned mounted pool whose per-device probe is forced to fail**:
a non-`/dev/mapper/` device is present in the pool's own `btrfs filesystem
show`, so `probe_pool` errors `PoolDevice` while `probe_fsid` still returns the
pool's FSID. `braid lock` must fall back to UUID-scanned cleanup rather than
abort. The filesystem it unmounts is braid's **own** pool, so the test stays
consistent with ADR 024 and takes no position on the foreign-mount ownership
question (see Follow-up).

**`.nix`** -- copy `tests/cli/luks-mapper-drift.nix` verbatim except: `name`,
the three-section `# What/Why/Scenario` preamble, `testScript` filename, and
provision **three** disks via `virtualisation.emptyDiskImages` with serials
`disk1`, `disk2`, `spare` (1024 MiB each; `disk1`/`disk2` are LUKS pool members,
`spare` is a raw device manually added to the pool to force the non-mapper
probe failure).

**`.py`** -- `# Intent / Why it exists / Scenario` preamble, then reuse the
established helpers (`shlex.quote` passphrase, the `add_cmd(name)` builder with
the `--pbkdf pbkdf2 --pbkdf-force-iterations 1000 --passphrase-stdin --yes`
fast-format flags, `machine.succeed/fail`, `machine.execute("... 2>&1")`,
`subtest`). Scenario framing: an operator manually ran `btrfs device add`
(bypassing braid), so the mounted pool's device list now contains a
non-`/dev/mapper/` path -- the deterministic stand-in for any non-mapper path a
real pool's `btrfs filesystem show` could report.

Staging:
1. `braid add disk1`, `braid add disk2` -> raid1 pool **stays mounted** at
   `/mnt/storage` on `braid-disk1`/`braid-disk2`; `pool.json` records both
   UUIDs; the mounted FSID is braid's own.
2. `touch /dev/mapper/braid-BOGUS` -- an unverifiable candidate (same trick as
   `luks-lock-skipped-no-false-closed.py`).
3. `btrfs device add -f /dev/disk/by-id/virtio-spare /mnt/storage` -- adds a raw
   (non-mapper) device to braid's own mounted pool. `btrfs filesystem show
   /mnt/storage` now lists `braid-disk1`, `braid-disk2`, and the raw spare path,
   so `probe_pool` errors `PoolDevice` on the spare while `probe_fsid` still
   returns the pool's FSID. (`pool.json` is untouched -- the raw device is not a
   braid member, and the UUID-scanned fallback ignores non-`braid-*` paths.)

Subtest A -- **dry-run preview is side-effect-free** (`braid lock --dry-run`):
- output contains `per-device probe failed (`, `not a /dev/mapper/ path`, and
  `falling back to UUID-scanned mapper cleanup` (the `ProbeFailed` warn, routed
  via `PoolDevice`);
- output does **not** contain `not btrfs` (did not mis-route to the `NotBtrfs`
  abort) nor `cannot probe pool` (proves `probe_fsid` succeeded -- the
  divergence);
- nothing changed: `mountpoint -q /mnt/storage` still succeeds,
  `/dev/mapper/braid-disk1` and `/dev/mapper/braid-BOGUS` still exist.

Subtest B -- **real lock executes the fallback** (`braid lock 2>&1`, exit 0):
- contains `disk disk1: locked` and `disk disk2: locked` (UUID-verified members
  closed) and `skipping mapper braid-BOGUS` (unverified skipped);
- contains **no** `already closed` row (the bogus skip leaves cleanup uncertain
  -- the no-false-closed contract holds in this arm too);
- post-state: `mountpoint -q /mnt/storage` **fails** (braid's own pool
  unmounted), `test -e /dev/mapper/braid-disk1` / `braid-disk2` **fail**
  (closed), `test -e /dev/mapper/braid-BOGUS` **succeeds** (only UUID-verified
  mappers were closed).

Assert on path-independent substrings (the spare's device path inside the warn
may be `/dev/vdX` or a by-id path); `not a /dev/mapper/ path` is the stable
detail.

**Register** in `flake.nix` alongside the other lock tests (after the
`braid-lock-btrfs-held` block ~`flake.nix:556`):
```nix
braid-lock-probe-failed = pkgs.testers.nixosTest (
  import ./tests/cli/braid-lock-probe-failed.nix {
    braid = linuxCrane.braid;
  }
);
```

### 2. Rust unit test -- `cli/src/lock.rs` (tests module)

Add a test next to `lock_refuses_when_exclusive_op_active` that pins the
`ProbeFailed`/User preflight call. It is that test's setup plus the
probe-failure override from
`mounted_probe_failure_fallback_closes_uuid_verified_member` (force the first
`CryptsetupStatus { braid-aaa }` to error so `probe_pool` -> `ProbeFailed`),
with `.with_excl_op("balance")` on the fs. Assert `cmd_lock_impl` returns
`Err` whose message contains `balance` and `in progress`, and that
`umount_request_count(&runner) == 0` (refuses before any umount/close). Preamble:

- *Intent*: the `ProbeFailed` fallback arm runs the exclusive-op preflight and
  refuses on an active balance before unmounting or closing any mapper.
- *Why it exists*: the FSID preflight is the only guard between the fallback's
  unmount and an in-flight exclusive op; a refactor that dropped it from this arm
  (as the `Unmounted` arm legitimately does) would risk unmount during balance.
- *Scenario*: a mounted pool whose per-device probe fails while a balance runs;
  operator runs `braid lock`.

### 3. Doc comments and ADR 024 -- deliberately **not** edited in this plan

This plan makes **no** comment or ADR edits. Four touchpoints state the same
policy -- "the FSID proves braid owns the mount": the `Snapshot` enum comment
("FSID still proved ownership"), the `ProbeFailed` variant comment ("FSID
matched"), the `uuid_scanned_fallback_warn_body` comment (`cli/src/lock.rs`
~line 340, "The FSID preflight still proves braid owns the mount"), and ADR 024
paragraph 7 (`docs/design/decisions/024-luks-uuid-identity.md`). They are
mutually consistent and express an **active** ADR invariant.

The original draft of this plan proposed rewording the `Snapshot` comment to the
**opposite** ("the arm trusts mount-point occupancy, not FSID identity"). That
was wrong: it would unilaterally reverse an active ADR's invariant via a code
comment, with no ADR change or rationale, contradicting the Decision Doc rules
in AGENTS.md. Reconciling the wording requires first deciding the *behavior*, so
it moves wholesale to the Follow-up. The new tests are written so none of them
depend on or assert that wording.

## Follow-up (separate ADR-024 work -- not in this plan)

ADR 024 (Active) paragraph 7 says: "If mounted per-device probing fails, `lock`
first requires the mounted filesystem FSID to prove braid owns the mount." The
lock code does not implement that as written: `probe_fsid`
(`cli/src/probe.rs#probe_fsid`) only extracts the FSID, and
`require_lock_preflight` (`cli/src/preflight.rs#require_lock_preflight`) only
uses it to key `/sys/fs/btrfs/<fsid>/exclusive_operation`. Nothing compares the
probed FSID to a recorded pool FSID (braid does not persist one), so the
practical license to unmount is mount-point occupancy, not FSID identity.
Consequence: a non-braid btrfs mounted at `/mnt/storage` would be unmounted by
`lock` (only verified `braid-*` mappers are ever closed, so no data is exposed,
but it is still an unmount of someone else's filesystem).

The follow-up must pick one and update **all four** touchpoints together (the
two `Snapshot` comments, the `uuid_scanned_fallback_warn_body` comment, and ADR
024 paragraph 7):
- (a) **Reword to match the code:** the FSID is read to key the exclusive-op
  preflight; braid acts on mount-point ownership, not FSID identity. Document
  the foreign-mount unmount as accepted behavior, with its security rationale.
- (b) **Strengthen the code to match the ADR:** persist the pool's FSID and have
  `lock` verify the probed FSID against it before unmounting, refusing on a
  mismatch.

Either way it is a behavior/policy decision carrying an ADR change plus
rationale, deliberately kept out of this test-coverage plan. The VM test above
sidesteps the question entirely by only ever unmounting braid's own pool.

## Reused assets

- Test scaffold: `tests/cli/luks-mapper-drift.nix` (3-section preamble, disk
  provisioning), `tests/cli/luks-lock-skipped-no-false-closed.py` (the
  `touch /dev/mapper/braid-BOGUS` + skip-assertion pattern).
- Unit fixtures: `lock_with_fsid_probe_mocks`, `.with_excl_op(...)`,
  `.with_output_sequence(...)`, `umount_request_count`,
  `mounted_probe_failure_fallback_closes_uuid_verified_member` (probe-failure
  override), `lock_refuses_when_exclusive_op_active` (busy-preflight assertion).
- Code under test: `cli/src/lock.rs#plan_lock` (`ProbeFailed` arm),
  `cli/src/lock.rs#build_close_sets_uuid_scanned_fallback`,
  `cli/src/probe.rs#probe_pool` / `#probe_fsid`,
  `cli/src/preflight.rs#require_lock_preflight`.

## Verification

1. `just test-rust` -- new unit test passes. Sanity-check it fails closed: with
   the line-912 `require_lock_preflight` call temporarily removed, the new unit
   test must fail (and the VM test stays green, confirming the split). Restore.
2. `just test-vm braid-lock-probe-failed` -- new VM test passes. If the warn
   assertions miss, inspect the merged `braid lock 2>&1` output and adjust to the
   actual rendered device path while keeping `not a /dev/mapper/ path`.
3. Change is purely additive (one new VM test, one new unit test; no source or
   ADR comment edits) -- no full-suite run required. Optionally re-run the
   adjacent lock tests
   (`just test-vm luks-mapper-drift luks-lock-skipped-no-false-closed`) to
   confirm no fixture/registration collision.
