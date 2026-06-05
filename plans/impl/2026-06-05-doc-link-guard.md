# Plan: doc-link guard for AGENTS.md + README.md

## Context

The finding: AGENTS.md's `](docs/...)` pointers are validated by nothing.
Verified true, and it generalizes. Three doc-reference guards exist, each
covering one slice, and they leave the repo-root agent-facing files
uncovered:

- `mdbook-linkcheck2` -- validates `](...)` links *inside* `docs/` only
  (`docs/book.toml` sets `src = "."`). AGENTS.md and README.md sit at the
  repo root, outside the book src, so their links are invisible to it.
- `check-code-doc-anchors.py` -- validates `principles.md#anchor` cites in
  *any* textual form (code span, prose comment, markdown link) across
  `cli`/`tests`/`modules`/`AGENTS.md`/`README.md`/`prompts`. Target is
  principles.md only; a `](docs/dev/foo.md)` link matches nothing.
- `check-see-paths.py` -- validates backticked source paths in
  `docs/design/**` ADR `## See` sections. Path-existence only; it strips
  `#anchor`; scope is decision docs, not the root files.

The uncovered surface is exactly the `](target)` *markdown links* in
AGENTS.md and README.md. This plan adds the third member of the existing
guard family to close it, mirroring `check-see-paths.py` (standalone,
stdlib-only, nix-free CI job). It is the `](...)`-link complement to the
backticked-path guard that the sibling plan
`plans/wip/2026-06-02-doc-source-pointer-guard.md` shipped.

**Two things the finding's literal fix ("extend check-code-doc-anchors.py
to assert each `](path)` exists") gets wrong, which this pivot corrects:**

1. **It would regress the existing check.** `principles.md#anchor` is cited
   as code spans and prose comments -- not just `](...)` links -- in
   `cli/src/{doctor,lock,pool,remove}.rs` and
   `tests/module/pool-lock-precedes-state-read.py`. A markdown-link-only
   resolver bolted onto that script narrows its reach. The new check must
   be **additive**, in its own script, leaving the principles pass intact.

2. **Path-existence is not enough.** AGENTS.md carries three `#fragment`
   links (`doc-citations.md#decision-doc-references`,
   `luks-unlock.md#header-backup-workflow-and-messaging`,
   `reference-source.md#citing-reference-code`). A path-only check passes
   when a heading is renamed and the fragment rots -- the exact silent
   breakage the finding is about. The guard validates the **anchor** too,
   not just the file.

**CI teeth (the load-bearing discovery).** The check must live in a
workflow that runs when AGENTS.md/README.md change. `docs.yml` -- the only
home of `check-code-doc-anchors.py` -- is path-filtered to `docs/**`,
`flake.nix`, `flake.lock`, and its own workflow file. It does **not**
trigger on `AGENTS.md`, `README.md`, `cli/**`, `tests/**`, or `modules/**`.
A PR that edits AGENTS.md to add a broken pointer never touches `docs/`, so
`docs.yml` never runs -- a guard placed there would be dead on its primary
vector. `checks.yml` has no `paths:` filter and runs on every PR; it is
where `check-see-paths.py` already lives. The new guard goes there.

This same path-filter neuters the *existing* `check-code-doc-anchors.py`
on code-only PRs (a bad principles cite added to `lock.rs` without touching
`docs/` is not caught). Moving it to the same nix-free `checks.yml` lane is
a recommended companion fix (see CI wiring); it is separable from the core
guard if a minimal diff is preferred.

## Empirical inventory (basis for the design)

Scan of `](...)` links in the two target files (current tree):

| File | Internal links | -- with `#anchor` | External (URL) | Resolve today |
| --- | --- | --- | --- | --- |
| AGENTS.md | 19 (17 `docs/`, `README.md`, `justfile`) | 3 | 0 in link form | all pass |
| README.md | 33 (32 `docs/`, `LICENSE`) | 0 | badges (https) | all pass |

Counts are link *occurrences* (what the guard scans, since it does not dedup):
`docs/SUMMARY.md` is linked twice in AGENTS.md, so its 17 `docs/` occurrences
span 16 unique targets. All 19 AGENTS.md occurrences resolve on a fresh scan.

