# Plan: git-cliff-rendered GitHub release notes

> Canonical slug on promotion: `YYYY-MM-DD-git-cliff-release-notes.md`
> (this file lives at the harness-provided name in gitignored `plans/wip/`).

## Context

braid's release workflow currently builds the GitHub release body with
`gh release create --generate-notes` (`.github/workflows/release.yml#L92`),
which produces GitHub's own "What's Changed / PRs / new contributors" format.
We want the grouped conventional-commit changelog that danterm produces (e.g.
`https://github.com/danneu/danterm/releases/tag/v0.0.71`): sections like
`### Features` with `- *(scope)* Subject` lines.

That format is **git-cliff** output driven by a `cliff.toml` template -- not a
GitHub feature. danterm renders it in CI with
`nix run nixpkgs#git-cliff -- --current --strip all` and feeds the result to
`gh release create --notes-file`. This plan ports that mechanism to braid, with
two braid-specific choices:

- **Source:** pin git-cliff in the existing `.#release` devShell and invoke it as
  `nix develop .#release -c git-cliff ...` from both CI and `just changelog`. A
  bare `nix run nixpkgs#git-cliff` resolves `nixpkgs` from the *active flake
  registry* -- outside braid's `flake.lock`, and resolved per-environment: the
  highest-priority registry entry wins, so what it pulls varies across dev
  machines and CI. (On this machine `nix flake metadata nixpkgs` resolves to a
  nix-darwin system-registry pin, `path:/nix/store/...-source`, which *shadows*
  the lower-priority `global` `nixpkgs-unstable` entry -- so the unstable mapping
  is a non-effective fallback here, and a vanilla machine or a CI runner would
  resolve differently again.) braid's flake instead pins `nixos-26.05`. So local
  preview and CI could run different git-cliff versions, and a registry or
  upstream bump could change output -- or break the release step -- with no repo
  diff. Routing through `.#release` locks the renderer to braid's pinned nixpkgs,
  makes `just changelog` preview exactly what CI publishes, and makes the release
  shell the single release-tool boundary. Cost is negligible: release.yml's build
  + Rust-test steps already realize the Rust toolchain in that job, so `.#release`
  adds only git-cliff itself to the closure.
- **Scope:** broad for now -- conventional commit types render into named
  sections (Features, Bug Fixes, Performance, Documentation, Refactoring, Tests,
  CI, Build, Chores, Style, Reverts), and unmatched subjects land in Other. The
  cargo-release `chore(release): vX.Y.Z` bump is visible in Chores. This keeps
  the first iteration complete; the section set can be narrowed later if the
  release bodies feel noisy.

Optional add-ons (requested): a `just changelog` local preview recipe and the
doc updates that braid's ADR discipline requires.

## Goals / non-goals

Goals:
- GitHub release bodies render as the danterm-style grouped changelog, with
  conventional commit types split into stable sections and unmatched commits
  kept under Other.
- A local `just changelog` preview so notes can be eyeballed before `just release`.
- ADR 029 + `releasing.md` describe the new mechanism (no doc drift).

Non-goals (explicit):
- No committed `CHANGELOG.md`; notes are generated per-release only (matches danterm).
- No new devShell; git-cliff is added to the existing `.#release` shell rather
  than a bespoke one.
- No change to the release trigger, step ordering, guards, version SoT, cache
  push, or the `release`-branch fast-forward. Only the *notes* of the existing
  "Create GitHub release" step change.
- No commit-convention change. braid already uses Conventional Commits +
  lowercase first line (AGENTS.md); git-cliff's `upper_first` capitalizes the
  subject for display only.

## Changes

### 1. New file: `cliff.toml` (repo root)

The section decision is broad for the first iteration: named conventional commit
types render into stable groups, and a catch-all Other section keeps unmatched
commits visible. First-match-wins ordering: feat/fix/perf first, then the common
non-user-facing conventional types, then Other. The `<!-- N -->` prefix +
`striptags` filter forces section order (group_by would otherwise sort titles
alphabetically). Body template is copied verbatim from danterm (proven in
production).

