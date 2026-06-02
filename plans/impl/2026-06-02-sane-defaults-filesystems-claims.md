# Fix fabricated `braid.shares.media` example + the latent "no fileSystems" overclaim it exposes

## Context

`docs/design/decisions/005-sane-defaults.md:32` uses `braid.shares.media`
("sets Samba config, permissions, and directory creation") as the canonical
example for the rule **"one braid option maps to many underlying options."**
No such option exists -- `rg "shares" modules/braid/*.nix` returns nothing --
and Samba is explicitly out of scope (`docs/guides/sharing-and-permissions.md:80`:
"Samba is not part of the braid module"). The doc's central "when to wrap"
example points at a feature braid does not have, which undermines trust in the
rest of the decision rules.

The accurate replacement is `braid.autoUnlock`, which genuinely fans out to
four underlying NixOS options -- one of them a `fileSystems` mount entry
(`modules/braid/storage.nix:184`). Naming that entry surfaces a **pre-existing**
latent inconsistency: ADRs 003, 017, and 018 carry blanket claims that the
module generates *no* `fileSystems` entries. That was already inaccurate the
moment `autoUnlock` shipped its USB-key mount; the new ADR 005 example would be
the first ADR to state it out loud, so a fix that touched only ADR 005 would
leave it openly contradicting three sibling ADRs -- repeating the exact
"ADR contradicts reality" failure this finding is about.

So the ideal fix is two coupled edits: (A) replace the fabricated example, and
(B) scope the blanket claims to the data pool so all four ADRs agree with the
code and with each other. The real invariant -- *nothing referencing the data
pool can block boot* -- is preserved, not weakened: the sole `fileSystems`
entry is the `autoUnlock` USB-key mount, marked `noauto`/`nofail`, which cannot
block boot and references the key device, not a data drive.

## Part A -- replace the fabricated example (`005-sane-defaults.md:32`)

Before:
```
- **One braid option maps to many underlying options** — e.g., `braid.shares.media` sets Samba config, permissions, and directory creation.
```
After:
```
- **One braid option maps to many underlying options** — e.g., `braid.autoUnlock` sets a `fileSystems` mount entry for the USB key, a `braid-auto-unlock.service`, `systemd.tmpfiles` rules, and assertions.
```

The four named targets are all real and gated on `cfg.autoUnlock.enable`:

| Claim in text                | Backing code                                          |
| ---------------------------- | ----------------------------------------------------- |
| `fileSystems` mount entry    | `modules/braid/storage.nix:184` (`/run/braid-key/mnt`) |
| `braid-auto-unlock.service`  | `modules/braid/storage.nix:198`                       |
| `systemd.tmpfiles` rules     | `modules/braid/storage.nix:48-53`                     |
| assertions                   | `modules/braid/options.nix:105-106`, `113-114`        |

### Wording decisions

- **`braid.autoUnlock`, not `braid.poolAccessGroup`.** `autoUnlock` fans out to
  four distinct options; `poolAccessGroup` only drives `users.groups`
  (`options.nix:118`) plus a runtime `pool_access_group` fixup (`cli.nix:16`) --
  a weaker "one-to-many", a better fit for the *adjacent* bullet (line 34), and
  already this doc's canonical example in the Defaults table (line 44).
- **Keep the bolded heading "underlying options"** (do not switch to "units").
  The doc reasons about NixOS *options* throughout (lines 22, 34); the four
  targets are all set via options. Only the example clause after the em-dash
  changes.
- **Keep the em-dash (`—`).** The file uses em-dashes throughout (lines 22-24,
  33-36); the AGENTS.md `--` rule governs CLI output, not prose docs, and the
  global ASCII rule exempts files already using the Unicode form.

## Part B -- scope the blanket "no fileSystems" claims (data-pool only)

The module declares exactly **one** `fileSystems` entry (verified:
`git ls-files 'modules/**' | xargs rg 'fileSystems\.'` -> single hit at
`storage.nix:184`). Insert the `data-pool` qualifier at each blanket claim, and
add the explicit USB-mount exception once, in ADR 003 (the resilient-boot ADR
that owns this invariant). The other ADRs reference the invariant; a one-word
qualifier is enough there.

Five sites across three files (all verified present):

- **`003-resilient-boot.md:26`** -- `... does not generate `boot.initrd.luks.devices`, ` -> insert `data-pool ` before `` `fileSystems` entries ``.
- **`003-resilient-boot.md:30`** -- change heading **"No build-time mount units"** -> **"No boot-blocking mount units"**; body `The module generates no ` -> `... no data-pool `fileSystems` or LUKS entries`; then append the exception sentence:
  > (The one build-time `fileSystems` entry is the optional `autoUnlock` USB-key mount at `/run/braid-key/mnt`, marked `noauto`/`nofail` so it never blocks boot and references the key device, not the pool.)
- **`017-runtime-disk-membership.md:98`** -- `no longer generates ` -> `no longer generates data-pool `fileSystems`, LUKS entries, or `btrfs-device-scan``.
- **`017-runtime-disk-membership.md:124`** -- `..., no `fileSystems`` -> `..., no data-pool `fileSystems``. (Site the upstream review missed.)
- **`018-systemd-lifecycle.md:12`** -- `must not generate ` -> `must not generate data-pool `fileSystems` or `boot.initrd.luks.devices` entries`. (Its rationale clause already says "hard boot dependencies on the data pool", so this just makes the noun match.)

Doc mentions of `fileSystems` that are **not** blanket claims and stay
untouched: `005:35` (hypothetical multi-pool scrub), `015:39` and
`internals/btrfs/luks-sector-size.md:46` (generic filesystem prose),
`018:205` (consumer-service binding), `dev/overview.md:67` (test VM config),
`guides/mounting-subvolumes.md:91-93` (user-facing "why not fileSystems").

## Verification

- `git ls-files docs README.md modules | xargs rg -n 'braid\.shares'` returns no
  matches (phantom option gone from tracked files). Note: the plain
  `rg -n 'braid\.shares' docs/` form *also* works -- ripgrep honors `.gitignore`
  on a directory walk, so it never descends into the generated `docs/book/`
  (verified: both forms return only the single pre-edit hit). The upstream
  review's claim that `rg docs/` would catch stale `docs/book/` output does not
  reproduce.
- `git ls-files 'docs/**' | xargs rg -n 'no .{0,40}fileSystems|must not generate `fileSystems`'`
  returns no *unqualified* blanket claim after the edits (every survivor reads
  "data-pool `fileSystems`").
- `mdbook build docs` succeeds (no new cross-links; confirms all four edited
  ADR pages still build and pass `mdbook-linkcheck2`).
- Docs-only change: no Rust or NixOS VM tests are affected.

## Findings triage (from the plan review)

- **Medium / "no fileSystems" contradiction -- ACCEPTED, widened.** Real and
  pre-existing. Folded in as Part B. Verification turned up a **fourth** site
  (`017:124`) the review did not list, and confirmed the module's `fileSystems`
  footprint is a single `noauto`/`nofail` USB mount, which is what lets the
  scoping preserve the invariant rather than weaken it.
- **Low / `rg docs/` matches `docs/book/` -- REJECTED, does not reproduce.**
  ripgrep respects `.gitignore` on the walk; the exact command returns only the
  real hit. Adopted the review's `git ls-files` form anyway as the canonical
  verification command (it is equivalent and tracked-only), but no bug existed.
