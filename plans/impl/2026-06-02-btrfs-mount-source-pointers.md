# Plan: fix stale `storage.nix` "See" pointers for btrfs mount config

## Context

Commit `fede2dd1` (2026-03-26, "remove braid.disks from nix module, add
braid-online lifecycle service") deleted the btrfs pool mount entry
(`fileSystems.${cfg.mountPoint}` with `noatime, skip_balance, subvolid=5`)
from `modules/braid/storage.nix`. That mount configuration now lives in the
Rust CLI: `cli/src/cmd.rs` `base_mount_options()` (the option set + its
rationale doc comment) and the `mount` invocation that joins them.

Two `Active` ADRs still point readers at `storage.nix` to find btrfs mount
configuration that is no longer there:

- `docs/design/decisions/015-hdd-defaults.md:45` -- the `noatime`/HDD-spindown
  pointer (the original finding). Born stale: the pointer was introduced in
  `403d1b07` (2026-05-22), two months *after* `fede2dd1` had already moved
  `noatime` out of `storage.nix`.
- `docs/design/decisions/001-btrfs-raid1.md:57` -- "btrfs mount configuration"
  (sibling, same root cause).

A reader following either pointer opens `storage.nix`, finds nothing
(`rg -c noatime modules/braid/storage.nix` -> 0; its only remaining
`fileSystems` entry is the auto-unlock key ramfs, not the pool), and either
concludes the doc is wrong or fails to find the real mount gate before
editing mount behavior. braid's docs discipline ("code that contradicts a
doc is drift; fix it") makes this a required correction, and its
unification-first ethos makes fixing both pointers in one pass the right
scope.

This is a pure pointer correction: no behavioral/invariant change, both ADRs
stay `Active`, no status bump or added rationale.

## Changes

### 1. `docs/design/decisions/015-hdd-defaults.md:45` (required)

Retarget the stale `storage.nix` "See" bullet to where the `noatime`
rationale actually lives (`cli/src/cmd.rs:435-436` doc comment, option set at
`:452-457`). Mirror the existing idiom of the bullet directly above it
(line 44 already reads `` `cli/src/cmd.rs` -- `base_mount_options()` ... ``).

- From: `` - `modules/braid/storage.nix` -- `noatime` rationale references HDD spindown ``
- To:   `` - `cli/src/cmd.rs` -- `base_mount_options()` sets `noatime`; rationale references HDD spindown ``

### 2. `docs/design/decisions/001-btrfs-raid1.md:57` (sibling, same root cause)

Retarget the stale `storage.nix` "See" bullet to the CLI mount path. ADR-001
is the RAID1 ADR, so name both the option set and the invocation that mounts
the array. Match the file's terse "See" style (sibling bullets list
paths/symbols, no line numbers).

- From: `` - `modules/braid/storage.nix` -- btrfs mount configuration ``
- To:   `` - `cli/src/cmd.rs` -- `base_mount_options()` and the btrfs mount invocation ``

### 3. `docs/design/decisions/015-hdd-defaults.md:15` -- deliberately unchanged

The finding also proposed editing line 15, but line 15
(`- `noatime` mount rationale references HDD spindown prevention.`) is a
Context bullet stating the *why*; it contains no file pointer and is not
stale. braid's ADRs keep the rationale narrative in Context and code pointers
in "See"; injecting a `cmd.rs` reference here would duplicate the corrected
line 45 and blur that separation. Leave it.

## Style constraints

- The two edited bullets keep the em-dash (`—`) used throughout these ADRs'
  "See" sections. The repo writing-style rule prefers ASCII (`--`) everywhere,
  *including markdown* -- the em-dash is allowed here only under the
  "surrounding file already uses the Unicode form" exception, which applies
  because 015 and 001 use `—` throughout. (This plan file itself uses ASCII
  `--`: it has no surrounding Unicode form to match.)
