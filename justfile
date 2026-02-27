system := `nix eval --impure --expr builtins.currentSystem --raw`

# Run NixOS VM tests (pass test names to run specific tests, add -v for verbose)
test *args:
    #!/usr/bin/env bash
    set -euo pipefail
    verbose=""
    tests=()
    for arg in {{args}}; do
        if [ "$arg" = "-v" ]; then verbose="-L"
        else tests+=("$arg")
        fi
    done
    if [ ${#tests[@]} -eq 0 ]; then
        nix flake check $verbose
    else
        for t in "${tests[@]}"; do
            echo "==> $t"
            nix build .#checks.{{system}}.$t $verbose
        done
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
