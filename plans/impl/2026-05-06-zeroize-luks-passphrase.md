# Plan: zeroize LUKS passphrase plaintext end-to-end

## Context

Today's `read_passphrase` chain returns plain `String`. Only two of
the seven plaintext lifetimes are scrubbed on drop:

- The TTY raw read buffer (`cli/src/luks.rs:129` --
  `Zeroizing<String>`)
- The mount-path resolved credential (`cli/src/mount.rs:80` --
  `OpenCredential::Passphrase(Zeroizing<String>)`)

Everywhere else passphrase plaintext lives in plain heap allocations
that are freed without being overwritten, plus std-owned buffers we
cannot reach. Concrete leaks present today, all confirmed against
the current source:

1. `validate_passphrase` (`cli/src/luks.rs:288-302`) calls
   `raw.trim_end_matches([…]).to_owned()` -- a fresh non-zeroizing
   `String` that briefly holds the plaintext on every read path.
2. The file source buffer
   (`cli/src/luks.rs:266` -- `std::fs::read_to_string`) holds the
   file contents (which include the passphrase) and drops without
   zero.
3. The stdin source buffer
   (`cli/src/luks.rs:307-310` -- `String::new()` + `read_line`) does
   the same.
4. The TTY read at `cli/src/luks.rs:130` runs through
   `std::io::BufReader::new(...)`. The BufReader's internal 8 KiB
   buffer is never zeroized.
5. The production stdin path locks `std::io::stdin()` and reads
   through its global `BufReader<StdinRaw>`. That buffer is owned
   by std (process-lifetime) and is *never* zeroized.
6. **`confirm::confirm_yes()` (`cli/src/confirm.rs:115`) runs before
   the passphrase read on `add` (`add.rs:675`) and `replace`
   (`replace.rs:325`). It uses `std::io::stdin().lock()`, whose
   `read_line` syscall pulls up to the BufReader's full 8 KiB capacity
   from fd 0 in one shot.** If stdin contains
   `yes\nsecret\n`, the BufReader's first read drains both lines
   from fd 0 into its internal buffer. Once that happens:
   - `read_line` returns `"yes\n"` and stashes `"secret\n"` in the
     std BufReader's process-lifetime buffer, where it stays
     un-zeroized for the rest of the process.
   - Any subsequent unbuffered fd-0 read sees EOF, since fd 0 has
     already given all its bytes to the std BufReader. The
     passphrase read fails -- a behavior bug -- and the secret
     cannot be recovered without re-piping.
   So the leak boundary is wider than the passphrase reader: it is
   "every stdin read on the same fd that can precede a passphrase
   on `--passphrase-stdin` flows."
7. `cli/src/add.rs:681`, `cli/src/replace.rs:329`,
   `cli/src/enroll_key_file.rs:462` hold the returned plain `String`
   for the entire command body and drop without zero.
8. `cli/src/recover.rs:1736` and `:2337` wrap the returned plain
   `String` in `Cow::Owned`, which drops without zero.

## Goals

- Every userspace allocation that holds passphrase bytes is owned by
  a `Zeroizing<…>` wrapper from kernel handoff to free.
- No `BufReader`, `std::io::stdin().lock()`, or
  `std::fs::read_to_string` on the passphrase path -- including
  every stdin read on a flow that can also read a passphrase on
  stdin (i.e. `confirm_yes` in `add` / `replace`).
- The raw byte buffer never reallocates: pre-allocated to
  `PASSPHRASE_MAX_BYTES + 1`, hard-capped before each push.
- Stack scratch buffers (`[u8; 1]` for byte reads, `[u8; N]` for
  chunked file reads) are wrapped in `Zeroizing<[u8; N]>` so every
  return path scrubs them.
- Existing semantics preserved: file source rejects multi-line
  files (`read_passphrase_file_embedded_newline_rejected`,
  `cli/src/luks.rs:2018`); stdin / TTY strip a trailing CRLF and
  reject embedded line breaks.
