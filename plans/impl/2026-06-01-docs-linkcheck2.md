# Fix: docs CI broken by nixpkgs 26.05 removing `mdbook-linkcheck`

## Context

After bumping the nixpkgs pin from 25.11 to 26.05 (commit `9d237f7`), the
`docs` GitHub workflow fails on push at its very first step
(`nix develop .#docs -c just check-docs`). Failing run:
https://github.com/danneu/braid/actions/runs/26770513123/job/78908429104

The error is a **Nix evaluation error**, not a docs-content error:

```
error: 'mdbook-linkcheck' has been removed and replaced by 'mdbook-linkcheck2'
       due to incompatibility with mdbook version 0.5.0+
```

In 26.05, `pkgs.mdbook-linkcheck` is a `throw`. `flake.nix` references it in the
`docsShellFor` devShell (`flake.nix:120`), so the `.#docs` shell can no longer
evaluate. Every step in `docs.yml` enters that shell, so the job dies on the
first one before `check-docs` even runs. (The `test.yml` workflow does not use
the docs shell and is unaffected.)

Intended outcome: docs CI evaluates and builds again, with the cross-link gate
configured in `docs/book.toml` still enforced -- now via the maintained
replacement `mdbook-linkcheck2`.

## Root cause / key facts (verified against the pinned toolchain)

- nixpkgs 26.05 ships `mdbook` `0.5.2`; the old `mdbook-linkcheck` is
  incompatible with mdbook `>=0.5.0` and is removed.
- Replacement package: `pkgs.mdbook-linkcheck2` (`0.12.0`), which requires
  mdbook `^0.5.1` -- compatible with `0.5.2`.
- **The replacement's binary is named `mdbook-linkcheck2`, not
  `mdbook-linkcheck`** (verified: `bin/mdbook-linkcheck2` only).
- mdbook invokes an output backend by running `mdbook-<output-table-name>`.
  So `[output.linkcheck]` would try to spawn the now-nonexistent
  `mdbook-linkcheck` -> the table must be renamed to `[output.linkcheck2]`
  (verified in `mdbook-linkcheck2` README: `output.linkcheck2` is canonical;
  `output.linkcheck` is only honored as a config-key fallback, which does not
  help because mdbook still derives the *command* from the table name).
- The `follow-web-links = false` option carries over unchanged
  (`mdbook-linkcheck2::run` deserializes the same config shape).
- Deploy path is unaffected: with two `[output.*]` tables (`html` +
  `linkcheck2`), mdbook still writes HTML to `docs/book/html`, which is exactly
  what `docs.yml` uploads.
- Not a parser-critical tool -> no fixture refresh obligation.

## Changes

1. **`flake.nix:120`** -- in `docsShellFor`, replace
   `pkgs.mdbook-linkcheck` with `pkgs.mdbook-linkcheck2`.

2. **`docs/book.toml`** -- rename the backend table:
   ```toml
   [output.linkcheck2]
   follow-web-links = false
   ```

3. **`AGENTS.md:110-111`** -- the living-doc prose currently reads:
   "validated by `mdbook-linkcheck` during `mdbook build docs` (configured in
   `docs/book.toml` per Decision 5)". Rewrite to name `mdbook-linkcheck2` and
   `docs/book.toml`, and **drop the "per Decision 5" clause entirely** -- ADR
   `005-sane-defaults.md` is "Sane Defaults" (status Active) and carries no
   mdBook/linkcheck contract, so the reference points future agents at the wrong
   authority. (The "Decision 5" label came from the docs-unification *plan's*
   internal numbering, not an ADR.) New text:
   "validated by `mdbook-linkcheck2` during `mdbook build docs` (configured in
   `docs/book.toml`) -- a broken cross-link fails CI."

### Explicitly out of scope

- `plans/impl/*.md` references to `mdbook-linkcheck` are historical
  implementation records -- do not rewrite them.
- No `flake.lock` change is needed; `mdbook-linkcheck2` already exists in the
  pinned 26.05 nixpkgs (confirmed by the verification build below).

## Verification

The `.#docs` shell evaluates on darwin, so this is fully verifiable locally.
Run the `docs.yml` `build` job's steps, in workflow order, each via the docs
shell (`.github/workflows/docs.yml`):

1. `nix develop .#docs -c just check-docs`
   -- the exact CI step that failed. Reaching it at all proves the devShell now
   evaluates (the original failure was the shell, not the recipe); it must also
   pass the `check-docs` logic.
2. `nix develop .#docs -c just check-docs-frontmatter`
3. `nix develop .#docs -c just check-code-doc-anchors`
4. `nix develop .#docs -c mdbook build docs`
   -- authoritative end-to-end check: confirms the `linkcheck2` backend is
   actually invoked (no "renderer command wasn't found" error) and that
   cross-link validation still runs and passes. HTML lands in `docs/book/html`.
5. `nix develop .#docs -c just check-docs-rendered-frontmatter`

These five commands are exactly what the CI `build` job runs, so a clean local
pass leaves no `build`-job gate unchecked. (Optionally push and confirm the
`docs` workflow goes green.)
