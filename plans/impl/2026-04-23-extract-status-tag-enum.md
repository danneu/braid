# Plan: extract shared `StatusTag` for `[ok  ]` / `[warn]` / `[fail]` / `[skip]` rows

## Context

Three CLI modules render human status rows with the same 4-char bracket
tag convention, with three slightly different implementations:

- `cli/src/lock.rs:21-23` -- private `fn tag(label: &str) -> String` =
  `format!("[{:<4}]", label)`. Used for `tag("ok")` at 4 non-test
  callsites, but warn/fail paths emit hardcoded `[fail]` / `[warn]`
  literals at 12 non-test callsites (see "Inventory" below).
- `cli/src/mount.rs:62-64` -- identical private `fn tag()` (same body).
  Used consistently with `tag("ok")` and `tag("skip")` at 8 non-test
  callsites.
- `cli/src/doctor.rs:919` -- no helper; `format_doctor_human` emits the
  same format inline as `format!("[{tag:<4}]  {label:<14}  {}\n", ...)`
  where `tag` is `"ok"`/`"warn"`/`"fail"`/`"skip"` from a `match` on
  `CheckStatus`.

These three local instances cover the same convention (the human
status-row family). The current lock.rs-only finding is a symptom of the
shared boundary being un-named. Fixing just lock.rs now and deduping
later means two mechanical passes over the same code.

Explicitly distinct (and out of scope): `cli/src/cmd.rs:266` uses
`format!("[{:<11}] {}", step.risk, ...)` for dry-run risk tags
(`[safe       ]`, `[destructive]`, etc.) -- different width, different
meaning, different audience.

## Recommended fix

### Introduce `StatusTag` enum

Create `cli/src/status_tag.rs`:

```rust
/// A 4-char bracket status tag for human CLI status rows.
///
/// Used by `lock`, `mount`, and `doctor` to prefix per-item outcome
/// lines. The bracketed form is always 6 columns wide so consecutive
/// rows align.
///
/// Distinct from the dry-run risk tag in `cmd::Step::print_dry_run`,
/// which uses an 11-wide column for `safe` / `destructive` etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTag {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl StatusTag {
    fn as_label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

impl std::fmt::Display for StatusTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{:<4}]", self.as_label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_tag_pins_four_known_levels() {
        // Byte-pin cross-command contract: lock/mount/doctor all rely on
        // these exact strings for column alignment.
        assert_eq!(StatusTag::Ok.to_string(), "[ok  ]");
        assert_eq!(StatusTag::Warn.to_string(), "[warn]");
        assert_eq!(StatusTag::Fail.to_string(), "[fail]");
        assert_eq!(StatusTag::Skip.to_string(), "[skip]");
    }
}
```

Register in `cli/src/lib.rs` with `pub mod status_tag;` (alphabetical
position).

Rationale for the enum over `fn status_tag(&str)`: the four-level
set is closed, compile-time enforcement removes a typo vector
(`"fial"`, `"WARN"`) at callsites, and `impl Display` lets callers
interpolate directly via `{}`.

### Migrate the three callers

Callsite inventory was refreshed from the current files just before this
plan was written. Implementation should re-verify via `grep -n` before
editing, in case drift occurs.

**`cli/src/lock.rs`** -- 4 existing `tag("ok")` + 12 hardcoded
`[warn]`/`[fail]` literals, all non-test:

