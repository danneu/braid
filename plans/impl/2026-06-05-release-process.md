# Release process for braid (v0.0.1 onward)

## Context

braid is currently unreleased: no git tags, version `0.0.1` hardcoded in two
places, consumers (caja) track `master` HEAD. We want a repeatable
`just release patch|minor|major` that bumps + tags + publishes, a Cachix binary
cache so caja doesn't recompile Rust on the NAS, and a "pin to latest release"
story for consumers.

The hard constraint that shaped the design: **the Mac cannot build the
`x86_64-linux` binary caja needs.** Verified: `/etc/nix/machines` advertises
`aarch64-linux` only; `nix show-config` has no `x86_64-linux` platform; and
`~/world/.../linux-builder.nix` explicitly notes x86 emulation is intentionally
omitted ("Fix tracked separately"). So the x86_64-linux build + Cachix push runs
in **GitHub Actions on the release tag**, not locally.

Second precondition the design now assumes: **the braid repo is flipped from
private to public right before implementation.** Going public makes
GitHub-hosted Actions available for release.yml, lets the install-doc
`github:danneu/braid?ref=release` flakeref resolve without a token, and keeps
`CACHIX_AUTH_TOKEN` unexposed to forks (release.yml triggers only on
`push: tags`, has no `pull_request` trigger, and forks cannot push tags
upstream). It does **not** mean re-enabling `.github/workflows/test.yml`: the VM
suite stays manual/ad-hoc, not a push-triggered CI or release gate. The public
flip widens the threat model for **every** secret-bearing workflow, not just
release.yml: the repo's other one, `claude.yml`, fires on public
`issue`/`comment`/`review` events and currently gates only on the literal
`@claude` -- so after the flip any stranger could spend the maintainer's
`CLAUDE_CODE_OAUTH_TOKEN` quota. Hardening it is a pre-flip prerequisite below.

## Decisions (resolved with user)

1. **Consumer pin = a moving `release` branch** the release fast-forwards to each
   tag. Consumer URL: `git+ssh://git@github.com/danneu/braid?ref=release`.
   `nix flake update braid` becomes the "upgrade to newest release" button;
   `flake.lock` still pins the exact rev. This is the ecosystem convention for a
   release channel (NixOS/nix `latest-release`, cachix `latest`); following a
   *branch* and letting the lockfile pin is exactly how you already consume
   `nixos-26.05`.
2. **x86_64-linux build + Cachix push run in CI on the `v*` tag** (native, no
   emulation, secrets in GitHub). `just release` (on the Mac) only bumps/tags/pushes.
3. **Cachix cache `braid` is public** -- consumers add a substituter + public key,
   no auth on the NAS.
4. **Version bump engine = `cargo-release`** (`publish = false`).

Consequence: the linux-builder x86-emulation gap and the pasted `world:linux-builder`
reset/start findings are **out of scope** -- CI builds x86_64-linux, so releases
don't depend on the Mac's builder at all.

## One-time prerequisites (manual, not in the recurring recipe)

0. **Flip the GitHub repo from private to public** (see Context) -- the plan
   assumes this throughout: tokenless `github:` flakerefs in install docs,
   release.yml on hosted Actions, and fork-safe secrets. Do **not** re-enable
   `.github/workflows/test.yml` push or pull-request triggers; that VM workflow
   remains manual/ad-hoc.
   **Before the flip, harden `.github/workflows/claude.yml`** -- the only
   pre-existing secret-bearing workflow (`CLAUDE_CODE_OAUTH_TOKEN`), with
   public-reachable `issue`/`comment`/`review` triggers. Add a trusted-author
   clause onto each arm of its existing `@claude` `if:` so only the owner, org
   members, and collaborators can spend the token -- per event,
   `contains(fromJSON('["OWNER","MEMBER","COLLABORATOR"]'), github.event.<comment|review|issue>.author_association)`
   ANDed with the `@claude` check. A stranger's `@claude` comment then fails the
   `if:` and never starts the job.
1. Create a **public** Cachix cache named `braid`; capture its public key
   (`braid.cachix.org-1:...`).
2. Add `CACHIX_AUTH_TOKEN` (a push token for that cache) as a **GitHub Actions
   repo secret**.
3. Land the braid-repo changes below on `master`.
4. Bootstrap the first release as `v0.0.1` (cli/Cargo.toml is already `0.0.1`, so
   the first release is "publish current," not a bump): from
   `nix develop .#release`, `git tag -a v0.0.1 -m v0.0.1 && git push origin
   v0.0.1`. The tag triggers `release.yml`, which **creates** the `release`
   branch, warms the cache, and cuts the GitHub release. All later
   `just release` runs bump from `0.0.2`.
5. Wire the consumer (`~/world`) -- see "Consumer changes" -- then
   `nix flake update braid` + rebuild caja.

