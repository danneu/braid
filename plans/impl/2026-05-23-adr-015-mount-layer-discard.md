# Plan: docs upgrade -- ADR-015 covers mount-layer discard

## Context

A code-review finding asked why braid does not set `discard=async` at btrfs
mount time, even though kernel >= 6.2 makes it the default for devices that
advertise discard support and the upstream btrfs docs recommend it for
SSD/NVMe. The reviewer noted that ADR-015 ("HDD defaults") rejects LUKS
`--allow-discards` as a security tradeoff, but the mount-layer knob is
orthogonal at the configuration level and the ADR is silent on it.

Verification (`/verify-issue`) concluded that the cited code is correct and
needs no behavioral change:

- `base_mount_options()` returns `noatime,skip_balance,subvolid=5`. Adding
  `discard=async` would be a no-op in braid's deployment, because without
  `--allow-discards` the dm-crypt layer never advertises discard support
  upward and silently drops any TRIMs btrfs would emit.
- Even if braid were flash-aware, kernel >= 6.2 already auto-enables
  `discard=async` on devices that report discard support, so an explicit
  mount option would only matter as an override.
- ADR-015 already accepts "No TRIM passthrough" as a tradeoff for the HDD
  target, but only names the LUKS-layer knob.

The gap is purely documentary: a reader of `cli/src/cmd.rs:429` or
ADR-015 cannot tell whether the mount-layer discard omission is part of
the same decision or an oversight. The ideal upgrade closes that loop in
the two places a future reviewer or contributor would look first.

## Scope

Two files. No behavioral change. No new files, no new ADRs, no new
internals pages.

1. `docs/design/decisions/015-hdd-defaults.md` -- the authoritative ADR.
   Expand to explicitly cover both discard knobs (LUKS `--allow-discards`
   and btrfs mount-layer `discard=async`) and cross-reference the mount
   callsite.
2. `cli/src/cmd.rs` -- the `base_mount_options()` doc comment at
   `cmd.rs:417-435`. Add one sentence that explains why no discard option
   is set and points at ADR-015.

Out of scope:

- Adding inline comments at `CryptsetupLuksOpen` / `CryptsetupLuksOpenKeyFile`
  to justify the absent `--allow-discards`. ADR-015's "See" section
  already names both sites; an inline comment would duplicate the ADR.
- Creating a dedicated `docs/internals/mount-options.md` page. The
  existing ADR is the right home; the surface here is one paragraph, not
  a whole page.
- Touching `docs/design/principles.md`. Principle 11 already points at
  ADR-015 and that link still resolves to the right answer after this
  change.

## ADR-015 changes

File: `docs/design/decisions/015-hdd-defaults.md`. Match the file's
existing em-dash style (the file already uses Unicode em-dashes; per
CLAUDE.md, in-file Unicode style is the local exception to the
double-hyphen rule).

### Context section

Expand the first bullet (currently lines 13-14, about `cryptsetup open`
omitting `--allow-discards`) to acknowledge that discard has two
independent knobs and braid's decision applies at both layers:

- Keep the existing LUKS sentence.
- Add a follow-on sentence noting that btrfs also exposes a mount-layer
  knob (`discard=async`, default since kernel 6.2 on devices that
  advertise discard support), but in braid's stack the LUKS layer gates
  it: without `--allow-discards` the mapped device never reports discard
  support upward, so the kernel default never activates and any explicit
  `discard=async` would be silently dropped.

### Tradeoffs accepted section

Rewrite the existing "No TRIM passthrough" bullet (line 28) so it covers
both layers in one place:

- Make the bullet state explicitly that braid pins discard off at both
  the LUKS layer (no `--allow-discards`) and, by consequence, at the
  btrfs mount layer (no effective `discard=async`, regardless of kernel
  default).
- Keep the existing user-visible consequence sentence ("SSDs experience
  increased write amplification...").

### See section

Add one bullet pointing at the mount-options callsite, parallel to the
existing `cli/src/cmd.rs` LUKS bullet:

- `` `cli/src/cmd.rs` `` -- `base_mount_options()` omits any
  `discard` option (relies on kernel default, which is itself gated by
  the LUKS layer).

## Code change: `base_mount_options()` doc comment

File: `cli/src/cmd.rs`, lines 417-428. The existing doc comment uses a
per-option-with-colon style and no dashes; match that style locally.

After the existing three option blurbs (`noatime`, `skip_balance`,
`subvolid=5`), add a closing paragraph (two short sentences max) that:

- States no `discard` option is set by design.
- Points at ADR-015 for the rationale and cross-references the LUKS-layer
  decision that gates it.

Example shape (not final wording -- match local prose style at write
time):

> No `discard` option is set. LUKS is opened without `--allow-discards`
> (see ADR-015), so the mapped device never advertises discard support
> upward and any mount-layer discard would be silently dropped.

## Verification

This is a docs change with no behavioral surface. Verification is
reading and link integrity, not test execution.

- **Read-through**: read ADR-015 end-to-end and confirm the two discard
  knobs are now both named and the See section lists both callsites.
- **Read-through**: read the new `base_mount_options()` doc comment and
  confirm the rationale lands without restating ADR-015 in full.
- **mdBook link check**: `mdbook build docs` runs `mdbook-linkcheck`
  (per CLAUDE.md / Decision 5). Any broken `[..](..)` link in ADR-015
  fails CI. Run locally before commit:
  ```
  mdbook build docs
  ```
- **Compile check**: `just test-rust` (or `cargo check -p braid-cli`).
  Doc-comment-only edits should compile cleanly; this catches any
  accidental syntax break from editing the rustdoc block.
- **No VM tests required**: nothing about mount or LUKS behavior
  changes, so `just test-vm` is not part of this change's verification.
  If the reviewer asks for one anyway, the closest relevant test is the
  mount path covered by the existing pool-lifecycle VM tests; they will
  remain green.

## Notes for the implementer

- Re-read ADR-015 immediately before editing -- the file is short but
  the prose style (em-dash, "Tradeoffs accepted" bullet form) must be
  matched exactly.
- Keep the ADR change additive. Do not restructure existing bullets or
  rename sections; the goal is a localized expansion, not a rewrite.
- The `base_mount_options()` doc comment change is one paragraph at
  most. Resist expanding it into a mini-essay; the ADR is the home for
  the full rationale.
- Do not touch `principles.md`, `005-sane-defaults.md`,
  `modules/braid/storage.nix`, or any other file flagged during
  exploration. They are correct and unchanged by this decision.
