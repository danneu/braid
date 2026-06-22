# Plan: centralize braid's subprocess-environment policy into an explicit allowlist

## Context

A security audit (`findings/2-secret-credential-handling.md`, Finding 5) observed
that `RealRunner` spawns cryptsetup/btrfs/mount/etc. with `.env("LC_ALL", "C")`
layered on top of braid's **full inherited environment** -- no `env_clear()`.
braid sets no secret env var, so there is no live leak; the finding is rated Low
defense-in-depth.

The literal proposal ("add `env_clear()` to both `RealRunner` helpers") is the
wrong altitude. Investigation surfaced the real shape of the work:

- **The policy is duplicated.** The `LC_ALL=C` setup and its identical
  explanatory comment are copy-pasted across `RealRunner::exec` and
  `RealRunner::exec_with_stdin`.
- **Two production spawn sites bypass `RealRunner` entirely** and inherit the
  full env with *no* policy at all: `ack.rs#stop_beeper` (`systemctl stop ...`)
  and `inhibit.rs` `SleepInhibitor::acquire` (`systemd-inhibit ...`). `stop_beeper`
  also surfaces `systemctl`'s stderr in a warning without `LC_ALL=C`, so that text
  is locale-dependent -- a consistency wart, not a bug (its warn/skip control flow
  keys only on the exit status).
