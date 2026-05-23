# AGENTS.md

## Project: braid

Github: https://github.com/danneu/braid (private, use `gh` cli tool for
access)

braid is a Rust CLI tool + NixOS module for managing a NixOS-based NAS of full-disk-encrypted drives (luks) in a btrfs raid1 array.

braid wraps luks + btrfs to provide higher level UX to make things easier, more accessible, and less error-prone for people just trying to manage their NAS without fiddling or reading manpages to do everything.

## Example

```
Physical drives:
  /dev/sda → LUKS ─┐
  /dev/sdb → LUKS ─┼─ single btrfs RAID1 → /mnt/storage
  /dev/sdc → LUKS ─┘

Unlock:
  NAS powers on → boots to login (pool offline)
  → ssh user@nas → sudo braid unlock
  → LUKS drives open → btrfs assembles → pool online
```

## The Stack

- **NixOS** — declarative, reproducible system configuration
- **LUKS** — passphrase-based full disk encryption (keys never stored on disk)
- **btrfs RAID1** — checksumming filesystem with automatic self-healing from redundant copies; dynamic add/remove drives

## Layout

- `cli/src/` — Rust CLI (clap commands, TUI in `tui/`)
- `modules/braid/` — NixOS module (options, systemd units, storage config)
- `tests/` — NixOS VM tests (`.py` scripts, `module/` NixOS configs, `hw/` hardware canary tests)
- `docs/` — unified mdBook docs (single TOC at `docs/SUMMARY.md`, landing at `docs/index.md`)
  - `guides/`, `commands/` — end-user material (formerly under `manual/`)
  - `design/principles.md`, `design/decisions/` — architecture authority
  - `internals/` — implementation notes (luks-unlock, tool behavior, btrfs deep-dives)
  - `dev/` — contributor docs (development workflow, testing, TUI snapshots)
