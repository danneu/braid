# Plan: collapse the duplicated passphrase-input flag pair into one shared clap struct

## Context

`braid` exposes the same two flags on five commands -- `add`, `replace`,
`unlock`, `enroll`, `recover` -- as `--passphrase-stdin` and
`--passphrase-file`. Today they live as separate fields in four
different clap structs (`RecoverArgs`, `CommonArgs`, `UnlockArgs`,
`EnrollKeyFileArgs`), and none of those structs declare the two flags
as mutually exclusive. When the operator passes both, clap accepts them
silently and `read_passphrase_with` (`cli/src/luks.rs:291-316`)
short-circuits on the file branch -- `--passphrase-stdin` is dropped on
the floor with no warning.

There is a related secondary smell: `CommonArgs` (`cli/src/main.rs:140-156`)
mixes mutation-control flags (`dry_run`, `yes`, `progress`) with the
passphrase-input pair, which is why `remove` and `remove-missing`
currently advertise `--passphrase-stdin` / `--passphrase-file` even
though they never read them. Per the AGENTS.md "no backwards
compatibility" rule for unreleased interfaces, this dead credential
surface should not be carried forward. Trimming it now also makes the
shared struct's purpose unambiguous: it lives only on commands that
genuinely consume a passphrase.

The right fix is structural, not local. The reason the bug exists in
multiple places is that the flag pair has no single owner; if we just
patch each struct in place, the next command that grows a passphrase
input will copy from one of them and have a 50/50 chance of inheriting
the fix. Bundling the pair into one shared `Args` struct that declares
the conflict once -- and flattening it only into the five commands
that actually read it -- eliminates the divergence class entirely. The
pattern already exists in the codebase: `LuksFormatArgs`
(`cli/src/main.rs:158-173`) is a tiny struct that exists only to be
`#[command(flatten)]`'d into `AddArgs` and `ReplaceArgs` for exactly
this reason.

This plan is a clap-surface, call-site, and manual-page refactor; the
behavior of `read_passphrase_with` is unchanged. No unit-test, fixture,
or runbook combines the two flags today (verified by grep across
`cli/`, `tests/`, `manual/`, and `README.md`), so the only behavioral
changes visible to an operator are:

1. Passing both `--passphrase-stdin` and `--passphrase-file` to a
   passphrase-consuming command now exits with a clap usage error
   instead of silently picking the file.