- **The invariant is untested and undocumented.** ADR 023 documents the
  argv/stdin half of the secret discipline ("secrets reach subprocesses only via
  `run_with_stdin`, never argv"); the env half has no home. No test pins it.
- **`PATH` is load-bearing.** braid resolves *every* tool by bare name; ADR 010
  makes PATH-wrapping the tool-resolution authority and explicitly **rejected**
  absolute-path resolution. So `env_clear()` must forward `PATH` or every spawn
  breaks.

The outcome: one shared env-policy seam applied at all three production spawn
sites, an explicit `{PATH, LC_ALL=C}` allowlist (adversarially verified
sufficient -- see below), a behavioral boundary test at *each* production spawn
site (not just the helper), and the invariant written into the authority docs
(new ADR 033 + a new principle, scoped to the braid Rust binary's own children).
This turns a one-line nit into the env-side companion of ADR 023 while removing
duplication and making a locale-dependent warning deterministically English.

### Allowlist sufficiency (verified, do not re-litigate)

`{PATH, LC_ALL=C}` is sufficient for every tool braid spawns. Grepped the
vendored `reference/` trees for `getenv`/`secure_getenv`:

- **cryptsetup** -- zero `getenv` outside `tests/`. No locking-dir/debug/keyfile env.
- **util-linux mount/umount/mountpoint** -- no direct `getenv`; `LIBMOUNT_*` are
  override-only and ignored under privileged execution (`lib/env.c#safe_getenv`).
  `/etc/fstab` is read from the hardcoded path.
- **NUT clients** -- read only `NUT_DEBUG_LEVEL`; host:port comes from argv
  (default `localhost:3493`). `NUT_CONFPATH`/`NUT_STATEPATH` are daemon-side.
- **smartctl** -- only `TZ`; self-defaults to `GMT` when unset (more deterministic).
- **btrfs-progs** -- only `*INJECT` test hooks.
- **systemctl / systemd-inhibit** -- `sd_bus_default_system()` falls back to the
  well-known socket `/run/dbus/system_bus_socket` when `DBUS_SYSTEM_BUS_ADDRESS`
  is unset (via `secure_getenv`, so a privileged braid ignores it anyway).
  `XDG_RUNTIME_DIR` is user-bus only. `systemd-inhibit` exec'ing `sh`/`sleep`
  needs only `PATH`.
- **Locale** -- `LC_ALL` overrides `LANG`/`LANGUAGE`/`LC_MESSAGES` per POSIX, so
  dropping them cannot change behavior.

## Implementation

### 1. The seam: `apply_child_env` (`cli/src/cmd.rs`)

Add one `pub(crate)` free function -- the sole place child env is configured:

```rust
pub(crate) fn apply_child_env(cmd: &mut std::process::Command) {
    cmd.env_clear();
    // braid resolves every tool by bare name, so PATH is load-bearing
    // (ADR 010). Forward the parent's PATH when present: in every supported
    // deployment the braid wrapper / systemd unit sets it (ADR 010), so the
    // pinned tool path is always there to forward. If PATH is somehow unset
    // we leave it unset -- that is an unsupported configuration, and how the
    // OS then resolves a bare program name (a libc default search path, or a
    // spawn failure) is OS-defined and not something braid relies on.
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("LC_ALL", "C");
}
```

Signature is `&mut Command` (not a `Command`-returning builder) so it composes
with every call site without constraining how each builds args/stdio -- in
particular `SleepInhibitor`'s `.process_group(0)`/`.stdout(piped)` and
`exec_with_stdin`'s three `Stdio::piped()` calls.

Doc comment must capture *why* (per `docs/dev/doc-comments.md`): PATH is the
tool-resolution authority (ADR 010); `LC_ALL=C` pins POSIX/English output for the
stderr/JSON parsers; `env_clear()` drops inherited-variable surprises and is the
env-side companion to ADR 023; this is the ONLY child-env configuration point.

### 2. Route all three production spawn sites through it

- **`cli/src/cmd.rs`** `RealRunner::exec` and `RealRunner::exec_with_stdin`:
  replace the inline `.env("LC_ALL", "C")` (and the duplicated locale comment)
  with `apply_child_env(&mut cmd)` on the builder before `.output()` / `.spawn()`.
- **`cli/src/ack.rs`** `stop_beeper`: build the `Command`, call
  `apply_child_env(&mut cmd)`, then `.output()`. Bonus: `LC_ALL=C` makes the
  `systemctl` stderr that `format_systemctl_stop_failure` surfaces deterministically
  English (consistency, not a bug fix -- the warn/skip decision keys only on the
  exit status).
  Extract the spawn into a private
  `stop_beeper_program(program: &str, args: &[&str]) -> io::Result<Output>`
  with `stop_beeper()` delegating `("systemctl", &["stop", "braid-alert.service"])`.
  Injecting *both* program and args is what lets the boundary test run the real
  `env` binary with empty args and read a clean two-line env dump from `.output()`
  stdout (Section 3) -- no shell wrapper, so nothing pollutes the recorded
  environment.
- **`cli/src/inhibit.rs`** `SleepInhibitor::acquire`: call `apply_child_env`
  on the `systemd-inhibit` builder, keeping `.stdout(piped).process_group(0)`.
  The `READY` handshake is locale-independent; verified safe. Add a
  `pub(crate) fn acquire_with(program: &str, args: &[&str])` seam (program +
  args injectable, mirroring `stop_beeper_program`); `acquire(why)` builds the
  real argv (`--what=sleep --who=braid --mode=block --why={why} sh -c
  'printf READY; exec sleep infinity'`) and delegates to
  `acquire_with("systemd-inhibit", &argv)`. The boundary test instead calls
  `acquire_with("sh", &["-c", "printf READY; exec env"])` -- the same
  `printf READY; exec ...` handshake idiom production already relies on, with
  `env` in place of `sleep`. `exec env` dumps the child env and exits (closing
  fd 1 on its own), so the test needs no compiled recorder, no manual fd-1
  close, and no blocking child. The test reads the dump from the guard's child
  stdout; a same-module `#[cfg(test)]` test can touch the private `child` field
  directly, so no extra accessor is needed.
- **Optional parity** (`cli/src/mountpoint_guard.rs` bind-mount test spawns):
  test-only, not production. Apply the helper there too only if cheap, to avoid
  a future reader wondering why the test path differs.

These injectable seams are the minimum needed to make the two bypass sites
boundary-testable without mutating the test process's own `PATH` (so the tests
stay parallel-safe). Production behavior is unchanged: both entry points still
spawn the real program by bare name with the real arguments.

### 3. Behavioral boundary tests -- one per production spawn site

A test of `apply_child_env` alone would pass even if a spawn site stopped
calling it. So the coverage must assert the child environment produced *at each
production spawn path*, observing the env a real child actually receives and
failing if the policy regressed. Sites 1-3 spawn the real `env` coreutil
directly (no shell) and assert the key set is *exactly* `{PATH, LC_ALL=C}` (PATH
value not pinned -- just present; `LC_ALL` exactly `C`; no third variable).
Site 4 must route through the `READY` handshake, so it uses a `sh` stand-in and
asserts a subset/negative property instead (below); exactness there would add
nothing, since sites 1-3 already pin the exact key set against the *same*
`apply_child_env` seam. Shared assertion helper for sites 1-3: parse the env
dump and assert the key set is exactly `{PATH, LC_ALL}` with `LC_ALL=C`.

Four tests, all parallel-safe (no `std::env::set_var`, unsafe in edition 2024):

1. **`RealRunner::exec`** (`cli/src/cmd.rs`) -- call the private `exec` directly
   with `CmdArgs { program: "env".into(), args: vec![] }`; assert the returned
   `stdout` env dump is exactly `{PATH, LC_ALL=C}`. (`exec` is the real env
   boundary; the public `run` only reaches fixed-program `CmdRequest`s, so it
   can't run `env`. Same-module tests may call the private fn.)
2. **`RealRunner::exec_with_stdin`** -- same, with empty stdin (`b""`, so the
   `write_all` cannot `EPIPE` if `env` exits before reading); assert the dump.
3. **`stop_beeper`** (`cli/src/ack.rs`) -- call `stop_beeper_program("env", &[])`
   (the real `env` coreutil, resolved via the forwarded PATH, no args so it just
   prints its environment); assert the `.output()` stdout env dump is exactly
   `{PATH, LC_ALL=C}`. No shell wrapper and no record file -- `env`'s stdout *is*
   the dump, captured directly by `.output()`, so nothing pollutes it.
4. **`SleepInhibitor::acquire`** (`cli/src/inhibit.rs`) -- call
   `acquire_with("sh", &["-c", "printf READY; exec env"])`. The `printf READY`
   satisfies the handshake (`acquire_with` consumes the 5-byte `READY`); the
   shell then `exec`s `env`, which dumps the child environment to stdout and
   exits (closing fd 1, so the read terminates at EOF -- no blocking child).
   After `acquire_with` returns the guard, the test reads the rest of the child
   stdout (same-module access to the private `child` field) and asserts a
   subset/negative property robust to the shell's own additions: `PATH` present,
   `LC_ALL=C` present, and a Cargo-injected sentinel that no shell synthesizes
   (e.g. `CARGO_PKG_NAME`) **absent**. A `sh` stand-in adds `PWD`/`SHLVL`/`_`
   (confirmed on this host) but never `CARGO_*`, so the sentinel's absence proves
   `env_clear()` ran while the additions are harmless. To keep the negative check
   from passing vacuously, first assert the sentinel *is* present in the test's
   own `std::env::vars()`. Then drop the guard so its `Drop` reaps the process
   group. This also exercises the `READY` handshake against the hardened env.

The test process has a populated env (`CARGO_*`, `HOME`, ...); at sites 1-3 the
child's complete absence of those vars proves `env_clear()` ran, a present
`PATH=` proves forwarding, and an exact `LC_ALL=C` proves the locale pin; site 4
proves the same three facts via its subset/negative check. None mutate the test
process's `PATH`, so the tests stay parallel-safe. The record channel survives
`env_clear()` because every test reads the child's *stdout* (captured by the
runner / `.output()`, or the piped guard stdout at site 4) -- never an
environment variable pointing at a record file, which `env_clear()` would wipe
before the child could read it. These are the first tests in the repo to inspect
spawn behavior this way; existing `RealRunner` tests only exercise the
fail-closed gates before any spawn.

Each carries the `// Intent / Why it exists / Scenario` preamble (they pin an
invariant + guard a silent regression). Cross-tool adequacy of the allowlist is
covered for free by the VM suite (see Verification) -- no new tests for that axis.

### 4. Authority docs

- **New ADR `docs/design/decisions/033-subprocess-environment-discipline.md`**
  (`status: Active`). **Scope (state it explicitly):** the policy governs child
  processes spawned by the **braid Rust CLI binary** (`std::process::Command` in
  `cli/src/`). It does **not** govern Nix-generated unit scripts and wrappers
  (`modules/braid/braid-wrapper.sh`, the `writeShellScript`/systemd `script =`
  blocks in `monitor.nix`, `storage.nix`, `fan-control.nix`, `ups.nix`), which
  receive their environment from the systemd unit definition and the wrapper.
  Note the composition: the wrapper *builds* the tool `PATH` (ADR 010); systemd
  units set their own `Environment=`/`path=`; `apply_child_env` *forwards* exactly
  that inherited `PATH` (plus `LC_ALL=C`) to the Rust binary's own children and
  drops everything else. Context: bare-name tool resolution + full-env
  inheritance + the two bypass sites. Decision: every Rust-CLI spawn gets
  `env_clear()` + explicit `{PATH forwarded, LC_ALL=C}` via the single
  `apply_child_env` seam, at all production spawn sites. Rejected alternatives:
  inherit full env (status quo -- non-deterministic, no defense-in-depth);
  absolute-path tool resolution (already rejected in ADR 010's "Alternatives
  considered", and additionally incompatible with ADR 010's `braid.packages.*`
  PATH-override hatch); per-call env setup (duplication/drift -- the bug this
  fixes). A `## See` section cross-linking
  ADR 023, ADR 010, and the new principle. If the Nix-generated scripts are ever
  brought under an equivalent allowlist, that is a separate audit/decision, not
  assumed by this ADR.
- **New principle in `docs/design/principles.md`** (Principle 14, "Explicit
  subprocess environment"): *Every child process the braid Rust CLI binary spawns
  runs with an explicit environment allowlist (`PATH` forwarded + `LC_ALL=C`);
  the CLI never passes its inherited environment through to a child. (Nix-generated
  unit scripts and wrappers are out of scope -- their environment is set by the
  systemd unit and the wrapper.)*
  `[Why →](decisions/033-subprocess-environment-discipline.md)`.
- **`docs/design/decisions/023-secret-handling.md`**: add ADR 033 to the
  `> Related:` block as the env-side companion to the argv/stdin rule.
- **`docs/SUMMARY.md`**: add the new ADR to the decisions block --
  `- [033: Subprocess environment discipline](design/decisions/033-subprocess-environment-discipline.md)`.
  mdbook will not render a page absent from `SUMMARY.md`, so without this the
  inbound links from Principle 14 and ADR 023 resolve to an unbuilt page and
  `mdbook-linkcheck2` (the `just docs-build` gate) fails.
- **Optional** `AGENTS.md` "Read before you touch": add a row mapping
  subprocess-spawn / env changes to ADR 033.

> `## See` / `> Related:` edits are governed by `docs/dev/doc-citations.md` and
> enforced by `scripts/docs/check-see-paths.py` -- match the required path form.

## Critical files

- `cli/src/cmd.rs` -- new `apply_child_env`; rewrite `RealRunner::exec` /
  `exec_with_stdin`; boundary tests for both (helper near
  `output_to_raw`/`RealRunner`).
- `cli/src/ack.rs` -- `stop_beeper` routes through the helper; add the
  `stop_beeper_program(program, args)` seam (program + args both injectable) +
  boundary test that drives it with the real `env` binary.
- `cli/src/inhibit.rs` -- `SleepInhibitor::acquire` routes through the helper;
  add the `acquire_with(program, args)` seam (program + args injectable). The
  boundary test substitutes `acquire_with("sh", &["-c", "printf READY; exec
  env"])` and reads the dump via same-module access to the private `child`
  field -- no compiled recorder, no extra accessor, no `current_exe()` discovery.
- `docs/design/decisions/033-subprocess-environment-discipline.md` -- new ADR.
- `docs/design/principles.md` -- new Principle 14.
- `docs/SUMMARY.md` -- TOC entry for ADR 033 (decisions block).
- `docs/design/decisions/023-secret-handling.md` -- `Related` cross-link.

## Reuse / existing structure

- `apply_child_env` replaces two duplicated `.env("LC_ALL", "C")` blocks and
  becomes the third site's first env policy -- net unification, not new surface.
- Mirror the ADR header (`intent:`/`status:` frontmatter) and `## See` format
  from ADR 023 / ADR 010; mirror the `## NN. Title ... [Why →]` form from the
  existing principles.

## Verification

1. `just test-rust` -- the four per-site env boundary tests + existing CLI tests.
   No recipe change: all four are plain `#[cfg(test)]` tests under `--lib`, with
   no extra bin target or build feature.
2. `just test-vm` -- proves the allowlist breaks no real tool end-to-end:
   - replace path (exercises `SleepInhibitor` / `systemd-inhibit` + logind),
   - unlock / add (cryptsetup + btrfs + mount via `RealRunner`),
   - the alert/ack path (`stop_beeper` -> `systemctl`),
   - `tool-versions`, UPS status, smartctl doctor, auto-suspend (ethtool).
3. `just docs-build` -- `mdbook-linkcheck2` validates the new ADR/principle
   cross-links; `check-see-paths.py` validates the `See`/`Related` edits.
4. `scripts/docs/check-output-ascii.py` -- unchanged user-facing strings; keep
   the new doc comment ASCII per house style.
5. Manual smoke: `sudo braid status` still resolves tools (PATH forwarded).

## Cleanup note

A Plan subagent left a stray verdict file at
`plans/wip/plan-the-ideal-pivot-vast-gray-agent-add0038e817c03504.md` during
research. Delete it before committing; its content is folded into the
"Allowlist sufficiency" section above.

## Implementation notes

- ADR 033 already exists as `docs/design/decisions/033-systemd-unit-hardening.md`, so this implementation uses ADR 034 for subprocess environment discipline and updates links accordingly.
- `stop_beeper` already stopped both `braid-alert.service` and `braid-alert-advisory.service`; the injectable spawn seam sits under `stop_unit` so both production calls keep their existing behavior.
