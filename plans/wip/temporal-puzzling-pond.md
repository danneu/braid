# Remove redundant shutdown.target from braid-scrub

## Context

`braid-scrub` in `modules/braid/storage.nix:51-58` explicitly lists both `shutdown.target` and `sleep.target` in its `conflicts` and `before` directives. Since `DefaultDependencies=yes` (the systemd default for all services) already adds `Conflicts=shutdown.target` and `Before=shutdown.target` (confirmed in `reference/systemd/man/systemd.service.xml:115-116`), the explicit `shutdown.target` entries are redundant noise. Only `sleep.target` is a meaningful addition.

## Change

**File:** `modules/braid/storage.nix`

Lines 51-58 — replace:
```nix
      conflicts = [
        "shutdown.target"
        "sleep.target"
      ];
      before = [
        "shutdown.target"
        "sleep.target"
      ];
```

With:
```nix
      # DefaultDependencies=yes (systemd default) already provides
      # Conflicts=shutdown.target + Before=shutdown.target.
      # Only sleep.target needs explicit declaration.
      conflicts = [ "sleep.target" ];
      before = [ "sleep.target" ];
```

**File:** `tests/module/auto-scrub.py`

Lines 85-99 — update subtest label and add comment clarifying the source of each target:

Replace:
```python
with subtest("defaults: scrub service conflicts with shutdown and sleep"):
```

With:
```python
with subtest("defaults: scrub service conflicts with shutdown (default deps) and sleep (explicit)"):
```

The assertions themselves stay unchanged — `shutdown.target` still appears in the rendered unit via `DefaultDependencies=yes`, and `sleep.target` appears via the explicit Nix config. Both are correctly validated.

## Verification

`just test-vm braid-auto-scrub scrub-lifecycle` — confirms scrub service still behaves correctly (stops on sleep, stops on shutdown via default deps).
