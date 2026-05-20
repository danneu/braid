# Add `nix` Rust crate source to `just fetch-references`

## Context

`reference/` holds upstream source for every external tool braid parses or wraps, so agents can read authoritative source without leaving the repo. Today it only covers the system tools pinned by `flake.lock` (btrfs-progs, cryptsetup, util-linux, smartmontools, nut, ...). When an agent needs to verify a `nix` Rust crate API -- feature gates, `OwnedFd` ownership types, `termios`/`signal` helpers, `unistd` semantics -- the only option is to spelunk `~/.cargo/registry/src/...`, which is brittle (path varies by user) and undiscoverable from a clean checkout.

Add the `nix` crate, pinned to the exact version in `Cargo.lock`, to `reference/` via `just fetch-references`. The crate is downloaded from crates.io as the `.crate` tarball that Cargo itself fetches, so the source matches what's actually compiled.

## Approach

Extend `scripts/fetch-references.py` with a third `fetch_type` -- `"cargo"` -- alongside the existing `"git"` and `"tarball"` arms. Version is read from `Cargo.lock` (not nixpkgs). Download `https://static.crates.io/crates/nix/nix-<version>.crate`, verify SHA-256 against the lockfile `checksum`, extract the gzipped tar into `reference/nix-crate/` with the top-level `nix-<version>/` directory stripped.

Reuse the script's existing tempdir-and-swap idioms so single-resource and all-resource modes both work without code duplication.

## Implementation

### 1. `scripts/fetch-references.py`

**Imports**: add `hashlib` and `tomllib` (Python 3.11+; the file already uses 3.10+ `T | None` syntax so the implied minimum just shifts up one minor).

**`Dep.fetch_type` literal**: extend from `Literal["git", "tarball"]` to `Literal["git", "tarball", "cargo"]`.

**`DEPS` list**: append one entry. The `repo` and `tag` fields are unused for cargo type; set placeholders:
```python
Dep("nix-crate", "cargo", "", lambda v: v),
```
The `nix_attr` field doubles as the directory name and `--list` label, matching the existing convention.

**New helper `read_nix_crate_pin() -> tuple[str, str]`**: resolve via the `braid-cli` direct dependency, not by global name uniqueness, so a transitive `nix` version (Cargo allows multiple versions of one crate in a single graph) cannot ambiguate the lookup. The current lockfile already has this shape: `braid-cli` depends directly on `nix 0.31.3`, while transitive dependencies still pull `nix 0.29.0`. Selecting the transitive `0.29.0` package is a bug. Two-step lookup:

1. Parse `ROOT / "Cargo.lock"` with `tomllib`. Find the `[[package]]` table where `name == "braid-cli"`. Read its `dependencies` array. Locate the entry that references `nix`. Cargo formats these as:
   - bare `"nix"` when exactly one version of `nix` is in the graph,
   - `"nix <version>"` when multiple versions of `nix` are in the graph,
   - `"nix <version> (registry+<url>)"` when there is also a source ambiguity.

   Parse all three forms. Capture the version qualifier if present.
2. Select the `[[package]]` table that matches: same `name == "nix"`, plus `version` and (if specified) `source` matching the qualifier captured above. Error out with a clear message if zero matches; if the qualifier was bare and multiple `nix` packages exist, error out -- the `braid-cli` dep needs to be re-resolved or the lockfile is malformed.

Return `(version, checksum)` from the selected package table. In the current lockfile this must return `("0.31.3", "cf20d2fde8ff38632c426f1165ed7436270b44f199fc55284c38276f9db47c3d")`.

**New helper `fetch_cargo_crate(target: Path) -> None`**: mirrors `fetch_tarball()`, but
- gets `(version, checksum)` from `read_nix_crate_pin()`,
- downloads `https://static.crates.io/crates/nix/nix-{version}.crate` via `urllib.request.urlretrieve` to a temp file,
- verifies SHA-256 of the downloaded file equals `checksum` -- hard-fail with a clear error on mismatch,
- opens with `tarfile.open(path, "r:gz")` (gzip, not xz),
- extracts to a sub-tempdir with `filter="data"`, strips the `nix-<version>/` top-level prefix, `shutil.move`s the inner directory to `target`,
- prints `version` and source URL with the same `  → ...` indentation the other fetchers use, so output stays consistent.

