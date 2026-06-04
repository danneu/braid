# Dry-run preview: tighten risk tags to `[safe]`, pad outside the bracket

## Context

`braid <cmd> --dry-run` renders each step with its risk tag padded *inside* the
brackets so descriptions line up:

```
[destructive] LUKS format ...
[safe       ] LUKS header backup ...
[safe       ] LUKS open ...
```

The inside-padding makes the tag a non-token: the literal `[safe]` never appears,
so `grep -F '[safe]'` finds nothing and you are forced into `grep '\[safe'`, which
also matches any future `[safer]`-style prefix. It also reads as visual noise.

The codebase already solved this for the *runtime* status tags. `status_tag.rs`
renders tight tokens (`[ok]`, `[warn]`, `[skip]`, ...) and pads *outside* the
bracket via `status_tag_pad`, with a test pinning a "seven visible columns"
prefix. The dry-run **step** renderer is the lone holdout still padding inside.

This change makes the step renderer follow the established convention: tight
bracketed token, alignment whitespace moved after the `]`. Result:

```
[destructive] LUKS format ...
[safe]        LUKS header backup ...
[safe]        LUKS open ...
```

`grep -F '[safe]'` / `grep -F '[destructive]'` now match exactly, the description
column (col 15) and the `$` command indent (15 spaces) are unchanged, and all
braid bracket tags (runtime + dry-run) become consistent greppable tokens.

Risk vocabulary is unchanged: `safe`, `destructive`, `long` (confirmed the
complete set; `risk` is a `&'static str` on `Step`, `cli/src/cmd.rs#Step`).

## The change (1 production site)

`cli/src/cmd.rs#Step::render_dry_run` is the **sole** producer of the tag line
(grep-verified). Every command's `render_steps()` feeds `Vec<Step>` through it,
directly or via `cli/src/preview.rs#Preview::render_with` (which calls
`Step::render_dry_run` at the step block). Fixing this one function fixes every
command's dry-run output.

Current (`cli/src/cmd.rs`):

```rust
out.push_str(&format!("[{:<11}] {}\n", step.risk, step.description));
```

Replace with a tight token left-aligned in a fixed column so the padding falls
*after* the `]` (trailing spaces, not inside the brackets), mirroring
`status_tag.rs#status_tag_pad`:

```rust
/// Visible width of the widest risk tag (`[destructive]`). Shorter tags get
/// trailing padding after the `]` to reach this column, so descriptions stay
/// aligned while the bracketed tag itself stays a tight, greppable token --
/// the tight-tag + outside-pad convention already used for runtime status
/// tags (status_tag.rs).
const RISK_TAG_COL: usize = "[destructive]".len(); // == 13

// in render_dry_run:
let tag = format!("[{}]", step.risk);
out.push_str(&format!(
    "{tag:<width$} {desc}\n",
    width = RISK_TAG_COL,
    desc = step.description,
));
```

Notes:
- `"[destructive]".len()` is a const expression (`str::len` is `const`), so the
  column is self-documenting and tied to the actual widest tag rather than a bare
  `13`.
- The trailing literal space guarantees at least one separator even if a future
  risk label exceeds 13 chars (graceful degradation), matching today's `] `.
- Output stays pure ASCII (the existing `is_ascii()` assertion still holds) and
  is not colorized -- unchanged.

### Exact resulting prefix

`[safe]` / `[long]` are 6 chars -> pad to 13 (+7 spaces) + 1 literal space =
**8 spaces** after the `]`. `[destructive]` is 13 chars -> 0 pad + 1 space (its
line is **unchanged**). Description still starts at column 15; command lines keep
their 15-space indent.

## Test + doc edits (grep-verified complete inventory)

Verification grep used: `rg -n '\[(safe|long|destructive) +\]'` over the repo
(only `safe`/`long` ever carry inner padding; `[destructive]` never does). Every
hit below; nothing else in the tree embeds the padded literal.

### A. Renderer-contract exact assertions -- update to the new exact string (8 spaces)

These pin the literal rendered line and *should* stay exact (they are the output
contract). Replace `[safe       ]` with `[safe]` + 8 spaces:

| File:line | New string |
| --- | --- |
| `cli/src/cmd.rs:3019` | `"[safe]        LUKS open -> braid-aaa"` |
| `cli/src/cmd.rs:3042` | `"[safe]        identity verification at execution time"` |
| `cli/src/preview.rs:347` | `[safe]        btrfs device scan` |
| `cli/src/preview.rs:370` | `[safe]        btrfs device scan` (the colored test; `[warn]` lines unchanged) |
| `cli/src/recover.rs:17986` | `"[safe]        replay verified returned-disk add /dev/mapper/braid-disk2 (skipped: target already live in pool)"` |
| `cli/src/recover.rs:17992` | `"[safe]        replay fresh add target /dev/disk/by-id/virtio-disk3 (skipped: target already live in pool)"` |

`cli/src/cmd.rs:3017` (`[destructive] ...`) is **unchanged** -- already tight.

