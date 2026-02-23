Capture these from a nixos-25.11 VM with LUKS + btrfs set up:

```
lsblk --json --bytes --output NAME,TYPE,SIZE,MODEL,SERIAL,UUID
btrfs --format json filesystem df <mount>
btrfs filesystem show <mount>
btrfs filesystem usage --raw <mount>
btrfs device stats <mount>
btrfs scrub status <mount>
cryptsetup status <mapper>
cryptsetup luksUUID <device>
findmnt --json --output TARGET,SOURCE,FSTYPE --mountpoint <mount>
```
