---
intent: "Record the NixOS-native decision and rationale. Read before changing related behavior or docs."
status: Active
---
# Decision: NixOS-native


## Context

Braid is a NixOS module. It only targets NixOS — no portability goal for other distros, container runtimes, or generic Linux. Every design decision can assume the full NixOS ecosystem is available.

## Decision

Braid follows NixOS module conventions — same option types, module patterns, and idioms as nixpkgs. **When in doubt, nixpkgs is the tiebreaker.**

### What this means in practice

- **Options** use standard `lib.mkOption` types (`lib.types.listOf`, `lib.types.attrsOf`, `lib.types.submodule`, etc.) — not custom validation or string parsing.
- **Activation** uses systemd units, not custom init scripts or cron jobs.
- **Dependencies** use systemd ordering (`after`, `wants`, `requires`), not polling or sleep loops.
- **Defaults** use `lib.mkDefault` / `lib.mkForce` priority, not conditional logic.
- **Config generation** uses NixOS module merge semantics — not imperative file templating.
- **No portability shims.** Use NixOS mechanisms (`boot.initrd.network`, `environment.etc`, `systemd.services`) directly.

### Tiebreaking

When two approaches both work:

1. Check how nixpkgs modules handle the same problem.
2. If no precedent, prefer whichever composes better with the NixOS module system.

## Alternatives considered

### Support other distros via abstraction layers

Rejected. Braid's value comes from deep NixOS integration — declarative disk config, reproducible builds, VM-tested infrastructure. The target user already runs NixOS.

### Use generic Linux tooling where possible

Rejected. "Generic" means reimplementing what NixOS already provides (shell scripts instead of systemd units, config files instead of NixOS options) — more maintenance, no atomicity or rollback.

## See

- [Sane defaults](005-sane-defaults.md) — use `lib.mkDefault` instead of braid-specific wrappers
- [Config-first workflow](002-config-first-workflow.md) — NixOS rebuild as the entry point
