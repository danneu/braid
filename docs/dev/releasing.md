---
intent: Operator runbook for cutting a braid release -- prerequisites, the local
  pre-release test step, the normal `just release` flow, first-release bootstrap,
  and CI-failure recovery. Read before running `just release` or recovering a
  failed release. For the design rationale see ADR 029.
---

# Releasing

Copy-pasteable runbook for cutting a braid release. The design rationale (why the
`release` branch is the channel, why the x86_64-linux build runs in CI, why
no-follows is the consumer default, the public-repo trust model) lives in
[ADR 029](../design/decisions/029-release-process.md).

## Prerequisites (one-time)

- The public Cachix cache `braid` exists; you have captured its public key
  (`braid.cachix.org-1:...`).
- `CACHIX_AUTH_TOKEN` (a push token for that cache) is set as a GitHub Actions
  repo secret.
- The `release` branch is **not** branch-protected against the Actions token --
  CI fast-forwards it with `GITHUB_TOKEN`.
- The releaser can push directly to `master`. `cargo release` commits the bump
  and pushes it to `master` (not via a PR), so any required-PR ruleset on
  `master` must exempt the releaser, or `just release` fails mid-run after the
  local commit/tag.
- Run from `nix develop .#release` (provides `cargo-release`, `cargo`, `gh`,
  `just` on the Mac; the default devShell is Linux-only and has no `cargo`).

## Before releasing

braid does **not** run the NixOS VM suite in CI, and neither `just release` nor
`release.yml` requires a VM result. VM coverage is a manual, per-release choice:
when a release warrants it, run the suite *outside* the release automation --
either locally:

```sh
just test-vm
```

or by triggering `test.yml` manually via `workflow_dispatch` (its only active
trigger). Do not re-enable `test.yml`'s `push`/`pull_request` triggers, and do
not wire `just release` to depend on it.

`just test-rust` (fast, no VM) does gate the release automatically: `release.yml`
re-runs it on the tag, and `just release` runs a local compile gate
(`nix build braid-cli-unwrapped`) before tagging.

## Normal release

From `nix develop .#release`:

```sh
just release <patch|minor|major>
```

This bumps `cli/Cargo.toml` + the `braid-cli` entry in `Cargo.lock`, commits
`chore(release): vX.Y.Z`, tags `vX.Y.Z`, and pushes `master` + the tag. The tag
triggers `release.yml`. Follow CI:

```sh
gh run list --workflow release.yml
gh run watch <run-id>
```

`release.yml` builds the x86_64-linux binary, pushes it to the `braid` cache,
creates the GitHub release, and -- last -- fast-forwards the `release` branch (the
consumer channel). Because the FF is last, consumers see the new rev only after
the cache is warm and the release object exists.

Pre-1.0 bumps are plain semver:

| Level   | From    | To      |
| ------- | ------- | ------- |
| `patch` | `0.0.1` | `0.0.2` |
| `minor` | `0.0.1` | `0.1.0` |
| `major` | `0.0.1` | `1.0.0` |

So `minor` jumps to `0.1.0`, not `0.0.x` -- expected, not a surprise.

Consumers upgrade by bumping the lock to the new `release` tip:

```sh
nix flake update braid   # then nixos-rebuild switch
```

(A consumer may wrap this in a shortcut, e.g. a `braid:upgrade` shell function.)

**One active release tag at a time.** `release.yml` sets `queue: max`, so a burst
of tags all queue (up to 100, FIFO by the time each starts waiting on the
concurrency group) and none is dropped. But that order is wait-start time, not
dispatch time, so pushing the next tag before the prior `release.yml` run finishes
risks two tags starting out of dispatch order -- the older one's `release`
fast-forward then fails as a non-fast-forward. That outcome is benign for
consumers (`release` only ever moves forward) but shows a red run. So push (or
`just release`) one tag at a time.

## Bootstrapping the first release (v0.0.1)

`cli/Cargo.toml` is already `0.0.1`, so the first release is "publish current,"
not a bump. Do it by hand -- `just release` is for bumps:

1. Confirm the `master` HEAD you will tag passes the local behavioral gate
   (`just test-vm`, `just test-rust`).
2. Tag that HEAD directly and push:

   ```sh
   git tag -a v0.0.1 -m v0.0.1
   git push origin v0.0.1
   ```

The tag triggers `release.yml`, which **creates** the `release` branch, warms the
cache, and cuts the GitHub release. All later `just release` runs bump from
`0.0.2`.

## If release CI fails

First rule: **never re-run `just release` after a tag exists** -- that would bump
again.

- **Transient or config-only failure** -- re-run the existing workflow:

  ```sh
  gh run rerun <run-id>
  gh run watch <run-id>
  ```

- **Bad tagged code** -- fix `master`, then move the same version tag to the
  fixed commit:

  ```sh
  git push origin :refs/tags/vX.Y.Z
  git tag -d vX.Y.Z
  git tag -a vX.Y.Z -m vX.Y.Z
  git push origin vX.Y.Z
  ```

Why this is safe: the `release` fast-forward is the last step, so until it runs
`release` has not advanced and consumers cannot `nix flake update` to the new rev
-- a failure at any earlier step (test, build, cache, or `gh release create`)
leaves consumers untouched. Re-running converges: the cache push and
`gh release create` are idempotent, and the FF re-pushes the same commit.