```toml
# git-cliff config for braid release notes.
#
# release.yml runs `git-cliff --current` to render the just-tagged release range
# (commits since the previous v* tag) into the GitHub release body. `just changelog`
# runs `git-cliff --unreleased` to preview the next release's notes before tagging.
#
# Scope is deliberately broad for now: conventional commit types render into
# named sections, and anything that does not match a named type lands in Other.
# The release workflow still keeps an empty-notes fallback for a genuinely empty
# rendered range.

[changelog]
header = ""
body = """
{% for group, commits in commits | group_by(attribute="group") %}
### {{ group | striptags | trim }}

{% for commit in commits -%}
- {% if commit.scope %}*({{ commit.scope }})* {% endif %}\
{{ commit.message | split(pat="\n") | first | upper_first }}
{% endfor %}
{% endfor %}
"""
trim = true
footer = ""

[git]
conventional_commits = true
filter_unconventional = false
filter_commits = false
tag_pattern = "^v[0-9]+\\.[0-9]+\\.[0-9]+$"
topo_order = false
sort_commits = "oldest"

# First match wins. Named conventional commit types render into stable sections,
# then a catch-all Other group keeps unconventional commits visible. Sections
# render in declaration order via the `<!-- N -->` prefix striptags removes.
commit_parsers = [
    { message = "^feat(\\([^)]+\\))?!?:",     group = "<!-- 00 -->Features" },
    { message = "^fix(\\([^)]+\\))?!?:",      group = "<!-- 01 -->Bug Fixes" },
    { message = "^perf(\\([^)]+\\))?!?:",     group = "<!-- 02 -->Performance" },
    { message = "^docs(\\([^)]+\\))?!?:",     group = "<!-- 03 -->Documentation" },
    { message = "^refactor(\\([^)]+\\))?!?:", group = "<!-- 04 -->Refactoring" },
    { message = "^test(\\([^)]+\\))?!?:",     group = "<!-- 05 -->Tests" },
    { message = "^ci(\\([^)]+\\))?!?:",       group = "<!-- 06 -->CI" },
    { message = "^build(\\([^)]+\\))?!?:",    group = "<!-- 07 -->Build" },
    { message = "^chore(\\([^)]+\\))?!?:",    group = "<!-- 08 -->Chores" },
    { message = "^style(\\([^)]+\\))?!?:",    group = "<!-- 09 -->Style" },
    { message = "^revert(\\([^)]+\\))?!?:",   group = "<!-- 10 -->Reverts" },
    { message = ".*",                          group = "<!-- 99 -->Other" },
]
```

Note: `tag_pattern` matches braid's `vX.Y.Z` tags (same as the release.yml tag
guard). The catch-all Other parser is intentional in this first iteration; it
keeps release bodies complete until the project decides which sections, if any,
are too noisy.

### 2. Edit `flake.nix` -- pin git-cliff in the `.#release` devShell

Add `pkgs.git-cliff` to the `packages` list in `flake.nix#releaseShellFor` (which
currently holds `cargo-release`, `cargo`, `rustc`, `gh`, `git`, `just`). This is
the pin that makes CI and `just changelog` use the same, repo-locked renderer.

The existing `releaseShellFor` comment must be reworded in the same edit: it
currently claims the shell is "for the Mac-side `just release` bump **only**" and
that "The CI Rust test gate uses the default Linux devShell (craneLib), **not
this one**." CI now also uses `.#release` for the git-cliff notes step, so the
"only"/"not this one" framing is no longer true. Reword to: a release-tool
boundary used two ways -- the Mac-side bump and the CI release-notes step (pinned
git-cliff) -- while keeping the still-true statement that the CI *Rust test* gate
uses the default Linux devShell. Verified `.#release` resolves for CI's
`x86_64-linux` (it is defined per-system via `releaseShellFor system`).

### 3. Edit `.github/workflows/release.yml` -- "Create GitHub release" step (~L86-92)

Replace only the notes source; keep the idempotency guard and `--verify-tag`.

Before:
```yaml
      - name: Create GitHub release (guarded, idempotent -- before the FF)
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          tag="${GITHUB_REF_NAME}"
          gh release view "$tag" >/dev/null 2>&1 && echo "release $tag exists; skipping." \
            || gh release create "$tag" --generate-notes --verify-tag
```

After:
```yaml
      - name: Create GitHub release (guarded, idempotent -- before the FF)
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          tag="${GITHUB_REF_NAME}"
          if gh release view "$tag" >/dev/null 2>&1; then
            echo "release $tag exists; skipping."
          else
            notes="$RUNNER_TEMP/release-notes.md"
            nix develop .#release -c git-cliff --current --strip all --output "$notes"
            [ -s "$notes" ] || printf '_No notable changes._\n' > "$notes"
            gh release create "$tag" --title "$tag" --notes-file "$notes" --verify-tag
          fi
```

