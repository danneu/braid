# Un-bundle systemd and util-linux from braid's pinned toolchain

## Context

braid pins its own `nixpkgs` and bundles known-good builds of the CLI tools it
shells out to, so version drift in those tools' human-readable output can't break
braid's parsers. That rationale is sound for the *fragile-text* tools
(btrfs-progs, cryptsetup, smartmontools, nut, ethtool) and stays.

`systemd` and `util-linux` are the exception. braid consumes them only through
*stable contracts*, not fragile text (see the safety gate below), so pinning them
adds toolchain inconsistency without any parse-stability benefit. **That
classification honesty -- not closure size -- is the primary justification for
this change.** The host's `nixos-26.05` already ships release-compatible
`systemd`/`util-linux`.

Closure size is a secondary, bounded bonus, and the original motivation's
"~429 MiB switch-closure win" does not survive the closure mechanics: that figure
measured the wrong delta. The deployed duplication is dominated by braid's pinned
`glibc` + the five still-pinned tools + `braid-cli-unwrapped` -- inherent to the
no-follows design and unaffected by un-pinning one tool. `glibc` is pulled by
`braid-cli-unwrapped` and by every still-pinned tool, so it cannot drop;
`systemd-minimal-libs` is pulled by btrfs-progs/nut/smartmontools, so it stays
too. Un-pinning util-linux can only remove util-linux's own subtree (tens of MiB),
not the base userland. The real lever for deployed-closure reduction is the
`follows`/cache-identity tradeoff in ADR 029 (deliberately recommended against),
not this change. Each closure effect is therefore **re-measured in isolation**
(see Verification) and reported as the actual number: expect the standalone
`nix build .#braid` win to be the larger one (it drops *full* systemd + util-linux
from the flake wrapper) and the switch win to be only util-linux's subtree.

**Safety gate (the "before removing" check) -- PASSES.** Code audit + direct
reads confirm braid never parses `systemd`/`util-linux` output in a
version-fragile (non-JSON, non-`show`) way:

- `lsblk --json` -> serde in `cli/src/parse/lsblk.rs#parse_lsblk_json`. No
  `deny_unknown_fields` (tolerates added columns); the `required_option` helper
  only fail-closes to `InvalidJson` when a *requested* column is missing. Stable
  structured contract.
- `lsblk -n -d -b -o <single col>` -> trimmed single value
  (`cli/src/confirm.rs#get_lsblk_field`). Machine query.
- `systemctl show -P ActiveState` / `-P BoundBy` -> fixed-word match (unknown
  handled gracefully) and whitespace split in `cli/src/online_state.rs`
  (`UnitActiveState::parse`, `RealOnlineStateOps::list_bound_by`). The documented
  `show` key=value/property contract.
- `systemctl list-units --output=json` -> tolerant serde + raw fallback; Browse
  TUI shows raw `lsblk`/`systemctl status|show` output *verbatim* (no parser to
  break); `mount`/`umount`/`wipefs`/`mountpoint` -> exit-status only.

These contracts are already test-backed -- the safety gate is not mere assertion.
`cli/src/online_state.rs` locks the systemd `show -P` and `mountpoint` paths
(`parses_active_state_refreshing`, `list_bound_by_parses_whitespace_separated_units`,
`real_ops_mountpoint_exit_*`), and `cli/src/confirm.rs` locks the single-field
lsblk query (`lsblk_field_*`). The one genuine coverage gap is lsblk's tolerance
of *added* columns: the existing `lsblk_rejects_malformed_inline` only proves
rejection of a type mismatch, not acceptance of extra keys -- which change 4 fills.

This matches braid's own authority: `docs/design/principles.md` Principle 10 and
ADR `docs/design/decisions/010-toolchain-pinning.md` already classify `systemd`
as a non-pinned generic helper; `util-linux` is pinned there only "for `lsblk`
JSON parsed by serde" -- which is exactly the stable contract that does not need a
pin.

**Key wiring fact (decides scope).** There are two PATH wrappers:

1. `flake.nix` `toolPath` (`lib.makeBinPath`, used only by the standalone
   `braid` wrapper at the `makeWrapper` call) -> wraps the `nix run`/`nix build`
   binary. Pins **both** systemd + util-linux.
