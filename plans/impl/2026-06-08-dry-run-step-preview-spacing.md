# Drop alignment whitespace from `--dry-run` step preview

## Context

The `--dry-run` preview for mutating commands (`add`, `remove`, `replace`,
`remove-missing`, `recover`, `lock`) renders each step as a risk-tagged
description line followed by the literal `$ <command>` it would run. Today the
renderer pads the risk tags into a fixed column and indents the command lines to
align under the description:

```
[destructive] LUKS format /dev/disk/by-id/ata-Ironwolf_ST12_YYYY
               $ cryptsetup luksFormat --type luks2 ...
[safe]        LUKS header backup -> /var/lib/braid/luks-headers/braid-ironwolf.luksheader
               $ cryptsetup luksHeaderBackup ...
```

The alignment is visual noise: it pushes long device paths and commands far to
the right, wastes horizontal space, and wraps awkwardly in narrow terminals. The
goal is to drop it so tags carry a single trailing space and commands sit flush
at column 0:

```
[destructive] LUKS format /dev/disk/by-id/ata-Ironwolf_ST12_YYYY
$ cryptsetup luksFormat --type luks2 ...
[safe] LUKS header backup -> /var/lib/braid/luks-headers/braid-ironwolf.luksheader
$ cryptsetup luksHeaderBackup ...
```

This is a pure rendering change. ADR 022's "Output contract" governs the
stdout/stderr split and note ordering but says nothing normative about column
alignment or command indentation, so no ADR change is required.

## The change

All output flows through one renderer: `cli/src/cmd.rs#Step::render_dry_run`.
Two format strings produce the alignment:

- `"{tag:<width$} {}\n"` with `width = RISK_TAG_COL` (`= "[destructive]".len()` =
  13) right-pads the `[safe]`/`[long]` tags into a 13-col field. (`[destructive]`
  is already 13 wide, so it is unaffected.)
- `"               $ {}\n"` prefixes every command line with a 15-space indent.

Rewrite the loop body to emit a single space after the tag and a flush-left
command line, and delete the now-unused `RISK_TAG_COL` const (`cli/src/cmd.rs`,
its only two references are the definition and this use):

```rust
pub fn render_dry_run(steps: &[Step]) -> String {
    let mut out = String::new();
    for step in steps {
        out.push_str(&format!("[{}] {}\n", step.risk, step.description));
        for cmd in &step.commands {
            out.push_str(&format!("$ {}\n", cmd.to_argv().to_shell_string()));
        }
    }
    out
}
```

`[destructive] <desc>` lines are byte-identical before and after; only `[safe]`
and `[long]` lines lose their extra padding, and every `$` line loses its indent.

## Test updates

These are hardcoded `assert_eq!` / `.contains()` strings (no insta snapshots), so
they are edited by hand. Most `render_dry_run` callers assert on bare substrings
(`.contains("[safe]")`, `.contains("$ cryptsetup ...")`) or line counts and are
unaffected. Only assertions that embed the padding or indent break. The complete,
swept set:

**Rule A -- collapse a padded tag to a single space** (`[safe]<spaces>` -> `[safe] `):
- `cli/src/cmd.rs` -- `render_dry_run_*` tests: the `lines[2]` / `lines[0]`
  assertions (`"[safe]        LUKS open -> braid-aaa"`,
  `"[safe]        identity verification at execution time"`).
- `cli/src/recover.rs` -- two `.contains(...)` checks whose searched substring
  embeds the padding (`"[safe]        replay verified returned-disk add ..."`,
  `"[safe]        replay fresh add target ..."`).
- `cli/src/preview.rs` -- the `[safe]` line inside both multi-line `expected`
  raw strings (see Rule B; same two tests).

**Rule B -- strip the 15-space indent from command lines** (`<15 spaces>$ ` -> `$ `):
- `cli/src/remove.rs` -- five `$`-line assertions (`btrfs device remove`,
  `cryptsetup close`, `btrfs balance start`, the renamed-mapper pair).