- Subprocess-feeding helpers (`luks_format`, `verify_passphrase`,
  `ensure_luks_open`, `enroll_key_file`,
  `verify_credential_for_targets`) keep their `&str` parameters
  unchanged. `Zeroizing<String>::as_str()` and deref coercion
  handle the new caller types.
- Compile-time return-type signature is the regression guarantee.
  No allocator-hook tests.

## Non-goals

- Page cache: kernel-side copies of the passphrase file are out of
  scope (we cannot zero kernel memory from userspace).
- No new dependency. `zeroize` is already in `Cargo.toml`.
- No change to `Credential::Passphrase(&'a str)` in
  `cli/src/credential_verify.rs:14`.

## Buffer discipline

A single shared constant pins the maximum:

```rust
/// Maximum bytes accepted from any passphrase source.
/// Pre-allocated up front so the raw buffer never reallocates.
/// 64 KiB is generous -- typical LUKS passphrases are <100 bytes;
/// the cap exists so a hostile or accidentally-large file (e.g. a
/// piped binary blob) cannot trigger Vec growth or wedge the
/// process. zeroize's docs explicitly warn that Vec/String growth
/// leaves prior heap copies behind, so the pre-allocation is the
/// core security invariant of this refactor.
const PASSPHRASE_MAX_BYTES: usize = 64 * 1024;
```

Two private byte-level readers in `cli/src/luks.rs`. Both return
`Zeroizing<Vec<u8>>` whose underlying allocation was created at
exactly `PASSPHRASE_MAX_BYTES + 1` capacity and never grows.

```rust
fn read_line_into_zeroizing<R: Read + ?Sized>(
    reader: &mut R,
    source: &str,
) -> Result<Zeroizing<Vec<u8>>, LuksError> {
    let mut zbuf: Zeroizing<Vec<u8>> =
        Zeroizing::new(Vec::with_capacity(PASSPHRASE_MAX_BYTES + 1));
    let mut byte: Zeroizing<[u8; 1]> = Zeroizing::new([0u8; 1]);
    loop {
        let n = reader.read(&mut *byte)?;
        if n == 0 || byte[0] == b'\n' {
            break;
        }
        if zbuf.len() >= PASSPHRASE_MAX_BYTES {
            return Err(LuksError::Validation(format!(
                "passphrase from {source} exceeds {PASSPHRASE_MAX_BYTES} bytes"
            )));
        }
        zbuf.push(byte[0]);
    }
    Ok(zbuf)
}

fn read_file_into_zeroizing(
    path: &Path,
) -> Result<Zeroizing<Vec<u8>>, LuksError> {
    let f = std::fs::File::open(path).map_err(|e| {
        LuksError::Validation(format!(
            "failed to read passphrase file {}: {e}",
            path.display()
        ))
    })?;
    let mut limited = f.take((PASSPHRASE_MAX_BYTES as u64) + 1);
    let mut zbuf: Zeroizing<Vec<u8>> =
        Zeroizing::new(Vec::with_capacity(PASSPHRASE_MAX_BYTES + 1));
    let mut chunk: Zeroizing<[u8; 512]> = Zeroizing::new([0u8; 512]);
    loop {
        let n = limited.read(&mut *chunk).map_err(LuksError::Io)?;
        if n == 0 {
            break;
        }
        zbuf.extend_from_slice(&chunk[..n]);
    }
    if zbuf.len() > PASSPHRASE_MAX_BYTES {
        return Err(LuksError::Validation(format!(
            "passphrase file {} exceeds {PASSPHRASE_MAX_BYTES} bytes",
            path.display()
        )));
    }
    Ok(zbuf)
}
```

Why the buffer never reallocates:

- `Vec::with_capacity(PASSPHRASE_MAX_BYTES + 1)` allocates exactly
  that many bytes upfront.
- `Vec::push` only grows when `len == capacity`; we hard-cap len at
  `PASSPHRASE_MAX_BYTES` before each push.