2. `remove` and `remove-missing` no longer advertise or accept the
   passphrase flags. (They were always ignored, so no real workflow
   loses anything; this is the AGENTS.md-mandated "delete the dead
   surface" cleanup.)

## Changes

### 1. New `PassphraseInputArgs` struct (`cli/src/main.rs`)

Add alongside `LuksFormatArgs` (around `cli/src/main.rs:158`):

```rust
/// Bundles the two passphrase-source flags so the stdin/file
/// conflict is declared once and inherited by every command
/// that takes a passphrase.
#[derive(Debug, Args)]
struct PassphraseInputArgs {
    /// Read passphrase from stdin
    #[arg(long)]
    passphrase_stdin: bool,
    /// Read passphrase from file instead of TTY prompt
    #[arg(long, conflicts_with = "passphrase_stdin")]
    passphrase_file: Option<std::path::PathBuf>,
}
```

Module-private (no `pub`) to match `LuksFormatArgs`. The doc-comment
satisfies the AGENTS.md "why it exists at that boundary" rule -- the
boundary is "single source of truth for the conflict declaration".

### 2. Trim `CommonArgs` to mutation controls only

`CommonArgs` (`cli/src/main.rs:140-156`) is left as:

```rust
#[derive(Debug, Args)]
struct CommonArgs {
    /// Show what would happen without executing
    #[arg(long)]
    dry_run: bool,
    /// Skip interactive confirmations
    #[arg(long)]
    yes: bool,
    /// Progress display mode
    #[arg(long, value_enum, default_value_t = ProgressMode::Auto)]
    progress: ProgressMode,
}
```

The two passphrase fields (lines 147-152) are removed. `CommonArgs`
remains `#[command(flatten)]`'d into `AddArgs`, `RemoveArgs`,
`RemoveMissingArgs`, and `ReplaceArgs` exactly as today; only its
field list shrinks.

### 3. Flatten `PassphraseInputArgs` into the five passphrase consumers

Each of the five command-arg structs gains
`#[command(flatten)] passphrase: PassphraseInputArgs,`. For commands
that today inline the two passphrase fields, those inline fields are
deleted. For `add` and `replace`, which today reach passphrase via
`CommonArgs`, the new flatten field sits next to the existing
`#[command(flatten)] common: CommonArgs` -- so each of those structs
ends up with two flatten fields side by side, like
`AddArgs`/`ReplaceArgs` already do for `LuksFormatArgs` + `CommonArgs`.

- `RecoverArgs` (`cli/src/main.rs:110-130`): delete inline pair at
  lines 111-116, add the flatten field. Other fields unchanged.
- `UnlockArgs` (`cli/src/main.rs:243-260`): delete inline pair at
  lines 245-250, add the flatten field. Keep `key_file` and its
  `conflicts_with_all = ["passphrase_stdin", "passphrase_file"]`
  exactly as today -- clap arg IDs come from field names regardless of
  nesting, so the existing string references still resolve to the
  flattened fields.
- `EnrollKeyFileArgs` (`cli/src/main.rs:263-278`): delete inline pair
  at lines 269-274, add the flatten field.
- `AddArgs` (`cli/src/main.rs:175-187`): add the flatten field
  alongside the existing `common` and `luks_format` flattens.
  `CommonArgs` no longer carries passphrase fields, so this is the
  only way `add` keeps its passphrase surface.
- `ReplaceArgs` (`cli/src/main.rs:207-225`): same as `AddArgs`.

`RemoveArgs` (`cli/src/main.rs:189-196`) and `RemoveMissingArgs`
(`cli/src/main.rs:198-205`) get NO flatten field. After the
`CommonArgs` trim above, these commands no longer expose
`--passphrase-stdin` or `--passphrase-file` at all. (Today those flags
parse silently and are then ignored; removing them is the dead-surface
cleanup.)

### 4. Update five call sites

Touch only the field-access expressions; types passed to the
underlying `*_Params` structs are unchanged.

- `cli/src/main.rs:408-409` (Add): `args.common.passphrase_stdin` ->
  `args.passphrase.passphrase_stdin`; same shape for `passphrase_file`.
  (The owner moved from `common` to `passphrase`.)
- `cli/src/main.rs:498-499` (Replace): same `args.passphrase.*` rewrite.
- `cli/src/main.rs:574-575` (Unlock): `args.passphrase_stdin` ->
  `args.passphrase.passphrase_stdin`; same for file.
- `cli/src/main.rs:614-615` (EnrollKeyFile): same as Unlock.
- `cli/src/main.rs:875-876` (Recover): same as Unlock.

`Remove` and `RemoveMissing` call sites need no change -- they never
read passphrase fields and won't have access to them after the
`CommonArgs` trim.

### 5. Regression test (inline `#[cfg(test)] mod tests` in `cli/src/main.rs`)

Append to the existing inline test module at `cli/src/main.rs:941-1031`,
which already follows the `Cli::try_parse_from` + typed-error pattern
(see `add_accepts_repeated_luks_format_arg_values_starting_with_hyphen`
at line 960 and `remove_does_not_accept_luks_format_arg` at line 1011).

`ErrorKind` is already imported on `cli/src/main.rs:1`, so no new
imports are required. Use `ErrorKind::ArgumentConflict` -- clap's stable
typed variant for `--foo cannot be used with --bar` errors -- so the
assertion does not depend on prose wording.

The test is table-driven over the seven argv combinations that should
all be rejected:

```rust
// Intent: clap rejects --passphrase-stdin and --passphrase-file when
//   passed together, and rejects --key-file alongside either of them
//   on `unlock`, on every command that exposes a passphrase input.
// Why it exists: before this fix the file branch in
//   `read_passphrase_with` short-circuited stdin without any
//   user-visible signal, leaving operators unsure which input source
//   they actually fed in. Table-driven so a missed flatten on any of
//   the five passphrase consumers, or a regression in the `unlock`
//   `--key-file` conflict surviving the `PassphraseInputArgs` flatten,
//   fails this test rather than slipping through.
// Scenario: an operator migrates a script from one passphrase input
//   mode to another and forgets to remove the old flag, e.g.
//   `braid unlock --passphrase-stdin --passphrase-file /etc/secret`.
//   Expected: clap usage error, exit 2 (which the binary emits via
//   ErrorKind::ArgumentConflict).
#[test]
fn passphrase_input_conflicts_are_rejected() {
    let cases: &[&[&str]] = &[
        &["braid", "add", "disk1=/dev/disk/by-id/x",
          "--passphrase-stdin", "--passphrase-file", "/dev/null"],
        &["braid", "replace", "--old", "a", "--new", "b=/dev/disk/by-id/x",
          "--passphrase-stdin", "--passphrase-file", "/dev/null"],
        &["braid", "unlock",
          "--passphrase-stdin", "--passphrase-file", "/dev/null"],
        &["braid", "enroll", "/mnt/usb",
          "--passphrase-stdin", "--passphrase-file", "/dev/null"],
        &["braid", "recover",
          "--passphrase-stdin", "--passphrase-file", "/dev/null"],
        &["braid", "unlock",
          "--key-file", "/dev/null", "--passphrase-stdin"],
        &["braid", "unlock",
          "--key-file", "/dev/null", "--passphrase-file", "/dev/null"],
    ];
    for argv in cases {
        let err = Cli::try_parse_from(argv.iter().copied())
            .expect_err(&format!("expected ArgumentConflict for {argv:?}"));
        assert_eq!(
            err.kind(),
            ErrorKind::ArgumentConflict,
            "wrong error kind for {argv:?}: {err}"
        );
    }
}
```

The seven cases lock in:

- All five passphrase consumers reject the stdin/file pair (covers F2's
  "missed flatten on any command" risk).
- `unlock`'s pre-existing `--key-file` vs each passphrase flag conflict
  survives the `PassphraseInputArgs` flatten -- if clap arg ID
  resolution were to regress under nesting, both `--key-file` cases
  would parse cleanly and fail the test.

A second inline test, parallel in shape, locks in the dead-surface
removal so that re-introducing the passphrase fields into `CommonArgs`
or accidentally flattening `PassphraseInputArgs` into
`RemoveArgs`/`RemoveMissingArgs` becomes a test failure rather than
silent regression. The existing `remove_does_not_accept_luks_format_arg`
test at `cli/src/main.rs:1010-1016` is the precedent (asserts
`unexpected argument` for a different flag on `remove`); using
`ErrorKind::UnknownArgument` matches the typed-error style of the
other new test.

```rust
// Intent: `remove` and `remove-missing` reject --passphrase-stdin and
//   --passphrase-file because they never read a passphrase.
// Why it exists: before this refactor, those commands silently
//   accepted the passphrase flags via CommonArgs even though the
//   command bodies ignored them, leaving operators with a dead
//   credential surface (the AGENTS.md "no backwards compatibility"
//   rule says delete this kind of thing on unreleased interfaces).
//   This test fails if a future change re-introduces the passphrase
//   fields into CommonArgs or flattens PassphraseInputArgs into
//   RemoveArgs / RemoveMissingArgs by accident.
// Scenario: an operator running a salvage workflow tries
//   `braid remove disk1 --passphrase-stdin` (perhaps copy-pasted from
//   an `add` invocation). Expected: clap usage error, exit 2,
//   ErrorKind::UnknownArgument; not a silent accept that misleads
//   the operator about which inputs the command actually consumes.
#[test]
fn remove_commands_reject_passphrase_flags() {
    let cases: &[&[&str]] = &[
        &["braid", "remove", "disk1", "--passphrase-stdin"],
        &["braid", "remove", "disk1",
          "--passphrase-file", "/dev/null"],
        &["braid", "remove-missing", "--missing-id", "1",
          "--passphrase-stdin"],
        &["braid", "remove-missing", "--missing-id", "1",
          "--passphrase-file", "/dev/null"],
    ];
    for argv in cases {
        let err = Cli::try_parse_from(argv.iter().copied())
            .expect_err(&format!("expected UnknownArgument for {argv:?}"));
        assert_eq!(
            err.kind(),
            ErrorKind::UnknownArgument,
            "wrong error kind for {argv:?}: {err}"
        );
    }
}
```

No additional test in `cli/tests/root_check.rs` is needed; the
typed-error inline tests are faster than subprocess spawns and give
sharper failure messages.

### 6. Include binary CLI tests in `just test-rust`

The existing `just test-rust` recipe selects `--lib` plus two
integration tests, so inline tests in `cli/src/main.rs` are not part of
that lane. Update the recipe to include the `braid` binary target:

```just
test-rust:
    cargo test --lib --bin braid --test golden_nixos_25_11 --test tty_guard
```

This keeps the new clap regression tests in the default Rust check
instead of relying on a one-off `cargo test -p braid-cli --bin braid`
invocation.

### 7. Update manual pages

The user-facing documentation should reflect the new exclusivity rule.
The existing wording on `manual/commands/unlock.md:57` -- which already
notes that `--key-file` "conflicts with passphrase flags" -- is the
template to mirror.

For each of `manual/commands/{add,replace,unlock,enroll,recover}.md`,
update the option table where `--passphrase-file` is listed so its
description ends with `(conflicts with --passphrase-stdin)`. Specific
table rows to edit:

- `manual/commands/add.md:69` -- `| --passphrase-file <path> | Read passphrase from a file (conflicts with --passphrase-stdin) |`
- `manual/commands/replace.md:80` -- same shape.
- `manual/commands/unlock.md:56` -- same shape; row 57 (`--key-file`)
  is already correct.
- `manual/commands/enroll.md:54` -- same shape.
- `manual/commands/recover.md:62` -- same shape.

Also remove the `--passphrase-stdin` and `--passphrase-file` rows (and
any examples that rely on them) from `manual/commands/remove.md` and
`manual/commands/remove-missing.md` if those pages currently advertise
them. Confirm during execution by grepping
`manual/commands/remove*.md` for `passphrase-` -- the inline grep
during planning showed no matches, but verify against the file at
implementation time in case the manual was updated.

`manual/guides/{troubleshooting,recovery-scenarios}.md` mention the
flags but do not document option semantics; leave them unchanged
unless a concrete example combines the two flags (none does today).

## Critical files

- `cli/src/main.rs` -- new struct (1 add) + 4 struct-edit blocks
  (`RecoverArgs`, `CommonArgs`, `UnlockArgs`, `EnrollKeyFileArgs` lose
  the inline pair) + 2 struct-edit blocks (`AddArgs`, `ReplaceArgs`
  gain a flatten) + 5 call-site renames + 2 inline tests added to the
  existing `mod tests`
  (`passphrase_input_conflicts_are_rejected`,
  `remove_commands_reject_passphrase_flags`).
- `manual/commands/add.md`, `manual/commands/replace.md`,
  `manual/commands/unlock.md`, `manual/commands/enroll.md`,
  `manual/commands/recover.md` -- one option-table edit each.
- `manual/commands/remove.md`, `manual/commands/remove-missing.md` --
  audit for stale passphrase rows and remove if present.
- `justfile` -- include the `braid` binary target in `just test-rust`
  so inline CLI tests are exercised by the normal Rust lane.

`cli/tests/root_check.rs` is unchanged. `cli/src/luks.rs` is
unchanged -- `read_passphrase_with` and its existing unit tests
(`cli/src/luks.rs:2773-3145`) remain authoritative for the
read-side behavior.

## Verification

1. `just test-rust` -- the two new inline binary tests
   (`passphrase_input_conflicts_are_rejected` and
   `remove_commands_reject_passphrase_flags`) must pass, locking in
   both the conflict declaration and the dead-surface removal;
   existing inline tests (`add_*`, `replace_accepts_*`,
   `remove_does_not_accept_luks_format_arg`,
   `luks_format_arg_rejects_space_form_for_hyphen_value`) and the
   `cli/src/luks.rs` passphrase tests must keep passing.
2. `cargo build -p braid-cli` -- catches any missed call-site rename
   (any leftover `args.common.passphrase_stdin` or `args.passphrase_stdin`
   in `main.rs` becomes a compile error after the struct trim).
3. Spot-check the help output by hand (sanity check; the automated
   tests above are the authoritative gates):
   - `braid add --help`, `braid replace --help`, `braid unlock --help`,
     `braid enroll --help`, `braid recover --help` still list both
     passphrase flags.
   - `braid remove --help` and `braid remove-missing --help` no longer
     list `--passphrase-stdin` or `--passphrase-file`.
4. `just test-vm` -- VM tests still pass; none of them combine the
   passphrase flags or pass passphrase flags to `remove` /
   `remove-missing` today, so they should be unaffected. (Cheap
   sanity check; not strictly required by this refactor.)
