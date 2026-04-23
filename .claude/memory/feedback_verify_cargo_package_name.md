---
name: Verify cargo package name before prescribing -p in commands
description: Before writing cargo test/build -p <name>, check cli/Cargo.toml; the braid CLI package is braid-cli, not braid
type: feedback
originSessionId: 59fde6fc-0396-43cd-98d2-4afe82145554
---
Before including `cargo test -p <pkg>` or similar in a plan's verification
steps, verify the actual package name from `Cargo.toml`. Don't assume it
matches the binary name or the repo name.

**Why:** The braid CLI crate's package name is `braid-cli` (per
`cli/Cargo.toml`), even though the binary is `braid` and the repo is
`braid`. A plan that says `cargo test -p braid ...` will silently fail to
target the intended crate, breaking the plan's verification path.

**How to apply:** When writing a plan's verification/testing section, if
you're about to include a `-p` flag, first read the relevant `Cargo.toml`
to confirm the package name. Or prefer `just test-rust` / the project's
own justfile recipes, which don't require knowing the exact package name.
