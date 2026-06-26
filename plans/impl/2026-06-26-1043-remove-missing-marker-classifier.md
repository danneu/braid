# Plan: classify the remove-missing relocation target on the missing marker, not device_size alone

## Context

`check_relocation_space` in `cli/src/remove_missing.rs#check_relocation_space` is the ENOSPC
pre-flight for `braid remove-missing`. It runs `btrfs device usage --raw`, identifies the
"missing" target by `device_size == 0 && devid == missing_id`, treats every other
`device_size > 0` device as a surviving `remaining`, and fails closed if it cannot prove
survivors can absorb the target's allocations. When no `device_size == 0` row matches, it
emits a single message claiming the devid "is not listed."

A review pass found this is not merely a misleading diagnostic -- the `device_size == 0`
classifier is an **unsafe trust boundary**:

- btrfs-progs derives `device_size` per device in
  `reference/btrfs-progs/cmds/filesystem-usage.c#load_device_info` (shared by
  `btrfs device usage` via `reference/btrfs-progs/cmds/device.c#_cmd_device_usage` ->
  `load_chunk_and_device_info`). For a **present** device with a real path it calls
  `device_get_partition_size`; on failure it logs a warning and sets `info->device_size = 0`.
- A genuinely missing device gets `device_size == 0` only as a *side effect* of the same
  probe failing on the kernel's pseudo-path. The kernel's `btrfs_dev_name()` returns the
  marker `<missing disk>` for a `BTRFS_DEV_STATE_MISSING` device (literal `missing` is the
  btrfs-progs fallback when the ioctl hands back an empty path). braid's parser captures
  this faithfully in `cli/src/parse/types.rs#BtrfsDeviceUsageEntry` (`path: String`), pinned
  by `cli/src/parse/btrfs_device_usage.rs#device_usage_parses_missing_device_marker` and the
  `btrfs-device-usage-missing.txt` golden.

So `device_size == 0` does **not** prove "missing." A present device whose partition-size
probe transiently fails is rendered as a **real path with `device_size == 0`** -- identical
to a missing device on the size axis. The current filter would adopt that live device as the
relocation target, run the RAID1 relocation math against it, and proceed toward
`btrfs device remove` on a device btrfs still considers present. That is exactly the
read-only-crash class of failure the pre-flight exists to prevent, and it violates ADR 012's
fail-closed-on-uncertainty rule for `remove-missing`
(`docs/design/decisions/012-intent-cli.md`, "ENOSPC pre-flight check").

The same gap also produces the originally-reported symptom: when `btrfs filesystem show`
(the upstream probe behind `validate_missing_id_target`) reports devid N missing but
`device usage` lists it with a nonzero size (probe disagreement, e.g. the device came back),
the operator is told it "is not listed" -- which they can disprove by reading the very
output the message cites.

The ideal fix is to **retire the size-only trust boundary**: classify the targeted devid's
usage stanza on the kernel missing marker plus zero size, and fail closed with an accurate,
case-specific diagnostic for every non-trusted shape. This also tightens ADR 012's trust
contract (which today names only "exactly one usage stanza" + allocation-row shape) and the
`remove-missing` command doc.

## Change

### 1. Replace the size-only `is_missing()` with a marker predicate (`cli/src/parse/`)

Co-locate the marker knowledge with the parser that already pins it. In
`cli/src/parse/btrfs_device_usage.rs`, add named constants and have the existing parser test
reference them instead of the bare literal:

```rust
/// Kernel `btrfs_dev_name()` marker for a BTRFS_DEV_STATE_MISSING device, copied through
/// BTRFS_IOC_DEV_INFO by btrfs device usage (reference/btrfs-progs/cmds/filesystem-usage.c).
pub const MISSING_DEVICE_PATH_MARKER: &str = "<missing disk>";
/// btrfs-progs fallback when the dev-info ioctl returns an empty path.
pub const MISSING_DEVICE_PATH_FALLBACK: &str = "missing";
```

