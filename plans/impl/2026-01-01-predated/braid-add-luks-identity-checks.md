# Plan: harden braid add disk identity checks

## Context

`braid add` currently treats any existing LUKS device (`PresentLuks`) as a
plausible braid disk. If the mapper has a btrfs superblock from a different
pool, the code will happily call `btrfs device add` on it — potentially
merging unrelated filesystems. If the mapper has no superblock, it gets added
as a fresh device without questioning why a braid-labeled LUKS container has
no btrfs data.

This plan hardens `braid add` so it only proceeds automatically in two
cases: a raw disk that braid will initialize, or an existing braid-managed
disk that is proven to belong to the current pool via btrfs FSID comparison.

This is an add-only hardening pass. The shared `ConfigDiskState` enum stays
unchanged — identity classification is add-local, so `unlock`, `status`,
`replace`, `remove`, and `enroll_key_file` get zero churn.

Defaults: LUKS label is a necessary-but-not-sufficient identity signal.
Matching the current pool's btrfs FSID is required before trust. Bootstrap
refuses existing LUKS devices entirely. No destructive override for existing
LUKS devices in this change.

## Key changes

### 1. LUKS label reading

Add infrastructure to read the LUKS label from the text output of
`cryptsetup luksDump` (**not** `--dump-json-metadata`, which outputs only
the JSON metadata area — keyslots, segments, tokens — and does NOT include
the LUKS2 binary header fields like the label). The text output includes a
`Label:` line:

```
LUKS header information
Version:       	2
...
Label:       	braid-disk1
...
```

Implementation:
- Add `CmdRequest::CryptsetupLuksDumpText { device }` in `cli/src/cmd.rs` →
  runs `cryptsetup luksDump <device>` (no `--dump-json-metadata`).
- Add parser `parse_cryptsetup_luks_label()` in a new file
  `cli/src/parse/cryptsetup_luks_label.rs` that extracts the `Label:` field.
  Return `Option<String>` — `None` if the label is `(no label)` or empty,
  `Some(label)` otherwise.

### 2. Parse btrfs FSID from `btrfs filesystem show`

`BtrfsFilesystemShowOutput` (`cli/src/parse/types.rs:126-131`) already has
`pub uuid: Option<String>` but the parser
(`cli/src/parse/btrfs_filesystem_show.rs:76-129`) hardcodes it to `None`.
The output line is:
`Label: none  uuid: f1e2d3c4-b5a6-9788-7654-321fedcba098`

- Add a nom parser for this line, populate the `uuid` field.
- Add `pub fsid: Option<String>` to `PoolState` (`cli/src/probe.rs`),
  populated from the now-parsed FSID when pool is mounted.

### 3. Add `BtrfsFilesystemShowTarget` command variant

`BtrfsFilesystemShow` (`cli/src/cmd.rs:27-29`) takes `MountPoint`.
`btrfs filesystem show` also accepts device paths, but stuffing a device
path into `MountPoint` is a type lie.

Add a distinct variant:
```rust
BtrfsFilesystemShowTarget { target: String }
```
Generates the same `btrfs filesystem show <target>` argv but accepts any
path (device or mount point). Used for per-device FSID queries in the add
path.

### 4. Add-local identity classification in `cmd_add`

**Do not change `ConfigDiskState`.** Keep it as the shared probe result:
`Absent | PresentNotLuks | PresentLuks { uuid, mapper_open }`.

Instead, add an add-local enrichment step in `cmd_add`
(`cli/src/add.rs:200-244`) that classifies `PresentLuks` disks further.
Define a local enum:

```rust
enum AddLuksIdentity {
    /// LUKS label is not braid-<name> (or absent).
    NonBraid,
    /// Correct braid label, but pool is not mounted — can't verify.
    BraidLabeledNoPool,
    /// Correct braid label, mapper open, no btrfs superblock.
    /// Ambiguous: could be a previously removed disk, partial/manual state,
    /// or stale encrypted data. Not auto-addable without extra provenance.
    BraidLabeledNoBtrfs,
    /// Correct braid label, mapper open, btrfs FSID differs from pool.
    BraidLabeledForeignPool,
    /// Correct braid label, mapper open, btrfs FSID matches pool, already in pool.
    BraidLabeledAlreadyInPool,
    /// Correct braid label, mapper open, btrfs FSID matches pool, not yet in pool.
    BraidLabeledRecoverable,
}
```

Classification logic (a helper function in `add.rs`):
1. Read LUKS label via `CryptsetupLuksDumpText`.
2. If label ≠ `braid-<config-name>` → `NonBraid`.
3. If pool not mounted → `BraidLabeledNoPool`.
4. Open mapper if closed.
5. Run `BtrfsFilesystemShowTarget { target: mapper_path }`.
6. No btrfs superblock → `BraidLabeledNoBtrfs`.
7. FSID differs from `pool.fsid` → `BraidLabeledForeignPool`.
8. Already in pool (by mapper name) → `BraidLabeledAlreadyInPool`.
9. Otherwise → `BraidLabeledRecoverable`.