All three `#anchor` targets resolve now (`## Decision-doc references` ->
`decision-doc-references`; `## Header backup workflow and messaging` ->
`header-backup-workflow-and-messaging`; `## Citing reference/ code` ->
`citing-reference-code`). **The guard ships green -- no audit edits
required** (unlike the sibling plan, which had 3 broken source pointers to
fix first).

Slug-dedup risk is currently dormant: none of the three real anchor-target
docs (`doc-citations.md`, `luks-unlock.md`, `reference-source.md`) has
duplicate heading slugs, so mdbook's `-N` suffix never fires today. The
guard replicates it anyway (cheap, future-proof); this is defensive, not
load-bearing.

`prompts/` and `.claude/agents/` are deliberately out of scope: the former's
links are templates (`](./<slug>.md:<lineno>)`, `](./add.md:147)`) that are
not navigable doc references, and the latter has zero markdown links.

## Guard design: `scripts/docs/check-doc-links.py`

Standalone, stdlib-only (`re`, `sys`, `pathlib`), mirroring
`check-see-paths.py` structure (`ROOT = parents[2]`, failures list, stderr
print, exit code, `--selftest`).

**Scope:** an explicit list of repo-root, agent-facing markdown files
outside `docs/`:

```python
TARGETS = ["AGENTS.md", "README.md"]   # extend as new root docs appear
```

**What it validates:** every inline markdown link `](target)` in each
target file. Resolve the path relative to the *file's own parent dir*
(both targets are at ROOT, so parent == ROOT; coding it parent-relative
keeps it correct if a non-root file is ever added). Fenced code blocks are
skipped so `](...)` inside an example does not get checked.

```python
LINK = re.compile(r"\]\(([^)]+)\)")     # [text](t) and ![alt](t)
SKIP = ("http://", "https://", "mailto:", "tel:", "#")  # external / same-page-anchor handled separately

def classify(target):
    """('skip',_,_) | ('check', path, frag|None)."""
    url = target.strip().split()[0]          # drop optional "title"
    if url.startswith(("http://", "https://", "mailto:", "tel:")):
        return ("skip", None, None)
    path, _, frag = url.partition("#")
    return ("check", path, frag or None)
```

For each `("check", path, frag)`:

1. **Path existence.** If `path` non-empty and `not (file.parent / path).exists()`
   -> fail (`unresolved link path`). Directories (`docs/design/decisions/`)
   and non-md files (`justfile`, `LICENSE`) pass on existence.
2. **Anchor validity.** If the resolved target is a `.md` file and `frag` is
   set, fail if `frag not in anchors_of(resolved)` (`unresolved link
   anchor`). Pure same-page `#frag` (empty path) is checked against the
   current file's own anchors.

Both passes live behind one entry point, `lint_file(md_path) -> list[str]`,
which extracts links, classifies, and runs the two checks resolving against
`md_path.parent`. `main()` is just
`for name in TARGETS: failures += lint_file(ROOT / name)`, and `--selftest`
calls the **same** `lint_file` over a temp fixture tree (see Test cases) --
so the test drives the real scanner end-to-end, not the helpers in isolation.

**Anchor extraction** (all heading levels, mdbook dedup, code-fence aware):

```python
def anchors_of(md_path):
    seen, anchors, fenced = {}, set(), False
    for line in md_path.read_text(encoding="utf-8").splitlines():
        if line.lstrip().startswith("```"):
            fenced = not fenced
            continue
        if fenced:
            continue
        m = re.match(r"#{1,6}\s+(.*)", line)
        if not m:
            continue
        base = normalize_id(m.group(1))
        n = seen.get(base, 0)
        anchors.add(base if n == 0 else f"{base}-{n}")  # mdbook: 2nd dup -> -1
        seen[base] = n + 1
    return anchors
