# Colorize Interactive StatusTag Output

## Context

The CLI uses `StatusTag` (ok/warn/fail/skip) to prefix per-item status rows in `lock`, `mount`, `doctor`, and -- via the shared `preview.rs` layer -- `unlock` and `recover`. Rows scan poorly in a terminal because every line starts with an identically-shaped monochrome tag. Adding color to the tag itself, only when the destination stream is an interactive TTY and `no_color_active` is false (see constraint below), makes outcomes legible without touching any contractually-stable byte stream (JSON output, dry-run previews captured to files, TUI).

Constraints:

- JSON output stays byte-stable.
- `Preview::render` and existing pure renderers stay byte-stable on their current public signatures; color is opt-in via new `*_with(color_enabled)` companions.
- Only the 6-column tag gets wrapped; row body is unchanged so column alignment survives ANSI stripping.
- No new CLI flag, no new dependency, no TUI changes.
- `NO_COLOR` disables color when set to a non-empty value (per https://no-color.org). `NO_COLOR` unset or set to the empty string does not disable color.

## Scope

**Policy:** color every bracketed `StatusTag` tag that flows through the shared `preview.rs` seam or through the few direct-`eprintln!` subsystems, whenever the destination stream is an interactive TTY and `no_color_active` is false (i.e. `NO_COLOR` is unset or empty). Pick the boundary by the *rendering layer*, not a hand-curated command list -- the preview seam is the right abstraction, and every caller of it should behave the same way.

**In (preview seam, stdout via `Preview::print` dry-run):** `lock`, `unlock`, `recover`, `add`, `remove`, `remove-missing`, `replace`, `enroll_key_file`.

**In (preview seam, stderr via `render_notes_for_stderr` real-run prelude and failure-path):** `unlock`, `recover`, `add`, `remove`, `remove-missing`, `replace`, `enroll_key_file`. For `enroll_key_file` the per-disk lines use `PerDiskStyle::Plain` (`skip: <name> ...`, no brackets, no `StatusTag`), so colorization is a no-op for per-disk rows there; a `[warn]` tag that fires on a `PreviewNote::Warn` in the same output still gets colored, which is consistent with every other command and matches the universal seam policy. `replace`'s `STDERR_STYLE` is `Bracketed`, so its prelude + failure path render `StatusTag` bytes through the seam just like `lock`/`add`/`remove`.

**In (direct-eprintln! non-preview sites):**
- `cmd_doctor` human output (stdout).
- `LockPlan::execute` direct stderr rows (~13 sites) plus the retry notice in `close_mapper_with_retry`.
- `mount.rs` direct stderr rows in the key-file unlock branch, `open_disks_with_passphrase`, and shared `scan_and_mount` (`cli/src/mount.rs:557`, `:690`, `:744`).

**Out:** Dry-run risk tags (`[safe]`/`[destructive]` in `cmd::Step::render_dry_run`), narrative lines (`error:`, `warning:`, `Done.`, `LUKS opened:`), TUI, JSON output, `--color` flag, `CLICOLOR_FORCE`.

## Design

### The primitive seam (`cli/src/status_tag.rs`)

Remove `impl fmt::Display for StatusTag` and its `as_label` helper. The compiler then rejects any `"{}"` format of a `StatusTag`, forcing every site to pick a color mode. Add pure render/gate helpers plus thin stream-boundary helpers:

```rust
pub enum StatusTag { Ok, Warn, Fail, Skip }   // unchanged

pub fn render_status_tag(tag: StatusTag, color_enabled: bool) -> &'static str {
    match (tag, color_enabled) {
        (StatusTag::Ok,   false) => "[ok  ]",
        (StatusTag::Warn, false) => "[warn]",
        (StatusTag::Fail, false) => "[fail]",
        (StatusTag::Skip, false) => "[skip]",
        (StatusTag::Ok,   true)  => "\x1b[32m[ok  ]\x1b[0m",
        (StatusTag::Warn, true)  => "\x1b[33m[warn]\x1b[0m",
        (StatusTag::Fail, true)  => "\x1b[31m[fail]\x1b[0m",
        (StatusTag::Skip, true)  => "\x1b[90m[skip]\x1b[0m",
    }
}

pub fn should_color_status_tags(is_terminal: bool, no_color_active: bool) -> bool {
    is_terminal && !no_color_active
}

/// Pure env-value parser for `NO_COLOR`. Matches no-color.org: a
/// non-empty value disables color; unset or empty does not.
pub fn no_color_active_from_env(value: Option<&std::ffi::OsStr>) -> bool {
    matches!(value, Some(v) if !v.is_empty())
}

pub fn color_enabled_for_stdout() -> bool {
    should_color_status_tags(
        std::io::stdout().is_terminal(),
        no_color_active_from_env(std::env::var_os("NO_COLOR").as_deref()),
    )
}

pub fn color_enabled_for_stderr() -> bool {
    should_color_status_tags(
        std::io::stderr().is_terminal(),
        no_color_active_from_env(std::env::var_os("NO_COLOR").as_deref()),
    )
}
```

`status_tag.rs` imports `std::io::IsTerminal` for the two stream helpers. Every call site uses `color_enabled_for_stdout()` or `color_enabled_for_stderr()` at the rendering boundary, not an inlined `IsTerminal` + `NO_COLOR` expression. Keeping the stream choice named at the call site reduces drift, and keeping the env parse in one place means the `!is_empty()` rule remains unit-testable without mutating process env.

Update `status_tag_pins_four_known_levels` to assert via `render_status_tag(..., false)`.

Colors: ok=green(32), warn=yellow(33), fail=red(31), skip=grey/bright-black(90). Always terminate with `\x1b[0m`.

### The shared renderer seam (`cli/src/preview.rs`)

`preview.rs` is the load-bearing seam. Every public render helper grows a color-aware companion; the existing public fn stays as the plain wrapper.

- `format_per_disk_line(name, level, message, style)` -> add `color_enabled: bool` param. Replaces `level.to_status_tag()` stringification (currently relying on `Display`) with `render_status_tag(level.to_status_tag(), color_enabled)`. Made private if not already.
- `render_per_disk_notes(notes, style)` -> `render_per_disk_notes_with(notes, style, color_enabled)`. Plain fn delegates `false`.
- `render_notes_for_stderr(notes, style)` -> `render_notes_for_stderr_with(notes, style, color_enabled)`. Plain fn delegates `false`. The hardcoded `[warn]` branches (`preview.rs:143` and `:179`) must go through `render_status_tag(StatusTag::Warn, color_enabled)`.
- `Preview::render(&self)` -> `Preview::render_with(&self, color_enabled)`. Plain `render` delegates `false`.
- `Preview::print(&self)` stays plain (used by tests and any caller that wants byte-stable plain). Add `Preview::print_colored(&self)` that calls `color_enabled_for_stdout()` and then `render_with`.

Every existing `preview.rs` test continues to exercise the plain path via the unchanged public APIs; color gets its own tests (see "Test plan").

### Direct-eprintln! subsystems

**`cli/src/doctor.rs`**
- `format_doctor_human(report) -> String` becomes `format_doctor_human_with(report, false)`.
- `format_doctor_human_with(report, color_enabled)` uses `render_status_tag(tag, color_enabled)` at line 919.
- `cmd_doctor(..., json)`: in the non-JSON branch, compute `color_enabled` with `color_enabled_for_stdout()` and call `format_doctor_human_with`. JSON branch unchanged.

**`cli/src/lock.rs`**
- `LockPlan::execute`: compute `color_enabled` once at the top with `color_enabled_for_stderr()`, bind a local closure `let tag = |t| render_status_tag(t, color_enabled);`, then replace every `StatusTag::X` in the ~13 `eprintln!` sites (lines 236, 259, 262, 266, 283, 291, 308, 313, 317, 325, 341, 345, 350, 354) with `tag(StatusTag::X)`. Pass `color_enabled` into `close_mapper_with_retry` (grows a `bool` param alongside the existing `sleeper`).
- `cmd_lock_impl` dry-run branch (line 462): `plan.preview().print()` -> `plan.preview().print_colored()`.
- `close_mapper_with_retry` at line 71/75: use the new `color_enabled` param.

**`cli/src/mount.rs`**
- Keep the public execute functions as the stream-boundary wrappers. `execute_mount_only` computes `color_enabled_for_stderr()` once and threads the bool into `scan_and_mount`. `execute_unlock_and_mount` computes the same bool once and threads it through every private helper/direct row in that path: the key-file unlock row in `execute_unlock_and_mount`, `open_disks_with_passphrase`, and `scan_and_mount`.
- Update `open_disks_with_passphrase(..., color_enabled)` and `scan_and_mount(..., color_enabled)` so the direct rows currently around `:557`, `:690`, and `:744` all use `render_status_tag(StatusTag::Ok, color_enabled)`. This matters because `scan_and_mount` is shared by both `execute_mount_only` and `execute_unlock_and_mount`; a local inline check inside only one public entry point would miss mount-only or duplicate policy.
- `render_probe_events` currently delegates to `preview::render_notes_for_stderr`; keep that delegation plain (byte-stable). `print_probe_events` calls `color_enabled_for_stderr()` and then `render_notes_for_stderr_with` -- this covers `unlock.rs:47`, `recover.rs:177/240/702` transparently.

**Preview-seam callers (all share the same two-line migration pattern)**

For every command that renders via the preview seam, migrate each site:

- `plan.preview().print()` -> `plan.preview().print_colored()`.
- `preview::render_notes_for_stderr(notes, STYLE)` -> compute `color_enabled` at that site with `color_enabled_for_stderr()`, then `preview::render_notes_for_stderr_with(notes, STYLE, color_enabled)`.

Sites to migrate (verified against current source with `rg "plan.preview\(\).print\|render_notes_for_stderr\(&"`):

| File                              | Site                                       | Call                           |
| --------------------------------- | ------------------------------------------ | ------------------------------ |
| `cli/src/unlock.rs:86`            | `UnlockPlan::execute` prelude              | `render_notes_for_stderr_with` |
| `cli/src/unlock.rs:210`           | `cmd_unlock` failure path                  | `render_notes_for_stderr_with` |
| `cli/src/unlock.rs:217`           | dry-run                                    | `print_colored`                |
| `cli/src/recover.rs:211`          | `RecoverPlan::execute` prelude             | `render_notes_for_stderr_with` |
| `cli/src/recover.rs:562`          | `cmd_recover` failure path                 | `render_notes_for_stderr_with` |
| `cli/src/recover.rs:569`          | dry-run                                    | `print_colored`                |
| `cli/src/add.rs:362`              | `AddPlan::execute` prelude                 | `render_notes_for_stderr_with` |
| `cli/src/add.rs:864`              | `cmd_add` failure path                     | `render_notes_for_stderr_with` |
| `cli/src/add.rs:871`              | dry-run                                    | `print_colored`                |
| `cli/src/remove.rs:125`           | `RemovePlan::execute` prelude              | `render_notes_for_stderr_with` |
| `cli/src/remove.rs:369`           | `cmd_remove` failure path                  | `render_notes_for_stderr_with` |
| `cli/src/remove.rs:376`           | dry-run                                    | `print_colored`                |
| `cli/src/remove_missing.rs:128`   | `RemoveMissingPlan::execute` prelude       | `render_notes_for_stderr_with` |
| `cli/src/remove_missing.rs:380`   | `cmd_remove_missing` failure path          | `render_notes_for_stderr_with` |
| `cli/src/remove_missing.rs:387`   | dry-run                                    | `print_colored`                |
| `cli/src/enroll_key_file.rs:379`  | `EnrollPlan::execute` prelude (Plain)      | `render_notes_for_stderr_with` |
| `cli/src/enroll_key_file.rs:522`  | `cmd_enroll_key_file` failure (Plain)      | `render_notes_for_stderr_with` |
| `cli/src/enroll_key_file.rs:529`  | dry-run                                    | `print_colored`                |
| `cli/src/replace.rs:430`          | `emit_replace_notes_to_stderr` wrapper     | split into a pure `render_replace_notes_for_stderr(notes, color_enabled) -> String` helper + a thin wrapper that calls `color_enabled_for_stderr()` and then routes through the existing `#[cfg(test)]` capture hook. Both callers at `:128` and `:683` pick up color automatically. |
| `cli/src/replace.rs:688`          | `cmd_replace` dry-run                      | `print_colored`                |
| `cli/src/lock.rs:462`             | `cmd_lock_impl` dry-run                    | `print_colored`                |

Re-run the grep at implementation time and sweep any sites that have shifted since this plan was written.

### Stream-to-TTY table

| Call site                                                               | Stream polled |
| ----------------------------------------------------------------------- | ------------- |
| `cmd_doctor` human output                                               | `stdout`      |
| `Preview::print_colored` (all dry-run previews)                         | `stdout`      |
| `print_probe_events`                                                    | `stderr`      |
| `UnlockPlan::execute` prelude, `cmd_unlock` failure                     | `stderr`      |
| `RecoverPlan::execute` prelude, `cmd_recover` failure                   | `stderr`      |
| `AddPlan::execute` prelude, `cmd_add` failure                           | `stderr`      |
| `RemovePlan::execute` prelude, `cmd_remove` failure                     | `stderr`      |
| `RemoveMissingPlan::execute` prelude, `cmd_remove_missing` failure      | `stderr`      |
| `emit_replace_notes_to_stderr` (replace prelude + failure)              | `stderr`      |
| `EnrollPlan::execute` prelude, `cmd_enroll_key_file` failure (Plain)    | `stderr`      |
| `LockPlan::execute` live rows                                           | `stderr`      |
| `close_mapper_with_retry` retry notice                                  | `stderr`      |
| `mount.rs` execute helpers/direct rows (`open_disks_with_passphrase`, key-file unlock row, `scan_and_mount`) | `stderr`      |

`is_terminal` via `std::io::IsTerminal` (already used in `cli/src/main.rs:295`, `cli/src/progress.rs:106`). No new dependency.

### Files touched

- `cli/src/status_tag.rs` -- add `render_status_tag`, `should_color_status_tags`, `no_color_active_from_env`, `color_enabled_for_stdout`, and `color_enabled_for_stderr`; remove `Display` impl; update the existing test.
- `cli/src/preview.rs` -- color-aware `*_with` companions on `format_per_disk_line` (private), `render_per_disk_notes`, `render_notes_for_stderr`, `Preview::render`; add `Preview::print_colored`.
- `cli/src/doctor.rs` -- `format_doctor_human_with`, thread color through `cmd_doctor` human branch.
- `cli/src/lock.rs` -- color-aware `LockPlan::execute` (closure + thread into `close_mapper_with_retry`), `print_colored` on dry-run branch.
- `cli/src/mount.rs` -- public execute wrappers compute color once and thread it through `open_disks_with_passphrase` / `scan_and_mount`; `print_probe_events` uses the stderr helper.
- `cli/src/unlock.rs`, `cli/src/recover.rs`, `cli/src/add.rs`, `cli/src/remove.rs`, `cli/src/remove_missing.rs`, `cli/src/replace.rs`, `cli/src/enroll_key_file.rs` -- migrate each preview-seam site per the table above.

## Test plan

Unit tests live next to each module. Every new `*_with` companion gets behavior-locked for the colored path; every existing pin test must pass unchanged on the plain path.

**`status_tag.rs`**

1. `render_status_tag` pins all 8 outputs exactly (4 plain + 4 colored ANSI-wrapped literals).
2. **Alignment invariant:** for each variant, stripping `\x1b\[[0-9;]*m` from `render_status_tag(tag, true)` equals `render_status_tag(tag, false)` byte-for-byte.
3. `should_color_status_tags` truth table: `(true, false) -> true`, `(false, false) -> false`, `(true, true) -> false`, `(false, true) -> false`. Plus a second test pinning `no_color_active_from_env`: `None -> false`, `Some("") -> false`, `Some("1") -> true`, `Some("0") -> true`, `Some("false") -> true` (any non-empty value disables). This is a pure-function test that needs no env mutation; it catches a regression where the `!is_empty()` rule is dropped or inverted at the seam.

**`preview.rs`**

4. All existing preview tests keep exercising the plain `Preview::render` / `render_notes_for_stderr` / `render_per_disk_notes` paths -- unchanged.
5. New: `render_notes_for_stderr_with(notes, Bracketed, true)` on the existing mixed-notes fixture (Info + PerDisk + Warn) asserts the expected string where each `[warn]` and each per-disk tag is ANSI-wrapped and every Info/message body is byte-identical to the plain form.
6. New: `Preview::render_with(true)` on the Warn+Step fixture (`render_emits_notes_before_steps`) asserts the `[warn]` is ANSI-wrapped and the step block (which is not in scope) is byte-identical.

**`mount.rs`**

7. Existing `render_probe_events_formats_mixed_probe_result` keeps pinning the plain form -- unchanged.

**`doctor.rs`**

8. Existing plain-format tests keep calling `format_doctor_human` -- unchanged.
9. New: `format_doctor_human_with(report, true)` for a minimal fixed report asserts each row's tag is ANSI-wrapped and the label + message columns are byte-identical.

**`lock.rs`**

10. Existing `plan.preview().render()` tests stay on the plain path -- unchanged.

**`replace.rs`**

10a. New: unit-test the extracted `render_replace_notes_for_stderr(notes, color_enabled)` helper with an `Info + Warn` fixture (replace's `require_mutation_preflight` path at `cli/src/preflight.rs:420` can emit both, and replace consumes them at `cli/src/replace.rs:556`). Assert: in plain mode the Info renders unadorned and the Warn renders `"[warn]  <body>\n"`; in colored mode the Info is byte-identical to plain (no ANSI around it), only the `[warn]` is yellow-wrapped, and the Warn body is byte-identical to plain. This pins both the "only the tag is colored" invariant and the preview-seam stderr wiring, which the `replace-preview-warnings` VM test cannot exercise (its success-path stderr is expected to be empty; see test 20).

The removed `Display` impl is the structural gate: every call site must choose a color mode or the code does not compile.

**VM-level non-TTY behavioral coverage (redirected streams)**

NixOS VM tests run commands without a PTY, so captured stdout/stderr is the non-TTY path. Existing `"[ok" in output` substrings accidentally match ANSI-wrapped forms too (`\x1b[32m[ok  ]\x1b[0m` contains the literal `[ok`), so they do not catch escape leaks. Add explicit no-ANSI assertions to one subtest per output family:

Add a one-line `assert "\x1b[" not in <captured_output>` to one already-capturing subtest in every VM test that exercises a newly-in-scope command family. This pins the per-site wiring for each command without adding new VMs; only add a subtest where the current test does not already capture the relevant stream.

11. **doctor non-TTY is plain** (`tests/cli/braid-doctor.py`, inside "Valid config -- human output"): after the existing `[ok` check, also `assert "\x1b[" not in output`.
12. **unlock probe non-TTY is plain** (`tests/cli/braid-unlock.py`): in a subtest that exercises a cold unlock whose captured stderr includes `[ok  ]  disk: diskN  found` rows, redirect `2>/tmp/probe-stderr`, cat it, assert a tag substring is present and `\x1b[` is not. Pins the stderr TTY poll shared by `print_probe_events`, `UnlockPlan::execute`, and the `mount.rs` direct helper rows.
13. **recover non-TTY is plain** (`tests/cli/braid-recover.py`): extend the existing captured dry-run streams. In Test 3a, after reading `/tmp/pcf-stderr`, assert `"\x1b[" not in err` alongside the existing `[skip]` and probe-note checks. In Test 3b, after reading `/tmp/drn-stdout`, assert `"\x1b[" not in out` alongside the existing `[ok  ]` probe-note checks. These pin recover's failure-path stderr rendering and successful dry-run stdout rendering without adding a new VM or subtest.
14. **lock live non-TTY is plain** (`tests/cli/braid-lock.py`): add a subtest that runs a real `braid lock` with `2>/tmp/live-stderr`, asserts the live row substring (e.g. `unmounted /mnt/storage`) is present and `\x1b[` is not. Pins `LockPlan::execute`'s stderr TTY poll and the `tag` closure wiring.
15. **lock dry-run non-TTY is plain** (`tests/cli/braid-lock.py`, extend existing "Test 5: dry-run preview goes to stdout"): after asserting `stdout == "nothing to do.\n"`, also `assert "\x1b[" not in stdout`.
16. **add non-TTY is plain** (`tests/cli/braid-add-warnings.py`): in the two existing `[warn]` subtests that already capture `/tmp/md-stdout` (dry-run) and `/tmp/rmd-stderr` (real-run), also `assert "\x1b[" not in out` and `assert "\x1b[" not in err`. Pins `plan.preview().print_colored()` on stdout and `render_notes_for_stderr_with` on stderr for `add`.
17. **remove non-TTY is plain** (`tests/cli/braid-remove-softwarn.py`): same pattern -- the existing dry-run subtest captures `/tmp/out` and the real-run subtest captures `/tmp/err2`; add `assert "\x1b[" not in ...` to both. Pins `remove`'s stdout dry-run + stderr prelude.
18. **remove-missing non-TTY is plain** (`tests/cli/braid-remove-missing-softwarn.py`): same pattern -- pick the existing `[warn]`-asserting subtest that captures stdout and/or stderr and add the no-ANSI check. Pins `remove_missing`'s stdout dry-run + stderr prelude.
19. **enroll non-TTY is plain** (`tests/cli/braid-enroll.py:299-310`, the "Phase A: dry-run" block): that subtest already captures `/tmp/t4d.out` and asserts `"[skip]  disk: disk3     not LUKS-formatted\n" in t4d_out` -- add `assert "\x1b[" not in t4d_out` right after it. Pins `enroll_key_file`'s dry-run `print_colored()` on the bracketed-tag path (the `Preview::render` stdout), not the plain-per-disk real-run stderr.
20. **replace dry-run stdout non-TTY is plain** (`tests/cli/replace-preview-warnings.py`): in the existing Phase 1 "live-path dry-run" subtest that redirects `>/tmp/live-out 2>/tmp/live-err`, add `assert "\x1b[" not in out`. Pins `replace`'s `plan.preview().print_colored()` on stdout. The existing Phase 1 `assert err == ""` already covers the stderr path's non-TTY correctness (empty stays empty under color mode), so a separate no-ANSI assertion on `err` is vacuous; the stderr helper path is pinned by unit test 10a above.

These VM tests are the non-TTY safety net. The pure `should_color_status_tags` test (test 3) pins the gate logic; tests 11-20 plus 10a pin per-site wiring at integration and unit level.

## Verification

1. `just test-rust` -- all new/updated unit tests pass; plain-text pins unchanged.
2. `just test-vm braid-lock braid-unlock braid-recover braid-doctor braid-add-warnings braid-remove-softwarn braid-remove-missing-softwarn braid-enroll replace-preview-warnings` -- runs every VM test covering a newly-in-scope command, including the new redirected-stream assertions (tests 11-20). Any ANSI leak on the non-TTY path fails here.
3. Manual smoke (one-time, documented in PR): on a dev NAS, run at least one command from each family (`sudo braid doctor`, `sudo braid unlock`, `sudo braid lock`, `sudo braid add ...`) and confirm colored tags appear. Then repeat at least `sudo braid doctor` with `NO_COLOR=1` and with a pipe to `cat` -- both must show plain tags.

The removed `Display` impl is the compile-time gate: any bare `"{}"` on a `StatusTag` fails to build, so no runtime grep check is needed.

## Assumptions

- Direct `StatusTag` consumers are `status_tag.rs`, `preview.rs`, `mount.rs`, `lock.rs`, `doctor.rs` (verified by repo grep). Every other command (`unlock`/`recover`/`add`/`remove`/`remove-missing`/`replace`/`enroll_key_file`) uses `StatusTag` indirectly via `preview.rs`.
- `preview.rs` is the single shared renderer seam for every `Shape A` command.
- Lowercase labels (`[fail]` etc.) are the current staged contract; not changing them.
- Grey `[skip]` uses ANSI 90 (bright black). On themes where bright-black is unreadable, skip lines will look the same as before -- acceptable given they're the lowest-signal rows.
