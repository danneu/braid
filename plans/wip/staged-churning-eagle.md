# Plan: Move nix build I/O to tmpfs (RAM)

## Context

Running `just test-fast` (67 NixOS VM tests) saturates disk I/O (100% `%util`), causing the entire system to feel sluggish — even launching apps like foot or fuzzel takes ~10 seconds. The VM tests create qcow2 disk images and do heavy I/O during builds. Moving this I/O to RAM eliminates disk contention and SSD wear.

## Approach

Add `boot.tmp.useTmpfs = true` to the silverstone NixOS config. This mounts `/tmp` as tmpfs. The nix daemon uses `/tmp` for build scratch space (where qcow2 images are created during VM test builds).

NixOS defaults `boot.tmp.tmpfsSize` to `"50%"` of RAM = 16GB on your 32GB machine. This should be sufficient:

- Largest single test: 12GB declared (3x 4GB disks), but qcow2 is sparse (~5-10% actual usage)
- With `-j 8`, worst case ~8 tests building concurrently, but builds don't all peak simultaneously
- If a build exceeds tmpfs, it fails cleanly (ENOSPC) — no data loss

## File to modify

`/home/dan/hunk/example/hosts/silverstone/configuration.nix`

Add:

```nix
boot.tmp.useTmpfs = true;
```

That's it. One line. Default 50% size is appropriate.

## Considerations

- **Not system-wide `/tmp` risk**: NixOS already cleans `/tmp` on boot by default, so tmpfs just means it's also cleaned on reboot (same behavior)
- **RAM pressure**: 16GB tmpfs ceiling + 32GB RAM. tmpfs only consumes RAM for actual data written, so idle overhead is zero. During heavy test runs, RAM usage will increase but should stay within bounds given `-j 8`
- **If tests OOM or hit ENOSPC**: Lower `-j` in justfile or set a smaller `boot.tmp.tmpfsSize`
- **UTM host**: Don't add this to UTM config (it's a VM with less RAM). Keep it silverstone-only

## Verification

1. `nixos-rebuild switch` on silverstone
2. Confirm: `mount | grep /tmp` shows `tmpfs on /tmp`
3. Run `just test-fast` while monitoring `iostat -x 1` — disk `%util` should drop dramatically
4. Confirm system stays responsive during test runs (launch foot, fuzzel)