- `Vec::extend_from_slice` calls `Vec::reserve(slice.len())`, which
  is a no-op when `len + slice.len() <= capacity`. `Read::take(MAX
  + 1)` bounds the cumulative read to MAX + 1 = capacity, so reserve
  is never forced to allocate.
- Both stack scratch buffers are `Zeroizing<[u8; N]>`. zeroize
  provides `impl<const N: usize> Zeroize for [u8; N]`, so any return
  path (`?` propagation, panic unwind) zeroes them.

The shared finalize step:

```rust
fn finalize_passphrase_bytes(
    raw: &[u8],
    source: &str,
) -> Result<Zeroizing<String>, LuksError> {
    // Trim trailing CR and LF (CRLF on stdin/TTY leaves a trailing
    // CR; file source may have a trailing LF).
    let mut end = raw.len();
    while end > 0 && (raw[end - 1] == b'\n' || raw[end - 1] == b'\r') {
        end -= 1;
    }
    let trimmed = &raw[..end];
    let s = std::str::from_utf8(trimmed).map_err(|_| {
        LuksError::Validation(format!(
            "passphrase from {source} is not valid UTF-8"
        ))
    })?;
    if s.is_empty() {
        return Err(LuksError::Validation(format!(
            "passphrase from {source} must not be empty"
        )));
    }
    if s.contains('\n') || s.contains('\r') {
        return Err(LuksError::Validation(format!(
            "passphrase from {source} contains line-break \
             characters -- this passphrase would be impossible to \
             enter interactively"
        )));
    }
    let mut z: Zeroizing<String> =
        Zeroizing::new(String::with_capacity(s.len()));
    z.push_str(s);
    Ok(z)
}
```

`String::with_capacity(s.len()) + push_str(s)` never reallocates.

### File semantics preserved

For the file branch, the entire file is passed to
`finalize_passphrase_bytes`. After the trailing-CR/LF trim, the
embedded-CR/LF check rejects multi-line input -- exactly what
`read_passphrase_file_embedded_newline_rejected` and
`read_passphrase_file_embedded_cr_rejected` pin today. No bytes are
silently dropped.

### Per-branch wiring in `read_passphrase_with_readers`

```rust
fn read_passphrase_with_readers(
    passphrase_file: Option<&Path>,
    passphrase_stdin: bool,
    confirm_new: bool,
    stdin: &mut dyn Read,        // changed from BufRead
    tty: &dyn PassphraseReader,
) -> Result<Zeroizing<String>, LuksError> {
    if let Some(path) = passphrase_file {
        let raw = read_file_into_zeroizing(path)?;
        return finalize_passphrase_bytes(
            &raw,
            &format!("file {}", path.display()),
        );
    }
    if passphrase_stdin {
        let raw = read_line_into_zeroizing(stdin, "stdin")?;
        return finalize_passphrase_bytes(&raw, "stdin");
    }
    let first = tty.read_tty("LUKS passphrase: ")?;
    if !confirm_new {
        return Ok(first);
    }
    let second = tty.read_tty("Confirm LUKS passphrase: ")?;
    check_passphrase_match(first, second)
}
```

The stdin parameter changes from `&mut dyn BufRead` to
`&mut dyn Read`. Tests pass `std::io::Cursor` (also `Read`).
Production wires the unbuffered fd-0 reader described next.

### Boundary expansion: confirm + passphrase share one fd-0 reader

This is the biggest delta from the previous draft. `add` and
`replace` call `confirm::confirm_yes()` before reading the
passphrase. If both reads use stdin, they MUST go through the same
unbuffered reader -- otherwise the std BufReader inside
`confirm_yes` drains the passphrase bytes off fd 0 (see leak #6 in
Context).

Refactor `cli/src/confirm.rs`:

```rust
use std::io::Read;

/// Maximum bytes accepted on the confirmation line before the
/// newline. Users only ever type "yes" / "no" here; the cap exists
/// so that a misdirected pipe (e.g. `printf 'secret\n' | braid add
/// --passphrase-stdin` with the user mistakenly omitting the "yes"
/// line) cannot place an unbounded amount of plaintext into the
/// confirmation buffer.
const CONFIRM_MAX_BYTES: usize = 256;