2. `modules/braid/wrapper.nix` `toolPackages` -> what a `nixos-rebuild switch`
   actually deploys. It wraps the **unwrapped** crane binary
   (`config.braid.package` defaults to `braid-cli-unwrapped`), so the flake
   `toolPath` is **never** in the module closure. Here systemd is **already
   host-provided** (`++ [ pkgs.systemd ]`, where `pkgs` is the consumer's
   nixpkgs) and only util-linux is pinned -- via the single line
   `utilLinux = lib.mkDefault braidPkgs.util-linux;` in `nixosModules.default`.

So the standalone-package edit (change 1) and the module-default edit (change 2)
are two separate edits with separate, isolation-measured closure effects (see
Context above). Per user decision, scope = **full un-bundle** (both).

## Changes

### 1. `flake.nix` -- standalone package wrapper (`toolPath`)

Remove `pkgs.util-linux` and `pkgs.systemd` from the `toolPath = pkgs.lib.makeBinPath [ ... ]`
list. Keep `pkgs.cryptsetup`, `pkgs.btrfs-progs`, `pkgs.smartmontools`,
`pkgs.nut`, `pkgs.ethtool`. `toolPath` is consumed only by the standalone `braid`
`makeWrapper` derivation (`flake.nix`: `makeWrapper ... --prefix PATH : ${toolPath}`).
That standalone binary (`linuxCrane.braid`) is exactly what the `tests/cli/*` VM
suite installs on `systemPackages` and exercises (see out-of-scope: VM-suite
wiring), so change 1 *is* covered and gated by `nix flake check`. Because
`makeWrapper` uses `--prefix` (prepend + ambient fallback), the un-bundled binary
resolves `lsblk`/`mount`/`systemctl` from the VM's base-system PATH, and the suite
passing is the regression proof that ambient resolution works.

### 2. `flake.nix` -- module default (`nixosModules.default`)

