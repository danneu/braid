---
intent: Record the braid release process -- moving `release` branch channel,
  version single-source-of-truth, cargo-release, the CI-built x86_64-linux binary,
  the public cachix cache, the local (non-CI) behavioral gate, and the public-repo
  trust model. Read before changing the release workflow, the version source, the
  consumer pin, or any secret-bearing workflow.
status: Active
---

# Decision: Release process

## Context

braid had no releases: no git tags, the version hardcoded in two places, and the
sole consumer (caja) tracked `master` HEAD. We want a repeatable
`just release patch|minor|major` that bumps + tags + publishes, a binary cache so
consumers do not recompile Rust on the NAS, and a "pin to latest release" story.

The hard constraint that shaped the design: the maintainer's Mac cannot build the
`x86_64-linux` binary consumers need. The nix-darwin linux-builder advertises
`aarch64-linux` only; x86 emulation is intentionally omitted. So the x86_64-linux
build and the cache push run in GitHub Actions on the release tag, not locally.

A precondition: the braid repo is public. That makes GitHub-hosted Actions
runners free, lets the `github:danneu/braid?ref=release` flakeref resolve without
a token, and keeps `CACHIX_AUTH_TOKEN` unexposed to forks.

## Decision

### Consumer pin = a moving `release` branch

The release fast-forwards a `release` branch to each `vX.Y.Z` tag's commit.
Consumers pin `braid.url = "...?ref=release"`; `nix flake update braid` is the
"upgrade to newest release" button, and `flake.lock` still pins the exact rev.
This is the ecosystem convention for a release channel (NixOS/nix
`latest-release`, cachix `latest`) and mirrors how a consumer already follows a
`nixos-26.05` branch and lets the lockfile pin.

The `release` branch is machine-owned: only `release.yml` advances it, and only
to a master-descended commit (enforced by the ancestry guard below). Never commit
to it, and ensure no branch protection blocks the Actions token's push.

### Version single source of truth = `cli/Cargo.toml`

`flake.nix#commonArgs` reads `pname` + `version` from `cli/Cargo.toml` via
`craneLib.crateNameFromCargoToml` (a pure path read, no IFD). `braid --version`
already reads `CARGO_PKG_VERSION` from the same manifest via clap. So
`cli/Cargo.toml` is the only version string in the repo, and `cargo release`
bumping it is the only version edit.

The invariant is enforced, not merely conventional: the flake check
`eval-version-matches-cargo` (`tests/eval/version-matches-cargo.nix`) asserts the
built `braid-cli-unwrapped.version` equals `cli/Cargo.toml`'s `[package]`
version. It is trivially true while the flake reads from the manifest, but fails
loudly if anyone reintroduces a hardcoded flake literal that then drifts.

### Version bump = `cargo-release`; build + publish in CI

`just release` (Mac-side) runs `cargo release` from the workspace root, so its
config lives in `[workspace.metadata.release]` in the root `Cargo.toml`. Two
independent publish guards: `[workspace.metadata.release] publish = false` stops
`cargo release` from touching crates.io, and `[package] publish = false` in
`cli/Cargo.toml` makes a direct `cargo publish` refuse outright. `tag-name =
"v{{version}}"` overrides cargo-release's workspace-member default
(`braid-cli-v{{version}}`), because the `release`-branch FF and `gh release` flow
assume `vX.Y.Z`.