| Line | Current                                                          | Replace with                                      |
| ---- | ---------------------------------------------------------------- | ------------------------------------------------- |
| 21   | `fn tag(label: &str) -> String { ... }`                          | (delete)                                          |
| 76   | `"[warn]  cryptsetup close {mapper} busy, retrying ..."`         | `"{}  cryptsetup close ...", StatusTag::Warn`     |
| 224  | `writeln!(out, "[warn]  {}", orphan_scan_warn_body(&e))`         | `writeln!(out, "{}  {}", StatusTag::Warn, orphan_scan_warn_body(&e))` |
| 296  | `eprintln!("[warn]  {}", orphan_scan_warn_body(&e))`             | `eprintln!("{}  {}", StatusTag::Warn, ...)`       |
| 317  | `eprintln!("[fail]  {err}")`                                     | `eprintln!("{}  {err}", StatusTag::Fail)`         |
| 318  | `eprintln!("[warn]  attempting to close ...")`                    | `eprintln!("{}  attempting to close ...", StatusTag::Warn)` |
| 321  | `eprintln!("{}  {:<14}unmounted {}", tag("ok"), "pool", ...)`    | `... StatusTag::Ok, "pool", ...`                  |
| 337  | `"[warn]  btrfs device scan --forget failed (exit {}): ..."`      | `"{}  btrfs device scan --forget failed ...", StatusTag::Warn` |
| 343  | `eprintln!("[warn]  btrfs device scan --forget failed: ...")`     | `eprintln!("{}  btrfs device scan --forget failed: ...", StatusTag::Warn)` |
| 359  | `eprintln!("{}  disk: {:<7}locked", tag("ok"), name)`            | `... StatusTag::Ok, name`                         |
| 363  | `"[warn]  disk: {:<7}close failed (umount was stuck): {}"`        | `"{}  disk: {:<7}close failed ...", StatusTag::Warn` |
| 368  | `eprintln!("[fail]  disk: {:<7}{}", name, e)`                    | `eprintln!("{}  disk: {:<7}{}", StatusTag::Fail, name, e)` |
| 376  | `eprintln!("{}  disk: {:<7}already closed", tag("ok"), name)`    | `... StatusTag::Ok, name`                         |
| 390  | `"[warn]  orphaned mapper {entry} (not in pool.json -- ...)"`     | `"{}  orphaned mapper ...", StatusTag::Warn`      |
| 394  | `eprintln!("{}  disk: {:<7}locked (orphan)", tag("ok"), ...)`    | `... StatusTag::Ok, ...`                          |
| 398  | `"[warn]  disk: {:<7}orphan close failed (umount was stuck): {}"` | `"{}  disk: {:<7}orphan close failed ...", StatusTag::Warn` |
| 403  | `eprintln!("[fail]  disk: {:<7}orphan: {}", disk_name, e)`       | `eprintln!("{}  disk: {:<7}orphan: {}", StatusTag::Fail, disk_name, e)` |

Add `use crate::status_tag::StatusTag;`.

Test-side assertions that must stay byte-identical (do NOT edit):
`lock.rs:1035` (expected-output literal `"[warn]  could not scan ..."`)
and `lock.rs:1071` (`!output.contains("[warn]")`). The `Display` impl
emits the same bytes, so these assertions keep passing unchanged.

**`cli/src/mount.rs`** -- 8 non-test `tag(...)` callsites:

| Line    | Level      |
| ------- | ---------- |
| 62-64   | `fn tag()` -- delete |
| 324     | `tag("skip")` -> `StatusTag::Skip` |
| 331     | `tag("skip")` -> `StatusTag::Skip` |
| 338     | `tag("skip")` -> `StatusTag::Skip` |
| 345     | `tag("ok")` -> `StatusTag::Ok`     |
| 350     | `tag("ok")` -> `StatusTag::Ok`     |
| 556     | `tag("ok")` -> `StatusTag::Ok`     |
| 692     | `tag("ok")` -> `StatusTag::Ok`     |
| 746     | `tag("ok")` -> `StatusTag::Ok`     |

Add `use crate::status_tag::StatusTag;`. Test-side string literals at
`mount.rs:1897-1898` (`[ok  ]  disk: disk4 ...`) stay byte-identical
because `StatusTag::Ok` renders to the same 6 bytes.

**`cli/src/doctor.rs`** -- rewrite `format_doctor_human` (lines
894-922):

- Replace the `CheckStatus -> &str` match with a
  `CheckStatus -> StatusTag` match:
  `CheckStatus::Ok => StatusTag::Ok`, `Warn => Warn`, `Fail => Fail`,
  `Skip => Skip`.
