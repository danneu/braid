- [ ] Show mixed-profile situation in braid status and tui
      Repro with `braid add <disk>` to a pool; there will be
      mixed profiles during the balance.
- [ ] Add test to ensure what happens when user ctrl-c balance
      during braid add. (Not sure what happens lol)
- [ ] After `braid remove` a device but it's still in config.json, braid status
      correctly labels it 'new' but the tui still says 'missing' instead of distinguishing new vs missing.