### B. Tag-presence checks -- loosen to the tight token (more robust, future-proof)

These only assert "a tag of this risk is present" and currently embed the full
padded literal `[long       ]` / `[safe       ]`, which **will break** (the third
exploration agent misclassified these as safe). Change to the tight token so they
no longer couple to column width:

| File:line | Change |
| --- | --- |
| `cli/src/lock.rs:2906` | `.find("[safe       ]")` -> `.find("[safe]")` |
| `cli/src/remove.rs:1495` | `.contains("[long       ]")` -> `.contains("[long]")` |
| `cli/src/remove.rs:1501` | `.contains("[safe       ]")` -> `.contains("[safe]")` |
| `cli/src/replace.rs:4296` | `.contains("[long       ]")` -> `.contains("[long]")` |

### C. Docs

`README.md` (the only doc with the example block; agent-confirmed): lines
116, 118, 122, 124, 126, 128 -- replace each `[safe       ]` with `[safe]` + 8
spaces. The `[destructive]` lines and every `$ ...` command line stay as-is. The
surrounding prose at `README.md:106` already says tags are `[destructive]`,
`[safe]`, `[long]` (tight) -- no prose change needed.

### D. Comment accuracy (prose -- NOT caught by the Verification grep)

- `cli/src/status_tag.rs:10-11` -- the `StatusTag` doc comment cross-references
  the dry-run risk tag and pins the *old* model:

  ```rust
  /// Distinct from the dry-run risk tag in `cmd::Step::print_dry_run`,
  /// which uses an 11-wide column for `safe` / `destructive` etc.
  ```

  After this change that sentence is false and contradicts the convention we are
  now matching. Rewrite it to describe the new shape -- e.g.: the dry-run risk
  tag in `cmd::Step::render_dry_run` follows the same tight-token + outside-pad
  convention, but pads to the widest risk tag (`[destructive]`, 13 cols) over a
  different vocabulary (`safe` / `destructive` / `long`). Because it is prose,
  not a padded literal, the Verification grep does not flag it -- edit by hand.
- `tests/cli/braid-add-during-balance.py:143` -- a comment showing
  `` `[safe       ]` / `[destructive]` / `[long       ]` `` as example shapes.
  Update the two padded examples to `` `[safe]` `` / `` `[long]` ``. The actual
  assertion (~line 146) tests the prefixes `[safe` / `[destructive` / `[long` and
  is unaffected.

## Out of scope (verified, no change)

- `cli/src/status_tag.rs` -- *code and behavior* unchanged; it is the precedent
  we are matching (already tight + outside-pad), and note tags (`[warn]`,
  `[skip]`) flow through it untouched. Its one edit is the stale doc-comment
  cross-reference at lines 10-11 -- see section D.
- `cli/src/add.rs:3792` -- `format!("[{}] {}", s.risk, ...)` is a *test* panic
  message formatter, already tight, not user-facing output.
- `docs/design/decisions/022-dry-run-preview-model.md` -- describes the
  work-plan/preview model, does not pin the bracket text. The `[wait]`/`[ok]`/
  `[skip]` rows it mentions are `StatusTag` runtime rows, not step tags.
- Per-command dry-run render tests in `add.rs` / `unlock.rs` / `enroll_key_file.rs`
  / `remove_missing.rs` -- none embed a padded `[safe       ]`/`[long       ]`
  literal (grep-confirmed), so none break.

## Verification

1. `just test-rust` -- the gate. Exercises the changed unit tests:
   `render_dry_run_formats_steps_with_commands`,
   `render_dry_run_step_without_commands` (cmd.rs);
   `render_emits_notes_before_steps`,
   `render_with_colors_only_warn_tag_before_steps` (preview.rs);
   `plan_recover_dry_run_..._renders_safe_placeholders` (recover.rs);
   `dry_run_render_*` in remove/replace and `dry_run_preview_mounted_happy_path`
   in lock.
2. Completeness guard -- after the change,
   `rg -n '\[(safe|long|destructive) +\]' --glob '!plans/**'` must return
   **zero** hits (only the tight `[destructive]` with no inner spaces remains).
   This is the exact discovery sweep that built the edit inventory, so a
   repo-wide zero proves every padded literal was fixed -- including the
   `tests/cli/braid-add-during-balance.py` comment, which a `cli/ README.md`-only
   grep would miss. The `!plans/**` exclusion is required because this plan file
   itself quotes the padded literals. (The `status_tag.rs` prose comment from
   section D is *not* covered by this grep -- it has no bracketed literal -- so
   confirm that one by eye.)
3. Eyeball + grep on real output: build, then
   `braid add ... --dry-run | grep -F '[safe]'` returns the safe lines, and the
   description column visibly aligns under the `[destructive]` rows.

No VM suite run required: this is pure CLI output formatting, the touched code is
fully unit-tested, and the one VM test that inspects dry-run output asserts tag
*prefixes* that still match. (Per AGENTS.md, default to focused runs.)
