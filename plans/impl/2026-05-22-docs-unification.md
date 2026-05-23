# Plan: Docs unification (manual/ + docs/ -> docs/)

## Context

braid has two parallel documentation trees:

- `manual/` -- mdBook for end users (book.toml, SUMMARY.md, guides/, commands/, development.md).
- `docs/` -- flat tree for devs/agents (principles.md, decisions/, luks-unlock.md, tool-behavior/, real-world/, testing.md, plus historical notes).

Goal for v0.1: a single unified tree at `docs/` that compiles as an mdBook, supports Obsidian-vault interlinking for both human and agent navigation, and drops stale notes. End-user, developer, and agent material live side-by-side under one TOC.

The unified tree is named `docs/` (not `manual/`) deliberately: error messages in the Rust CLI quote doc paths verbatim (`docs/luks-unlock.md`, `docs/decisions/...`) and ~25 code comments + AGENTS.md reference `docs/...`. Keeping the `docs/` name minimizes path churn that doesn't need to change to achieve the goal. The `docs/decisions/` -> `docs/design/decisions/` move and the `docs/luks-unlock.md` -> `docs/internals/luks-unlock.md` move are intentional taxonomy changes, paid for in this one-time cleanup.

Cross-tree references from docs/ to repo-root files (`AGENTS.md`, `archive/`, `cli/`, `plans/`) are intentionally backticked prose paths rather than markdown links. docs/ is self-contained as an mdBook tree -- no links escape its src. Path-as-text is navigable for the audience of these cites (contributors with a local checkout) and avoids both the deployed-mdBook broken-link case (relative escapes 404 on the deployed site at `/braid/`, which doesn't contain `AGENTS.md`) and the context-breaking absolute-github-URL case (pulls the reader out of the mdBook mid-read, returns 404 for unauthenticated viewers while the repo is private, doesn't reflect uncommitted local edits). This shapes Step 4.5's cross-tree-cites table and is the reason no `check-docs-github-urls` gate is needed.

## Implementation tracking

Before editing, load the checklist below into your todo list tool and keep it updated as steps complete. Do not rely on memory for this migration: many steps are mechanical, cross-cutting, and order-sensitive.

## Implementation checklist

- [ ] Move files with `git mv` and update `.gitignore`.
- [ ] Rewrite `docs/book.toml`, `flake.nix`, `justfile`, and `.github/workflows/docs.yml`.
- [ ] Rebuild `docs/SUMMARY.md` and the `docs/index.md` landing page.
- [ ] Rewrite docs-internal links, escaped links, prose paths, and breadcrumbs.
- [ ] Rewrite Rust/Nix/Python comments and user-facing strings that cite moved docs.
- [ ] Rewrite `README.md`, `AGENTS.md`, `.claude/agents`, `.claude/memory`, and `prompts` references.
- [ ] Add and normalize frontmatter for `docs/design/`, `docs/internals/`, and `docs/design/principles.md`.
- [ ] Add `check-docs-frontmatter`, `check-docs-rendered-frontmatter`, and `check-code-doc-anchors` recipes and wire all three into the docs toolchain/CI path.
- [ ] Run verification gates:
  - [ ] `just check-docs`
  - [ ] `just check-docs-frontmatter`
  - [ ] `just check-code-doc-anchors`
  - [ ] `nix develop .#docs -c mdbook build docs`
  - [ ] `just check-docs-rendered-frontmatter`
  - [ ] stale-path audit
  - [ ] `just test-rust`
  - [ ] focused VM tests from the generated list
- [ ] Verify the GitHub Actions PR build and post-merge deploy behavior.

## Decisions (locked)

1. **`luks-unlock.md` placement**: move to `docs/internals/luks-unlock.md`. Consistent taxonomy; one-time error-string churn during v0.1 cleanup.
2. **Subtree name for principles + ADRs**: `docs/design/`.
3. **`principles.md` stays one file, moved to `docs/design/principles.md`**: code references retarget from `docs/principles.md:LINE` to `docs/design/principles.md#NN-anchor` -- more durable than line numbers and avoids cascading 33 ADR backlink rewrites that a per-file split would force. The per-file split was considered and rejected on cost/benefit grounds in review round 6.
4. **Stale files**: delete `docs/1-user-stories.md` and `docs/notes-calculating-used-free-total-pool-space.txt` outright (git history preserves them).
5. **mdbook-linkcheck**: add now. Hard guarantee on cross-link health pairs with the Obsidian-vault goal. mdbook-linkcheck is an **output backend only** (not a preprocessor); configure as `[output.linkcheck]`. Set `follow-web-links = false` so the build doesn't depend on github.com reachability or the repo's public visibility (braid is currently private; unauthenticated `blob/master/...` lookups return 404 even though the repo exists, so external link verification would fail CI on every push). No external link verification is needed because no external links are introduced by docs/ (cross-tree cites are backticked prose paths per the Context section's Option C note). Pin `mdbook` + `mdbook-linkcheck` in `flake.nix` devShell.
6. **Published URL**: `/braid/` (replaces current manual/ path). Keep `site-url = "/braid/"` in `book.toml`.
7. **Frontmatter format**: YAML frontmatter on agent-facing pages (`docs/design/`, `docs/internals/`) for Obsidian-native metadata. mdBook by default renders raw `---` blocks as `<hr/>` thematic breaks (so the YAML body leaks as visible text), so pin a frontmatter-stripping preprocessor. The available nixpkgs package is `mdbook-yml-header` (not `mdbook-yaml-header`). Configure it in `docs/book.toml` and include it on the mdBook process's PATH via the `nix develop .#docs` shell in Decision 8.
8. **Repo-local docs shell, used everywhere**: define a `devShells.<system>.docs` in `flake.nix` containing `mdbook`, `mdbook-linkcheck`, and `mdbook-yml-header`. Every `mdbook` call -- workflow, justfile recipes, verification gates -- enters that shell so the preprocessor and the linkcheck backend share `PATH` with `mdbook` and all three resolve against the repo's `flake.lock` (not the caller's registry). Canonical form: `nix develop .#docs -c mdbook <subcommand> docs`. The bare `nix shell nixpkgs#...` form is rejected because it pulls `nixpkgs` from the caller's flake registry instead of braid's pin, defeating the toolchain-pinning principle (Decision 10 in `docs/design/principles/`).

## Final tree

```
docs/
  book.toml                                     (moved from manual/)
  SUMMARY.md                                    (new, full TOC)
  index.md                                      (new landing; merged from manual/index.md + catalog blurb)

  guides/                                       (from manual/guides/, unchanged filenames)
    install-nixos.md, getting-started.md, day-to-day-nas-usage.md,
    auto-unlock.md, monitoring-and-alerts.md, power-management.md,
    fan-control.md, ups.md, nixos-configuration.md,
    sharing-and-permissions.md, mounting-subvolumes.md,
    troubleshooting.md, recovery-scenarios.md

  commands/                                     (from manual/commands/, unchanged filenames)
    add.md ... ups-status.md

  design/
    principles.md                               (from docs/principles.md, unchanged content; single file with 13 anchors `#NN-name`)
    decisions/
      001-..027-...md                           (from docs/decisions/, unchanged filenames)

  internals/
    luks-unlock.md                              (from docs/luks-unlock.md)
    tool-behavior/device-disappearance.md
    real-world/sata-hot-unplug.md
    btrfs/
      balance-profiles.md
      balance-soft.md
      enospc-vs-hang.md                         (renamed from claude-enospc-vs-hang.md)
      luks-sector-size.md                       (renamed from btrfs-luks-sector-size.md)

  dev/
    overview.md                                 (from manual/development.md)
    testing.md                                  (from docs/testing.md)
    tui-snapshots.md                            (renamed from docs/tui-insta-guide.md)
