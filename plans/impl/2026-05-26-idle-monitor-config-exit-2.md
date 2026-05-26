# Pin exit-code 2 on config-load failure for `braid idle` and `braid monitor`

## Context

`braid idle` and `braid monitor` are the systemd/autosuspend-facing health-check
commands. Both return a distinct exit code **2** ("setup error / config load
failure") when their `--config` file is unreadable or unparseable, as opposed to
exit **1** (busy / alert / probe failure). This 2-vs-1 split is a documented
contract:

- `docs/commands/idle.md:36` -- exit 2 = "Setup error -- config could not be read"
- `docs/commands/monitor.md:29` -- exit 2 = "Pre-monitor setup error (e.g. ... config load failure)"
- ADR 016 `docs/design/decisions/016-auto-suspend.md:39` -- enumerates the
  exit-2 -> `!` -> block-suspend (fail-closed) path.

The exit code is set in the dispatch arms by `load_config_or_exit(path, 2)` at
`cli/src/main.rs:781` (idle) and `:853` (monitor); `load_config_or_exit`
(main.rs:1061) prints the error and `std::process::exit`s with that exact code.
**No test pins this.** A refactor that passed `1`, or swapped in the exit-1
loader `load_config_for_cmd_or_exit`, would silently collapse exit 2 into exit 1
and leave the docs wrong with no test failure. (During investigation a careful
reader assumed malformed config yields exit 1 -- which is exactly the regression
this guards.) Severity is low because the autosuspend `!`-inversion blocks
suspend on both 1 and 2, so the regression is diagnostic, not a safety hole --
but the documented contract is currently unprotected.

**Why a VM test, not `cargo test`.** The exit code lives in `main.rs` dispatch.
The root gate (`cli/src/main.rs:476-485`) requires root for `idle`/`monitor` and
runs *before* the config load. The Rust integration harness (`cli/tests/*.rs` via
`CARGO_BIN_EXE_braid`) runs non-root under `just test-rust`, so it would hit the
root gate (exit 1) before reaching the config load -- or be skipped under an
`is_root()` guard (the pattern in `cli/tests/root_check.rs`), giving zero
protection. NixOS VM commands run as root, so they reach the exit-2 path. The two
target VMs already boot for `braid-idle` / `braid-monitor`, so the new subtests
add no VM-boot cost.

## Approach

Add one `with subtest(...)` block to each existing VM test file. Each block
exercises **both** branches of `config_read` (`cli/src/config.rs:153`) and asserts
the exit code is **exactly 2** for each:

- `ConfigError::Parse` -- malformed JSON file (reuse doctor's fixture).
- `ConfigError::Read` -- a nonexistent `--config` path (no fixture needed).

The two branches diverge in `config_read` but reconverge at `load_config_or_exit`;
covering both pins the full "unreadable/unparseable" contract the docs state. A
third "bad schema" case would re-hit the same `Parse` branch, so it is excluded
as redundant.

Each case asserts the exit code is **exactly 2** AND that the captured `2>&1`
output contains the injected config path. The path appears in output only via
`ConfigError::{Read,Parse}` Display (`failed to {read,parse} config file <path>`,
config.rs:141/146), so it proves `config_read` actually ran and disambiguates the
config-load exit-2 from the other exit-2 sources these commands hit *before*
reaching the config load: Clap usage errors (`Cli::parse()` runs first and Clap
exits 2 -- cli/tests/root_check.rs:159) for **both** commands, plus the pool-lock
error (main.rs:1023) for monitor. The injected path is data we control, not
message prose, so the assertions stay structure-insensitive (no error-wording
checks). `2>&1` is required because `print_cli_error` writes to stderr
(main.rs:1253).

### Per-command placement and lock confound

`lock_policy` (`cli/src/main.rs:102`) makes the two commands behave differently
before the config load, which dictates placement:

- **idle** -- `Idle => None` (main.rs:178): takes **no** pool lock, so it has no
  lock-error exit-2 (unlike monitor). But `Cli::parse()` still runs before the
  config load and Clap usage errors also exit 2, so the exit code alone does not
  prove `config_read` ran -- the path-substring assertion (above) is required here
  too. The existing test already runs `braid idle` in a bare VM
  (`tests/cli/braid-idle.py:26`). **Place the new subtest right after the "braid
  idle exits 0 when pool is offline" subtest (~line 28), before pool creation.**
  Assert `status == 2` **and** that the output contains the injected config path.

- **monitor** -- `Monitor => MonitorSilent` (main.rs:164): acquires the pool lock
  *before* the config load, and a lock error **also** exits 2
  (main.rs:1018-1024). A naive subtest placed in a bare VM could exit 2 because
  the lock dir is missing -- passing for the wrong reason without ever reaching
  the config load. **Place the new subtest immediately after the "Healthy pool:
  monitor exits 0" subtest (`tests/cli/braid-monitor.py:61-62`)**, where the lock
  is demonstrably acquirable (the healthy run is the control). Assert `status == 2`
  **and** that the captured output contains the config path string (`/tmp/bad.json`
  or the nonexistent path) -- this is data we injected, not prose, so it stays
  structure-insensitive while ruling out the pool-lock and Clap-usage exit-2s.

### Test body shape

idle (after braid-idle.py:28), reusing the capture idiom from braid-idle.py:75
and the malformed-config fixture from braid-doctor.py:152:

```python
with subtest("braid idle exits 2 on config-load failure (setup error, not exit 1)"):
    # Exit 2 is the documented "config could not be read" contract (idle.md, ADR 016).
    # The path-substring check proves config_read ran: Cli::parse() runs before the
    # config load and Clap usage errors also exit 2, so the exit code alone is not
    # proof (the injected path only appears via ConfigError::{Read,Parse} Display).
    machine.succeed("echo 'not json {{{' > /tmp/bad.json")
    status, output = machine.execute("braid idle --config /tmp/bad.json 2>&1")
    assert status == 2, f"unparseable config must exit 2 (not 1), got {status}: {output}"
    assert "/tmp/bad.json" in output, f"exit 2 must be config-load (not clap usage), got: {output}"
    status, output = machine.execute("braid idle --config /tmp/nonexistent.json 2>&1")
    assert status == 2, f"missing config must exit 2 (not 1), got {status}: {output}"
    assert "/tmp/nonexistent.json" in output, f"exit 2 must be config-load (not clap usage), got: {output}"
```

monitor (after braid-monitor.py:62) -- same two cases, plus the path-substring
assertion to disambiguate from the lock-error exit-2:

```python
with subtest("braid monitor exits 2 on config-load failure (setup error, not lock/alert exit)"):
    # monitor takes the pool lock first (MonitorSilent); lock errors also exit 2.
    # The healthy run above proves the lock is acquirable, so the only changed
    # variable is --config. Assert the error names the config path to confirm
    # exit 2 is the config-load path, not a lock error.
    machine.succeed("echo 'not json {{{' > /tmp/bad.json")
    status, output = machine.execute("braid monitor --config /tmp/bad.json 2>&1")
    assert status == 2, f"unparseable config must exit 2, got {status}: {output}"
    assert "/tmp/bad.json" in output, f"exit 2 must be config-load (not lock), got: {output}"
    status, output = machine.execute("braid monitor --config /tmp/nonexistent.json 2>&1")
    assert status == 2, f"missing config must exit 2, got {status}: {output}"
    assert "/tmp/nonexistent.json" in output, f"exit 2 must be config-load (not lock), got: {output}"
```

### Preambles

Extend each file's file-level `#` preamble (Intent + Scenario) to include the
exit-2 setup-error contract, so the preamble stays honest about what the file
verifies. Per `docs/dev/testing.md`, the preamble is per-file for VM tests;
individual `with subtest(...)` blocks need no separate preamble (a brief inline
comment, as shown above, suffices).

## Files to modify

- `tests/cli/braid-idle.py` -- add the exit-2 config-failure subtest; extend the
  file preamble.
- `tests/cli/braid-monitor.py` -- add the exit-2 config-failure subtest; extend
  the file preamble.

No `flake.nix` change (both `.nix` wrappers are already registered at
flake.nix:878 / :918; adding subtests to existing files needs no new
registration). No Rust/source changes. No doc changes -- the contract is already
documented; this only adds the missing coverage.

## Reuse

- Malformed-config fixture: `tests/cli/braid-doctor.py:152`
  (`echo 'not json {{{' > /tmp/bad.json`) and nonexistent path
  `tests/cli/braid-doctor.py:137`.
- Exact-exit-code capture idiom: `tests/cli/braid-idle.py:75`
  (`status, output = machine.execute(...)`).
- No shared "bad config" helper exists; inline fixtures are the suite norm.

## Verification

1. Focused run of the two touched tests:
   `just test-vm braid-idle braid-monitor` -- both pass, including the new
   subtests.
2. Vacuity / mutation check (recommended -- proves the test catches the exact
   regression): temporarily change `cli/src/main.rs:781` from
   `load_config_or_exit(Path::new(&config_path), 2)` to `..., 1)`, run
   `just test-vm braid-idle`, confirm the new subtest **fails** with the
   "must exit 2 (not 1)" message, then revert.
3. No full-suite run required -- blast radius is two test files. Hand back to the
   user for any broader run.
