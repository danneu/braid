system := `nix eval --impure --expr builtins.currentSystem --raw`

# Internal: shared build logic for VM test commands
_build-checks flake_attr *args:
    #!/usr/bin/env bash
    set -euo pipefail
    verbose=""
    rebuild=false
    keep_going=""
    nix_override=""
    tests=()
    for arg in {{args}}; do
        if [ "$arg" = "-v" ] || [ "$arg" = "--verbose" ]; then verbose="-L"
        elif [ "$arg" = "-rebuild" ]; then rebuild=true
        elif [ "$arg" = "-k" ] || [ "$arg" = "--keep-going" ]; then keep_going="--keep-going"
        elif [ "$arg" = "--unstable" ]; then nix_override="--override-input nixpkgs github:NixOS/nixpkgs/nixos-unstable"
        else tests+=("$arg")
        fi
    done
    build_dir=()
    if [ -d /tmp-braid ]; then
        build_dir=(--option build-dir /tmp-braid)
        echo "build-dir: /tmp-braid"
    else
        echo "build-dir: default"
    fi
    rc=0
    if [ ${#tests[@]} -eq 0 ]; then
        # Build all checks in the given flake attr. Uses nix eval to enumerate
        # check names, then builds them all in a single `nix build` so the
        # scheduler can run them concurrently. The `--max-jobs N` flag gates
        # concurrent local test-driver processes: braid VM tests are
        # aarch64-darwin derivations whose build phase invokes qemu+HVF on the
        # Mac, so this flag is the wall-clock concurrency ceiling. Tuned to Mac
        # RAM budget (see ~/world/agent-docs/linux-builder.md for the full
        # budget).
        # https://nix.dev/manual/nix/stable/advanced-topics/cores-vs-jobs.html
        mapfile -t names < <(nix eval ".#{{flake_attr}}.{{system}}" \
            $nix_override \
            --apply 'cs: builtins.concatStringsSep "\n" (builtins.attrNames cs)' --raw)
        installables=()
        for t in "${names[@]}"; do
            installables+=(".#{{flake_attr}}.{{system}}.$t")
        done
        flags=()
        if $rebuild; then flags+=(--rebuild); fi
        # --no-link: tests run for side effects only; suppress result/result-N symlinks (one per check) that would otherwise pile up in the repo root.
        nix build --no-link "${installables[@]}" "${flags[@]}" --max-jobs 7 "${build_dir[@]}" $nix_override $keep_going $verbose || rc=$?
    else
        installables=()
        for t in "${tests[@]}"; do
            installables+=(".#{{flake_attr}}.{{system}}.$t")
        done
        flags=()
        if $rebuild; then flags+=(--rebuild); fi
        # --no-link: tests run for side effects only; suppress result/result-N symlinks (one per check) that would otherwise pile up in the repo root.
        nix build --no-link "${installables[@]}" "${flags[@]}" --max-jobs 7 "${build_dir[@]}" $nix_override $keep_going $verbose || rc=$?
    fi
    if [ $rc -eq 0 ]; then
        printf '\033]777;notify;braid;tests passed\033\\'
    else
        printf '\033]777;notify;braid;tests failed\033\\'
    fi
    exit $rc

# Run NixOS VM tests -- excludes repro tests. Pass test names to scope the run,
# -v for verbose (full VM logs), -k to continue past the first failure (default
# stops on it), -rebuild to force a rebuild, --unstable to run against
# nixos-unstable (e.g. `just test-vm hello-world --unstable`).
#
# The full (no-arg) suite takes 20-30 min. Default to focused runs
# (`just test-vm test1 test2`); reserve the full suite for changes with broad
# blast radius (systemd lifecycle, pool lock, mount/unmount, module-wide
# refactors) or a pre-handoff check on a substantial change. No -v by default;
# add it only to a single failing test whose plain output is unclear, never to
# the whole suite (too much output).
[doc('Run NixOS VM tests, excludes repro (full suite ~20-30 min; -v, -k, --unstable, or named tests)')]
test-vm *args:
    just _build-checks checks {{args}}

# Run repro tests only (same flags as `test-vm`: -v, -rebuild, -k, --unstable, or named tests)
test-repro *args:
    just _build-checks reproChecks {{args}}

# Full stable + unstable pipeline: capture fixtures, run parser tests, run all VM tests
supertest:
    just capture-all-fixtures
    just test-rust
    just test-all
    just capture-all-fixtures-unstable
    just test-rust-unstable
    just test-all-unstable

# Run all tests including repro (zero-arg only — use `test-vm` or `test-repro` for named tests)
test-all:
    just _build-checks checks && just _build-checks reproChecks

# Run NixOS VM tests with parallel evaluation via flake-pinned nix-fast-build
# -j mirrors _build-checks' Mac-RAM-tuned --max-jobs 7
# Before timing with `time just test-fast`, prewarm the tool first:
#   nix run .#nix-fast-build -- --help
test-fast:
    #!/usr/bin/env bash
    set -euo pipefail
    build_dir=()
    if [ -d /tmp-braid ]; then
        build_dir=(--option build-dir /tmp-braid)
        echo "build-dir: /tmp-braid"
    else
        echo "build-dir: default"
    fi
    nix run .#nix-fast-build -- --no-link -j 7 --eval-workers 4 --eval-max-memory-size 2048 --skip-cached --no-nom -f ".#checks" "${build_dir[@]}"

# Run parser compatibility canary tests (CLI parsers against live tool output)
test-parsers *args:
    just test-vm braid-status-rust braid-status-during-balance braid-status-ups braid-idle braid-discover braid-tui-browse {{args}}

# Run Rust unit tests (excludes unstable golden tests). The CLI crate's package
# name is `braid-cli` (not `braid`); prefer this recipe over `cargo test -p <name>`.
test-rust:
    cargo test --lib --bin braid --test golden_nixos_26_05 --test tty_guard --test confirm_yes
    just test-state-modes

# Run state-mode tests that mutate process-wide umask; keep them serial and
# outside the default parallel Rust lane.
test-state-modes:
    cargo test --manifest-path cli/Cargo.toml --lib exact_0600 -- --ignored --test-threads=1

# Format nix source + tests with nixfmt
fmt-nix:
    find flake.nix modules tests vm -name '*.nix' -print0 \
        | xargs -0 nix run nixpkgs#nixfmt --

# Run clippy lints
clippy:
    cargo clippy --manifest-path cli/Cargo.toml --tests

# Auto-fix compiler warnings in CLI tests where possible
clippy-fix:
    cargo fix --manifest-path cli/Cargo.toml --tests --allow-dirty

# Capture tool output fixtures into cli/tests/fixtures/nixos-26.05/
capture-fixtures:
    nix build .#checks.{{system}}.capture-tool-fixtures -L
    chmod u+w cli/tests/fixtures/nixos-26.05/* 2>/dev/null || true
    cp -f result/fixtures/* cli/tests/fixtures/nixos-26.05/
    @echo "Fixtures written to cli/tests/fixtures/nixos-26.05/"

# Capture in-progress fixtures from progress-monitoring VM test
capture-progress-fixtures:
    nix build .#checks.{{system}}.progress-monitoring -L
    chmod u+w cli/tests/fixtures/nixos-26.05/* 2>/dev/null || true
    cp -f result/fixtures/* cli/tests/fixtures/nixos-26.05/
    @echo "Progress fixtures written to cli/tests/fixtures/nixos-26.05/"

# Capture upsc fixtures for the NUT parser into cli/tests/fixtures/nixos-26.05/upsc/
capture-ups-fixtures:
    nix build .#checks.{{system}}.capture-ups-fixtures -L
    mkdir -p cli/tests/fixtures/nixos-26.05/upsc
    chmod u+w cli/tests/fixtures/nixos-26.05/upsc/* 2>/dev/null || true
    cp -f result/fixtures/* cli/tests/fixtures/nixos-26.05/upsc/
    @echo "UPS fixtures written to cli/tests/fixtures/nixos-26.05/upsc/"

# Capture all stable fixtures (base + progress + ups)
capture-all-fixtures:
    just capture-fixtures
    just capture-progress-fixtures
    just capture-ups-fixtures

# Run all tests (including repro) against nixos-unstable to foresee tool changes
test-all-unstable:
    just _build-checks checks --unstable && just _build-checks reproChecks --unstable

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

# Capture upsc fixtures from nixos-unstable into cli/tests/fixtures/nixos-unstable/upsc/
capture-ups-fixtures-unstable:
    nix build .#checks.{{system}}.capture-ups-fixtures --override-input nixpkgs github:NixOS/nixpkgs/nixos-unstable -L
    mkdir -p cli/tests/fixtures/nixos-unstable/upsc
    chmod u+w cli/tests/fixtures/nixos-unstable/upsc/* 2>/dev/null || true
    cp -f result/fixtures/* cli/tests/fixtures/nixos-unstable/upsc/
    @echo "Unstable UPS fixtures written to cli/tests/fixtures/nixos-unstable/upsc/"

# Capture all unstable fixtures (base + progress + ups)
capture-all-fixtures-unstable:
    just capture-fixtures-unstable
    just capture-progress-fixtures-unstable
    just capture-ups-fixtures-unstable

# Run golden parser tests against unstable fixtures (requires capture-all-fixtures-unstable first)
test-rust-unstable:
    cargo test --test golden_nixos_unstable

# Boot interactive VM with btrfs + Samba playground
playground:
    nix run .#playground

# Cut a release: bump cli/Cargo.toml + Cargo.lock, tag vX.Y.Z, push master+tag.
# The tag triggers .github/workflows/release.yml, which builds x86_64-linux,
# pushes to the public `braid` cachix cache, creates the GitHub release (body
# rendered from conventional commits by git-cliff; preview with `just changelog`),
# and fast-forwards the `release` branch (the consumer channel). Run from
# `nix develop .#release`.
#
# VM coverage is a manual, per-release choice -- run `just test-vm` locally (or a
# workflow_dispatch run of test.yml) when a release warrants it. CI does not run
# the VM suite, and this recipe does not require it; release.yml re-runs the fast
# `just test-rust` on the tag. See docs/dev/releasing.md.
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
    # Compile gate: darwin-native via nix (the Mac cannot build x86_64-linux -- CI
    # does). Catches Rust compile breakage before the irreversible tag. The VM
    # behavioral suite is a manual pre-release step (see header), not gated here.
    nix build .#packages.{{system}}.braid-cli-unwrapped --no-link
    cargo release {{level}} --execute --no-confirm
    tag="$(git describe --tags --abbrev=0)"
    echo "==> pushed $tag; release workflow triggered. Watch: gh run watch (release.yml)"

# Preview the release notes git-cliff will render for the next release, using the
# same pinned git-cliff CI publishes with. Before the first v* tag, this prints
# nothing because v0.0.1 intentionally ships with an empty release body.
changelog:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! git tag --list | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
        exit 0
    fi
    nix develop .#release -c git-cliff --unreleased --strip all

# Build and push x86_64-linux binary to cachix. Manual/ad-hoc only: real release
# cache pushes go through .github/workflows/release.yml on the v* tag. Must run on
# an x86_64-linux host (the Mac cannot build x86_64-linux).
cachix:
    nix build .#packages.x86_64-linux.braid-cli-unwrapped --no-link --print-out-paths | xargs nix run nixpkgs#cachix -- push braid

# Run hardware canary tests (requires root, DESTRUCTIVE to specified drives)
# Usage: just test-hw
test-hw *args:
    sudo python3 tests/hw/runner.py {{args}}

# Fetch/refresh reference source repos + btrfs docs at pinned versions
# Fetch reference source/docs at versions pinned in flake.lock or Cargo.lock
# Usage: just fetch-references              (fetch all resources)
#        just fetch-references linux        (fetch only linux kernel)
#        just fetch-references nix-crate    (fetch only nix Rust crate)
#        just fetch-references --list       (list available resources)
fetch-references +ARGS="":
    python3 scripts/fetch-references.py {{ARGS}}

# Build the docs once -- runs mdbook-linkcheck2, mirroring the CI cross-link gate
docs-build:
    nix develop .#docs -c mdbook build docs

# Build and serve the docs locally with live reload
docs-serve:
    nix develop .#docs -c mdbook serve docs --open

# Verify SUMMARY.md parity and docs link integrity
check-docs:
    #!/usr/bin/env bash
    set -euo pipefail
    # Set A: .md files on disk (excluding SUMMARY.md itself)
    disk=$(find docs -path docs/book -prune -o -name '*.md' ! -name SUMMARY.md -type f -print \
           | sed 's|^docs/||' | sort)
    # Set B: link targets extracted from SUMMARY.md markdown links
    summary=$(sed -n 's/.*](\([^)]*\.md\)).*/\1/p' docs/SUMMARY.md | sort)
    # Compare
    missing=$(comm -23 <(echo "$disk") <(echo "$summary"))
    stale=$(comm -13 <(echo "$disk") <(echo "$summary"))
    rc=0
    if [ -n "$missing" ]; then
        printf 'files missing from SUMMARY.md:\n'
        printf '  %s\n' $missing
        rc=1
    fi
    if [ -n "$stale" ]; then
        printf 'stale entries in SUMMARY.md (no file on disk):\n'
        printf '  %s\n' $stale
        rc=1
    fi
    # Link escapes and broken cross-links are caught by mdbook-linkcheck2 during
    # `mdbook build docs` (it forbids linking outside the book root and validates
    # in-book targets), so check-docs no longer re-checks them here.
    # README.md / docs/index.md tables must match SUMMARY.md order and use the
    # canonical labels computed by check-doc-tables.py. Command labels include
    # the experimental marker when their page frontmatter sets experimental: true.
    if ! python3 scripts/docs/check-doc-tables.py; then
        rc=1
    fi
    if [ $rc -eq 0 ]; then echo "docs check ok"; fi
    exit $rc

# Verify docs source frontmatter required for agent-facing pages
check-docs-frontmatter:
    python3 scripts/docs/check-frontmatter.py

# Verify rendered docs do not leak YAML frontmatter
check-docs-rendered-frontmatter:
    python3 scripts/docs/check-rendered-frontmatter.py

# Verify code-side docs anchors resolve to rendered mdBook headings (selftest first)
check-code-doc-anchors:
    python3 scripts/docs/check-code-doc-anchors.py --selftest
    python3 scripts/docs/check-code-doc-anchors.py

# Verify public Rust command entry points carry boundary doc comments (selftest first)
check-cmd-doc-comments:
    python3 scripts/docs/check-cmd-doc-comments.py --selftest
    python3 scripts/docs/check-cmd-doc-comments.py

# Verify durable docs/code/tests do not cite transient plans/wip files (selftest first)
check-plans-refs:
    python3 scripts/docs/check-plans-refs.py --selftest
    python3 scripts/docs/check-plans-refs.py

# Verify durable docs/code/tests do not cite tracked files by line number (selftest first)
check-line-cites:
    python3 scripts/docs/check-line-cites.py --selftest
    python3 scripts/docs/check-line-cites.py

# Verify decision-doc See-section code-span paths resolve
check-docs-see-paths:
    python3 scripts/docs/check-see-paths.py

# Guard: no typographic Unicode in user-facing CLI output (selftest first)
check-output-ascii:
    python3 scripts/docs/check-output-ascii.py --selftest
    python3 scripts/docs/check-output-ascii.py

# Verify ](...) links in AGENTS.md and README.md resolve (path + anchor)
check-doc-links:
    python3 scripts/docs/check-doc-links.py --selftest
    python3 scripts/docs/check-doc-links.py

# Verify every braid doctor check has a row in docs/commands/doctor.md (and no
# stale rows). The cargo test first pins expected_names == run_doctor output, so
# the python guard's code-side source of truth cannot silently go stale.
check-doctor-table:
    cargo test --lib valid_config_parses_ok_declared_disks_skips
    python3 scripts/docs/check-doctor-table-parity.py --selftest
    python3 scripts/docs/check-doctor-table-parity.py

# Destroy an entire braid pool (dev use only — wipes LUKS signatures + state files)
destroy config="/etc/braid/config.json":
    ./scripts/braid-destroy.sh {{config}}