- `scripts/` — helper scripts (fetch references, destroy pool)
- `reference/` — upstream source checkouts for reading, not shipped. See [Reference source](#reference-source) below for the full inventory. Refresh with `just fetch-references`.

## Systemd Lifecycle

Systemd lifecycle design: [`docs/design/decisions/018-systemd-lifecycle.md`](docs/design/decisions/018-systemd-lifecycle.md). Read before modifying units, the wrapper, or writing systemd-related tests.

## No backwards compatibility

braid is unreleased software. Never add migration paths, compatibility shims, or legacy support. If a format or interface changes, change it everywhere — old versions are not a concern.

## Architecture Authority

Design principles and invariants live in [`docs/design/principles.md`](docs/design/principles.md). Detailed rationale, rejected alternatives, and historical context live in [`docs/design/decisions/`](docs/design/decisions/).

Any change to behavior or invariants must update those docs. Code that contradicts a principle is wrong — fix the code or update the principle with rationale.

Decision docs must include an explicit status: `Draft`, `Active`, `Superseded`, or `Deprecated`.

Before modifying dry-run, preview, or mutating command planning/execution, read [`docs/design/decisions/022-dry-run-preview-model.md`](docs/design/decisions/022-dry-run-preview-model.md).

## User Guide

End-user material lives in two places: [`README.md`](README.md) is the cookbook-style overview
(brief, copy-paste examples), and `docs/guides/` + `docs/commands/` is the mdBook reference
(formerly `manual/`). Keep both in sync when adding features or changing behavior. Style for
README.md: brief, cookbook-like — short descriptions with copy-paste examples. Not reference
material.

## Documentation

[`docs/SUMMARY.md`](docs/SUMMARY.md) is the TOC for the unified docs tree (end-user guides,
commands, design principles, ADRs, internals, contributor docs). [`docs/index.md`](docs/index.md)
is the landing page. Check `SUMMARY.md` before searching the codebase for context. All cross-links
inside `docs/` are validated by `mdbook-linkcheck` during `mdbook build docs` (configured in
`docs/book.toml` per Decision 5) -- a broken cross-link fails CI.

### Reference source

Before searching the web for tool behavior, consult local resources first. `reference/` contains shallow clones of upstream repos at the versions pinned in nixpkgs, plus Rust crate sources pinned in `Cargo.lock`. Refresh with `just fetch-references`.

**When to look:** Any time you're implementing, modifying, or debugging code that interacts with these tools — especially parsers. Read the relevant source before making assumptions about output format or behavior.

- **btrfs-progs** — [kdave/btrfs-progs](https://github.com/kdave/btrfs-progs)
  - **Source:** [`reference/btrfs-progs/cmds/`](reference/btrfs-progs/cmds/) — one file per subcommand (e.g. `cmds/scrub.c`). Parser output formats, exit codes.
  - **Docs:** [`reference/btrfs-progs/Documentation/`](reference/btrfs-progs/Documentation/) — RST. See [btrfs docs](#btrfs-docs) below for the topic table.
- **systemd** — [systemd/systemd](https://github.com/systemd/systemd)
  - **Source:** [`reference/systemd/src/`](reference/systemd/src/) — unit lifecycle internals, `systemd-ask-password`, mount/automount.
  - **Docs:** [`reference/systemd/docs/`](reference/systemd/docs/) — markdown design docs (`BOOT.md`, `INHIBITOR_LOCKS.md`, `MOUNT_REQUIREMENTS.md`, `CREDENTIALS.md`, `PASSWORD_AGENTS.md`, etc.). [`reference/systemd/man/`](reference/systemd/man/) — XML man-page sources for unit/option reference (`systemd.service.xml`, `systemd.mount.xml`, …).
- **autosuspend** — [languitar/autosuspend](https://github.com/languitar/autosuspend)
  - **Source:** [`reference/autosuspend/src/`](reference/autosuspend/src/) — check classes, config schema, wakeup scheduling.
  - **Docs:** [`reference/autosuspend/doc/source/`](reference/autosuspend/doc/source/) — RST (`available_checks.rst`, `available_wakeups.rst`, `configuration_file.rst`, `systemd_integration.rst`).
- **cryptsetup** — [cryptsetup/cryptsetup](https://gitlab.com/cryptsetup/cryptsetup)
  - **Source:** [`reference/cryptsetup/src/`](reference/cryptsetup/src/) (CLI), [`reference/cryptsetup/lib/`](reference/cryptsetup/lib/) (libcryptsetup) — `luksDump` output, LUKS2 header structure, keyslot operations.
  - **Docs:** [`reference/cryptsetup/man/`](reference/cryptsetup/man/) — `*.8.adoc` man pages (`cryptsetup-luksDump.8.adoc`, `cryptsetup-open.8.adoc`, …). [`reference/cryptsetup/docs/`](reference/cryptsetup/docs/) — design notes including `LUKS2-locking.txt` and `on-disk-format-luks2.pdf`.
- **util-linux** — [util-linux/util-linux](https://github.com/util-linux/util-linux)
  - **Source:** [`reference/util-linux/misc-utils/`](reference/util-linux/misc-utils/) (`lsblk`, `blkid`), [`reference/util-linux/sys-utils/`](reference/util-linux/sys-utils/) (`mount`, `umount`), [`reference/util-linux/libmount/`](reference/util-linux/libmount/), [`reference/util-linux/libblkid/`](reference/util-linux/libblkid/) — `lsblk` JSON schema, `blkid` output, mount/unmount behavior.
  - **Docs:** Man pages live next to source as `*.8.adoc` (e.g. `misc-utils/lsblk.8.adoc`, `sys-utils/mount.8.adoc`). [`reference/util-linux/Documentation/`](reference/util-linux/Documentation/) is project meta (build/test/contribution notes), not user reference.
- **smartmontools** — [smartmontools/smartmontools](https://github.com/smartmontools/smartmontools)
  - **Source:** [`reference/smartmontools/smartmontools/`](reference/smartmontools/smartmontools/) — flat layout. `smartctl` output format, SMART attribute definitions, exit codes.
  - **Docs:** No separate docs dir. Man-page sources are inline alongside the code: `smartctl.8.in`, `smartd.8.in`, `smartd.conf.5.in`.
- **hddfancontrol** — [desbma/hddfancontrol](https://github.com/desbma/hddfancontrol)
  - **Source:** [`reference/hddfancontrol/src/`](reference/hddfancontrol/src/) — Rust daemon. `device/` (drivetemp, hddtemp, smartctl probing), `probe/` (pwm-test ramp logic), `fan.rs` (PWM control), `pwm.rs` (sysfs PWM I/O), `cl.rs` (CLI args).
  - **Docs:** No separate docs dir. [`reference/hddfancontrol/README.md`](reference/hddfancontrol/README.md) and [`reference/hddfancontrol/systemd/hddfancontrol.service`](reference/hddfancontrol/systemd/hddfancontrol.service) — the upstream unit we intentionally don't use (see `modules/braid/fan-control.nix`).
- **nut** — [networkupstools/nut](https://github.com/networkupstools/nut)
  - **Source:** [`reference/nut/clients/`](reference/nut/clients/) (`upsmon.c` -- shutdown-on-LB daemon, `upsc.c` -- status query, `upscmd.c`, `upssched.c`, `upsrw.c`), [`reference/nut/server/`](reference/nut/server/) (`upsd.c` and net protocol handlers), [`reference/nut/drivers/`](reference/nut/drivers/) (`usbhid-ups.c` and per-vendor `*-hid.c` for the USB HID path v1 targets).
  - **Config schema:** [`reference/nut/conf/`](reference/nut/conf/) — sample files (`nut.conf.sample`, `ups.conf.sample`, `upsd.conf.sample`, `upsd.users.sample`, `upsmon.conf.sample.in`, `upssched.conf.sample.in`). Authoritative for fields braid generates into `/etc/nut/*`.
  - **Docs:** [`reference/nut/docs/man/`](reference/nut/docs/man/) — `*.txt` asciidoc man pages for daemons, drivers, and config files. [`reference/nut/docs/`](reference/nut/docs/) — design notes (`design.txt`, `net-protocol.txt`, `developer-guide.txt`, `new-drivers.txt`, `FAQ.txt`).
- **linux** — [torvalds/linux](https://github.com/torvalds/linux)
  - **Source:** [`reference/linux/`](reference/linux/) — kernel source at the exact version pinned in nixpkgs. Look in `fs/btrfs/` for btrfs-specific I/O scheduling, raid handling, and read balancing logic. `drivers/md/` for raid and block layer behavior.
  - **Use for:** Understanding kernel-level I/O behavior, raid1 read balancing, mount semantics, block device management.
- **coreutils** — [coreutils/coreutils](https://github.com/coreutils/coreutils) (GitHub mirror of GNU Coreutils)
  - **Source:** [`reference/coreutils/src/`](reference/coreutils/src/) — one C file per utility (e.g. `src/timeout.c`, `src/realpath.c`, `src/stat.c`, `src/chmod.c`, `src/chown.c`, `src/head.c`, `src/base64.c`). Read these to confirm what each helper actually guarantees -- e.g. `timeout(1)` exit-code semantics and signal forwarding live in `src/timeout.c`, not in any manpage.
  - **Docs:** [`reference/coreutils/doc/coreutils.texi`](reference/coreutils/doc/coreutils.texi) — the canonical reference manual (per-utility sections inside one big Texinfo file). Per-utility manpage stubs live in [`reference/coreutils/man/`](reference/coreutils/man/) as `*.x` (e.g. `man/timeout.x`); these are short prologues that get merged with `--help` output by `help2man` at build time, so the full prose is in `coreutils.texi`.
  - **Use for:** Any time braid code or a plan reasons about a Coreutils helper's behavior beyond the obvious — exit codes, signal handling, race windows, `--help` text, edge cases. Especially `timeout(1)`: `timeout` cannot bound an uninterruptible kernel wait, and the proof is in `src/timeout.c`'s use of `kill()` against a userspace child.
- **nix (Rust crate)** -- [nix-rust/nix](https://github.com/nix-rust/nix)
  - **Source:** [`reference/nix-crate/src/`](reference/nix-crate/src/) -- Rust crate at the version pinned in `Cargo.lock`, not `flake.lock`. `unistd.rs` (User/Group/chown/exec helpers, fd ownership types), `fcntl.rs` (`open`, `flock`, `OFlag`), `errno.rs` (`Errno`), `sys/stat.rs` (`Mode`), `sys/signal.rs` (sigaction, signal handlers), `sys/termios.rs` (termios constants, terminal flags).
  - **Docs:** No separate docs dir -- rustdoc is inline as `///` doc comments on each item. [`reference/nix-crate/Cargo.toml`](reference/nix-crate/Cargo.toml) declares the feature gates (braid currently enables `fs`, `user`, `term`, and `signal`); consult it before reaching for a `nix` API to confirm which feature it lives under.
  - **Use for:** Touching any `nix::` API, checking feature gates, understanding fd-ownership types, signal-safe helpers, or termios constants. Refresh after any change to the `nix` line in `cli/Cargo.toml` or any `cargo update`-driven bump in `Cargo.lock`.

### btrfs docs

- **Docs:** [`reference/btrfs-progs/Documentation/`](reference/btrfs-progs/Documentation/) — RST docs from btrfs-progs. Start with `index.rst` for a full table of contents, or use the topic table below for common lookups. Glob by keyword for anything not in the table. `ch-*` fragments are inlined by `just fetch-references`.

| Topic                             | File(s)                                     |
| --------------------------------- | ------------------------------------------- |
| Adding/removing devices           | `Volume-management.rst`, `btrfs-device.rst` |
| Device replacement                | `btrfs-replace.rst`                         |
| Rebalancing                       | `Balance.rst`, `btrfs-balance.rst`          |
| RAID profiles (RAID1 etc.)        | `mkfs.btrfs.rst` (search for "profiles")    |
| Mount options                     | `btrfs-man5.rst`                            |
| Scrub / self-healing              | `Scrub.rst`                                 |
| Filesystem limits & storage model | `btrfs-man5.rst`                            |
| Administration overview           | `Administration.rst`                        |

## Git Commits

The first line of a commit message must not be capitalized (e.g. `fix the foo bug`, not `Fix the foo bug`).

## CLI Output Style

Use `--` (double hyphen), not `—` (em-dash), in all user-facing CLI output -- error messages, help text, TUI strings, shell `echo` lines. Em-dashes render poorly over SSH and in non-UTF-8 locales.

Example: `pool is not mounted -- nothing to acknowledge`

For the LUKS header backup workflow and the messaging invariant for `doctor`/`status`/`unlock` recovery hints, see [`docs/internals/luks-unlock.md`](docs/internals/luks-unlock.md#header-backup-workflow-and-messaging).

## Doc Comments

When adding a new top-level function, type, module, trait, or
`pub`/`pub(crate)` item in the Rust CLI, add a `///` doc comment justifying
why it exists at that boundary. Capture intent, invariant, ownership, or
call-site coupling -- not the signature.

Prefer one to three lines. If removing the comment would not lose any
information a reader could not recover from the code, do not write it.

Skip:

- Trait impls whose purpose is the trait (`Display`, `Debug`, `From`,
  `Default`, ...)
- Enum variants already covered by an enum-level doc
- `#[cfg(test)]` items and test fixtures

Good:

- "Shared mapper ownership classifier so planner and executor use the
  same LUKS UUID invariant."
- "Separate from `MountState` because we observe LUKS state without
  holding the pool lock."

Bad:

- "Returns mapper ownership." (restates signature)
- "Helper used by the planner." (vague)
- "Caller must ensure path is canonical." (fabricated invariant nothing
  enforces)

Rust CLI only. Nix module options use NixOS option `description` fields;
shell scripts and Python tests follow their own conventions (see Test
Conventions).

## Commands

- `just test-vm` — Run NixOS VM tests (excludes repro tests).
- `just test-vm -v` — Run tests with full VM logs.
- `just test-vm test1 test2` — Run one or more specific checks.
- `just test-vm test1 -v` — Run specific checks with verbose output.
- `just test-repro` — Run repro tests only (same flags as `test-vm`).
- `just test-all` — Run all tests including repro.
- `just test-parsers` — Run parser compatibility canary (CLI parsers against live VM tool output).
- `just test-rust` — Run Rust unit tests (`cargo test`). The CLI crate's package name is `braid-cli` (not `braid`); prefer `just test-rust` over `cargo test -p <name>` so you don't have to remember.
- `just test-all-unstable` — Run all VM tests (including repro) against nixos-unstable.
- `just capture-all-fixtures` — Capture all stable fixtures (base + progress).
- `just capture-all-fixtures-unstable` — Capture all unstable fixtures (base + progress).
- `just test-rust-unstable` — Run golden parser tests against unstable fixtures.

`just test-vm` and `just test-repro` accept `--unstable` to run VM tests against nixos-unstable (e.g. `just test-vm hello-world --unstable`). For fixture capture and Rust golden tests, use the dedicated `-unstable` recipes above.

**Test verbosity:** Run tests without `-v` by default. Only add `-v` to a specific failing test when the non-verbose output doesn't explain the failure. Never run `just test-vm -v` (all tests verbose) — it produces too much output to be useful.

**Test scope:** Default to focused runs (`just test-vm test1 test2`) -- the full suite takes 20-30 minutes. Only run the unscoped `just test-vm` for changes with broad blast radius (systemd lifecycle, pool lock, mount/unmount, module-wide refactors) or right before handing work back to the user on a substantial change. For small, localized changes, run only the tests that exercise the touched code path.

## Test Conventions

Every individual test starts with a `//` line-comment preamble with three labeled sections:

1. **Intent** — what behavior this test verifies (or tries to verify)
2. **Why it exists** — what risk/regression this protects against
3. **Scenario** — the real-world user/system story this models, especially the concrete bug or incident that inspired the test

For the literal preamble form, the flake.nix `checks` registration rule for new VM tests, and NixOS VM test framework gotchas, see [`docs/dev/testing.md`](docs/dev/testing.md).

## Development Approach: TDD with NixOS VM Tests

Write failing tests first, confirm they fail for the expected reasons, then implement the NixOS config to make them pass.

- **Test framework:** NixOS VM tests (`nixos/lib/testing-python.nix`)
- **Runs on macOS:** Requires `nix.linux-builder.enable = true` in nix-darwin. Tests are `checks.aarch64-darwin`.
- **Virtual disks:** `virtualisation.emptyDiskImages` creates throwaway virtual drives.

## Parser Compatibility

braid parses output from btrfs-progs, cryptsetup, util-linux, smartmontools, and NUT. These parsers can break when tool versions change. Two validation lanes exist:

### Stable lane (pinned contract)

- `just test-parsers` — CLI parser canary. Exercises CLI-reachable parsers against live tool output in VMs (including `braid-status-ups`, the NUT canary).
- `just test-rust` — validates golden fixtures for the full parser set, including `parse_upsc`. Fixture-backed coverage stays current only after running `just capture-all-fixtures` when parser-critical tool versions change (e.g. nixpkgs bump).
- Fixture refresh is a separate obligation: `just test-parsers` passing does not guarantee TUI-only parsers (`parse_lsblk_json`, `parse_cryptsetup_luks_dump`, `parse_smartctl_health`) or unused parsers (`parse_btrfs_scrub_status_per_device`) are compatible with the current toolchain.
- Fixtures in `cli/tests/fixtures/nixos-25.11/` are committed and authoritative. NUT fixtures live in `cli/tests/fixtures/nixos-25.11/upsc/` (and the unstable mirror); they are produced by `just capture-ups-fixtures`, which boots a dedicated NUT VM with per-state `dummy-ups` drivers (see `tests/capture-ups-fixtures.nix`).

Parser-critical tool versions are the pinned `nixpkgs` versions of `btrfs-progs`, `cryptsetup`, `util-linux`, `nut`, and `smartmontools`. Treat any change to the `nixpkgs` node in `flake.lock`, any `flake.nix` change that alters the `nixpkgs` input, or any change to `braid.packages.{btrfsProgs,cryptsetup,utilLinux,nut,smartmontools}` as a required fixture-refresh event.

When parser-critical tool versions change, run:

1. `just capture-all-fixtures`
2. `just test-rust`
3. `just test-parsers`

### Unstable lane (tracked forecast)

Early-warning lane for upstream parser/output drift. Unstable failures signal upcoming changes, not a contract violation. Fixtures in `cli/tests/fixtures/nixos-unstable/` are committed so upstream output changes are visible in git history, but they are non-authoritative.

- `just test-all-unstable` — VM tests against nixos-unstable. Covers CLI-reachable parsers against live tool output but does not cover the full parser surface (TUI-only parsers, unused parsers).
- `just capture-all-fixtures-unstable` + `just test-rust-unstable` — covers the full parser surface (btrfs/cryptsetup/util-linux/smartctl/NUT) against unstable tool output via golden fixtures. Missing fixtures fail (not skip).

Full unstable canary workflow:

1. `just test-all-unstable`
2. `just capture-all-fixtures-unstable`
3. `just test-rust-unstable`

---

## Plan Files

Agents write plan files (design proposals, implementation plans, scratch
analysis) to `./plans/wip/`, named `YYYY-MM-DD-{slug}.md` where `{slug}`
is a short kebab-case identifier of the topic (e.g.
`2026-05-23-test-perf.md`). Use today's date from the system context, not
a guess.

The `plans/wip/` directory is gitignored deliberately -- stale plans
surfacing in project greps was causing agents to treat them as
authoritative. Don't flag plan files as "untracked" or suggest committing
them; that's the intended state.

## Plan Review Protocol

When reviewing a plan:

- List findings ordered by severity.
- For each finding: state issue, impact, and include one recommended fix.
- Prescriptions must be singular: do not present multiple options in the report.
- After listing findings, assess the overall plan viability.
- If you think there's an even better + simpler + more robust solution, tell the
  user so that they can consider pivoting to a new, better plan.

Decision rule:

- For each finding, consider the best resolutions and their trade-offs internally, then choose the best solution.
- If multiple open-ended solutions exist, brainstorm with the user until one
  solution is agreed.
- After alignment, report only that agreed solution.

Example (single finding):

- High: Plan makes `braid status` mutate `pool.json` membership.
  Impact: A read command causes side effects, which breaks safety expectations and complicates debugging.
  Recommended fix: Keep `braid status` read-only; perform `pool.json` membership reconciliation only in explicit mutating commands (`add`, `remove`, `remove-missing`, `replace`).

## Git commits

Use Conventional Commits-style commit messages.