/// Read a single line and check it equals "yes". Reader bound is
/// `Read` (not `BufRead`) so production can pass an unbuffered
/// File over fd 0 -- BufReader's internal buffer would otherwise
/// pre-drain bytes that a subsequent passphrase read needs. The
/// `+ ?Sized` bound lets the helper accept `&mut dyn Read` as well
/// as concrete readers like `Cursor` and `File`.
///
/// The buffer is `Zeroizing<[u8; CONFIRM_MAX_BYTES]>` because a
/// user (or hostile script) may pipe a passphrase here by mistake.
/// A line longer than the cap is rejected outright -- silently
/// truncating-then-trimming would let `"yes" + N spaces + "no"`
/// false-match `"yes"` after truncation strips the `"no"` tail.
pub fn confirm_yes_from<R: Read + ?Sized>(reader: &mut R) -> Result<(), String> {
    eprint!("Type 'yes' to continue: ");
    let mut buf: Zeroizing<[u8; CONFIRM_MAX_BYTES]> =
        Zeroizing::new([0u8; CONFIRM_MAX_BYTES]);
    let mut len = 0usize;
    let mut byte: Zeroizing<[u8; 1]> = Zeroizing::new([0u8; 1]);
    loop {
        let n = reader.read(&mut *byte).map_err(|e| {
            format!("failed to read confirmation: {e}")
        })?;
        if n == 0 || byte[0] == b'\n' {
            break;
        }
        if len >= CONFIRM_MAX_BYTES {
            // Line exceeds cap; reject without trying to drain more
            // (the process will abort with this error before any
            // subsequent stdin read). No bytes leaked: buf zeroizes
            // on Drop.
            return Err("aborted by user".into());
        }
        buf[len] = byte[0];
        len += 1;
    }
    let input = std::str::from_utf8(&buf[..len])
        .unwrap_or("")
        .trim();
    if input == "yes" {
        Ok(())
    } else {
        Err("aborted by user".into())
    }
}

