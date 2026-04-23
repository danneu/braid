---
name: Prefer localized shadowing over widening `mut` scope
description: When a refactor folds two bindings into one and mutation is localized to a seam, use `let mut x = x;` at the seam instead of making the original binding `mut` up-front
type: feedback
originSessionId: 373c6d77-4cb7-4f5b-97ff-0924022e6f88
---
When collapsing a two-binding pattern like `let x = foo(); ... let mut y = x; y.bar();` into a single variable name, prefer same-name shadowing at the mutation seam (`let mut x = x;`) over widening the original binding to `let mut x = foo();`.

**Why:** The clarity win of this kind of refactor comes from scoping mutability to the section that actually mutates. Making the top binding `mut` leaks the mutability signal across the whole function, which partially undoes the readability benefit. Raised during review of the `replace.rs` `target_membership -> final_membership` simplification (2026-04-23) — the initial plan made line 118 `let mut`, the reviewer asked for `let mut target_membership = target_membership;` at the post-commit seam instead.

**How to apply:** Any "rename two vars to one" / "drop pointless rebind" simplification. If the mutation is confined to a tail block, shadow at that block's entry; keep the earlier binding immutable. Only widen the original `let` to `let mut` if mutation is genuinely spread across the whole scope.
