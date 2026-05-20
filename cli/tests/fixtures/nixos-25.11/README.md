Golden-file fixtures captured from a nixos-25.11 VM.

To populate (or refresh) these fixtures:

```
just capture-fixtures
```

This boots a VM, sets up LUKS + btrfs RAID1, captures tool output,
and copies the results here. The `golden_nixos_25_11` cargo tests
then parse these fixtures to verify the parsers handle real output.

The `smartctl-selftest-*.json` fixtures are hand-authored. VM virtio disks
do not emit useful SMART self-test logs, so `just capture-all-fixtures` does
not regenerate them. They are still parser-critical contracts: on a
smartmontools or nixpkgs bump that changes `ata_smart_self_test_log.standard`
JSON shape, review and update these fixtures by hand before accepting the
parser contract.
