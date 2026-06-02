# Plan: document scrub-error status output on the status command page

## Context

When a finished/aborted/interrupted scrub reports `error_count > 0`,
`braid status` renders a `(N errors)` count on the `Last scrub:` line and
appends a two-line `scrub error details:` block with a copyable
`journalctl` command (`cli/src/status.rs#format_scrub_journal_command`,
emitted at the tail of the text-output path around `status.rs:1314-1378`).

The status **command reference page** (`docs/commands/status.md`) never
documents this. Its "Last scrub result" example shows only clean states
(`(no errors)`, `cancelled (will resume)`, `interrupted`, `never`,
`running`), and its JSON `last_scrub` bullet omits the serialized
`error_count` field. This is an oversight from commit `41bece39`
("feat(status): point scrub errors at journal details"), which added the
hint, the TUI mirror, and a thorough troubleshooting section but never
touched `status.md`.

Note the headline gap is **discoverability, not absence**: the journalctl
command and its meaning are already documented well in
`docs/guides/troubleshooting.md` under "Scrub reported errors". So the
ideal fix makes the reference page complete and *points at* that section
-- it does not re-explain the journal output.

Intended outcome: a reader on the `braid status` page sees the error-state
output and the JSON `error_count` field, and is sent to the existing
troubleshooting section for diagnosis -- with no duplicated prose.

## Scope

Two edits, both in `docs/commands/status.md`. No code changes.

### Edit 1 -- "Last scrub result" section (currently `docs/commands/status.md:130-138`)

Add the `(N errors)` variant to the example block, then a second fenced
block showing the appended detail lines, then a short note + cross-link.

Faithful output to reproduce (verified against `status.rs:1333-1378`):

- The human `Last scrub:` line uses the space-padded ctime form
  `Mon Jan  1 00:00:00 2024` (two spaces before a single-digit day), via
  `status.rs#format_scrub_timestamp`.
- The `--since` argument uses a **different** format,
  `2024-01-01 00:00:00`, via `status.rs#format_scrub_timestamp_for_journalctl`.
  Both derive from the same scrub start instant; the example must show the
  two distinct shapes, not reuse one.
- The detail lines are indented two spaces.
- The `--grep` string must stay byte-identical to the
  `SCRUB_JOURNAL_GREP` constant (`cli/src/status.rs`,
  `cli/src/tui/view/mod.rs#SCRUB_JOURNAL_GREP`) and to the copy already in
  `troubleshooting.md` ("Scrub reported errors"):
  `BTRFS.*(at logical.*on (dev|mirror)|super block at physical)`.

Resulting section (target shape):

````
### Last scrub result

```
Last scrub: Mon Jan  1 00:00:00 2024 (no errors)
Last scrub: Mon Jan  1 00:00:00 2024 (3 errors)
Last scrub: Mon Jan  1 00:00:00 2024 cancelled (will resume)
Last scrub: Mon Jan  1 00:00:00 2024 interrupted
Last scrub: never
Last scrub: running (45%)
```

A nonzero error count replaces `(no errors)` with `(N errors)` on a
finished scrub, and prefixes the `cancelled (will resume)` and
`interrupted` lines when a partial scrub recorded errors. When the count
is nonzero, braid appends a copyable kernel-journal query for the
per-error detail lines:

```
Last scrub: Mon Jan  1 00:00:00 2024 (3 errors)
  scrub error details:
  sudo journalctl -k --since '2024-01-01 00:00:00' --grep 'BTRFS.*(at logical.*on (dev|mirror)|super block at physical)'
```

The `--since` argument is the scrub's start time. See
[Scrub reported errors](../guides/troubleshooting.md#scrub-reported-errors)
for how to read the journal output -- including corrected vs. uncorrectable
lines and why the count can exceed the visible journal lines.
````

The `../guides/troubleshooting.md#scrub-reported-errors` link mirrors the
existing ENOSPC cross-link at `status.md:245` and is validated by
`mdbook-linkcheck2` at build time (slug derives from the `## Scrub
reported errors` heading).

### Edit 2 -- JSON `last_scrub` bullet (currently `docs/commands/status.md:382-387`)

Append two sentences documenting the serialized `error_count` field and
the deliberate omission of the command string. Accurate against the serde
shape (`status.rs#ScrubReport`, `tag = "state"`): `finished`/`aborted`/
`interrupted` serialize `state`, `started_at`, and `error_count`;
`started_at_human` and `journal_since` are `#[serde(skip)]`.

Add after the existing `started_at` prose:

```
  The same three states also carry `error_count` (integer) -- the count
  btrfs reported, the same number the text output renders as `(N errors)`.
  The `scrub error details:` journalctl command from the text output is
  not part of the JSON (mirroring the profile annotations above); a
  `--json` consumer derives its own `--since` value from `started_at`.
```

## Non-goals

- **Do not duplicate troubleshooting prose.** The journal grammar,
  rate-limit caveat, corrected-vs-uncorrectable distinction, and
  `inode-resolve` recipe stay solely in
  `troubleshooting.md#scrub-reported-errors`. The status page links, it
  does not restate.
- **TUI docs unchanged.** The scrub-tab error rendering is covered by
  snapshot tests (`snapshot_scrub_tab_with_errors.snap`); no user-facing
  TUI page enumerates scrub states, so there is nothing to keep in sync.
- **No code changes** -- output and JSON are correct; only the reference
  page is incomplete.

## Critical files

- `docs/commands/status.md` -- the only file edited (Edits 1 and 2).
- `docs/guides/troubleshooting.md` -- cross-link target (read-only;
  confirm the `## Scrub reported errors` heading slug).
- `cli/src/status.rs` -- source of truth for the example strings
  (`format_scrub_timestamp`, `format_scrub_timestamp_for_journalctl`,
  `format_scrub_journal_command`, `SCRUB_JOURNAL_GREP`, render path
  `~1333-1378`). Read-only; used to verify fidelity.

## Verification

1. `nix develop .#docs -c mdbook build docs` -- `mdbook-linkcheck2`
   validates the new `troubleshooting.md#scrub-reported-errors` cross-link;
   a wrong slug fails the build.
2. `just check-docs` -- SUMMARY/table parity sanity (unaffected, no new
   page, but cheap to confirm).
3. Manual fidelity check: re-read `cli/src/status.rs:1333-1378` and confirm
   the three rendered example strings (the `(3 errors)` line, the
   two-space-indented `scrub error details:` line, and the journalctl
   command with its two distinct timestamp formats) match the code exactly.
4. Confirm the example `--grep` string is byte-identical to the
   `SCRUB_JOURNAL_GREP` constant and to the copy in `troubleshooting.md`.
