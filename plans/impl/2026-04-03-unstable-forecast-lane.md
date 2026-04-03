# Plan: Unstable compatibility forecast lane

## Context

braid pins nixpkgs to `nixos-25.11`. Tool versions (btrfs-progs 6.17.1, cryptsetup, util-linux) are frozen until the next nixpkgs bump. To foresee breakage (parser changes, output format changes, behavior changes) before they hit stable, we want a separate unstable lane that runs the full test suite and parser fixture pipeline against `nixos-unstable`.

Two lanes:

- **Stable lane** (unchanged): pinned `nixos-25.11`, committed fixtures in `cli/tests/fixtures/nixos-25.11/`, golden tests in `golden_nixos_25_11.rs`. This is the authoritative compatibility contract.
- **Unstable lane** (new): `nixos-unstable` via `--override-input`, committed fixtures in `cli/tests/fixtures/nixos-unstable/`, golden tests in `golden_nixos_unstable.rs`. Tracked forecast lane — upstream output changes are visible in git history. Not authoritative: unstable failures signal upcoming drift, not a redefinition of braid's supported contract.

## Changes

### 1. `justfile` — add `--unstable` flag to `_build-checks`

Add to the arg parser loop (alongside `-v`, `-rebuild`, `-k`):

```bash
nix_override=""
...
elif [ "$arg" = "--unstable" ]; then nix_override="--override-input nixpkgs github:NixOS/nixpkgs/nixos-unstable"
```

Thread `$nix_override` into both `nix eval` (line 34) and both `nix build` calls (lines 42, 50). Leave unquoted so it word-splits to nothing when empty.

### 2. `justfile` — add `test-all-unstable` recipe

```just
# Run all tests (including repro) against nixos-unstable to foresee tool changes
test-all-unstable:
    just _build-checks checks --unstable && just _build-checks reproChecks --unstable
```

Do **not** change `test-all` semantics.

### 3. `justfile` — add unstable fixture capture recipes

`capture-fixtures-unstable` clears the directory first (fresh base set). `capture-progress-fixtures-unstable` merges into the existing directory (adds progress-only fixtures on top). Run in this order.

```just
# Capture tool output fixtures from nixos-unstable into cli/tests/fixtures/nixos-unstable/
capture-fixtures-unstable:
    nix build .#checks.{{system}}.capture-tool-fixtures --override-input nixpkgs github:NixOS/nixpkgs/nixos-unstable -L
    rm -rf cli/tests/fixtures/nixos-unstable
    mkdir -p cli/tests/fixtures/nixos-unstable
    cp -f result/fixtures/* cli/tests/fixtures/nixos-unstable/
    @echo "Unstable fixtures written to cli/tests/fixtures/nixos-unstable/"

# Capture in-progress fixtures from nixos-unstable (adds to existing unstable fixtures)
capture-progress-fixtures-unstable:
    nix build .#checks.{{system}}.progress-monitoring --override-input nixpkgs github:NixOS/nixpkgs/nixos-unstable -L
    mkdir -p cli/tests/fixtures/nixos-unstable
    cp -f result/fixtures/* cli/tests/fixtures/nixos-unstable/
    @echo "Unstable progress fixtures written to cli/tests/fixtures/nixos-unstable/"
```

### 4. Shared golden test harness + per-lane instantiation

Extract the shared golden test definitions into a macro module and instantiate it twice, avoiding a large duplicated test file.

**`cli/tests/golden_common.rs`** — shared harness:

Contains `golden_test!` macro, `is_dm_or_mapper_path` helper, `fixture` loader (parameterized by dir and require mode), and all test definitions as a `golden_suite!` macro.

```rust
/// Invoke as: golden_suite!(FIXTURE_DIR, require: true/false)
/// - require: true  → panic on missing fixture (unstable lane)
/// - require: false → skip on missing fixture (stable lane)
```

**`cli/tests/golden_nixos_25_11.rs`** — stable lane:

