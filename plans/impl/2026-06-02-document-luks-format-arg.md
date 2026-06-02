# Plan: document `--luks-format-arg` in the command reference

## Context

`--luks-format-arg` is a real, advanced `braid` CLI flag (pass-through of raw
argv elements to `cryptsetup luksFormat`) accepted by both `braid add` and
`braid replace`. It is defined once via `cli/src/main.rs#LuksFormatArgs` and
flattened into `AddArgs` and `ReplaceArgs`. It carries a `///` doc comment, so
it *is* discoverable through `braid add --help` -- but it appears nowhere in the
mdBook command reference (`docs/commands/`). AGENTS.md requires the reference to
stay in sync with shipped features, so this is a real (if narrow) completeness
gap.

A review finding flagged the gap on `docs/commands/add.md` and proposed a
flags-table row. Verification confirmed the gap but found the finding's proposed
wording understated two facts, and that the gap also exists on the sibling
`replace.md` page:

1. **Equals form is mandatory for *every* value, not just hyphen-leading ones.**
   `LuksFormatArgs` sets `require_equals = true`, so the space form
   (`--luks-format-arg --pbkdf`) is rejected by clap with "equal sign is needed"
   (pinned by `cli/src/main.rs`'s `luks_format_arg_rejects_space_form_for_hyphen_value`
   test). The finding said "use the equals form for hyphen values"; the accurate
   instruction is "always use the equals form." The same understatement is live
   in the flag's own `--help` text (the `LuksFormatArgs` field doc comment), so
   this plan corrects it at the source too -- see "Source `--help` fix" below.
2. **The refused set is far broader than `--uuid`/`--label`.** The reject list
   `cli/src/types.rs#MANAGED_LUKS_FORMAT_LONG_FLAGS` (21 long flags) +
   `MANAGED_LUKS_FORMAT_SHORT_FLAGS` (6 short flags), enforced by
   `cli/src/types.rs#is_managed_format_flag`, covers identity, key-material,
   integrity, and on-disk-layout/offset options. `--uuid`/`--label` are only two
   examples.

Intended outcome: an operator can discover `--luks-format-arg` from the
reference page for both commands that accept it, with accurate usage and refusal
notes. (Scope chosen: **Minimal** -- one table row per page, no new example, no
new safety-checks bullet.)

## Scope

Two new doc rows plus one source doc-comment edit. No test or fixture changes,
no behavior change.

- `docs/commands/add.md` -- "Important flags" table (new row)
- `docs/commands/replace.md` -- "Important flags" table (new row)
- `cli/src/main.rs#LuksFormatArgs` -- correct the field doc comment (the
  `--help` text) so it states the equals form is always required, keeping
  `--help` and both reference pages in agreement. See "Source `--help` fix".

## Change

Add this row to the "Important flags" table on **both** pages, placed
immediately **before the `--progress` row** (keeps `--progress` last, matching
every other command page). Use identical wording on both pages so they stay in
sync:

```
| `--luks-format-arg=<ARG>` | Advanced: pass one raw argument to `cryptsetup luksFormat`, repeated once per argument; always use the equals form (e.g. `--luks-format-arg=--pbkdf`). braid refuses flags it manages itself -- identity, key-material, integrity, and on-disk-layout options such as `--uuid`, `--label`, `--type`, `--key-file`, and offset/sizing flags. |
```

Why this wording (each clause traces to code):

- "pass one raw argument to `cryptsetup luksFormat`, repeated once per argument"
  -- mirrors the `LuksFormatArgs` doc comment; `ArgAction::Append` + `num_args = 1`.
- "always use the equals form" -- `require_equals = true`. Corrects finding's
  conditional phrasing. The `--pbkdf` example doubles as the hyphen-value case
  (`allow_hyphen_values = true`).
- "braid refuses flags it manages itself -- identity, key-material, integrity,
  and on-disk-layout options such as ..." -- four-category summary of
  `is_managed_format_flag` (matching Context point 2; integrity is 6 of the 21
  managed long flags and the most storage-model-altering family), with four
  recognizable examples and an open-ended "offset/sizing flags". Accurate and
  non-exhaustive; corrects the finding's `--uuid`/`--label`-only claim. Refusal
  is a hard CLI-boundary error before any probe/journal/format (see
  `cli/src/add.rs` `LuksFormatExtraOpts::parse` call site and `cli/src/replace.rs`).

The cell contains no literal `|` outside code spans, so no pipe-escaping is
needed (unlike the adjacent `--progress auto\|always\|never` row).

### Source `--help` fix (`cli/src/main.rs#LuksFormatArgs`)

The field doc comment is the text `braid add --help` / `braid replace --help`
print, so it must agree with the new rows. It currently understates the rule the
same way the original finding did:

