---
name: Doc warnings about new behavior describe new code paths, not universal changes
description: Before treating an upstream "X can now be Y'd in version N" warning as comprehensive, identify which code path the new behavior touches and which other paths handle the same situation differently
type: feedback
---

When an upstream doc warns about new behavior in a version ("X can now be Y'd as of vN"), do not treat it as if it describes every situation where Y might be expected. Identify which code path the new feature touches. Long-lived alternative paths usually still exist and still handle the original situation the original way.

**Why:** I read `reference/btrfs-progs/Documentation/btrfs-replace.rst:41-47` saying "device replace can be interrupted by various events after v6.19 kernel ... [and] will be cancelled" and concluded that *any* interruption to a btrfs replace would be canceled on v6.19+. I designed a repro test (`tests/repro/btrfs-replace-interrupted-mid-flight.py`) that interrupts a replace via `machine.crash()` (qemu SIGKILL) and added observation locks predicting they would flip on a 6.19+ kernel bump. They will not. The v6.19 work added a new freeze/signal cancellation path inside the scrub worker loop (`fatal_signal_pending` / `try_to_freeze` / cgroup freeze checks) — that is the *only* situation the doc warning is about. The unmodified `btrfs_resume_dev_replace_async` path that handles "kernel died with replace in flight, on-disk dev_replace_item still says STARTED" is **completely separate** and is still in current torvalds master, unchanged. An unclean kill bypasses the new path entirely because there is no userspace context left to observe the freeze. Dan caught this and made me reframe the test, the findings note, and the plan.

**How to apply:** When an upstream doc says "in version N, X can now happen for Y", treat that as an *addition* to the surface area, not a replacement of existing behavior. Before designing a test or a fix around the new behavior:

1. Find the actual code path the new feature lives in (use `reference/` source, grep for the new function names, or follow the LWN/commit message for the series).
2. Identify what other code paths handle the same kind of situation. Are they unchanged? Are they affected by the new feature, or not at all?
3. Be explicit in any test, comment, or doc you write about *which* code path you are exercising. Use the function names if needed.
4. If your test uses a specific interruption mechanism, ask: "would this interruption actually trigger the new code path?" — physical kills, panics, and power loss usually don't, because the new path typically requires a live process to observe a signal/freeze.
