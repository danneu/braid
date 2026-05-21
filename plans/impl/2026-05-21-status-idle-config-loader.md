# Refactor: route Status + Idle config-read failures through `load_config_or_exit`

## Context

`cli/src/main.rs` has a shared `load_config_or_exit(path, exit_code)` helper
(line 1091) that delegates to `print_cli_error` (line 1253). `print_cli_error`
checks for an `error[` prefix so structured error messages (`error[CODE]: ...`)
don't get double-prefixed to `error: error[CODE]: ...`.

Today's `Commands::Status` (`cli/src/main.rs:659-666`) and `Commands::Idle`
(`cli/src/main.rs:791-798`) bypass that helper and inline
`eprintln!("error: {e}"); std::process::exit(N);` on their `config_read` failure
paths. Behaviorally identical right now (the `ConfigError` Display string never
begins with `error[`), but they are drift sites that would silently produce
`error: error[CODE]: ...` the moment `ConfigError` -- or any error type later
returned by `config_read` -- adopts the structured-error form.

The May 21 commit `81843c4 refactor(cli): hoist mutator config loading into
dispatch` swept the *mutating* dispatch arms onto `load_config_or_exit`. The
two remaining inline sites are both read-only arms that were not part of that
sweep. The goal of this refactor is to finish that consolidation so every
config-read failure in the dispatch table flows through the same helper.

Outcome: one drift class eliminated; future structured-error changes apply
uniformly without per-arm audit.

## Scope

Two call sites in `cli/src/main.rs`. No other files change.

| Site | Lines | Current exit code | Replace with |
|---|---|---|---|
| `Commands::Status` | 659-666 | 1 | `load_config_or_exit(Path::new(&config_path), 1)` |
| `Commands::Idle` | 791-798 | 2 | `load_config_or_exit(Path::new(&config_path), 2)` |

Existing helper to reuse (no new code introduced):
- `load_config_or_exit` at `cli/src/main.rs:1091-1099`.

Exit codes must be preserved exactly -- `Status` exits 1, `Idle` exits 2.
These match the original inline values and are part of the observable CLI
contract (notably `idle`'s exit 2 distinguishes config failure from the
`Busy` exit-1 case below it).

## Pattern

Each site replaces the six-line `match config_read(...) { Ok(c) => c, Err(e)
=> { eprintln!("error: {e}"); std::process::exit(N); } }` block with a single
line: `let config = load_config_or_exit(Path::new(&config_path), N);`.

The surrounding code (`RealRunner`, `RealFilesystem`, the subsequent
`cmd_status` / `cmd_idle` call) is untouched.

## Non-goals

- Do not modify `print_cli_error` (line 1253) or `load_config_or_exit`
  (line 1091). They are correct as-is.
- Do not migrate `load_config_for_cmd_or_exit` (line 1103) call sites --
  those intentionally render with a `config error:` prefix for legacy
  reasons and are out of scope.
- Do not touch the unrelated `eprintln!("error: ...")` sites at lines
  437 (help-output write failure) and 482 (root check). Those are not
  config-read paths and do not fit the helper.
- No new helpers. No rename. No doc-comment additions.

## Verification

1. **Build.** `just test-rust` -- compiles the CLI crate and runs Rust unit
   tests. No new tests are required (no behavioral change today; the
   refactor's value is future-proofing, which a test would have to
   contrive a synthetic `ConfigError` to exercise).

2. **Manual stderr check.** With a deliberately broken config (e.g. a
   non-existent path passed via `--config`), confirm `braid status` and
   `braid idle` still emit a single `error: ...` line on stderr and exit
   with codes 1 and 2 respectively. The Display output should match
   what these commands produced before the refactor byte-for-byte,
   because `ConfigError` does not start with `error[`.

3. **No VM tests needed.** None of the existing NixOS VM tests assert on
   the config-read failure stderr for these commands (verified during
   planning). Skipping `just test-vm` is appropriate for a refactor of
   this size and blast radius.