/// Production confirmation: read from fd 0 directly so we share the
/// unbuffered reader with any subsequent passphrase read on the same
/// fd. ManuallyDrop prevents File's Drop from closing fd 0.
pub fn confirm_yes() -> Result<(), String> {
    use std::os::unix::io::FromRawFd;
    let mut stdin_file = std::mem::ManuallyDrop::new(unsafe {
        std::fs::File::from_raw_fd(0)
    });
    confirm_yes_from(&mut *stdin_file)
}
```

`confirm_yes_from`'s tests pass `Cursor`, which is also `Read`, so
the trait bound change is compatible. Confirm input ("yes") is not
sensitive, but the reader must NOT be a BufReader.

Existing callers of `confirm_yes()` (`add.rs:675`, `replace.rs:325`,
`remove.rs:220`, `remove_missing.rs:194`) need no source changes --
the function signature is unchanged. The byte-by-byte read is
negligible perf cost for confirm input.

### Production passphrase stdin path

```rust
pub fn read_passphrase_with(...) -> Result<Zeroizing<String>, LuksError> {
    if passphrase_file.is_none() && passphrase_stdin {
        use std::os::unix::io::FromRawFd;
        let mut stdin_file = std::mem::ManuallyDrop::new(unsafe {
            std::fs::File::from_raw_fd(0)
        });
        return read_passphrase_with_readers(
            passphrase_file,
            passphrase_stdin,
            confirm_new,
            &mut *stdin_file,
            tty,
        );
    }
    let mut unused_stdin = std::io::Cursor::new(&[][..]);
    read_passphrase_with_readers(
        passphrase_file,
        passphrase_stdin,
        confirm_new,
        &mut unused_stdin,
        tty,
    )
}
```

The `confirm_yes()` reader and the `read_passphrase_with` reader
are independent `File`s over the same fd 0; sequential `read(2)`
syscalls on the kernel-side fd return successive bytes without
interleaving (single-threaded, no concurrent stdin readers). The
test-injection path (`read_passphrase_with_readers` direct calls)
remains free to pass a `Cursor`.

### TTY path

`read_tty_from_file` keeps the termios setup but drops
`std::io::BufReader::new(...)`. After the echo-off guard installs,
it calls `read_line_into_zeroizing(tty, "terminal")` directly on the
`&mut std::fs::File`, then funnels the bytes through
`finalize_passphrase_bytes`. Both the per-byte scratch and the raw
buffer are zeroizing.

## Type changes

| Function | Today | After |
|---|---|---|
| `read_line_into_zeroizing<R: Read + ?Sized>` (new) | -- | `Result<Zeroizing<Vec<u8>>, _>` |
| `read_file_into_zeroizing` (new) | -- | `Result<Zeroizing<Vec<u8>>, _>` |
| `finalize_passphrase_bytes` (new) | -- | `Result<Zeroizing<String>, _>` |
| `read_tty_from_file` (line 118) | `Result<String, _>` | `Result<Zeroizing<String>, _>` |
| `read_passphrase_with_readers` (line 258) | `Result<String, _>` | `Result<Zeroizing<String>, _>` |
| `read_passphrase_with` (line 230) | `Result<String, _>` | `Result<Zeroizing<String>, _>` |
| `read_passphrase` (line 215) | `Result<String, _>` | `Result<Zeroizing<String>, _>` |
| `check_passphrase_match` (line 317) | `(String, String) -> Result<String, _>` | `(Zeroizing<String>, Zeroizing<String>) -> Result<Zeroizing<String>, _>` |
| `PassphraseReader::read_tty` (line 90 trait) | `Result<String, _>` | `Result<Zeroizing<String>, _>` |
| `confirm_yes_from` (`confirm.rs:101`) | `R: BufRead` | `R: Read + ?Sized` |

Drop the redundant outer wrap at `cli/src/mount.rs:80`:
`Ok(OpenCredential::Passphrase(pp))`.

`ScriptedPassphraseReader::read_tty` (line 202) wraps the popped
queue value in `Zeroizing::new`.

`validate_passphrase` (line 288) and `read_passphrase_stdin_from`
(line 307) are deleted -- subsumed by `finalize_passphrase_bytes`
and `read_line_into_zeroizing`.

## Caller updates

`cli/src/add.rs:681`, `cli/src/replace.rs:329`,
`cli/src/enroll_key_file.rs:462` -- no source change. Inferred
local type becomes `Zeroizing<String>` and existing `&passphrase`
arguments resolve to `&str` via deref coercion.

`cli/src/recover.rs` -- replace `Cow<'a, str>` with a small
borrowed-or-owned credential enum so the borrowed arm references
the existing `OpenCredential` passphrase without cloning:

```rust
pub(super) enum RecoverPassphrase<'a> {
    Borrowed(&'a Zeroizing<String>),
    Owned(Zeroizing<String>),
}

impl<'a> RecoverPassphrase<'a> {
    pub(super) fn as_str(&self) -> &str {
        match self {
            Self::Borrowed(z) => z.as_str(),
            Self::Owned(z) => z.as_str(),
        }
    }
}
```

`recover_passphrase` (`:1727`) and `recover_passphrase_for_context`
(`:2327`) return `Result<RecoverPassphrase<'a>, RecoverError>`.

Caller updates in `recover.rs`:

- `:2052-2058` -- `passphrase: Option<RecoverPassphrase<'_>>`. Read
  out as `passphrase.as_ref().map(|p| p.as_str())` for the
  `&str`-expecting `ensure_luks_open` call.
- `:2073-2074` -- `passphrase.as_ref()` becomes `passphrase.as_str()`.
- `:2424-2444` -- same.

## Test surface

`zeroize` 1.8.2 derives `PartialEq` for `Zeroizing<Z>` against
`Zeroizing<Z>` only -- not `Zeroizing<String> == &str`. Existing
tests that do `assert_eq!(result, "literal")` against a
`Zeroizing<String>` return value will fail to compile.