Why it works: checkout is at the tag with `fetch-depth: 0` (`release.yml#L37-40`),
so `--current` (the latest tag's range) has the history and tag it needs, and the
tagged `flake.nix` already carries the git-cliff pin from change #2 (committed
before any release). Under the default `bash -eo pipefail`, a git-cliff failure
aborts the step (no release with broken notes); the `[ -s ]` test sits behind
`||` so an empty file is the fallback path, not an `-e` abort.

### 4. Edit `justfile` -- add `changelog` recipe (after the `release` recipe, ~L225)

`nix develop .#release -c ...` self-enters the release shell, so the recipe is
runnable from any shell and uses the same pinned git-cliff CI does. `--unreleased`
(commits since the last tag, i.e. what the next release will contain) is the right
local-preview mode; CI uses `--current` on the just-created tag, which covers the
same range -- the matched pair (the no-tag bootstrap case is covered in
Verification):

```just
# Preview the release notes git-cliff will render for the next release (commits
# since the last v* tag), using the same pinned git-cliff CI publishes with.
# Run before `just release` to sanity-check the body.
changelog:
    nix develop .#release -c git-cliff --unreleased --strip all
```

Also (minor) extend the `release` recipe's header comment (~L195-199) to note the
body is git-cliff-rendered and previewable: after "creates the GitHub release",
add "(body rendered from conventional commits by git-cliff; preview with
`just changelog`)".

### 5. Edit `docs/design/decisions/029-release-process.md` (ADR, status Active)

