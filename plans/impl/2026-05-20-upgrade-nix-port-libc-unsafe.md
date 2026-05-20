# Upgrade to `nix` 0.31.3 and port `libc` `unsafe` to safe nix wrappers across cli/src + cli/tests

## Context

`cli/src/luks.rs`, a handful of nearby production sites, and several
integration tests use raw `libc` for Unix syscall work that already
has typed safe-wrapper equivalents in `nix`. After this plan, the
only production `unsafe` retained is the `hdparm.rs` `HDIO_DRIVE_CMD`
ioctl (which `nix` cannot model as a safe wrapper -- see "Unsafe
deliberately left in place" below).

Production sites in scope:

1. **`luks.rs` termios** -- terminal echo suppression while reading a
   passphrase from a TTY, currently using `libc::tcgetattr` /
   `libc::tcsetattr` with `MaybeUninit`/`assume_init` dances and raw
   `c_lflag &= !libc::ECHO` bit math.
2. **`luks.rs` openpty** -- `openpty(3)` test helper that leaks both
   fds if the surrounding `assert_eq!` panics.
3. **`luks.rs` and `confirm.rs` stdin fd 0 wrappers** -- both files
   use `File::from_raw_fd(0)` + `ManuallyDrop` to read from the
   process stdin without closing fd 0 on drop. Port to
   `nix::unistd::dup(std::io::stdin())` returning a separate
   `OwnedFd`, then `File::from(owned_fd)`. This keeps the
   "don't close fd 0" invariant intact (the duped fd is independent),
   continues to avoid `std::io::Stdin`'s line-buffering and lock
   semantics (we still hand a `File` to the readers), and removes
   both `unsafe` blocks. Reads via the dup'd fd share the open file
   description with fd 0, so seek/buffer state is identical to today.
4. **`main.rs` `geteuid`** -- `unsafe { libc::geteuid() }` for the
   root-check guard. Trivial parameterless syscall with an obvious
   safe wrapper.
5. **`inhibit.rs` process-group SIGKILL** -- `unsafe { libc::kill(-pgid, SIGKILL) }`
   used to tear down the systemd-inhibit + sh + sleep tree on
   `SleepInhibitor` drop. The pgid/pid-reuse safety story is the
   reason the unsafe exists, not the FFI shape -- moving to
   `nix::sys::signal::killpg` keeps the safety story intact, removes
   the FFI unsafe, and drops the `-pgid` negation idiom in favor of
   the named wrapper.

Plus one type-shape fallout from upgrading nix:

6. **`pool_lock.rs` `open_lock_file`** -- `nix::fcntl::open` returns
   `OwnedFd` in 0.31.3 instead of `RawFd`, eliminating the local
   `unsafe { File::from_raw_fd(fd) }` adapter.

Integration-test sites in scope:

7. **`cli/tests/tty_passphrase.rs`** -- reimplements the same
   `openpty` / `tcgetattr` / `termios_bytes` helpers being ported in
   `luks.rs`'s test module. Apply the same nix port for consistency.
8. **`cli/tests/root_check.rs`** -- `unsafe { libc::geteuid() }` in
   `is_root()`. Same trivial swap as `main.rs`.
9. **`cli/tests/tty_guard.rs`** -- `libc::setsid()` and two
   `libc::dup2(/dev/null fd, STDIN_FILENO/STDOUT_FILENO)` calls used
   by the redirected-stdio probe. nix provides typed safe wrappers
   (`nix::unistd::setsid`, `dup2_stdin`, `dup2_stdout`).

New regression test (not an existing port site, but added so the
stdin-wrapper change in item 3 has a real behavioral test):

10. **`cli/tests/confirm_yes.rs`** -- new subprocess test that drives
    the real `confirm::confirm_yes()` entrypoint with `"yes\n"` on
    the child's process stdin. Existing unit tests cover
    `confirm_yes_from(reader)` with injected `Cursor` readers, but
    they bypass the `dup(stdin)` wrapper introduced in §9. Without
    this regression test, the new wrapper would have no behavioral
    coverage.

Upgrade the existing direct `nix` dependency from 0.29.0 to 0.31.3 and
add the `signal` feature, then apply each of the above ports.

## Justification

### Termios (`luks.rs:182-237`)

| Issue today (libc)                                            | After (nix)                                                    |
| ------------------------------------------------------------- | -------------------------------------------------------------- |
| `MaybeUninit::<libc::termios>::zeroed()` + `assume_init()`    | `tcgetattr(fd)?` returns an initialized `Termios` directly     |
| Hand-written `rc == -1` -> `io::Error::last_os_error()` twice | nix returns `Result<_, Errno>`; mapped via `Errno: Into<std::io::Error>` |
| Raw integer math on `c_lflag` with `libc::ECHO`/`libc::ECHONL`| Typed `LocalFlags::ECHO`/`LocalFlags::ECHONL` bitflags         |
| Nothing prevents OR-ing a wrong-context flag (e.g. `CRTSCTS`) | Compile error: wrong flag type                                 |
| Two `unsafe { ... }` blocks                                   | Zero `unsafe`                                                  |

### Openpty (`luks.rs:1390-1409`)

| Issue today (libc)                                                | After (nix)                                  |
| ----------------------------------------------------------------- | -------------------------------------------- |
| Three raw `i32` fd outparams + `null_mut()` x3 + `unsafe ioctl`   | `openpty(None, None)?` returns `OpenptyResult` |
| `from_raw_fd(master)` / `from_raw_fd(slave)` after a panicking `assert_eq!` -- if the assert fires, both fds leak | `OwnedFd` pair; `File::from(owned_fd)` is infallible and RAII-safe |
| One `unsafe { libc::openpty(...) }` and two `unsafe { File::from_raw_fd(...) }` | Zero `unsafe`                          |

### Pool lock (`pool_lock.rs:282-289`)

`nix` 0.31.3 changes `nix::fcntl::open` from returning `RawFd` to
returning `OwnedFd`. `pool_lock.rs` currently adapts the 0.29 return
value with `unsafe { File::from_raw_fd(fd) }`. After the upgrade that
wrapper becomes both unnecessary and wrong-shaped: `File::from(owned_fd)`
is infallible and keeps fd ownership explicit.

### geteuid (`main.rs:380-381`)

| Issue today (libc)                                                | After (nix)                              |
| ----------------------------------------------------------------- | ---------------------------------------- |
| `unsafe { libc::geteuid() } != 0` with a SAFETY comment           | `!nix::unistd::geteuid().is_root()`      |
| One `unsafe { ... }` block                                        | Zero `unsafe`                            |

Uses the existing `user` feature already enabled on the `nix` dep. The
SAFETY comment becomes dead weight after the port -- remove it.

### Process-group SIGKILL (`inhibit.rs:25-40`)

The current call is `unsafe { libc::kill(-(child.id() as libc::pid_t),
libc::SIGKILL) }` wrapped by `kill_pgroup_and_reap`. The unsafe is FFI
shape; the real safety story is the pid-reuse window (the direct
child's pid is still valid in the kernel until we `wait()`), which is
documented in the existing SAFETY comment. That comment is about the
*signal semantics*, not the FFI, and stays valid post-port -- update
its wording to refer to the nix call.

Use `nix::sys::signal::killpg`, not `kill` with a negated pid.
POSIX `killpg(pgrp, sig)` is defined as `kill(-pgrp, sig)`
semantically, but the named wrapper documents the intent (process-
group fan-out) and removes the negation/`libc::pid_t` cast idiom.

| Issue today (libc)                                          | After (nix)                                                                 |
| ----------------------------------------------------------- | --------------------------------------------------------------------------- |
| `unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) }` | `nix::sys::signal::killpg(Pid::from_raw(child.id() as libc::pid_t), Signal::SIGKILL)` |
| Negate-and-pass idiom documents process-group target by convention | `killpg` name documents it explicitly                                       |
| Ignored `c_int` return                                      | Returns `nix::Result<()>` -- preserve best-effort by binding to `let _ = ...`. |
| One `unsafe { ... }` block                                  | Zero `unsafe`                                                               |

Requires adding the `signal` feature to the `nix` dep. Verified
against `nix-0.31.3/src/sys/signal.rs:1113`
(`pub fn killpg<T: Into<Option<Signal>>>(pgrp: Pid, signal: T) -> Result<()>`,
dispatches to `libc::killpg`) and `nix-0.31.3/Cargo.toml:84`
(`signal = ["process"]`, so enabling `signal` transitively pulls
`process` which provides `Pid`).

## Scope

Production source files:

- **`cli/src/luks.rs`** -- termios + openpty + stdin fd 0 wrapper.
- **`cli/src/confirm.rs`** -- stdin fd 0 wrapper.
- **`cli/src/pool_lock.rs`** -- adapt `nix::fcntl::open`'s new
  `OwnedFd` return type.
- **`cli/src/main.rs`** -- swap `libc::geteuid` for `nix::unistd::geteuid`.
- **`cli/src/inhibit.rs`** -- swap `libc::kill(-pgid, SIGKILL)` for
  `nix::sys::signal::killpg`.

Integration test files:

- **`cli/tests/tty_passphrase.rs`** -- port `openpty` / `tcgetattr` /
  termios byte-cast to nix.
- **`cli/tests/tty_guard.rs`** -- port `setsid` / `dup2` to nix.
- **`cli/tests/root_check.rs`** -- port `geteuid` to nix.
- **`cli/tests/confirm_yes.rs`** -- new file; subprocess regression
  test that drives `confirm::confirm_yes()` with a real stdin pipe
  to protect the new `dup(stdin)` wrapper.

Build files:

- **`cli/Cargo.toml`** -- upgrade `nix` to 0.31.3, add `"term"` and
  `"signal"` features.
- **`Cargo.lock`** -- generated by Cargo after the version update.
- **`Justfile`** -- extend the `test-rust:` recipe to include
  `--test confirm_yes` so the new regression test runs under the
  canonical Rust test command.

After this plan, the only `unsafe` block remaining in `cli/src/` is
`hdparm.rs:38` (the `HDIO_DRIVE_CMD` ioctl); `cli/tests/` is
`unsafe`-free. Existing non-`luks.rs` nix call sites are
`pool_lock.rs` and `online_state.rs`; `online_state.rs` still uses
the unchanged `User`/`Group`/`chown` APIs.

## Changes

### 1. `cli/Cargo.toml`

```toml
nix = { version = "0.31.3", features = ["fs", "user", "term", "signal"] }
```

Feature gates verified against nix 0.31.3 source:

- `term` gates *both* `nix::sys::termios::*` and `nix::pty::openpty` --
  the `pty` module sits behind `feature! { #![feature = "term"] pub mod pty; }`
  in `~/.cargo/registry/src/.../nix-0.31.3/src/lib.rs:169-171`, and
  `nix-0.31.3/Cargo.toml` declares only `term = []` (no `pty` feature).
  Adding `"term"` is sufficient; do not add `"pty"`.
- `signal` gates `nix::sys::signal::{kill, Signal}`. `nix-0.31.3/Cargo.toml:84`
  declares `signal = ["process"]`, so enabling `signal` transitively
  pulls the `process` feature (needed for `Pid`).
- `fs` and `user` are retained for existing call sites
  (`pool_lock.rs`, `online_state.rs`, and the ported `main.rs`).

Run `cargo update -p nix --precise 0.31.3` after editing
`cli/Cargo.toml` so `Cargo.lock` records the intended crate version.

### 2. `cli/src/pool_lock.rs` -- `open` now returns `OwnedFd`

`nix::fcntl::open` in 0.31.3 returns `std::os::fd::OwnedFd`, not
`RawFd`. Update `open_lock_file` to consume that owned fd directly:

```rust
fn open_lock_file(path: &Path) -> io::Result<File> {
    let fd = open(
        path,
        OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(io_from_errno)?;
    Ok(File::from(fd))
}
```

Remove the now-unused `FromRawFd` import from `pool_lock.rs`. This
preserves the existing error mapping and lock behavior while deleting
the local fd-ownership `unsafe`.

### 3. `cli/src/luks.rs` -- termios

**fd ownership model.** nix 0.31.3 types both `tcgetattr` and `tcsetattr`
as `fn<Fd: AsFd>(fd: Fd, ...)`, not `RawFd`. The current code threads a
`RawFd` derived from `tty.as_raw_fd()` through `tcgetattr`,
`tcsetattr_now`, and `TermiosGuard`. The port carries an `AsFd` borrow
through `BorrowedFd<'a>` instead, and -- since `std::fs::File`
implements `Read` *and* `Write` for `&File` on Unix -- the function
never needed `&mut File` to begin with. Switching the parameter to a
shared reference dissolves the borrow-check problem the previous
draft worked around with a `&*tty` reborrow.

- `TermiosGuard` becomes lifetime-parameterized and holds a
  `BorrowedFd<'a>` rather than a `RawFd`. Same lifetime as the
  `&File` passed into `read_tty_from_file`. The guard cannot outlive
  the file -- which is the correct invariant; today it's enforced
  only by documentation.
- **Public signature change.** `read_tty_from_file` becomes
  `pub fn read_tty_from_file(mut tty: &std::fs::File, label: &str)
  -> Result<Passphrase, LuksError>`. The `mut tty: &File` binding is
  needed so that `&mut tty` can reborrow the shared reference as a
  `&mut R: Read` for `read_line_into_zeroizing` (the callee's
  `&mut R: Read + ?Sized` signature at `luks.rs:349-352`). The
  `BorrowedFd<'a>` returned by `tty.as_fd()` borrows the underlying
  file (the data behind `tty`), not the local binding -- so
  `_guard`'s lifetime is independent of the `&mut tty` reborrow used
  to satisfy `Read::read`. All previous concerns about E0502 go
  away once the parameter is `&File`.

  The function body:

  ```rust
  let fd = tty.as_fd();
  let orig = nix::sys::termios::tcgetattr(fd).map_err(std::io::Error::from)?;
  let mut modified = orig.clone();
  modified.local_flags.remove(LocalFlags::ECHO);
  modified.local_flags.insert(LocalFlags::ECHONL);
  let _guard = TermiosGuard::install(fd, modified, orig)?;
  tty.write_all(label.as_bytes())?;           // &File: Write
  tty.flush()?;                                // &File: Write
  let raw = read_line_into_zeroizing(&mut tty, "terminal")?;  // &mut (&File): Read
  finalize_passphrase_bytes(&raw, "terminal")
  ```

  No `tty_io` local, no `&*tty` reborrow, no "borrow-check shape"
  discussion needed.

- **Call-site updates.** Two `read_tty_from_file` call sites need
  their `&mut` dropped, and their local bindings can shed the `mut`:
  - `luks.rs:177` (production, `RealTty::read_tty`): the local
    binding currently reads `let mut tty = std::fs::OpenOptions::new()
    .read(true).write(true).open("/dev/tty")...?;` followed by
    `read_tty_from_file(&mut tty, label)`. After the pivot:
    `let tty = ...?;` and `read_tty_from_file(&tty, label)`.
  - `tty_passphrase.rs:105` (integration test, `probe_pty_integration`):
    `let (mut master_file, mut slave_file) = open_pty_pair();` ->
    `let (mut master_file, slave_file) = open_pty_pair();`
    (`master_file` keeps `mut` because it's used directly with
    `read_exact` / `write_all` which still take `&mut self`).
    `read_tty_from_file(&mut slave_file, PROMPT)` ->
    `read_tty_from_file(&slave_file, PROMPT)`.

**Error mapping.** `read_tty_from_file` returns `Result<_, LuksError>`,
and `LuksError` has `From<std::io::Error>` (variant `Io`) but not
`From<nix::Errno>`. nix's `tcgetattr` / `tcsetattr` return
`nix::Result<_, Errno>`. Don't add a new `LuksError` variant -- route
errors through the existing `Io` boundary by mapping `Errno` to
`std::io::Error` explicitly. `nix::Errno` implements `From<Errno> for
std::io::Error`, so the call sites look like:

```rust
nix::sys::termios::tcgetattr(fd).map_err(std::io::Error::from)?
```

This produces an `std::io::Error`, which then flows through
`LuksError::Io` via the existing `#[from]` impl. A one-line local
helper or `.map_err(LuksError::from_errno)` could centralize the
conversion if it appears more than twice -- the implementer picks
whichever reads cleaner.

**Delete:**

- `fn tcgetattr(fd: RawFd) -> std::io::Result<libc::termios>` (lines 197-205)
- `fn tcsetattr_now(fd: RawFd, termios: &libc::termios) -> std::io::Result<()>` (lines 207-214)

**Update `TermiosGuard` (lines 217-237):**

```rust
struct TermiosGuard<'a> {
    fd: BorrowedFd<'a>,
    orig: nix::sys::termios::Termios,
}

impl<'a> TermiosGuard<'a> {
    fn install(
        fd: BorrowedFd<'a>,
        modified: Termios,
        orig: Termios,
    ) -> std::io::Result<TermiosGuard<'a>> {
        nix::sys::termios::tcsetattr(fd, SetArg::TCSANOW, &modified)
            .map_err(std::io::Error::from)?;
        Ok(TermiosGuard { fd, orig })
    }
}

impl<'a> Drop for TermiosGuard<'a> {
    fn drop(&mut self) {
        let _ = nix::sys::termios::tcsetattr(self.fd, SetArg::TCSANOW, &self.orig);
    }
}
```

(Keep the existing comment that explains Drop covers normal returns and
Rust unwinds, not process signals.)

**Update `read_tty_from_file` (lines 182-195):** see the "Public
signature change" block above for the new signature
(`mut tty: &std::fs::File`) and the full function body. Update the
two call sites accordingly (also covered above).

Verified against `~/.cargo/registry/src/.../nix-0.31.3/src/sys/termios.rs`:
`Termios` derives `Clone, Debug, Eq, PartialEq` and is not `Copy`
(it holds a `RefCell<libc::termios>`). The `.clone()` is required.

### 4. `cli/src/luks.rs` -- openpty (test code only)

**Update `open_pty_pair` (lines 1390-1409):**

```rust
fn open_pty_pair() -> (std::fs::File, std::fs::File) {
    let r = nix::pty::openpty(None, None).expect("openpty failed");
    (std::fs::File::from(r.master), std::fs::File::from(r.slave))
}
```

`OpenptyResult { master, slave }` are `OwnedFd`; `File: From<OwnedFd>` is
infallible.

### 5. `cli/src/luks.rs` -- test helpers using `libc::termios`

**Termios is not `Copy`, and derived equality is a trap.** Verified
against `nix-0.31.3/src/sys/termios.rs:155-167`: `Termios` derives
`Clone, Debug, Eq, PartialEq` but includes a private
`inner: RefCell<libc::termios>` field alongside the public
`input_flags` / `output_flags` / `control_flags` / `local_flags` /
`control_chars` fields. Derived `PartialEq` compares *all* fields,
including `inner`.

Crucially, mutating a public flag (`modified.local_flags.remove(...)`)
does *not* update the inner `RefCell`. nix syncs the inner from the
public fields only inside `get_libc_termios()` -- which is called when
nix uses the value in a syscall (e.g. `tcsetattr`), not when you
mutate the wrapper. So a `modified` value whose `local_flags` you just
flipped still carries the *pre-flip* bytes in `modified.inner`. A
naive `assert_eq!(modified, during)` after the syscall would compare
pre-flip `modified.inner` against post-flip `during.inner` (the
latter freshly built from the kernel via `From<libc::termios>`) and
fail.

Three call-site implications:

1. **Pass clones.** `let mut modified = before` previously worked
   because `libc::termios` is `Copy`. `Termios` is not. Use
   `let mut modified = before.clone();` and pass `before.clone()` /
   `modified.clone()` into `TermiosGuard::install` and
   `install_then_fail`, keeping the originals available for
   assertions.

2. **Compare the field we actually mutate, not the whole struct.**
   In the "during" assertion of `termios_guard_restores_on_drop`,
   replace the byte-cast comparison with:

   ```rust
   assert_eq!(modified.local_flags, during.local_flags);
   ```

   This is structure-insensitive (verifies the behavior the test
   actually exercises -- the ECHO flip landed) and avoids the
   `inner`-out-of-sync trap.

3. **"After restore" assertion is a public-field helper (per-field
   `assert_eq!`s), not whole-struct, and not `local_flags` alone.**

   Two failure modes to avoid here:

   - Whole-struct `assert_eq!(before, after)` on `Termios` looks
     stronger but is misleading: `From<libc::termios> for Termios`
     builds the flag fields via `*Flags::from_bits_truncate`
     (`nix-0.31.3/src/sys/termios.rs:238-241`), and `tcsetattr`
     re-marshals through `get_libc_termios` which writes
     `inner.c_lflag = self.local_flags.bits()` (and the same for
     `c_iflag` / `c_oflag` / `c_cflag`). Any bit the kernel set that
     a `*Flags` bitflag set does not enumerate is *silently
     stripped* on every restore round-trip. Because the test holds
     the original `before` value alongside the post-restore `after`
     (we pass clones into `install`, per implication 1), the
     original `before.inner` still contains the kernel's full
     unstripped c_lflag, while `after.inner` reflects the stripped
     post-round-trip state. A whole-struct assert can therefore
     *fail spuriously* whenever the kernel sets a c_lflag bit nix
     doesn't model -- a future platform or kernel bit would break
     the test even though the guard restored everything nix actually
     exposes. The private-`inner` axis is exactly what the public-
     field helper is meant to sidestep.
   - Narrowing to `local_flags` alone is too tight in the opposite
     direction: `TermiosGuard`'s contract is "restore *terminal
     attributes*," not just echo. A regression that clobbers
     `input_flags`, `output_flags`, `control_flags`, or
     `control_chars` would pass a local-flags-only assert.

   Compare the *full* set of public field axes, sidestepping the
   private `inner: RefCell<libc::termios>` while exercising the full
   behavioral surface the guard owns. The full public field set in
   nix 0.31.3 is:

   - `input_flags`, `output_flags`, `control_flags`, `local_flags`,
     `control_chars` (cross-platform, always present)
   - `line_discipline` (cfg-gated to Linux, Android, Haiku --
     verified at `nix-0.31.3/src/sys/termios.rs:169-173`)

   Introduce a small helper `assert_termios_public_eq(before: &Termios,
   after: &Termios)` that handles the cfg gate cleanly via per-field
   asserts:

   ```rust
   fn assert_termios_public_eq(before: &Termios, after: &Termios) {
       assert_eq!(before.input_flags,   after.input_flags,   "input_flags");
       assert_eq!(before.output_flags,  after.output_flags,  "output_flags");
       assert_eq!(before.control_flags, after.control_flags, "control_flags");
       assert_eq!(before.local_flags,   after.local_flags,   "local_flags");
       assert_eq!(before.control_chars, after.control_chars, "control_chars");
       #[cfg(any(target_os = "linux", target_os = "android", target_os = "haiku"))]
       assert_eq!(before.line_discipline, after.line_discipline, "line_discipline");
   }
   ```

   Why this shape rather than a tuple:

   - Tuple equality would require its own `#[cfg]` arms to include
     or exclude `line_discipline`, doubling the test surface for no
     benefit.
   - Per-field `assert_eq!` calls with field-name messages give
     direct diagnostics on failure ("input_flags assertion failed
     ..."), pinpointing the regressed axis without needing
     pretty-assertions tuple-diff parsing.
   - On NixOS/Linux (the production target), the `line_discipline`
     branch is always active, so the test exercises the *full*
     public-field surface that nix exposes for this platform.

   Use this helper at all three "after restore" assertion sites:
   `termios_guard_restores_on_drop` (`luks.rs:1454`),
   `termios_guard_restores_on_question_mark_return` (`luks.rs`,
   inside the outer test), and `pty_integration`
   (`tty_passphrase.rs:124`). Define one copy file-local in the
   `luks.rs` test module and one copy in `tty_passphrase.rs` (or
   factor into `cli/tests/support/termios.rs` for the integration
   tests if that pattern is already in use -- a `golden_common.rs`
   shared module already exists in `cli/tests/support/`).

   In practice all current Linux c_lflag / c_iflag / c_oflag /
   c_cflag bits are enumerated by nix's `*Flags`, so even a
   whole-struct form would pass today -- but a future kernel bit or
   platform variant would surface as a spurious failure the test
   was never actually guarding against. The helper form is the
   structure-insensitive boundary that matches the guard's
   documented contract while still covering every public-field axis
   nix exposes on this platform.

**Concrete rewrites:**

- `flip_echo(termios: &mut libc::termios)` (lines 1411-1417) ->
  `flip_echo(termios: &mut Termios)`; body becomes
  `termios.local_flags.toggle(LocalFlags::ECHO)`.
- `termios_bytes` (lines 1419-1426) and `assert_termios_eq`
  (lines 1428-1430) -- **delete both**. Add the
  `assert_termios_public_eq` helper from item 3 inside the
  `#[cfg(test)]` module instead. The remaining assertions are
  `assert_eq!(modified.local_flags, during.local_flags)` for the
  "during" axis, and `assert_termios_public_eq(&before, &after)` for
  the "after restore" axis. Whole-struct equality on `Termios` is
  not used.
- `termios_guard_restores_on_drop` (line 1440) -- bindings become
  `Termios`; pass clones into `install`; "during" assert is on
  `local_flags`; "after restore" assert calls
  `assert_termios_public_eq(&before, &after)` at the current
  `luks.rs:1454` site, covering every public-field axis nix exposes
  for Linux (`input_flags`, `output_flags`, `control_flags`,
  `local_flags`, `control_chars`, plus the cfg-gated
  `line_discipline`).
- `termios_guard_restores_on_question_mark_return` (line 1466) -- the
  nested `install_then_fail(fd: RawFd, before: libc::termios)` (line
  1467) retypes to `install_then_fail(fd: BorrowedFd<'_>, before: Termios)`,
  internally `let mut modified = before.clone(); flip_echo(&mut modified);
  let _guard = TermiosGuard::install(fd, modified, before)?;`. The
  outer test's "after restore" assertion is
  `assert_termios_public_eq(&before, &after)` -- same helper as
  `termios_guard_restores_on_drop`.

### 6. `cli/src/main.rs` -- `geteuid`

Replace lines 380-381:

```rust
// SAFETY: geteuid() is a trivial syscall with no arguments, always safe to call.
if needs_root && unsafe { libc::geteuid() } != 0 {
```

with:

```rust
if needs_root && !nix::unistd::geteuid().is_root() {
```

Verified `nix::unistd::geteuid()` returns `Uid` at
`nix-0.31.3/src/unistd.rs:1770`. `Uid::is_root()` returns `true` iff
the uid equals `0`. Delete the obsolete SAFETY comment.

### 7. `cli/src/inhibit.rs` -- process-group SIGKILL

Replace the body of `kill_pgroup_and_reap` (lines 30-40):

```rust
fn kill_pgroup_and_reap(child: &mut Child) {
    // pgid equals the direct child's pid (spawned with
    // `process_group(0)`), and that pid stays valid in the kernel
    // until we wait() below, so there is no pid-reuse window.
    // killpg fans the signal out to the entire process group.
    // Best-effort: the result is intentionally ignored (the child
    // may have already exited).
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(child.id() as libc::pid_t),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = child.wait();
}
```

Key points:

- Use `killpg`, not `kill`. `nix::sys::signal::killpg(pgrp, sig)` at
  `nix-0.31.3/src/sys/signal.rs:1113` dispatches to `libc::killpg`,
  which is POSIX-defined as `kill(-pgrp, sig)`. The named wrapper
  documents the intent (process-group fan-out) and eliminates the
  unary-minus + `libc::pid_t` cast idiom that the previous code
  used to express "pgrp target."
- `libc::pid_t` is still used once for the `u32 -> pid_t (i32)`
  cast on `child.id()`. `libc` remains a direct dep for this and
  for the `hdparm.rs` ioctl.
- `nix::unistd::Pid::from_raw` is a simple `pid_t` constructor
  (`nix-0.31.3/src/unistd.rs:165-167`); it does not validate.
- `killpg` returns `nix::Result<()>`; bind to `let _ = ...` to
  preserve today's ignored-result, best-effort behavior. No
  `LuksError` / `io::Error` mapping is needed -- the function
  returns `()`.
- Rewrite the SAFETY comment so it documents the *signal semantics*
  (pid-reuse window, process-group fan-out) rather than the FFI
  shape or the negation idiom, both of which are no longer relevant
  to the call site.

### 8. `cli/src/luks.rs` -- stdin fd 0 wrapper (`read_passphrase_with`)

Replace lines 299-309. Today:

```rust
if passphrase_file.is_none() && passphrase_stdin {
    use std::os::unix::io::FromRawFd;
    let mut stdin = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
    return read_passphrase_with_readers(
        passphrase_file, passphrase_stdin, confirm_new, &mut *stdin, tty,
    );
}
```

After:

```rust
if passphrase_file.is_none() && passphrase_stdin {
    // dup so we can hand a plain File to read_passphrase_with_readers --
    // std::io::stdin() would re-engage Stdin's line buffer and global mutex,
    // breaking the byte-at-a-time read contract the passphrase reader relies on.
    let stdin_fd = nix::unistd::dup(std::io::stdin())
        .map_err(std::io::Error::from)?;
    let mut stdin = std::fs::File::from(stdin_fd);
    return read_passphrase_with_readers(
        passphrase_file, passphrase_stdin, confirm_new, &mut stdin, tty,
    );
}
```

The inline comment captures the institutional rationale that today's
`ManuallyDrop<File::from_raw_fd(0)>` pattern was carrying implicitly
("don't close fd 0, don't use Stdin"). Without it, a future maintainer
seeing this code might "simplify" to `std::io::stdin().lock()` and
silently regress the unbuffered-read contract documented at
`confirm.rs:113-116`.

Verified `nix::unistd::dup<Fd: AsFd>(oldfd: Fd) -> Result<OwnedFd>`
at `nix-0.31.3/src/unistd.rs:439`. `std::io::Stdin` implements
`AsFd` (Rust 1.63+, I/O safety RFC), so `dup(std::io::stdin())`
compiles. `File::from(owned_fd)` is infallible.

**Behavioral equivalence (passphrase-sensitive surface):**

- *fd 0 stays open.* The duped fd is a separate kernel-level fd
  pointing at the same open file description. When `stdin: File`
  is dropped at function exit, only the duped fd is closed; fd 0 is
  unaffected. The `ManuallyDrop` trick is no longer needed.
- *No stdio buffering.* The reader is still a `&mut File`, not
  `Stdin` / `StdinLock`. `std::io::Stdin`'s line-buffered reads
  and the global stdin mutex are *not* engaged through this path,
  preserving today's behavior for `read_passphrase_with_readers`.
- *Read position semantics.* Reads via the duped fd share the same
  open file description with fd 0, so file/pipe seek state and
  buffer drain are identical to reading via fd 0 directly. For TTY
  stdin (no seek state) the read path is also identical.
- *Failure mode.* `dup(2)` can fail with `EMFILE`/`ENFILE` if the
  process or system has exhausted fds. Today's `from_raw_fd(0)`
  cannot fail. The plan maps the failure through
  `LuksError::Io(std::io::Error)` -- a passphrase read that
  cannot acquire a duped stdin fd surfaces as a clear I/O error
  to the caller instead of running. This is a strict improvement
  over the silent assumption that fd 0 is always wrappable.

### 9. `cli/src/confirm.rs` -- stdin fd 0 wrapper (`confirm_yes`)

Replace lines 143-148. Today:

```rust
pub fn confirm_yes() -> Result<(), String> {
    use std::os::unix::io::FromRawFd;
    let mut stdin_file = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
    confirm_yes_from(&mut *stdin_file)
}
```

After:

```rust
pub fn confirm_yes() -> Result<(), String> {
    // dup so we hand a plain File to confirm_yes_from -- std::io::stdin()
    // would re-engage Stdin's line buffer and pre-drain bytes the next
    // --passphrase-stdin reader needs (see invariant at line 113).
    let stdin_fd = nix::unistd::dup(std::io::stdin())
        .map_err(|e| format!("dup stdin: {e}"))?;
    let mut stdin_file = std::fs::File::from(stdin_fd);
    confirm_yes_from(&mut stdin_file)
}
```

`confirm_yes` returns `Result<(), String>`, so the `dup` error is
flattened to a `String` via `format!`. Same behavioral guarantees as
§8. The inline comment ties the implementation choice to the
existing invariant doc on `confirm_yes_from` (`confirm.rs:113-116`)
so the rationale is discoverable from the call site.

### 10. `cli/tests/tty_passphrase.rs` -- PTY/termios helpers

Replace the local copies of `open_pty_pair` (lines 58-72), `tcgetattr`
(lines 74-85), and `termios_bytes` (lines 87-...) with nix equivalents:

```rust
fn open_pty_pair() -> (File, File) {
    let r = nix::pty::openpty(None, None).expect("openpty failed");
    (File::from(r.master), File::from(r.slave))
}
```

For the termios reads + equality check, use
`nix::sys::termios::tcgetattr` returning `nix::sys::termios::Termios`,
and call the `assert_termios_public_eq(before: &Termios, after: &Termios)`
helper prescribed in §5 implication 3 at the current
`tty_passphrase.rs:124` site:

```rust
assert_termios_public_eq(&before, &after);
```

Add a copy of the helper file-local in `tty_passphrase.rs` (or factor
into `cli/tests/support/termios.rs` alongside the existing
`golden_common.rs` module). The helper body matches §5 implication 3
verbatim: per-field `assert_eq!` on `input_flags`, `output_flags`,
`control_flags`, `local_flags`, `control_chars`, plus `line_discipline`
under `#[cfg(any(target_os = "linux", target_os = "android", target_os = "haiku"))]`.

`read_tty_from_file`'s contract is "restore all terminal attributes,"
not just echo, so the integration test must exercise the full
public-field surface that nix exposes for the platform -- not just
`local_flags`, and not the whole struct (which silently strips
unknown bits through nix's `*Flags::from_bits_truncate` + `bits()`
marshaling). The shared helper keeps the in-tree `luks.rs` tests and
the integration test asserting against the same contract shape.
Delete `termios_bytes` and the manual `assert_termios_eq` helper.

Drop the `use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};` and
`use libc;` imports that become unused after the rewrites.

### 11. `cli/tests/root_check.rs` -- `geteuid` swap

Replace lines 3-6. Today:

```rust
fn is_root() -> bool {
    // SAFETY: geteuid() is a trivial syscall with no arguments, always safe to call.
    (unsafe { libc::geteuid() }) == 0
}
```

After:

```rust
fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}
```

Delete the SAFETY comment and the `libc` import if it becomes unused
in this file.

### 12. `cli/tests/tty_guard.rs` -- `setsid` and `dup2` swaps

**`detach_session` (lines 33-42):**

```rust
fn detach_session() {
    nix::unistd::setsid().expect("setsid");
}
```

`nix::unistd::setsid() -> Result<Pid>` (`nix-0.31.3/src/unistd.rs:334`).
The Pid return value is unused; `.expect("setsid")` preserves today's
panic-on-failure shape.

**`redirect_stdio_to_dev_null` (lines 44-56):**

```rust
fn redirect_stdio_to_dev_null() {
    let null = File::options()
        .read(true)
        .write(true)
        .open("/dev/null")
        .expect("open /dev/null");

    nix::unistd::dup2_stdin(&null).expect("dup2 stdin");
    nix::unistd::dup2_stdout(&null).expect("dup2 stdout");
}
```

`dup2_stdin<Fd: AsFd>` and `dup2_stdout<Fd: AsFd>` exist at
`nix-0.31.3/src/unistd.rs:454, 500`. `&File: AsFd`, so the `null`
handle binds directly. Delete the `AsRawFd` import that becomes
unused.

### 13. New regression test: `cli/tests/confirm_yes.rs`

The existing unit tests cover `confirm_yes_from(reader)` with injected
`Cursor`/scripted readers, but the *real* `confirm_yes()` wrapper --
which is what §9 changes from `File::from_raw_fd(0)` + `ManuallyDrop`
to `nix::unistd::dup(std::io::stdin())` + `File::from(OwnedFd)` -- has
no behavioral test.

A naive "pipe `yes\n` and assert success" test is insufficient. It
would pass even if a future implementation used buffered
`std::io::stdin()` and pre-drained later bytes, or if it closed
fd 0 after consuming the confirmation. Both regressions silently break
the invariant documented at `confirm.rs:113-116`:

> This helper intentionally accepts `Read`, not `BufRead`, so
> confirmation cannot pre-drain bytes needed by a later
> `--passphrase-stdin` read.

The test must therefore prove three things at once:

1. `confirm_yes()` accepts `"yes\n"` from the real process stdin.
2. After it returns, bytes piped *after* the newline are still
   readable from the process stdin description.
3. fd 0 is still usable -- specifically, a fresh
   `nix::unistd::dup(std::io::stdin())` succeeds *and* the resulting
   `File` drains the bytes the parent piped after `"yes\n"`.

Add `cli/tests/confirm_yes.rs` using the existing recurse-via-`current_exe()`
+ env-var-probe pattern from `tty_passphrase.rs` and `tty_guard.rs`,
with the strengthened post-confirm read:

```rust
#![cfg(unix)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};

fn maybe_run_probe() {
    if std::env::var("BRAID_CONFIRM_YES_PROBE").is_err() {
        return;
    }

    // Step 1: real confirm_yes() against process stdin.
    if let Err(e) = braid_cli::confirm::confirm_yes() {
        eprintln!("confirm_yes failed: {e}");
        std::process::exit(2);
    }

    // Step 2: prove fd 0 is still open and not pre-drained.
    // Dup process stdin into a fresh OwnedFd, wrap in an unbuffered
    // File, and read all remaining bytes. We expect exactly "secret\n"
    // -- the bytes the parent piped after the "yes\n" confirmation.
    let stdin_fd = match nix::unistd::dup(std::io::stdin()) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("dup(stdin) after confirm failed: {e}");
            std::process::exit(3);
        }
    };
    let mut tail = std::fs::File::from(stdin_fd);
    let mut buf = Vec::new();
    if let Err(e) = tail.read_to_end(&mut buf) {
        eprintln!("read remaining stdin failed: {e}");
        std::process::exit(4);
    }
    if buf != b"secret\n" {
        eprintln!(
            "post-confirm stdin tail mismatch: expected b\"secret\\n\", got {:?}",
            buf
        );
        std::process::exit(5);
    }

    std::process::exit(0);
}

#[test]
fn confirm_yes_does_not_predrain_following_bytes() {
    maybe_run_probe();

    let mut child = Command::new(std::env::current_exe().expect("current test binary"))
        .env("BRAID_CONFIRM_YES_PROBE", "1")
        .arg("--exact")
        .arg("confirm_yes_does_not_predrain_following_bytes")
        .arg("--test-threads=1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit()) // surface probe diagnostics on failure
        .spawn()
        .expect("spawn confirm_yes probe");

    // Pipe both the confirmation line and a follow-on payload.
    // confirm_yes must consume exactly "yes\n" and leave "secret\n"
    // readable from the underlying process stdin description.
    {
        let stdin = child.stdin.as_mut().expect("child stdin");
        stdin.write_all(b"yes\nsecret\n").expect("write to child stdin");
    }
    // Closing stdin so the child's read_to_end returns once it has
    // consumed the trailing "secret\n".
    drop(child.stdin.take());

    let status = child.wait().expect("wait for child");
    assert!(
        status.success(),
        "confirm_yes() must accept yes\\n and leave following bytes \
         readable from fd 0 -- child exited {status:?}. Exit codes: \
         2=confirm failed, 3=post-confirm dup failed, 4=read failed, \
         5=tail bytes mismatch."
    );
}
```

What this catches:

- **fd 0 closure:** if a future regression drops the dup'd `File`
  in a way that closes fd 0 (e.g. forgets `ManuallyDrop` semantics
  *and* misuses `from_raw_fd` instead of going through `nix::dup`),
  the child's post-confirm `nix::unistd::dup(std::io::stdin())`
  fails with `EBADF` and the probe exits 3.
- **Wrong-fd read:** if the wrapper reads from a different fd than
  fd 0 (e.g. a stale duplicate), the parent's `"secret\n"` is still
  pending on fd 0 and the post-confirm read sees it. Conversely, if
  reads happened on the right fd but were duplicated to another fd
  pre-port, the probe would still see `"secret\n"` -- but the
  positive path is what we want to assert.
- **Buffering / pre-drain:** if a future implementation switches to
  `Stdin::lock()` / `BufReader<...>`, the buffered reader pulls
  ahead and consumes `"secret\n"` while looking for the `'\n'` in
  the confirmation line. The child's post-confirm fresh read then
  sees an empty buffer (or `"\n"` alone, depending on read sizing)
  and the probe exits 5. Today's byte-at-a-time loop in
  `confirm_yes_from` (`confirm.rs:122-134`) does *not* pre-drain;
  this test pins that property to the wrapper.

Notes:

- `BRAID_CONFIRM_YES_PROBE` env var name follows the local
  convention (`BRAID_TTY_PROBE`, `BRAID_TTY_GUARD_PROBE`).
- Test name and `--exact` argument must match so cargo's test
  runner filters down to a single function in the child.
- `stderr(Stdio::inherit())` so the probe's diagnostic prints
  appear in the parent's test output on failure -- the `status`
  alone tells you *that* something failed; the inherited stderr
  tells you *which* of the five failure modes above tripped.
- The parent drops `child.stdin` after writing so the child's
  `read_to_end` returns on EOF; otherwise the child would block
  forever waiting for more bytes after `"secret\n"`.

### 14. Remove unused `libc` imports

After the ports, run a final sweep on the touched files for
remaining `libc::` references. Expected remnants:

- `cli/src/inhibit.rs` still uses `libc::pid_t` for the `pid_t` cast
  on `child.id()` inside `nix::unistd::Pid::from_raw` (positive pgid,
  passed to `killpg`).
- `cli/src/cmd.rs` still uses `libc::SIG*` integer constants for the
  signal-name match (out of scope for this plan -- cosmetic only).
- `cli/src/hdparm.rs` still uses `libc::ioctl` (deliberately retained
  -- see "Unsafe deliberately left in place" below).

The `luks.rs` termios paths, both stdin-wrapper sites, the cli/tests
helpers, `main.rs::geteuid`, and the `inhibit.rs` `kill` callsite
should have no remaining `libc::` references. `libc` stays a direct
dep crate-wide.

## Unsafe deliberately left in place

After this plan, the only `unsafe` block in `cli/src/` (and zero in
`cli/tests/`) is the `HDIO_DRIVE_CMD` ioctl in `hdparm.rs:38`.

### `cli/src/hdparm.rs` -- `HDIO_DRIVE_CMD` ioctl

Keep the raw ioctl unsafe. `nix`'s ioctl helpers (`ioctl_*!` macros)
generate per-ioctl wrappers that still expose `unsafe fn` to the
caller, because the safety story is a per-ioctl buffer/contract
matter that nix can't model generically. `HDIO_DRIVE_CMD` is a
legacy ATA passthrough ioctl with a command-specific four-byte
buffer convention -- the unsafe documents the buffer contract, and
nixifying it would mostly relocate the same `unsafe` rather than
remove it. Leave the existing SAFETY comment in place.

## Critical files

- `cli/src/luks.rs` (read-tty path: ~182-237; stdin wrapper: ~299-309;
  PTY test helpers: ~1390-1484)
- `cli/src/confirm.rs` (`confirm_yes`, ~143-148)
- `cli/src/pool_lock.rs` (`open_lock_file`, ~282-289)
- `cli/src/main.rs` (root-check guard, ~380-381)
- `cli/src/inhibit.rs` (`kill_pgroup_and_reap`, ~30-40)
- `cli/tests/tty_passphrase.rs` (`open_pty_pair`, `tcgetattr`,
  `termios_bytes`, ~58-...)
- `cli/tests/tty_guard.rs` (`detach_session`,
  `redirect_stdio_to_dev_null`, ~33-56)
- `cli/tests/root_check.rs` (`is_root`, ~3-6)
- `cli/tests/confirm_yes.rs` (NEW -- subprocess regression test
  for the `dup(stdin)` wrapper in `confirm::confirm_yes()`)
- `cli/Cargo.toml` (`nix` version/features)
- `Cargo.lock` (`nix` resolution)
- `Justfile` (`test-rust:` recipe -- add `--test confirm_yes`)

## Reuse / existing patterns

- `nix` is already a direct dep -- consistent with `pool_lock.rs` and
  `online_state.rs` which already use `nix` for `flock` and `chown`
  respectively. No new dependency.
- `nix::Errno: Into<std::io::Error>` is the standard conversion path.
  Use `.map_err(std::io::Error::from)` at nix call sites; the existing
  `LuksError::Io(#[from] std::io::Error)` impl (luks.rs:140-141) then
  handles the `?` flow into `LuksError`.

## Verification

End-to-end check that behavior is unchanged:

1. **`just test-rust`** -- the Justfile recipe at `Justfile:104-105`
   currently runs `cargo test --lib --bin braid --test
   golden_nixos_25_11 --test tty_guard`. This plan **extends** the
   recipe to also include `--test confirm_yes`, the new integration
   binary added in §13. The updated `test-rust:` recipe line:

   ```
   cargo test --lib --bin braid --test golden_nixos_25_11 --test tty_guard --test confirm_yes
   ```

   The extension makes the §13 test part of the canonical Rust test
   command, so a future regression in the §9 `dup(stdin)` wrapper
   surfaces without a contributor needing to remember an ad-hoc
   `cargo test --test confirm_yes`. The new test has no environment
   prerequisite (no TTY, no root, no privileged syscall) -- it uses
   subprocess pipes only -- so adding it to the default recipe is
   safe.

   What `just test-rust` covers after the recipe edit:
   - All unit tests in `cli/src/` (via `--lib`), including the two
     in-tree `luks.rs` termios tests (`termios_guard_restores_on_drop`,
     `termios_guard_restores_on_question_mark_return`), which
     directly exercise the ported `TermiosGuard` paths.
   - `cli/tests/tty_guard.rs` -- exercises the ported `setsid` /
     `dup2_stdin` / `dup2_stdout` helpers via
     `tui_rejects_non_tty_stdio` and `tui_demo_rejects_non_tty_stdio`.
     These fork a child probe that redirects stdio and calls
     `braid_cli::tui::run`; if the ported helpers regress, the child
     probe wedges or misbehaves and the test fails.
   - `cli/tests/confirm_yes.rs` -- the new subprocess regression
     test for the §9 `dup(stdin)` wrapper. See step 4 below for the
     full coverage description.

   `just test-rust` still does *not* run `tty_passphrase` or
   `root_check` -- those exclusions predate this plan and are kept
   (tty_passphrase historically excluded as slow / PTY-sensitive;
   root_check assertions are no-ops when the runner is root, making
   it a poor default).

2. **`cargo test --test tty_passphrase`** -- mandatory: covers the
   ported `open_pty_pair` / `tcgetattr` helpers in
   `cli/tests/tty_passphrase.rs` *and* the real behavioral surface
   of `read_tty_from_file` (prompt write, PTY read, termios restore,
   stdin-deadlock immunity). Not run by `just test-rust`.

3. **`cargo test --test root_check`** -- mandatory: covers the
   ported `is_root` helper plus the non-root behavior assertions.
   Not run by `just test-rust`.
4. **`cargo test --test confirm_yes`** (now part of `just test-rust`
   per step 1, but separately runnable) -- new test file added in
   Changes §13. The subprocess
   `confirm_yes_does_not_predrain_following_bytes` test exercises the
   real `confirm::confirm_yes()` entrypoint by piping `"yes\nsecret\n"`
   to the child, dropping the parent's writer to provoke EOF, and
   then -- *after* `confirm_yes()` returns successfully in the child --
   running a second `nix::unistd::dup(std::io::stdin())` to wrap fd 0
   in a fresh unbuffered `File` and asserting `read_to_end` yields
   exactly `b"secret\n"`. Differentiated exit codes (2/3/4/5) name
   each failure mode: confirm failed / post-confirm dup failed / read
   failed / tail bytes mismatch. This pins three contracts at once:
   (a) the new `nix::unistd::dup(std::io::stdin()) + File::from(OwnedFd)`
   wrapper accepts `"yes\n"`; (b) the wrapper does *not* pre-drain
   bytes the next `--passphrase-stdin` reader needs (the invariant
   documented at `confirm.rs:113-116`); (c) fd 0 remains open and
   usable after `confirm_yes()` returns. Without this test, a buffered
   `Stdin::lock()` regression or an fd-0 closure would only surface
   in interactive use. Included in `just test-rust` (step 1) once
   the Justfile `test-rust:` recipe is extended to add `--test
   confirm_yes`.
5. **`--passphrase-stdin` VM coverage:** `just test-vm braid-unlock`
   -- the test (`tests/cli/braid-unlock.py:59`) drives `braid unlock
   --passphrase-stdin` by piping the passphrase via `printf '%s\n' <p>
   | braid unlock --passphrase-stdin`. This exercises the production
   `read_passphrase_with(... passphrase_stdin=true ...)` path in
   `luks.rs`, which is exactly the §8 `dup(stdin)` wrapper.
   `braid-unlock` therefore covers the `luks.rs` stdin port end-to-
   end in a real NixOS VM. It does *not* cover termios: the
   passphrase is piped from a non-TTY, so the interactive prompt
   path with `TermiosGuard` is never reached. Termios coverage is
   `cargo test --test tty_passphrase` plus the in-tree
   `termios_guard_restores_on_*` tests run by `just test-rust`.
   `braid-unlock` also does not cover `inhibit.rs`; see step 6.
6. **Inhibitor process-group teardown VM test:** `just test-vm
   replace-inhibits-suspend` -- this is the direct behavioral
   regression test for `kill_pgroup_and_reap`. The "inhibitor process
   group is torn down (no leaked sh/sleep)" subtest at
   `tests/cli/replace-inhibits-suspend.py:212-239` runs `pgrep -g
   <pgid>` after `SleepInhibitor` drop and asserts the process group
   has zero live members within a 10-second settle window -- exactly
   the behavior the `nix::sys::signal::killpg(Pid::from_raw(pgid),
   SIGKILL)` swap must preserve. Test registered at
   `flake.nix:329-330`. This is the required VM verification for the
   `inhibit.rs` port; `braid-unlock` alone gives only compile coverage
   of `kill_pgroup_and_reap`.
7. **Forced error-return restore:** the existing question-mark-return
   test (line 1466) already covers the `?`-bailout path with echo
   flipped. After the port, that test continues to validate the
   guard's RAII contract on Rust error paths.
8. **Crate version + features:** `cargo tree -p braid-cli --depth 1`
   -- confirm `nix v0.31.3` is the resolved direct dependency. The
   enabled feature set (`fs`, `user`, `term`, `signal`) is sufficient
   for every call site touched by this plan; if compile fails on a
   nix import, the plan's feature list is wrong.
9. **Unsafe audit:** `rg -n 'unsafe\s*\{' cli/src cli/tests` --
   after the port, the only `unsafe` block present anywhere in
   `cli/src/` or `cli/tests/` should be `cli/src/hdparm.rs:38` (the
   `HDIO_DRIVE_CMD` ioctl, deliberately retained -- see "Unsafe
   deliberately left in place"). The diff *should* eliminate every
   other `unsafe` block currently in the tree:

   Production (`cli/src/`):
   - `luks.rs:199, 203, 208` -- `tcgetattr` / `tcsetattr_now` wrappers.
   - `luks.rs:301` -- stdin fd 0 wrapper (`File::from_raw_fd(0)`).
   - `luks.rs:1393, 1403, 1420` -- `open_pty_pair` `libc::openpty` and
     `File::from_raw_fd(master/slave)` test helpers.
   - `confirm.rs:146` -- stdin fd 0 wrapper.
   - `pool_lock.rs:289` -- `File::from_raw_fd(fd)` adapter in
     `open_lock_file`.
   - `main.rs:381` -- `libc::geteuid()` in the root-check guard.
   - `inhibit.rs:36` -- `libc::kill(-pgid, SIGKILL)` block in
     `kill_pgroup_and_reap`.

   Integration tests (`cli/tests/`):
   - `tty_passphrase.rs:61, 71, 76, 83, 88` -- `openpty` /
     `from_raw_fd` / `tcgetattr` / `termios_bytes` helpers.
   - `tty_guard.rs:36` -- `libc::setsid`.
   - `tty_guard.rs:52` -- `libc::dup2(null_fd, STDIN/STDOUT_FILENO)`.
   - `root_check.rs:5` -- `libc::geteuid()` in `is_root`.

   Inspect the `rg` output post-port and confirm only the
   `hdparm.rs:38` block remains, with no `cli/tests/` entries.

**Explicitly out of verification scope:** signal-driven termination
(SIGINT / Ctrl-C, SIGTERM). The existing comment at `luks.rs:216`
documents that `TermiosGuard::drop` runs on normal returns and Rust
unwinds, *not* on process-killing signals. Restoring echo on Ctrl-C
would require a signal handler -- out of scope for this refactor. Do
not add a verification step that depends on echo restoration after
Ctrl-C; the code does not promise it.

## Out of scope

- `cli/src/hdparm.rs` `HDIO_DRIVE_CMD` ioctl -- see "Unsafe
  deliberately left in place" above.
- `cli/src/cmd.rs` signal-name match (`libc::SIGHUP` / etc. as
  integer constants for a Display helper). nix's `Signal::as_str()`
  could replace it but it's cosmetic, not safety; tracked elsewhere
  if at all.
- Touching `online_state.rs`, which already uses nix's typed
  `User`/`Group`/`chown` APIs.
- Removing the `libc` direct dep. `hdparm.rs` still needs raw
  `libc::ioctl`, `inhibit.rs` still uses `libc::pid_t` for the
  `pid_t` cast on `child.id()` inside `Pid::from_raw` (the positive
  pgid handed to `killpg`), and `cmd.rs` still uses `libc::SIG*`
  constants. `libc` stays in `Cargo.toml`.

## Implementation notes

- `cargo update -p nix --precise 0.31.3` re-resolved several target-specific
  `windows-sys` edges from `0.61.2` to `0.60.2`; a follow-up
  `cargo update -p windows-sys@0.60.2 --precise 0.61.2` confirmed current
  crate metadata requires `windows-sys ^0.60` for those edges, so the lockfile
  is left Cargo-generated rather than hand-edited.
