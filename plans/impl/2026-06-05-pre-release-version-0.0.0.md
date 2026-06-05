# Start the version at 0.0.0 so `just release patch` cuts the first public release v0.0.1

## Context

braid is unreleased. The release machinery (commit `4dbddbd8`) was built around
`cli/Cargo.toml` already sitting at `0.0.1`, so the documented first release is a
**special-case hand-tag** ("publish current, not a bump": `git tag -a v0.0.1`).
Every *later* release goes through `just release <level>`.

The user wants the first release to use the same path as every other: keep the
repo at a pre-release `0.0.0` and run `just release patch` to bump+tag+publish
**v0.0.1**. This removes the bespoke bootstrap step entirely -- the first release
becomes mechanically identical to the hundredth.

Why this works cleanly:
- `cargo release patch` does plain semver, so `0.0.0 -> 0.0.1` (the repo's own
  table already documents `patch 0.0.1->0.0.2`, i.e. no pre-1.0 special-casing).
- We never tag `0.0.0`; it is only the in-tree "development" version. The first
  *tag* is still `v0.0.1`, exactly as desired.
- The first `just release` run still creates the `release` branch and the first
  GitHub release for free: `release.yml`'s final step is
  `git push origin <commit>:refs/heads/release`, which creates the ref on first
  push, and `gh release create` needs no pre-existing release. No special-casing.
- Version is a single source of truth (`cli/Cargo.toml`, read dynamically by
  `flake.nix` via `crateNameFromCargoToml`), so `0.0.0` flows everywhere:
  `braid --version` -> `0.0.0`, the built package version -> `0.0.0`, and the
  `eval-version-matches-cargo` guard (relative equality) stays green.

## The change

### 1. Set the in-tree version to `0.0.0` (the actual mechanism)

- `cli/Cargo.toml#3` -- `version = "0.0.1"` -> `version = "0.0.0"`.
- `Cargo.lock` -- the `braid-cli` package entry (`name = "braid-cli"`, line ~135)
  `version = "0.0.1"` -> `version = "0.0.0"`. This must change in the *same*
  commit: crane builds `--locked`, so a lock that disagrees with `cli/Cargo.toml`
  fails the build. Regenerate with `cargo check` from `nix develop .#release`
  (which has `cargo`) rather than relying on the Mac default shell, or hand-edit
  the single line.

Commit these two together, e.g. `chore: set pre-release version to 0.0.0`, and
push to `origin/master` before releasing -- `just release` refuses unless the
working tree is clean and `master` is in sync with origin.

### 2. Documentation (mandatory -- behavior/runbook change)

**`docs/dev/releasing.md`**
- Replace the `## Bootstrapping the first release (v0.0.1)` section (the hand-tag
  recipe) with a short note that the first release is just `just release patch`
  (`0.0.0 -> v0.0.1`), identical to every later release. Keep the two facts that
  are genuinely first-run-only: this run *creates* the `release` branch and the
  first GitHub release (automatic, no extra steps), and -- since CI has no VM gate
  -- run `just test-vm` / `just test-rust` locally before this first cut. The old
  "all later `just release` runs bump from `0.0.2`" line becomes "from `0.0.1`".
- Frontmatter `intent` mentions "first-release bootstrap"; soften to reflect that
  the first release is the normal flow.
- Bump table (`patch 0.0.1->0.0.2` ...): leave as the steady-state semver rule;
  the rewritten first-release note states the `0.0.0 -> 0.0.1` first step
  explicitly, so the table needs no change.

**`docs/design/decisions/029-release-process.md`** (status: Active)
- In the `### Version bump = cargo-release` subsection, add one sentence: the
  in-tree pre-release version is `0.0.0`, so the first `just release patch` cuts
  `v0.0.1` through the same path as every later release -- there is no
  special-case bootstrap. The existing `patch 0.0.1->0.0.2` example stays as the
  general rule. The Context paragraph ("version hardcoded in two places") is
  pre-029 history and stays.

## Out of scope (leave as-is)

- `plans/impl/2026-06-05-release-process.md` -- frozen implementation record for
  `4dbddbd8`; it documents the old bootstrap and is a point-in-time artifact, not
  live guidance. Do not rewrite it.
- `plans/impl/2026-05-25-linux-flake-ergonomics.md#283` -- uses `braid-cli-0.0.1`
  as an illustrative name; historical, leave.
- `tests/eval/version-matches-cargo.nix#15-17` -- the `0.0.2`/`0.0.1` drift
  *scenario* in the comment is illustrative and still valid; leave.
- `flake.nix`, `modules/`, `tests/` -- no hardcoded version literals; nothing to
  change. No live test asserts the literal `0.0.1` (the only matches are the IP
  `127.0.0.1` in TUI snapshots and the eval comment above).

## Verification

Before the irreversible release, confirm the bump arithmetic with cargo-release's
own dry run (no `--execute` = preview, mutates nothing), from `nix develop
.#release`:

```sh
cargo release patch            # expect: planning 0.0.0 -> 0.0.1, tag v0.0.1
```

After committing the `0.0.0` change, confirm the version flows through:

```sh
nix build .#packages.aarch64-darwin.braid-cli-unwrapped --no-link \
  && nix eval .#packages.aarch64-darwin.braid-cli-unwrapped.version   # -> "0.0.0"
nix build .#checks.aarch64-darwin.eval-version-matches-cargo --no-link # green at 0.0.0
just test-rust                                                          # lock/build sane
mdbook build docs                                                       # cross-links still valid after doc edits
```

## The release (user action -- not part of implementation)

Implementation stops after the `0.0.0` commit + doc edits land on `master`. The
user then cuts the first public release themselves (irreversible; pushes a tag):

```sh
# optionally: just test-vm   # first-release behavioral gate
just release patch           # 0.0.0 -> v0.0.1, tags + pushes, CI publishes
```
