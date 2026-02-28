system := `nix eval --impure --expr builtins.currentSystem --raw`

# Run NixOS VM tests (pass test names to run specific tests, add -v for verbose)
test *args:
    #!/usr/bin/env bash
    set -euo pipefail
    verbose=""
    rebuild=false
    tests=()
    for arg in {{args}}; do
        if [ "$arg" = "-v" ]; then verbose="-L"
        elif [ "$arg" = "-rebuild" ]; then rebuild=true
        else tests+=("$arg")
        fi
    done
    if [ ${#tests[@]} -eq 0 ]; then
        nix flake check $verbose
    else
        # Build all specified tests in a single `nix build` so nix can run them
        # concurrently (up to the linux-builder's maxJobs). A single invocation
        # also evaluates shared dependencies once and avoids SQLite lock
        # contention that happens when multiple nix processes hit the store.
        # Add --keep-going to continue past failures instead of bailing on first error.
        # https://nix.dev/manual/nix/stable/advanced-topics/cores-vs-jobs.html
        installables=()
        for t in "${tests[@]}"; do
            installables+=(".#checks.{{system}}.$t")
        done
        if $rebuild; then
            nix build "${installables[@]}" --rebuild $verbose
        else
            nix build "${installables[@]}" $verbose
        fi
    fi

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

# Destroy an entire braid pool (dev use only — wipes LUKS signatures + state files)
destroy config="/etc/braid/config.json":
    ./scripts/braid-destroy.sh {{config}}
