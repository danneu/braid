# Plan: key TUI `Bus` (transport) by LUKS UUID, not mapper name

## Context

The Data-tab disk table builds `disk_transport` (the `Bus` column: sata/nvme/usb)
by walking lsblk parent devices and keying the transport value by
`name_from_mapper(&child.name)` -- the crypt child's mapper basename
(`cli/src/tui/probe.rs:282-291`). Every *other* cell in a disk row resolves by
identity: size/errors by btrfs devid -> membership name
(`probe.rs:225, 308`), SMART/temp by membership name via `disks.by_id`
(`probe.rs:252-272`). Transport is the lone live-pool correlation still keyed
by a runtime mapper handle.

Under tolerated mapper-name drift (a member's LUKS container opened under
`braid-WRONG` while membership still names it `toshiba`, per
`docs/design/decisions/024-luks-uuid-identity.md`), every other cell in that
row resolves correctly but `Bus` silently renders `--`, because
`disk_transport` is keyed by `WRONG` rather than `toshiba`. The row reads as a
partial probe failure when it is really benign drift.

A clean identity join is available and was simply missed during the UUID
migration. The lsblk *parent* crypto_LUKS device carries both `tran` and the
LUKS header `uuid`. That uuid is the same on-disk field `cryptsetup luksUUID`
reads (libblkid LUKS prober: `reference/util-linux/libblkid/src/superblocks/luks.c:88-92`),
so it equals the membership `luks_uuid`. The `Pattern #3: display-only`
annotation was added in the "wip: finish luks uuid identity migration" commit
(`9c23a15`) -- transport was the one correlation left un-migrated and
*annotated* rather than converted.

