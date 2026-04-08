---
name: just test-repro requires the full repro- prefix
description: When invoking `just test-repro <name>`, pass the full attribute name including the `repro-` prefix as it appears in flake.nix (e.g. `just test-repro repro-btrfs-replace-interrupted-mid-flight`). The justfile does NOT strip the prefix.
type: reference
---

`just test-repro <name>` and `just test-vm <name>` pass the test name
verbatim to nix as a final attribute selector. The `reproChecks` flake
output is built by `filterAttrs` with `hasPrefix "repro-"` (flake.nix
around lines 575-577) — it KEEPS the `repro-` prefix in the filtered set.
So the attribute name you pass to `just test-repro` must be exactly the
name in flake.nix, prefix and all.

**Example:** to run `repro-btrfs-replace-interrupted-mid-flight`, type:

```
just test-repro repro-btrfs-replace-interrupted-mid-flight
```

Not `just test-repro btrfs-replace-interrupted-mid-flight` — that fails
with `flake ... does not provide attribute ... reproChecks.aarch64-darwin.btrfs-replace-interrupted-mid-flight`.

The `test-vm` checks set strips the `repro-` prefix entries (flake.nix
line 572: `filterAttrs (n: _: !(hasPrefix "repro-" n))`), so `test-vm`
test names look like `cli-recover-replace-completed` — no prefix.
