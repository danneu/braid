system := `nix eval --impure --expr builtins.currentSystem --raw`

# Run NixOS VM tests (pass test names to run specific tests, add -v for verbose)
test *args:
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
    if [ ${#tests[@]} -eq 0 ]; then
        nix flake check --max-jobs 4 "${build_dir[@]}" $verbose
    else
        # Build all specified tests in a single `nix build` so nix can run them
        # concurrently (up to the linux-builder's maxJobs). A single invocation
        # also evaluates shared dependencies once and avoids SQLite lock
        # contention that happens when multiple nix processes hit the store.
        # https://nix.dev/manual/nix/stable/advanced-topics/cores-vs-jobs.html
        installables=()
        for t in "${tests[@]}"; do
            installables+=(".#checks.{{system}}.$t")
        done
        if $rebuild; then
            nix build "${installables[@]}" --rebuild --max-jobs 4 "${build_dir[@]}" $keep_going $verbose
        else
            nix build "${installables[@]}" --max-jobs 4 "${build_dir[@]}" $keep_going $verbose
        fi
    fi

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
    nix-fast-build --no-link -j 8 --eval-workers 4 "${build_dir[@]}"

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
# Usage: just test-hw --from-config /etc/braid/config.json
test-hw *args:
    sudo python3 tests/hw/runner.py {{args}}

# Destroy an entire braid pool (dev use only — wipes LUKS signatures + state files)
destroy config="/etc/braid/config.json":
    ./scripts/braid-destroy.sh {{config}}