### 5. Update `cmd_add` execution path

Behavior by case:

| State | Action |
|-------|--------|
| `PresentNotLuks` | Keep current destructive path (format, open, add) |
| `PresentLuks` → `NonBraid` | Hard refuse: "disk is already a LUKS container but is not labeled as braid-\<name\>; braid will not adopt a non-braid encrypted device" |
| `PresentLuks` → `BraidLabeledNoPool` | Hard refuse: "disk is braid-labeled but no mounted pool exists to verify identity; bootstrap only accepts fresh disks" |
| `PresentLuks` → `BraidLabeledNoBtrfs` | Hard refuse: "braid-labeled but contains no btrfs superblock; identity is ambiguous without pool membership proof" |
| `PresentLuks` → `BraidLabeledForeignPool` | Hard refuse: "braid-managed device from a different btrfs filesystem; braid will not merge foreign pools" |
| `PresentLuks` → `BraidLabeledAlreadyInPool` | No-op |
| `PresentLuks` → `BraidLabeledRecoverable` | Recovery/add path |

Bootstrap (pool not mounted) is handled by `BraidLabeledNoPool` — any
PresentLuks disk with a braid label is refused when there's no pool to
verify against. Non-braid LUKS is refused unconditionally.

`BraidLabeledNoBtrfs` is also a hard refusal by design. This state is
ambiguous: it can result from a previously removed disk, but it can also mean
partial initialization, manual wiping, or stale encrypted data from an older
lifecycle. Without extra provenance (explicit tombstone/decommission state,
which is out of scope here), `braid add` must not treat this as a safe
recovery or re-add path.

The existing multi-disk superblock check (`add.rs:264-279`) becomes redundant
for the braid-labeled case but remains as defense-in-depth for the recovery
path.

### 6. Dry-run contract

`--dry-run` must remain read-only (no LUKS opens).

For `PresentLuks` disks in dry-run:
- If mapper is **already open**: the identity classification helper can run
  its full logic (label check, btrfs probe, FSID comparison) without side
  effects. Dry-run reports the exact blocked state or recovery step.
- If mapper is **closed**: dry-run reads the LUKS label (which doesn't
  require opening the mapper). If non-braid → report "blocked: non-braid
  LUKS". If braid-labeled → report "LUKS open + identity verification at
  execution time" instead of guessing the outcome.

Update `compile_add_steps_multi` (`cli/src/add.rs:344-450`) accordingly.

### 7. Update docs

**`docs/decisions/intent-cli.md`** (authoritative design doc):

- Table row for `braid add` (line 19): change "safe (existing LUKS)" to
  reflect that existing LUKS is only safe when it passes identity checks
  (braid label + matching pool FSID). Non-braid LUKS and foreign-pool
  devices are refused.
- Safety model section (lines 38-47): expand point #2 "Superblock guard"
  into a layered identity check:
  1. LUKS label must be `braid-<key>` (non-braid LUKS refused outright).
  2. Pool must be mounted (bootstrap refuses existing LUKS).
  3. Opened mapper's btrfs FSID must match the current pool (foreign-pool
     disks refused).
  4. Superblock guard remains as defense-in-depth within the FSID-matching
     path.
- Add a note on dry-run behavior: `--dry-run` reads the LUKS label without
  side effects. Full identity verification (FSID comparison) requires
  opening the mapper, so dry-run defers this to execution time when the
  mapper is closed.

**`docs/principles.md`** — Principle #3 (line 19):

- Expand "the btrfs superblock guard prevents accidental data loss" to also
  mention the LUKS-label + pool-FSID identity checks. The superblock guard
  is now one layer of a multi-layer identity verification.

**`README.md`**:

- Update the `braid add` section to describe the three cases: fresh disk
  (format + add), returning braid disk (identity-verified recovery), and
  refused (non-braid LUKS, foreign pool, braid-labeled without btrfs identity
  proof, or unmounted pool with existing LUKS).

## Files to modify

| File | Change |
|------|--------|
| `cli/src/cmd.rs` | Add `CryptsetupLuksDumpText` and `BtrfsFilesystemShowTarget` variants |
| `cli/src/parse/cryptsetup_luks_label.rs` | New: LUKS label parser |
| `cli/src/parse/btrfs_filesystem_show.rs` | Parse FSID from uuid line |
| `cli/src/parse/types.rs` | Add `CryptsetupLuksLabelOutput` |
| `cli/src/parse/mod.rs` | Register new parser module |
| `cli/src/probe.rs` | Add `fsid` to `PoolState`, populated from parsed FSID |
| `cli/src/add.rs` | Add `AddLuksIdentity` enum + classification helper; rewrite `PresentLuks` handling; update `compile_add_steps_multi` dry-run logic |
| `docs/decisions/intent-cli.md` | Expand safety model with layered identity checks; update `braid add` risk description; document dry-run contract |
| `docs/principles.md` | Expand Principle #3 to include LUKS-label + FSID identity checks |
| `README.md` | Document the three `braid add` cases (fresh, returning, refused) |

