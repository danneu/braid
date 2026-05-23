# Plan: surface and reframe the `block-group-tree` mkfs pin

## Context

braid pins `-O block-group-tree` on every `mkfs.btrfs` invocation
(`cli/src/cmd.rs:125-135`, with argv enforcement at `cmd.rs:687-704`).
A dedicated ADR exists at
`docs/design/decisions/027-mkfs-block-group-tree.md` and is listed in
the mdBook TOC (`docs/SUMMARY.md:72`), but:

- The Rust doc comments and tests do not name ADR-027, breaking the
  citation convention used elsewhere in the CLI
  (`alert.rs:67`, `add.rs:2698,5502`, `doctor.rs:1476`, `online_state.rs:249`,
  `recover.rs:11041,11115`, `main.rs:491,496,807`, etc.).
- The operator-facing `docs/commands/add.md` does not mention the
  feature pin at all, so an operator wondering "what btrfs features
  does braid use?" has no entry point.
- ADR-027 itself is one-way: it names `cli/src/cmd.rs` and the VM test
  but does not link the exact code site or tests.
- The current framing leans on "btrfs-progs 6.19 flips the default" --
  a forward-looking concern that loses meaning once NixOS 26.05 lands.
  The durable framing is "braid currently targets nixos-25.11's
  btrfs-progs 6.17.1, but explicitly passes `-O block-group-tree` so
  pools created with that toolchain get the same `block-group-tree`
  bit that the nixos-26.05-era btrfs-progs 6.19.1 enables by
  default."

### Scope of the invariant

`mkfs.btrfs` starts from `btrfs_mkfs_default_features` and `-O <name>`
only ORs (or with `^name`, clears) the named feature bit
(`reference/btrfs-progs/mkfs/main.c:1418`,
`reference/btrfs-progs/common/fsfeatures.c:364-372`). braid's `-O
block-group-tree` therefore pins only that one feature bit -- the rest
of the feature set is still whatever the linked btrfs-progs defaults
to. All wording in the plan, ADR, code comments, test preambles, and
operator docs must reflect that narrow scope. Do not claim the whole
feature set or on-disk format is stable across toolchain versions.

### Version-specific reason

The explicit pin exists because braid's current stable baseline is
nixos-25.11 with btrfs-progs 6.17.1, while the nixos-26.05-era
btrfs-progs 6.19.1 default set turns `block-group-tree` on. ADR-027
should say this directly so readers understand why braid singles out
one btrfs default: this is an intentional 25.11 -> 26.05 bridge for
that one feature bit, not a broad mkfs-default freeze.

The goal is to (a) reframe the rationale to that narrow, durable
invariant, (b) cross-link code/tests/ADR/operator-docs so any one of
the four entry points reaches the others, and (c) give operators a
one-line pointer.

## Approach

One reframe, four cross-references. Bidirectional: code <-> ADR,
tests <-> ADR, operator docs -> ADR.

### 1. Reframe ADR-027 (`docs/design/decisions/027-mkfs-block-group-tree.md`)

Rewrite the `## Context` and `## Decision` sections to lead with the
durable, narrowly scoped framing:

- Lead: braid pins the `block-group-tree` choice/presence at mkfs
  time so nixos-25.11's btrfs-progs 6.17.1 creates pools with the
  same `block-group-tree` bit that the nixos-26.05-era btrfs-progs
  6.19.1 default set enables. The rest of the feature set still tracks
  whatever btrfs-progs defaults to.
- Supporting context: kernel 6.1+ supports it (kept); btrfs-progs
  6.19+ defaults it on (mentioned as the upstream/default-set reason
  for matching this one 26.05-era behavior, not as a claim that braid
  pins every mkfs default).
- Keep the `compat_ro` / rescue-boot note in `## Notes`.

Add a `## Where this is enforced` section with code-span backlinks.
**Use code spans, not relative Markdown links** for paths outside
`docs/` -- `just check-docs` rejects any `[...](../../...)` link
because it breaks in rendered mdBook output
(`justfile:238-244`). Pattern:

- `` `cli/src/cmd.rs` `` -- `MkfsBtrfs` / `MkfsBtrfsRaid1` argv
  builders.
- `` `cli/src/cmd.rs` `` -- `mkfs_btrfs_single_generates_correct_argv`
  and `mkfs_btrfs_raid1_generates_correct_argv` unit tests assert the
  exact argv.
- `` `tests/module/mkfs-block-group-tree.{nix,py}` `` -- VM test
  asserts the on-disk feature bit.

Avoid hard-coded line numbers in the ADR (they rot); name the items.

### 2. Refresh `cli/src/cmd.rs` doc comments (lines 125-135)

Update the two doc comments on `MkfsBtrfs` and `MkfsBtrfsRaid1` to
match the narrowed reframe and cite ADR-027 once, following the
dominant `ADR-NNN` convention (see `add.rs:2698`, `add.rs:5502`,
`doctor.rs:1476`, `remove.rs:2256`). Keep them to two or three lines
each.

Form:

- One line of what the argv does (kept).
- One line of why (braid pins the `block-group-tree` feature
  explicitly so nixos-25.11's btrfs-progs 6.17.1 gets the
  26.05-era btrfs-progs 6.19.1 default for that one bit; do not claim
  the rest of the feature set is pinned).
