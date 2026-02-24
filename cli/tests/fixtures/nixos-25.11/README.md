Golden-file fixtures captured from a nixos-25.11 VM.

To populate (or refresh) these fixtures:

```
make capture-fixtures
```

This boots a VM, sets up LUKS + btrfs RAID1, captures tool output,
and copies the results here. The `golden_nixos_25_11` cargo tests
then parse these fixtures to verify the parsers handle real output.
