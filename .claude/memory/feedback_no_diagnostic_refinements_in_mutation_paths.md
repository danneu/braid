---
name: Don't widen probe-result enums into mutation paths if the new distinction only matters for diagnostics
description: When refining an enum variant for better user-facing diagnostics, do not propagate the refinement into mutating-command code paths that match on the same enum — they may already treat the coarse state as a destructive default
type: feedback
---

When a probe/state enum needs a finer-grained classification for diagnostics
(status, doctor, error messages, TUI), do NOT widen the enum variant for
every caller. Refine only at the diagnostic call sites that actually need
the distinction; keep the coarse state for callers whose semantics depend
on the original interpretation.

**Why:** Dan rejected a plan that proposed splitting
`ConfigDiskState::PresentNotLuks` into `LuksHeaderUnreadable` +
`LuksHeaderDamaged` so `braid status` and the unlock error could
distinguish them. The split would have flowed through to
`add.rs`/`replace.rs`/`enroll_key_file.rs`, which today treat
`PresentNotLuks` as "fresh disk, route to destructive LUKS-format path".
A *recoverable* damaged-LUKS-header disk would have been silently
re-formatted via `add` or `replace`, destroying data that
`cryptsetup repair` might have saved. Dan's framing: "the current design
overloads a destructive-command input type with recovery-oriented
semantics." This conflicts with the safe-by-construction invariant.

**How to apply:** Before splitting/widening any enum that is matched in
multiple modules, list every match arm. For each consumer, ask: does this
caller currently treat the coarse state as a destructive default (format,
wipe, overwrite)? If yes, the new refinement must NOT reach that caller —
either keep the enum coarse and refine inside the diagnostic call site
(call `probe_luks_header` etc. directly when rendering), or introduce a
separate diagnostic-only type. The probe/state enum is a contract with
all consumers; widening it can silently change destructive-command
behavior even when the diff looks like it only adds variants.
