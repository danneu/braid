# Refactor: eliminate dead-code suppressions in `cli/src/test_fixtures/`

## Context

A review finding flagged a `#[allow(dead_code)]` on `DiscoverLabelMap::with_uuid`
as misleading because the method is actually live (4 call sites). Investigation
showed it is one instance of a wider pattern left by the staged "phase-4" fixture
migration: ~22 `#[allow(dead_code)]` attributes across `cli/src/test_fixtures/`,
some on genuinely-live fixtures (the suppression is stale and misleads readers)
and most on an orphaned, abandoned scaffolding file.

The root cause is dead code. The repo policy is explicit (`AGENTS.md`:
"unreleased software... Never add migration paths, compatibility shims, or legacy
support"). The ideal fix is therefore to **delete the dead code and strip the
stale suppressions**, leaving zero `dead_code` suppressions in `test_fixtures/`.
With no suppression left, this class of finding cannot recur.

Scope is deliberately limited to the `dead_code` class. The adjacent
`#[allow(unused_imports)]` suppressions on the facade re-export blocks share the
same migration root cause but are a different lint and can cascade into a
fixture-export audit; they are out of scope for this change.

## The classification (what the ~22 allows are)

The whole `test_fixtures` module is `#[cfg(test)]`-gated (`cli/src/lib.rs:59-60`),
so every call site lives in the same build config -- the compiler is the final
arbiter of live-vs-dead.

- **Orphaned file -- delete entirely (15 allows):** `cli/src/test_fixtures/add.rs`.
  It is the only submodule with **no** `pub(crate) use add::{...}` re-export in the
  facade (`cli/src/test_fixtures.rs`), so nothing outside it can name its types.
  `add`'s own production tests roll a *separate, local* `AddPlanKeyfileProbe`
  (`cli/src/add.rs:7817`) rather than the shared one -- confirming the shared
  scaffolding was never adopted.
- **Live -- strip the attribute (3 allows):**
  - `DiscoverLabelMap::with_uuid` (`test_fixtures/discover.rs:53`) -- 4 calls in
    `cli/src/discover.rs` tests.
  - `disk_member_with` (`test_fixtures/shared.rs:50`) -- ~20 calls, mostly
    `cli/src/status.rs`.
  - `RemoveParamsBuilder::yes` (`test_fixtures/remove.rs:224`) -- calls via
    `f.remove_params()...yes(...)`.
- **Dead builder methods -- delete the method (4 allows):**
  - `RemoveParamsBuilder::progress` (`test_fixtures/remove.rs:230`)
  - `RemoveMissingParamsBuilder::yes` (`test_fixtures/remove_missing.rs:260`)
  - `ReplaceParamsBuilder::passphrase_stdin` (`test_fixtures/replace.rs:397`)
  - `ReplaceParamsBuilder::progress` (`test_fixtures/replace.rs:414`)

  Note: `progress`/`yes` are defined on several builders; only the compiler
  resolves which call binds to which `fn`. Do not trust grep counts -- see the
  procedure below, which makes the compiler authoritative.

## Changes

1. **Delete `cli/src/test_fixtures/add.rs`** and remove its module declaration
   `mod add;` (`cli/src/test_fixtures.rs:120`).
2. **Prune the facade doc** in `cli/src/test_fixtures.rs` so it stops advertising
   deleted types: remove the `AddTopology` / `AddStatefulPool` + `AddPoolHandle`
   + `AddDynFs` / `AddPlanTopology` bullets (lines ~16-22) and the
   `AddParamsBuilder` token in the builders bullet (line ~109).
3. **Strip `#[allow(dead_code)]`** from the 3 live items (discover/shared/remove
   above).
4. **Delete the 4 dead builder methods** (and their `#[allow(dead_code)]`) from
   `remove.rs`, `remove_missing.rs`, `replace.rs`. These are `pub(crate)` setters
   with zero callers; the builders themselves stay live via their other methods.

End state: `rg 'allow\(dead_code\)' cli/src/test_fixtures/` returns nothing.

## Implementation procedure (compiler-arbitrated)

Match items by symbol, not line number (numbers drift). The final compile
validates every classification, so a misclassification fails loudly rather than
shipping:

1. Do change #1 + #2 (delete `add.rs`, `mod add;`, doc bullets). Compile the test
   target: `just test-rust`. A clean build proves the file was fully orphaned; a
   compile error means something referenced it -- restore and investigate.
2. Do change #3 (strip the 3 live allows) and #4 (delete the 4 dead methods).
   Re-run the **fail-closed gate** (see Verification) -- `cargo clippy ... --
   -D dead_code -D unfulfilled_lint_expectations` -- so any residual lint is a
   non-zero exit, not silent output that the wrapper recipes swallow. It enforces
   correctness:
   - **Compile error** -> a "dead" method was actually called; it was live.
     Restore it and strip its attribute instead of deleting.
   - **Gate fails on `dead_code`** -> a method whose allow was stripped is
     actually dead; delete the method (or restore a justified suppression).
   - **Gate exits 0** -> classification confirmed.

Optional safety aid for the same-name methods (`progress`/`yes`): before deleting,
temporarily convert the 4 candidate allows to `#[expect(dead_code)]` and run the
gate. The `-D unfulfilled_lint_expectations` flag turns any actually-live item
into a hard error, pinpointing it precisely before you delete. `#[expect]` is
available (toolchain >= 1.81, edition 2024) but is net-new to this repo, so use it
only transiently -- it is not part of the end state.

## Critical files

- `cli/src/test_fixtures/add.rs` -- deleted.
- `cli/src/test_fixtures.rs` -- remove `mod add;`, prune doc bullets.
- `cli/src/test_fixtures/{discover,shared,remove}.rs` -- strip live allows.
- `cli/src/test_fixtures/{remove,remove_missing,replace}.rs` -- delete dead
  builder methods.

## Verification

This is test-only fixture code with no production-behavior change, so the NixOS VM
suite is not needed (do not run the 20-30 min `just test-vm`).

**Authoritative fail-closed gate** -- the "compiler-arbitrated" guarantee depends
on this. Plain `just test-rust` (`justfile:108`) and `just clippy` (`justfile:117`)
do not pass `-D warnings`: they *print* a `dead_code` warning but still exit 0, so
they cannot prove the classification. Use instead:

- `cargo clippy --manifest-path cli/Cargo.toml --tests -- -D dead_code -D unfulfilled_lint_expectations`
  -- must exit 0. `-D` promotes those two lints to hard errors, so a
  misclassified-live item (stale allow stripped but still dead) or an unfulfilled
  `#[expect]` fails the command non-zero. In-source `#[allow(dead_code)]` elsewhere
  in the crate still overrides this CLI `-D` for its own item, so the gate fires
  only on the items this refactor touches and is not derailed by pre-existing,
  intentionally-suppressed dead code.

Then, for behavior and the surrounding checks:

- `just test-rust` -- runs the `#[cfg(test)]` unit tests; must pass.
- `rg 'allow\(dead_code\)' cli/src/test_fixtures/` -- must return nothing.
- `rg -n 'AddParamsBuilder|AddTopology|AddStatefulPool|AddPoolHandle|AddDynFs|AddPlanTopology|mod add' cli/src/test_fixtures.rs`
  -- must return nothing (facade fully de-referenced).

## Out of scope (separate pass if ever wanted)

The ~13 `#[allow(unused_imports)]` blocks on the facade re-exports
(`cli/src/test_fixtures.rs`) are the same migration leftover on a different lint.
Cleaning them can cascade into removing exported-but-unused fixtures, so handle
them as their own change.