Mechanical updates:

- `read_passphrase_*` tests at `cli/src/luks.rs:1973-2308` (~13
  sites): change every `assert_eq!(result, "literal")` to
  `assert_eq!(result.as_str(), "literal")`.
- `check_passphrase_match_*` tests at
  `cli/src/luks.rs:2118-2161`: `"secret".into()` will not infer to
  `Zeroizing<String>` (no `From<&str>` impl). Wrap each input with
  `Zeroizing::new(String::from("..."))` (4 tests x 2 args = 8
  callsites). Success-path equality becomes
  `assert_eq!(got.as_str(), "secret")`.
- `read_passphrase_stdin_from_*` tests (`luks.rs:2174-2210`)
  reference a function that is being deleted. Port them to the full
  `read_passphrase_with_readers(None, true, false, &mut cur, &tty)`
  stdin branch -- which exercises the line read AND finalization
  AND returns `Zeroizing<String>` -- preserving ok / empty-rejected
  / CRLF-strip coverage end-to-end. Asserts on `result.as_str()`.
- `confirm_yes_from` tests (`confirm.rs:177-200`) -- bound change
  from BufRead to Read is transparent (Cursor implements both); no
  source change. Add two new tests covering the
  zeroizing-fixed-cap behavior (see below).
- `ScriptedPassphraseReader::new(["pass"])` at the add-flow tests
  is unchanged -- `S: Into<String>` is unaffected.

Add four new unit tests (oversized-stdin, oversized-file, two for
`confirm_yes_from`) plus a NixOS VM regression for the
confirm-plus-passphrase contract:

```rust
/*
 * Intent: a piped passphrase exceeding PASSPHRASE_MAX_BYTES is
 *   rejected before the line buffer would have grown.
 * Why: the core security guarantee of this refactor is that the
 *   raw passphrase buffer never reallocates. A regression that
 *   removed the pre-allocation or the cap check would re-introduce
 *   "Vec growth leaves prior heap copies behind."
 * Scenario: operator pipes a hostile or accidentally-large blob
 *   to `--passphrase-stdin`.
 */
#[test]
fn read_passphrase_stdin_rejects_oversized() {
    let huge = vec![b'x'; PASSPHRASE_MAX_BYTES + 1];
    let mut cur = std::io::Cursor::new(huge);
    let tty = ScriptedPassphraseReader::new(Vec::<String>::new());
    let err = read_passphrase_with_readers(None, true, false, &mut cur, &tty)
        .unwrap_err();
    match err {
        LuksError::Validation(msg) => assert!(
            msg.contains("exceeds"),
            "expected 'exceeds' in: {msg}"
        ),
        other => panic!("expected Validation, got {other:?}"),
    }
}

/*
 * Intent: a passphrase file exceeding PASSPHRASE_MAX_BYTES is
 *   rejected, exercising the same buffer cap on the file branch.
 * Why: the file reader has its own cap path (Read::take + len
 *   check after the loop). Without a dedicated test, a future
 *   regression could leave read_file_into_zeroizing unbounded
 *   while the stdin oversized test still passes.
 * Scenario: operator points `--passphrase-file` at a large file
 *   by mistake (e.g. a binary keyfile).
 */
#[test]
fn read_passphrase_file_rejects_oversized() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&vec![b'x'; PASSPHRASE_MAX_BYTES + 1]).unwrap();
    file.flush().unwrap();
    let err = read_passphrase(Some(file.path()), false).unwrap_err();
    match err {
        LuksError::Validation(msg) => assert!(
            msg.contains("exceeds"),
            "expected 'exceeds' in: {msg}"
        ),
        other => panic!("expected Validation, got {other:?}"),
    }
}
```

In `cli/src/confirm.rs`'s test module, add:

