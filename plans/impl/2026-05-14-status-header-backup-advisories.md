# Fix: `braid status` Advisories doc inversion (and JSON omission)

## Context

`manual/commands/status.md:144-146` describes the Advisories section
with the **opposite** condition from what the code emits:

- **Doc says:** "Warnings appear when LUKS header backups are missing
  for one or more disks."
- **Code does** (`cli/src/luks.rs:997-1011`, `header_backup_advisories_in`):
  emits a single warning *only when* `.luksheader` files **exist** in
  `/var/lib/braid/luks-headers/`. The intent is to nudge the operator
  to copy the local header backup off-system and then delete the local
  copy, per the workflow documented in
  `docs/luks-unlock.md#header-backup-workflow-and-messaging`.

Impact: operators reading the doc are told "no warning = backups
missing = I'm exposed", the reverse of reality. They may run extra
`braid doctor` flows or, worse, skip the offsite-copy task the
warning was designed to nudge.

While in the file, the same section's JSON output list (lines 148-165)
omits the `advisories` field even though `cli/src/status.rs:68`
serializes it (`#[serde(skip_serializing_if = "Vec::is_empty", default)]`).
Same feature, same file -- fix both in one pass.

This is a documentation-only fix. The code and the TUI mirror string
(`cli/src/tui/view/mod.rs:1701`) are correct. Two adjacent guide
paragraphs (`manual/guides/auto-unlock.md:143-147`,
`manual/guides/day-to-day-nas-usage.md:160`) describe the same
workflow but stop at "copy off-system" without telling the operator
to delete the local copy -- a half-truth, since
`docs/luks-unlock.md:118-124` defines the workflow as a three-step
sequence (write, copy, **delete**) and `braid status` warns until the
local copy is removed. The guides are extended here so all surfaces
agree.

## Files modified

- `manual/commands/status.md` (primary: Advisories section + JSON
  field list)
- `manual/guides/auto-unlock.md` (one paragraph: append the
  delete-local-copy step)
- `manual/guides/day-to-day-nas-usage.md` (one bullet: append the
  delete-local-copy step)

No code, fixture, or test changes. Cross-linking from `add.md` /
`replace.md` / `enroll.md` remains out of scope -- those pages
describe what each command does, not what the operator does next.

## Change 1: Rewrite the Advisories section

Replace `manual/commands/status.md:144-146`:

```markdown
### Advisories

Warnings appear when LUKS header backups are missing for one or more disks.
```

with prose that describes the actual emitted condition, shows the
real stdout shape, and links the canonical workflow doc (outer fence
is four backticks so the embedded triple-backtick sample renders
unambiguously):

````markdown
### Advisories

When a header-mutating operation (`braid add`, `braid replace`,
`braid enroll`) writes a local LUKS header backup to
`/var/lib/braid/luks-headers/<disk>.luksheader`, `braid status`
prints a warning until those files are removed:

```
warning: LUKS header backups exist in /var/lib/braid/luks-headers -- copy offsite and delete local copies
```

The local copy is a transient byproduct of the header-mutating
operation, not the intended backup target. Copy each `.luksheader`
file to an off-system location (USB, another machine, cloud key
storage), then remove the local copy to silence the warning.

