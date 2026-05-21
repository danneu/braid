# plan: extract by-id seam into its own module

## Context

`cli/src/recover.rs` and `cli/src/discover.rs` share a small cluster of `/dev/disk/by-id/` semantics but own it asymmetrically, producing a circular module dependency:

- `recover.rs:103-133` defines `pub trait ByIdResolver` and `pub struct RealByIdResolver`. Its doc claims it is a "Narrow recovery-local abstraction," but `discover.rs` consumes it through `discover_pool_members` (line 217) and `discover_from_dir{,_inner}` (lines 278, 290), plus ~16 test sites. The "recovery-local" framing is stale.
- `discover.rs:619, 638` defines `pub(crate) fn by_id_priority` and `pub(crate) fn is_partition_entry`. `recover.rs:163, 172` calls both via `discover::is_partition_entry` and `discover::by_id_priority` inside `resolve_by_id_for_underlying`.

Net: `recover -> discover` for the priority/partition helpers, `discover -> recover` for the resolver trait. Rust tolerates intra-crate cycles, but the hygiene cost is real: a reader looking for "who owns the by-id contract?" has to follow two hops, and future readers will be tempted to drop new shared helpers into whichever side they happen to be editing.

The four symbols form one coherent concept (by-id symlink enumeration, canonicalization, prefix priority, partition filtering). The fix is to give that concept its own module.

## Scope

Create `cli/src/by_id.rs` and move:

- `pub trait ByIdResolver` (from `recover.rs:108`)
- `pub struct RealByIdResolver` and its `impl ByIdResolver` (from `recover.rs:117-133`)
- `pub(crate) fn by_id_priority` (from `discover.rs:619`)
- `pub(crate) fn is_partition_entry` (from `discover.rs:638`)
- `MockByIdResolver` (from `recover.rs:3917-3960`, currently inside `#[cfg(test)] mod tests` with no visibility modifiers) -- relocated into a `#[cfg(test)] pub(crate) mod test_helpers` block inside the new `by_id.rs`. To be reachable from recover's tests module, the relocated items need explicit `pub(crate)` visibility:
  - `pub(crate) struct MockByIdResolver { ... }` -- fields stay private (the test API is the builder methods, not direct field access).
  - `pub(crate) fn with_entries<...>` and `pub(crate) fn with_canonical(...)` builder methods on the `impl MockByIdResolver` block.
  - `pub(crate) fn resolver_for(...)` standalone helper.
  - The `impl ByIdResolver for MockByIdResolver` block inherits visibility from the trait + struct; no annotation needed there.
  - Recover tests pull them in via `use crate::by_id::test_helpers::{MockByIdResolver, resolver_for};`.

Do **not** fold into `probe.rs`. The author deliberately kept `ByIdResolver` separate from `probe::Filesystem` to avoid widening a trait with 14 mock impls (`recover.rs:104-106`). A dedicated module is the right shape.

`resolve_by_id_for_underlying` (`recover.rs:143-195`) stays in `recover.rs` -- it is the recovery-specific orchestration that consumes the seam, not part of the seam itself.

## Implementation

### 1. Create `cli/src/by_id.rs`

Layout follows the `inhibit.rs` template (trait + concrete impl + tests):

- Module-level doc comment naming the concept: `/dev/disk/by-id/` symlink enumeration, canonicalization, priority, and partition filtering. Used by both `discover` (scanning attached braid-labeled disks) and `recover` (resolving stable identifiers for live pool devices).
- `pub trait ByIdResolver` -- refreshed doc comment that drops the "recovery-local" framing. Keep the rationale for staying separate from `probe::Filesystem` (the 14-mock-impl reason).
- `pub struct RealByIdResolver` and its `impl ByIdResolver`. Add a `///` intent doc on the struct (today at `recover.rs:117` it has none): production impl that reads `/dev/disk/by-id/` from the real filesystem and canonicalizes via `std::fs::canonicalize`; tests substitute their own. AGENTS.md "Doc Comments" rule applies.
- `pub(crate) fn by_id_priority` -- preserve the existing prefix table doc comment.
- `pub(crate) fn is_partition_entry`. Add a `///` intent doc (today at `discover.rs:638` it has none): filter for udev `-partN` suffixes on by-id entries so callers operate on whole-disk identifiers rather than partition aliases.
- `#[cfg(test)] pub(crate) mod test_helpers { ... MockByIdResolver ... }` -- relocated from `recover.rs:3917-3960`. Builders (`MockByIdResolver::default()`, the `resolver_for` helper at recover.rs:3959) follow.
- `#[cfg(test)] mod tests { ... }` -- move the existing `by_id_priority_ordering` test (`discover.rs:1016`) and `partition_detection` test (`discover.rs:1007`) here so unit tests live next to the helpers they exercise.