```

`normalize_id` is the byte-for-byte copy of the one in
`check-code-doc-anchors.py` (lowercase; keep alnum/`_`/`-`; spaces -> `-`),
so the two guards agree on what a valid anchor is.

**Non-goals (documented in the module docstring):**

- Not content accuracy -- a link can point at the right file with prose
  that misdescribes it; that is a human-audit concern.
- Not line numbers -- `](file.md:12)` style is not used by these files;
  a `:line` suffix (if ever added) is not validated.
- No overlap with `mdbook-linkcheck2` (which never sees root files) or
  `check-see-paths.py` (backticked code spans in ADR See sections, not
  `](...)` links).

## Open decision: share `normalize_id`/`anchors_of` or duplicate?

The established precedent is **self-contained, stdlib-only scripts** with no
shared module (`check-see-paths.py` duplicates its own path logic). Matching
it means copying `normalize_id` (8 lines) into the new script.

The robustness argument for a shared helper: `check-code-doc-anchors.py`
currently extracts **H2 only** (`line.startswith("## ")`) and has no dedup.
That is a latent gap -- a code cite to a `principles.md#some-h3` anchor would
false-fail today (no such cite exists yet, so it is dormant). A shared
`anchors_of(md_path)` (all levels + dedup) used by both scripts removes the
duplication *and* upgrades the principles check for free, with one canonical
slug implementation that cannot drift.

