---
name: Docs at contract level, not impl helper names
description: Architecture docs (principles, ADRs) describe behavioral contracts, never name internal Rust helpers like `plan_open_pool`; also check `exec` vs invoke semantics in wrappers before writing process-boundary claims
type: feedback
originSessionId: 7b9ceb57-f718-4c04-a3f0-04f2b15e2db7
---
When a plan proposes updating `docs/design/principles.md` or `docs/design/decisions/*` as part of a code change, the doc update must stay at the contract level -- not pin itself to internal implementation names or exact process semantics.

**Why:** Two concrete failures from the same plan review:

1. Plan proposed rewording Principle 12 / ADR 018 to say the CLI's `plan_open_pool` re-checks `mountpoint -q`. The user flagged this: architecture docs must not name internal helpers, because a harmless refactor (rename, inline, split) would create fake "design drift" where code and doc diverge despite unchanged behavior.

2. Plan proposed saying "the wrapper `exec`s the CLI with the flock FD inherited." The wrapper does NOT `exec` -- it invokes (`@braidBin@ "$@"; ret=$?`) and keeps running post-fixup while still holding the lock (`modules/braid/braid-wrapper.sh:78-109`). Writing `exec` would have made the doc less accurate than the code.

**How to apply:** For any plan that touches `docs/design/principles.md` or `docs/design/decisions/`:
- Phrase in terms of behavioral contracts ("`unlock` re-checks mount state under the held lock") not implementation sites ("`plan_open_pool` re-checks at `mount.rs:163`").
- Before writing process-boundary claims (exec, fork, fd inheritance, subprocess lifetime), actually read the wrapper / invocation site and confirm. Do not assume `exec` -- many shell wrappers invoke-and-continue.
- If a plan is tempted to cite a helper name in an ADR, that is a signal to re-word, not a signal to cite harder.
