# Resumable Existing-Pool Add Transaction

## Summary

Redesign existing-pool `braid add` recovery as a phased transaction instead of
a generic "pending add" replay. `braid add` has two logically different parts:

1. Pool membership mutation:
   - prepare/open the target disk;
   - for a returned braid-labeled missing disk, remove the stale btrfs signature
     with narrow `wipefs --all --types btrfs`;
   - run `btrfs device add`, using `-f` only for the returned-disk case;
   - write the updated `pool.json`.
2. Post-mutation maintenance:
   - run or resume RAID1 balance so data and metadata are distributed according
     to the intended profile.

The journal must say which part was interrupted. Recovery must not infer this
from live state alone.

This plan covers Add into an existing pool. First-pool/bootstrap Add keeps its
current recovery escape behavior.

## Journal Model

Change `OpKind::Add` to carry an explicit phase and mode-aware targets:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AddPhase {
    PoolMutation,
    PostAddBalanceRaid1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AddJournalTarget {
    pub by_id: ByIdPath,
    pub mapper_name: String,
    pub mode: AddJournalMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AddJournalMode {
    RecoverableBraidLabeled {
        verified_pool_fsid: String,
        luks_uuid: LuksUuid,
    },
    FreshLuks {
        luks_label: String,
        luks_format_extra_opts: Vec<String>,
        enroll_key_file: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op")]
pub enum OpKind {
    Add {
        phase: AddPhase,
        targets: BTreeMap<String, AddJournalTarget>,
    },
    // existing variants unchanged
}
```

`target_membership` remains the intended final `pool.json`. `pre_membership`
remains the known-good state before the Add mutation.

For fresh disks, store the effective LUKS format options needed for replay:

- `luks_format_extra_opts` is the exact `extra_opts` vector passed to
  `CryptsetupLuksFormat`, including the explicit format args from the original
  `add` invocation and the generated `--label braid-<name>`.
- Recovery must use the stored options. It must not re-read
  command-boundary LUKS format args.
- The passphrase is never stored. Recovery resolves a credential at recovery
  time and verifies it against the existing pool before using it for delayed
  `luksFormat`.

No backward-compatible journal migration is needed because braid is unreleased.

## Phase Invariant

`AddPhase::PoolMutation` means recovery may still perform disk preparation and
pool membership mutation.

This is the only phase allowed to:

- format a fresh target as LUKS when the target is still non-LUKS and the
  journaled `FreshLuks` options provide the replay format contract;
- open, keyfile-enroll, and header-back-up a fresh target;
- handle a returned braid-labeled missing disk;
- run `btrfs device scan --forget`;
- run narrow `wipefs --all --types btrfs`;
- run `btrfs device add`;
- use `btrfs device add -f`, only for `RecoverableBraidLabeled`.

Once every journaled target is a live member of the btrfs filesystem and the
updated `pool.json` has been durably saved, `add` or `recover` must atomically
rewrite `pending-op.json` to:

```rust
AddPhase::PostAddBalanceRaid1
```

`AddPhase::PostAddBalanceRaid1` means the pool membership mutation is
committed. Recovery in this phase must be deliberately narrow:

- do not format LUKS;
- do not run `wipefs`;
- do not run `btrfs device add`;
- do not replay target preparation;
- mount/probe/validate the committed pool;
- resume or run the owed RAID1 balance;
- clear the journal only after the balance obligation is complete.

Main invariant: once the Add journal advances to `PostAddBalanceRaid1`,
recovery must never redo disk preparation or pool membership mutation. It may
only finish post-add maintenance.

There is an unavoidable cross-file crash window after `pool.json` is durably
saved and before the journal phase rewrite is durably saved. If that happens,
recovery still sees `PoolMutation`; it must reconcile live targets, observe that
all journaled targets are already live, re-save `pool.json` if needed, then
advance the journal to `PostAddBalanceRaid1`. It must not wipe or re-add live
targets.

## Add Execution

For existing-pool Add:

1. Validate the command and discover target states.
2. For `RecoverableBraidLabeled`, verify before journaling that the decrypted
   mapper belongs to the mounted pool FSID and is not currently a live member.
3. For `FreshLuks`, compute and store the effective LUKS format options before
   journaling.
4. Exclude already-in-pool no-op disks from `targets`.
5. If there are no Add targets and no fresh setup work, return without writing a
   journal.
6. Write `pending-op.json` with `phase: PoolMutation` before any irreversible
   fresh-disk `luksFormat` or returned-disk `wipefs`.
7. Run the PoolMutation work:
   - `FreshLuks`: format if still non-LUKS, optionally enroll keyfile, back up
     header, open mapper, then add with `force=false`.
   - `RecoverableBraidLabeled`: open mapper if needed, then add with
     `force=true`.
8. Probe the live pool and verify every journaled target is now a live member.
9. Write updated `pool.json` from the live pool and durably save it.
10. Atomically rewrite the Add journal to `phase: PostAddBalanceRaid1`.
11. Run or resume the RAID1 balance obligation.
12. Clear `pending-op.json` after balance completion.

Change existing-pool `pool_add_device` to accept `force: bool`:

- `force=false`: run only `btrfs device add --enqueue <device> <mount>`.
- `force=true`: run `btrfs device scan --forget <device>`, then
  `wipefs --all --types btrfs <device>`, then
  `btrfs device add --enqueue -f <device> <mount>`.

Fresh targets always use `force=false`. Returned braid-labeled recovery targets
use `force=true` only while the journal is in `PoolMutation` and only after
the replay path has re-validated the target against the journaled identity.

Keep the existing `add` sleep-inhibitor boundary: acquire before the journaled
irreversible window and hold it through PoolMutation, phase advance, balance,
and journal clear. Dry-run must not acquire the inhibitor.

## Recovery Behavior

For existing-pool Add journals, mount membership is phase-specific:

- `PoolMutation`: mount from `pre_membership`, not the target union. A target
  may be wiped-but-not-added or not-yet-formatted, so it must not be selected as
  a mount source.
- `PostAddBalanceRaid1`: mount from `target_membership`, because membership is
  committed and recovery is finishing maintenance on the committed pool.

The target union is only an allow-list for live-pool validation during
`PoolMutation`.

If the pool is not already mounted and the Add journal is in `PoolMutation`,
run a pre-mount non-destructive open/scan pass before `mount::plan_open_pool`:

- consider journaled targets outside `pre_membership`;
- if the by-id path is present and LUKS-openable, validate its non-destructive
  identity first, then open the expected mapper:
  - `RecoverableBraidLabeled`: raw LUKS UUID must match the journaled UUID;
  - `FreshLuks`: an existing LUKS header must have the expected label;
- run btrfs scan for opened target mappers, or run the existing scan-all
  command;
- do not format, enroll, back up headers, wipe, or call `btrfs device add`;
- still choose the mount source only from `pre_membership`.

This lets btrfs discover an already-committed but closed Add target before the
initial mount while keeping wiped, raw, or not-yet-added targets out of mount
source selection.

Pre-mount scan output is never commit evidence by itself. It is only a setup
step that may make an already-committed target visible to the later mounted
live-pool probe. Only the mounted live-pool probe may classify a journaled
target as already committed. If the target remains absent from the mounted live
pool after probing, recovery must continue through the target's normal
PoolMutation replay path. This is required for the crash-after-journal-before
wipefs returned-disk case, where the mapper still has a stale same-FSID btrfs
signature but has not been re-added.

Before any PoolMutation replay:

- probe the mounted live pool;
- validate that every live member is in `pre_membership + journaled targets`;
- fail before destructive target preparation, wiping, adding, writing
  `pool.json`, or clearing the journal if an unknown live member is present.
- acquire a sleep inhibitor before any destructive PoolMutation replay command;
  if acquisition fails, fail before mutation and preserve the journal.

In `AddPhase::PoolMutation`, recovery performs an idempotent membership resume:

1. Run a non-destructive reconciliation pass for journaled targets that are not
   in the first live probe:
   - if the by-id path is present and LUKS-openable, open the expected mapper;
   - run btrfs scan;
   - re-probe the live pool;
   - if the target appears, treat it as already committed.
2. For still-absent `RecoverableBraidLabeled` targets:
   - validate the raw LUKS UUID from the journaled by-id path equals the
     journaled `luks_uuid`;
   - open the expected mapper from the journaled by-id path;
   - if a btrfs superblock is still visible, validate its FSID equals the
     journaled `verified_pool_fsid`;
   - if UUID or FSID validation fails, do not run scan-forget, wipefs, or add;
     fail and keep the journal;
   - run `pool_add_device(force=true)`.
3. For still-absent `FreshLuks` targets:
   - if the by-id path is non-LUKS, resolve and verify the pool credential, run
     `luksFormat` using the stored `luks_format_extra_opts`, then continue;
   - if the by-id path is already LUKS with the expected label, do not reformat;
   - if the by-id path is LUKS with an unexpected label or identity, fail and
     keep the journal;
   - ensure requested keyfile enrollment idempotently: test the requested
     keyfile first, skip enrollment if accepted, run `luksAddKey` only if the
     keyfile is rejected, and fail on keyfile-probe errors;
   - back up the LUKS header, open mapper, then run
     `pool_add_device(force=false)`.
4. If a target is physically absent, cannot be opened, has the wrong identity,
   or cannot be added, fail and keep `pending-op.json`.
5. Probe again after any replay and validate every live member against the
   target union.
6. Write recovered `pool.json` from the live pool only after every journaled
   target is live.
7. Rewrite the Add journal to `PostAddBalanceRaid1`.
8. Run the PostAddBalanceRaid1 recovery path.

In `AddPhase::PostAddBalanceRaid1`, recovery must not perform target prep or
pool membership mutation:

1. Mount/probe the committed pool using `target_membership`.
2. Validate live members against `target_membership`.
3. If `pool.json` is missing or stale but the live pool exactly matches the
   committed target membership, write `pool.json` from the live probe; otherwise
   fail and keep the journal.
4. Acquire a sleep inhibitor before starting or resuming the owed RAID1 balance.
   If acquisition fails, do not run balance and keep the journal.
5. Clear `pending-op.json`.

Dry-run output should reflect the phase:

- `PoolMutation`: show reconciliation before any replay commands, then show
  returned-target force add or fresh-target setup/add steps as applicable.
- `PostAddBalanceRaid1`: show only mount/probe/validate, possible `pool.json`
  repair from committed live state, RAID1 balance, and journal clear. Do not
  show any `luksFormat`, `wipefs`, or `btrfs device add` step.

## Tests

### Journal and Command Tests

- Round-trip `OpKind::Add { phase, targets }` for both phases.
- Round-trip both target modes, including fresh `luks_format_extra_opts` and
  `enroll_key_file`.
- Assert fresh journal construction stores the effective original
  `CryptsetupLuksFormat.extra_opts`, including label and explicit format args.
- Assert recover uses stored fresh format options and does not re-snapshot
  command-boundary LUKS format args.
- Assert recover keyfile ensure tests the requested keyfile first, skips
  `luksAddKey` when accepted, enrolls only when rejected, and fails on probe
  errors.
- Assert `CmdRequest::BtrfsDeviceAdd { force: false }` renders without `-f`.
- Assert `CmdRequest::BtrfsDeviceAdd { force: true }` renders with `-f`.
- Assert `WipefsBtrfs` renders exactly
  `wipefs --all --types btrfs <device>`.
- Assert `pool_add_device(force=true)` runs scan-forget, narrow wipefs, then
  force add in order.
- Assert `pool_add_device(force=false)` runs only non-force add.

### Add and Recover Tests

- PoolMutation crash after narrow wipefs: `wipefs` succeeds,
  `btrfs device add -f` fails, recover replays the returned-disk add, writes
  `pool.json`, rewrites the journal to `PostAddBalanceRaid1`, runs balance, and
  clears the journal.
- Returned target committed but mapper closed: recover opens/scans/re-probes,
  does not wipe or add, writes `pool.json`, advances to
  `PostAddBalanceRaid1`, runs balance, and clears the journal.
- Returned target committed but mapper closed and pool offline: pre-mount
  reconciliation opens/scans the target before mount planning, mount source is
  still chosen from `pre_membership`, the first post-mount probe sees the
  target, and recover runs no wipe or add.
- Returned target crash after journal before wipefs, pool offline: the target
  still has a stale same-FSID btrfs signature. Pre-mount open/scan may run, but
  it is not treated as commit evidence. If the mounted live-pool probe does not
  report the target as live, recover runs the returned-target force replay
  instead of writing `pool.json` without it.
- Returned target replay with wrong raw LUKS UUID or visible wrong btrfs FSID:
  recover fails before scan-forget, wipefs, or force add; `pool.json` is
  untouched and the journal remains.
- Fresh target committed but mapper closed: recover opens/scans/re-probes, does
  not format or add, writes `pool.json`, advances phase, runs balance, and
  clears the journal.
- Fresh target committed but mapper closed and pool offline: pre-mount
  reconciliation opens/scans the target before mount planning, mount source is
  still chosen from `pre_membership`, the first post-mount probe sees the
  target, and recover runs no format, enrollment, header backup, or add.
- Fresh target crash before `luksFormat`: recover verifies the pool credential,
  formats using stored options, enrolls keyfile if requested, backs up header,
  opens mapper, adds with `force=false`, writes `pool.json`, advances phase,
  balances, and clears the journal.
- Fresh target crash after `luksFormat` but before add: recover does not
  reformat; it validates expected label/identity, completes setup/add, advances
  phase, balances, and clears the journal.
- Fresh target keyfile already enrolled: recover accepts the requested keyfile,
  skips `luksAddKey`, backs up the header if needed, and continues.
- Missing or wrong fresh target in PoolMutation: recover fails, leaves
  `pool.json` untouched, and preserves the journal.
- Present fresh LUKS target with no usable credential: recover fails before
  writing `pool.json` and preserves the journal.
- Unknown live member in PoolMutation: recover fails before destructive target
  preparation, wipefs, add, `pool.json` write, or journal clear. For an
  already-mounted pool, assert it also happens before any target open. For an
  offline pool, pre-mount non-destructive open/scan may have happened first,
  but no mutation may happen after the unknown member is discovered.
- Crash after btrfs membership and `pool.json` are committed but before balance
  completes: seed `phase: PostAddBalanceRaid1`; recover only mounts/probes,
  validates, resumes/runs RAID1 balance, and clears the journal. Assert it does
  not run `luksFormat`, keyfile enrollment, header backup as target prep,
  add-target reconciliation, `btrfs device scan --forget`, `wipefs`, or
  `btrfs device add`. Normal LUKS open needed to mount `target_membership` is
  allowed.
- Crash after `pool.json` save but before phase rewrite: seed
  `phase: PoolMutation` with all journaled targets already live; recover does
  not wipe or add, rewrites/repairs `pool.json` if needed, advances to
  `PostAddBalanceRaid1`, balances, and clears the journal.
- PostAddBalanceRaid1 with stale or missing `pool.json`: if the live pool
  exactly matches `target_membership`, recover writes `pool.json` and resumes
  balance; if it does not match, recover fails and preserves the journal.
- Sleep inhibitor coverage:
  - PoolMutation replay acquires a sleep inhibitor before `luksFormat`,
    `luksAddKey`, header backup, wipefs, btrfs add, or balance.
  - PoolMutation inhibitor acquisition failure runs no destructive command,
    leaves `pool.json` untouched, and preserves the journal.
  - PostAddBalanceRaid1 acquires a sleep inhibitor before balance.
  - PostAddBalanceRaid1 inhibitor acquisition failure runs no balance and
    preserves the journal.
  - Dry-run acquires no inhibitor.
- Dry-run tests assert phase-specific ordering and absence:
  - PoolMutation dry-run shows reconciliation before replay.
  - PostAddBalanceRaid1 dry-run shows balance only and contains no format,
    wipefs, or add command.

### VM Test

Keep or add one focused VM repro proving the real returned-disk path:

- build a 3-disk RAID1 pool;
- make disk3 missing;
- run `braid remove-missing`;
- return and re-add disk3;
- assert success, all three disks in `pool.json`, no `pending-op.json`, and data
  intact.

The exact crash windows should stay unit-level because they need command-level
fault injection.

## Documentation and Acceptance Criteria

- User docs describe Add recovery as phased: PoolMutation completes membership,
  PostAddBalanceRaid1 completes maintenance.
- Docs explain why returned braid-labeled disks may need narrow btrfs signature
  wiping, and that `-f` is used only for verified returned disks.
- `docs/decisions/012-intent-cli.md` is updated so it no longer says all
  previously removed disks must be wiped before re-add; verified returned disks
  are accepted through the journaled recovery-add path.
- Docs state that once recovery reaches PostAddBalanceRaid1, it will not format,
  wipe, or add disks.
- No broad `wipefs --all` exists in this implementation path.
- The pool-phase carrier (`PoolAddExecutionTarget`, `cli/src/add.rs:257`)
  carries the mapper path and a per-target force flag derived from
  `AddJournalMode` at construction time; nothing depends on `probed_idx` or
  parallel-vector alignment.
- Existing-pool Add recovery uses phase-specific mount membership:
  `pre_membership` during PoolMutation and `target_membership` during
  PostAddBalanceRaid1.
- Offline PoolMutation recovery runs non-destructive pre-mount open/scan for
  present journaled targets outside `pre_membership`, while still selecting the
  mount source from `pre_membership`.
- Returned-disk force replay validates the raw LUKS UUID and any visible btrfs
  FSID against the journal before scan-forget, wipefs, or add.
- Fresh keyfile enrollment is an idempotent ensure operation, not a blind
  `luksAddKey`.
- Recover holds a sleep inhibitor for destructive PoolMutation replay and for
  the post-add balance obligation; inhibitor acquisition failure preserves the
  journal.
- Recovery never clears the Add journal while membership is incomplete or while
  the owed balance remains unfinished.