Verify the `release` branch is **not branch-protected** against the Actions token
(CI pushes it with `GITHUB_TOKEN`). Symmetrically, verify the **releaser can push
the version-bump commit directly to `master`**: `cargo release` commits the bump
and pushes it to `master` (not via a PR), so any post-public branch-protection
ruleset on `master` -- required PRs, "include administrators" -- must exempt the
releaser, or `cargo release --execute` fails mid-run after the local commit
(leaving a local bump commit + tag to unwind).

## braid-repo changes

### 1. Single source of truth for version -- `flake.nix`

Replace the hardcoded literal at `flake.nix#commonArgs` (currently
`version = "0.0.1";`) so crane reads it from the crate manifest. `craneLib` is
already in scope (`crane.mkLib pkgs`). It MUST point at `./cli/Cargo.toml` -- the
repo-root `Cargo.toml` is `[workspace]`-only.

```nix
commonArgs = {
  inherit src;
  inherit (craneLib.crateNameFromCargoToml { cargoToml = ./cli/Cargo.toml; }) pname version;
  meta = commonMeta;
};
```

This is a pure path read (no IFD/impurity). `pname` resolves to `braid-cli`
(unchanged). After this, `cli/Cargo.toml` is the only version string in the repo;
`braid --version` already reads `CARGO_PKG_VERSION` from it via clap
(`cli/src/main.rs#Cli`, `#[command(version)]`). No test asserts braid's version
(`cli/tests/root_check.rs` checks only the exit code; `tests/cli/tool-versions.nix`
ignores it), so this is safe.

**Guard the SoT invariant with an eval check.** Add a flake check
`eval-version-matches-cargo` to `flake.nix#checksFor`, following the existing
`eval-nixos-module-default-supplies-package` template. Pass `system` and the
manifest path explicitly so neither is ambiguous inside the eval file:

```nix
eval-version-matches-cargo = import ./tests/eval/version-matches-cargo.nix {
  inherit pkgs self system;
  cargoToml = ./cli/Cargo.toml;   # resolves relative to flake.nix = repo root
};
```

The eval file takes `{ pkgs, self, system, cargoToml }` and asserts
`self.packages.${system}.braid-cli-unwrapped.version ==
(builtins.fromTOML (builtins.readFile cargoToml)).package.version`, returning a
trivial `pkgs.runCommand` that builds only when they match. Passing `cargoToml`
from the call site matters: a literal `./cli/Cargo.toml` written *inside*
`tests/eval/` would resolve to the wrong path, and `system` is not otherwise in
scope in the eval file. While the version is read from Cargo.toml this is trivially
true, but it fails loudly if anyone reverts `flake.nix` to a hardcoded literal that
then drifts as `cargo release` bumps the crate -- turning the SoT from a convention
into an enforced invariant. It runs in the release gate (section 5) and as part of
`nix flake check`.

### 2. `cargo-release` config -- root `Cargo.toml` (workspace metadata) + Cargo guard in `cli/Cargo.toml`

`just release` runs `cargo release` from the **workspace root** (justfile recipes
execute in the repo root). The release config must therefore live in
**`[workspace.metadata.release]` in the root `Cargo.toml`** -- a package-scoped
`[package.metadata.release]` in `cli/Cargo.toml` is not reliably read from a
root/virtual-manifest invocation and would silently fall back to cargo-release's
defaults, including the catastrophic `publish = true` and a `braid-cli-v{{version}}`
tag. Put it in the root, beside `[workspace]`:

```toml
# root Cargo.toml
[workspace.metadata.release]
publish = false            # never touch crates.io (release-tool layer)
tag = true
push = true
tag-name = "v{{version}}"  # override the workspace-member default braid-cli-v{{version}}
pre-release-commit-message = "chore(release): v{{version}}"
tag-message = "v{{version}}"
```

Also set Cargo's **own permanent guard** in `cli/Cargo.toml`'s `[package]` table,
so a *direct* `cargo publish` (bypassing cargo-release entirely) is refused by
Cargo itself:

```toml
# cli/Cargo.toml
[package]
name = "braid-cli"
# ...existing fields...
publish = false            # Cargo-level guard: `cargo publish` refuses outright
```

Two independent layers: `[workspace.metadata.release] publish = false` stops
`cargo release`; `[package] publish = false` stops any direct `cargo publish`.
`tag-name = "v{{version}}"` is load-bearing -- in a *workspace* cargo-release's
default member tag is `{{crate_name}}-v{{version}}` (i.e. `braid-cli-v0.0.2`), but
the `release`-branch FF and `gh release` flow assume `vX.Y.Z`. No
`pre-release-replacements` needed -- the flake holds no version literal, and
cargo-release updates `cli/Cargo.toml` + the `braid-cli` entry in `Cargo.lock`
itself. Pre-1.0 bumps are plain semver: `patch` 0.0.1->0.0.2, `minor`->0.1.0,
`major`->1.0.0 (document this so `minor`'s jump to 0.1.0 isn't a surprise).

### 3. A darwin-evaluable `release` devShell -- `flake.nix`

