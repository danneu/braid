---
intent: When and how to write /// doc comments on Rust CLI pub items -- justify why an item exists at its boundary, not its signature -- with the Skip list and a Good/Bad catalog.
---

# Rust doc comments

When adding a new top-level function, type, module, trait, or
`pub`/`pub(crate)` item in the Rust CLI, add a `///` doc comment justifying
why it exists at that boundary. Capture intent, invariant, ownership, or
call-site coupling -- not the signature.

Prefer one to three lines. If removing the comment would not lose any
information a reader could not recover from the code, do not write it.

The `pub fn cmd_*` and `pub(crate) fn cmd_*` subset is CI-enforced by
`scripts/docs/check-cmd-doc-comments.py`.

Skip:

- Trait impls whose purpose is the trait (`Display`, `Debug`, `From`,
  `Default`, ...)
- Enum variants already covered by an enum-level doc
- `#[cfg(test)]` items and test fixtures

Good:

- "Shared mapper ownership classifier so planner and executor use the
  same LUKS UUID invariant."
- "Separate from `MountState` because we observe LUKS state without
  holding the pool lock."

Bad:

- "Returns mapper ownership." (restates signature)
- "Helper used by the planner." (vague)
- "Caller must ensure path is canonical." (fabricated invariant nothing
  enforces)

Rust CLI only. Nix module options use NixOS option `description` fields;
shell scripts and Python tests follow their own conventions (see
[testing.md](testing.md)).