- `cli/src/remove_missing.rs` -- two `$`-line assertions (device remove, balance).
- `cli/src/replace.rs` -- one `$`-line assertion (`cryptsetup close braid-disk2`).
- `cli/src/preview.rs` -- the `$ btrfs device scan` line inside both `expected`
  raw strings.

`cli/src/preview.rs` has both rules in the same two tests
(`render_emits_notes_before_steps`, `render_with_colors_only_warn_tag_before_steps`).
Their leading `[warn] ...` line is a preview *note*, not a step -- leave it
untouched. The post-edit `expected` becomes:

```
[warn] scan failed
[safe] btrfs device scan
$ btrfs device scan
```

**Explicitly unaffected** (verified, do not touch): `[destructive]` full-line
assertions (`cmd.rs`, `add.rs`, `replace.rs`); bare `.contains("[safe]")` /
`.find("[safe]")` (`remove.rs`, `lock.rs`); all `.contains("$ <cmd>")` checks
including recover's column-0 ones; `assert_eq!(lines.len(), N)` counts; and
render-equivalence/determinism tests that compare two `render_dry_run` outputs.
No NixOS VM test (`tests/`) asserts on this format -- they check refusal
behavior, rc codes, and pool.json byte-stability, never the alignment.

## README

`README.md` is the only doc that shows this output (no `docs/` page does).
Under "Preview with --dry-run":

- Update the fenced example block: collapse each `[safe]        ` to `[safe] `
  and strip the 15-space indent from every `$` line so commands are flush left.
  (`[destructive]` lines already have a single space -- leave them.)
- Correct the two stale fresh-format UUIDs in the same block. A real fresh
  `add --dry-run` renders `--uuid '<generated-at-format-time>'` -- the preview
  placeholder (`cli/src/cmd.rs#PREVIEW_LUKS_UUID_PLACEHOLDER`, emitted by
  `CryptsetupLuksFormatPreview` at `cmd.rs:916`, single-quoted by `shell_words`)
  -- not a concrete UUID. See ADR 022's "Fresh-format identity placeholder" and
  the pin test `cli/src/cmd.rs#to_shell_string_luks_format_preview_quotes_placeholder`.
  Replace both `--uuid 7f9d2e4a-...` and `--uuid 3a8c1d9f-...` tokens with
  `--uuid '<generated-at-format-time>'`. This is a bundled correctness fix: we
  are already rewriting these exact `luksFormat` lines to strip the indent, so
  leaving the wrong UUID would knowingly ship a misleading example.
- Fix the prose that describes the now-removed indent: "...and the indented `$`
  line is the literal command" -> drop "indented" (e.g. "...and the `$` line
  beneath each step is the literal command").

## Out of scope

The per-disk / status-note renderer (`[ok]   disk ...`, `[warn] ...`, `[skip]`
via `cli/src/status_tag.rs` + `cli/src/preview.rs#format_per_disk_line`) uses a
separate 7-col tag width shared across `status`, `doctor`, and the event log. The
user's example only shows step rows, so notes are left exactly as-is.

## Verification

1. `cd cli && cargo test` (or `just test-rust`) -- all `render_dry_run`,
   `preview`, `remove`, `remove_missing`, `replace`, and `recover` unit tests
   must pass with the updated expectations. Before editing the expected strings,
   running the suite first should fail *only* in the ~14 sites listed above,
   confirming the breaks-set is complete and nothing else depended on the
   alignment.
2. `cargo clippy` -- confirms `RISK_TAG_COL` removal left no dead-code/unused
   warning.
3. Spot-check the rendered shape via the existing `cmd.rs` render test output
   (it asserts `[destructive] LUKS format ...` unchanged and the new
   `[safe] LUKS open -> braid-aaa`), which exercises the real renderer end to end.