- `ADR-027` reference.

### 3. Refresh test preambles

Four locations, all currently silent on ADR-027. Both Rust unit
tests must use the exact labels `// Intent:` / `// Why it exists:`
/ `// Scenario:` documented at `docs/dev/testing.md:11-22` -- not
short or block-comment variants.

- `cli/src/cmd.rs:~2553-2581` --
  `mkfs_btrfs_raid1_generates_correct_argv`. Uses the contiguous
  `//` form but with a short `// Why:` label (`cmd.rs:2555`).
  Normalize `// Why:` to `// Why it exists:`, reframe that line
  to the narrow invariant, and append the `ADR-027` reference.
- `cli/src/cmd.rs:~2583-2608` --
  `mkfs_btrfs_single_generates_correct_argv`. Currently uses a `/*
  ... */` block-comment preamble that violates
  `docs/dev/testing.md:11`'s "contiguous block of `//` line
  comments" rule. Convert to the exact `// Intent:` / `// Why it
  exists:` / `// Scenario:` labels, reframe the rationale to the
  narrow invariant, and add the `ADR-027` reference.
- `tests/module/mkfs-block-group-tree.py` -- the VM test's
  "Why it exists" line. Reframe to the narrow invariant ("braid pins
  the `block-group-tree` bit specifically; rest of the feature set
  still tracks btrfs-progs defaults") and append the `ADR-027`
  reference.
- `tests/module/mkfs-block-group-tree.nix:1-11` -- the `.nix` file
  has its own preamble with the old framing ("braid pins mkfs.btrfs
  feature flags explicitly so new pool feature bits do not depend on
  nixpkgs' btrfs-progs default set"). Reframe to the same narrow
  invariant and append the `ADR-027` reference.

### 4. Add an operator-facing pointer in `docs/commands/add.md`

In the existing "What happens under the hood" list, extend step 4
(currently "creates a btrfs filesystem (RAID1 if 2+ disks, single if
1 disk)") with a brief parenthetical:

- Mention that braid pins the `block-group-tree` feature explicitly
  so that bit is visible and stable across toolchain versions. Do
  not claim the broader on-disk format is stable.
- Link `[ADR-027](../design/decisions/027-mkfs-block-group-tree.md)`.
  This is an intra-`docs/` link; `just check-docs`'s escape rule
  does not apply.
- Keep the concrete nixos-25.11 btrfs-progs 6.17.1 -> nixos-26.05-era
  btrfs-progs 6.19.1 explanation in ADR-027, not in the command-doc
  sentence. The command page should point readers to the ADR for why
  this one btrfs default is singled out.

One sentence, no new section. Mirrors the rest of the doc's terse
cookbook tone (README-style per AGENTS.md "User Guide").

## Files to modify

- `docs/design/decisions/027-mkfs-block-group-tree.md` -- reframe +
  `## Where this is enforced` section (code-span backlinks only).
- `cli/src/cmd.rs` -- two doc comments at lines 125-135; two unit
  test preambles around lines 2553-2608 (including converting the
  single-disk test from a `/* */` block to the contiguous `//`
  form).
- `tests/module/mkfs-block-group-tree.py` -- preamble reframe + ADR
  citation.
- `tests/module/mkfs-block-group-tree.nix` -- preamble reframe + ADR
  citation.
- `docs/commands/add.md` -- one-sentence extension to step 4 (line 87).

No code behavior changes. No flake/test infrastructure changes. The
argv and the VM-test feature-bit assertion remain identical.

## Verification

Doc/comment-only; no new behavior to exercise. The existing argv
unit tests and VM feature-bit test are the right behavioral
coverage. Verification:

- `just test-rust` -- confirms `mkfs_btrfs_*_generates_correct_argv`
  still pass after the preamble edits and the `/* */` -> `//`
  conversion. Required because preambles live inside `#[test]`
  functions and a typo breaks compilation.
- `just test-vm braid-module-mkfs-block-group-tree` -- confirms the reframed VM
  test preamble (both `.nix` header and `.py` preamble) compiles and
  the test still asserts the feature bit.
- `just check-docs` -- rejects Markdown links that escape `docs/`
  (`justfile:238-244`); also enforces `SUMMARY.md` / doc-table
  consistency. Run this *before* `mdbook build` so a stray
  `[...](../../cli/src/cmd.rs)` is caught at the source-tree level.
- `nix develop .#docs -c mdbook build docs` -- mdbook-linkcheck (per
  `docs/book.toml`, Decision 5) validates the new
  `docs/commands/add.md` -> ADR-027 cross-link and any intra-`docs/`
  link in the rewritten ADR. CI runs this too; running locally before
  commit avoids the round-trip.
- Manual read of ADR-027 after the reframe to confirm the narrowed
  framing (one specific feature bit, not the whole feature set) and
  that it explicitly explains the nixos-25.11 btrfs-progs 6.17.1 ->
  nixos-26.05-era btrfs-progs 6.19.1 bridge without "upcoming
  default" wording.

## Out of scope

- No change to the argv, no other mkfs features added or removed.
- No new feature flags introduced.
- No edits to other ADRs.
- No edits to README.md (the README does not currently enumerate
  mkfs features; adding one is scope creep for a low-severity finding).