Add a predicate on `cli/src/parse/types.rs#BtrfsDeviceUsageEntry` (alongside `used_bytes` /
`allocated_by_type`), with a `///` per the project's doc-comment rule:

```rust
/// True when btrfs rendered this stanza with a "missing device" path marker rather than a
/// real block-device path. Trusting a relocation target keys on this, never on
/// `device_size == 0` alone: btrfs-progs also reports `Device size: 0` for a PRESENT device
/// whose `device_get_partition_size` probe failed, so size alone cannot tell a missing
/// member from a live device with a transient probe failure.
pub fn has_missing_marker(&self) -> bool {
    self.path == MISSING_DEVICE_PATH_MARKER || self.path == MISSING_DEVICE_PATH_FALLBACK
}
```

No fixture/parse-behavior change -- `path` is already parsed; this only adds a classifier and
names existing constants.

**Retire the misleading `is_missing()` helper.**
`cli/src/parse/types.rs#BtrfsDeviceUsageEntry::is_missing` today returns `self.device_size == 0`
but is named and documented as a marker check ("Identifies btrfs's `<missing disk>` usage
row"). That conflation is the root of this bug: it implements the *zero-size* concept while
claiming the *missing-marker* concept, and the two diverge for a present device whose size
probe failed (real path, size 0). Remove `is_missing()` so `has_missing_marker()` is the only
parser-level "missing" predicate, and route each of its three call sites to the concept it
actually needs:

- `cli/src/remove_missing.rs#check_relocation_space`, target identity (`d.is_missing() &&
  d.devid == missing_id`) -> the `classify_usage_target` classifier in Change 2 (marker **and**
  size 0).
- `cli/src/remove_missing.rs#check_relocation_space`, survivor filter (`!d.is_missing()`) ->
  inline `d.device_size > 0` in Change 2: a *capacity-trust* check, deliberately size-based (a
  size-0 survivor -- missing or probe-failed -- has no capacity we can trust; see Non-goals).
- `cli/src/ack.rs#write_enospc_baseline` (`entries.iter().any(|e| e.is_missing())`) -> the
  ENOSPC-baseline bail-out. This stays **size-based**: replace with inline `e.device_size == 0`
  (behavior-preserving). ADR 014 (`docs/design/decisions/014-alerts.md`) makes this a
  load-bearing invariant -- "a zero-sized device never appears in a baseline that suppresses an
  alert," with the snooze marker keyed on per-device `(devid, device_size)`. A *present* device
  with a failed size probe (`device_size == 0`, real path) must still trip this guard, so this
  site must **not** move to `has_missing_marker()`: narrowing it to the marker would let a
  zero-sized real-path row be baselined and violate ADR 014. Reword its comment to name the real
  condition:

```rust
// ADR 014: never baseline a snapshot that carries a zero-sized device. btrfs renders a
// missing member as `<missing disk>` with device size 0, and a present device whose size
// probe failed also reports 0; either makes the baseline untrustworthy, so the bail-out keys
// on size, not on the missing marker.
if entries.iter().any(|entry| entry.device_size == 0) {
    eprintln!(
        "warning: usage reports a missing device; ack cleared the alert but wrote no ENOSPC baseline"
    );
    return;
}
```

The `eprintln!` text is kept verbatim to stay behavior-preserving; tightening "a missing
device" to "a zero-sized device" is optional and would require updating any `ack` test that
matches the current substring. The code change preserves ADR 014's behavior and its operative
invariant (`docs/design/decisions/014-alerts.md#severity-tiers-and-the-enospc-baseline`: "a
zero-sized device never appears in a baseline that suppresses an alert"); Change 7 tightens that
ADR's marker-worded *description* of the ack-write condition so the authority doc matches its own
invariant instead of luring future cleanup back toward `has_missing_marker()`.

### 2. Replace the size-only filter with a `UsageTargetState` classifier (`cli/src/remove_missing.rs`)

Introduce an enum + classifier (the reviewer's pivot). It owns target *identity*; the
existing `validate_missing_target_usage_shape` keeps allocation-row *shape* validation.

```rust
/// How the targeted devid appears in `btrfs device usage --raw`. The relocation preflight
/// trusts a stanza as the missing target only when btrfs renders it with the kernel missing
/// marker AND zero device size -- see BtrfsDeviceUsageEntry::has_missing_marker for why size
/// alone is unsafe.
enum UsageTargetState<'a> {
    /// devid does not appear in usage output at all.
    Absent,
    /// Exactly one stanza, missing marker path, device_size == 0.
    TrustedMissing(&'a BtrfsDeviceUsageEntry),
    /// Exactly one stanza, but a real path and/or nonzero size: `btrfs filesystem show`
    /// (upstream missing-set probe) and `btrfs device usage` disagree about missing-ness.
    PresentNotMissing(&'a BtrfsDeviceUsageEntry),
    /// More than one stanza for the devid: ambiguous/corrupt output.
    Duplicate(usize),
}

fn classify_usage_target(
    devices: &[BtrfsDeviceUsageEntry],
    missing_id: Devid,
) -> UsageTargetState<'_> {
    let matches: Vec<&BtrfsDeviceUsageEntry> =
        devices.iter().filter(|d| d.devid == missing_id).collect();
    match matches.as_slice() {
        [] => UsageTargetState::Absent,
        [entry] if entry.has_missing_marker() && entry.device_size == 0 => {
            UsageTargetState::TrustedMissing(entry)
        }
        [entry] => UsageTargetState::PresentNotMissing(entry),
        many => UsageTargetState::Duplicate(many.len()),
    }
}
```

Rewrite the body of `check_relocation_space` (the block currently computing `target` /
`remaining` and the `target.is_empty()` arm) to match on the classifier. `remaining` is
computed only on the trusted path:

```rust
let target = match classify_usage_target(&usage.devices, missing_id) {
    UsageTargetState::Absent => {
        return Err(RemoveMissingError::Validation(format!(
            "ENOSPC pre-flight: missing devid {missing_id} is not listed in \
             `btrfs device usage --raw {mount_point}`, so its allocations cannot be \
             measured. Refusing to remove the missing device without a validated \
             relocation-space check. Inspect the command output manually, then re-run."
        )));
    }
    UsageTargetState::Duplicate(n) => {
        return Err(RemoveMissingError::Validation(format!(
            "ENOSPC pre-flight: missing devid {missing_id} is listed {n} times in \
             `btrfs device usage --raw {mount_point}`. Refusing to remove the missing \
             device without a validated relocation-space check. Inspect `btrfs device \
             usage --raw {mount_point}` manually, then re-run."
        )));
    }
    UsageTargetState::PresentNotMissing(entry) => {
        return Err(RemoveMissingError::Validation(format!(
            "ENOSPC pre-flight: devid {missing_id} is listed in `btrfs device usage --raw \
             {mount_point}` as `{path}` with device size {size}, not as a missing device \
             (a missing member appears as `{marker}` with device size 0). The pool probe \
             (`btrfs filesystem show`) and `btrfs device usage` disagree about devid \
             {missing_id} -- the device may have come back online, or its size probe \
             failed. Refusing to remove the missing device without a validated \
             relocation-space check. Re-run `braid status` to re-probe the pool, then \
             re-run remove-missing only if the device is still missing.",
            path = entry.path,
            size = entry.device_size,
            marker = crate::parse::btrfs_device_usage::MISSING_DEVICE_PATH_MARKER,
        )));
    }
    UsageTargetState::TrustedMissing(entry) => entry,
};

validate_missing_target_usage_shape(target, mount_point, missing_id)?;

let remaining: Vec<_> = usage.devices.iter().filter(|d| d.device_size > 0).collect();
preflight::check_raid1_relocation_space(&[target], &remaining).map_err(|e| {
    RemoveMissingError::Validation(format!(
        "{e}\n\nFree up space by deleting files, or add a new device first with `braid add`."
    ))
})
```

Notes:
- `preflight::check_raid1_relocation_space` takes `&[&BtrfsDeviceUsageEntry]`
  (`cli/src/preflight.rs#check_raid1_relocation_space`), so the single trusted `target`
  passes as `&[target]`.
- The `PresentNotMissing` arm covers BOTH probe-disagreement shapes with one message: real
  path + nonzero size (device looks live) and real path + `device_size == 0` (the btrfs-progs
  size-probe failure). It names the observed path and size so the operator sees exactly why.
- The `remaining` filter inlines `d.device_size > 0`, replacing the old `!d.is_missing()` call
  (Change 1) -- this site is a capacity-trust check, so it stays size-based, not marker-based.
- ASCII-only (enforced by `scripts/docs/check-output-ascii.py` over non-test `cli/src`): the
  literals use `--`, straight quotes, backticks, and `<missing disk>` (all ASCII).

### 3. Narrow `validate_missing_target_usage_shape` to a single entry (`cli/src/remove_missing.rs`)

`cli/src/remove_missing.rs#validate_missing_target_usage_shape` currently takes
`&[&BtrfsDeviceUsageEntry]` and starts with a `target.len() > 1` "listed more than once" arm.
The classifier now owns duplicate detection (and guarantees exactly one entry on the trusted
path), so:

- Change the signature to take a single `target: &BtrfsDeviceUsageEntry`.
- Delete the `target.len() > 1` arm and the `let target = target[0];` line.
- Keep the allocation-shape checks unchanged (supported `Data/Metadata/System` RAID1 types;
  at least one positive supported row).

This removes the now-dead duplicate arm previously documented as unreachable (commit
`fcedb3b6`), consolidating the "exactly one stanza" rule in one place.

### 4. Tighten the `check_relocation_space` doc comment (`cli/src/remove_missing.rs`)

The doc comment currently asserts: "Missing devices are identified by `device_size == 0` ...
This is reliable: present devices always have device_size > 0, and missing devices always
report 0." That claim is false (see Context). Replace it with: missing devices are identified
by the kernel missing-marker path (`<missing disk>`, or the `missing` fallback) together with
`device_size == 0`; size alone is not trusted because btrfs-progs reports `Device size: 0`
for a present device whose partition-size probe fails, so a real-path stanza is treated as a
probe disagreement and refused.

### 5. Update ADR 012 trust contract (`docs/design/decisions/012-intent-cli.md`)

ADR 012 is `status: Active`. Its "ENOSPC pre-flight check" section currently defines the
`remove-missing` trust check as shape-only: "the targeted missing devid must have exactly one
usage stanza, every positive target allocation row must be one of Data/Metadata/System RAID1,
and at least one positive supported row must be present." Adding a path-marker requirement is
an invariant change, so the ADR must record it: the single stanza must also carry the kernel
missing-device path marker; a stanza with a real device path (even at `device_size == 0`,
which btrfs-progs emits when a present device's size probe fails) is a probe disagreement and
is refused rather than trusted as the relocation target.

### 6. Update the command doc refusal list (`docs/commands/remove-missing.md`)

The "Safety checks / refusal cases" ENOSPC bullet enumerates untrusted-shape sub-cases ("did
not list the targeted missing devid ... listed more than once ... allocation profile braid
does not model ... no positive Data/Metadata/System RAID1 row"). Add the new case: the
targeted devid is listed but **not as a missing device** (a real path, or a nonzero device
size) -- a disagreement between `btrfs filesystem show` and `btrfs device usage`. Keep
`README.md` in sync if it mirrors this list (it does not currently enumerate these
sub-cases; verify during implementation).

### 7. Tighten ADR 014's ENOSPC-baseline wording (`docs/design/decisions/014-alerts.md`)

ADR 014 is `status: Active`. The operative invariant under
`docs/design/decisions/014-alerts.md#severity-tiers-and-the-enospc-baseline` is already
size-based ("a zero-sized device never appears in a baseline that suppresses an alert," and the
snooze `pool_key` is keyed on per-device `(devid, device_size)`). But the "Ack snoozes the
reminder" bullet in that same section still describes the ack-write condition in *marker* terms:
ack writes a snooze only when "the fresh usage snapshot contains no missing-device marker," and
writes none when it "contains btrfs's missing marker (`device_size == 0`, rendered as `<missing
disk>`)." That phrasing equates "missing marker" with `device_size == 0` -- the exact conflation
this plan retires (Change 1) -- and is narrower than the invariant, since a present device with a
failed size probe is `device_size == 0` with a *real* path, not the `<missing disk>` marker. Left
as is, the authority doc points future cleanup back toward the unsafe `has_missing_marker()`
narrowing that the new ack regression test (Tests) guards against.

Tighten the bullet to size-based wording, naming both cases:

- ack writes a snooze marker only when the fresh usage snapshot contains **no zero-sized
  device**;
- it writes **no** marker when the snapshot contains any device with `device_size == 0` --
  whether a btrfs missing member (rendered `<missing disk>`) or a present device whose size probe
  failed (real path, size 0).

This is a wording-consistency fix, not an invariant change: it brings the bullet into agreement
with the invariant the same section already states, and matches the size-based `ack` guard in
Change 1. Touch only the "Ack snoozes the reminder" bullet; the `(devid, device_size)` pool_key
and the "accepted race" paragraph already use the correct size-based framing.

## Tests

Follow the repo's `// Intent / Why it exists / Scenario` preamble convention. Assertions are
behavioral (error variant + message substrings), not structure-sensitive. Build stanzas with
`cli/src/test_fixtures/shared.rs#DeviceUsageSpec::live` (renders any literal path -- a real
device path, or the `missing` fallback marker) and `DeviceUsageSpec::missing` (renders the
`<missing disk>` marker, size 0), via `relocation_usage_live_device` / `device_usage_raw_body`
/ `EnospcRunner` / `mp()`.

Add:

1. **Present with nonzero size** (probe disagreement, device looks live): devid 3 via
   `relocation_usage_live_device("/dev/mapper/braid-disk3", 3, ..., _)` (its hardcoded size is
   nonzero). Assert `Err(Validation)` whose message contains "device size", "disagree", and
   the real path, and does **not** contain "is not listed".
2. **Present with real path and `device_size == 0`** (the btrfs-progs size-probe failure that
   invalidates the size-only premise): build with
   `DeviceUsageSpec::live("/dev/mapper/braid-disk3", 3, 0, &[("Data","RAID1", 67_108_864)], _)`.
   Assert it fails closed with the same probe-disagreement message (names the real path), and
   does **not** reach relocation/shape errors -- i.e. a live device with a failed size probe
   is never adopted as the missing target.
3. **Duplicate**: two stanzas with devid 3. Assert `Err(Validation)` containing "listed 2
   times" (or the chosen wording).
4. **Trusted-missing via the `missing` fallback marker** (covers the
   `MISSING_DEVICE_PATH_FALLBACK` arm of `has_missing_marker`, which the `<missing disk>`
   fixtures never exercise): build the targeted devid with
   `DeviceUsageSpec::live("missing", 3, 0, &[("Data","RAID1", <positive>)], _)` so
   `device_usage_raw_body` renders the literal `missing, ID: 3` / `Device size: 0` header
   (btrfs-progs' empty-path fallback). Give survivors enough space (mirror
   `check_relocation_space_accepts_sparse_data_only_missing_target`'s survivor set) and assert
   the preflight returns `Ok(())` -- the fallback marker classifies as `TrustedMissing` and
   proceeds, never the `PresentNotMissing` probe-disagreement refusal. Regression guard: an
   implementation that drops the `MISSING_DEVICE_PATH_FALLBACK` arm would misclassify this
   stanza as `PresentNotMissing` and fail this test.
5. **Existing `<missing disk>` accept tests stay green**: confirm the existing accept tests
   (`check_relocation_space_accepts_sparse_data_only_missing_target`, etc.) stay green --
   they use `DeviceUsageSpec::missing` (-> `<missing disk>`, size 0) and so classify as
   `TrustedMissing`. Likewise `check_relocation_space_fails_closed_on_present_zero_allocation_missing_target`
   (missing marker + no positive row) still reaches and fails in `validate_missing_target_usage_shape`.

The existing `check_relocation_space_fails_closed_on_target_absent_from_usage` keeps its "is
not listed" assertion (the `Absent` arm wording is unchanged).

**`cli/src/ack.rs` (ADR 014 regression).** Removing `is_missing()` turns
`write_enospc_baseline`'s bail-out into an inline `device_size == 0`. Add one behavioral test
mirroring `cmd_ack_mounted_enospc_missing_usage_writes_no_snooze`, but render the zero-sized
device with a **real path** -- `DeviceUsageSpec::live("/dev/mapper/braid-disk3", 3, 0, ...)`
instead of the `<missing disk>` marker -- and assert ack writes **no** ENOSPC snooze/baseline
marker. This pins ADR 014's "a zero-sized device never appears in a baseline that suppresses an
alert" for the present-but-probe-failed case, so a future narrowing of the guard to
`has_missing_marker()` would fail it. Existing ack baseline tests
(`cmd_ack_mounted_enospc_missing_usage_writes_no_snooze`,
`cmd_ack_mounted_enospc_risk_writes_reprobed_keyed_baseline`) stay green -- the predicate change
is behavior-preserving.

## Non-goals / scope decisions

- **Survivor filter stays `device_size > 0`.** A surviving device whose size probe failed
  (real path, size 0) is excluded from `remaining`, undercounting capacity and biasing toward
  refusal -- the safe direction for this fail-closed path. We cannot trust the capacity of a
  size-0 survivor, so excluding it is correct; making survivor classification marker-aware is
  not needed and is out of scope -- the same capacity-trust rationale keeps `ack`'s
  ENOSPC-baseline guard size-based (Change 1). (Surfaced for review.)
- **Do not unify or re-probe the two btrfs probes.** Sharing one probe between
  `validate_missing_id_target` and `check_relocation_space`, or re-running on disagreement, is
  a larger architectural change; failing closed with an accurate diagnostic is the correct,
  minimal-surface fix here.

## Verification

1. `just test-rust` -- runs the Rust unit suite. Confirm the new/expanded
   `check_relocation_space_*` behaviors above pass (both marker forms -- `<missing disk>` and
   the `missing` fallback -- on the trusted path) and all existing `check_relocation_space_*`
   and `device_usage_*` parser tests stay green. Confirm the new `ack` zero-sized-real-path
   baseline test passes and the existing `cmd_ack_mounted_enospc_*` baseline tests stay green
   (the `is_missing()` removal is behavior-preserving at every call site).
2. ASCII guard: `python3 scripts/docs/check-output-ascii.py` (or CI) -- new non-test message
   literals must be ASCII-clean.
3. `just docs-build` -- `mdbook-linkcheck2` validates the edited ADR 012, ADR 014, and
   `remove-missing.md` (no broken cross-links).
4. No fixture refresh: no parser behavior, fixture, or `flake.lock` `nixpkgs` node changes, so
   `just capture-all-fixtures` / `just test-parsers` are not triggered.

## Implementation notes

- Tests item 3 (Duplicate) was satisfied by updating the existing
  `check_relocation_space_fails_closed_on_duplicate_target_stanza` assertion from "listed more
  than once" to "listed 2 times" rather than adding a second duplicate test -- the existing test
  already builds two devid-3 stanzas and exercises the new `UsageTargetState::Duplicate` arm, so
  a fresh test would have been redundant.
