# Plan: make `braid add`'s hard-convert balance self-document

## Context

An `/ultrareview` finding proposed switching `braid add`'s post-add balance
from the hard `-dconvert=raid1 -mconvert=raid1` to the `,soft` variant "to
save I/O." That change is **wrong** and is the exact anti-pattern documented
in `docs/internals/btrfs/balance-soft.md` (status: Active): on an
already-RAID1 pool, `soft` skips every already-raid1 chunk, so the balance
no-ops and the newly added device is left holding **zero copies** of existing
data. The hard rewrite is what physically redistributes copies onto the new
device -- that redistribution is the whole point of the post-add balance.

So the code is already correct and deliberate. The real defect is
**legibility**: the rationale lives only in an internals doc, and the finding
proves a competent reader will sit at the callsite (`add.rs`, `cmd.rs`),
reason about it, and never find that doc. Every other RAID1-restoring path
uses `,soft` (`remove_missing`, `replace`, `recover`, `pool.rs:476`), which
makes `add`'s hard choice look anomalous unless it explains itself.

This is the one improvement we judged worthwhile. It is comment-only -- no
behavior change, no test change.

## Intended outcome

A reader (human or agent) standing where they would make the switch-to-soft
edit is routed to the rationale before making it. Closes the asymmetry where
the soft helper self-documents its purpose but the hard path does not.

## The change

### 1. Primary -- comment at the variant-choice site (`cli/src/add.rs`)

In the preview/plan `steps()` builder, the `total_after >= 2` block (currently
~`add.rs:762-771`) emits `CmdRequest::BtrfsBalanceRaid1`. This is where the
hard-vs-soft choice is actually made and where the finding author was reading.

**The wording must not overclaim the no-op.** This callsite serves *both*
existing-pool growth cases (it lives in the `pool_was_mounted` branch and only
gates on `total_after >= 2`):

- **1 -> 2 add** (existing pool was a single-disk `mkfs -d single -m dup`):
  existing chunks are `single`/`dup`, none are raid1, so `,soft` would *convert*
  them, not skip them -- hard and soft are equivalent here.
- **2+ -> 3+ add** (existing pool already raid1): every existing chunk is
  already raid1, so `,soft` skips them all and no-ops, leaving the new disk
  empty. Only the hard rewrite redistributes copies onto it.

So the no-op/skip claim must be scoped to the already-raid1 (2+ -> 3+) case;
hard is *required* there and *equivalent to soft* for 1 -> 2. Add a `//` comment
directly above the `commands: vec![...]`, e.g.:

```rust
// HARD convert, not ,soft. When growing an already-raid1 pool (3rd+ device)
// every chunk is already raid1, so only a hard rewrite redistributes their
// copies onto the new device -- ,soft would skip them all and leave it empty.
// (A 1->2 add converts the existing single chunks either way.) See
// docs/internals/btrfs/balance-soft.md.
```

(Wording illustrative; keep it ASCII and `--`-style per CLAUDE.md.)

### 2. Secondary -- contrast the two executor helpers (`cli/src/pool.rs`)

`pool_balance_raid1` (~`pool.rs:349`) has a bare doc comment ("Balance pool to
RAID1 with progress display."), while its sibling `pool_balance_raid1_soft`
(~`pool.rs:397`) already explains *its* purpose and when it applies. That
asymmetry is the same root cause. Augment the hard helper's `///` to name it
as the hard convert and explain that `add` uses it so the rewrite redistributes
copies onto a newly added device *when the pool was already raid1* (the case
where `pool_balance_raid1_soft` would skip every chunk and no-op). Point to the
doc and contrast `pool_balance_raid1_soft`. Keep the same no-op scoping as the
`add.rs` comment -- do not imply soft always no-ops. Match the ~2-3 line scale
of the existing soft-helper doc.

## Explicitly out of scope (considered, deferred)

- **Regression test pinning hard-not-soft.** Defensible and house-style
  consistent (mirrors the soft-pinning asserts at `pool.rs:1152`,
  `remove_missing.rs:1489`), but lower leverage: the rationale already lives in
  a doc, and a switch to soft already breaks existing `add` tests via
  `_ => MissingMock` at `add.rs:5157`. The comment stops the change *before*
  it is made; the test only fires after. Skip unless the user wants the
  symmetric guard.
- **`docs/commands/add.md` step 6 wording.** Minor user-facing polish; not the
  source of the confusion (the finding was reasoned from code, not the guide).

## Files

- `cli/src/add.rs` -- comment in the `total_after >= 2` plan step (~762-771).
- `cli/src/pool.rs` -- augment `pool_balance_raid1` doc comment (~349).

No changes to `cmd.rs` (pure arg-mapping layer; "why" belongs at the planning
site, not the encoder, per the project's doc-comment guidance).

## Verification

- `git diff` shows only added comment/doc-comment lines; zero changes to any
  expression, `CmdRequest` variant, or control flow.
- `just test-rust` still passes unchanged (sanity check that the comment
  edits did not disturb compilation; no test should change).
- Convention check: the cited path `docs/internals/btrfs/balance-soft.md`
  exists and the comment style matches existing in-code doc citations (e.g.
  `cli/src/status.rs:4294`, `cli/src/discover.rs:171`).