**Dispatch in `fetch_source_repos`**: add the third branch. Hoist `nix_version(nixpkgs, dep.nix_attr)` into the git/tarball branches so the cargo branch doesn't call nixpkgs at all. Cargo branch self-prints its version after reading `Cargo.lock`.

**Module docstring**: update the `Includes:` line to mention `nix crate` and note it is pinned by `Cargo.lock` rather than `flake.lock`. Update the `Usage:` example to show `python3 scripts/fetch-references.py nix-crate`.

No changes needed to `--list` (it iterates `DEPS`, so the new entry appears automatically), to `filter_deps` (string-keyed lookup works), or to the swap logic (it iterates `for dep in deps` and uses `dep.nix_attr`).

### 2. `justfile`

No change required -- the recipe is a pass-through (`python3 scripts/fetch-references.py {{ARGS}}`). Optionally update the comment block above the recipe (lines 200-202) to show the new resource name, mirroring the existing `linux` example.

### 3. `AGENTS.md` -- Reference source section

Add a new bullet after `coreutils` (alphabetical placement isn't strictly observed; group it near the other Rust-source reference, `hddfancontrol`, or at the end). Pattern follows the existing style:

```markdown
- **nix (Rust crate)** -- [nix-rust/nix](https://github.com/nix-rust/nix)
  - **Source:** [`reference/nix-crate/src/`](reference/nix-crate/src/) -- Rust crate at the version pinned in `Cargo.lock`, not `flake.lock`. `unistd.rs` (User/Group/chown/exec helpers, fd ownership types), `fcntl.rs` (`open`, `flock`, `OFlag`), `errno.rs` (`Errno`), `sys/stat.rs` (`Mode`), `sys/signal.rs` (sigaction, signal handlers), `sys/termios.rs` (termios constants, terminal flags).
  - **Docs:** No separate docs dir -- rustdoc is inline as `///` doc comments on each item. [`reference/nix-crate/Cargo.toml`](reference/nix-crate/Cargo.toml) declares the feature gates (braid currently enables `fs`, `user`, `term`, and `signal`); consult it before reaching for a `nix` API to confirm which feature it lives under.
  - **Use for:** Touching any `nix::` API, checking feature gates, understanding fd-ownership types, signal-safe helpers, or termios constants. Refresh after any change to the `nix` line in `cli/Cargo.toml` or any `cargo update`-driven bump in `Cargo.lock`.
```

Also amend the section preamble (currently: *"`reference/` contains shallow clones of upstream repos at the versions pinned in nixpkgs"*) to acknowledge the new pinning source -- a parenthetical like *"...at the versions pinned in nixpkgs (or, for Rust crates, in `Cargo.lock`)"*.

### 4. `manual/development.md` -- nixpkgs-bump workflow

The current "Update vendored reference source" step (`manual/development.md:105-111`) frames the refresh as a flake-update follow-up only. With `nix-crate` pinned by `Cargo.lock`, that framing no longer covers every refresh trigger.

Make two edits there:

1. Amend the preamble sentence on line 107 to acknowledge mixed pin sources -- something like: *"`reference/` contains upstream source used for code-level reference (parser behavior, output formats, config schemas). Most entries track `flake.lock` (nixpkgs-pinned tools); the `nix-crate` entry tracks `Cargo.lock`."*
2. Add a new short section after the existing "4. Refresh fixtures and run tests" titled *"5. Update vendored crate sources"* (or fold into section 3 as a sub-bullet) telling contributors to run `just fetch-references nix-crate` after any change that touches the `nix` line of `cli/Cargo.toml` or that bumps `nix` in `Cargo.lock` (e.g. `cargo update -p nix`).

## Files to modify

- `scripts/fetch-references.py` -- add `tomllib`/`hashlib` imports, new `Dep` entry, `read_nix_crate_pin`, `fetch_cargo_crate`, third dispatch arm, updated docstring.
- `AGENTS.md` -- new bullet in the Reference source list, updated preamble sentence.
- `manual/development.md` -- preamble sentence on line 107 + new section (or sub-bullet) covering `nix-crate` refresh after Cargo bumps.
- `justfile` -- optional: update the usage comment above `fetch-references`.

## Existing utilities to reuse

- `tempfile.NamedTemporaryFile` + finally-cleanup -- mirror `fetch_tarball` (`scripts/fetch-references.py:118-141`).
- `tarfile.open(..., filter="data")` + strip-top-prefix + `shutil.move` -- same idiom, just `"r:gz"` instead of `"r:xz"`.
- Tempdir-staging + per-dep swap -- already handled by `main()`; no new code path.
- `--list` enumeration, `filter_deps` lookup -- driven off `Dep.nix_attr`; no change needed.

## Verification

Run from repo root:

1. `python3 scripts/fetch-references.py --list` -- confirm `nix-crate` is listed alongside the existing resources.
2. `python3 scripts/fetch-references.py nix-crate` -- confirm:
   - it prints version `0.31.3` (the `braid-cli` direct dependency in the current `Cargo.lock`, not the transitive `0.29.0` entry),
   - the SHA-256 check passes (no error),
   - `reference/nix-crate/` exists after the run,
   - other already-fetched `reference/*` dirs (e.g. `coreutils/`) are untouched.
3. Inspect the result:
   ```
   ls reference/nix-crate/src/sys/termios.rs reference/nix-crate/src/sys/signal.rs reference/nix-crate/src/unistd.rs reference/nix-crate/Cargo.toml
   ```
   All four paths exist.
4. Confirm crate version matches `Cargo.lock`:
   ```
   grep -m1 '^version' reference/nix-crate/Cargo.toml
   ```
   reports `version = "0.31.3"`.
5. Bad-input test: temporarily run `python3 scripts/fetch-references.py nix-crat` (typo) -- confirm the existing unknown-resource error path fires with the new entry in the `Available:` list.
6. Negative-path integrity test (proves the checksum check actually runs and that swap stays clean on failure):
   ```bash
   cp Cargo.lock Cargo.lock.bak
   # Flip one hex digit in the nix checksum in Cargo.lock (e.g. via your editor).
   rm -rf reference/nix-crate
   python3 scripts/fetch-references.py nix-crate    # must exit non-zero with a clear sha256-mismatch error
   test ! -e reference/nix-crate                     # must not have created or partially populated the target
   mv Cargo.lock.bak Cargo.lock
   ```
   Confirms: (a) checksum verification is actually wired up (not silently skipped), (b) on failure the staged dir is discarded and `reference/nix-crate` is not left half-written, (c) other `reference/*` dirs are untouched. After restoring `Cargo.lock`, re-run step 2 to leave `reference/nix-crate` in a good state.
7. Optional, time/network permitting: `just fetch-references` (full refresh) -- confirm `nix-crate` is fetched alongside the rest and the all-resource atomic swap still works (`reference/` is never absent mid-run).

## Out of scope

- Vendoring the crate for builds (no `[source.crates-io].replace-with` in `.cargo/config.toml`, no `cargo vendor`).
- Changing dependency resolution or `Cargo.lock`.
- Replacing or augmenting Cargo's registry cache.
- Adding other Rust crates to `reference/`. Generalizing the cargo arm beyond `nix` (e.g. multi-crate, dynamic crate names) is deferred until a second crate is actually needed; for now, the crate name `"nix"` is hardcoded in `read_nix_crate_pin` and `fetch_cargo_crate`.

## Implementation notes

- Adjusted the crate-version verification command to `grep -m1 '^version'` because `reference/nix-crate/Cargo.toml` contains dependency version fields after the package version.
