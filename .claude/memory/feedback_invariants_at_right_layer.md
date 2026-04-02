---
name: Enforce invariants at the layer that owns them
description: When hardening a fail-open path, put each guard at the layer that owns the invariant — don't push probe-level failures into consumer-level validation
type: feedback
---

Enforce invariants at the layer that owns them, not at consumers.

**Why:** When a mounted pool is missing its FSID, that's a probe invariant violation — catching it in `classify_braid_disk_fsid` (a consumer) turns it into a misleading disk-specific validation error, hides the real bug, and weakens the "mounted pool state is authoritative" boundary. Dan flagged this when the plan proposed handling `pool.fsid == None` in the classification layer instead of in `probe_pool`.

**How to apply:** When fixing a fail-open path that spans multiple layers, trace each missing value back to where it should have been populated. Make that layer reject the bad state. Consumers should only guard their own inputs (e.g., the device-side UUID from a command they ran), not re-validate upstream invariants.
