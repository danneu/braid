system := `nix eval --impure --expr builtins.currentSystem --raw`

# Internal: shared build logic for VM test commands
_build-checks flake_attr *args:
    #!/usr/bin/env bash
    set -euo pipefail
    verbose=""
    rebuild=false
    keep_going=""
    tests=()
    for arg in {{args}}; do
        if [ "$arg" = "-v" ] || [ "$arg" = "--verbose" ]; then verbose="-L"
        elif [ "$arg" = "-rebuild" ]; then rebuild=true
        elif [ "$arg" = "-k" ] || [ "$arg" = "--keep-going" ]; then keep_going="--keep-going"
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
        # Build all checks in the given flake attr.
        # Uses nix eval to enumerate check names, then builds them all in a
        # single `nix build` so nix can run them concurrently (up to the
        # linux-builder's maxJobs). A single invocation also evaluates shared
        # dependencies once and avoids SQLite lock contention that happens
        # when multiple nix processes hit the store.
        # https://nix.dev/manual/nix/stable/advanced-topics/cores-vs-jobs.html
        mapfile -t names < <(nix eval ".#{{flake_attr}}.{{system}}" \
            --apply 'cs: builtins.concatStringsSep "\n" (builtins.attrNames cs)' --raw)
        installables=()
        for t in "${names[@]}"; do
            installables+=(".#{{flake_attr}}.{{system}}.$t")
        done
        flags=()
        if $rebuild; then flags+=(--rebuild); fi
        nix build "${installables[@]}" "${flags[@]}" --max-jobs 4 "${build_dir[@]}" $keep_going $verbose || rc=$?
    else
        installables=()
        for t in "${tests[@]}"; do
            installables+=(".#{{flake_attr}}.{{system}}.$t")
        done
        flags=()
        if $rebuild; then flags+=(--rebuild); fi
        nix build "${installables[@]}" "${flags[@]}" --max-jobs 4 "${build_dir[@]}" $keep_going $verbose || rc=$?
    fi
    if [ $rc -eq 0 ]; then
        printf '\033]777;notify;braid;tests passed\033\\'
    else
        printf '\033]777;notify;braid;tests failed\033\\'
    fi
    exit $rc

# Run NixOS VM tests — excludes repro tests (pass test names to run specific tests, add -v for verbose)
test *args:
    just _build-checks checks {{args}}

# Run repro tests only (same flags as `test`: -v, -rebuild, -k, or named tests)
test-repro *args:
    just _build-checks reproChecks {{args}}

# Run all tests including repro (zero-arg only — use `test` or `test-repro` for named tests)
test-all:
    just _build-checks checks && just _build-checks reproChecks

# Run NixOS VM tests with parallel evaluation (requires nix-fast-build)
# Add --no-nom to replace the dep graph with a one-liner progress bar
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
    nix-fast-build --no-link -j 8 --eval-workers 4 -f ".#checks" "${build_dir[@]}"

# Run parser compatibility canary tests (CLI parsers against live tool output)
test-parsers *args:
    just test braid-status-rust braid-status-during-balance braid-idle braid-discover braid-browse {{args}}

# Run Rust unit tests
test-rust:
    cargo test

# Capture tool output fixtures into cli/tests/fixtures/nixos-25.11/
capture-fixtures:
    nix build .#checks.{{system}}.capture-tool-fixtures -L
    chmod u+w cli/tests/fixtures/nixos-25.11/* 2>/dev/null || true
    cp -f result/fixtures/* cli/tests/fixtures/nixos-25.11/
    @echo "Fixtures written to cli/tests/fixtures/nixos-25.11/"

# Capture in-progress fixtures from progress-monitoring VM test
capture-progress-fixtures:
    nix build .#checks.{{system}}.progress-monitoring -L
    chmod u+w cli/tests/fixtures/nixos-25.11/* 2>/dev/null || true
    cp -f result/fixtures/* cli/tests/fixtures/nixos-25.11/
    @echo "Progress fixtures written to cli/tests/fixtures/nixos-25.11/"

# Boot interactive VM with btrfs + Samba playground
playground:
    nix run .#playground

# Build and push x86_64-linux binary to cachix
cachix:
    nix build .#packages.x86_64-linux.braid-cli-unwrapped --no-link --print-out-paths | xargs nix run nixpkgs#cachix -- push braid

# Run hardware canary tests (requires root, DESTRUCTIVE to specified drives)
# Usage: just test-hw
test-hw *args:
    sudo python3 tests/hw/runner.py {{args}}

# Destroy an entire braid pool (dev use only — wipes LUKS signatures + state files)
destroy config="/etc/braid/config.json":
    ./scripts/braid-destroy.sh {{config}}