```

**Deleted outright**: `docs/1-user-stories.md`, `docs/notes-calculating-used-free-total-pool-space.txt`, `docs/index.md` (catalog role moves to SUMMARY.md).

## Execution steps

### Step 1 -- File moves (one commit)

Ordered script. Each block is independent; do not reorder within a block. Use `git mv` to preserve history.

```bash
# 1a. Remove the gitignored mdBook output dir (not tracked, so git won't see it).
rm -rf manual/book

# 1b. Create destination parent directories so `git mv` to new paths works.
mkdir -p docs/dev docs/design docs/internals/btrfs

# 1c. Stage deletions of files being replaced or dropped.
#     docs/index.md is replaced in Step 2; delete it before moving manual/index.md
#     onto the same path. Stale files are dropped outright per Decision 4.
git rm docs/index.md
git rm docs/1-user-stories.md
git rm docs/notes-calculating-used-free-total-pool-space.txt

# 1d. Move the manual/ tree into docs/.
git mv manual/guides docs/guides
git mv manual/commands docs/commands
git mv manual/development.md docs/dev/overview.md
git mv manual/book.toml docs/book.toml
git mv manual/SUMMARY.md docs/SUMMARY.md
git mv manual/index.md docs/index.md           # edited in Step 2

# 1e. Reorganize the existing docs/ subtree.
git mv docs/principles.md docs/design/principles.md
git mv docs/decisions docs/design/decisions
git mv docs/luks-unlock.md docs/internals/luks-unlock.md
git mv docs/tool-behavior docs/internals/tool-behavior
git mv docs/real-world docs/internals/real-world
git mv docs/testing.md docs/dev/testing.md
git mv docs/tui-insta-guide.md docs/dev/tui-snapshots.md
git mv docs/claude-enospc-vs-hang.md docs/internals/btrfs/enospc-vs-hang.md
git mv docs/btrfs-balance-profiles.md docs/internals/btrfs/balance-profiles.md
git mv docs/btrfs-balance-soft.md docs/internals/btrfs/balance-soft.md
git mv docs/btrfs-luks-sector-size.md docs/internals/btrfs/luks-sector-size.md

# 1f. Remove the now-empty manual/ directory.
rmdir manual

# 1g. Update .gitignore: change `manual/book/` to `docs/book/`.
```

### Step 2 -- Landing + index

Replace old `docs/index.md` (catalog) with a landing page patterned on `manual/index.md` (Common tasks / Guides / Commands tables) plus new Design / Internals / Development sections.

### Step 3 -- mdBook config

- `docs/book.toml`:
  - Keep `title = "braid"`, `site-url = "/braid/"`, `git-repository-url = "https://github.com/danneu/braid"`.
  - Add `[output.linkcheck]` (backend only -- there is no preprocessor variant of mdbook-linkcheck). Within that block, set `follow-web-links = false` per Decision 5 so external URL fetches don't gate the build.
  - Add `[preprocessor.yml-header]` per the `mdbook-yml-header` plugin's config block. Confirm with a smoke build that the preprocessor recognizes the `intent:` (and `status:` -- see Step 7) fields and strips the `---` delimiters.
- `flake.nix`: declare a new docs-only devShell. Critical structural constraint: the existing `devShells` block is gated to Linux only because the default shell pulls in `btrfs-progs`/`cryptsetup`/`nut`/`util-linux`, which do not evaluate on darwin (see the comment in `flake.nix` reading `Linux-only: the devShell pulls in btrfs-progs/cryptsetup/nut/util-linux`). The `docs` shell does NOT have that constraint -- `mdbook`/`mdbook-linkcheck`/`mdbook-yml-header` evaluate cleanly on both `aarch64-darwin` and `x86_64-linux`. Define `docs` for every `forAllSystems` entry, and keep only `default` behind the Linux-only condition. Shape:

  ```nix
  devShells = forAllSystems (
    system:
    let
      pkgs = nixpkgs.legacyPackages.${system};
      docsShell = pkgs.mkShell {
        packages = [
          pkgs.mdbook
          pkgs.mdbook-linkcheck
          pkgs.mdbook-yml-header
          pkgs.just                                              # runs check-docs + check-docs-frontmatter in CI
          (pkgs.python3.withPackages (ps: [ ps.pyyaml ]))        # parses source-side YAML frontmatter for the Step 8 gate
        ];
      };
      isLinux = builtins.match ".*-linux" system != null;
    in
    { docs = docsShell; }
    // (if isLinux then { default = devShellFor system; } else { })
  );
  ```

  After this change, `nix develop .#docs -c mdbook --version` must run on the maintainer's macOS workspace as well as the Linux CI runner. This is the toolchain entry point used by Decision 8's invocation form; if the shell is Linux-only, local mdbook gates in Step 8 fail before they can catch real problems.
- Write `docs/SUMMARY.md` covering: braid (landing) -> Guides -> Commands -> Design (Principles, Decisions) -> Internals -> Development.

### Step 3.5 -- Build and deploy surface

The existing build pipeline and just recipes are hard-coded to `manual/`. Migrate all of them in this step; the verification gate in Step 8 depends on them.

