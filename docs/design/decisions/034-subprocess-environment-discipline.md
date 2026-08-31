---
intent: Record the explicit environment allowlist for child processes spawned by the Rust CLI binary.
status: Active
---

# Decision: Subprocess environment discipline

> Principle: [Explicit subprocess environment](../principles.md#14-explicit-subprocess-environment)

## Scope

This policy governs child processes spawned by the braid Rust CLI binary through
`std::process::Command` in `cli/src/`.

It does not govern Nix-generated unit scripts or wrappers, including
`modules/braid/braid-wrapper.sh`, `writeShellScript` outputs, or systemd
`script =` blocks in `modules/braid/`. Those receive their environment from the
systemd unit definition and the wrapper.

## Context

braid resolves runtime tools by bare program name. ADR 010 makes PATH wrapping
the tool-resolution authority and rejects absolute-path resolution inside the
Rust binary. That means child processes need the wrapper-built PATH, but they do
not need the rest of the CLI process environment.

Before this decision, `RealRunner` pinned `LC_ALL=C` for parser stability but
inherited every other variable from the parent process. Two production spawn
sites, the ack beeper stop path and the sleep inhibitor path, bypassed
`RealRunner` and inherited the full environment without even the locale pin.

The secret-handling discipline in ADR 023 already says secret material reaches
subprocesses through stdin, never argv. The same defense-in-depth rule should
exist for environment inheritance: child processes should receive only the
variables braid deliberately supports.

## Decision

Every child process spawned by the Rust CLI gets the same explicit environment:

- inherited environment cleared with `env_clear()`;
- parent `PATH` forwarded when present;
- `LC_ALL=C` set unconditionally.

The only configuration point is `cli/src/cmd.rs#apply_child_env`. Every
production Rust CLI spawn site must call that helper before spawning:

- `cli/src/cmd.rs#RealRunner::spawn_exec` (the process creation behind both
  `RealRunner::run` and the deferred-wait `RealRunner::spawn`);
- `cli/src/cmd.rs#RealRunner::exec_with_stdin`;
- `cli/src/ack.rs#stop_beeper_program`;
- `cli/src/inhibit.rs#SleepInhibitor::acquire_with`.

If PATH is absent, braid leaves it absent. Supported deployments set PATH in the
wrapper or unit environment, so an unset PATH is outside the supported
configuration rather than a reason to synthesize a fallback search path.

This composes with the NixOS module boundary: the wrapper builds the PATH, the
systemd unit owns its unit-level environment, and the Rust CLI forwards only
PATH plus `LC_ALL=C` to its own children.

## Test strategy

Unit tests exercise each production spawn boundary by running a real stand-in
child and inspecting the environment that child actually receives. The
`RealRunner` and ack paths execute `env` directly and assert the complete key set
is exactly `PATH` and `LC_ALL=C`; the blocking, stdin-bearing, and deferred-wait
spawn paths each get their own such assertion. The inhibitor path uses the
existing READY handshake with a shell stand-in, then asserts PATH and `LC_ALL=C`
survive while a parent-only `CARGO_*` variable is absent despite the shell adding
its own local variables.

The VM suite remains the end-to-end compatibility check that the allowlist is
sufficient for cryptsetup, btrfs, mount, systemd, NUT, smartmontools, and
ethtool under the wrapped deployment environment.

## Alternatives considered

### Inherit the full environment

Rejected. It preserves status quo behavior but leaves parser locale behavior
duplicated and lets unrelated parent variables influence subprocesses.

### Resolve absolute tool paths inside Rust

Rejected. ADR 010 already rejects this because Nix PATH wrapping is the
resolution authority and `braid.packages.*` overrides intentionally flow through
PATH.

### Configure environment at each call site

Rejected. Per-call setup already drifted: `RealRunner` had duplicated locale
setup, while ack and inhibitor bypassed it. A single helper gives the invariant a
grep-friendly owner.

### Clear every variable, including PATH

Rejected. PATH is load-bearing because every runtime tool is invoked by bare
name. Dropping it would break the supported wrapper model.

## See

- [ADR 010: Toolchain pinning](010-toolchain-pinning.md)
- [ADR 023: Secret handling](023-secret-handling.md)
- [Principle 14: Explicit subprocess environment](../principles.md#14-explicit-subprocess-environment)
- `cli/src/cmd.rs#apply_child_env`
