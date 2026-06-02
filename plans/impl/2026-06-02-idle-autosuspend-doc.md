# Plan: fix the `braid idle` autosuspend-integration doc

## Context

`docs/commands/idle.md` (the `braid idle` command reference) ends its
"Autosuspend integration" section (lines 69-88) with a **hand-written raw INI
block** that tells the reader to "Add it to your autosuspend configuration":

```ini
[check.BraidPool]
enabled = true
class = ExternalCommand
command = ! timeout -k 2 10 braid idle
```

This is wrong on three counts:

1. **Wrong shape / bare names.** braid's NixOS module actually generates
   `command = "${pkgs.bash}/bin/bash -c '! ${pkgs.coreutils}/bin/timeout -k 2 10 ${braidWrapped}/bin/braid idle'"`
   -- a `bash -c '! ...'` wrapper with **fully qualified `/nix/store` paths**
   (`modules/braid/auto-suspend.nix:78-88`, pinned by
   `tests/module/braid-auto-suspend.py:58-71`). autosuspend runs the check
   outside braid's wrapper, so its service PATH does not contain `braid` or
   coreutils `timeout` -- the qualified-path requirement is the explicit
   invariant in `docs/design/decisions/016-auto-suspend.md:99-101`.

2. **Wrong deployment model.** braid is NixOS-only and is configured with
   `braid.autoSuspend.enable = true`; the module emits this check (and the
   `BraidWol`, SSH, etc. checks) for you. Presenting a raw INI the user should
   hand-write contradicts how the tool is actually deployed. The canonical
   setup already lives correctly in `docs/guides/power-management.md:36-64`
   with no raw INI; idle.md is the only page in the tree showing a
   hand-written check command.

3. **Silent failure if copied.** The original review finding claimed a
   hand-copied block "exits 127 -> SevereCheckError". That mechanism is
   wrong: the leading `!` is part of the string handed to
   `subprocess.check_call(self._command, shell=True)`
   (`reference/autosuspend/src/autosuspend/checks/command.py:43`), so the
   shell inverts a not-found command's non-zero (127) to **0**, which
   autosuspend reads as "activity detected". The real consequence is worse and
   quieter: the check is stuck reporting activity, so **the NAS silently never
   auto-suspends** -- no `SevereCheckError` is raised (that path only fires for
   a bare, non-`!` missing command). This strengthens the case for the fix.

**Decision (confirmed with the user):** reframe the section around
`braid.autoSuspend.enable = true` and the real generated command shape, and
**trim the depth** -- defer the exit-inversion table and the inner-`timeout`
rationale to ADR 016 (which already contains an identical inversion table at
`016-auto-suspend.md:45-53` and the timeout reasoning at `:55-57`), rather than
duplicating them on the command page. This matches the project's doc
architecture ("detailed rationale lives in `docs/design/decisions/`") and house
style (avoid literal `/nix/store/...` hashes in examples; use "the NixOS module
generates X" framing).

## The change

Single file, single section. Replace `docs/commands/idle.md` lines **69-88**
(the `## Autosuspend integration` heading through the final "load-bearing"
bullet) with the trimmed, module-attributed form below. Everything from line 90
onward (`## What happens under the hood`, `## Related commands`) is unchanged.

Proposed replacement text:

```markdown
## Autosuspend integration

`braid idle` is the activity check behind braid's auto-suspend. You don't write
this check by hand: set `braid.autoSuspend.enable = true` and braid's NixOS
module generates the [autosuspend](https://autosuspend.readthedocs.io/)
`services.autosuspend` ExternalCommand check (`BraidPool`) for you. The
generated command -- `bash -c '! timeout -k 2 10 braid idle'`, with fully
qualified `/nix/store` paths for `bash`, `timeout`, and `braid` -- handles the
exit-code inversion autosuspend expects and a fail-closed inner `timeout`.
Don't reproduce it by hand: autosuspend runs the check outside braid's wrapper,
so bare `braid`/`timeout` are not on its PATH.

See the [power management guide](../guides/power-management.md) for setup, and
[ADR 016: Auto-Suspend](../design/decisions/016-auto-suspend.md) for the
exit-inversion table, the qualified-path requirement, and why `timeout` must
sit inside the `!`-inverted command.
```

Notes for the implementer:
- Keep the visible command shape `bash -c '! timeout -k 2 10 braid idle'` so the
  page still shows what the check looks like, but do **not** invent literal
  store hashes (`/nix/store/abc.../bin/...`) -- house style avoids them.
- Drop the `enabled = true` bullet and its parenthetical entirely; it only
  described a raw-INI quirk the module form never exposes.
- Use `--` (double hyphen), not em-dashes, per the repo CLI/doc style rule.
- Link paths are single-`../` from `docs/commands/`:
  `../guides/power-management.md` and `../design/decisions/016-auto-suspend.md`
  (both registered in `docs/SUMMARY.md`; idle.md already links the latter at
  line 98, so a second link to the same target is fine).

## Out of scope / explicitly not changing

- `modules/braid/auto-suspend.nix` -- already correct; it is the source of
  truth this doc is being aligned to.
- `docs/design/decisions/016-auto-suspend.md`, `docs/guides/power-management.md`
  -- already accurate; no edits.
- `tests/module/braid-auto-suspend.*` -- the generated-form assertions still
  hold; no test pins the idle.md prose (verified).
- The `## What happens under the hood` and `## Related commands` sections of
  idle.md.

## Verification

Doc-only change; no Rust or VM tests are required (no test snapshots this
prose).

1. `just check-docs` -- SUMMARY.md parity + link integrity, including the
   `../../`-escape check. The two new links are single-`../` and resolve within
   `docs/`, so they pass.
2. Build with linkcheck: `nix develop .#docs -c mdbook build docs` (linkcheck2
   is configured in `docs/book.toml`; a broken cross-link fails the build).
3. Read-through: confirm the rewritten section matches the generated command in
   `modules/braid/auto-suspend.nix:87` (shape, qualified-paths claim,
   fail-closed inner `timeout`) and that no raw INI / `enabled = true` framing
   remains.
