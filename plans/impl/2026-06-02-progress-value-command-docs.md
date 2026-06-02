# Fix `--progress` value enum in command docs

## Context

Five braid commands expose a `--progress` flag backed by `ProgressMode`
(`cli/src/progress.rs:57-62`) -- a clap `ValueEnum` with variants
`Auto`/`Always`/`Never` and no `#[value(...)]` renames, so clap renders the
accepted values as `auto`, `always`, `never` and defaults to `auto`. The flag
is wired in two places: `CommonArgs` (`cli/src/main.rs:264-266`), embedded by
`add`/`remove`/`remove-missing`/`replace`; and `RecoverArgs` directly
(`cli/src/main.rs:239-240`), used by `recover`.

The command docs are wrong about this flag in two distinct ways:

- Four pages document a value set the CLI rejects -- `--progress auto|on|off`.
  `on`/`off` never existed; a reader who runs `braid add ... --progress on`
  hits a clap "invalid value" error.
- `recover`'s page omits the flag entirely, even though `recover --progress`
  is real and meaningful -- it governs the post-mount remediation phase, which
  can run for minutes (see the arg's doc comment, `cli/src/main.rs:234-238`).

The `(default: auto)` text in the four existing rows is correct
(`default_value_t = ProgressMode::Auto`) and stays.

Intended outcome: all five command pages document `--progress` with exactly
the values the CLI accepts.

## Scope

Docs only. No code or test changes. (A drift-guard test was considered and
declined -- braid's command docs are maintained as hand-written cookbook/
reference, and keeping them in sync with the CLI is already a documented human
obligation, not an automated one. clap auto-generates the correct
`[possible values: ...]` in `--help`, so the CLI itself is already right.)

## The change

Two kinds of edit.

**(a) Correct the wrong row** in the four files that already have it. Replace:

```
| `--progress auto\|on\|off` | Control progress display (default: auto) |
```

with:

```
| `--progress auto\|always\|never` | Control progress display (default: auto) |
```

All four rows are currently byte-identical, so the edit is the same in each
file (only the line number differs).

**(b) Add the missing row** to `docs/commands/recover.md`. Its Flags table
header is `| Flag | Effect |`; append `--progress` as the last row (matching
the other pages, where it sorts last), after the `--dry-run` row:

```
| `--progress auto\|always\|never` | Control progress display (default: auto) |
```

The description text is kept identical to the other four pages for
cross-command consistency.

## Files to modify

All five command docs whose command exposes `--progress`. Type (a) corrects a
wrong row; type (b) adds a missing row.

- `docs/commands/add.md:71` -- (a)
- `docs/commands/remove.md:40` -- (a)
- `docs/commands/remove-missing.md:56` -- (a)
- `docs/commands/replace.md:78` -- (a)
- `docs/commands/recover.md` -- (b), insert after the `--dry-run` row (~line 65)

Inventory is derived from code, not from current doc hits: `rg 'progress:'
cli/src/main.rs` shows exactly two arg definitions -- `CommonArgs`
(`add`/`remove`/`remove-missing`/`replace`) and `RecoverArgs` (`recover`).
The original four-file scope missed `recover` precisely because it defines the
flag directly instead of embedding `CommonArgs`.

Out of scope (verified): `unlock`/`lock`/`status` have flag tables but their
commands take no `--progress` flag, so they correctly carry no row; `README.md`
does not mention `--progress`. These five files are the complete blast radius.

## Verification

1. All five rows are present and correct, with no stale `on|off`. Scope the
   search to source docs -- `docs/book/` is generated (gitignored,
   `.gitignore:12`) and would add stale built-HTML hits if previously built:
   ```
   rg -- '--progress' docs/commands README.md
   ```
   Expect five hits, all reading `auto\|always\|never`; zero `on\|off`.
   Confirm one of the hits is in `recover.md`.

2. Docs values match the code's rendered enum names (`auto`, `always`,
   `never`) for both arg-definition sites:
   ```
   cargo run -q -p braid-cli -- add --help
   cargo run -q -p braid-cli -- recover --help
   ```
   Both must print `[possible values: auto, always, never]` for `--progress`
   -- the authoritative list the doc rows mirror.

3. Docs still build (no table or cross-link breakage):
   ```
   mdbook build docs
   ```
   `mdbook-linkcheck2` must stay green (this change touches no links).