**Intended outcome:** transport resolves by LUKS UUID like every sibling cell,
eliminating the `--` degradation under drift and finishing the Decision 024
migration. Decision 024 already prescribes exactly this for display code
(line 93-95: "Display code has an explicit join rule... resolve a live pool
device's UUID back to DiskName for presentation").

## The change

### 1. Re-key the transport join (`cli/src/tui/probe.rs:282-291`)

Replace the child-mapper-name walk with a parent-UUID -> membership-name
resolution. `uuid_to_name: HashMap<&LuksUuid, &str>` is already in scope at
`probe.rs:151` and is queried the same way at `probe.rs:163`
(`uuid_to_name.get(&device.luks_uuid)`), so the lookup ergonomics are proven.

Shape (final form at implementer's discretion):

```rust
// Transport (sata/nvme/usb) for the Bus column. Join by the parent
// crypto_LUKS device's LUKS UUID -> member name (Decision 024 display
// join), NOT the crypt child's mapper name, so transport survives mapper
// drift like every other identity-keyed cell. braid uses whole-disk LUKS,
// so the disk node carries both `tran` and the LUKS header uuid.
let mut disk_transport = HashMap::new();
if let Ok(lsblk_raw) = runner.run(&CmdRequest::LsblkJson)
    && let Ok(lsblk) = parse_lsblk_json(&lsblk_raw)
{
    for dev in &lsblk.blockdevices {
        if let Some(tran) = &dev.tran
            && let Some(uuid_raw) = &dev.uuid
            && let Ok(uuid) = LuksUuid::parse(uuid_raw)
            && let Some(name) = uuid_to_name.get(&uuid)
        {
            disk_transport.insert((*name).to_owned(), tran.clone());
        }
    }
}
```

This drops the nested child loop and the `crate::config::name_from_mapper`
dependency. `LuksUuid::parse` (`cli/src/types.rs:36`) canonicalizes both sides
to lowercase hyphenated, so the lookup is case/format robust. Fails safe: a
non-member or foreign-disk uuid is absent from `uuid_to_name` and is skipped,
exactly as today.

`disk_transport` stays `HashMap<String, String>` keyed by disk name; the sole
production consumer (`cli/src/tui/view/mod.rs:856-861`, width calc at `:933`)
is unchanged.

### 2. Fix the existing transport test fixture (`cli/src/tui/probe.rs:955-983`)

CRITICAL: `lsblk_transport_json` currently sets the parent `vdb` `uuid` to
`aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa`, which does NOT match the membership
LUKS uuid in `transport_test_disks()` (`11111111-1111-1111-1111-111111111111`).
Today that mismatch is invisible because the code never reads the parent uuid.
After the pivot the parent uuid is the join key, so this fixture must set the
parent `uuid` to `11111111-1111-1111-1111-111111111111` or the existing test
`disk_transport_comes_from_parent_lsblk_tran` (`probe.rs:1250`) breaks. The
child `braid-vdb` uuid (the btrfs FSID) stays arbitrary and irrelevant.

### 3. Add the drift regression test (`cli/src/tui/probe.rs` tests)

A focused, single-invariant test matching the shape of the existing transport
test `disk_transport_comes_from_parent_lsblk_tran` (`probe.rs:1250`) and the
sibling resolution guard `device_errors_keyed_by_devid_not_path`
(`probe.rs:1274`). Both assert one resolution invariant and carry the
motivation in a rich preamble rather than asserting unrelated cells -- the
braid convention ("behavioral and structure-insensitive").

Reuse the transport helper family. Extend `lsblk_transport_json` to take the
crypt child name (default still `braid-vdb`) so the happy-path and drift tests
share one JSON builder; the drift test passes `braid-WRONG` while the parent
disk still carries the member's LUKS uuid + `tran: "sata"`.

Assert exactly the two things that pin the pivot:

```rust
// happy path already covered by disk_transport_comes_from_parent_lsblk_tran;
// drift case: crypt child = braid-WRONG, parent uuid = vdb's membership uuid.
let transport = probe_disk_transport_drifted("sata");   // child braid-WRONG
assert_eq!(transport.get("vdb").map(String::as_str), Some("sata")); // resolves by UUID
assert!(!transport.contains_key("WRONG"));              // mapper name leaks into no key
```

A revert to `name_from_mapper` fails BOTH assertions (`get("vdb")` -> None,
`contains_key("WRONG")` -> true), so this fully guards the regression. The
"row stays coherent under drift" motivation goes in the Intent/Why/Scenario
preamble per Test Conventions -- NOT in redundant size/SMART/errors assertions,
which test cells the pivot never touches and which are already pinned by the
allocations test and `device_errors_keyed_by_devid_not_path`.

**Scenario preamble must be honest about the modeled layer.** The transport
join reads only lsblk + membership; it never consults the btrfs-show device
path or `cryptsetup status` mapper. The helper therefore drifts only the lsblk
crypt-child name (`braid-WRONG`) while holding the btrfs/cryptsetup mocks at
their canonical `braid-vdb` values, because the join does not read them. The
`Scenario` section must state this explicitly: drift is modeled at the lsblk
layer (the disk's crypt child opened under a non-canonical name while the
parent retains the member's LUKS UUID), so the test isolates the exact input
the old `name_from_mapper` keying consumed. Do NOT claim btrfs/cryptsetup are
also drifted -- they are held canonical by design, not by oversight.

### 4. Pin the lsblk tool-output contract in the golden lane (`cli/tests/support/golden_common.rs:69-81`)

The pivot introduces a NEW reliance on real lsblk emitting the LUKS UUID (and
`tran`) on the parent *disk* node. The unit tests in §2/§3 bake this into
hand-authored JSON, so they cannot catch real-tool drift. The golden lsblk
check (`golden_lsblk_json`, a `golden_test!` macro invocation) currently
asserts only `blockdevices.len() == 2`, `device_type == "disk"`, a non-empty
children list, and `children[0].device_type == "crypt"` -- it never asserts the
parent `uuid`/`tran` are populated. So if a future toolchain bump nulled or
relocated the parent `uuid`, transport would silently degrade to `--` for every
disk and no test would flag it (the parser's `lsblk_rejects_missing_required_*`
tests only fire on a MISSING key, not a present-but-null value).

Add two assertions inside the existing `for dev in &out.blockdevices` loop:

```rust
assert!(dev.uuid.is_some(), "disk {} missing lsblk uuid (LUKS header UUID join key)", dev.name);
assert!(dev.tran.is_some(), "disk {} missing lsblk tran (Bus column source)", dev.name);
```

The `lsblk-2disk.json` fixture already contains this data (parent `uuid` =
`8c78…`/`fafe…`, `tran` = `virtio`), so this is free. The `golden_test!`
closure is shared, so the assertions ride both the `nixos-25.11` and
`nixos-unstable` fixture lanes -- converting a silent re-capture regression into
a loud failure in the parser-drift lane built for exactly that.

### 5. Update Decision 024 (`docs/design/decisions/024-luks-uuid-identity.md`)

Mandatory per AGENTS.md (behavior/invariant change). Purely additive: add the
new drift test to "Tests That Enforce This", and add one sentence (e.g. under
"Concrete Improvements" / the existing "Display code has an explicit join rule"
bullet) noting the lsblk transport bridge now joins by LUKS UUID -- the last
display correlation to adopt the UUID->name rule. No removal is needed: the
`Pattern #3: display-only` text lives only as a code comment at `probe.rs:285`
and is deleted by the §1 rewrite, not by a doc edit; Decision 024 contains no
"Pattern #3"/"transport"/"Bus" text to remove.

## Out of scope

- Other `name_from_mapper` callsites (`lock.rs`, `recover.rs`, `add.rs`,
  `replace.rs`, `discover.rs`, journal Pattern #1) are correct per Decision 024
  (observed-mapper cleanup, label bootstrapping, display fallbacks) and are not
  touched.
- No change to `disk_transport`'s type or its consumer in `view/mod.rs`.

## Verification

- `just test-rust` -- runs the probe.rs unit tests (the corrected
  `disk_transport_comes_from_parent_lsblk_tran` and the new drift test) and the
  golden lane (the §4 `golden_lsblk_json` parent-`uuid`/`tran` assertions
  against the committed `nixos-25.11` fixture).
- `just test-rust-unstable` -- runs the same §4 golden assertions against the
  `nixos-unstable` fixture, so upstream lsblk output drift that drops the parent
  `uuid`/`tran` surfaces in the early-warning lane.
- Confirm the new test FAILS against the un-pivoted join (temporarily, to prove
  it guards the right thing): with mapper-name keying, transport keys by
  `WRONG`, so `get("vdb")` is `None` and `contains_key("WRONG")` is true.
- No import churn: `name_from_mapper` is called fully-qualified
  (`crate::config::name_from_mapper`, no top-level `use` in `probe.rs`), so
  removing the call leaves no unused import; `LuksUuid` is already in scope at
  `probe.rs:151`. No `use` lines change.
- No PTY/snapshot or VM test asserts the Bus column join (transport is pure
  resolution logic; `demo.rs` builds `disk_transport` by hand), so no VM run is
  required for correctness -- a focused `just test-rust` is the gate.

## Implementation notes

- §3: rather than the illustrative `probe_disk_transport_drifted("sata")`
  wrapper, the drift case reuses the existing `probe_disk_transport` helper with
  a new `child_name: &str` parameter (`probe_disk_transport(Some("sata"),
  "braid-WRONG")`). One shared builder, no extra wrapper function; the two
  pinning assertions are unchanged.
- §2: updated the happy-path test
  `disk_transport_comes_from_parent_lsblk_tran`'s Intent/Scenario preamble to
  describe the parent-LUKS-UUID join. The old preamble said the value mapped
  "to the child braid mapper's disk name", which the pivot makes inaccurate.