The default devShell is Linux-only, but `just release` runs on the Mac and
`cargo-release` needs `cargo` on PATH. Add a cross-platform shell (mkShell-based,
like the existing `docs` shell) next to `docsShellFor`:

```nix
releaseShellFor =
  system:
  let pkgs = nixpkgs.legacyPackages.${system};
  in pkgs.mkShell {
    packages = [ pkgs.cargo-release pkgs.cargo pkgs.rustc pkgs.gh pkgs.git pkgs.just ];
  };
```

Expose it in the `devShells` output alongside `docs` (all systems):
`release = releaseShellFor system;`. This shell is for the **Mac-side bump only**
(`cargo-release` needs `cargo` on PATH; the bump compiles nothing). The CI Rust
test gate does **not** use it -- it runs in the project's canonical build
environment, the default Linux devShell (`craneLib.devShell`, exposed as
`devShells.x86_64-linux.default`), which has the full Rust toolchain and linker
crane already wires. A bare `mkShell` would likely link on Linux via the implicit
stdenv `cc`, but routing the test gate through the purpose-built bump shell
couples two unrelated concerns and is needless fragility -- see section 5.

### 4. `just release` recipe -- `justfile`

Thin Mac-side recipe. Irreversible once the tag is pushed; CI does the rest.

```just
# Cut a release: bump cli/Cargo.toml + Cargo.lock, tag vX.Y.Z, push master+tag.
# The tag triggers .github/workflows/release.yml, which builds x86_64-linux,
# pushes to the public `braid` cachix cache, creates the GitHub release,
# and fast-forwards the `release` branch. Run from `nix develop .#release`.
#
# IRREVERSIBLE once the tag is pushed. If CI fails downstream, fix and re-run the
# release workflow from the GitHub Actions UI (its steps are idempotent) -- do
# NOT re-run `just release` (that would bump again). See docs/dev/releasing.md.
release level:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{level}}" in patch|minor|major) ;; *) echo "error: level must be patch|minor|major" >&2; exit 2 ;; esac
    command -v cargo-release >/dev/null || { echo "error: cargo-release missing -- run inside 'nix develop .#release'" >&2; exit 1; }
    command -v gh >/dev/null || { echo "error: gh missing" >&2; exit 1; }
    [ -z "$(git status --porcelain)" ] || { echo "error: working tree not clean" >&2; exit 1; }
    [ "$(git rev-parse --abbrev-ref HEAD)" = "master" ] || { echo "error: must release from master" >&2; exit 1; }
    git fetch origin master --tags
    [ "$(git rev-parse @)" = "$(git rev-parse origin/master)" ] || { echo "error: master out of sync with origin (git pull --ff-only)" >&2; exit 1; }
    # Compile gate: darwin-native via nix (the Mac cannot build x86_64-linux -- CI does).
    nix build .#packages.{{system}}.braid-cli-unwrapped --no-link
    cargo release {{level}} --execute --no-confirm
    tag="$(git describe --tags --abbrev=0)"
    echo "==> pushed $tag; release workflow triggered. Watch: gh run watch (release.yml)"
```

`{{system}}` is the justfile's existing top-level var (`aarch64-darwin` on the Mac);
`braid-cli-unwrapped` is pure Rust and builds there, so this catches compile
breakage before the irreversible tag. Keep the existing `cachix` recipe but add a
one-line comment that release pushes go through CI and it must run from an
x86_64-linux host.

### 5. `release.yml` -- new `.github/workflows/release.yml`

Single sequential job, ordered cheapest-gate-first: **validate tag (lineage +
format + version) -> test -> build -> push cache -> GitHub release -> fast-forward
`release`.** Serialized at workflow level (`concurrency`) so two tag pushes can't
race on the cache or the `release` ref. Three enforcement points close the trust
gap: an **ancestry guard** rejects any `v*` tag whose commit is not on `master`,
a **tag guard** rejects any tag that is not `vX.Y.Z` matching `cli/Cargo.toml` --
both before any build or cache write -- and `skipPush: true` makes the explicit
`cachix push` the *only* upload. The `release` branch FF is the **last** step and
the sole consumer-visible "it's released" gate: it lands only after the cache is
warm *and* the GitHub release object exists, so reaching it implies every prior
step succeeded and no consumer can `nix flake update` to a half-published rev.

```yaml
name: release
on:
  push:
    tags: ['v*']
permissions:
  contents: write          # push `release` branch + create the GitHub release
concurrency:
  group: release           # serialize all release runs repo-wide
  cancel-in-progress: false # never kill an in-progress release; queue behind it
  queue: max               # up to 100 pending releases (don't drop a tag); requires cancel-in-progress != true