```rust
/// Repeat for multiple arguments. Use the equals form for values that
/// start with a hyphen, e.g. --luks-format-arg=--pbkdf.
```

`require_equals = true` rejects the space form for *every* value, not just
hyphen-leading ones (`--luks-format-arg pbkdf2` fails with "equal sign is
needed"), so this is misleading and would contradict the new reference rows.
Correct it to state the rule unconditionally -- recommended wording (keep the
first `///` line unchanged):

```rust
/// Repeat for multiple arguments. Always use the equals form
/// (--luks-format-arg=<ARG>); the space form is rejected. Required even for
/// hyphen-leading values, e.g. --luks-format-arg=--pbkdf.
```

Constraints:

- ASCII only (`--`, not em-dash) per the CLI output-style rule -- this text is
  user-facing CLI output.
- **No literal `(s)` plural marker.** `cli/tests/root_check.rs` asserts
  `!stdout.contains("(s)")` on `braid add --help`, `braid help add`, and
  `braid --help` -- a deliberate house invariant (braid overrides clap's help so
  it stays `(s)`-free). Write "arguments"/"values", never
  "argument(s)"/"value(s)". The recommended wording above already complies.
- Narrow hand edit; do not run `cargo fmt` (repo formatter-drift rule).
- Otherwise safe to change. `cli/tests/root_check.rs` is the only thing that
  captures `add --help`/`help add`, and beyond the `(s)` rule it only asserts the
  presence of `--dry-run`/`--yes`/`--progress` (none of which this edit touches);
  `replace --help` is not captured at all. No `.snap`/`insta` snapshot captures
  `--help` (the only snapshots are UPS-status and TUI), and the two current help
  phrases ("Use the equals form" / "values that start with a hyphen") appear
  nowhere outside `main.rs`. The equals-form behavior the new prose describes is
  pinned by `cli/src/main.rs#luks_format_arg_rejects_space_form_for_hyphen_value`,
  which asserts on clap's parse error independently of the help string.

## Deliberately out of scope

- **README.md** -- intentionally cookbook/brief per AGENTS.md ("not reference
  material"); an advanced tuning flag does not belong there.
- **Guides / internals** -- no existing page discusses cryptsetup/pbkdf tuning;
  inventing one exceeds this finding. `docs/internals/luks-unlock.md` would be
  the home if such a guide is ever wanted.
- **`replace.md`'s existing refusal bullet** (under "Safety checks / refusal
  cases", the capacity-preflight sentence naming offset-affecting
  `--luks-format-arg` flags) stays as-is. It remains accurate and is
  complementary to the new row -- the row states the general refusal, that
  bullet explains the replace-specific capacity rationale. Do not duplicate it.
- No new "Common variations" example and no new safety-checks bullet (Minimal
  treatment).

## Verification

Two doc rows plus one source doc-comment edit. No behavior change, so no
VM/fixture tests are implicated; a quick Rust run confirms the comment edit is
clean.

1. `just test-rust` (run as a normal, non-root user) -- compiles the
   `LuksFormatArgs` doc-comment edit and runs the real guards: `cli/tests/root_check.rs`
   (`add_dry_run_flag_accepted`, `add_progress_values_accepted`,
   `help_subcommand_works_without_root`) re-renders `add --help` / `help add` and
   enforces the `(s)`-free + `--dry-run`/`--yes`/`--progress` invariants, and
   `luks_format_arg_rejects_space_form_for_hyphen_value` re-checks the equals-form
   parse rule. Those `--help` assertions are gated `if is_root() { return; }`, so
   they are only active when the test process is non-root.
2. `nix develop .#docs -c mdbook build docs` -- builds the book and runs
   `mdbook-linkcheck2`. Confirms both tables still render and no broken links
   were introduced (the new row adds no links, so this is a low-risk sanity
   check). For local visual review: `just docs` (`mdbook serve`).
3. `just check-docs` -- SUMMARY.md parity + `scripts/docs/check-doc-tables.py`.
   Unaffected by this change (no new pages, no SUMMARY/index/README table
   edits); run to confirm no regression.
4. Eyeball the rendered "Important flags" tables on `add.md` and `replace.md`:
   new row present, placed before `--progress`, wording identical on both pages.
5. Cross-check both rows against `braid add --help` / `braid replace --help`:
   the flag name, the "always use the equals form" rule, and the "advanced"
   framing must now *agree* between the pages and `--help` (the doc-comment fix
   makes them consistent). A mismatch on the equals-form rule means one side was
   not updated -- do not "reconcile" by reverting the page to the conditional
   wording; the unconditional rule is the correct one.