**Recommendation:** extract `scripts/docs/_mdslug.py` with `normalize_id` +
`anchors_of`, consumed by both scripts. Per AGENTS.md ("ideal, robust, most
correct -- regardless of churn") the one-canonical-slugger win outweighs the
new shared-module pattern.

**This route makes the `check-code-doc-anchors` move to `checks.yml` (below)
mandatory, not separable.** The extraction modifies
`check-code-doc-anchors.py`, but that script's only CI home (`docs.yml`) is
path-filtered to `docs/**`/`flake.*`/its own workflow file. The extraction PR
touches `scripts/docs/**` and `checks.yml` -- *not* `docs/**` -- so `docs.yml`
never triggers and the modified principles guard would ship unexercised by CI
(a slug/import regression could land green). Moving it to the all-PR
`checks.yml` lane is what guarantees it runs on its own PR.

**Fallback** (minimal churn, matches precedent): duplicate the two helpers
into the new script and leave `check-code-doc-anchors.py` untouched. Because
nothing about the existing script changes, the `checks.yml` move stays an
optional, independent cleanup under this route.

## CI wiring

**Local recipe** in `justfile` (next to the other doc checks, ~line 320),
self-test then live scan:

```make
# Verify ](...) links in AGENTS.md and README.md resolve (path + anchor)
check-doc-links:
    python3 scripts/docs/check-doc-links.py --selftest
    python3 scripts/docs/check-doc-links.py
```

**CI job** added to `.github/workflows/checks.yml` (no `paths:` filter ->
runs on every PR, the only lane that fires on AGENTS.md/README.md edits;
nix-free, mirrors the `docs-see-paths` job):

```yaml
  doc-links:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Self-test the link checker
        run: python3 scripts/docs/check-doc-links.py --selftest
      - name: Check AGENTS.md and README.md links
        run: python3 scripts/docs/check-doc-links.py
```

**Move `check-code-doc-anchors` to a nix-free `checks.yml` job.** Move the
existing invocation out of the path-filtered `docs.yml` (line 40); it is
stdlib-only, so it needs no `nix develop .#docs`. This closes its dormant
teeth-hole on code-only PRs *and*, under the shared-helper route, is the only
thing that exercises the now-modified script on its own PR (see Open
decision). **Required whenever the shared helper is used; separable** -- an
independent follow-up -- only under the duplicate fallback, which leaves
`check-code-doc-anchors.py` untouched.

## Test cases (`--selftest`)

`--selftest` builds a temporary fixture tree with `tempfile` (stdlib) and
runs the **same `lint_file()` the live scan uses** over it, asserting the
returned failure set is exactly the expected one. This is validator-level,
not helper-level: it catches the scanner silently ceasing to enforce path
existence, parent-relative resolution, fenced-code skipping, or anchor
failures -- regressions the live scan cannot surface while the real tree is
clean, and which a `classify()`/`anchors_of()`-only table would miss. (This
goes deliberately beyond the helper-table precedent of
`check-output-ascii.py` / `check-doc-source-pointers.py`.)

Fixture tree written under the temp dir:

- `target.md` -- the file under test; contains a `## Top section` heading and
  the links `a`..`i` below (one inside a fenced code block).
- `dep.md` -- contains `## Known heading` **twice** (anchor targets
  `known-heading` and, via dedup, `known-heading-1`).
- `sub/child.md` -- exists, to prove parent-relative resolution.

`lint_file(tmp/target.md)` must return a failure set of exactly `{g, h}`:

| Link in `target.md` | Expected |
| --- | --- |
| `[a](dep.md)` | pass -- path ok, no anchor |
| `[b](dep.md#known-heading)` | pass -- path + anchor ok |
| `[c](sub/child.md)` | pass -- **parent-relative** path resolves |
| `[d](#top-section)` | pass -- **same-page** anchor (target.md's own heading) |
| `[e](https://example.com)` | pass -- URL skipped |
| `[f](dep.md)` inside a fenced code block | pass -- **fenced code skipped** |
| `[i](dep.md#known-heading-1)` | pass -- **dedup** second occurrence |
| `[g](gone.md)` | **fail** -- unresolved path |
| `[h](dep.md#no-such-heading)` | **fail** -- unresolved anchor |

Asserting the set is exactly `{g, h}` -- not empty, not a superset -- pins
every enforcement step at once (path existence, parent-relative resolution,
anchor lookup, dedup, fenced-code and URL skipping). Self-test runs before the
live scan in both the recipe and CI, so a regression fails before the tree
scan (the `check-output-ascii.py` precedent).

## Risks / decisions to ratify

- **Anchor slug approximation.** `normalize_id` mirrors, but does not import,
  mdbook's slugger (not vendored in `reference/`). It is already trusted for
  principles.md; generalizing to other docs inherits that approximation.
  Punctuation in headings rides on this: the real link
  `reference-source.md#citing-reference-code` resolves only because
  `normalize_id` and mdbook agree to drop the `/` in `## Citing reference/
  code`. Mitigated by all-levels extraction + dedup + code-fence skipping; the
  only residual gap is explicit-id headings (`### Foo {#bar}`) -- grep confirms
  the target docs use none. Add `{#id}` handling only if one appears.
- **Script name.** `check-doc-links.py` chosen over generalizing/renaming
  `check-code-doc-anchors.py`, to match the one-guard-per-concern precedent.
- **Shared helper vs. duplicate** -- see Open decision above; recommended
  shared, fallback duplicate.
- **Move `check-code-doc-anchors` to `checks.yml`** -- required under the
  shared-helper route (it is the only lane that exercises the modified script
  on its own PR); separable only under the duplicate fallback.

## Implementation notes

- Took the recommended **shared-helper route**: added `scripts/docs/_mdslug.py`
  (`normalize_id` + `anchors_of`) and refactored `check-code-doc-anchors.py` onto
  it, swapping its H2-only `valid_anchors()` for `anchors_of(PRINCIPLES)` (now all
  heading levels + dedup). Per the Open decision this makes the `docs.yml` ->
  `checks.yml` move of `check-code-doc-anchors` mandatory; done. `_mdslug.py` is
  the first shared module under `scripts/docs/`. The refactor ships green
  (`check-code-doc-anchors.py` still passes) because the new anchor set is a
  superset of the old H2-only set.
- The `doc-links` job in `checks.yml` uses a single `run: |` block (selftest then
  scan), mirroring the adjacent `output-ascii` selftest-first job, rather than the
  plan's two-step snippet -- functionally identical, chosen for in-file consistency.
- `classify()` guards an all-whitespace `](   )` target (returns `skip`) instead
  of the plan's bare `.split()[0]`, which would `IndexError`; no real link hits
  this, it is defensive only.
- The selftest's cited precedent `check-doc-source-pointers.py` is not present in
  the tree, so the validator-level `--selftest` is modeled on the existing
  `check-output-ascii.py` (the real selftest-first precedent it follows).