- `.github/workflows/docs.yml`:
  - **Trigger paths.** `on.push.paths`: `manual/**` -> `docs/**` (and keep `.github/workflows/docs.yml`). Add `flake.nix` and `flake.lock` to the path list too: Step 3.5 makes the build depend on the `nix develop .#docs` shell, so any change to the docs toolchain inputs must re-trigger the build -- otherwise a `flake.lock` bump can silently break the docs shell while the workflow stays quiet.
  - **PR trigger.** Add `on.pull_request:` with the same `paths` list (including `flake.nix`/`flake.lock`). This is what makes the build runnable from a working branch BEFORE merge: per the [GitHub Actions events docs](https://docs.github.com/actions/using-workflows/events-that-trigger-workflows), `workflow_dispatch` only fires when the workflow file is present on the default branch, so adding `workflow_dispatch` on the working branch is a chicken-and-egg. `pull_request` fires from the PR head ref, which is exactly what we need to verify the build before merging.
  - **Manual trigger.** Also add `on.workflow_dispatch:` (empty body) as a secondary trigger, useful AFTER the workflow lands on master for re-running deploys without touching docs/.
  - **Job split.** Critical structural change: the current workflow has a single `deploy:` job with `environment: github-pages` declared at job level (see `.github/workflows/docs.yml:20-22`). Job-level `environment:` makes every run that reaches the job create a deployment object and consult environment protection rules, even when the actual deploy *steps* are step-gated. Split into two jobs:

    ```yaml
    jobs:
      build:
        runs-on: ubuntu-latest
        permissions:
          contents: read
        steps:
          - uses: actions/checkout@v4
          - uses: DeterminateSystems/nix-installer-action@main
          - run: nix develop .#docs -c just check-docs
          - run: nix develop .#docs -c just check-docs-frontmatter
          - run: nix develop .#docs -c just check-code-doc-anchors
          - run: nix develop .#docs -c mdbook build docs
          - run: nix develop .#docs -c just check-docs-rendered-frontmatter
          - uses: actions/upload-artifact@v4
            with:
              name: docs-html
              path: docs/book/html

      deploy:
        if: (github.event_name == 'push' && github.ref == 'refs/heads/master') || github.event_name == 'workflow_dispatch'
        needs: build
        runs-on: ubuntu-latest
        concurrency:
          group: pages
          cancel-in-progress: true
        environment:
          name: github-pages
          url: ${{ steps.deployment.outputs.page_url }}
        permissions:
          pages: write
          id-token: write
        steps:
          - uses: actions/download-artifact@v4
            with:
              name: docs-html
              path: docs/book/html
          - uses: actions/configure-pages@v5
          - uses: actions/upload-pages-artifact@v3
            with:
              path: docs/book/html
          - id: deployment
            uses: actions/deploy-pages@v4
    ```

    Why this shape:
    - `build` has only `contents: read` and runs on every push/PR/dispatch -- no `environment:`, no Pages permissions. PR runs cannot create deployment objects or wait on environment protection rules because the deployment-shaped job is never reached.
    - `deploy` is job-level-gated with `if:` to either push-to-master OR `workflow_dispatch`, depends on `build` via `needs:`, and carries the `environment:` + Pages permissions only there. The `workflow_dispatch` arm is necessary because `workflow_dispatch` only fires from the default branch (per the GitHub Actions events docs), so its implicit ref is always master -- making the dispatch trigger a usable post-merge re-deploy path. Without the disjunct, the `workflow_dispatch` trigger added in the trigger block would build successfully and silently skip deploy, defeating the stated post-merge re-run purpose.
    - Step-level `if:` guards on individual Pages actions (the previous plan shape) are abandoned -- they don't suppress the job-level `environment:` side effect, which is the actual bug this finding catches.
    - `concurrency: group: pages` moves from workflow top level (the original location) onto the `deploy` job only, keeping `cancel-in-progress: true` there. Workflow-level placement would put PR `build` runs in the same group as master `deploy` runs; with `cancel-in-progress: true`, a PR push could cancel an in-flight master deploy. Scoping the group to `deploy` keeps the "one deploy at a time" guarantee while letting PR builds run unaffected.
  - **Build job steps.** Five pinned-toolchain calls in the `build` job, each entering the docs shell to inherit the flake.lock-pinned versions:
    1. `nix develop .#docs -c just check-docs` -- link/SUMMARY parity from the rewritten justfile recipe.
    2. `nix develop .#docs -c just check-docs-frontmatter` -- the Step 8 source-side YAML gate; pulls in pyyaml via the docs shell.
    3. `nix develop .#docs -c just check-code-doc-anchors` -- the Step 8 in-code anchor gate; reads heading anchors out of `docs/design/principles.md` (using mdBook's actual `id_from_content` algorithm) and asserts every `docs/design/principles.md#<anchor>` cite in `cli/`, `tests/`, `modules/`, `AGENTS.md`, `README.md`, `.claude/agents/`, `.claude/memory/`, and `prompts/` resolves to a real heading. Backstops Decision 3's "anchors are more durable than line numbers" claim -- without this, a future principle rename rots in-code citations silently because mdbook-linkcheck doesn't inspect source-tree citations.
    4. `nix develop .#docs -c mdbook build docs` -- the actual render, which also runs mdbook-linkcheck via the configured `[output.linkcheck]` backend.
    5. `nix develop .#docs -c just check-docs-rendered-frontmatter` -- the rendered-output leak check; must come AFTER `mdbook build docs` because it scans `docs/book/html`. Without this step in CI, a workflow run can pass the other gates, upload the artifact, and deploy HTML with visible YAML if the preprocessor configuration is removed, mis-configured, or stops stripping (e.g., an `mdbook-yml-header` version bump in a future `flake.lock` change). Recipe lives alongside `check-docs-frontmatter`; see Step 8 for the rg pattern.
    
    All five gates fail the workflow's `build` job before the artifact upload, so an invalid `status:` value, a broken cross-link, a stale in-code principle anchor, or a leaked frontmatter block blocks both PRs and master pushes -- not just PRs. There is no absolute-github-URL gate because docs/ introduces no such URLs (Option C, see Context).
  - **Upload path.** In the `deploy` job, `path: manual/book` -> `path: docs/book/html`. Required: enabling `[output.linkcheck]` moves the html backend output from `book/` into `book/html/` (mdBook puts each backend in its own subdir once there is more than one).
- `justfile`:
  - `docs` recipe: `nix run nixpkgs#mdbook -- serve manual --open` -> `nix develop .#docs -c mdbook serve docs --open`.
  - `check-docs` recipe: rewrite the bash body to scan `docs/` instead of `manual/` (the `find ... | sed 's|^manual/||'` and `manual/SUMMARY.md` and `grep -rn '...' manual/` lines). Drop the existing `../../` escape grep and its precedent comment: under Option C (Context section) no markdown links escape docs/ at all, so the grep always returns empty and the surrounding `fix: replace with https://github.com/danneu/braid/blob/master/<path>` advice is actively misleading. The rewritten recipe keeps the SUMMARY-parity scan only.

### Step 4 -- Principles link survival check

No principles split. Per Decision 3, `docs/principles.md` moves wholesale to `docs/design/principles.md`. ADR backlinks of the form `../principles.md#NN-anchor` survive untouched because both files moved one level deeper into `docs/design/`, so `../` still resolves correctly inside the same subtree. mdbook-linkcheck verifies this in Step 8 -- no rewrite table needed.

### Step 4.5 -- Docs-internal link rewrites

After Step 1, intra-docs links break in several systematic ways. Rewrite them so mdbook-linkcheck passes and Obsidian backlinks resolve.

**Tree-relocation rewrites:**

| File | Old link | New link |
|---|---|---|
| `docs/internals/luks-unlock.md` (post-move) | `../manual/guides/recovery-scenarios.md` | `../guides/recovery-scenarios.md` |
| `docs/internals/luks-unlock.md` (post-move) | `decisions/020-ups-integration.md` (now `../design/decisions/...`) | `../design/decisions/020-ups-integration.md` |
| `docs/design/decisions/018-systemd-lifecycle.md` (post-move) | prose mention `manual/guides/sharing-and-permissions.md#binding-shares-to-the-pool-lifecycle` | `../../guides/sharing-and-permissions.md#binding-shares-to-the-pool-lifecycle` |
| `docs/guides/mounting-subvolumes.md:98` (post-move) | `https://github.com/danneu/braid/blob/master/docs/decisions/018-systemd-lifecycle.md` (absolute URL pointing at the old ADR location) | `../design/decisions/018-systemd-lifecycle.md` -- relative link now that the ADR lives inside the same mdBook tree; the absolute github URL was only there because the guide used to be outside docs/. Target is inside docs/, so Option C (Context) does not apply -- this stays as a markdown link. |
| `docs/dev/overview.md` (post-move from `manual/development.md:1`) | `[← Manual](index.md)` | `[← braid](../index.md)` -- the old guide sat at `manual/development.md` with a sibling `manual/index.md`, so a bare `index.md` resolved correctly. After moving to `docs/dev/overview.md`, the sibling is gone; the landing is one level up at `docs/index.md`. |

The three previously-listed AGENTS.md rewrites (`docs/design/principles.md` line 121, `docs/dev/testing.md:7`, `docs/dev/testing.md:72`) target `AGENTS.md` at the repo root, OUTSIDE docs/, so Option C (Context) applies -- they convert to backticked prose paths instead of markdown links. See the "Cross-tree cites -> backticked prose paths" section below for the rewrite forms.

**Breadcrumb sweep (manual/ -> docs/ branding):**

Every moved `manual/{commands,guides}/*.md` page begins with a first-line breadcrumb of the form `[← Manual](../index.md)`. The link target is fine (the new landing sits at `docs/index.md`, which is still one level up from `docs/{commands,guides}/`), but the visible label still says "Manual" -- the rendered unified docs would present 29 pages as the old manual.

Sweep every moved page's first line and rewrite the label `← Manual` to `← braid` (matching the precedent set for `docs/dev/overview.md` above). Driver:

```bash
rg -l '\[← Manual\]\(\.\./index\.md\)' docs/commands docs/guides
```

Apply: replace `[← Manual](../index.md)` with `[← braid](../index.md)` in every hit. Spot-checked count via `rg -rln '\[← Manual\]' manual/ | wc -l` before the move: 29 files total carry the `[← Manual]` label, but the prescribed driver query hits 28 of them -- not 29 -- because `manual/development.md` (which becomes `docs/dev/overview.md`) uses the link form `[← Manual](index.md)` (no `../` prefix), not `[← Manual](../index.md)`. The development.md case is handled separately by the Step 4.5 tree-relocation table (it rewrites `[← Manual](index.md)` -> `[← braid](../index.md)`). Post-sweep, `rg -n '← Manual' docs/` must come back empty.

**Prose-path rewrites (backticked text, not markdown links -- mdbook-linkcheck doesn't catch these):**

Several ADRs and the internals docs cite other docs in prose using pre-move paths. Run:

```bash
rg -n 'docs/(decisions/|principles\.md|luks-unlock\.md|tool-behavior|testing\.md)' docs/
```

Known hit sites (rewrite each to the post-move path):

| File | Old prose path | New prose path |
|---|---|---|
| `docs/internals/luks-unlock.md` (post-move) | `docs/luks-unlock.md` (lines 151, 180 -- pinned error strings embedded verbatim in the doc) | `docs/internals/luks-unlock.md` -- must re-pin in lockstep with the Step 5 string updates so the doc-quoted form matches the in-code form |
| `docs/design/decisions/022-dry-run-preview-model.md` (post-move) | `docs/decisions/012-intent-cli.md` (line 109, prose backtick ref) | `docs/design/decisions/012-intent-cli.md` |
| `docs/design/decisions/019-inhibit-sleep.md` (post-move) | `docs/decisions/018-systemd-lifecycle.md:131` (line 154, prose backtick ref) | `docs/design/decisions/018-systemd-lifecycle.md:131` |
| `docs/design/decisions/019-inhibit-sleep.md` (post-move) | `docs/luks-unlock.md` (line 210, prose backtick ref) | `docs/internals/luks-unlock.md` |
| `docs/design/decisions/008-unified-cli.md` (post-move) | `docs/decisions/002-config-first-workflow.md`, `009-safe-by-construction-reconciliation.md`, `007-disk-pool-management.md` (lines 83-85, prose backtick refs) | `docs/design/decisions/...` for each |
| `docs/design/decisions/007-disk-pool-management.md` (post-move) | `docs/decisions/002-config-first-workflow.md` (line 115, prose backtick ref) | `docs/design/decisions/002-config-first-workflow.md` |
| `docs/design/decisions/021-wait-in-unlock.md` (post-move) | `docs/principles.md:3` (line 47, prose backtick ref) | `docs/design/principles.md` (drop the `:3` line ref; the body of the cite already names the principle by title) |

**Cross-tree cites -> backticked prose paths (Option C):**

Per the Context section's Option C note, docs/ does not contain markdown links escaping its tree, and it does not contain absolute github URLs back to its own repo. Every cite from inside docs/ to a repo-root file (`AGENTS.md`, `archive/`, `cli/`, `plans/`) becomes a backticked prose path with no link affordance. The contributor audience for these cites has a local checkout; the path-as-text is just as discoverable as a clickable link (open in editor, type into shell) without the broken-deployed-link / context-break / private-repo-404 downsides.

Seven sites total. The "New form" column gives prose that reads naturally without the link affordance; adjust the surrounding sentence to match.

| Site | Old form | New form |
|---|---|---|
| `docs/design/principles.md` (end, currently around line 121) | `Implementation workflow and conventions are in [AGENTS.md](../AGENTS.md).` | ``Implementation workflow and conventions are in `AGENTS.md` at the repo root.`` |
| `docs/dev/testing.md:7` | `The short three-bullet preamble contract (Intent / Why it exists / Scenario) lives in [AGENTS.md](../AGENTS.md); everything else ... is here.` | ``The short three-bullet preamble contract (Intent / Why it exists / Scenario) lives in `AGENTS.md` at the repo root; everything else ... is here.`` |
| `docs/dev/testing.md:72` | `see [AGENTS.md](../AGENTS.md#parser-compatibility)` | ``see `AGENTS.md` (Parser Compatibility section)`` |
| `docs/design/decisions/003-resilient-boot.md` (sources list, currently `:60`) | `- [archive/plans/test-boot-degraded.md](../../archive/plans/test-boot-degraded.md) -- original plan and research` | ``- `archive/plans/test-boot-degraded.md` -- original plan and research`` |
| `docs/design/decisions/002-config-first-workflow.md` (sources list, currently `:60`) | `- [archive/design-docs/1-nixos-best-practices.md](../../archive/design-docs/1-nixos-best-practices.md) -- original best practices analysis` | ``- `archive/design-docs/1-nixos-best-practices.md` -- original best practices analysis`` |
| `docs/design/decisions/014-alerts.md` (body, inline, currently `:57`) | `` `AlertPoolState::recognized_devids` ([cli/src/probe.rs](../../cli/src/probe.rs)) returns... `` | `` `AlertPoolState::recognized_devids` (in `cli/src/probe.rs`) returns... `` |
| `docs/design/decisions/022-dry-run-preview-model.md` (sources list, currently `:111`) | `- [plans/impl/2026-05-06-unify-cli-plan-execution.md](../../plans/impl/2026-05-06-unify-cli-plan-execution.md) -- historical implementation plan...` | ``- `plans/impl/2026-05-06-unify-cli-plan-execution.md` -- historical implementation plan...`` |

**Full audit pass:** after the prose-path conversions, `grep -rn '](\.\./\.\./' docs/` must return empty (no markdown links escaping docs/). `rg 'https://github\.com/danneu/braid/blob/master/' docs/` must also return empty (no absolute self-links). mdbook-linkcheck during `mdbook build docs` is the backstop for everything else.

### Step 5 -- Code reference rewrites

**Pure comment churn (no behavior change):**

Line-number anchors below are quoted fragments (substrings of the actual comment), not file:LINE pairs -- the codebase drifts between writing the plan and executing it, and a quoted fragment survives a 30-line drift. Confirm with `rg -n '<fragment>' <file>` when applying.

| File | Old fragment | New fragment |
|---|---|---|
| `cli/src/remove.rs` ("docs/principles.md:23") | `docs/principles.md:23` | `docs/design/principles.md#3-safe-by-construction-operations` |
| `cli/src/doctor.rs` ("docs/principles.md:21") | `docs/principles.md:21` | `docs/design/principles.md#3-safe-by-construction-operations` |
| `cli/src/replace.rs` (bare "docs/principles.md") | `docs/principles.md` | `docs/design/principles.md` (no anchor -- the cite refers to principles as a whole) |
| `cli/src/pool.rs` ("principle 3, docs/principles.md:23") | `docs/principles.md:23` | `docs/design/principles.md#3-safe-by-construction-operations` |
| `cli/src/lock.rs` ("docs/principles.md:18") | `docs/principles.md:18` | `docs/design/principles.md#3-safe-by-construction-operations` (justification: line 18 in current `docs/principles.md` is blank -- the `:18` cite is already drifted. The surrounding in-code comment says "crash between cryptsetup open and pool.json write," which matches principle 3's "Post-commit persist with journal" bullet about journal-guarded mutations between disk operation and `pool.json` write. Spot-check the surrounding comment context before committing the rewrite.) |
| `tests/module/pool-lock-precedes-state-read.py` ("principle 12 (docs/principles.md)") | `docs/principles.md` | `docs/design/principles.md#12-one-pool-operation-at-a-time` |
| All `docs/decisions/N-...md` in `cli/`, `tests/`, `modules/braid/`, `AGENTS.md` (scope includes `cli/src/tui/` -- e.g. `cli/src/tui/app.rs` cites `docs/decisions/015-hdd-defaults.md` and `016-auto-suspend.md`) | `docs/decisions/N-...md` | `docs/design/decisions/N-...md` |
| `cli/src/status.rs` ("docs/tool-behavior/device-disappearance.md") | `docs/tool-behavior/device-disappearance.md` | `docs/internals/tool-behavior/device-disappearance.md` |
| `tests/repro/btrfs-remove-enospc.nix` ("docs/claude-enospc-vs-hang.md") | `docs/claude-enospc-vs-hang.md` | `docs/internals/btrfs/enospc-vs-hang.md` |
| `tests/repro/btrfs-remove-enospc-crash.nix` ("docs/claude-enospc-vs-hang.md") | `docs/claude-enospc-vs-hang.md` | `docs/internals/btrfs/enospc-vs-hang.md` |
| `tests/module/subvol-mount-lifecycle.nix` ("manual/guides/mounting-subvolumes.md") | `manual/guides/mounting-subvolumes.md` | `docs/guides/mounting-subvolumes.md` |
| `tests/module/subvol-mount-lifecycle.py` ("manual/guides/mounting-subvolumes.md") | `manual/guides/mounting-subvolumes.md` | `docs/guides/mounting-subvolumes.md` |

**User-facing error strings (touches pinned tests; expect insta/golden updates):**

| File | Old fragment | New fragment |
|---|---|---|
| `cli/src/journal.rs` (4 strings + 1 const `PENDING_OP_MANUAL_REMEDIATION` + 1 doc comment that asserts the pinned-string contract -- the doc comment reads "`Display` text is pinned verbatim (so `docs/luks-unlock.md` can quote..." and is the canonical author-side claim that the strings appear in `docs/luks-unlock.md`; rewriting strings without rewriting the doc comment leaves a silent lie behind) | `docs/luks-unlock.md` | `docs/internals/luks-unlock.md` |
| `cli/src/discover.rs` (5 strings) | `see docs/luks-unlock.md` | `see docs/internals/luks-unlock.md` |
| `cli/src/membership.rs` (2 strings) | `see docs/luks-unlock.md` | `see docs/internals/luks-unlock.md` |
| `cli/src/recover.rs:67` | `docs/luks-unlock.md and manual/guides/recovery-scenarios.md` | `docs/internals/luks-unlock.md and docs/guides/recovery-scenarios.md` |
| `cli/src/recover.rs:1963` | `manual/guides/recovery-scenarios.md` | `docs/guides/recovery-scenarios.md` |
| `modules/braid/options.nix:55` | `docs/luks-unlock.md` (in option description) | `docs/internals/luks-unlock.md` |

After string updates, run `just test-rust` and `cargo insta review` (or `accept` if you trust the diff) to refresh pinned snapshots. Then run the VM tests that exercise the touched error-string paths. `just test-vm` does not glob-expand check names, so list them explicitly. Generate the current list from the flake before running:

```bash
nix eval --json .#checks.aarch64-darwin --apply 'cs: builtins.attrNames cs' \
  | jq -r '.[] | select(test("^braid-(discover|recover|doctor|add|remove)"))'
```

Run `just test-vm <name1> <name2> ...` against the listed names verbatim.

### Step 6 -- README.md, AGENTS.md, and agent/prompt sweep

- `README.md`: rewrite all `manual/commands/...` and `manual/guides/...` link targets to `docs/commands/...` / `docs/guides/...` (~14 links).
- `AGENTS.md`: do not work from an enumerated count -- AGENTS.md has at least 7 distinct stale paths spread over 7+ lines (with several lines carrying both a markdown link and its label form, so each hit has two textual occurrences). Drive the rewrite with:

  ```bash
  rg -n 'docs/(principles\.md|decisions/|luks-unlock\.md|index\.md|testing\.md|tool-behavior|real-world|tui-insta-guide\.md|btrfs-balance|btrfs-luks-sector-size|claude-enospc)' AGENTS.md
  ```

  Rewrite every hit per these mappings (all are label-and-link pairs on the same line, so each hit produces two textual edits):
  - `docs/principles.md` -> `docs/design/principles.md`
  - `docs/decisions/` -> `docs/design/decisions/`
  - `docs/decisions/018-systemd-lifecycle.md` -> `docs/design/decisions/018-systemd-lifecycle.md`
  - `docs/decisions/022-dry-run-preview-model.md` -> `docs/design/decisions/022-dry-run-preview-model.md`
  - `docs/index.md` -- file is being deleted; retarget the cross-link to point at the new `docs/index.md` (rewritten landing page in Step 2) or to `docs/SUMMARY.md`, whichever the surrounding sentence intends. Read the line in context before rewriting.
  - `docs/luks-unlock.md` -> `docs/internals/luks-unlock.md`
  - `docs/testing.md` -> `docs/dev/testing.md`

  After the path rewrites, perform three structural section rewrites to bring AGENTS.md's framing in line with the unified tree. The path-rewrite rg sweep above handles scattered path mentions in prose; these rewrites are surgical to named sections:

  - **Layout section** (currently the `## Layout` block listing top-level dirs): replace the single ``- `docs/decisions/` — architecture decision records`` line with a nested entry covering all five docs/ subtrees:

    ```markdown
    - `docs/` — unified mdBook docs (single TOC at `docs/SUMMARY.md`, landing at `docs/index.md`)
      - `guides/`, `commands/` — end-user material (formerly under `manual/`)
      - `design/principles.md`, `design/decisions/` — architecture authority
      - `internals/` — implementation notes (luks-unlock, tool behavior, btrfs deep-dives)
      - `dev/` — contributor docs (development workflow, testing, TUI snapshots)
    ```

    The "formerly under `manual/`" parenthetical is intentional: AGENTS.md has no current `manual/` references (the path-rewrite rg pattern correctly doesn't search for any), but a contributor reading post-migration AGENTS.md needs to know where end-user material moved if they encounter a stale `manual/...` mention in code, git history, or older PRs.

  - **User Guide section** (currently `## User Guide` + one paragraph saying README.md is the end-user guide): rewrite the paragraph to reflect that end-user material is now split across two surfaces:

    ```markdown
    End-user material lives in two places: [`README.md`](README.md) is the cookbook-style overview
    (brief, copy-paste examples), and `docs/guides/` + `docs/commands/` is the mdBook reference
    (formerly `manual/`). Keep both in sync when adding features or changing behavior. Style for
    README.md: brief, cookbook-like — short descriptions with copy-paste examples. Not reference
    material.
    ```

    The README-style guidance from the existing paragraph is preserved verbatim in the last two sentences; only the framing of which docs are end-user-facing changes.

  - **Documentation section** (currently `## Documentation` + one paragraph saying `docs/index.md` is the directory): rewrite the paragraph to reflect the new TOC/landing split and the unified-tree scope:

    ```markdown
    [`docs/SUMMARY.md`](docs/SUMMARY.md) is the TOC for the unified docs tree (end-user guides,
    commands, design principles, ADRs, internals, contributor docs). [`docs/index.md`](docs/index.md)
    is the landing page. Check `SUMMARY.md` before searching the codebase for context. All cross-links
    inside `docs/` are validated by `mdbook-linkcheck` during `mdbook build docs` (configured in
    `docs/book.toml` per Decision 5) -- a broken cross-link fails CI.
    ```

    The linkcheck-backed cross-link guarantee folds in here as the closing sentence rather than living in a separate tree-map paragraph.
- `.claude/agents/command-reviewer.md`: rewrite `docs/principles.md` -> `docs/design/principles.md` (with `#NN-anchor` when the rule cites a specific principle), `manual/commands/` -> `docs/commands/`, `docs/decisions/*.md` -> `docs/design/decisions/*.md`.
- `prompts/command-review-fanout.md`: `ls manual/commands/` -> `ls docs/commands/`, `manual/commands/<slug>.md` -> `docs/commands/<slug>.md`, `docs/decisions/024-luks-uuid-identity.md` -> `docs/design/decisions/024-luks-uuid-identity.md`.
- `.claude/memory/feedback_docs_at_contract_level_not_impl_names.md`: `docs/principles.md` -> `docs/design/principles.md`, `docs/decisions/*` -> `docs/design/decisions/*`.

### Step 7 -- Frontmatter pass

Two YAML keys are required in this migration:

- **`intent:`** -- a one-line statement of the file's purpose, used by agents for browse-time triage.
- **`status:`** -- the document's lifecycle state. Must be exactly one of the enum values defined by `AGENTS.md`'s `## Architecture Authority` section: `Active`, `Superseded`, `Draft`, or `Deprecated`. Anything else (including a qualifier like `Superseded by [012-intent-cli.md](012-intent-cli.md)` or `Active -- Refines [017-runtime-disk-membership.md](017-runtime-disk-membership.md)`) makes the field stop being an enum and lets the gate added in Step 8 catch the typo class.

Current ADR state, verified by spot-checking `docs/decisions/`: every ADR has `Status:` as prose at the top of the body (e.g., `Status: Active`); roughly half of those carry a qualifier on the same prose line (`Superseded by [012-intent-cli.md](012-intent-cli.md)`, `Active -- Supersedes [002-config-first-workflow.md](002-config-first-workflow.md). Refined by [024-luks-uuid-identity.md](024-luks-uuid-identity.md).`, `Superseded by [Principle 13](../principles.md#13-announce-long-running-work)`). Only ~10 ADRs (014, 019, 020, 021, 022, 023, 025, 026, 027) plus `docs/tool-behavior/device-disappearance.md` carry a YAML frontmatter block at all, and only `device-disappearance.md` has YAML `status:`. The rest have YAML `intent:` with prose `Status:` below. Treat this as the baseline.

**Normalization rule (load-bearing):** when moving prose `Status: <line>` into YAML, normalize to only the enum value. Preserve any qualifier as body prose immediately after the `---` block so supersession/refinement links survive. Concrete example:

Before:

```
---
intent: ...
---

# Title

Status: Superseded by [012-intent-cli.md](012-intent-cli.md)
```

After:

```
---
intent: ...
status: Superseded
---

# Title

> Superseded by [012-intent-cli.md](012-intent-cli.md).
```

The blockquote-prefixed sentence is the post-move home for the qualifier. (Body prose, not YAML, so markdown links render normally.) `Active -- Supersedes ... Refined by ...` lines split the same way: `status: Active` plus a one-line blockquote that preserves both link clauses. Treat the qualifier as part of the doc's history -- never drop it.

Per-file work:

- `docs/design/principles.md`: add a frontmatter block with `intent:` describing the invariant the file expresses. No `status:` needed -- principles are not lifecycle-versioned.
- `docs/design/decisions/*.md`: ensure each has a single `---` block at the top with both `intent:` (add where missing) and `status:` (always add -- move the enum value out of the existing prose `Status: <X>` into the YAML key, preserve any qualifier as a blockquote per the rule above, then delete the original prose `Status:` line). The prose `Status:` line must not survive; otherwise the rendered page shows both YAML and the leftover prose. **Bold-wrapped variants**: ADRs 009, 011, and 012 currently use bold-prefixed `**Status: ...**` (sometimes with the closing `**` mid-line, as in 012's `**Status: Active** -- Supersedes ...`). The prose-removal step must also strip these bold-wrapped forms, not just plain `Status:` lines -- match with a regex like `^\s*\**\s*Status:` (or just visually verify each of 009, 011, 012 after the rewrite). Missing the bold form leaves visible `**Status: Active**` text in the body alongside the new YAML key.
- `docs/internals/*.md` (luks-unlock + tool-behavior + real-world + btrfs subtree): ensure each has `intent:` and `status:` (set to `Active` unless the file's body says otherwise). Same prose-removal and normalization rules apply.

Guides and commands skip frontmatter -- they're audience-clear.

The frontmatter-stripping preprocessor configured in Step 3 (Decision 7) is what makes this safe to ship in rendered HTML. After this step, the Step 8 mdBook build acts as the gate: it must produce HTML with no leaked YAML keys. A second Step 8 gate (added below) parses source-side frontmatter and enforces both presence and enum validity -- the rendered-HTML gate alone doesn't catch invalid enum values, because mdbook-yml-header strips the YAML before render whether or not the value is valid.

### Step 8 -- Verification gates

- `just check-docs` (post-rewrite of the recipe) -> green: SUMMARY.md is in parity with the docs/ tree and no markdown link escapes the subtree.
- `nix develop .#docs -c mdbook build docs` -> clean. mdbook-linkcheck reports zero broken cross-links.
- **Rendered frontmatter gate.** Wired as a just recipe `check-docs-rendered-frontmatter` -- invoked locally as `nix develop .#docs -c just check-docs-rendered-frontmatter`, and in CI as the fourth step of the workflow's `build` job (Step 3.5), placed AFTER `mdbook build docs` because it scans the build output.
  
  **Implementation: Python, not `! rg ...`.** A shell pipeline of the form `! rg -n <pattern> docs/book/html` is rejected because shell negation collapses both "no matches" (exit 1) and "scanner error" (exit 2 from rg, or any non-rg error like a missing binary, missing directory, or unreadable file) into success. Verified: `! rg 'foo' /missing-dir` exits 0. With that recipe shape, a build that produced no `docs/book/html` directory at all -- or a sandbox where `rg` was unavailable -- would pass the gate, upload the artifact, and deploy. The gate's whole job is to fail closed when the rendered HTML is suspect, not when the scanner happens to error.
  
  Use a small Python script supplied by the docs shell (the same `python3.withPackages [ ps.pyyaml ]` already added in Step 3 -- pyyaml isn't needed here, but the interpreter is). Required semantics:
  1. Assert `docs/book/html` exists and is a non-empty directory. If not -> exit non-zero with a clear message.
  2. Walk every `*.html` file under it (recursively, so pages rendered into subdirectories aren't missed).
  3. For each file, compile and scan with the union regex `(<p>|<br ?/?>|^)(intent|status):`, multiline-aware. Track and print every match with `file:line:matched-text`.
  4. Exit non-zero on ANY of: assertion failure (step 1), IO error reading a file (step 2), runtime error inside the regex compile/match, or non-empty match set (step 3).
  5. Exit zero only when the walk completed without IO errors AND the match set is empty.
  
  The recipe wraps the script invocation. Just runs each recipe line in `bash -e` (errexit) by default, so an uncaught Python exception propagates to a non-zero recipe exit -- the CI step fails closed.
  
  Why the regex pattern is what it is (kept from the previous revision -- only the runner shape is changing):
  - Wrapped in a paragraph: when mdBook parses a YAML body whose first line isn't a recognized horizontal-rule pattern, it inlines the text as a paragraph, producing `<p>intent: ...</p>` (or `<p>status: ...</p>`) in the rendered HTML.
  - Following a `<br/>`: when only the closing `---` of a `---...---` block is interpreted as a thematic break, the keys preceding it land on a `<br/>`-prefixed inline run.
  - Anchored at the start of a line: literal `intent:` / `status:` at column zero -- the original form, kept as a third alternative because the union shouldn't lose coverage.
  
  The earlier `^---$` form is rejected (mdBook turns bare `---` into `<hr/>`, so YAML leaks while the grep passes). The bare `^(intent|status):` form is rejected because a paragraph-wrapped leak slips past it. Checking only `intent:` is rejected because some ADRs already had `intent:`, giving false reassurance about `status:`.
  
  Living as a CI-invoked recipe -- not a manual Step 8 grep -- is load-bearing: without the CI wiring, a future `mdbook-yml-header` version regression, a deleted `[preprocessor.yml-header]` block in `book.toml`, or a typo in the preprocessor config could pass `check-docs` + `check-docs-frontmatter` + `mdbook build docs`, upload the artifact, and deploy HTML with visible YAML. The render gate must be on the same automated path that publishes -- and it must fail closed on scanner errors, not silently pass.
- **In-code principle anchor gate.** Decision 3 turns code-side citations from `docs/principles.md:LINE` into `docs/design/principles.md#NN-anchor`, on the grounds that anchors are "more durable than line numbers." After Steps 5 and 6, ~30+ code locations cite `docs/design/principles.md#NN-anchor` from Rust, Python, Nix, AGENTS.md, and README.md. mdbook-linkcheck only validates links inside the rendered mdBook output -- it does not parse Rust comments, Python docstrings, Nix prose, or AGENTS.md prose for `docs/...#anchor` references. So a future principle rename or renumbering would change the rendered anchors and rot every in-code citation silently.
  
  Wire as a just recipe `check-code-doc-anchors`, invoked locally as `nix develop .#docs -c just check-code-doc-anchors` and in CI by the build job (Step 3.5). Implementation: ~30-line Python script under `scripts/docs/`, using stdlib `re`. The check must:
  - Parse `docs/design/principles.md` headings (lines starting with `## `) and compute each heading's mdBook anchor using mdBook's actual `id_from_content` algorithm. Materialize the set of valid anchors.
  - Grep the regex `docs/design/principles\.md#(\S+?)["` + "`)" + `\s]` across `cli/`, `tests/`, `modules/`, `AGENTS.md`, `README.md`, `.claude/agents/`, `.claude/memory/`, and `prompts/` (terminator class covers `"`, backtick, `)`, and whitespace -- the closing chars for the contexts these URLs appear in; the `.claude/` and `prompts/` paths are in scope because Step 6 explicitly rewrites principle-anchor cites in those trees).
  - For each captured anchor, assert it is in the set of valid anchors.
  - Exit non-zero on any unresolved anchor, naming the file, line, and broken cite.
  
  **mdBook's `id_from_content` algorithm (load-bearing -- get this wrong and every cite fails to validate against the wrong-shape set):** first `.trim()` the heading (whitespace from both ends -- a trailing newline left over from naive line reading would otherwise map to `-`); then iterate over each remaining character (after stripping the leading `## ` markdown prefix); keep `is_alphanumeric() || ch == '_' || ch == '-'` chars lowercased; replace each whitespace char with `-`; drop every other char. **There is no collapse-consecutive-hyphens step, and there is no strip-leading-or-trailing-hyphens step (after the initial `.trim()`).** Confirmed by reading `crates/mdbook-html/src/utils.rs:76-100` (the `id_from_content` function in the post-workspace-split mdBook layout) and its own unit tests at `crates/mdbook-html/src/utils.rs:116-134` (`assert_eq!(id_from_content("\`--passes\`: add more rustdoc passes"), "--passes-add-more-rustdoc-passes")` -- preserves consecutive `-`s and a leading `-`; `assert_eq!(id_from_content("Method-call 🐙 expressions \u{1f47c}"), "method-call--expressions-")` -- the space+dropped-emoji combination produces a `--` that is NOT collapsed, and the trailing `-` is NOT stripped).
  
  Python form (the `normalize_id` name is an internal symbol -- it is not a reference to mdBook's function, only the local helper name in the gate script):
  
  ```python
  def normalize_id(heading: str) -> str:
      heading = heading.strip()                       # mirrors mdBook's .trim()
      out = []
      for ch in heading:
          if ch.isalnum() or ch in ('_', '-'):
              out.append(ch.lower())
          elif ch.isspace():
              out.append('-')
      return ''.join(out)
  ```
  
  Worked example against current `docs/principles.md`: heading `3. Safe-by-construction operations` -> walk chars: `3` (keep), `.` (drop), ` ` (->`-`), `S` (->`s`), `a` (keep), `f` (keep), `e` (keep), `-` (keep), `b` (keep), `y` (keep), `-` (keep), `b` (keep), `y` (keep), `-` (keep), `c` (keep), `o` (keep), `n` (keep), `s` (keep), `t` (keep), `r` (keep), `u` (keep), `c` (keep), `t` (keep), `i` (keep), `o` (keep), `n` (keep), ` ` (->`-`), `o` (keep), `p` (keep), `e` (keep), `r` (keep), `a` (keep), `t` (keep), `i` (keep), `o` (keep), `n` (keep), `s` (keep) -> `3-safe-by-construction-operations`. This matches the anchor used in the Step 5 rewrite table (`#3-safe-by-construction-operations`) -- treat any deviation from this exact output for that heading as a script bug, not a docs bug.
  
  This triggers on `docs/**` changes -- a principle rename in docs/ that breaks in-code cites is exactly the case the gate is meant to catch, and it triggers reliably. Code-side cites added later (without a principle rename) will be checked on the next docs/ change.
- **Source-side frontmatter gate.** The rendered-HTML check above only proves YAML is stripped, not that it's valid. mdbook-yml-header strips whatever it finds, including a typo like `status: Superseded by [012-intent-cli.md](...)`, so a broken enum leaks past the render gate without catching. Add an enforcer that parses the source files directly. Implementation: a short Python check using stdlib `re` + `yaml.safe_load` -- pyyaml is supplied by the docs shell (Step 3, devShells definition). Wire it as a new just recipe `check-docs-frontmatter` -- invoked locally as `nix develop .#docs -c just check-docs-frontmatter`, and from CI by the build job (Step 3.5). The check must enforce:
  - Every `.md` file under `docs/design/decisions/` and `docs/internals/` has a leading `---` ... `---` block at line 1.
  - The block parses as valid YAML.
  - Both `intent:` and `status:` keys are present.
  - `status:` value is exactly one of the four enum values `Active | Superseded | Draft | Deprecated` (case-sensitive). No qualifier text on the same line; any qualifier prose belongs in the body (see Step 7).
  - For `docs/design/principles.md`: same block check, `intent:` required, `status:` optional (principles are not lifecycle-versioned).
  
  Exit non-zero on any violation, naming the file and the failed rule. Add this as a verification gate alongside `check-docs` -- it backstops Step 7's normalization rule with a machine-checkable contract, so a future doc edit that drops `status:` or scribbles a qualifier into the YAML key fails CI rather than silently shipping.
- `just test-rust` -> green after snapshot updates.
- VM tests against the names generated by the Step 5 `nix eval` snippet -> green.
- **Stale-path audit** (one combined scan, scoped to braid-local patterns and including build-surface files):
  ```bash
  rg -n \
    -e 'manual/(commands|guides|development\.md|book|SUMMARY\.md|\*\*)' \
    -e '\.\./manual/' \
    -e '(build|serve) +manual\b' \
    -e 'docs/(decisions/|principles\.md|luks-unlock\.md|tool-behavior|real-world|testing\.md|tui-insta-guide\.md|btrfs-balance|btrfs-luks-sector-size|claude-enospc-vs-hang|1-user-stories|notes-calculating)' \
    cli/ modules/ tests/ docs/ README.md AGENTS.md \
    .claude/agents .claude/memory prompts .github justfile flake.nix
  ```
  -> empty. The patterns are chosen to:
  - Match braid-local `manual/` subpaths plus the workflow-trigger glob `manual/**` and the `manual/SUMMARY.md` reference inside `check-docs`, while ignoring legitimate external URLs like `https://nixos.org/manual/nixos/stable/`.
  - Catch bare `mdbook build manual` / `mdbook serve manual` arguments in the workflow and justfile.
  - Cover every old `docs/` file or subdir that this migration relocates -- including `docs/claude-enospc-vs-hang.md` (referenced in `tests/repro/btrfs-remove-enospc{,-crash}.nix`) and the deleted `1-user-stories` / `notes-calculating-...` notes.
  - Scope set includes `.github` and `justfile` (and `flake.nix`) so build-surface stragglers can't hide.
- `.github/workflows/docs.yml`: exercise the workflow before merge via the `pull_request` trigger added in Step 3.5. Open (or push to an existing) PR from the working branch, then:
  ```bash
  gh pr checks --watch
  ```
  On the PR run, the `build` job must succeed (all the gate steps -- `just check-docs`, `just check-docs-frontmatter`, `just check-code-doc-anchors`, `mdbook build docs`, `just check-docs-rendered-frontmatter` -- green) and the `deploy` job must be reported as **skipped** in the run log. "Skipped" is the correct state for the whole job because of its job-level `if: (push && master) || workflow_dispatch` guard (Step 3.5); it is NOT skipped step-by-step. If individual Pages steps are listed as skipped but the `deploy` job itself appears as "completed" or "in progress," that means the job ran (creating a deployment object against the `github-pages` environment) and only the actions were guarded -- the previous, abandoned shape. Treat that as a failure: re-check the job topology in the workflow file.
  
  After merge to master, the same workflow re-runs; the `build` job re-passes and the `deploy` job now runs (because the job-level `if:` evaluates true for the `push` arm), publishing from `docs/book/html`. `gh workflow run docs.yml` (workflow_dispatch) becomes usable as a deploy path only after the workflow file is present on master; once it lands, the `if:` disjunct's `workflow_dispatch` arm enables operator-initiated re-deploys without touching `docs/`.

## Churn estimate

- ~25 file moves via `git mv` (history preserved).
- ~12 verbatim error strings + their pinned tests.
- ~25 code-comment path rewrites in Rust/Python/Nix.
- ~14 README.md link rewrites + AGENTS.md path-rewrite sweep (7+ stale-path hits driven by `rg`) plus three structural section rewrites (Layout, User Guide, Documentation).
- ~3-4 intra-docs link rewrites (tree-relocation rewrites + cross-tree cites + prose-path rewrites). The per-principle split is rejected (Decision 3), so the ~6 ADR-backlink rewrites it would have forced are no longer needed.
- ~5 agent/prompt file rewrites under `.claude/agents`, `.claude/memory`, `prompts/`.
- 1 GitHub Actions workflow update (`.github/workflows/docs.yml`): trigger paths (`docs/**` + `flake.nix` + `flake.lock` + workflow file), new `pull_request` (pre-merge) and `workflow_dispatch` (post-merge) triggers, build step, and a full job-level split into an unprivileged `build` job + a master-push-only `deploy` job that holds the `environment: github-pages` declaration.
- 2 justfile recipes rewritten (`docs`, `check-docs`).
- `flake.nix`: new `devShells.<system>.docs` containing `mdbook`, `mdbook-linkcheck`, `mdbook-yml-header`, `just`, and `python3.withPackages (ps: [ ps.pyyaml ])` (the latter two consumed by the new `check-docs-frontmatter` gate).
- `.gitignore`: `manual/book/` -> `docs/book/`.
- 2 tests/repro nix file comment rewrites for the renamed `claude-enospc-vs-hang.md` note.
- ~10 ADR YAML `status:` additions (normalize prose `Status: <enum> <qualifier>` into YAML `status: <enum>` plus a body-prose blockquote for the qualifier; remove the original prose `Status:` line). ~5 ADRs carry qualifiers that need this two-step normalization (002, 007, 008, 017, 021).
- ~29 `[← Manual]` -> `[← braid]` breadcrumb-label sweeps under `docs/commands/` + `docs/guides/`.
- 3 new just recipes, each backed by a small Python script under (e.g.) `scripts/docs/`, all wired into the workflow `build` job:
  - `check-docs-frontmatter` -- source-side YAML for presence and enum validity, Python + pyyaml.
  - `check-docs-rendered-frontmatter` -- rendered-HTML leak check; Python walks `docs/book/html` and matches the multiline union regex, failing closed on IO/runtime errors as well as on matches.
  - `check-code-doc-anchors` -- parses headings out of `docs/design/principles.md` using mdBook's actual `id_from_content` algorithm (initial `.trim()` then filter-and-drop, no collapse, no inner strip -- not the github-flavored slug), asserts every `docs/design/principles.md#<anchor>` cite in `cli/`, `tests/`, `modules/`, `AGENTS.md`, `README.md`, `.claude/agents/`, `.claude/memory/`, `prompts/` resolves to a real heading. Backstops Decision 3's anchor-durability claim against future principle renames/renumbering.

No `check-docs-github-urls` gate is needed: Option C (Context section) ensures docs/ does not contain absolute `https://github.com/danneu/braid/blob/master/<path>` URLs at all, and the Step 4.5 empty-grep check enforces that.

Suggested commit slicing for reviewable history:

1. File moves + .gitignore (Step 1).
2. mdBook config + flake.nix devShell + justfile + workflow (Steps 3, 3.5).
3. Landing page rewrite + SUMMARY.md (Steps 2, 3 SUMMARY).
4. Docs-internal link rewrites (Step 4.5) -- no longer paired with a principles split.
5. Code reference rewrites + snapshot refresh (Step 5).
6. README/AGENTS/agent-prompt sweep (Step 6).
7. Frontmatter pass + final verification (Steps 7, 8).
