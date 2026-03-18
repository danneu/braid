- [ ] how to move a braid disk pool from one machine to another?
- [ ] Sound some system beep when degraded via automount
- [ ] Braid status should show the by-id so user can use it for other cli stuff.
- [ ] Show mixed-profile situation in braid status and tui
      Repro with `braid add <disk>` to a pool; there will be
      mixed profiles during the balance.
- [ ] Add test to ensure what happens when user ctrl-c balance
      during braid add. (Not sure what happens lol)
- [ ] After `braid remove` a device but it's still in config.json, braid status
      correctly labels it 'new' but the tui still says 'missing' instead of distinguishing new vs missing.
- [ ] Close luks mapper on 'braid add' failure

  Title: braid add doesn't clean up LUKS mappers on failure

  Description:

  When braid add opens a LUKS device but fails before completing (e.g.
  the "braid-labeled but no btrfs superblock" check), the LUKS mapper is
  left open. This blocks subsequent operations like wipefs -a on the
  underlying device with "Device or resource busy".

  Repro:

  $ sudo braid remove bbb
  $ sudo braid add bbb ccc
  LUKS opened: /dev/disk/by-id/wwn-0x5000c500c095dc33 → braid-bbb
  error: disk 'bbb' is braid-labeled but contains no btrfs superblock...

  $ sudo wipefs -a /dev/disk/by-id/wwn-0x5000c500c095dc33
  wipefs: error: probing initialization failed: Device or resource busy

  Workaround: sudo cryptsetup close braid-bbb

  Expected behavior: braid add should close any LUKS mappers it opened
  if it exits with an error.
