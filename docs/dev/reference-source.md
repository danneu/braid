---
intent: Inventory of the vendored upstream source under `reference/` -- what each checkout contains and what to read it for -- plus how to cite external upstream code in braid's docs and comments. Consult before searching the web for tool behavior or output formats, or before citing reference/ code.
---

# Reference source

Before searching the web for tool behavior, consult local resources first. `reference/` contains shallow clones of upstream repos at the versions pinned in nixpkgs, plus Rust crate sources pinned in `Cargo.lock`. Refresh with `just fetch-references`.

**When to look:** Any time you're implementing, modifying, or debugging code that interacts with these tools — especially parsers. Read the relevant source before making assumptions about output format or behavior.

- **btrfs-progs** — [kdave/btrfs-progs](https://github.com/kdave/btrfs-progs)
  - **Source:** `reference/btrfs-progs/cmds/` — one file per subcommand (e.g. `cmds/scrub.c`). Parser output formats, exit codes.
  - **Docs:** `reference/btrfs-progs/Documentation/` — RST. See [btrfs docs](#btrfs-docs) below for the topic table.
- **systemd** — [systemd/systemd](https://github.com/systemd/systemd)
  - **Source:** `reference/systemd/src/` — unit lifecycle internals, `systemd-ask-password`, mount/automount.
  - **Docs:** `reference/systemd/docs/` — markdown design docs (`BOOT.md`, `INHIBITOR_LOCKS.md`, `MOUNT_REQUIREMENTS.md`, `CREDENTIALS.md`, `PASSWORD_AGENTS.md`, etc.). `reference/systemd/man/` — XML man-page sources for unit/option reference (`systemd.service.xml`, `systemd.mount.xml`, …).
- **autosuspend** — [languitar/autosuspend](https://github.com/languitar/autosuspend)
  - **Source:** `reference/autosuspend/src/` — check classes, config schema, wakeup scheduling.
  - **Docs:** `reference/autosuspend/doc/source/` — RST (`available_checks.rst`, `available_wakeups.rst`, `configuration_file.rst`, `systemd_integration.rst`).
- **cryptsetup** — [cryptsetup/cryptsetup](https://gitlab.com/cryptsetup/cryptsetup)
  - **Source:** `reference/cryptsetup/src/` (CLI), `reference/cryptsetup/lib/` (libcryptsetup) — `luksDump` output, LUKS2 header structure, keyslot operations.
  - **Docs:** `reference/cryptsetup/man/` — `*.8.adoc` man pages (`cryptsetup-luksDump.8.adoc`, `cryptsetup-open.8.adoc`, …). `reference/cryptsetup/docs/` — design notes including `LUKS2-locking.txt` and `on-disk-format-luks2.pdf`.
- **util-linux** — [util-linux/util-linux](https://github.com/util-linux/util-linux)
  - **Source:** `reference/util-linux/misc-utils/` (`lsblk`, `blkid`), `reference/util-linux/sys-utils/` (`mount`, `umount`), `reference/util-linux/libmount/`, `reference/util-linux/libblkid/` — `lsblk` JSON schema, `blkid` output, mount/unmount behavior.
  - **Docs:** Man pages live next to source as `*.8.adoc` (e.g. `misc-utils/lsblk.8.adoc`, `sys-utils/mount.8.adoc`). `reference/util-linux/Documentation/` is project meta (build/test/contribution notes), not user reference.
- **smartmontools** — [smartmontools/smartmontools](https://github.com/smartmontools/smartmontools)
  - **Source:** `reference/smartmontools/smartmontools/` — flat layout. `smartctl` output format, SMART attribute definitions, exit codes.
  - **Docs:** No separate docs dir. Man-page sources are inline alongside the code: `smartctl.8.in`, `smartd.8.in`, `smartd.conf.5.in`.
- **hddfancontrol** — [desbma/hddfancontrol](https://github.com/desbma/hddfancontrol)
  - **Source:** `reference/hddfancontrol/src/` — Rust daemon. `device/` (drivetemp, hddtemp, smartctl probing), `probe/` (pwm-test ramp logic), `fan/` (PWM control -- `pwm_fan.rs`), `pwm.rs` (sysfs PWM I/O), `cl.rs` (CLI args).
  - **Docs:** No separate docs dir. `reference/hddfancontrol/README.md` and `reference/hddfancontrol/systemd/hddfancontrol.service` — the upstream unit we intentionally don't use (see `modules/braid/fan-control.nix`).
- **nut** — [networkupstools/nut](https://github.com/networkupstools/nut)
  - **Source:** `reference/nut/clients/` (`upsmon.c` -- shutdown-on-LB daemon, `upsc.c` -- status query, `upscmd.c`, `upssched.c`, `upsrw.c`), `reference/nut/server/` (`upsd.c` and net protocol handlers), `reference/nut/drivers/` (`usbhid-ups.c` and per-vendor `*-hid.c` for the USB HID path v1 targets).
  - **Config schema:** `reference/nut/conf/` — sample files (`nut.conf.sample`, `ups.conf.sample`, `upsd.conf.sample`, `upsd.users.sample`, `upsmon.conf.sample.in`, `upssched.conf.sample.in`). Authoritative for fields braid generates into `/etc/nut/*`.
  - **Docs:** `reference/nut/docs/man/` — `*.txt` asciidoc man pages for daemons, drivers, and config files. `reference/nut/docs/` — design notes (`design.txt`, `net-protocol.txt`, `developer-guide.txt`, `new-drivers.txt`, `FAQ.txt`).
- **linux** — [torvalds/linux](https://github.com/torvalds/linux)
  - **Source:** `reference/linux/` — kernel source at the exact version pinned in nixpkgs. Look in `fs/btrfs/` for btrfs-specific I/O scheduling, raid handling, and read balancing logic. `drivers/md/` for raid and block layer behavior.
  - **Use for:** Understanding kernel-level I/O behavior, raid1 read balancing, mount semantics, block device management.
- **coreutils** — [coreutils/coreutils](https://github.com/coreutils/coreutils) (GitHub mirror of GNU Coreutils)
  - **Source:** `reference/coreutils/src/` — one C file per utility (e.g. `src/timeout.c`, `src/realpath.c`, `src/stat.c`, `src/chmod.c`, `src/chown.c`, `src/head.c`, `src/base64.c`). Read these to confirm what each helper actually guarantees -- e.g. `timeout(1)` exit-code semantics and signal forwarding live in `src/timeout.c`, not in any manpage.
  - **Docs:** `reference/coreutils/doc/coreutils.texi` — the canonical reference manual (per-utility sections inside one big Texinfo file). Per-utility manpage stubs live in `reference/coreutils/man/` as `*.x` (e.g. `man/timeout.x`); these are short prologues that get merged with `--help` output by `help2man` at build time, so the full prose is in `coreutils.texi`.
  - **Use for:** Any time braid code or a plan reasons about a Coreutils helper's behavior beyond the obvious — exit codes, signal handling, race windows, `--help` text, edge cases. Especially `timeout(1)`: `timeout` cannot bound an uninterruptible kernel wait, and the proof is in `src/timeout.c`'s use of `kill()` against a userspace child.
- **nix (Rust crate)** -- [nix-rust/nix](https://github.com/nix-rust/nix)
  - **Source:** `reference/nix-crate/src/` -- Rust crate at the version pinned in `Cargo.lock`, not `flake.lock`. `unistd.rs` (User/Group/chown/exec helpers, fd ownership types), `fcntl.rs` (`open`, `flock`, `OFlag`), `errno.rs` (`Errno`), `sys/stat.rs` (`Mode`), `sys/signal.rs` (sigaction, signal handlers), `sys/termios.rs` (termios constants, terminal flags).
  - **Docs:** No separate docs dir -- rustdoc is inline as `///` doc comments on each item. `reference/nix-crate/Cargo.toml` declares the feature gates (braid currently enables `fs`, `user`, `term`, and `signal`); consult it before reaching for a `nix` API to confirm which feature it lives under.
  - **Use for:** Touching any `nix::` API, checking feature gates, understanding fd-ownership types, signal-safe helpers, or termios constants. Refresh after any change to the `nix` line in `cli/Cargo.toml` or any `cargo update`-driven bump in `Cargo.lock`.

## btrfs docs

- **Docs:** `reference/btrfs-progs/Documentation/` — RST docs from btrfs-progs. Start with `index.rst` for a full table of contents, or use the topic table below for common lookups. Glob by keyword for anything not in the table. `ch-*` fragments are inlined by `just fetch-references`.

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

## Citing reference/ code

braid's own tracked files are cited by `path#symbol` or `path#heading-slug`
([doc and ADR file references](doc-citations.md)); external upstream code is
different. It lives in `reference/`, which is gitignored and refreshed wholesale
by `just fetch-references`: it is absent on a clean checkout and invisible to CI.
A line number into it drifts on every refresh, and a braid-style `path#symbol` is
not greppable when the file is not on disk -- neither form validates or even
resolves. Cite external upstream code by its **shape**:

- **Short, behavior-defining snippet** -- one line or small function emitting a format,
  token, or exit code braid parses. Inline the excerpt as frozen ground truth, so a reader
  sees the contract without fetching `reference/`. Stamp it `pkg <version>, <path> (fn name)`
  and drop the line number. Fence the excerpt with a non-`rust` language tag -- `c` for
  source, `text` for tool output -- so rustdoc does not run it as a doctest. An unannotated
  or `rust`-tagged block becomes a failing doctest, caught by `cargo test -p braid-cli --doc`
  (not `just test-rust`, whose `--lib --bin --test` selectors skip doctests).
  Precedent: `cli/src/parse/cryptsetup_luks_version.rs#parse_cryptsetup_luks_version`. An
  inline code span (`` `printf(...)` ``) is fine for a tight function or field doc where a
  fenced block is too heavy. The `pkg <version>` stamp is the upstream release tag (`git -C
reference/<pkg> describe --tags`); it pins the excerpt and is the re-verify trigger when a
  nixpkgs bump changes that tool's version -- the same [parser-compatibility](parser-compatibility.md) refresh event
  that recaptures fixtures.
- **Region or multi-line** -- a code area with no single quotable line (a long function, a
  struct, two scattered lines). Keep a pointer, not a wall of inlined code: `pkg <version>,
<path> (fn name)` plus a one-line paraphrase of what's there. Prefer a function name over a line
  number; a bare line range is a last resort.

Existing bare-line-number `reference/` citations are tolerated -- nothing validates them
either way -- but migrate them toward the excerpt or pointer form when you next touch the
surrounding file.