```rust
/*
 * Intent: a confirmation line exceeding CONFIRM_MAX_BYTES is
 *   rejected outright -- not silently truncated and trimmed.
 * Why: an earlier draft truncated input at the cap and trimmed
 *   the truncated bytes; "yes" + (CONFIRM_MAX_BYTES - 3) spaces +
 *   "no\n" then false-matched "yes" because trim stripped the
 *   trailing whitespace inside the truncated window. Reject-
 *   instead-of-truncate eliminates that class of bug.
 * Scenario: hostile or accidentally-large input on the confirm
 *   prompt (e.g. user pastes a long string).
 */
#[test]
fn confirm_rejects_overlong_line() {
    let line: Vec<u8> = std::iter::repeat(b' ')
        .take(CONFIRM_MAX_BYTES + 1)
        .chain([b'\n'])
        .collect();
    let mut input = std::io::Cursor::new(line);
    let err = confirm_yes_from(&mut input).unwrap_err();
    assert_eq!(err, "aborted by user");
}

/*
 * Intent: pin the truncate-collision regression directly: input
 *   `"yes" + N spaces + "no\n"` (where N pushes total length past
 *   the cap) must reject, not accept.
 * Why: the precise false-match the reviewer flagged.
 * Scenario: hostile input designed to exploit an old truncating
 *   confirm reader.
 */
#[test]
fn confirm_rejects_yes_with_trailing_garbage_past_cap() {
    let mut line: Vec<u8> = b"yes".to_vec();
    line.extend(std::iter::repeat(b' ').take(CONFIRM_MAX_BYTES));
    line.extend(b"no\n");
    let mut input = std::io::Cursor::new(line);
    let err = confirm_yes_from(&mut input).unwrap_err();
    assert_eq!(err, "aborted by user");
}
```

A unit test that uses a single `Cursor` does not catch the
production bug being fixed: the bug only manifests when stdin is a
real file descriptor whose first read by std's BufReader can drain
multiple lines off fd 0. A `Cursor` is unbuffered and would still
satisfy a regressed `confirm_yes()` that re-introduced
`stdin().lock()`. The behavioral pin therefore lives in a NixOS VM
regression test, not a unit test.

Add `tests/cli/confirm-then-passphrase-on-stdin.{nix,py}`,
modeled on `tests/cli/add-passphrase-mismatch.py`:

```python
# Test: confirm prompt and passphrase share one stdin stream
#
# Intent:
#   `braid add --passphrase-stdin` (without `--yes`) consumes
#   "yes\n" for the confirm prompt and "secret\n" for the
#   passphrase read from a single piped stdin. Both reads must see
#   their own line; neither may swallow bytes meant for the other.
#
# Why it exists:
#   Prior to the zeroizing-passphrase refactor, `confirm_yes()` used
#   `std::io::stdin().lock()` whose BufReader can drain up to 8 KiB
#   from fd 0 in one syscall. With `printf 'yes\nsecret\n' | braid
#   add ... --passphrase-stdin`, the BufReader stashed "secret\n"
#   in its process-lifetime internal buffer and the subsequent
#   unbuffered passphrase read on fd 0 saw EOF. That broke the
#   command and left the secret un-zeroized in std's buffer.
#
# Scenario:
#   Operator pipes both confirmation and passphrase on stdin
#   (interactive prompt automated by a script).

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

# No --yes flag: the confirm prompt MUST run and read "yes\n".
add_cmd = (
    "printf 'yes\\nsecret\\n' | "
    "braid add --luks-format-arg=--pbkdf "
    "--luks-format-arg=pbkdf2 "
    "--luks-format-arg=--pbkdf-force-iterations "
    "--luks-format-arg=1000 "
    "disk1=/dev/disk/by-id/virtio-disk1 "
    "--passphrase-stdin"
)
machine.succeed(add_cmd)
# A second add proves the same passphrase was actually set on the
# first disk (otherwise verify_credential_for_targets would reject).
add_cmd2 = add_cmd.replace("disk1", "disk2")
machine.succeed(add_cmd2)

fi_show = machine.succeed("btrfs fi show /mnt/storage")
for name in ["braid-disk1", "braid-disk2"]:
    assert "/dev/mapper/" + name in fi_show, name + " missing"
