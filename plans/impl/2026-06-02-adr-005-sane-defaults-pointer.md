# Plan: fix stale ADR-005 sane-defaults pointer and decision rule

> Follow-up 1 from `plans/impl/2026-06-02-btrfs-mount-source-pointers.md`
> (`## Follow Up`, first bullet). Planning only -- this plan does not implement.
>
> Round-1 review broadened scope (still docs-only, still ADR-005 only): beyond
> the "See"-section pointer, also fix the adjacent stale Decision sentence
> (line 16) and complete the scrub-lifecycle pointer with
> `braid-scrub-resume-trigger`.

## Context

`docs/design/decisions/005-sane-defaults.md` is the `Active` ADR for braid's
"sane defaults." Two stale spots in it now contradict the current code and the
governing principle. Both fixes are pure docs edits in this one file -- ADR
stays `Active`, no status bump, no behavioral/invariant change.

**1. Decision sentence (line 16).** The opening of the "Decision" section reads:

```
Braid sets opinionated defaults for the underlying NixOS options using `lib.mkDefault`. Users override them with normal NixOS config — no braid-specific wrapper options needed.
```

This states a one-sided rule ("`lib.mkDefault`, no wrapper options needed")
that contradicts:

- The ADR's own two subsections immediately below ("When to use mkDefault" /
  "When to wrap in a braid option") and its "Defaults applied" table, whose
  examples `braid.autoScrub` and `braid.poolAccessGroup` *are* `braid.*`
  wrapper options.
- The governing principle, `docs/design/principles.md#7-sane-defaults` (Principle 7, "Sane
  defaults"), which already states the correct dual rule: "Use `lib.mkDefault`
  for simple pass-through defaults on stable NixOS options. Wrap in a `braid.*`
  option when the feature is inside braid's product boundary and benefits from
  lifecycle control, discoverability, or a unified config surface -- even if
  the mapping is 1:1."

Line 16 is the outlier. The fix aligns it with the principle and the
subsections; `principles.md` is already correct and is **not** edited.

**2. "See" pointer (line 62).** The "See" section points readers at the code
with a single bullet:

```
- `modules/braid/storage.nix` — where defaults are applied
```

That pointer is misleading. Ownership of the table's three defaults is split
three ways, and `storage.nix` is the wrong single anchor for two of them:

- The default *values* are **declared** in `modules/braid/options.nix`, not
  `storage.nix`.
- `storage.nix` **realizes only `autoScrub`** (the scrub lifecycle units); it
  applies no permissions. Its own comment says so -- the comment above the mount-point `systemd.tmpfiles.rules` in `modules/braid/storage.nix` reads
  "Permissions are set by Rust post-unlock lifecycle fixups (root:poolAccessGroup
  2770)."
- The `poolAccessGroup` permission (`root:<group> 2770`) is **applied in Rust**
  by `cli/src/online_state.rs` `mark_online()`.

A reader following the current pointer finds the scrub timer but no option
declarations and no permission code -- plus a comment redirecting them
elsewhere. Same doc-drift class fixed for ADR-001/015 in `b3b068c5`.

## Verified current ownership

| Default (ADR table) | Declared (default value) | Realized / applied |
|---|---|---|
| `braid.autoScrub.enable` (`true`) | `braid.autoScrub` in `modules/braid/options.nix` | the scrub lifecycle units in `modules/braid/storage.nix`, all gated on `cfg.autoScrub.enable` and bound to `braid-online.service`: `systemd.timers.braid-scrub`, `systemd.services.braid-scrub`, `systemd.services.braid-scrub-resume-trigger` |
| `braid.autoScrub.interval` (`"monthly"`) | `braid.autoScrub.interval` in `modules/braid/options.nix` | `OnCalendar = cfg.autoScrub.interval` in `systemd.timers.braid-scrub` (`modules/braid/storage.nix`) |
| `braid.poolAccessGroup` (`"storage"`) | `braid.poolAccessGroup` in `modules/braid/options.nix` (group also created via `users.groups`) | `mark_online()` in `cli/src/online_state.rs` (`chown root:<group>` + `chmod 2770`); group value bridged via `pool_access_group = cfg.poolAccessGroup` in `modules/braid/cli.nix` into the CLI config JSON |

Confirmed by direct reads plus `rg -n 'poolAccessGroup|pool_access_group'
modules/ cli/`, `rg -n 'autoScrub' modules/ cli/`, `rg -n 'mark_online' cli/`.
No other module or Rust source realizes these defaults.

**Decision-sentence drift (line 16)** confirmed against `docs/design/principles.md#7-sane-defaults`
(read-only): the principle already states the dual `mkDefault`/wrap rule, which
ADR-005:16 contradicts.

## Changes (docs-only, two edits in one file)

**File:** `docs/design/decisions/005-sane-defaults.md`. Both are exact-string
replacements, so line numbers are not load-bearing.

### Edit A -- Decision rule (line 16)

Replace exactly:

```
Braid sets opinionated defaults for the underlying NixOS options using `lib.mkDefault`. Users override them with normal NixOS config — no braid-specific wrapper options needed.
```

with:

```
Braid sets opinionated defaults two ways: `lib.mkDefault` for simple pass-through defaults on stable NixOS options, and a `braid.*` wrapper option when the feature is inside braid's product boundary and benefits from lifecycle control, discoverability, or a unified config surface — even if the mapping is 1:1. The two cases below say which applies.
```

Mirrors `principles.md#7-sane-defaults` and forward-references the existing "When to use
mkDefault (don't wrap)" / "When to wrap in a braid option" subsections (the
override mechanic is already covered at the current line 26, so it is not
repeated here).