- In the step-ordering paragraph (the one ending "...re-runnable from the Actions
  UI."), append one sentence: the GitHub release body is rendered by git-cliff
  (pinned in the `.#release` devShell, invoked `nix develop .#release -c
  git-cliff`) from `cliff.toml` -- conventional commit types grouped into stable
  sections such as Features, Bug Fixes, Documentation, Tests, CI, Build, and
  Chores, unmatched subjects kept under Other, and a genuinely empty rendered
  range getting the `_No notable changes._` placeholder. (This is the one place
  CI uses `.#release`; the Rust test gate still uses the default Linux devShell
  -- keep that wording consistent with the reworded `flake.nix` comment from
  change #2.)
- Add a `## See` bullet (after the `release.yml` bullet, repo-root-relative to
  satisfy `scripts/docs/check-see-paths.py`):
  `` - `cliff.toml` -- git-cliff template + commit-group config for the GitHub release-notes body. ``

Reference `cliff.toml` as a bare code span, not a link (it's outside the mdBook
root; AGENTS.md "File References" forbids linkifying it). No line numbers.

### 6. Edit `docs/dev/releasing.md` -- add a "Release notes" subsection

Insert after "## Normal release" (before "## The first release"):

```markdown
## Release notes

The GitHub release body is generated by git-cliff from commit subjects, grouped
by conventional-commit type (config in `cliff.toml`). Named types render into
stable sections such as Features, Bug Fixes, Documentation, Tests, CI, Build,
and Chores; anything unmatched lands in Other. A genuinely empty rendered range
gets a `_No notable changes._` placeholder.

Preview the next release's notes before tagging:

    just changelog

(renders commits since the last tag). The first release has no prior tag, so its
notes span the whole history; trim it afterward with `gh release edit <tag>` if
you like. Editing a release body never affects consumers -- the `release` branch
fast-forward is what publishes.
```

## Decisions / rationale (recap)

- **git-cliff pinned in `.#release`** (invoked `nix develop .#release -c
  git-cliff`): a bare `nix run nixpkgs#git-cliff` resolves `nixpkgs` from the
  active flake registry -- outside braid's `flake.lock` and resolved
  per-environment (highest-priority entry wins; here a nix-darwin system pin
  shadows the lower-priority `global` `nixpkgs-unstable` fallback, but another
  machine or a CI runner resolves differently), diverging from braid's pinned
  `nixos-26.05` and letting a registry or upstream bump change output -- or break
  the release step -- with no repo diff. Pinning
  makes `just changelog` preview exactly what CI publishes and makes `.#release`
  the single release-tool boundary. (Deliberately diverges from danterm, which
  uses the unpinned `nix run` form. Earlier "pinning gains nothing" reasoning was
  wrong: it conflated runtime-parser pinning (ADR 010) with release-step
  reproducibility -- a broken or drifting release step is a real failure.)
- **Broad scope for now**: per follow-up request. Named conventional commit
  types get stable sections, and unmatched subjects land in Other so the first
  iteration does not hide potentially useful release context. The section set can
  be narrowed later if it proves noisy.
- **No `CHANGELOG.md`**: notes are generated per-release; a committed changelog
  would be redundant maintenance.
- **First-release whole-history body**: inherent to `--current`/`--unreleased`
  with no prior tag (braid is at 0.0.0, first cut is v0.0.1). Acceptable; trim
  with `gh release edit` if desired. Documented in releasing.md.

## Future tunables (not doing now)

- Narrow the section set later by turning noisy types into `skip = true` entries
  before the catch-all Other parser.

## Verification

Prereq: stage the edits first (`git add flake.nix cliff.toml`) so `nix develop
.#release` evaluates the working tree's git-cliff pin (Nix dirty-tree eval needs
the change visible).

1. **Local preview, `--unreleased` mode (what `just changelog` runs).** braid has
   no tags yet, so `--unreleased` renders the entire history. From the repo root:
   ```sh
   just changelog          # nix develop .#release -c git-cliff --unreleased --strip all
   ```
   Confirm: exit 0 (validates `cliff.toml` parses and the pinned git-cliff runs);
   output includes all matching named sections in configured order, including
   non-feat/fix/perf sections such as Documentation, Tests, CI, Build, and
   Chores when commits exist; unmatched commits land in Other; scopes render as
   `- *(scope)* Subject` with capitalized subject; known feat/fix commits appear
   (e.g. `*(release)* Add cargo-release pipeline...`, `*(recover)* Fail closed
   on non-idle owed raid1 replay`).
2. **Release-mode smoke test, `--current` (the exact CI command).** CI uses
   `--current`, not `--unreleased`; with no real tag `--current` behaves
   differently, so exercise it faithfully against a HEAD-tagged checkout -- real
   repo and tags untouched via a temp clone, pinned git-cliff + working-tree
   `cliff.toml` supplied from the main tree:
   ```sh
   tmp="$(mktemp -d)"; git clone --quiet . "$tmp/r"; git -C "$tmp/r" tag v0.0.1
   nix develop .#release -c git-cliff \
     --repository "$tmp/r" --config cliff.toml --current --strip all
   rm -rf "$tmp"
   ```
   Assert the same section order and grouping as step 1 (for the first release
   this range == whole history, so the two outputs match).
3. **Empty-notes fallback (faithful).** A genuinely empty rendered range still
   gets the exact release.yml placeholder:
   ```sh
   notes="$(mktemp)"
   nix develop .#release -c git-cliff --config cliff.toml HEAD..HEAD --strip all --output "$notes"
   [ -s "$notes" ] || printf '_No notable changes._\n' > "$notes"   # the literal release.yml line
   cat "$notes"   # expect: _No notable changes._
   ```
4. **Docs gates.** After the ADR 029 / releasing.md edits:
   ```sh
   just docs-build              # mdbook build + mdbook-linkcheck2 cross-link gate
   just check-docs-see-paths    # validates the new `cliff.toml` See bullet resolves
   ```
5. **Workflow YAML sanity.** Confirm `release.yml` still parses (`actionlint
   .github/workflows/release.yml` if available, else a YAML load). The full CI
   path (`nix develop .#release` on the runner, `gh release create --notes-file`)
   can only be exercised end-to-end by the next real release tag -- call this out;
   it is not locally runnable.
6. **Diff scope.** `git status` shows exactly: new `cliff.toml`; modified
   `flake.nix`, `.github/workflows/release.yml`, `justfile`,
   `docs/design/decisions/029-release-process.md`, `docs/dev/releasing.md`. No
   `CHANGELOG.md`.

## Files touched

- `cliff.toml` (new)
- `flake.nix` (`git-cliff` in `releaseShellFor` packages + reworded comment)
- `.github/workflows/release.yml` (notes step only)
- `justfile` (`changelog` recipe + `release` header comment)
- `docs/design/decisions/029-release-process.md` (body sentence + `## See` bullet)
- `docs/dev/releasing.md` ("Release notes" subsection)
