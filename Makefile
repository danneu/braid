.PHONY: help test test-one test-verbose test-one-verbose playground

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*## "}; {printf "  %-15s %s\n", $$1, $$2}'

test: ## Run all NixOS VM tests
	nix flake check

test-one: ## Run a single test (e.g. make test-one t=hello-world)
	nix build .#checks.aarch64-darwin.$(t)

test-verbose: ## Run all NixOS VM tests (verbose, shows VM logs)
	nix flake check -L

test-one-verbose: ## Run a single test verbose (e.g. make test-one-verbose t=hello-world)
	nix build .#checks.aarch64-darwin.$(t) -L

playground: ## Boot interactive VM with btrfs + Samba (SMB on localhost:4450)
	nix run .#playground
