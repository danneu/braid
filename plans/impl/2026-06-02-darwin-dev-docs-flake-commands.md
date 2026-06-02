# Fix darwin-broken flake commands in dev docs

## Context

`docs/dev/overview.md` is the contributor dev-workflow page. Line 5 scopes it to
macOS ("Tests run on macOS via `nix.linux-builder.enable`"), yet two commands on
the page name flake attributes that exist only on Linux:

- **Dev shell (line 12):** `nix develop` resolves to `devShells.<system>.default`,
  which is added only under `if isLinux`.
- **Building the CLI (line 79):** `nix build .#braid` resolves to
  `packages.<system>.braid`, which is added only under `if isLinux`.

Both are the same root cause: Linux-only flake attributes documented on a
macOS-scoped page. A macOS contributor fails at the very first step (`nix develop`)
and again at the build step.

Confirmed by direct flake evaluation on this darwin host:

```
$ nix eval --raw .#packages.aarch64-darwin --apply '<attrNames>'
braid-cli-unwrapped nix-fast-build playground          # no `braid`, no `default`

$ nix eval --raw .#devShells.aarch64-darwin --apply '<attrNames>'
docs                                                   # no `default`
```

Why they are Linux-gated:

- `packagesFor` (`flake.nix:82-111`) adds wrapped `braid`/`default` only under
  `if isLinux`; the wrapper sets `meta.platforms = linux` and shells out to
  btrfs/luks tooling (`flake.nix:62-75`). The pure-Rust `braid-cli-unwrapped` builds
  on darwin (`flake.nix:38-39`).
- `devShells` (`flake.nix:1068-1077`) adds `default` only under `if isLinux`; that
  shell (`devShellFor`, `flake.nix:113-130`) bundles `btrfs-progs`/`cryptsetup`/
  `nut`/`util-linux`, "none of which evaluate on darwin" (`flake.nix:1066-1067`).
  Only the `docs` shell (mdbook) is exposed on darwin.

Intended outcome: every command shown on this macOS-scoped page either works on
macOS or is explicitly marked Linux-only with the macOS-correct alternative.

This is a documentation-only fix. No code or flake change -- the Linux gating of
both the wrapped binary and the full dev shell is intentional and correct.

## Change

One file, `docs/dev/overview.md`, two sections.

### 1. Dev shell (lines 7-15)

Keep the existing `nix develop` block and toolchain description; append a platform
caveat as a new paragraph after the "The shell includes..." sentence (line 15):

> That shell is Linux-only -- it bundles the storage tools (`btrfs-progs`,
> `cryptsetup`, `util-linux`, `nut`), which don't evaluate on darwin -- so
> `nix develop` resolves only on a Linux host. On macOS, run VM tests through the
> linux-builder and build the CLI with `nix build .#braid-cli-unwrapped` (below);
> `nix develop .#docs` works on macOS but carries only the docs toolchain (mdbook).

### 2. Building the CLI (lines 76-82)

Before:

```markdown
## Building the CLI

​```bash
nix build .#braid
​```

Rust source lives in `cli/`.
```

After:

```markdown
## Building the CLI

​```bash
nix build .#braid-cli-unwrapped
​```

The wrapped `.#braid` and `default` put btrfs/luks tooling on PATH and are Linux-only; on macOS build the pure-Rust `braid-cli-unwrapped`. Rust source lives in `cli/`.
```

Notes for the implementer:

- Single unwrapped lines -- `overview.md` does not hard-wrap prose paragraphs (see
  lines 5, 15, 45). Line breaks shown above are display only.
- Both edits name `default` alongside the primary attr: `flake.nix:97-100` and
  `flake.nix:1076` gate `default` too, so omitting it would be incomplete.
- Wording mirrors the flake's own comments (`flake.nix:38-39`, `:66`,
  `:1066-1067`) so docs and code stay consistent.
- `nix develop .#docs` is the docs shell (mdbook + linkcheck), NOT a Rust dev
  environment -- do not present it as a `cargo` substitute.

## Why this shape

- **Self-contained notes, no cross-link.** No ADR covers the darwin/linux flake
  gating (ADR 010-toolchain-pinning is about tool-version pinning), so inline notes
  are more accurate than an imprecise link.
- **Keep the `nix build` idiom for the build step.** `nix build .#braid-cli-unwrapped`
  is the macOS-correct build path. We deliberately do NOT point macOS readers at
  `cargo build`: the `cargo`-bearing dev shell is Linux-only, and the repo documents
  no macOS host-toolchain workflow -- inventing one is out of scope.
- **Annotate, don't restructure, the Dev shell section.** Matches the surgical shape
  chosen for the build section; the existing `nix develop` block stays for Linux
  readers, with a macOS caveat appended.

## Out of scope (verified, no change needed)

- `README.md:63` (`nix run github:danneu/braid -- --help`) resolves to the flake
  `default` (Linux-only) but is an end-user NAS-install context, not a macOS-dev
  one. Correct as-is.
- The macOS host Rust/`cargo` inner loop: `just test-rust` runs `cargo` directly
  (`justfile:109`), but the repo exposes no nix dev shell for it on darwin and
  documents none. Surfacing or designing that workflow is a separate concern, not
  this docs fix; the Dev shell caveat stays silent on it rather than inventing a
  rustup path.
- `plans/impl/2026-05-25-linux-flake-ergonomics.md` references `nix build .#braid`
  producing `result/bin/braid` -- accurate Linux behavior; plan files are historical
  records, not user docs.
- No other `nix build`/`nix develop`/`nix run .#braid` instructions exist in `docs/`.

## Verification

Documentation-only change -- no Rust unit tests or NixOS VM tests required (no
behavioral code path changes).

1. **Corrected build command works on macOS:**
   `nix build .#braid-cli-unwrapped` -> produces `result/bin/braid`.
2. **Linux-only package claim holds (guards the build note):**
   `nix eval --raw .#packages.aarch64-darwin --apply 'p: builtins.concatStringsSep " " (builtins.attrNames p)'`
   -> `braid-cli-unwrapped nix-fast-build playground` (no `braid`, no `default`).
3. **Linux-only dev-shell claim holds (guards the dev-shell caveat):**
   `nix eval --raw .#devShells.aarch64-darwin --apply 'p: builtins.concatStringsSep " " (builtins.attrNames p)'`
   -> `docs` (no `default`); and `nix develop .#docs` enters successfully on macOS.
4. **Page renders / markdown well-formed:** in the docs shell
   (`nix develop .#docs`), run `mdbook build docs`. No links are added or changed,
   so `mdbook-linkcheck2` is unaffected; this just confirms both sections render.