Pre-1.0 bumps are plain semver: `patch` 0.0.1->0.0.2, `minor`->0.1.0,
`major`->1.0.0 (so `minor`'s jump to 0.1.0 is expected, not a surprise). The
in-tree pre-release version is `0.0.0`, so the first `just release patch` cuts
`v0.0.1` through the same path as every later release -- there is no special-case
bootstrap.

The tag triggers `.github/workflows/release.yml`, a single sequential job ordered
cheapest-gate-first: ancestry guard -> tag/version guard -> Rust test + version
eval gate -> build x86_64-linux -> push cache -> create GitHub release ->
fast-forward `release`. Two guards close the trust gap before any build or cache
write: an **ancestry guard** (`git merge-base --is-ancestor`) rejects any `v*`
tag whose commit is not on `master`, and a **tag guard** rejects any tag that is
not `^vX.Y.Z$` equal to `cli/Cargo.toml`'s version at the tagged commit. The
`release` FF is the **last** step and the sole consumer-visible "it's released"
gate: it lands only after the cache is warm and the GitHub release object exists,
so no consumer can `nix flake update` to a half-published rev. Every step is
idempotent, so a failed run is re-runnable from the Actions UI.

### Public cachix cache `braid`

The cache is public; consumers add the substituter `https://braid.cachix.org` +
its public key and need no auth. `release.yml` sets `skipPush: true` on
cachix-action and does one explicit `cachix push braid <out>`, so **only**
`braid-cli-unwrapped` (x86_64-linux) lands in the cache -- exactly what the
module default (`flake.nix#nixosModules.default`, which sets `package =
braid-cli-unwrapped`) consumes. The wrapped `braid` would duplicate all storage
tools for no consumer benefit.

### Behavioral gate is local, not in CI

braid does **not** run the NixOS VM suite in GitHub Actions, and neither
`just release` nor `release.yml` requires a VM result. The release path runs the
Rust tests (`just test-rust`) and the version-SoT eval check in `release.yml` on
the tag, then builds and publishes. `.github/workflows/test.yml` stays
`workflow_dispatch`-only (its `push`/`pull_request` triggers remain disabled).

VM coverage is a manual, per-release choice: when a release warrants it, run the
suite outside the release automation -- `just test-vm` locally, or a
`workflow_dispatch` run of `test.yml`. `just release` keeps a cheap local compile
gate (`nix build braid-cli-unwrapped`, darwin-native) so a Rust compile break is
caught before the irreversible tag, but it does not gate on a VM run.

This is a deliberate scope choice: the VM suite is slow and runs on the
maintainer's machine through the linux-builder, and keeping it out of the release
path keeps releases fast and free of VM flakiness, without turning the expensive
VM workflow into a push-triggered CI gate. The tradeoff is that release
behavioral coverage rests on maintainer discipline, not an automated CI gate.
**Revisit-if** braid gains additional maintainers or the VM suite becomes cheap
enough to run in CI; at that point a master VM gate plus a fail-closed parent
check in `just release` would make the behavioral gate automatic.

### No-follows is the recommended consumer default

This ADR is the authoritative home for the cache-path-identity rationale; ADR 010
points here. The recommended consumer snippet does **not** set
`braid.inputs.nixpkgs.follows`. With no follows, braid's `nixpkgs` input stays on
its pinned `nixos-26.05` -- the exact nixpkgs the release cache is built against
-- so `braid-cli-unwrapped` resolves to the cached store path: a cache hit.
Setting `follows = "nixpkgs"` rebuilds `braid-cli-unwrapped` against the
consumer's nixpkgs, producing a different store path and a cache miss (the NAS
recompiles Rust, defeating the cache). `follows` remains a valid advanced opt-out
(smaller closure via nixpkgs dedup) at the cost of release-cache path identity;
it also moves the pinned tool versions onto the consumer's nixpkgs (see ADR 010).

This aligns docs with reality: the deployed consumer already runs no-follows with
a deliberate "do NOT set follows" tool-version-boundary comment.

### Public-repo trust model (every secret-bearing workflow)

Going public widens the threat model for every workflow that consumes a secret,
not just the release path:

- `release.yml` is **fork-safe by trigger**: `push: tags` only, no
  `pull_request`, and forks cannot push tags upstream, so `CACHIX_AUTH_TOKEN`
  never reaches a fork.
- `claude.yml` is **not** trigger-safe -- it fires on public
  issue/comment/review events. It is hardened with a trusted-author gate: each
  event arm ANDs the `@claude` trigger with `author_association` in
  `OWNER`/`MEMBER`/`COLLABORATOR`, so a stranger's `@claude` comment never starts
  the job and cannot spend `CLAUDE_CODE_OAUTH_TOKEN`.

Any future workflow that consumes a secret must re-clear this bar before it
merges.

## Risks / gotchas

- `publish = false` (both layers) is mandatory -- braid is a private crate that
  must never reach crates.io.
- **Dangling tag on CI failure**: the tag exists but the `release` FF (the last
  step) never runs, so `release` does not advance and consumers are unaffected no
  matter which earlier step failed. Recover by re-running the same workflow
  (transient/config failures) or by fixing `master` and moving the same version
  tag to the fixed commit (the runbook has exact commands).
- **Cache trust on the consumer**: skip the public-key step and the consumer
  reaches the cache but rejects the signature and silently rebuilds from source.
- **Master protection vs. the bump push**: `cargo release` pushes the bump commit
  straight to `master`; a required-PR ruleset on `master` that does not exempt
  the releaser makes `just release` fail after the local commit/tag. Keep the
  releaser exempt.
- **Concurrent releases**: `concurrency` serializes release runs
  (`cancel-in-progress: false`) and `queue: max` lets up to 100 later tags wait
  (FIFO by the time each starts waiting on the group), so a burst of tags drops
  no release. Because that order is wait-start time, not dispatch time, a
  near-simultaneous burst can still start out of order and fail an older tag's
  `release` FF as a non-fast-forward (benign -- `release` only moves forward --
  but a red run). The runbook rule stays one active release tag at a time.

## See

- `justfile` -- the `release` recipe (Mac-side bump + local gates).
- `.github/workflows/release.yml` -- CI build, cache push, GitHub release, `release` FF.
- `tests/eval/version-matches-cargo.nix` -- the version single-source-of-truth eval guard.
- [Releasing](../../dev/releasing.md) -- the operator runbook.
- [Toolchain pinning](010-toolchain-pinning.md) -- no-follows default and parser-critical tool pinning.