Delete the line `utilLinux = lib.mkDefault braidPkgs.util-linux;` from
`config.braid.packages`. With the pinned `mkDefault` gone, `cfg.packages.utilLinux`
falls back to the option's own default in `modules/braid/options.nix`
(`utilLinux = lib.mkPackageOption pkgs "util-linux" { }`, where `pkgs` is the
consumer's nixpkgs) -> **host util-linux**. No systemd change is needed in the
module (already host).

**Blast radius (one knob, several refs).** Every deployed util-linux reference
routes through `cfg.packages.utilLinux`, so this single deletion moves all of them
from pinned to host at once: the CLI wrapper PATH (`wrapper.nix` `toolPackages`),
the `braid-unlock.service` and `braid-auto-unlock.service` unit `path`s, and the
scrub-cancel `${utilLinux}/bin/mountpoint` check -- the last three all in
`storage.nix` via its `utilLinux = cfg.packages.utilLinux` let-binding. They move
together. This is benign (`mountpoint` is exit-status only; the unit-path tools are
the same stable contracts) and is the deployed-closure effect to re-measure in
isolation (Verification step 2). The lone literal `${pkgs.util-linux}/bin/setpriv`
in `monitor.nix` is unaffected -- it already resolves from the consumer's `pkgs`.

**Keep** `modules/braid/wrapper.nix` (`util-linux` still belongs on PATH -- it
now resolves to host), the `options.braid.packages.utilLinux` option in
`options.nix` (preserves the documented override knob), and the other five pins
in `nixosModules.default`.

### 3. Docs -- reclassify util-linux from "pinned" to "stable contract, host-provided"

Edit only the claims that util-linux is *pinned-by-default at runtime*. util-linux
remains a *parsed* tool (lsblk JSON) and stays in fixture coverage; it simply
joins systemd's "stable contract, not pinned" category. systemd is already
documented correctly.

**First, enumerate every site** so no stale claim survives:
`rg -n -i 'pinn|util-linux|utilLinux|braid\.packages|parser-critical' docs/ AGENTS.md README.md`
and reconcile each hit against the new invariant (five pinned fragile tools --
btrfs-progs, cryptsetup, nut, smartmontools, ethtool; util-linux + systemd
host-provided). For each ADR, check its status header first and honor the
frozen-ADR rule in `docs/dev/doc-citations.md#decision-doc-references` (edit in
place only if Active; otherwise add a forward-pointer rather than rewriting).

- `docs/design/decisions/010-toolchain-pinning.md` (authoritative ADR -- update
  comprehensively, not just two spots): the Context paragraph ("parser-critical
  runtime tools (... util-linux ...)"); the **How it works** bullets
  ("`braid.packages.*` (... utilLinux ...) default to braid's `nixpkgs` flake
  input" and "builds the `braid.packages.*` defaults with `import
  self.inputs.nixpkgs`" -- now true for the five pinned tools, with
  util-linux/systemd as the consumer-sourced exceptions); the overrides paragraph
  ("Parser-critical tools are pinned by default ..."); and the decision **table**
  row -> "util-linux (lsblk) | No -- host `pkgs` | Yes (`braid.packages.utilLinux`)
  | `lsblk --json` is a stable structured contract (serde, no `deny_unknown_fields`,
  tolerant of added columns, fail-closed only on missing *requested* columns), so
  pinning adds no parse-stability benefit". **Also amend the Classification
  guideline rule itself**, which today has only two buckets ("**Pin** when: braid
  parses the tool's output ..." vs. generic helpers). `lsblk --json` *is* parsed,
  so leaving the rule as "pin if parsed" while the table lists util-linux as
  host-provided makes the ADR self-contradictory. Introduce a **third category** --
  "parsed, but via a contract stable enough (tolerant structured JSON, fail-closed
  on missing requested keys) that pinning is unnecessary" -- and classify
  util-linux there. Do **not** lump it with "generic helpers (coreutils, systemd)":
  it is parsed and stays fixture-covered. The reclassification must then *follow
  from* the stated rule. Add a short rationale note; preserve the ADR's status.
- `docs/design/principles.md` Principle 10: today this principle has only two
  buckets (pinned-because-parsed vs. generic helpers "not part of braid's parser
  contract"). Drop `util-linux` from the pinned parser-critical list **and** add
  the same third category as ADR 010 -- "parsed, but via a contract stable enough
  that pinning is unnecessary; host-provided and overridable" -- so util-linux is
  reclassified by a stated rule, not lumped with generic helpers (it is parsed and
  stays fixture-covered). P10 and ADR 010 must define the new bucket identically.
- `docs/design/decisions/020-ups-integration.md` (status: **Active**, so not
  frozen): three sentences name util-linux among the pins. Handle them **by kind**,
  to correct the contradiction without falsifying the historical record:
  - **Present-tense current-state claims -- correct inline.** (1) "NUT
    (`networkupstools`) joins btrfs-progs, cryptsetup, and util-linux in the
    parser-critical toolchain ..." and (2) the Consequences bullet "NUT joins
    btrfs-progs, cryptsetup, and util-linux as a pinned parser-critical tool ...":
    drop util-linux from the *pinned* enumeration (NUT joins btrfs-progs and
    cryptsetup). Preserve util-linux's still-true fixture-refresh obligation, which
    is independent of pinning.
  - **Decision-narrative claim -- preserve history with a dated forward-note.**
    "A new `braid.packages.networkupstools` option is added alongside the existing
    `btrfsProgs`, `cryptsetup`, and `utilLinux` pins" records what the UPS decision
    did when util-linux *was* pinned; rewriting it to drop util-linux would read as
    if it never was. Instead append an inline note -- "(util-linux has since been
    un-pinned; see ADR 010)" -- whose explicit current-status statement removes the
    contradiction while keeping the record honest.
  - Cite ADR 010 only as the *rationale* for the reclassification; do not otherwise
    rewrite the surrounding UPS rationale. NUT itself stays pinned, so its own
    pinning claims are unchanged.
- `docs/design/decisions/029-release-process.md` (the no-follows section): caveat
  that util-linux and systemd resolve from the consumer's nixpkgs **regardless**
  of `braid.inputs.nixpkgs.follows`, since only the five pinned tools +
  `braid-cli-unwrapped` ride braid's `nixpkgs` input now.
- `docs/guides/nixos-configuration.md`: remove `util-linux` from the "Pinned
  toolchain" bullet; add the same consumer-sourced caveat to the "Override these
  only if ..." paragraph (it currently says all these defaults come from braid's
  pinned nixpkgs); **keep** the `braid.packages.utilLinux` options-table row and
  the commented override example (the option still exists; its default is now host
  util-linux).
- `docs/dev/parser-compatibility.md`: reclassify `util-linux` -- parsed via stable
  JSON and still fixture-covered, but host-provided rather than pinned-by-default;
  the still-pinned fragile-text set is btrfs-progs/cryptsetup/nut/smartmontools/ethtool.
- `AGENTS.md`: bring the "Parser compatibility" blurb in line with the above
  (util-linux parsed via stable JSON contract / host-provided; keep it in the
  fixture-refresh enumeration since `braid.packages.utilLinux` and the lsblk
  parser still exist).
- `README.md` and `docs/guides/getting-started.md`: **checked, no edit needed** --
  listed so the bullets above are not mistaken for the full sweep set (the change-3
  `rg` inventory remains the authority for completeness). Their only matches are
  generic "pinned nixpkgs"/"pinned toolchain" phrasing (README's NixOS-config
  table row + the binary-cache "its pinned nixpkgs" line; getting-started's "own
  pinned nixpkgs" / `braid-cli-unwrapped` lines), all still true since braid still
  pins its nixpkgs and five tools. `nixos-configuration.md`'s "Pinned toolchain"
  bullet *is* a real util-linux list, handled by its own bullet above.

### 4. Tests (regression guards)

These lock in the two load-bearing premises so a future revert or strict-serde
change fails CI rather than silently weakening the rationale.

- **Module util-linux source (guards change 2 -- the deployed-source change).** Add a
  pure-eval check `tests/eval/nixos-module-default-util-linux-host.nix` and
  register it in `flake.nix` `checks` as `eval-nixos-module-util-linux-host`
  (same shape + registration as `tests/eval/nixos-module-default-package.nix`;
  Intent/Why/Scenario preamble per `docs/dev/testing.md`). Build
  `self.nixosModules.default` with `braid.enable = true` and a consumer
  `nixpkgs.overlays` entry that tags **both** `util-linux` **and** `cryptsetup`
  with **distinct** sentinel passthru markers (one marker per package, e.g.
  `passthru.braidHostMarkerUtilLinux` and `passthru.braidHostMarkerCryptsetup`).
  Both markers must be **absent** from braid's clean `import self.inputs.nixpkgs`,
  so the test distinguishes consumer/host-sourced from pinned (a consumer that
  reuses braid's own nixpkgs would not). Force-eval only the two package attrs
  (stay pure-eval, no toplevel build) and assert: (a) `config.braid.packages.utilLinux`
  **carries** its util-linux marker -> resolved from the consumer's overlaid pkgs
  (host); (b) `config.braid.packages.cryptsetup` **lacks** its own cryptsetup
  marker -> resolved from braid's clean pinned `nixpkgs`, not the overlaid consumer
  pkgs. Tagging cryptsetup too is what gives (b) teeth: a util-linux-only marker
  can never appear on cryptsetup regardless of its source, so a single-tool tag
  would make (b) vacuously pass even if a pinned tool leaked from consumer pkgs.
  Re-adding the deleted `utilLinux = lib.mkDefault braidPkgs.util-linux;` line
  fails (a); a regression that resolves a pinned tool from consumer pkgs fails (b).
  **TDD sequencing (per AGENTS.md / `docs/dev/testing.md`):** this is a genuine
  red->green guard, so add it and confirm it **fails for the right reason before
  change 2** -- with util-linux still pinned, `config.braid.packages.utilLinux` is
  `braidPkgs.util-linux`, which carries no consumer marker, so assertion (a) fails
  -- then apply change 2 to turn it green. (It is the one change-2-sensitive test;
  see Verification step 3.)
- **lsblk JSON tolerance (guards the safety-gate premise).** Add a
  `parse_lsblk_json` unit test in `cli/src/parse/lsblk.rs` feeding JSON that
  includes all requested columns **plus** extra unknown keys (one top-level
  alongside `blockdevices`, one per-device) and asserting `Ok` with the requested
  fields intact. This pins the "tolerates added columns" behavior the
  host-util-linux rationale depends on; adding `#[serde(deny_unknown_fields)]`
  later would fail it. Complements the existing missing-required-key and
  malformed-JSON tests. (This is a characterization test of existing behavior --
  it passes immediately on current code, so the red-before-green sequencing above
  does not apply to it.)

### Out of scope / intentionally unchanged (verify, do not edit)

- **Dev shell** (`flake.nix` `devShellFor`): independent `packages` list that
  already carries `pkgs.util-linux` from braid's pinned nixpkgs -- required for
  reproducible `just capture-all-fixtures`. Not touched; `docs/dev/overview.md`
  stays accurate.
- **Literal `${pkgs.*}` unit refs (already host, unchanged by either change):**
  `${pkgs.util-linux}/bin/setpriv` in `monitor.nix` (the *only* literal
  `pkgs.util-linux`) and `${pkgs.systemd}/bin/systemctl|systemd-ask-password`
  across `monitor/ups/storage.nix` -- `pkgs` there is the consumer's nixpkgs, so
  these are host-provided independent of `toolPath` and of change 2. **Correction
  from a prior draft:** `storage.nix`'s util-linux refs are **not** literal
  `${pkgs.util-linux}`; they are `cfg.packages.utilLinux` and so *are* moved by
  change 2 (covered in change 2's blast-radius note, not out of scope).
- **VM-suite wiring of change 1 (verify, no new test -- existing coverage).** The
  `tests/cli/*` checks (and the few `tests/storage/*` checks that take a `braid`
  arg, e.g. `luks-header-backup.nix`) install `braid = linuxCrane.braid` -- the
  flake's single `toolPath`-wrapped standalone -- **directly** on
  `environment.systemPackages`. They import no module and set no `braid.package`
  (the sole `braid.package =` anywhere in `tests/` is `tool-versions.nix`, set to
  `braid-cli-unwrapped`), so there is **no double-wrap**: the binary under test is
  the same standalone change 1 edits. Because `makeWrapper` uses `--prefix PATH`
  (ambient fallback), the un-bundled standalone resolves `lsblk`/`mount`/`umount`/
  `wipefs`/`systemctl` from the VM's NixOS base-system PATH -- so the ~100-check
  CLI suite *is* change 1's functional gate (it would fail if ambient resolution
  did not work). The raw-primitive storage tests (`luks.nix`, `btrfs-*.nix`)
  install no braid binary at all and are irrelevant to change 1. The
  `cli.nix`/`wrapper.nix` re-wrap applies only to module-based systems and the
  `tests/module/*` checks, which install the **unwrapped** `braid-cli-unwrapped`
  and so never touch `toolPath`. (A prior draft of this plan claimed the VM tests
  pass `linuxCrane.braid` as `braid.package` and the module re-wraps it into a
  double-wrap that masks change 1; the code shows otherwise -- corrected here.)
- **Parsers + fixtures**: no parser code or fixtures change, and `nixpkgs` is not
  bumped, so this is **not** a fixture-refresh event.
- `docs/dev/reference-source.md` (keep vendored systemd/util-linux sources -- we
  still read the lsblk JSON schema and systemd docs), ADR 018, ADR 034 (not about
  pinning).

### Behavior change to call out

The standalone `braid` -- what `nix run`/`nix build .#braid` produces and what the
`tests/cli/*` VM suite installs and exercises -- now relies on **ambient** host
`systemd`/`util-linux` (via the wrapper's `--prefix PATH` fallback) rather than
bundled copies. This is change 1. The module deployment path (`nixos-rebuild
switch`, `tests/module/*`) re-wraps the unwrapped binary and is governed
separately by change 2. Both tools are base-system on any systemd Linux host and
consumed via stable contracts, so this is safe; it trades a little
self-containment of the standalone binary for a smaller standalone closure,
consistent with the classification rationale.

## Verification

1. **Standalone wrapper + closure (change 1):** the **functional gate is the
   `tests/cli/*` VM suite under `nix flake check`** -- it installs the un-bundled
   `linuxCrane.braid` directly and drives `lsblk`/`mount`/`systemctl` through it,
   proving the standalone resolves util-linux/systemd from the VM's ambient
   base-system PATH (the `--prefix` fallback). Separately, for the closure-shrink
   check: `nix build .#braid`, inspect the wrapper
   (`$(nix path-info .#braid)/bin/braid`) and confirm no util-linux/systemd store
   paths remain in the `--prefix PATH`, and measure the real delta in isolation
   (`nix path-info -S .#braid` before/after change 1 only). Expect a meaningful
   drop, since the flake `toolPath` pins *full* systemd + util-linux.
2. **Switch closure (change 2), re-measured in isolation:** measure on a toplevel
   that **actually consumes the pin** -- one that imports `self.nixosModules.default`
   (where `utilLinux = lib.mkDefault braidPkgs.util-linux` lives, `flake.nix`) with
   `braid.enable = true` on a base NixOS system. **Do not reuse a `tests/module/*`
   config:** those import the raw `../../modules/braid`, which never carries the
   `nixosModules.default` `mkDefault` pin, so deleting the line there changes
   nothing and would report a misleading ~0 delta (this is exactly the
   insensitivity step 3 establishes). No buildable `self.nixosModules.default`
   toplevel exists to reuse today -- the eval tests use it but force only
   `config.braid.package`, never `config.system.build.toplevel`, so they build no
   closure -- so construct a minimal one for this measurement. Compare
   `nix path-info -S` on that toplevel before/after deleting only the
   `utilLinux = lib.mkDefault braidPkgs.util-linux;` line, all else equal. In the
   "before" state braid's `braidPkgs.util-linux` is a duplicate of the host's base
   util-linux; the delta is that duplicate's **own** subtree. Confirm the deployed
   braid wrapper's PATH then resolves util-linux to the **host** store path (not
   `braidPkgs`), and do **not** expect `glibc` or `systemd-minimal-libs` to drop
   (both retained by `braid-cli-unwrapped` and the five still-pinned tools). Report
   this figure in place of the retired ~429 MiB claim.
3. **Eval + VM:** `nix flake check` (runs on aarch64-darwin via
   `nix.linux-builder`) catches eval breakage from the removed line and runs the
   new `eval-nixos-module-util-linux-host` regression check (util-linux carries the
   consumer marker -> host-sourced; cryptsetup lacks its own marker -> stays
   pinned). **That eval check is the sole change-2-sensitive guard.** The
   `tests/module/*` VM tests import the raw `../../modules/braid` (not
   `self.nixosModules.default`), so `cfg.packages.utilLinux` already resolves to
   the `options.nix` host default (`mkPackageOption pkgs "util-linux"`) before
   *and* after change 2 -- they pass identically and **cannot** detect the
   flake-pin deletion. What the VM suite *does* provide is integration confidence
   that braid works on host util-linux -- and it has provided exactly that since
   before this change: the module path (the `storage.nix`
   `braid-unlock`/`braid-auto-unlock` unit `path`s and the scrub mountpoint check)
   has run on host util-linux in CI all along, which is strong evidence change 2 is
   low-risk. For that integration confidence, confirm the relevant checks still
   pass: a lifecycle test driving `lsblk`/`mount`/`systemctl` through braid, the
   `braid unlock` CLI path (`tests/cli/braid-unlock.nix`), and the auto-unlock
   service path (`tests/module/auto-unlock-key-present.nix`) whose unit `path`s now
   carry host util-linux (ADR 034).
4. **Parsers:** `just test-rust` (now including the new `parse_lsblk_json`
   extra-keys tolerance test) and `just test-parsers` pass; no parser logic or
   golden fixtures change.
5. **Docs:** re-run the change-3 `rg` inventory and confirm no remaining claim
   that util-linux is pinned-by-default; `just docs-build` (mdbook-linkcheck2 link
   validation), `scripts/docs/check-output-ascii.py`, and
   `scripts/docs/check-see-paths.py` if any `## See` section is touched.

## Implementation notes

- `tests/cli/braid-tui-browse.nix` intentionally starts the standalone flake
  wrapper directly from a systemd unit. Because systemd units do not inherit the
  VM's `environment.systemPackages` PATH, the canary now sets the unit `path` to
  host `pkgs.util-linux` and `pkgs.systemd` so it exercises the standalone
  wrapper's new caller-supplied ambient PATH contract instead of the module
  wrapper.
- The exact standalone closure delta was not measurable in this session: the
  exported standalone package target is `x86_64-linux`, but the configured remote
  Linux builder was `aarch64-linux`. The functional VM, eval, Rust parser, live
  parser, docs, and ASCII/See-path checks were run instead.

## Follow-up

- Measure the exact standalone and deployed closure deltas on an `x86_64-linux`
  builder before publishing a closure-size number.
