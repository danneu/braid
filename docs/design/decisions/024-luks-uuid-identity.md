---
intent: "Record the LUKS UUID Is Disk Identity decision and rationale. Read before changing related behavior or docs."
status: Active
---
> Active -- Refines [017-runtime-disk-membership.md](017-runtime-disk-membership.md).

# Decision: LUKS UUID Is Disk Identity


> Principle: [Stable identifiers](../principles.md#5-stable-identifiers)

## Context

Runtime membership originally used the operator disk name as the key in
`pool.json`. The same name also appears in mapper names and LUKS labels, so
code could accidentally treat display/runtime handles as identity. That made
label drift, mapper drift, and cloned disks hard to reason about: a member
could be the same encrypted device while its label or mapper path changed, or
two different by-id paths could expose the same cloned LUKS header.

## Decision

Use the LUKS UUID as the persistent disk identity. `pool.json` and
`pending-op.json` membership snapshots are keyed by canonical LUKS UUIDs.
`DiskMember.name` remains the operator-facing name and `DiskMember.by_id`
remains the hardware address used to reach the device. `DiskMember.devid` is
persisted only as prior-binding state for btrfs `missing_devids`, where btrfs
reports a member by devid with no device at all.

An open mapper's LUKS UUID comes from the kernel, not from its disk.
cryptsetup sets the device-mapper UUID `CRYPT-LUKS2-<uuid hex>-<name>` (or
`CRYPT-LUKS1-...`) when it activates a mapping, and the kernel keeps it for as
long as the mapping exists, with or without its backing disk. braid reads it
from `/sys/block/dm-*/dm/uuid` through `cli/src/dm_identity.rs#mapper_luks_uuid`
for every "which LUKS container is this open mapper" question: `lock`'s
classification, `probe_pool` (pool devices with and without a backing disk),
the post-commit close re-probe, and the UUID step of mapper reuse. So a mapper
whose disk died or was unplugged (`null_underlying`) binds by UUID like any
other. `cryptsetup luksUUID` stays for questions about a disk itself: by-id
probes, format and recovery checks, and the backing-path check in rule 8 below.

Fresh `add` and `replace` operations pre-generate the UUID that cryptsetup must
write, store that UUID in the journal before mutation, and pass it through the
structured `CryptsetupLuksFormat` request. User-supplied `--luks-format-arg`
values may not override `--uuid`, `--label`, or the target device, and may not
end option parsing with `--`. cryptsetup formats the first word after
`luksFormat` that is not an option, so `cli/src/cmd.rs#luks_format_argv` puts
the by-id there and the user extras last. No extra can then become the device
or swallow a braid token. `cli/src/luks.rs#check_format_args`, which every
format passes through, refuses `--` again before cryptsetup sees the argv, so
extras that serde built without `cli/src/types.rs#LuksFormatExtraOpts::parse`
cannot reach a format with `--` either.

## Identity Boundaries

| Identifier | Role | Persistent identity? | Normal user vocabulary? |
| --- | --- | --- | --- |
| `LuksUuid` | Encrypted-volume identity used for membership correlation, journals, duplicate detection, and live probe checks. | Yes | No |
| `DiskName` | Operator-facing name used in commands, status summaries, mapper suffixes, and labels. | No | Yes |
| `ByIdPath` | Hardware address used to find, open, or format a disk before it is mapped. | No | Setup and repair only |
| `DiskMember.devid` | Prior btrfs binding used when btrfs reports a missing device by devid alone (`missing_devids`). | Fallback binding only | Repair diagnostics only |
| `braid-<DiskName>` mapper name | Runtime handle passed to cryptsetup, btrfs, mount, and close operations. | No | Mostly hidden |
| `braid-<DiskName>` LUKS label | Human/debug label for LUKS headers and discovery bootstrapping. | No | Mostly hidden |

This means UUID identity does not move normal command vocabulary from names to
UUIDs. Operators still add, replace, remove, and read disks by names such as
`toshiba1`. UUIDs belong in `pool.json`, journals, machine-readable status, and
diagnostics where braid must prove that the encrypted member is the expected
one.

## Benefits

- **Single source of truth.** `pool.json` has one persistent member identity:
  the LUKS UUID map key. Disk name, by-id path, and btrfs devid no longer
  duplicate or compete with a value-side `luks_uuid` field.
- **Drift-tolerant member correlation.** Commands resolve membership by UUID
  instead of reconstructing identity from `braid-<name>`. A member opened under
  a drifted mapper can still be recognized as the same disk, and cleanup paths
  close the observed mapper rather than the expected one.
- **Safer recovery replay.** Journals carry UUID-keyed pre-operation and target
  membership snapshots. Recovery can compare the live pool against the
  journaled member set by UUID/devid and re-check live UUIDs before replaying
  format, add, replace, resize, or close steps.
- **Earlier clone and swap detection.** Duplicate LUKS UUIDs are rejected before
  membership writes or destructive operations, and UUID mismatches catch disks
  that were swapped, cloned, or reformatted after the original plan was made.
  `add` and `replace` also re-probe the mounted pool at execution time before
  writing the journal, so confirmation/passphrase-window races still hit the
  UUID guard.
- **Human-facing names stay human-facing.** Operators still type and read disk
  names such as `toshiba1`; mapper names and labels remain `braid-<DiskName>`.
  UUIDs appear where they help diagnostics or machine-readable state, not as the
  normal command vocabulary.
- **Present-device probes use live paths.** Queries such as lsblk model/serial
  and smartctl use the live backing path (`PoolState::underlying_for_uuid`), and
  the TUI disk-detail LUKS metadata dump (`cryptsetup luksDump`) reads the live
  backing path for a verified-present (`Unlocked`) member -- not persisted by-id
  setup/repair handles that can drift while the disk is still present. Metadata
  for locked or ownership-unverified mappers stays on the by-id handle.

## Concrete Improvements

- **Membership shape is simpler.** Membership has one identity axis: UUID keys
  map to name/by-id/devid metadata.
- **Formatting is crash-recoverable.** Fresh `add` and `replace` paths generate
  the UUID before mutation, journal it, and pass it to cryptsetup. Recovery can
  tell whether it is seeing the exact LUKS container that the interrupted plan
  intended to create, so it finishes the plan for a disk the format wrote and
  never formats one itself.
- **Cleanup follows observed ownership.** `lock` classifies live mappers by
  the LUKS UUID their dm UUID records and closes the mapper it actually
  observed, including one whose disk is gone. A mapper opened as
  `braid-WRONG` but owned by `disk1` is closed as `braid-WRONG`; braid does not
  merely try `braid-disk1` and leave the real mapper open.
- **Recovery compares member sets by identity.** Pending operations carry
  UUID-keyed pre-operation and target membership snapshots, so recovery can
  compare live topology with the journaled member set instead of re-discovering
  by label or assuming names still line up.
- **Display code has an explicit label-provenance rule.** Every user-facing
  disk row is labeled with an operator `DiskName`, never a mapper basename.
  That `DiskName` is sourced by one of two attested routes. The
  live-UUID->`DiskName` join resolves a live pool device's UUID back to its name
  for presentation, while UUIDs stay available to verbose/machine-readable paths
  as evidence. This join covers the read surfaces: the TUI Data-tab Bus column
  (its lsblk transport bridge joins the parent disk's LUKS UUID to the member
  name) and the passphrase credential-verification display for `add`,
  `replace`, and their recovery replays. The second route is an
  already-validated typed `DiskName` carried through the operation: the
  post-`btrfs`-commit best-effort close that `remove`, `replace`, and `recover`
  run keeps its close target on the observed mapper (`braid-WRONG`, per
  "Cleanup follows observed ownership" above) but labels its
  `disk <name>: locking...`/`locked` progress row from the journaled operator
  name carried as `close_mapper_best_effort`'s typed `disk_label`. `add`'s
  pre-commit rollback cleanup uses the same route: `LuksCleanupGuard` carries
  a typed `DiskName` with each tracked mapper and labels its
  `disk <name>: ... (cleanup)` rows from that value while closing the tracked
  mapper. `mount`'s fail-closed unlock cleanup
  (`cli/src/mount.rs#close_opened_mappers`) also carries a typed `DiskName`
  next to each mapper opened by the command, labels its
  `disk <name>: locking...`/`locked` cleanup rows from that value, and closes
  the observed mapper rather than reconstructing `mapper_name(&name)`.
  `recover`'s remount cycle (`cli/src/recover.rs#relock_and_remount`, step 3)
  closes its planned `&[DiskName]` set through the same shared
  `cli/src/mapper_close.rs#emit_close_progress` core, so its
  `disk <name>: locking...`/`locked` rows stay typed and it inherits the
  busy-retry close every other close path already had.
  `CredentialVerifyTarget`'s two constructors -- a UUID-joined
  `existing_pool_member` and an attested `named_candidate` -- are this same
  split in type form. Either route lets member names survive mapper drift: a
  member open as `braid-WRONG` still shows its operator name in the
  `passphrase: checking against ...` line, the
  `... does not match existing pool member '...'` rejection, and the close
  progress row, instead of blanking to `--` or echoing the drifted mapper
  basename. The remaining basename-derived strings are deliberate carve-outs:
  the low-level `cryptsetup close <mapper>` busy-retry diagnostic still names
  the real mapper because it is a command echo, not a disk-status row, and
  `cli/src/config.rs#name_from_mapper` / `cli/src/config.rs#braid_disk_name`
  remain the documented display-only mapper parsers.

## Runtime Handles And Labels

1. Mapper names remain `braid-<DiskName>`.
2. LUKS labels remain `braid-<DiskName>`.
3. Both mapper names and labels are presentation/runtime handles, not identity.
4. `LuksUuid` is the only persistent identity for membership decisions.
5. Code may construct `mapper_name(&member.name)` when opening or addressing
   braid's expected mapper.
6. Code must not parse mapper names or LUKS labels to decide membership, target
   a member, or correlate live pool state. Narrow exceptions are allowed for
   bootstrapping and sanity checks only: `discover` bootstraps from cold
   braid-labeled disks; returning-disk adoption in `add` may gate on label match
   after identity correlation still uses `LuksUuid`/`devid`/FSID; fresh add and
   replace recovery may require the expected label before treating an
   already-formatted target as the crash-created LUKS container, but still
   requires the journaled UUID to match. `lock` may use the `braid-*` prefix
   only to discover cleanup candidates; member identity still requires the
   LUKS UUID the candidate's dm UUID records, and candidates without one are
   warned and skipped.
7. `lock` is the special cleanup case: classify every live mapper by the LUKS
   UUID its dm UUID records, then close the observed mapper name, not a
   reconstructed `mapper_name(&member.name)`, so drifted-but-member-owned
   mappers are closed correctly. A mapper whose disk is gone classifies the
   same way, as a member or an orphan; no devid lookup is involved. If mounted
   per-device probing fails, `lock` reads the mounted filesystem FSID to key
   the exclusive-operation preflight (so it will not unmount mid
   balance/replace), then scans `/dev/mapper/braid-*` candidates and closes
   only those whose dm UUID records a LUKS UUID. The unmount is licensed by
   mount-point ownership, not an FSID identity match (see Limits And
   Non-Goals). `lock` reports
   `disk <name>: already closed` only for members the planner has proved absent
   from every observed live state; it must not reconstruct
   `mapper_name(&member.name)` during execute to infer absence. If a mapper is
   skipped because classification failed, or `/dev/mapper` cannot be
   enumerated in either close-set arm, cleanup is uncertain and lock suppresses
   all already-closed claims for unobserved members.
8. Commands that reuse an already-open expected mapper for a requested by-id
   path must verify the mapper's canonical backing path before trusting the
   mapper's LUKS UUID. A cloned LUKS header can give two physical devices the
   same UUID, so the runtime proof is backing path match first, then UUID
   match.
9. Recovery binds the live pool to a journaled membership in one pass,
   `cli/src/membership.rs#bind_live_pool`, for every journal kind and phase.
   A live btrfs device binds by the LUKS UUID its mapper's dm UUID records,
   whether or not the mapper still has a backing disk. Recovery fails closed
   when that UUID cannot be read or names no journaled member. A btrfs
   `missing_devids` entry has no mapper, so it binds to the one member that
   records that devid and is not bound by UUID. A member bound by UUID claims
   no missing devid: its live devid replaces the recorded one. A missing devid
   no such member records is carried: it stays out of `pool.json`, as live
   `add` leaves it. Recovery fails closed when a carried devid sits beside a
   member that is unbound and records no devid, since either could be the
   other. It also fails closed when one UUID or devid shows up twice live, or
   two unbound members record the same missing devid. Every member btrfs
   still holds stays in `pool.json`, at the devid btrfs holds it at. Recovery
   must not infer identity from `braid-<DiskName>`.
10. `replace` must re-probe the mounted pool after confirmation and passphrase
    verification but before sleep inhibitor acquisition, journal write, or
    `btrfs replace start`. If the pool is no longer mounted, the FSID differs
    from the planned pool, or any live pool device has the replacement target's
    LUKS UUID, replace fails closed with the canonical pre-journal validation
    or `DuplicateUuid { scope: LivePool }` refusal.
11. A replace's old and new LUKS UUIDs differ. An equal pair means the new
    disk is the old disk itself under another by-id path, or a clone of it.
    Recover tells a committed replace from an uncommitted one by which of
    the two UUIDs is live, so an equal pair would strand `pending-op.json`.
    `cli/src/journal.rs#ReplaceIdentity` is the only way to pair them:
    `plan_replace` refuses an equal pair before `--dry-run` output or the
    confirmation prompt, and `load_journal` rejects a Replace journal that
    records one.

## Offline Disk State

A recorded member whose by-id path is present, whose LUKS header is readable,
and whose on-disk LUKS UUID matches the `pool.json` membership key is identity
verified. If that member is not assembled into the live btrfs pool, status and
TUI surfaces render it as `offline`, distinct from `missing` (device absent) and
`unknown` (braid cannot classify the state).

`offline` is deliberately cause-neutral. It can describe a locked member in a
degraded mount, an interrupted post-commit mutation, or another state where
membership and live btrfs topology have not yet been reconciled. Because those
causes have different remedies, `braid status` does not print an `Action:` hint
for offline rows.

`braid doctor`'s `declared_disks` check also surfaces an offline member as a
cause-neutral `Warn`, never `Fail`; `Fail` stays reserved for a live LUKS UUID
mismatch. When the pool is mounted but live topology cannot be probed, doctor
warns rather than claiming every declared member is assembled.

## Whole-Disk Targets

A `by_id` path addresses hardware, but it cannot tell two targets apart. One
disk has several by-id links (ata-, wwn-, scsi-, ...), and each of its
partitions has links of its own. So before any probe, `add` (every spec) and
`replace` (the new spec) run `cli/src/target_disk.rs#check_whole_disk_targets`.
It resolves each link to its kernel device and refuses two specs that reach one
device. Then it refuses any device that sysfs does not show as a whole disk:
`/sys/class/block/<name>` must exist and have no `partition` file. Once every
target is a whole disk, two targets overlap only by being the same device. A
stacked device (dm, md) on another target's disk needs no extra rule: the stack
holds that disk, so the fresh-target claim check refuses it.

Two specs on one disk would pass every per-disk check. After the journal, the
first format holds the disk, and cryptsetup refuses the second as in use. Then
`recover` finds the second link carrying the first target's label and fails on
every run, so the operator cannot leave recovery mode. A partition member is
stuck the same way, because `cli/src/recover.rs#resolve_by_id_for_underlying`
skips `-partN` links and fails when no other link resolves to the device. udev
gives every whole disk a link without that suffix, so recover can rebuild every
member that add and replace admit.

The kernel decides, not the link's `-partN` suffix, which is a udev naming
habit braid cannot vouch for. A sysfs read that gives no answer refuses,
because a wrong answer strands a journal. A link that does not exist skips the
sysfs proof, so the probe's "not present" refusal still owns an absent disk.

Rejected:

- **Compare raw by-id strings.** Misses two links to one disk.
- **Normalize link names, such as stripping `ata-`/`wwn-`.** The links carry
  different firmware fields (model plus serial, WWN, SCSI VPD), so no string
  rule can join them. Only the kernel's symlink resolution can.
- **Keep partition targets, and refuse only a partition whose disk is another
  target.** Closes the in-command overlaps, but leaves partition members
  unrecoverable.
- **Hold each fresh target's exclusive claim from the check until its format.**
  The kernel would answer every overlap, but the proof would own a live
  resource every path must release before luksFormat, and an in-use refusal
  could not name the sibling spec.

Limits: a device whose sysfs name differs from its `/dev` name (a cciss-style
disk, where sysfs spells `/` as `!`) is refused, loudly and before the
journal. Two kernel paths to one dual-ported disk without dm-multipath are two
devices to the kernel, so this check cannot join them.

## Limits And Non-Goals

- A LUKS UUID identifies an encrypted LUKS container, not a physical drive,
  enclosure slot, SATA port, or by-id path.
- A cloned LUKS header intentionally has the same UUID as its source. Braid
  treats that as a duplicate identity and rejects it; it does not invent a new
  member identity for the clone.
- Mapper and label drift are tolerated for correlation and cleanup, but braid
  does not silently rewrite drifted mapper names or labels back into the
  expected `braid-<DiskName>` form.
- `devid` remains btrfs state. It is allowed only as a prior binding for
  btrfs `missing_devids`, where btrfs reports a member with no device at all.
  A devid an open mapper holds belongs to that mapper's LUKS UUID, so a
  persisted devid binds only the missing devids no open mapper holds
  (`cli/src/types.rs#PoolState::devid_only_missing`).
- A member with neither an observable LUKS UUID nor a journaled/persisted
  devid is not recoverable by mapper-name inference. The right behavior is to
  preserve recovery state and require manual reconciliation.
- An open mapper's identity is only as trustworthy as its dm UUID. braid
  trusts the value cryptsetup set at activation, the same way it trusts
  cryptsetup itself; only root can create or name dm devices. A mapper whose
  dm UUID is not `CRYPT-LUKS1-`/`CRYPT-LUKS2-` plus a UUID (plain dm-crypt,
  LUKS2 OPAL, a stale `/dev/mapper` node with no dm device, an unreadable sysfs
  table) is unverifiable: `lock` skips it, and every other caller fails
  closed.
- UUIDs are not a user-facing naming scheme. They may appear in diagnostics,
  `pool.json`, `pending-op.json`, and machine-readable output, but command
  selection and normal summaries should continue to use `DiskName`.
- `lock`'s mounted-fallback teardown unmounts the configured btrfs mount point
  (licensed by mount-point ownership, not an FSID identity match -- braid
  persists no durable pool FSID to compare a probe against), then scans only
  `/dev/mapper/braid-*` and closes by the LUKS UUID each candidate's dm UUID
  records: verified member UUIDs
  close as members, verified non-member `braid-*` mappers close as orphans;
  non-`braid-*` devices and unverified candidates are skipped. The cleanup is
  scoped by the `braid-*` namespace plus UUID, not by which devices backed the
  unmounted filesystem. Consequence: a foreign btrfs at braid's mount point
  would be unmounted (a non-destructive, EBUSY-safe `umount` with no `-f`/`-l`);
  a foreign filesystem normally sits on non-`braid-*` devices, so the realistic
  consequence is the unmount alone. This is accepted, and gating it would
  require a durable pool-FSID identity axis this decision deliberately omits to
  keep membership single-axis.

## Tests That Enforce This

- `cli/src/membership.rs` unit tests pin UUID-keyed `pool.json`, reject stale
  value-side `luks_uuid`, and enforce duplicate checks across UUID, name,
  by-id, and devid axes.
- `cli/src/types.rs` and `cli/src/cmd.rs` unit tests reject user-supplied
  `--uuid`/`--label` and `--` extras and pin the structured
  `cryptsetup luksFormat <by-id> ... --uuid <uuid> --label <label> ... <extras>`
  argv order, including a stray extra that stays behind the by-id.
  `cli/src/luks.rs` unit tests pin that `check_format_args`, the gate every
  format passes through, refuses serde-built extras holding `--` before it
  runs cryptsetup.
- `cli/src/status.rs` unit tests pin compact status names by resolving live
  pool UUIDs back to `DiskName`, including a drifted mapper case.
- `cli/src/status.rs` and `cli/src/tui/probe.rs` unit tests pin that a present,
  LUKS-identity-verified member absent from the live pool is classified offline
  (`UnpooledDiskRender::Offline` in the TUI), not `missing` or `unknown`.
  `cli/src/tui/view/mod.rs#unpooled_disk_status_cell_renders_each_variant` pins
  that the TUI renders that classification as the literal yellow `offline` cell
  rather than collapsing to the `missing` fallback.
- `cli/src/doctor.rs` unit tests pin that `declared_disks` renders verified
  members absent from the live pool as cause-neutral `Warn`, keeps UUID
  mismatches as `Fail`, preserves offline-pool identity-only behavior, and warns
  when mounted-pool topology cannot be probed. They also pin that `declared_disks`
  issues its LUKS-identity probe against the persisted by-id handle -- not the
  live backing path -- even for an assembled member in a mounted pool, so a disk
  swapped or reformatted at the stable hardware handle is detected (`Fail`) while
  the live device still carries the matching UUID. This is the deliberate
  counterpart to the live-backing-path SMART and TUI bullets below: doctor's swap
  detection must read the stable handle the live path would mask.
- `tests/cli/braid-status-rust.py` pins that present disks' rendered
  `luks_uuid` equals the real cryptsetup UUID and the `pool.json` membership
  key, and that `name` is the operator name, in intact and degraded states.
- `tests/cli/braid-status-rust.py` pins that a degraded mount with one closed
  verified member renders that member as `OFFLINE` in human output and
  `offline` in JSON while the pool summary remains degraded.
- `tests/cli/braid-doctor-offline-member.py` pins that a degraded mounted pool
  with one closed verified member makes `declared_disks` warn with offline
  wording, while a fully assembled pool and an offline pool remain Ok.
- `tests/cli/status-mapper-drift.py` pins that `braid status` resolves the
  operator name via the UUID join when a member is open under a drifted mapper
  (`braid-WRONG`), not the mapper basename, in both JSON and human output.
- `cli/src/membership.rs`, `cli/src/add.rs`, `cli/src/replace.rs`, and
  `cli/src/recover.rs` unit tests pin that the passphrase credential-verify
  display resolves each existing pool member's name through the UUID join: a
  member open under a drifted mapper (`braid-WRONG`) is named by its operator
  name (`disk1`) in the verify prelude and the `... does not match existing pool
  member '...'` / `... rejected by '...'` rejection, not the mapper basename.
- `CredentialVerifyTarget`'s fields are private; its only constructors are
  `existing_pool_member` (display name UUID-joined through membership) and
  `named_candidate` (an operator-attested, already-validated `DiskName`), so
  no call site can put a mapper-derived name into a credential-verify target.
  `cli/src/credential_verify.rs` unit tests pin the drifted-mapper join and
  the foreign-UUID full-basename fallback at the constructor.
- `cli/src/replace.rs`, `cli/src/recover.rs`, and `cli/src/remove.rs` unit
  tests pin that the post-commit best-effort close trailer closes the observed
  drifted mapper (`braid-WRONG`) while its operator-facing
  `disk <name>: locking...`/`locked` row -- and `remove`'s
  `pool: removing <name>...` row -- names the member by its journaled operator
  name (`disk2`), not the mapper basename. `close_mapper_best_effort`'s
  `disk_label` parameter is typed `DiskName`, so no caller can pass a
  mapper-derived label. `cli/src/add.rs#guard_cleanup_row_uses_disk_name_under_mapper_drift`
  pins the same label provenance for `add`'s rollback cleanup: the cleanup row
  names `disk2` while closing the tracked `braid-WRONG` mapper.
  `cli/src/mount.rs#cleanup_row_uses_disk_name_under_mapper_drift` pins the
  same label provenance for `mount`'s fail-closed cleanup: the cleanup rows
  name `disk1` while scoped forget and close target the observed
  `braid-WRONG` mapper. Each drives the exit-0 close path; the deliberate
  carve-out -- that the busy-retry diagnostic still echoes the raw mapper
  (`cryptsetup close <mapper> busy, retrying...`) because it is a command echo,
  not a disk-status row -- is pinned by
  `cli/src/add.rs#guard_retries_busy_close_before_success`.
  `cli/src/recover.rs#recover_remount_cycle_retries_busy_step3_close` pins that
  `recover`'s remount-cycle step-3 close inherits the same busy-retry: a
  transient EBUSY is retried and the cycle completes, rather than hard-aborting
  the recovery on the first busy close.
- `cli/src/tui/probe.rs` unit tests pin the TUI Data-tab Bus column's transport
  join to the parent disk's LUKS UUID, so a member open under a drifted mapper
  (`braid-WRONG`) still renders its bus instead of degrading to `--`.
- `cli/src/tui/probe.rs` unit tests pin that the disk-detail LUKS metadata dump
  reads the live backing path for a verified-present member (surviving by-id
  drift), and that a foreign / ownership-unverified mapper does not surface the
  live device's metadata under the declared disk.
- `cli/src/tui/app.rs` unit tests pin that session temperature watermarks
  accumulate by LUKS UUID: a member's watermark keeps widening across a
  disk-name change between probes, and a reused display name carrying a
  different LUKS UUID (`DiskName` is non-identity and reusable) starts a
  separate watermark instead of merging two disks' thermal history.
  `cli/src/tui/probe.rs` pins the producer side -- the reading's `id` is the
  member's live LUKS UUID.
- `cli/src/tui/probe.rs` and `cli/src/tui/browse/state.rs` unit tests pin that
  the TUI Browse SMART picker resolves a verified-present member through its live
  backing path (`PoolState.disk_underlying`, shared with the Data-tab SMART
  loop) and an offline member through its persisted by-id handle, so the two
  SMART surfaces cannot disagree under by-id drift. `tests/cli/braid-tui-browse.py`
  pins the live `/dev/vd*` node end-to-end for a present, unlocked member.
- `cli/src/lock.rs` unit tests pin the normal UUID-classified close set,
  observed-mapper closing, UUID-scanned fallback cleanup, orphan warnings for
  non-member UUIDs, and skip warnings for candidates with no LUKS dm UUID.
  `cli/src/lock.rs#uuid_scanned_fallback_closes_null_backed_member_by_dm_uuid`
  pins that a member whose disk is gone still closes as that member.
- `cli/src/parse/dm_uuid.rs` and `cli/src/dm_identity.rs` unit tests pin which
  dm UUIDs name a LUKS container and that an unreadable sysfs table is an
  error, not absence. `cli/tests/golden_nixos_26_05.rs#golden_dm_uuid_luks2_matches_luks_uuid`
  pins that a real captured dm UUID names the same UUID `cryptsetup luksUUID`
  reads from the disk. `cli/src/probe.rs#probe_pool_unprovable_dm_identity_fails_closed`
  pins that a pool device without a provable dm UUID fails the probe.
- `cli/src/remove.rs` unit tests pin all live member devids into the
  pre-operation journal snapshot before mutation, so recovery has a legitimate
  fallback binding for a member btrfs later reports missing.
- `cli/src/recover.rs` unit tests verify `remove` recovery binds a
  null-underlying target by its dm UUID with no persisted devid and records
  the devid btrfs holds it at, and refuses a null-underlying device whose dm
  UUID names a LUKS UUID outside the pre-operation membership, instead of
  falling back to mapper-name inference.
- `cli/src/membership.rs` unit tests pin rule 9's one binding pass: present,
  null-backed, and MISSING members bind; a member bound by UUID claims no
  MISSING devid; a stray MISSING devid is carried; and a carried devid beside
  a member with no recorded devid refuses. `cli/src/recover.rs` unit tests
  named `add_recovery_keeps_absent_member_*` pin that add recovery keeps a
  member btrfs reports MISSING or null-backed, before and after the phase
  advance, and
  `cli/src/recover.rs#bootstrap_recovery_refuses_member_seen_only_as_missing_devid`
  pins the ambiguity refusal.
  `cli/src/recover.rs#add_recovery_refuses_missing_new_disk_before_phase_advance`
  pins that a MISSING devid never binds an add target the journal records no
  devid for.
- `cli/src/status.rs`, `cli/src/replace.rs`, and `cli/src/remove_missing.rs`
  unit tests pin that a hot-unplugged member whose mapper is still open binds
  its live devid by UUID when pool.json's devid is absent or stale: the
  compact `Drives:` row shows it, replace resolves or refuses on it instead of
  rebuilding over another disk's devid, and remove-missing drops the member
  that UUID names. They also pin that a persisted devid never claims a devid
  an open mapper holds.
- `cli/src/enroll_key_file.rs` unit tests verify standalone enroll rejects a
  member whose live LUKS UUID does not match the pool.json membership key
  before any slot inventory or keyfile mutation runs. Enroll also re-probes
  each member's live UUID again at its mutation boundary, after the passphrase
  prompt and before `luksAddKey`, to catch a disk swapped or reformatted during
  the prompt window: unit tests pin the standalone re-probe's mismatch and
  fail-closed arms and the discovery->execute window closure (a swap that passes
  discovery is rejected at execute before any keyfile is enrolled or generated).
- `cli/src/replace.rs` unit tests verify `ReplacePlan::execute` re-probes the
  live pool before journal write, rejects unmounted/FSID-drifted/colliding
  live-pool state, and still proceeds when the fresh probe is clean.
- `cli/src/replace.rs#tests::cmd_replace_refuses_new_disk_with_old_luks_uuid_on_missing_path`
  pins that a `--new` disk carrying the missing `--old`'s LUKS UUID is refused
  before any journal write, sleep inhibitor, LUKS open, or `btrfs replace
  start`. `cli/src/journal.rs#tests::replace_equal_old_and_new_uuid_fails`
  pins that `load_journal` rejects a Replace journal with an equal pair.
- `cli/src/target_disk.rs` unit tests pin the whole-disk target check: one link
  named twice refuses before any sysfs read, a partition beside its whole disk
  refuses, a device with no sysfs entry or an unreadable `partition` file or a
  link that cannot resolve refuses as unproven, and an absent link skips the
  sysfs proof so the probe still owns "not present".
  `cli/src/add.rs#tests::cmd_add_refuses_two_links_to_one_disk_before_journal`
  pins that two different links to one disk refuse with no `pending-op.json`,
  no sleep inhibitor, and no `luksFormat`.
  `cli/src/add.rs#tests::dry_run_add_refuses_partition_target` and
  `cli/src/replace.rs#tests::dry_run_replace_refuses_partition_target` pin that
  a partition target whose link has no `-partN` suffix refuses in `--dry-run`,
  before any probe.
- `cli/src/recover.rs` unit tests verify post-maintenance replace recovery
  re-probes the old mapper UUID before close, skips foreign mappers, and still
  closes owned active dm mappings without relying on `/dev/mapper` path nodes.
- `cli/src/luks.rs` and `cli/src/probe.rs` unit tests verify already-open
  expected mappers must have the requested backing path before UUID ownership
  is accepted.
- `tests/cli/luks-mapper-drift.py` verifies `braid lock` closes the observed
  drifted mapper owned by a member UUID.
- `tests/cli/braid-lock-dead-member.py` verifies `braid lock` closes a member's
  mapper by its dm UUID after the member's SCSI disk is deleted out from under
  it.
- `tests/cli/luks-lock-skipped-no-false-closed.py` verifies skipped mapper
  uncertainty does not produce false `already closed` rows.
- `tests/cli/unlock-uuid-mismatch.py`,
  `tests/cli/enroll-uuid-mismatch.py`, and
  `tests/cli/recover-replace-existing-luks-uuid-mismatch.py` verify swapped or
  reformatted disks fail UUID re-checks before unsafe replay, slot enrollment,
  or mount.
- `tests/cli/replace-new-in-pool-guard.py` verifies duplicate LUKS UUIDs are
  rejected before braid writes membership or calls into btrfs mutation.
- `tests/cli/replace-live-pool-collision-race-rejected.py` verifies replace's
  execute-time live-pool re-probe rejects a cloned replacement UUID added to
  the mounted pool while replace waits for confirmation.
- `tests/cli/braid-add-cloned-luks-header-rejected.py` and
  `tests/cli/replace-cloned-luks-header-rejected.py` verify cloned LUKS
  headers cannot make add or replace reuse a mapper opened from the wrong
  physical device.
- `tests/cli/braid-add-persists-before-balance.py` verifies fresh add writes
  canonical UUID-keyed membership, without a duplicate value-side `luks_uuid`,
  before post-add maintenance continues.
- `tests/cli/braid-doctor-uuid-swap.py` verifies `braid doctor` fails closed
  when a member's live LUKS UUID diverges from its pool.json key, surfacing
  the swap before any mutating command runs.

## Consequences

- `pool.json` key order is UUID order, not disk-name order. Display surfaces
  that need stable operator ordering must sort by `DiskName`.
- Recovery trusts journaled UUID-keyed membership snapshots for phase-specific
  replay and verifies live UUIDs again at mutation boundaries where a physical
  disk could have been swapped or reformatted.
- Mapper and label drift no longer break membership correlation, but drifted
  handles are not silently reconciled back into membership.
- Cloned disks with duplicate LUKS UUIDs are rejected before membership is
  written.

## Rejected Alternatives

1. **Keep disk name as identity.** Disk names are useful for humans but are not
   intrinsic to the encrypted device. Keeping them as identity preserves the
   label/mapper drift hazard.
2. **Use by-id as identity.** by-id paths identify hardware slots/devices, not
   encrypted membership. They can change with enclosures or controller
   behavior, and they do not detect cloned LUKS headers.
3. **Use btrfs devid as identity.** Devids are live filesystem state and are
   unavailable before mount. They remain useful only as fallback binding for
   missing devices.
4. **Identify an open mapper by rereading its disk's header.** braid used to
   run `cryptsetup luksUUID` on a mapper's backing device. That fails exactly
   when identity matters: a dead or unplugged disk leaves its mapper open with
   no header to read, so `lock` skipped the mapper and left its volume key in
   kernel memory, and recovery fell back to a persisted devid that is only as
   fresh as the last `pool.json` write. The dm UUID is kernel state, set at
   activation, and outlives the disk.

## See

- [017-runtime-disk-membership.md](017-runtime-disk-membership.md)
- [../principles.md](../principles.md)
- `cli/src/membership.rs`
- `cli/src/journal.rs`
- `cli/src/recover.rs`
- `cli/src/lock.rs`
- `cli/src/dm_identity.rs`
- `cli/src/parse/dm_uuid.rs`
- `cli/src/target_disk.rs`