jobs:
  publish:
    if: ${{ github.event.deleted != true }}   # tag-delete push events also fire here; skip them
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with: { ref: ${{ github.ref }}, fetch-depth: 0 }
      - name: Require the tag to be on master (reject stray v* tags)
        id: guard
        run: |
          git fetch --no-tags origin master            # FETCH_HEAD = origin/master tip
          release_commit="$(git rev-list -n1 "$GITHUB_REF_NAME")"
          git merge-base --is-ancestor "$release_commit" FETCH_HEAD \
            || { echo "::error::$GITHUB_REF_NAME ($release_commit) is not on master; refusing to release" >&2; exit 1; }
          echo "commit=$release_commit" >> "$GITHUB_OUTPUT"
      - name: Require the tag to be vX.Y.Z matching cli/Cargo.toml (reject malformed/mismatched)
        run: |
          [[ "$GITHUB_REF_NAME" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] \
            || { echo "::error::$GITHUB_REF_NAME is not a vX.Y.Z release tag" >&2; exit 1; }
          tag_ver="${GITHUB_REF_NAME#v}"
          # read [package] version only (checkout is at the tagged commit); no nix/cargo yet
          cargo_ver="$(sed -n '/^\[package\]/,/^\[/{s/^version = "\(.*\)"/\1/p;}' cli/Cargo.toml | head -1)"
          [ "$tag_ver" = "$cargo_ver" ] \
            || { echo "::error::$GITHUB_REF_NAME != cli/Cargo.toml version $cargo_ver" >&2; exit 1; }
      - uses: DeterminateSystems/nix-installer-action@main
      - uses: cachix/cachix-action@v15
        with:
          name: braid
          authToken: ${{ secrets.CACHIX_AUTH_TOKEN }}
          skipPush: true        # disable the push daemon; the explicit step below is the ONLY push
      - name: Version SoT + Rust test gate
        run: |
          nix build .#checks.x86_64-linux.eval-version-matches-cargo --no-link
          nix develop --command just test-rust   # default Linux devShell (craneLib) -- full toolchain + linker
      - name: Build x86_64-linux binary
        id: build
        run: echo "out=$(nix build .#packages.x86_64-linux.braid-cli-unwrapped --no-link --print-out-paths)" >> "$GITHUB_OUTPUT"
      - name: Push closure to cachix (synchronous, sole push -- gates the release)
        env: { CACHIX_AUTH_TOKEN: ${{ secrets.CACHIX_AUTH_TOKEN }} }
        run: echo "${{ steps.build.outputs.out }}" | cachix push braid
      - name: Create GitHub release (guarded, idempotent -- before the FF)
        env: { GH_TOKEN: ${{ github.token }} }
        run: |
          tag="${GITHUB_REF_NAME}"
          gh release view "$tag" >/dev/null 2>&1 && echo "release $tag exists; skipping." \
            || gh release create "$tag" --generate-notes --verify-tag
      - name: Fast-forward `release` (LAST -- sole consumer gate, after cache + release object)
        run: git push origin ${{ steps.guard.outputs.commit }}:refs/heads/release
```

Notes: the **ancestry guard** (`git merge-base --is-ancestor`) enforces the
invariant the plan relies on -- `release` only ever advances to a master-descended
commit -- so an accidental or hand-pushed `v*` tag off a feature branch is rejected
before it can warm the cache or move `release` (and the FF pushes that *validated*
commit, not a re-resolved ref). The **tag guard** then rejects any ref that is not
`^vX.Y.Z$` *and* equal to `cli/Cargo.toml`'s `[package]` version at the tagged
commit, so a malformed `v0.0.0-test` or a hand-pushed wrong `v9.9.9` on master
fails before Nix install or any cache/branch write -- lineage alone does not prove
the tag names the right version. Both guards run before `nix-installer-action`.
**`concurrency: { group: release, cancel-in-progress: false, queue: max }`**
serializes releases: GitHub keeps the in-progress run and queues subsequent tags'
runs -- `queue: max` allows up to 100 pending (vs. the `single` default's one), so
a bursty recovery that pushes several tags drops none. (`queue: max` is
incompatible with `cancel-in-progress: true`, which is why we keep it `false`.)
This only prevents *dropping* a queued run; GitHub does **not** guarantee the
order pending runs start, so two overlapping releases could run newest-first. The
runbook therefore makes the rule one active release tag at a time (push the next
tag only after the prior `release.yml` run finishes) -- `queue: max` is a safety
net for accidental overlap, not a substitute for that discipline.
**`skipPush: true`** disables cachix-action's default push daemon, so the explicit
`cachix push braid <out>` is the sole upload and **only** `braid-cli-unwrapped`
x86_64-linux lands in the cache -- exactly what caja's module default consumes
(`flake.nix#nixosModules.default` sets `package = ... braid-cli-unwrapped`; the
wrapped `braid` would duplicate all storage tools for no consumer benefit).
**Step order is the release invariant**: cachix push -> `gh release create` ->
`release` FF, so the FF (the only thing consumers watch) lands last, after the
cache is warm and the release object exists. Every step is idempotent on re-run:
the guards, eval check, and build are pure; cachix push skips present paths;
`gh release create` is guarded by `gh release view`; the branch FF re-pushes the
same commit. **Do not add `just test-vm` to this workflow**, and do **not**
re-enable `.github/workflows/test.yml` push or pull-request triggers. The release
path intentionally avoids the VM suite so it adds **zero VM latency** and does not
turn the expensive VM workflow into a CI gate. VM coverage remains a manual
development/release-readiness choice (`just test-vm` locally, or an explicit
workflow_dispatch run when wanted), not something `release.yml` or `just release`
requires.