- This pivot edits only the two pointers above; do not retarget any other
  `storage.nix` reference here. Every other `storage.nix` doc pointer is
  outside this btrfs mount-config fix and is *not* audited for correctness by
  this plan -- that is follow-up 2's job: `003:61`, `017:124`, `018:217`,
  `019:155/160`, `002:62`, `007:119`, `010:79`,
  `docs/guides/sharing-and-permissions.md:168`. This plan makes no ownership
  claim about them. Two are already known to be stale, not correct: `002:62`
  and `007:119` -- see follow-up 2. `005-sane-defaults.md:62` is likewise
  known-misleading -- see follow-up 1.

## Critical files

- `docs/design/decisions/015-hdd-defaults.md` (line 45 edit; line 15 left as-is)
- `docs/design/decisions/001-btrfs-raid1.md` (line 57 edit)
- `cli/src/cmd.rs` -- read-only reference; `base_mount_options()` is the
  retarget anchor (do not modify)

## Verification

1. `rg -n 'storage\.nix' docs/design/decisions/015-hdd-defaults.md docs/design/decisions/001-btrfs-raid1.md`
   returns no hits (both stale pointers gone).
2. `rg -n 'base_mount_options' cli/src/cmd.rs` confirms the retarget symbol
   exists; `rg -c noatime modules/braid/storage.nix` returns 0 (old target
   really is empty).
3. `mdbook build docs` succeeds -- the doc tree still builds and
   `mdbook-linkcheck2` passes (no doc->doc cross-links touched; these "See"
   bullets are doc->source pointers, which linkcheck does not validate --
   see follow-up below).
4. No Rust/VM tests are affected (docs-only change).

## Out of scope / follow-up (do not bundle)

**1. `005-sane-defaults.md:62` -- sane-defaults ownership drift (separate
fix).** The bullet `` `modules/braid/storage.nix` -- where defaults are
applied `` is misleading: ownership is now split three ways. Option *defaults*
live in `modules/braid/options.nix` (e.g. `poolAccessGroup` default
`"storage"`, ~line 38); the `autoScrub` *timer lifecycle* is in
`modules/braid/storage.nix` (~line 57); and the mount-root *permission
application* (`root:storage 2770`) is done in Rust by `cli/src/online_state.rs`
`mark_online()` (~line 277), wired via `modules/braid/cli.nix`
(`pool_access_group`, ~line 16) -- `storage.nix:40` even comments that Rust
applies those permissions. This is a different root cause than the `fede2dd1`
mount move, so it is *not* folded into this pivot; it needs its own retarget
(likely splitting the bullet across options.nix / storage.nix /
online_state.rs).

**2. Doc->source pointers are not CI-validated (root cause of this drift
class).** ADR "See" pointers to *source files* (`cli/src/...`,
`modules/braid/...`) are not validated by CI. `mdbook-linkcheck2` only checks
cross-links *inside* `docs/`, so a doc->source pointer can rot invisibly --
exactly how these survived a two-month-old refactor. A lightweight CI guard
(assert backticked repo-path references in `docs/design/` resolve to existing
files) would prevent recurrence, but it is a separate decision with its own
tradeoffs (path-vs-symbol granularity, false positives on prose) and must not
be folded into this two-line doc fix. The one-time correctness audit of the
other `storage.nix` pointers listed under "Style constraints" belongs with
this follow-up, starting with two already-confirmed-stale ones:
`002-config-first-workflow.md:62` and `007-disk-pool-management.md:119` both
read "config export and LUKS entry generation," but config export now lives in
`modules/braid/cli.nix` (the `config.json` generator) and static LUKS entry
generation was removed with `braid.disks` in `fede2dd1` (LUKS is now opened at
runtime by `braid-online.service`).

## Follow Up

- Retarget `docs/design/decisions/005-sane-defaults.md:62` so sane-defaults ownership points at the current split across `modules/braid/options.nix`, `modules/braid/storage.nix`, and `cli/src/online_state.rs`.
- Add a separate doc-source pointer audit/guard for `docs/design/` source-file references, starting with stale `modules/braid/storage.nix` references in `docs/design/decisions/002-config-first-workflow.md:62` and `docs/design/decisions/007-disk-pool-management.md:119`.
