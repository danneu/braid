# Mark experimental commands (🧪) with a single source of truth

## Context

The README, the mdBook sidebar (`docs/SUMMARY.md`), and the mdBook landing page
(`docs/index.md`) each list every braid command. We want experimental commands
(`recover`, `discover`, ...) visually flagged so readers can tell them from the
daily-driven core (`add`, `remove`, ...). braid is pre-v1.0; this is a maturity
signal within that, not a "works / does not work" flag.

The catch: the three command lists are already kept in lockstep by
`scripts/docs/check-doc-tables.py` (run in CI via `just check-docs`). It enforces
that all three surfaces carry **identical command labels in identical order**,
where order is canonical from `SUMMARY.md` and each label is derived from the
command page's H1 (`# braid add` -> `add`). Naively adding a status column or
reordering one surface breaks that check (already observed).

**Approach (single source of truth):** declare one boolean per command in the
command page's YAML frontmatter, and fold a `🧪` prefix into the canonical *label*
the checker already computes. Because the existing parity check forces all three
surfaces to match that canonical label byte-for-byte, the prefix fans out to the
sidebar and both tables automatically -- and CI fails if any surface drifts. The
mdBook sidebar has no column to hold a status, so a prefix on the command name
(`🧪 recover`) is the only shape that works uniformly across all three surfaces.

Decisions made with the user:
- **Model:** boolean `experimental: true|false`, set explicitly on **every**
  command page (no default) so the checker can fail closed when the key is missing
  or not boolean-like.
- **Render:** `experimental: true` -> label prefixed with `🧪 `;
  `experimental: false` -> bare command label, no prefix. (Only one emoji; solid
  commands are unmarked, so their labels are unchanged from today.)
- **Classification:**
  - Experimental (8): `monitor`, `ack`, `idle`, `enroll`, `ups status`,
    `seal-mountpoint`, `discover`, `recover`.
  - Non-experimental (9): `add`, `remove`, `remove-missing`, `replace`, `unlock`,
    `lock`, `status`, `doctor`, `tui`.
- **SSOT home:** command-page frontmatter (same convention `docs/design/decisions/*`
  and `docs/internals/*` use; stripped from rendered HTML by the existing
  `mdbook-yml-header` preprocessor).

## Working-tree note

Part 1 (frontmatter prepend) is **not yet applied** -- the command pages still
start with the `[← braid]` backlink, not `---` (an earlier prepend run was
reverted). Part 1 must run as part of implementation. Sequencing matters: if the
checker change (Part 2) lands before Part 1, every command page fails the new
`experimental` validation. The prepend script is idempotent (skips files that
already start with `---`), so re-running is safe.

`docs/commands/` is **not** clean: `add.md` and `recover.md` currently carry
unrelated edits (balance resume/cancel wording). Part 1 touches those same files,
so preserve those edits and keep them out of the stability commit unless you
intend to include them.

## Changes

### Part 1 -- source of truth: `experimental` frontmatter on each command page

For every `docs/commands/*.md`, prepend a frontmatter block above the existing
`[← braid](../index.md)` backlink:

```
---
experimental: false
---
[← braid](../index.md)

# braid add
...
```

Experimental (8) get `experimental: true`: `monitor`, `ack`, `idle`, `enroll`,
`ups-status`, `seal-mountpoint`, `discover`, `recover`. The other 9 get
`experimental: false`. (No `intent:` key -- command pages are
end-user docs, exempt from `check-frontmatter.py`, which only covers
`principles.md`, `design/decisions/*`, `internals/**`.)

Idempotent script:

```python
import pathlib
EXPERIMENTAL = {"monitor", "ack", "idle", "enroll", "ups-status",
                "seal-mountpoint", "discover", "recover"}
d = pathlib.Path("docs/commands")
for p in sorted(d.glob("*.md")):
    if p.read_text().startswith("---\n"):
        continue
    value = "true" if p.stem in EXPERIMENTAL else "false"
    p.write_text(f"---\nexperimental: {value}\n---\n{p.read_text()}")
```

### Part 2 -- teach the parity checker to read `experimental` and emit the label

`scripts/docs/check-doc-tables.py`:

- Add `EXPERIMENTAL_EMOJI = "🧪"` and a dependency-free frontmatter reader
  `read_experimental(path)` (`re.compile(r"\A---\n(.*?)\n---\n", re.DOTALL)` + scan
  for an `experimental:` scalar; return the raw value string or `None`). No `yaml`
  import (keep it standalone-runnable; the value is a simple scalar).
- Canonical command label becomes `f"{EXPERIMENTAL_EMOJI} {name}"` when
  experimental is true, else `name`, where `name = h1.removeprefix("braid ")`.
  Guides keep the bare H1 -- the `kind != "commands"` path is unchanged.
