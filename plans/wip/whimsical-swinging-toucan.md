# Fix: braid-lock-umount-busy test race condition

## Context

The `braid-lock-umount-busy` VM test fails because `braid lock` succeeds (exit 0) when it should fail. The `tail -f` process backgrounded on line 45 hasn't opened the file yet when `umount` runs, so `umount` succeeds and the test assertion on line 50 fires.

## Fix

**File:** `tests/cli/braid-lock-umount-busy.py` lines 44-46

Capture the `tail` PID and wait until `lsof` confirms that exact PID has the file open:

```python
# Hold the mount busy with tail -f in the background
pid = machine.succeed("nohup tail -f /mnt/storage/test.txt > /dev/null 2>&1 & echo $!").strip()
# Wait until that specific tail process has the file open
machine.wait_until_succeeds(f"lsof -t /mnt/storage/test.txt | grep -qx {pid}", timeout=10)
```

No changes to `.nix` — `pkgs.lsof` is already in systemPackages.

Also update the `pkill` in test 2 to use the captured PID for consistency (though not strictly required since there's only one `tail`).

## Verification

`just test braid-lock-umount-busy`