```

Register the test in `flake.nix`'s `checks` table per the rule in
`docs/testing.md`. A second NixOS regression for `replace` follows
the same shape -- pipe `yes\nsecret\n` to `braid replace
--passphrase-stdin`, assert the replace completes.

The compile-time signature on `read_line_into_zeroizing` (returning
`Zeroizing<Vec<u8>>` from a `Vec::with_capacity(MAX + 1)`
allocation) is the regression guarantee for buffer reallocation;
the unit-level oversized tests pin the cap behavior; the VM
regression pins the confirm-plus-passphrase fd-0 contract under
real kernel stdin.

## Files modified

- `cli/src/luks.rs` -- new `PASSPHRASE_MAX_BYTES`,
  `read_line_into_zeroizing`, `read_file_into_zeroizing`,
  `finalize_passphrase_bytes`. Rewrites of `read_tty_from_file`,
  `read_passphrase_with_readers` (file + stdin branches),
  `read_passphrase_with` (production stdin via fd-0 File). Type
  changes on `PassphraseReader`, `ScriptedPassphraseReader`,
  `check_passphrase_match`. Deletion of `validate_passphrase` and
  `read_passphrase_stdin_from`. Test assertions updated to
  `.as_str()` and `Zeroizing::new(String::from(...))`. New
  oversized-stdin and oversized-file unit tests.
- `cli/src/confirm.rs` -- new `CONFIRM_MAX_BYTES = 256`;
  `confirm_yes_from` bound changes from `BufRead` to
  `Read + ?Sized` and reads byte-by-byte into a
  `Zeroizing<[u8; CONFIRM_MAX_BYTES]>`; lines longer than the cap
  reject outright (no truncate-then-trim). `confirm_yes()` opens
  fd 0 as a `ManuallyDrop<File>` so it shares an unbuffered reader
  with the subsequent passphrase read. New
  `confirm_rejects_overlong_line` and
  `confirm_rejects_yes_with_trailing_garbage_past_cap` tests.
- `cli/src/mount.rs` -- drop the redundant outer
  `Zeroizing::new(pp)` at line 80.
- `cli/src/add.rs` -- no source change; inferred local type at
  `:681` becomes `Zeroizing<String>`.
- `cli/src/replace.rs` -- same at `:329`.
- `cli/src/enroll_key_file.rs` -- same at `:462`.
- `cli/src/recover.rs` -- new `RecoverPassphrase<'a>` enum;
  `recover_passphrase` and `recover_passphrase_for_context` return
  it; three call-site updates from `Cow::as_ref()` to
  `.as_str()` / `.as_ref().map(|p| p.as_str())`.
- `tests/cli/confirm-then-passphrase-on-stdin.{nix,py}` -- new
  NixOS VM regression covering the confirm-plus-passphrase
  contract with `printf 'yes\nsecret\n' | braid add ...
  --passphrase-stdin`. Companion `replace` regression in the same
  test or a sibling pair.
- `flake.nix` -- register the new VM check(s) per
  `docs/testing.md`.

## Verification

1. `just test-rust` -- existing tests pass with updated assertions;
   the two new unit tests (stdin oversized, file oversized) pin the
   buffer-cap contract.
2. `cargo check -p braid-cli` -- compile-time pin: trait and every
   caller agree on `Zeroizing<String>`.
3. `just test-vm confirm-then-passphrase-on-stdin` -- the new VM
   regression must pass; it exercises the real fd-0 +
   confirm-then-passphrase pipeline that no unit test can simulate.
4. `git grep -nE 'String\b' cli/src/luks.rs` -- manual audit. The
   only `String` types on a passphrase path should be inside
   `Zeroizing<…>` or `LuksError::Validation(String)`.
5. `git grep -nE 'BufReader|read_to_string|stdin\(\)\.lock' cli/src`
   -- confirm none remain on the passphrase or pre-passphrase
   confirm path.
6. `just test-vm` -- exercise the full unlock / add / replace /
   enroll-key-file / recover paths end-to-end to confirm no
   behavior regression under real systemd / kernel stdin.
