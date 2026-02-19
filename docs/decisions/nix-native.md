# Decision: NixOS-native

Status: Active

## Context

Braid is a NixOS module. It only targets NixOS — there is no portability goal for other distros, container runtimes, or generic Linux. This is a deliberate choice, not a limitation.

Because braid only runs on NixOS, it should be deeply integrated with NixOS conventions. There is no reason to introduce abstractions, indirections, or patterns that exist to support other platforms. Every design decision can assume the full NixOS ecosystem is available.

## Decision

Braid follows NixOS module conventions. It uses the same option types, module patterns, and idioms that nixpkgs modules use. When choosing between implementation approaches, prefer what an upstream nixpkgs module would do.

**When in doubt, look at how existing nixpkgs modules solve the same problem.** This is the tiebreaker for design disagreements — nixpkgs is the reference implementation.

### What this means in practice

- **Options** use standard `lib.mkOption` types (`lib.types.listOf`, `lib.types.attrsOf`, `lib.types.submodule`, etc.) — not custom validation or string parsing.
- **Activation** uses systemd units, not custom init scripts or cron jobs.
- **Dependencies** between services use standard systemd ordering (`after`, `wants`, `requires`), not polling or sleep loops.
- **Defaults** use `lib.mkDefault` / `lib.mkForce` priority, not conditional logic.
- **Config generation** uses NixOS's existing module merge semantics — not imperative file templating.
- **No portability shims.** If NixOS provides a mechanism (e.g., `boot.initrd.network`, `environment.etc`, `systemd.services`), use it directly rather than wrapping it in something "more portable."

### Tiebreaking

When two approaches both work:

1. Check how nixpkgs modules handle the same problem.
2. Pick the approach that is more idiomatic to NixOS, even if the alternative is simpler in isolation.
3. If nixpkgs doesn't have a precedent, prefer the approach that composes better with the rest of the NixOS module system.

## Alternatives considered

### Support other distros via abstraction layers

Rejected. Braid's value comes from deep NixOS integration — declarative disk config, reproducible builds, VM-tested infrastructure. Abstracting away NixOS to support Debian or Arch would compromise every design decision. The target user already runs NixOS.

### Use generic Linux tooling where possible

Rejected. "Generic" often means reimplementing what NixOS already provides (e.g., writing shell scripts instead of systemd units, managing config files instead of using NixOS options). This adds maintenance burden and loses NixOS guarantees like atomicity and rollback.

## See

- [Sane defaults](sane-defaults.md) — related: use `lib.mkDefault` instead of braid-specific wrappers
- [Config-first workflow](config-first-workflow.md) — related: NixOS rebuild as the entry point
