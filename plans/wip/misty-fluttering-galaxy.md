# Fix: Scrub ExecStop spurious failure log

## Context

The `braid-scrub` service's `ExecStop` script (`modules/braid/storage.nix:69-72`) can exit non-zero during shutdown, producing a spurious journal warning. This happens when `btrfs scrub status` fails (e.g., pool already unmounted by a race with `braid-online` stop ordering), causing `grep` to exit 1, which triggers `btrfs scrub cancel` — which also fails because there's nothing to cancel. Benign but noisy.

We want to suppress only the benign "pool already unmounted" path, not mask genuine cancel failures.

## Change

**File:** `modules/braid/storage.nix:69-71`

Add a mountpoint guard that short-circuits when the pool is already gone, then keep the existing status/cancel logic unchanged so real failures remain visible:

```diff
  ExecStop = pkgs.writeShellScript "braid-scrub-maybe-cancel" ''
+   # If pool is already unmounted during shutdown race, nothing remains to cancel.
+   ${utilLinux}/bin/mountpoint -q ${cfg.mountPoint} || exit 0
+
+   # Mounted path: keep original behavior so genuine cancel failures still surface.
    (${btrfsProgs}/bin/btrfs scrub status ${cfg.mountPoint} | ${pkgs.gnugrep}/bin/grep finished) || ${btrfsProgs}/bin/btrfs scrub cancel ${cfg.mountPoint}
  '';
```

## Verification

`just test-vm scrub-lifecycle`