```rust
const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nixos-25.11");
golden_suite!(FIXTURE_DIR, require: false);
```

Behavior unchanged: skips missing fixtures (same as today).

**`cli/tests/golden_nixos_unstable.rs`** — unstable lane:

```rust
const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nixos-unstable");
golden_suite!(FIXTURE_DIR, require: true);
```

`require: true` means missing fixtures **fail** the test. This prevents `just test-rust-unstable` from silently passing with incomplete fixtures.

### 5. `justfile` — add `test-rust-unstable`

```just
# Run golden parser tests against unstable fixtures (requires capture-fixtures-unstable first)
test-rust-unstable:
    cargo test --test golden_nixos_unstable
```

Runs only the unstable golden tests. Stable `just test-rust` is unchanged.

### 6. `cli/src/parse/mod.rs` — update fixture contract text

Line 12 currently says:
```
//! - File-based fixtures must come from `tests/fixtures/nixos-25.11` only.
```

Update to:
```
//! - Stable contract fixtures live in `tests/fixtures/nixos-25.11/` (authoritative).
//! - Unstable forecast fixtures live in `tests/fixtures/nixos-unstable/` (tracked, non-authoritative).
//! - Parser module unit tests use stable fixtures only.
```

### 7. `AGENTS.md` — document both lanes

Update the Commands section to add:
- `just test-all-unstable` — Run all VM tests against nixos-unstable.
- `just capture-fixtures-unstable` — Capture parser fixtures from nixos-unstable.
- `just capture-progress-fixtures-unstable` — Capture progress fixtures from nixos-unstable.
- `just test-rust-unstable` — Run golden parser tests against unstable fixtures.

Update the Parser Compatibility section to distinguish:

**Stable (pinned contract):**
- Existing `just test-parsers`, `just test-rust`, `just capture-fixtures`, `just capture-progress-fixtures` — unchanged.
- Fixtures in `cli/tests/fixtures/nixos-25.11/` are committed and authoritative.

**Unstable (tracked forecast lane):**
- `just test-all-unstable` — VM tests against nixos-unstable. Covers CLI-reachable parsers against live tool output but does not cover the full parser surface (TUI-only parsers, unused parsers).
- `just capture-fixtures-unstable` + `just capture-progress-fixtures-unstable` + `just test-rust-unstable` — covers all 18 parsers against unstable tool output via golden fixtures. Missing fixtures fail (not skip).
- Unstable fixtures are committed so upstream output changes are visible in git history. They are non-authoritative: failures signal upcoming drift, not a contract violation.

**Full unstable canary workflow:**
1. `just test-all-unstable`
2. `just capture-fixtures-unstable`
3. `just capture-progress-fixtures-unstable`
4. `just test-rust-unstable`

## Files modified

| File | Change |
|------|--------|
| `justfile` | `--unstable` in `_build-checks`, 4 new recipes |
| `cli/tests/golden_common.rs` | New — shared golden test harness macro |
| `cli/tests/golden_nixos_25_11.rs` | Refactor to use `golden_suite!` from `golden_common.rs` |
| `cli/tests/golden_nixos_unstable.rs` | New — unstable lane, `require: true` |
| `cli/src/parse/mod.rs` | Update fixture contract comment (line 12) |
| `AGENTS.md` | Document unstable lane in Commands + Parser Compatibility |

## Verification

1. `just test hello-world --unstable` — single VM test against unstable
2. `just test-all-unstable` — full VM suite against unstable
3. `just capture-fixtures-unstable && just capture-progress-fixtures-unstable` — fixtures land in `cli/tests/fixtures/nixos-unstable/`
4. `just test-rust-unstable` — golden tests pass against captured unstable fixtures
5. `just test-rust-unstable` without fixtures — should **fail**, not silently pass
6. `just test hello-world` — stable lane unchanged
7. `just test-rust` — stable golden tests still pass, behavior unchanged