- In `collect_canonical`, for command pages read `experimental` and **fail closed**:
  emit a clear error if the key is missing or the value is not exactly `true` or
  `false` (e.g. `commands/foo.md: experimental frontmatter must be exactly true or
  false (got 'beta')`). Map `"true"` -> prefixed label, `"false"` -> bare.
- The existing `compare()` already requires SUMMARY/index/README labels to equal the
  canonical label, so no other checker logic changes -- the prefix is enforced
  everywhere for free.
- Update the module docstring's "Source-of-truth rules" to record that a command
  label is `🧪 + H1-name` when `experimental: true`, else the bare H1-name, and that
  the page frontmatter is the sole source of that flag.

The `check-docs` bash recipe *logic* needs no change: it extracts only hrefs
(`sed -n 's/.*](\([^)]*\.md\)).*/\1/p'`), so a prefix in labels does not affect the
SUMMARY-vs-disk membership check. But its inline comment is now stale -- update
the two lines in `justfile` that currently read "README.md / docs/index.md tables
must match SUMMARY.md order and use the H1-derived label for each guide/command."
Command labels are no longer purely H1-derived: they are the SUMMARY-canonical
order plus the canonical labels computed by `check-doc-tables.py`, which prefix
`🧪` onto commands whose page sets `experimental: true`.

### Part 3 -- add the `🧪` prefix to all three surfaces

Mechanical label edits (descriptions and ordering untouched; only the experimental
commands' link text gains the `🧪 ` prefix -- the 9 non-experimental commands are
unchanged). Must match the new canonical labels or CI fails.

- `docs/SUMMARY.md`, "# Commands" list: prefix only the 8 experimental bullets,
  e.g. `- [🧪 recover](commands/recover.md)`, `- [🧪 ups status](commands/ups-status.md)`;
  leave `- [add](commands/add.md)` etc. as-is.
- `docs/index.md`, "## Commands" table: prefix the same 8 label cells. Add a
  one-line legend above the table:
  `🧪 Experimental commands are less-trodden and more likely to have rough edges. All of braid is pre-v1.0.`
- `README.md`, "### Commands" table: same 8 label-cell prefixes + the same legend
  above the table.

Sidebar gets no legend line (a non-link line risks breaking mdBook's SUMMARY
parser); the prefix is self-evident and explained by the index/README legend.
Rebuild the two tables with reasonable column spacing -- alignment is cosmetic, not
validated.

### Part 4 -- leak guard (mandatory)

This change introduces a new frontmatter key, so the rendered-HTML guard must be
extended to cover it -- otherwise a `mdbook-yml-header` misconfig/regression could
render raw `experimental:` onto a command page with nothing in CI catching it (the
current guard only matches `intent|status`).
`scripts/docs/check-rendered-frontmatter.py`: extend the leak regex to
`(intent|status|experimental):`, matching how `intent`/`status` are already
guarded.

## Critical files

- `docs/commands/*.md` -- new `experimental: true|false` frontmatter (the SSOT).
- `scripts/docs/check-doc-tables.py` -- `read_experimental`, `EXPERIMENTAL_EMOJI`,
  prefixed canonical label, fail-closed validation, docstring.
- `justfile` -- refresh the stale `check-docs` recipe comment describing the label
  contract.
- `docs/SUMMARY.md`, `docs/index.md`, `README.md` -- `🧪`-prefixed labels for the 8
  experimental commands (+ legend on the two tables).
- `scripts/docs/check-rendered-frontmatter.py` -- extend the leak regex to cover
  `experimental`.

## Verification

1. `python3 scripts/docs/check-doc-tables.py` -> `doc-table parity ok`.
2. Negative tests (both required; revert after each): proves the two enforcement
   branches independently, since a single test could pass while leaving the other
   branch unproven.
   a. Set one command page to an invalid value (e.g. `experimental: beta`) and
      confirm `check-doc-tables.py` fails with the fail-closed validation error --
      exercises the new validation branch.
   b. Remove the `🧪 ` prefix from one experimental SUMMARY command label and confirm
      it fails with the label-mismatch error -- exercises the cross-surface parity
      branch.
3. `just check-docs` -> `docs check ok` (SUMMARY-vs-disk membership + parity).
4. Mandatory -- the `.#docs` devshell is cross-platform (`devShells.docs` is not
   gated by `isLinux`, so it works on this macOS host):
   `nix develop .#docs -c mdbook build docs` then
   `nix develop .#docs -c just check-docs-rendered-frontmatter` -> `rendered
   frontmatter check ok`. Confirms the new `experimental:` key does not leak into
   HTML and the sidebar/tables render the `🧪` prefix as part of each experimental
   command name.
