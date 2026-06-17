# Convert out-of-scope reference/external line-number citations

## Context

The prior pass (commit `1eda8d25`, "docs: convert internal line-number citations
to path#symbol") migrated braid's *own* in-tree citations to the `path#symbol`
form. It deliberately deferred citations that point into **external upstream
source**, because those follow a different rule with a different fix shape:
[`reference-source.md#citing-reference-code`](../../docs/dev/reference-source.md).

External code lives in `reference/` (gitignored, refreshed wholesale by
`just fetch-references`, absent on clean checkout, invisible to CI) -- or, for the
NixOS test driver, isn't vendored at all. A line number into it drifts on every
refresh and nothing validates it. The doc prescribes citing by **shape**: drop the
line number, stamp `pkg <version>, <path> (fn name)`, and either inline a short
behavior-defining snippet as frozen ground truth or keep a function-name pointer
plus a one-line paraphrase.

This pass is **narrow**: it converts only the 6 hddfancontrol + NixOS-test-driver
citations (4 in `probe.rs`, 1 in `fan-control.nix`, 1 in `systemd-lifecycle.py`).
It is *not* the full external-citation migration -- tracked source still carries
~40 more `reference/...:<line>` cites (btrfs-progs, nut, cryptsetup, linux) that
are explicitly deferred (see [Out of scope](#out-of-scope)). The migrate-when-you-
touch rule in the doc means each gets converted the next time its file is edited;
batching them all is a separate, larger pass.

### Verified facts (drift the conversion must correct)

- **Version:** `git -C reference/hddfancontrol describe --tags` -> `2.1.1`. nixpkgs
  at the pinned rev (`flake.lock`: `nixos-26.05`, `b51242d7...`)
  ships `hddfancontrol` `2.1.1` too (`pkgs/by-name/hd/hddfancontrol/package.nix`).
  So the existing `2.0.6` stamp in `fan-control.nix` is **stale** -- stamp `2.1.1`.
- **Path moved:** `src/fan.rs` no longer exists in 2.1.1; `resolve_rpm_path` is now
  a method in `src/fan/pwm_fan.rs`.
- **nixpkgs source identity:** the test-driver line `858` is stale (the snippet has
  moved within the file). The locked source is at the full repo path
  `nixos/lib/test-driver/src/test_driver/machine/__init__.py`, in `QemuMachine._execute`,
  snippet verified at the pinned rev (`flake.lock` nixpkgs rev `b51242d7...`):
  `command = f"set -euo pipefail; {command}"`. Cite the **rev**, not the channel:
  nixpkgs is not a `reference/` checkout with a release tag, and `nixos-26.05` is a
  mutable channel pointer that would let the source drift while the stamp stays put;
  the flake.lock rev is the exact pin and the real re-verify trigger. (The
  hddfancontrol cites use the package release `2.1.1` instead, because that tool *is*
  vendored in `reference/` with a clean `git describe` tag -- a principled asymmetry:
  stamp the most precise stable identity each source offers.)

### Form decisions (per the doc's two shapes)

- **Canonical stamp shape, used uniformly:** `pkg <version>, <path> (fn \`name\`)`,
  optionally followed by `: <inline snippet or one-line paraphrase>`. Introduce the
  stamp with a dash or as a standalone clause -- never nest it inside an outer
  parenthetical -- so the `(fn \`name\`)` parens read cleanly without `(... (fn x))`.
  This matches the in-tree precedents exactly: `cli/src/parse/types.rs#as_token`
  (`-- nut 2.8.4, clients/upsc.c (fn \`list_vars\`):`) and `cli/src/parse/upsc.rs`.
  (The bare-parenthetical `(pkg ver, path fn \`name\`: ...)` variant in
  `cli/src/cmd.rs` drops the inner parens only to dodge nesting; converting to the
  dash lead-in lets every cite use the canonical `(fn \`name\`)` instead.)
- All four `probe.rs` `///` cites carry a paraphrase in surrounding prose -> keep
  them as prose pointers; **no fenced code blocks**, so no `cargo test --doc`
  doctest risk is introduced.
- The two single-line, behavior-defining cites (the `cl.rs` partition filter, the
  test-driver pipefail wrapper) get an **inline code-span excerpt** (backticks,
  not a fenced block) of the exact upstream line, frozen as ground truth, appended
  after the `(fn \`name\`)` stamp with a `:`.

## Changes

Precedent style to mirror (already in-tree): `cli/src/parse/upsc.rs` module doc and
`cli/src/parse/types.rs#as_token` -- `pkg <version>, <path> (fn \`name\`)`.

### 1. `cli/src/tui/probe.rs` (4 cites)

- **`resolve_rpm_path` doc (currently `src/fan.rs:118-143`).** The fn is the cited
  subject, so name it in the stamp (drop the redundant prose mention) and use the
  canonical `(fn ...)` form:
  - `Mirrors hddfancontrol's \`resolve_rpm_path\` (\`reference/hddfancontrol/src/fan.rs:118-143\`) in its sole-candidate branch:` ->
    `Mirrors the sole-candidate branch of hddfancontrol 2.1.1, src/fan/pwm_fan.rs (fn \`resolve_rpm_path\`):`
- **`-d ata` selector doc (currently `src/cl.rs:117-135`).** Dash lead-in to avoid
  nesting the stamp inside the existing parenthetical:
  - `Mirror of hddfancontrol's \`-d ata\` selector (\`reference/hddfancontrol/src/cl.rs:117-135\`): enumerate` ->
    `Mirror of hddfancontrol's \`-d ata\` selector -- hddfancontrol 2.1.1, src/cl.rs (fn \`to_drive_paths\`): enumerate`
- **Partition-exclusion inline comment (currently `src/cl.rs:128`).** Inline the
  exact upstream predicate as frozen ground truth:
  - `// Matches \`reference/hddfancontrol/src/cl.rs:128\`.` ->
    ```
    // Matches hddfancontrol 2.1.1, src/cl.rs (fn `to_drive_paths`):
    // `!f.trim_end_matches(char::is_numeric).ends_with("-part")`.
    ```
- **`read_drivetemp` doc (currently `src/probe/drivetemp.rs:20-46`).**
  - `Mirror \`reference/hddfancontrol/src/probe/drivetemp.rs:20-46\`:` ->
    `Mirror hddfancontrol 2.1.1, src/probe/drivetemp.rs (fn \`prober\`):`

### 2. `modules/braid/fan-control.nix` (1 cite, line 151)

Replace the stale `(src/probe/mod.rs:84 in 2.0.6)` pointer with a dash-led canonical
stamp (no nested parens) plus the concrete symbol as the frozen fact. Reflow the
comment span, e.g.:

```
    # No hddtemp daemon dependency: hddfancontrol tries drivetemp first in
    # its probe chain -- hddfancontrol 2.1.1, src/probe/mod.rs (fn `prober`)
    # lists drivetemp::Method first in the methods array. drivetemp is loaded
    # via boot.kernelModules above.
```

### 3. `tests/module/systemd-lifecycle.py` (1 cite, line 161)

Stamp the nixpkgs rev, use the full resolvable repo path, name the method with the
canonical `(fn ...)` form, and inline the exact wrapper line; drop `:858`. Dash
lead-ins set off the stamp so its parens don't nest:

```
    # NixOS test driver wraps every command with `set -euo pipefail` --
    # nixpkgs b51242d7, nixos/lib/test-driver/src/test_driver/machine/__init__.py
    # (fn `QemuMachine._execute`): `command = f"set -euo pipefail; {command}"` --
    # so a bare
```

### 4. `docs/dev/reference-source.md` (inventory correction)

The same `fan.rs` -> `fan/` move that stales the `probe.rs` cite also stales the
hddfancontrol inventory line in this doc (it still lists `\`fan.rs\` (PWM control)`,
but 2.1.1 has a `fan/` module: `cmd_fan.rs`, `mod.rs`, `pwm_fan.rs`). Correct it in
the same pass so the central reference inventory matches the pinned source:

- `\`fan.rs\` (PWM control)` -> `\`fan/\` (PWM control -- \`pwm_fan.rs\`)`

(One inline-code edit, no link change.)

## Verification

Nothing validates `reference/` citations either way (per the doc), so verification
is "didn't break the build, and the new stamps are accurate":

- `cargo build -p braid-cli` and `cargo test -p braid-cli --doc` -- confirms the
  `probe.rs` comment edits compile and introduce no accidental doctest (we add no
  fenced blocks, so `--doc` should be a no-op pass).
- Re-confirm the stamps resolve against the vendored tree:
  `git -C reference/hddfancontrol describe --tags` == `2.1.1`; the cited symbols
  exist -- `grep -n 'fn resolve_rpm_path' reference/hddfancontrol/src/fan/pwm_fan.rs`,
  `grep -n 'fn to_drive_paths' reference/hddfancontrol/src/cl.rs`,
  `grep -n 'fn prober' reference/hddfancontrol/src/probe/{drivetemp,mod}.rs`.
- `python3 -m py_compile tests/module/systemd-lifecycle.py` -- comment-only edit,
  confirms no syntax breakage.
- Code/comment edits are inside comments (ASCII, no Unicode introduced); the
  `scripts/docs/check-output-ascii.py` checker exempts comments, so it is unaffected.
- The `reference-source.md` edit changes only inline-code text (no links), so
  `just docs-build` (mdbook + `mdbook-linkcheck2`) should still pass; run it to
  confirm the one doc-page edit didn't break the build.

## Out of scope

Only the 6 listed hddfancontrol/test-driver citations plus the one co-located
`reference-source.md` inventory line they stale. No behavior change, no functional
code, no other doc-page edits.

### Deferred external citations (same drift, different pass)

The same line-number-into-`reference/` pattern lives in ~40 other tracked-source
cites; full inventory: `rg -n 'reference/[^ ]*\.[a-z]+:[0-9]' --glob '!docs/book/**' -g '!plans/**'`.
They are deliberately left for the migrate-when-you-touch rule (or a dedicated
later pass), grouped by tool:

- **btrfs-progs:** `cli/src/scrub_cancel.rs`, `cli/src/cmd.rs`, `cli/src/replace.rs`,
  `cli/src/remove.rs`, `cli/src/inhibit.rs`, `cli/src/parse/btrfs_replace_status.rs`,
  `cli/src/pool.rs`, `tests/module/scrub-lifecycle.py`, `tests/repro/...`, `tests/cli/...`.
- **nut:** `cli/src/parse/upsc.rs`, `cli/tests/support/golden_common.rs`, and several
  `tests/module/ups-*` + `tests/module/lib/ups-fixture.nix` files.
- **cryptsetup:** `cli/src/discover.rs`, `cli/src/cmd.rs`,
  `cli/src/parse/cryptsetup_luks_version.rs`.
- **linux:** `cli/src/replace.rs`, `tests/repro/btrfs-replace-rejected-during-scrub.py`.
- **docs (markdown, still external-reference shape):** `docs/guides/troubleshooting.md`,
  `docs/design/decisions/014-alerts.md`, `docs/design/decisions/020-ups-integration.md`.
  (These cite `reference/` code, so they take the shape stamp -- *not* `path#heading-slug`,
  which the doc reserves for braid's own tracked files.)

Touching any of those files for another reason is the cue to convert its cites then.