### 2. Wire the module in `cli/src/lib.rs`

Add `pub mod by_id;` between `pub mod add;` and `pub mod alert;` (alphabetical slot).

### 3. Update imports

- `cli/src/recover.rs`:
  - Replace `use crate::discover;` (line 7) with a narrower import, or drop it entirely if no other `discover::` references remain (verify -- the doc-comment references at lines 138 and 13731 do not require a `use`).
  - Add `use crate::by_id::{ByIdResolver, RealByIdResolver, by_id_priority, is_partition_entry};` (or path-qualified per site -- match the surrounding style).
  - Update `recover.rs:163, 172` from `discover::is_partition_entry(...)` / `discover::by_id_priority(...)` to the unqualified or `crate::by_id::...` form.
  - Inside the `#[cfg(test)] mod tests` block, replace the local `MockByIdResolver` definition with `use crate::by_id::test_helpers::MockByIdResolver;` (and the `resolver_for` helper, if it stays a test-fixture-shaped helper -- move it alongside the mock).
- `cli/src/discover.rs`:
  - Replace every `crate::recover::ByIdResolver` / `crate::recover::RealByIdResolver` reference with `crate::by_id::...`. This is the ~20 sites the verify step enumerated (lines 220, 280, 292, 804, 860, plus the ~16 test sites between 891 and 1747).
  - Remove the `pub(crate)` `by_id_priority` and `is_partition_entry` definitions.
  - Update the in-module call site at `discover.rs:319` (`if is_partition_entry(&name_str)`) to import from the new module.
- `cli/src/main.rs:977`: change `braid_cli::recover::RealByIdResolver` to `braid_cli::by_id::RealByIdResolver`.

### 4. Doc-comment touch-ups

- New `ByIdResolver` doc: drop "recovery-local"; describe it as the shared seam for both `discover` and `recover`. Keep the "don't fold into `probe::Filesystem`" rationale.
- Update the cross-reference comment at `recover.rs:138` from "by `discover::by_id_priority`" to "by `by_id::by_id_priority`" (or similar) so it stays accurate.
- Update the cross-reference comment at `recover.rs:13731` (inside a test doc) the same way.

## Critical files

- New: `cli/src/by_id.rs`
- Edited: `cli/src/lib.rs`, `cli/src/recover.rs`, `cli/src/discover.rs`, `cli/src/main.rs`

## Verification

- `cargo check -p braid-cli` -- structural compile must pass.
- `just test-rust` -- `by_id_priority_ordering` and `partition_detection` (both now in `cli/src/by_id.rs`'s tests block) plus all ~50 recover tests that touch `MockByIdResolver` must continue to pass identically. Behavior must not change; this is a pure code-organization move.
- Spot-check: `grep -rn "ByIdResolver\|by_id_priority\|is_partition_entry" cli/src/` -- after the change, only `cli/src/by_id.rs`, `cli/src/discover.rs`, `cli/src/recover.rs`, and `cli/src/main.rs` should appear, and recover/discover/main should reference only `crate::by_id::...` (no more `crate::recover::ByIdResolver`).
- Spot-check: `grep -n "fn partition_detection\|fn by_id_priority_ordering" cli/src/` -- both must now appear only in `cli/src/by_id.rs`, not `cli/src/discover.rs`.
- Spot-check: `grep -n "use crate::discover\|use crate::recover" cli/src/discover.rs cli/src/recover.rs` -- the cycle is broken when neither file imports the other for these symbols.

## Out of scope

- No behavior change. Same symbol semantics, same call signatures, same test surface.
- `resolve_by_id_for_underlying` stays in `recover.rs`.
- `probe::Filesystem` is not widened or modified.
- No follow-on consolidation into `probe.rs` or any "udev" / "device_path" abstraction -- that would re-open the trait-widening question the original author closed.
