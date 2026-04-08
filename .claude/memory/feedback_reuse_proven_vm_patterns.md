---
name: Reuse proven VM test patterns instead of inventing
description: When a new VM test needs a setup primitive (missing disk, degraded mount, ENOSPC, etc.), find and copy the existing canonical pattern from tests/cli or tests/repro before writing fresh setup code
type: feedback
---

When designing a new NixOS VM test that needs an environmental setup primitive — making a disk missing, forcing a degraded mount, filling a pool to ENOSPC, hot-unplugging a device, creating single-profile chunks, etc. — locate the existing canonical pattern in `tests/cli/`, `tests/repro/`, or `tests/hw/` and copy it line-for-line, rather than inventing a new mechanism in the new test.

**Why:** Reviewer feedback during a planning round flagged that a new `remove-missing-inhibits-suspend` VM test was specifying its own "force a disk missing" mechanism when `tests/cli/braid-remove-disk.py` and `tests/repro/degraded-soft-balance.py` already had a proven pattern (`umount` → `cryptsetup close braid-diskN` → `mount -o degraded`) and a reusable `get_missing_devid()` helper. Inventing a new mechanism risks: (1) flapping tests because the mechanism is not yet hardened, (2) divergence between tests that should behave identically, (3) wasted effort re-discovering edge cases the existing pattern already handles.

**How to apply:** During plan phase for any new VM test, run an Explore agent (or grep) to identify existing tests that use the same setup primitive. Reference those test files and helper function names directly in the plan, and instruct the implementation to lift them. Only invent a new mechanism if no existing one exists, and call that out explicitly as a deliberate choice. This applies to all VM test setup primitives, not just missing-disk simulation.