- Replace `format!("[{tag:<4}]  {label:<14}  {}\n", c.message)` with
  `format!("{tag}  {label:<14}  {}\n", c.message)`.
- Add `use crate::status_tag::StatusTag;`.

Test-side assertions at `doctor.rs:1145` (`human.contains("[ok  ]")`)
and `doctor.rs:1165` (`human.contains("[fail]")`) pass unchanged because
the `Display` impl emits the same bytes.

### Out of scope (deliberate)

- Not a project-wide CLI output abstraction. Only the 4-char bracket
  status-row family.
- `cli/src/cmd.rs::Step::print_dry_run` (`[{:<11}]` dry-run risk tags).
  Different format, different domain.
- Other stderr styles left untouched: `warning:`, `Warning:`,
  `WARNING:`, `error:`, `ok:`, `skip:`, narrative lines like `Done.`,
  `LUKS opened:`, etc.
- No cross-command snapshot tests, no byte-pinning full command output.

## Files modified

- `cli/src/status_tag.rs` -- new module (~40 lines incl. unit test).
- `cli/src/lib.rs` -- add `pub mod status_tag;`.
- `cli/src/lock.rs` -- drop private `fn tag()`, rewrite 4 + 12
  callsites.
- `cli/src/mount.rs` -- drop private `fn tag()`, rewrite 8 callsites.
- `cli/src/doctor.rs` -- rewrite `format_doctor_human` to use
  `StatusTag`.

## Tests

- **New:** `cli/src/status_tag.rs::tests::status_tag_pins_four_known_levels`.
  Byte-pins all four variants. Once the type is shared across
  lock/mount/doctor, its output is a cross-command contract -- the unit
  test is the right layer to pin it.
- **No new tests in lock/mount/doctor.** The migration is
  behavior-preserving at each callsite: the `Display` impl emits the
  same bytes as the strings being replaced, and the trailing format
  arguments are unchanged. Existing test-side assertions in those files
  (`lock.rs:1035`/`1071`, `mount.rs:1897-1898`, `doctor.rs:1145`/`1165`)
  continue to pass unchanged and serve as implicit byte-pins for the
  migration's output.

Explicitly skipped (keeps the quality bar behavioral and
structure-insensitive):

- No broad snapshot tests of lock/mount/doctor human output.
- No byte-pin tests of full command output.

## Verification

1. `just test-rust` -- existing lock, mount, doctor unit tests pass
   unchanged.
2. `cargo test -p braid-cli status_tag` -- new unit test passes.
3. Targeted "helpers are gone" checks:
   - `grep -nE '^fn tag\(' cli/src/lock.rs cli/src/mount.rs` -> no
     matches (private helpers removed).
   - `grep -n '\[{tag:<4}\]' cli/src/doctor.rs` -> no matches (inline
     format removed).
   - `grep -nE '"\[(fail|warn|skip)\]' cli/src/lock.rs cli/src/mount.rs cli/src/doctor.rs`
     -> manually inspect every hit. Post-migration, the only legitimate
     matches are four test-side assertions / expected-output literals:
     `lock.rs:1035`, `lock.rs:1071`, `doctor.rs:1165`, `doctor.rs:1166`.
     Any other hit means a hardcoded tag was missed -- including in
     multi-line `eprintln!` / `format!` blocks where the `"` and `(`
     land on separate lines (the current `lock.rs:337` and
     `lock.rs:390` shape).
     (Note: this pattern intentionally excludes `[ok  ]`, because no
     non-test `ok` literal currently exists -- every `ok` goes through
     the helper. The existing test-side `[ok  ]` bytes at
     `mount.rs:1897-1898` and `doctor.rs:1145` are not targeted by this
     grep and are independently preserved by the new
     `status_tag_pins_four_known_levels` unit test and by those tests
     continuing to pass unchanged.)
4. Spot-check in a VM test (`just test-vm`, any lock/mount scenario)
   that status rows render identically to before.
