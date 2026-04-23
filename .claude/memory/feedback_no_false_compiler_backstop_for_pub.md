---
name: no false compiler backstop for public APIs
description: Don't justify "no new tests" by citing rustc's unused-variant/unused-field warnings on public types -- rustc does not warn on unused public enum variants or fields
type: feedback
originSessionId: d5a7d053-6c27-43f3-aece-b8bdab3676b8
---
Do not claim rustc's unused-variant / unused-field / dead-code lints as a
regression backstop when the item is `pub` (or reachable from a `pub` item in
a `pub` module). Rust's `dead_code` lint ignores public items by design.
Unused-*import* warnings still apply, but unused-*variant* warnings do not.

**Why:** On a plan to delete `LockError::Membership` (a `pub enum` variant in
a `pub mod`), I justified "no new tests" with "the compiler's unused-variant
check guards against reintroducing it." User pointed out this is false: rustc
is not a meaningful safety net for public variants, so the justification
weakened the review trail with a wrong premise.

**How to apply:** When writing a "no new tests" justification for a dead-code
removal, lead with the real point -- deleting an unconstructed variant /
unreferenced public API is non-behavioral, so there is nothing worth a
behavioral regression test. Do not invoke rustc's dead-code lint as backup
unless the item is truly private (non-`pub`, or behind a private mod/item).
Unused-import warnings are fair game on any visibility.