See [LUKS header backup workflow](../../docs/luks-unlock.md#header-backup-workflow-and-messaging)
for the full rationale.
````

Notes for the editor:
- The stdout sample reflects the real format from
  `cli/src/status.rs:916-918` (one line per advisory, `warning: ` prefix,
  no section header).
- The path `/var/lib/braid/luks-headers` (no trailing slash in the
  emitted message) matches what the advisory actually prints; the
  prose mentions `/var/lib/braid/luks-headers/` (with slash) for
  directory clarity. Both forms are accurate.
- Use `--` (double hyphen), not em-dash, per project style (AGENTS.md
  CLI Output Style section). The emitted message already uses `--`.
- The relative link `../../docs/luks-unlock.md` resolves from
  `manual/commands/status.md` to the repo's `docs/` directory.

## Change 2: Add `advisories` to the JSON fields list

In the JSON section (`manual/commands/status.md` around lines 148-165),
add one bullet to the field list. The natural location is alongside
`alert_active` / `alert_causes` since advisories play the same
"banner-style notice" role. Insert after the `alert_causes` bullet:

```markdown
- `advisories`: array of human-readable advisory strings (omitted when
  none). See the Advisories section above for what currently produces
  them.
```

## Change 3: Extend the two guide paragraphs to the full workflow

Both guide paragraphs describe steps 1 and 2 of the workflow (braid
writes a local backup; copy it off-system) but omit step 3 (delete
the local copy) and the fact that `braid status` warns while it
remains. Add one sentence to each.

### `manual/guides/auto-unlock.md:143-147`

Replace the current paragraph:

```markdown
## LUKS header backups

After enrolling a keyfile, braid modifies the LUKS header on each drive (adding slot 1). braid stores LUKS header backups in `/var/lib/braid/luks-headers/`.

Copy these backups to a separate location (external drive, another machine). If a drive's LUKS header is corrupted, the header backup is the only way to recover access to that drive's data.
```

with:

```markdown
## LUKS header backups

After enrolling a keyfile, braid modifies the LUKS header on each drive (adding slot 1). braid stores LUKS header backups in `/var/lib/braid/luks-headers/` as a transient byproduct.

Copy each `.luksheader` file to a separate location (external drive, another machine), then delete the local file. `braid status` warns until the local copies are removed. If a drive's LUKS header is corrupted, the off-system backup is the only way to recover access to that drive's data.
```

### `manual/guides/day-to-day-nas-usage.md:160`

Replace the bullet:

```markdown
- **Keep LUKS header backups** -- braid stores header backups in `/var/lib/braid/luks-headers/` after operations that modify LUKS headers. Copy these off the NAS to a separate location. If a drive's LUKS header is corrupted and you have no backup, the data on that drive is unrecoverable.
```

with:

```markdown
- **Keep LUKS header backups** -- braid stores header backups in `/var/lib/braid/luks-headers/` after operations that modify LUKS headers. Copy each `.luksheader` file off the NAS to a separate location, then delete the local file (`braid status` warns until they are removed). If a drive's LUKS header is corrupted and you have no off-system backup, the data on that drive is unrecoverable.
```

Both edits preserve the existing prose tone, add only the missing
delete step plus the visible `braid status` consequence, and keep
`--` (double hyphen) per project style.

## Out of scope (intentional)

- `manual/commands/add.md`, `replace.md`, `enroll.md` mention "creates
  a LUKS header backup" but don't link the Advisories workflow.
  Adding those cross-references is a separate doc-discovery concern,
  not part of this finding's root cause.
- The doc subsection name "Advisories" doesn't literally appear in
  stdout (the emitted output uses bare `warning:` prefix with no
  header). Keeping the doc subsection name "Advisories" matches the
  prose-naming convention used by sibling subsections in the same
  page (`Pool summary`, `Capacity`, `Drives`, etc.). No rename.

## Verification

Documentation-only change; no tests to add or run. Manual checks:

1. `rg "header backups are missing" manual --glob '*.md'` returns no
   results afterwards. (`--glob` is used because `manual/book/` is a
   gitignored mdbook output tree; default `rg` already skips it, but
   the glob makes the check portable to `grep` / `ag` / `--no-ignore`
   shells.)
2. `rg "LUKS header backups exist in" manual/commands/status.md` finds
   the new sample line.
3. `rg "delete the local" manual/guides --glob '*.md'` finds the
   appended workflow step in both guide pages.
4. Render `manual/commands/status.md` in a markdown viewer (or
   `gh pr view` once on a branch) and confirm:
   - The Advisories section reads as a nudge to copy-offsite-and-delete,
     not as a warning about missing backups.
   - The relative link to `docs/luks-unlock.md#header-backup-workflow-and-messaging`
     resolves.
5. Cross-check against actual CLI behavior, optionally:
   - In a VM (`just test-vm` env) or local repro, run any header-
     mutating operation, then `sudo braid status`. Confirm the
     emitted line matches the sample in the new doc:
     `warning: LUKS header backups exist in /var/lib/braid/luks-headers -- copy offsite and delete local copies`
   - Then `sudo braid status --json | jq .advisories` should show the
     array; with no `.luksheader` files present, the field should be
     absent from the JSON entirely (serde `skip_serializing_if`).
6. `just test-rust` -- sanity check, expected to pass unchanged
   (no code touched).

## Critical references

- Emitter: `cli/src/luks.rs:997-1011` (`header_backup_advisories_in`)
- Call sites: `cli/src/status.rs:348` (status command),
  `cli/src/tui/mod.rs:32` (TUI), `cli/src/tui/view/mod.rs:1701`
  (TUI mirror string)
- Renderer (stdout shape): `cli/src/status.rs:916-918`
- JSON field declaration: `cli/src/status.rs:68`
- Canonical workflow doc: `docs/luks-unlock.md:118-136`
  ("Header backup workflow and messaging")
- Project messaging invariant: `AGENTS.md:131` (links the workflow
  doc as the authority for `doctor`/`status`/`unlock` recovery
  hints).
