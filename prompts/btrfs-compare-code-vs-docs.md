Read btrfs docs:

- man5 https://btrfs.readthedocs.io/en/latest/btrfs-man5.html
- balance https://btrfs.readthedocs.io/en/latest/btrfs-balance.html
- filesystem https://btrfs.readthedocs.io/en/latest/btrfs-filesystem.html
- replace https://btrfs.readthedocs.io/en/latest/btrfs-replace.html
- scrub https://btrfs.readthedocs.io/en/latest/btrfs-scrub.html
- restore https://btrfs.readthedocs.io/en/latest/btrfs-restore.html
- device https://btrfs.readthedocs.io/en/latest/btrfs-device.html

And look at our own code that calls `btrfs` cli.

1. Are we using btrfs cli correctly in all scenarios?
2. Can any btrfs cli options be used to help our objective?
3. Do we have an accurate model of how btrfs works?