### Edit B -- "See" pointer (line 62)

Replace the single bullet with three focused bullets -- one per ownership role
(option defaults / scrub lifecycle / permission application). Leave the second
"See" bullet (the `003-resilient-boot.md` cross-link, line 63) untouched.

Replace exactly:

```
- `modules/braid/storage.nix` — where defaults are applied
```

with:

```
- `modules/braid/options.nix` — declares the option defaults (`braid.autoScrub`, `braid.poolAccessGroup`)
- `modules/braid/storage.nix` — realizes `braid.autoScrub` into the scrub lifecycle units (`braid-scrub` timer/service and `braid-scrub-resume-trigger`), all bound to `braid-online.service`
- `cli/src/online_state.rs` — `mark_online()` applies the mount-root permissions from `braid.poolAccessGroup` (`root:<group> 2770`)
```

### Style / scope notes for the implementer

- **Keep the em-dash (`—`)** in both edits. The repo writing-style rule prefers
  ASCII `--`, but ADR-005 uses `—` throughout (lines 16, 62, 63), so the
  "surrounding file already uses the Unicode form" exception applies -- the same
  call `b3b068c5` made for ADR-015/001. Do not convert. (This plan file itself
  uses ASCII `--`: it has no surrounding Unicode form to match.)
- **`principles.md#7-sane-defaults` is the alignment target for Edit A, read-only.** It
  already states the correct rule; do not edit it.
- **`cli.nix` is deliberately not a fourth "See" bullet.** It only bridges the
  group value into the config JSON (via `pool_access_group = cfg.poolAccessGroup` in `cli.nix`); adding it would bloat a terse
  pointer. The three-way split (options.nix / storage.nix / online_state.rs)
  matches the impl plan's stated target; the bridge detail lives in this plan's
  ownership table.
- **The mount-root tmpfiles dir (`systemd.tmpfiles.rules` in `storage.nix`) is intentionally omitted**
  from the storage.nix bullet -- it is not one of the table's three defaults.
- Line shifts: Edit A is one line -> one line (no shift); Edit B is one line ->
  three lines (+2). No shipped doc cross-links ADR-005 by line number (mdBook
  links are line-less), so the shift is harmless. The impl plan's historical
  `:62` reference is a committed plan file and is not edited.

## Test coverage

Docs-only edit -- no new behavioral tests. The plan's existing-behavior claims
are already covered:

- `autoScrub` lifecycle units (timer + services gated on the enable flag):
  `tests/module/auto-scrub.py`.
- `poolAccessGroup` permission application (`root:<group> 2770`):
  `cli/src/online_state.rs` unit test `mark_online_applies_pool_access_group_without_lifecycle`,
  and end-to-end in `tests/module/add-bootstrap.py`.

## Out of scope (do not bundle)

- The broader doc->source pointer audit/guard (impl plan follow-up 2), and the
  other stale `storage.nix` pointers it owns (in ADR-002, ADR-007, ADR-003,
  ADR-017, ADR-018, ADR-019, ...). This plan makes no correctness
  claim about them.
- Any other ADR.
- `docs/design/principles.md` -- already states the correct rule (`#7-sane-defaults`); read
  only, not edited.
- The pre-existing `just check-docs` failure on
  `docs/design/decisions/010-toolchain-pinning.md#upgrading-tools` (unrelated escaped-link
  issue). Not caused by, and not fixed by, this change.
- No formatters (`cargo fmt`, `just fmt`, mdbook formatters, etc.).

## Verification

1. **Decision rule fixed (Edit A):**
   ```
   rg -n 'no braid-specific wrapper options needed' docs/design/decisions/005-sane-defaults.md   # expect: no match
   rg -n 'simple pass-through defaults' docs/design/decisions/005-sane-defaults.md                # expect: one match
   ```
2. **Stale pointer phrasing gone, storage.nix correctly retained (Edit B):**
   ```
   rg -n 'where defaults are applied|modules/braid/storage.nix' docs/design/decisions/005-sane-defaults.md
   ```
   Expected: **no** match for `where defaults are applied`; **exactly one** match
   for `modules/braid/storage.nix` -- the new scrub-lifecycle bullet. (Unlike
   ADR-001/015, storage.nix legitimately remains here because it owns the scrub
   lifecycle, so a storage.nix hit is success, not failure.)
3. **New owners + resume-trigger present (Edit B):**
   ```
   rg -n 'modules/braid/options.nix|cli/src/online_state.rs' docs/design/decisions/005-sane-defaults.md   # expect: two matches
   rg -n 'braid-scrub-resume-trigger' docs/design/decisions/005-sane-defaults.md                          # expect: one match
   ```
4. **Docs build:**
   ```
   nix develop .#docs -c mdbook build docs
   ```
   Expected: succeeds (the existing mdBook preprocessor version warning is
   pre-existing and unrelated). The edits change only prose and doc->source
   pointers -- no doc->doc cross-links are touched -- so `mdbook-linkcheck2`
   behavior is unaffected.
5. **No code/tests affected:** docs-only change; no Rust or NixOS VM test
   exercises this file.

## Critical files

- `docs/design/decisions/005-sane-defaults.md` -- the only file edited (Edit A:
  line-16 sentence; Edit B: line-62 bullet -> three bullets).
- `modules/braid/options.nix`, `modules/braid/storage.nix`,
  `cli/src/online_state.rs`, `modules/braid/cli.nix`,
  `docs/design/principles.md` -- read-only anchors (do not modify).