**Keep `.github/workflows/test.yml` manual-only.** Leave its `workflow_dispatch`
trigger in place and keep `push` / `pull_request` commented out. The ancestry
guard still requires every release tag to be on `master`, but this plan no longer
claims that `master` is VM-gated by GitHub. release.yml's required gates are the
cheap eval/Rust checks, the x86_64-linux build, Cachix push, GitHub release
creation, and final `release` fast-forward.

### 6. Docs

- **ADR `docs/design/decisions/029-release-process.md`** (`status: Active`, the
  `intent:`/`# Decision:`/`## See` shape of recent ADRs). Records: release-branch
  channel, version SoT in Cargo.toml, cargo-release, CI-builds-x86_64-linux
  rationale (Mac can't), public cache. Include two notes that make the rest of the
  plan cohere: (a) the **no-follows recommendation** is anchored here as the
  cache-path-identity home (010 points here), and the deployed consumer `~/world`
  already runs no-follows with a "deliberate tool-version boundary -- do NOT set
  follows" comment, so the doc flip aligns docs with reality, not against it;
  (b) a brief **public-repo trust note covering every secret-bearing workflow**,
  not just release.yml: release.yml is fork-safe by trigger (`push: tags` only, no
  `pull_request`, forks can't push tags upstream, so `CACHIX_AUTH_TOKEN` never
  reaches a fork); `claude.yml` is *not* trigger-safe (public issue/comment/review
  events) and is hardened separately with a trusted-`author_association` gate
  (prereq 0) so strangers can't spend `CLAUDE_CODE_OAUTH_TOKEN`. `## See` -> the
  `release` justfile recipe, `.github/workflows/release.yml`,
  `docs/dev/releasing.md`.
- **`docs/dev/releasing.md`** (`intent:` frontmatter, `# Title`, dev-doc style):
  operator runbook with copy-pasteable commands:
  - Prereqs: public Cachix cache, `CACHIX_AUTH_TOKEN`, unprotected `release`
    branch for Actions, and `nix develop .#release`.
  - Before releasing: decide whether you want a manual VM sweep for this release
    and run it outside the release automation if so (`just test-vm` locally, or
    an explicit `workflow_dispatch` run of `test.yml`). Do **not** re-enable
    `test.yml` push or pull-request triggers, and do not make `just release`
    depend on `test.yml`. The release path itself does not run or query the VM
    suite; it gates on the release.yml eval/Rust checks, x86_64-linux build,
    Cachix push, GitHub release creation, and final `release` fast-forward.
  - Normal release:
    `just release <patch|minor|major>`, then `gh run list --workflow release.yml`
    / `gh run watch <run-id>` to follow CI. Include the pre-1.0 bump table and
    the consumer upgrade command (`braid:upgrade`). **One active release tag at a
    time**: do not push (or `just release`) the next tag until the prior
    `release.yml` run has completed successfully -- GitHub does not guarantee
    queued-run order, so an overlapping older tag would run after a newer one and
    fail its `release` FF (a non-fast-forward).
  - **If release CI fails**: first rule is "never re-run `just release` after a
    tag exists" (that would bump again). For transient/config-only failures,
    rerun the existing workflow with `gh run rerun <run-id>` and watch it with
    `gh run watch <run-id>`. For bad tagged code, fix `master`, then move the
    same version tag to the fixed commit with:

    ```sh
    git push origin :refs/tags/vX.Y.Z
    git tag -d vX.Y.Z
    git tag -a vX.Y.Z -m vX.Y.Z
    git push origin vX.Y.Z
    ```

    State why this is safe: the `release` FF is the last step, so until it runs
    `release` has not advanced and consumers cannot `nix flake update` to the new
    rev -- a failure at any earlier step (test, build, cache, or `gh release
    create`) leaves consumers untouched. (Re-running converges: the cache push and
    `gh release create` are idempotent, and the FF re-pushes the same commit.)
- **`docs/SUMMARY.md`**: add `- [Releasing](dev/releasing.md)` under Development
  and `- [029: Release process](design/decisions/029-release-process.md)` under
  Decisions.
- **Install docs** (`docs/guides/getting-started.md`,
  `docs/guides/nixos-configuration.md`, `README.md` -- all three carry the same
  `braid.url` block, currently `github:danneu/braid` +
  `braid.inputs.nixpkgs.follows = "nixpkgs"`):
  - Pin the recommended snippet to the channel: `braid.url = "...?ref=release"`.
  - **Flip the recommendation from `follows` to no-follows, everywhere.** Today
    every install snippet *recommends* `braid.inputs.nixpkgs.follows = "nixpkgs"`;
    the cache changes which default is correct. `follows` rebuilds
    `braid-cli-unwrapped` against the *consumer's* nixpkgs rather than the
    nixos-26.05 CI builds against -> a different store path -> a **cache miss**
    (the NAS recompiles Rust, defeating the cache). No-follows uses braid's pinned
    nixpkgs = the exact path CI pushed = cache hit; caja already does this. So drop
    `follows` from the recommended snippet and present it as an advanced opt-out
    (smaller closure via nixpkgs dedup, but it forfeits release-cache path
    identity).
  - The `unversioned|unreleased|github:` sweep regex does **not** catch the
    follows-recommendation prose. Hand-edit each site that still calls `follows`
    "recommended" so it does not contradict the new default:
    - `docs/guides/nixos-configuration.md`: the Minimal-config snippet line
      (drops `follows` per above), the **Tool overrides** prose ("With the
      recommended `braid.inputs.nixpkgs.follows`..."), and the inline comment in
      the **Full config example** ("with the recommended nixpkgs `follows`,
      defaults track your nixpkgs"). Fold the cache-identity dimension into the
      tradeoff discussion while flipping which side is recommended.
    - `flake.nix#nixosModules.default`: the `braidPkgs` NOTE comment ("the install
      docs recommend `braid.inputs.nixpkgs.follows`") -- reword so it states the
      recommended default is no-follows (cache path identity) with `follows` as
      the closure-dedup opt-out.
    - `getting-started.md` and `README.md`: the shared `braid.url` block (drop
      `follows`, covered above).
  - Add the binary-cache note: caja-side `nix.settings` substituter
    `https://braid.cachix.org` + public key.
- **ADR 010 (Active) is the authoritative toolchain-pinning doc and currently
  *recommends* `follows`** (its "Consumer `follows` decides the actual source"
  section, and the mitigation line that already lists "not following braid's
  `nixpkgs` input" as valid). A one-line pointer is not enough -- it would leave
  the governing ADR contradicting the install docs it governs (an Active-ADR
  correctness defect per AGENTS.md). **Rewrite 010's follows discussion so
  no-follows is the recommended default**, citing ADR 029 for the cache-path
  mechanics. This is consistent, not a rationale reversal: 010 already lists
  no-follows as a valid mitigation; the cache simply promotes it from "a
  mitigation" to "the default." ADR 029 stays the single authoritative home for
  the cache-path-identity rationale; 010 points to it.
- **Stale release-state sweep** (post-v0.0.1):
  `rg -n 'unversioned|unreleased|github:danneu/braid' README.md docs/` and rewrite
  every hit:
  - `README.md` "this is unversioned" -> "pre-v1.0 and unstable" (keep the "I
    change things" message);
  - the `nix run github:danneu/braid -- --help` HEAD/public-shorthand example and
    any remaining `github:danneu/braid` pins -> the `?ref=release` pinned form;
  - the lone Active-doc `unreleased` hit, `docs/design/decisions/015-hdd-defaults.md`
    ("...into unreleased software...") -> a "pre-v1.0" / no-backwards-compat
    phrasing that preserves the rationale.

  **Out of scope: only `AGENTS.md`'s** "No backwards compatibility / unreleased
  software" policy statement -- its no-migration *intent* persists regardless of
  release state; the user rewords it separately. Also skip any `status:
  Superseded`/`Deprecated` ADR (frozen point-in-time records) -- verify each hit's
  doc is Active before rewriting; today the only Active hit is 015. Keep README +
  mdBook in sync per AGENTS.md.

## Consumer changes (in `~/world`, separate repo)

- `~/world/flake.nix` braid input: `braid.url = "git+ssh://git@github.com/danneu/braid?ref=release";`
  (keep the "do NOT set inputs.nixpkgs.follows" comment). The SSH URL scheme is
  fine to keep -- it does not affect the locked rev or the cache-hit store path --
  but update its now-stale rationale comment ("SSH URL ... because the repo is
  private"): post-flip the repo is public, so SSH is a kept preference, not a
  necessity. (Separate repo; do here when wiring the consumer in prereq step 5.)
- `~/world/hosts/caja/configuration.nix`: extend the existing
  `nix.settings.extra-substituters` / `extra-trusted-public-keys` block (sibling to
  the numtide cache) with `https://braid.cachix.org` + the cache's public key.
  caja already has `nix.settings.trusted-users = [ "dan" ]`, so the substituter
  takes effect. `braid:upgrade` (`hosts/caja/modules/shells.nix`) is unchanged.

## Verification (end-to-end)

1. **cargo-release config + dry-run** (from the repo root, where the recipe runs):
   `cargo release config` reports `publish = false`, `tag-name = "v{{version}}"`,
   and `pre-release-commit-message = "chore(release): v{{version}}"` -- proving the
   `[workspace.metadata.release]` block is picked up from a root invocation and not
   silently defaulted to `publish = true` / `braid-cli-v{{version}}`. Then
   `cargo release patch` (no `--execute`) prints the planned bump/commit/tag
   `v0.0.2` without doing anything.
2. **flake version SoT**: `nix eval
   .#packages.x86_64-linux.braid-cli-unwrapped.version` returns `0.0.1` after the
   crateNameFromCargoToml change (no hardcoded literal);
   `nix build .#checks.x86_64-linux.eval-version-matches-cargo` passes, and
   hand-editing `flake.nix` back to a mismatched literal makes it fail (proves the
   guard).
3. **Bootstrap v0.0.1**: push the `v0.0.1` tag; confirm `release.yml` goes green,
   the `release` branch now exists at that commit, and `gh release view v0.0.1`
   shows the release.
4. **Cache warm**: `nix path-info --store https://braid.cachix.org <out-path>`
   resolves the pushed `braid-cli-unwrapped`.
5. **Consumer**: after repointing `~/world` to `?ref=release` + adding the key,
   `nixos-rebuild` on caja reports *copying* braid-cli-unwrapped *from*
   `https://braid.cachix.org` (not building), and `braid --version` -> `0.0.1`.
6. **Real bump**: `just release patch` -> `v0.0.2`; CI publishes; `braid:upgrade`
   on caja pulls `0.0.2` from the cache.
7. **Tag guard (negative)**: push a throwaway `v0.0.0-test` tag on a non-master
   commit; `release.yml` must fail at the ancestry-guard step before any build,
   cache write, or branch move. Delete the tag afterward -- the delete push event
   is skipped by the job's `github.event.deleted` guard (shows as skipped, no noisy
   re-run).
8. **Docs sweep clean**: `rg -n 'unversioned|unreleased|github:danneu/braid' README.md docs/`
   (same regex as the sweep) returns only intended occurrences -- every
   `github:danneu/braid` carries `?ref=release`, and no prose still says
   "unversioned"/"unreleased" (AGENTS.md is outside this `README.md docs/` scope by
   design).
9. **Follows-recommendation flip clean** (step 8's sweeps miss this -- they grep
   the `github:` URL and `unversioned|unreleased`, not `follows`). Two checks:
   (a) **No recommended snippet contains `follows`.**
   `rg -n 'nixpkgs\.follows' README.md docs/guides/getting-started.md
   docs/guides/nixos-configuration.md` -- README.md and getting-started.md carry
   only the recommended block (no opt-out example), so they must have **no**
   `follows` line at all; in nixos-configuration.md any surviving `follows` sits
   *only* in the labeled advanced-opt-out, never the recommended snippet. The
   assertion is "absent from recommended," not "absent everywhere."
   (b) **No prose recommends `follows`.**
   `rg -ni 'recommend' flake.nix docs/guides/nixos-configuration.md
   docs/guides/getting-started.md docs/design/decisions/010-toolchain-pinning.md`
   -- no hit recommends `follows`; ADR 010, the `flake.nix#nixosModules.default`
   NOTE comment, and the `nixos-configuration.md` prose + inline comment all
   present no-follows as the default with `follows` as the opt-out.
10. **CI Rust gate shell**: the bootstrap run (step 3) reaching green proves
    `nix develop --command just test-rust` links and passes in the default Linux
    devShell on the hosted runner (not the bump-only `.#release` shell).
11. **VM workflow remains manual-only**: inspect `.github/workflows/test.yml` and
    confirm `workflow_dispatch` is the only active trigger; `push` and
    `pull_request` remain commented out. A push to `master` must not trigger the VM
    suite automatically.
12. **Master push-ability**: the releaser can push a trivial commit directly to
    `master` (no required-PR ruleset blocks the bump), so `cargo release` will not
    stall mid-run.
13. **Tag guard (format + version)**: on a *master* commit, push `v0.0.0-test`
    (malformed) and separately a well-formed-but-wrong `v9.9.9` (while
    `cli/Cargo.toml` is e.g. `0.0.2`); `release.yml` must fail at the tag-guard
    step -- before `nix-installer-action`, the cache push, the release object, or
    the `release` FF -- for both. (This is the master-commit complement to step
    7's non-master ancestry rejection.) Delete the throwaway tags afterward.
14. **Recipe has no `test.yml` dependency**: inspect the `just release` recipe and
    confirm it does not call `gh run list --workflow test.yml` or otherwise depend
    on the VM workflow before `cargo release` bumps and tags.
15. **`claude.yml` author gate (inspection)**: after hardening, each event arm of
    the `if:` ANDs `author_association` in `OWNER`/`MEMBER`/`COLLABORATOR` with the
    `@claude` check, so a comment from a non-collaborator does not start the job
    (confirm by inspection; the secret never reaches an untrusted trigger).

## Risks / gotchas

- **`publish = false` is mandatory** -- the single most important config line for
  a private crate.
- **Dangling tag on CI failure**: the tag exists but the `release` FF -- the last
  step -- never runs, so `release` does not advance and **consumers are
  unaffected** no matter which earlier step failed (test, build, cache, or
  `gh release create`). Recover by re-running the same workflow for
  transient/config-only failures, or by fixing `master` and deleting/recreating
  the same version tag for bad tagged code. Documented with exact commands in the
  runbook.
- **Cache trust on caja**: skip the public-key step and caja reaches the cache but
  rejects the signature and silently rebuilds from source -- defeating the point.
- **Release tooling location**: `just release` must run inside `nix develop .#release`
  (the Mac's default devShell is Linux-only and has no cargo). The recipe fails
  with a hint if `cargo-release` isn't on PATH.
- **`release` branch hygiene**: the workflow's ancestry guard *enforces* that
  `release` only advances to a master-descended commit -- a stray or hand-pushed
  `v*` tag off a feature branch is rejected before any cache write or branch move,
  so this is no longer a convention the plan merely asserts. It's still
  machine-owned; never commit to it, and ensure no branch protection blocks the
  Actions token's push.
- **Master protection vs. the bump push** (post-public): `cargo release` pushes
  the bump commit straight to `master`. If a public-repo ruleset later requires
  PRs on `master` without exempting the releaser, `just release` fails after the
  local commit/tag. Keep the releaser exempt, or the recovery is the manual
  tag-unwind in the runbook.
- **No CI VM behavioral gate**: `.github/workflows/test.yml` remains
  workflow_dispatch-only, and neither `just release` nor `release.yml` requires a
  VM-suite result. That is deliberate: release automation stays fast and avoids
  reintroducing the expensive VM workflow as a push-triggered gate. When a release
  deserves VM coverage, run `just test-vm` locally or trigger `test.yml` manually
  before tagging.
- **Secret-bearing workflows after the public flip**: `claude.yml`
  (`CLAUDE_CODE_OAUTH_TOKEN`) fires on public issue/comment/review events; without
  the trusted-`author_association` gate (prereq 0) any stranger could spend the
  maintainer's Claude quota. release.yml is fork-safe by trigger (tags-only). Any
  *future* workflow that consumes a secret must re-clear this bar before it merges.
- **Concurrent releases**: `concurrency: { group: release, cancel-in-progress:
  false, queue: max }` serializes release runs so near-simultaneous tags can't
  interleave cache pushes or `release` FFs, and `queue: max` (up to 100 pending
  vs. the `single` default's one) means a burst drops no run. But GitHub does not
  guarantee queued start order, so if a newer tag runs first its `release` FF wins
  and an older overlapping tag's FF then fails as a non-fast-forward. That outcome
  is benign for consumers (`release` ends at the newest tag, never backwards) but
  shows a red run -- so the runbook rule is one active release tag at a time.

## Implementation notes

- **YAML flow maps converted to block style (section 5).** The plan's
  `with: { ref: ${{ github.ref }}, fetch-depth: 0 }` and `env: { ... ${{ }} }`
  flow mappings break the YAML parser, because the `}}` inside `${{ }}` closes the
  flow map. Every such `with:`/`env:` is written in block style in the shipped
  `release.yml`. Verified by `yaml.safe_load` on both workflows.
- **No-CI-VM model (matches the revised plan).** The shipped `just release` recipe
  has no `test.yml` parent-gate, `test.yml` is left `workflow_dispatch`-only, and
  ADR 029 / `releasing.md` / the recipe + `release.yml` comments frame VM coverage
  as a manual, per-release choice (local `just test-vm` or a `workflow_dispatch`
  run of `test.yml`). `release.yml` still runs `just test-rust` + the version eval
  check + build + cache push on the tag.
- **Cachix public key sourced from the API, not a placeholder.** The `braid`
  cache already existed and is public; the real key
  (`braid.cachix.org-1:I/p7fx1z5n0+O80KzMuT7aXRdkVyHr/buZKaBu7HvJs=`) was read
  from the Cachix API and used directly in the install-doc cache notes.
- **Binary-cache note placement.** The `nix.settings` substituter + key block was
  added to all three install docs (README, getting-started, nixos-configuration),
  mirroring the existing 3-way duplication of the `braid.url` block, using
  `extra-substituters`/`extra-trusted-public-keys` (append) rather than replacing
  the default cache list.

## Follow Up

- Manual operator prerequisites remain before the first release can run (the
  plan's "One-time prerequisites" 2, 4, 5): add `CACHIX_AUTH_TOKEN` as a GitHub
  Actions repo secret; bootstrap the `v0.0.1` tag; and wire the `~/world`
  consumer (`?ref=release` + the cache substituter/key). These are user-owned
  operational steps, not code.
- `claude.yml` is now hardened, but the repo went public before the gate landed,
  so any `@claude` issue/comment/review between the flip and this commit shipping
  could already have spent `CLAUDE_CODE_OAUTH_TOKEN`. Worth a quick look at recent
  Actions runs of the Claude Code workflow.
