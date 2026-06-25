# Pin that `discover` skips decrypted-mapper by-id aliases of an unlocked member

## Context

`braid discover` is a repair tool that scans `/dev/disk/by-id/` for LUKS devices
with `braid-<name>` labels to rebuild a lost or corrupt `pool.json`
(`docs/commands/discover.md`). It may legitimately run against an **attached,
already-unlocked** array.

When a pool is unlocked, each opened LUKS member gains a device-mapper target
named `braid-<name>` (`cli/src/config.rs#mapper_name`), and udev creates extra
`/dev/disk/by-id/` symlinks pointing at the *plaintext* mapper (`/dev/dm-N`):

- `dm-name-braid-<name>`
- `dm-uuid-CRYPT-LUKS2-<uuid-without-dashes>-braid-<name>`

These plaintext aliases must **not** be admitted as members. Today they are
correctly filtered: the decrypted mapper has no LUKS header, so
`cryptsetup isLuks` returns nonzero and the alias is skipped at the isLuks gate
(`cli/src/discover.rs#discover_from_dir_inner`). The whole-disk `wwn-`/`ata-`
alias supplies the member.

**The gap is test coverage, not behavior.** Nothing pins this:

- The `dm-*` aliases are **not** partition-filtered -- `is_partition_entry`
  (`cli/src/by_id.rs#is_partition_entry`) only matches `-partN`, so
  `dm-name-*`/`dm-uuid-*` flow straight to the isLuks probe. The isLuks gate is
  therefore the *sole* thing keeping a plaintext alias out of membership.
- No existing discover test exercises an unlocked pool's by-id directory: the
  unit tests in `cli/src/discover.rs` build synthetic tempdir by-id fixtures
  that never include `dm-name-*`/`dm-uuid-*` aliases, and the VM test in
  `tests/cli/braid-discover.py` runs discover *before* unlock (asserting
  whole-disk `virtio-` handles only).

A future change to the isLuks gate or the entry filter could let a decrypted
mapper alias through. The realistic failure mode is not a silent double-count
(`cli/src/membership.rs#PoolMembership::insert` enforces unique name/by-id
across four axes) but either a spurious `LuksDumpFailed` warning or a hard
`LabelCollision` error (the dm alias canonicalizes to `/dev/dm-N`, distinct from
the physical `/dev/sdX`, so it collides with the real member's name in the
alias-dedup of `cli/src/discover.rs#discover_from_dir_inner`). Either regression
would slip through CI today.

This change adds one cheap, deterministic unit test that pins the invariant at
the scanner boundary -- no production code changes, no VM test.

## Change: one unit test in `cli/src/discover.rs`

Add a single `#[cfg(test)]` test to the existing `tests` module in
`cli/src/discover.rs`, placed immediately after
`non_luks_device_never_reaches_luks_dump` (its closest sibling -- both pin the
isLuks gate filtering a non-LUKS device). **No fixture changes are needed**; the
test reuses existing helpers (`DiscoverLabelMap::new`, `discover_create_target`,
`discover_create_by_id_symlink`, `by_id_for`, `discover_from_dir`, `.calls()`).

The mock already mirrors a headerless plaintext device exactly: any path absent
from the label map returns `isLuks` exit 1 (and `luksDump` exit 1), per
`cli/src/test_fixtures/discover.rs#DiscoverLabelMap`.

