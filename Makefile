SYSTEM := $(shell nix eval --impure --expr builtins.currentSystem --raw)

.PHONY: help test test-one test-verbose test-one-verbose playground test-rust capture-fixtures capture-progress-fixtures

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*## "}; {printf "  %-15s %s\n", $$1, $$2}'

test: ## Run all NixOS VM tests
	nix flake check

test-one: ## Run a single test (e.g. make test-one t=hello-world)
	nix build .#checks.$(SYSTEM).$(t)

test-verbose: ## Run all NixOS VM tests (verbose, shows VM logs)
	nix flake check -L

test-one-verbose: ## Run a single test verbose (e.g. make test-one-verbose t=hello-world)
	nix build .#checks.$(SYSTEM).$(t) -L

test-rust: ## Run Rust unit tests
	cd cli && cargo test

capture-fixtures: ## Capture tool output fixtures from nixos VM into cli/tests/fixtures/nixos-25.11/
	nix build .#checks.$(SYSTEM).capture-tool-fixtures -L
	chmod u+w cli/tests/fixtures/nixos-25.11/* 2>/dev/null || true
	cp -f result/fixtures/* cli/tests/fixtures/nixos-25.11/
	@echo "Fixtures written to cli/tests/fixtures/nixos-25.11/"

capture-progress-fixtures: ## Capture in-progress fixtures from progress-monitoring VM test
	nix build .#checks.$(SYSTEM).progress-monitoring -L
	chmod u+w cli/tests/fixtures/nixos-25.11/* 2>/dev/null || true
	cp -f result/fixtures/* cli/tests/fixtures/nixos-25.11/
	@echo "Progress fixtures written to cli/tests/fixtures/nixos-25.11/"

playground: ## Boot interactive VM with btrfs + Samba playground
	nix run .#playground