**No changes to:** `types.rs` (ConfigDiskState), `unlock.rs`, `status.rs`,
`replace.rs`, `remove.rs`, `enroll_key_file.rs`.

## Test plan

### Rust unit tests

**Parser tests:**

1. `parse_cryptsetup_luks_label` extracts `braid-foo` from luksDump text.
2. `parse_cryptsetup_luks_label` returns `None` for `(no label)` or empty.
3. `parse_btrfs_filesystem_show` populates FSID from uuid line.
4. `parse_btrfs_filesystem_show` existing tests still pass with FSID parsing
   added.
5. `BtrfsFilesystemShowTarget` argv renders correctly.
6. `CryptsetupLuksDumpText` argv renders correctly.

**Add-path identity classification:**

7. Identity helper returns `NonBraid` when label is absent.
8. Identity helper returns `NonBraid` when label is some other value.
9. Identity helper returns `BraidLabeledNoPool` when pool not mounted.
10. Identity helper returns `BraidLabeledNoBtrfs` when mapper has no btrfs
    superblock.
11. Identity helper returns `BraidLabeledForeignPool` when FSID differs.
12. Identity helper returns `BraidLabeledRecoverable` when FSID matches and
    not in pool.
13. Identity helper returns `BraidLabeledAlreadyInPool` when FSID matches
    and already in pool.

**Add-path decision logic:**

14. `cmd_add` rejects `NonBraid` with appropriate message.
15. `cmd_add` rejects `BraidLabeledNoPool` with appropriate message.
16. `cmd_add` rejects `BraidLabeledNoBtrfs` with appropriate message.
17. `cmd_add` rejects `BraidLabeledForeignPool` with appropriate message.
18. `cmd_add` accepts `BraidLabeledRecoverable` — completes add.
19. `cmd_add` treats `BraidLabeledAlreadyInPool` as no-op.

**Dry-run tests:**

20. Dry-run for raw disk still shows destructive LUKS format flow.
21. Dry-run for non-braid LUKS (label readable without opening) reports
    blocked.
22. Dry-run for braid-labeled + mapper open + foreign FSID reports blocked.
23. Dry-run for braid-labeled + mapper closed reports "identity verification
    at execution time".

### NixOS VM integration tests

Each with the required intent/why/scenario block comment:

24. Raw fresh disk: `braid add` formats, opens, adds, mounts successfully.
25. Existing non-braid LUKS disk with no label: `braid add` refuses, pool
    unchanged.
26. Existing non-braid LUKS disk with unrelated label: `braid add` refuses,
    pool unchanged.
27. Existing braid-labeled disk from same pool, mapper closed: `braid add`
    reopens and completes recovery.
28. Existing braid-labeled disk from same pool, mapper already open:
    `braid add` completes recovery.
29. Existing braid-labeled disk already in current pool: `braid add` no-op.
30. Existing braid-labeled disk from a different braid pool but same disk
    name: `braid add` refuses with foreign-pool message.
31. Existing braid-labeled disk with no btrfs superblock: `braid add`
    refuses with ambiguous-identity message.
32. Bootstrap with `PresentNotLuks` disks succeeds.
33. Bootstrap with an existing braid-labeled LUKS disk refuses.
34. Bootstrap with an existing non-braid LUKS disk refuses.

### Regression

35. All existing `replace` tests pass unchanged (shared `ConfigDiskState`
    untouched).
36. All existing `unlock`, `status`, `enroll_key_file` tests pass unchanged.
37. Existing add-path tests pass (existing behavior for `PresentNotLuks` and
    basic `PresentLuks` unchanged where identity checks pass).

## Verification

1. `just test-rust` — all unit tests pass.
2. `just test` — all NixOS VM tests pass (run without `-v` first).
3. `just test <new-test-name> -v` for any new VM test that fails, to debug.

## Assumptions

- LUKS label is necessary but not sufficient identity signal.
- Matching current pool FSID is required before trusting existing braid-
  labeled disks.
- Bootstrap refuses all existing LUKS devices (no pool to validate against).
- Destructive "reformat existing LUKS" workflow is out of scope.
- The `-f` flag removal from `btrfs device add` is already complete
  (commit `592b11b`).
- LUKS label can be read without opening the mapper (via `cryptsetup luksDump`
  on the raw device), so dry-run can classify non-braid LUKS without side
  effects.
- Full identity verification (btrfs FSID comparison) requires the mapper to
  be open, so dry-run defers this to execution time when the mapper is closed.
- A braid-labeled disk with no btrfs superblock is intentionally refused as
  ambiguous, even if it may have been previously removed from the pool.