```rust
#[test]
fn discover_skips_decrypted_mapper_aliases_of_unlocked_member() {
    /*
     * Intent: when a pool is already unlocked, /dev/disk/by-id/ carries
     *   dm-name-braid-<name> and dm-uuid-CRYPT-LUKS2-<uuid>-braid-<name>
     *   symlinks pointing at the plaintext mapper (/dev/dm-N). Discover must
     *   record the member exactly once, via its whole-disk wwn-/ata- handle,
     *   and must neither admit nor warn about the decrypted-mapper aliases.
     * Why it exists: discover is a repair tool that may run against an
     *   attached, unlocked array. The decrypted mapper has no LUKS header, so
     *   `cryptsetup isLuks` returns nonzero and the alias is skipped silently
     *   at the isLuks gate -- but nothing pinned this. The dm-* aliases are
     *   NOT partition-filtered (is_partition_entry only matches -partN), so the
     *   isLuks gate is the sole thing keeping a plaintext alias out of
     *   membership. A future change to that gate or the entry filter could let
     *   a decrypted-mapper alias collide with the real member's name
     *   (LabelCollision) or pollute the warning stream, and no other discover
     *   test exercises an unlocked pool's by-id directory -- the unit fixtures
     *   never include dm-* aliases and the VM test runs discover before unlock.
     * Scenario: an operator whose pool.json was lost unlocks the array, then
     *   runs `braid discover` to inspect membership before rebuilding. udev has
     *   created dm-name-/dm-uuid- aliases for the open mapper alongside the
     *   stable wwn- whole-disk handle; discover must reconstruct one member
     *   from the whole-disk handle and ignore the plaintext aliases.
     */
    let dir = tempfile::tempdir().unwrap();

    // The physical disk (locked LUKS header lives here) and the plaintext
    // mapper exposed by the open dm-crypt target are DISTINCT canonical
    // devices, mirroring /dev/sda vs /dev/dm-0.
    let disk_target = discover_create_target(dir.path(), "fake-sda");
    let mapper_target = discover_create_target(dir.path(), "fake-dm-0");

    // Whole-disk handle: the real LUKS member, in the label map.
    let wwn_path =
        discover_create_by_id_symlink(dir.path(), "wwn-0x5000c500deadbeef", &disk_target);

    // Decrypted-mapper aliases udev creates while the pool is unlocked. Both
    // point at the plaintext mapper and are absent from the label map, so the
    // mock's isLuks returns exit 1 for them -- exactly as cryptsetup does on a
    // headerless plaintext device. Neither is a -partN entry, so neither is
    // partition-filtered; the isLuks gate is what must skip them.
    let dm_name_path =
        discover_create_by_id_symlink(dir.path(), "dm-name-braid-disk1", &mapper_target);
    let dm_uuid_path = discover_create_by_id_symlink(
        dir.path(),
        // Real udev form: CRYPT-LUKS2-<uuid-without-dashes>-<mapper-name>. The
        // exact suffix is irrelevant to the gate; what matters is that it is
        // neither a -partN entry nor present in the label map.
        "dm-uuid-CRYPT-LUKS2-0123456789abcdef0123456789abcdef-braid-disk1",
        &mapper_target,
    );

    // Only the whole-disk handle is a real LUKS device.
    let runner = DiscoverLabelMap::new(&[(&wwn_path, "braid-disk1")]);
    let scan = discover_from_dir(&runner, dir.path());
    let members = scan.result.unwrap();

    // Exactly one member, recorded via the whole-disk handle -- not a dm alias.
    assert_eq!(members.len(), 1, "expected one member: {members:?}");
    let by_id = by_id_for(&members, "disk1");
    assert!(
        by_id.ends_with("wwn-0x5000c500deadbeef"),
        "member must be recorded via the whole-disk wwn- handle, got: {by_id}"
    );

    // The plaintext aliases are filtered silently at the isLuks gate (a nonzero
    // exit is the common non-member case and must not warn).
    assert!(
        scan.warnings.is_empty(),
        "decrypted-mapper aliases must not warn: {:?}",
        scan.warnings
    );

    // Belt-and-suspenders: the isLuks gate must stop the plaintext aliases
    // before luksDump -- discover must never probe a decrypted mapper's header.
    // (Mirrors non_luks_device_never_reaches_luks_dump.)
    let dm_dump_calls: Vec<_> = runner
        .calls()
        .into_iter()
        .filter(|(cmd, dev)| {
            cmd == "luksDump" && (dev == &dm_name_path || dev == &dm_uuid_path)
        })
        .collect();
    assert!(
        dm_dump_calls.is_empty(),
        "luksDump must not be called on a decrypted-mapper alias: {dm_dump_calls:?}"
    );
}
```

### Why these specifics (refinements over the finding's sketch)

The finding proposed a single `dm-name-braid-disk1` entry. Three refinements
make the test faithful and give it real teeth:

1. **Distinct canonical targets.** The dm aliases point at a *separate*
   placeholder (`fake-dm-0`) from the physical disk (`fake-sda`), mirroring
   `/dev/dm-0` vs `/dev/sda`. If they shared a canonical path, the alias-dedup in
   `cli/src/discover.rs#discover_from_dir_inner` would mask any erroneous
   admission (same-disk alias; `wwn` priority 0 beats `dm` priority 5
   regardless). Distinct targets make the `LabelCollision` hazard real.
2. **Both alias forms.** udev creates `dm-name-*` *and* `dm-uuid-CRYPT-*` for an
   open mapper; including both guards a regression that filters one prefix but
   not the other.
3. **`luksDump`-not-called assertion.** Pins the gate directly at the scanner
   boundary (the member never reaches header probing), independent of what the
   probe would have returned -- matching the sibling test's `.calls()` style.

## Files

- `cli/src/discover.rs` -- add the one test above to the `#[cfg(test)] mod tests`
  block, immediately after `non_luks_device_never_reaches_luks_dump`. No other
  files change; no fixture, no production-code, no `flake.nix` edits.

## Verification

1. **Test passes against current code:**
   ```
   just test-rust
   # or, focused:
   cargo test -p braid --lib discover_skips_decrypted_mapper_aliases_of_unlocked_member
   ```
2. **Confirm it actually guards the gate (fails for the right reason).** This is
   a regression test for already-correct behavior, so it is green immediately.
   To prove it has teeth, temporarily weaken the isLuks gate in
   `cli/src/discover.rs#discover_from_dir_inner` (change its
   `if raw.exit_status != 0 { continue; }` guard to `if false { continue; }`) and
   re-run: the dm aliases then flow to `luksDump`, producing `LuksDumpFailed`
   warnings, so both `scan.warnings.is_empty()` and the `luksDump`-not-called
   assertion fail. Revert the gate change.
3. No ASCII/doc/lint surfaces are touched (test comments are exempt from
   `scripts/docs/check-output-ascii.py`), so `just` lint recipes are unaffected.

## Out of scope

- **No production code change.** The filtering behavior is already correct; this
  is purely a coverage gap.
- **No VM test.** A unit test at the scanner boundary is cheaper and more
  deterministic than booting a VM, unlocking a pool, and inspecting udev output
  -- and it pins the exact `is_partition_entry` + isLuks interaction the finding
  is about.
