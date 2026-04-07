---
name: Acquire environment-side resources before writing the journal
description: In braid mutation paths, environment/preflight resource acquisition (locks, inhibitors, external handshakes) must happen before journal::write_journal, not after, so failures don't strand the user in recovery mode for non-mutation reasons.
type: feedback
---

In braid's mutating commands (`add`, `remove`, `remove-missing`, `replace`, `recover`), `pending-op.json` is the trigger for recovery mode — once it exists on disk, only `status`, `recover`, and `lock` are permitted, and the user has to run `braid recover` to clear it. So writing the journal commits the user to a recovery flow on any subsequent failure.

Rule: any preflight/environment-side resource acquisition (file locks, sleep inhibitors, dbus handshakes, external service availability checks) must happen **before** `journal::write_journal`, even if it's only a few lines apart. The journal write should land immediately before the first irreversible disk operation, as principle 3 in `docs/principles.md` already requires.

**Why:** A failure to acquire a preflight resource (e.g. logind unreachable, flock contention) is a pure environment problem with zero pool mutation. It should error cleanly with no on-disk side effects. Writing the journal first means any such failure leaves a stranded `pending-op.json`, forcing an unnecessary `braid recover` flow for what was conceptually a "command never started" failure. That's a UX regression and obscures real recovery scenarios in the journal's audit trail.

**How to apply:** When adding any new step that might fail and has no on-disk side effects (RAII guards, environment probes, logind/dbus handshakes, flock acquisition), insert it above `journal::write_journal` in the relevant `cmd_*` function. Reorder existing code if needed — the journal write is the line of no return. Inhibitor-style RAII guards bound at the same scope as the journal write are fine because Drop fires on every error path inside the function regardless of bind position.

Origin: plan review of plans/wip/bubbly-noodling-wigderson.md (issue #45 — sleep inhibitor for `braid replace`). First draft acquired the SleepInhibitor after journal::write_journal; reviewer flagged that a logind failure would strand pending-op.json. Fixed by reordering.
