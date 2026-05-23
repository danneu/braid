# Pivot: surface scrub error details via journalctl pointer

## Context

`braid status` reports a scrub error count but offers no guidance on how to
investigate. A pasted finding ("3. Scrub errors not mapped to affected
files") proposed parsing kernel logs from inside the CLI to extract
per-inode error sites.

That proposal is misaligned with braid's architecture:

- **ADR 014 (Active) explicitly rejected kernel-journal scanning.** It was
  originally implemented as an alert source and removed because btrfs
  device-stats counters cover the alert pipeline reliably within 30s, and
  cursor/parsing/latch complexity was not worth it. Repro VMs at
  `tests/repro/kernel-journal-*` preserve the evidence.
- **The diagnostic data is already in the journal.** When btrfs scrub hits
  an uncorrectable data error, the kernel's `scrub_print_warning_inode`
  (`reference/linux/fs/btrfs/scrub.c:391`) calls `paths_from_inode()` and
  logs the resolved file path directly. Corrected errors, metadata
  errors, and superblock errors emit their own rate-limited diagnostic
  shapes (block coordinates, tree-block info, device + devid) without a
  file path. The user just doesn't know where to look.

The pivot: don't ingest kernel logs. Point the user at them, both in the
status output (so the pointer is right next to the count) and in the
troubleshooting guide (so the full investigation workflow has a home).

## Outcome

When `braid status` shows `(N errors)` for a finished/aborted/interrupted
scrub, the user can immediately copy a `journalctl` command that surfaces
the kernel's scrub error details for that scrub window. The details
include file paths only when the kernel was able to log them
(uncorrectable data errors with a resolvable inode); corrected, metadata,
and superblock errors appear as block-coordinate or device-level lines.
The scrub error count remains the authoritative tally; the journal output
is the diagnostic surface. The troubleshooting guide explains each
message shape and the inspect-internal fallback for the
path-resolution-failed case.

## Critical files to modify

### 1. `cli/src/status.rs`

- **Add a second timestamp formatter** alongside `format_scrub_timestamp`
  (lines 778-784): produce `"YYYY-MM-DD HH:MM:SS"`, which is the form
  `journalctl --since` accepts for naive local time. Use
  `time::macros::format_description!` -- same pattern as the existing
  formatter; `ScrubTimestamp` already wraps `time::PrimitiveDateTime`
  (`cli/src/parse/types.rs:208`).
- **Carry the journalctl-formatted start time on the terminal scrub
  variants.** Today `ScrubReport::{Finished,Aborted,Interrupted}` only
  carry `started_at: String` (the already-formatted ctime, see
  `status.rs:140-159`), so the human renderer no longer has the raw
  `ScrubTimestamp`. Reparsing the display string would be ugly and the
  ctime format does not round-trip to journalctl. Add a JSON-skipped
  `journal_since: String` field to each of the three terminal variants:
  ```rust
  Finished {
      started_at: String,
      error_count: u64,
      #[serde(skip)]
      journal_since: String,
  },
  // ... Aborted, Interrupted the same
  ```
  Populate `journal_since` inside `get_scrub_report` (`status.rs:726-776`)
  by calling the new ISO formatter on the raw `ScrubTimestamp` before it
  is consumed by `format_scrub_timestamp`. JSON output is unchanged
  (`#[serde(skip)]`), preserving the cause-neutral JSON convention.
- **Conditionally emit a hint block** inside the existing scrub block
  (`status.rs:1138-1164`): only when state is `Finished`, `Aborted`, or
  `Interrupted` AND `error_count > 0`. Format:
  ```
  Last scrub: <ts> (<N> errors)
    scrub error details:
    sudo journalctl -k --since '2026-05-20 10:05:30' --grep 'BTRFS.*(at logical.*on (dev|mirror)|super block at physical)'
  ```
  The `error_count` braid surfaces aggregates six distinct kernel
  message shapes -- not all of them carry a file path, and the
  RAID1-corrected case carries no file context at all:

  Per-extent detail messages (emitted from `scrub_print_common_warning`,
  only for uncorrectable data and metadata errors):
  - Data extent, path resolved (`scrub.c:457`): `... at logical N on
    dev X, physical N, root N, inode N, offset N, length N, links N
    (path: ...)`.
  - Data extent, path resolution failed (`scrub.c:471`): same shape
    but ends `... path resolving failed with ret=N` instead of
    `(path: ...)`.
  - Metadata tree block (`scrub.c:538`): `... at logical N on dev X,
    physical N: metadata leaf|node (level N) in tree N`. No inode,
    no path.

  Repair-summary messages (emitted from `scrub_stripe_report_errors`,
  `scrub.c:941-965`, via the rate-limited helper
  `btrfs_err_rl_in_rcu`):
  - Corrected via RAID1 mirror (`scrub.c:944`/`scrub.c:949`):
    `fixed up error at logical N on dev X physical N` (or `... on
    mirror N` when the source mirror has no device). The kernel
    takes a `continue` branch (`scrub.c:952`), so corrected errors
    do not get a per-extent detail line at all -- only block
    coordinates, never a file path.
  - Uncorrectable (`scrub.c:958`/`scrub.c:963`): `unable to fixup
    (regular) error at logical N on dev X physical N` (or `... on
    mirror N`). May be followed by one of the per-extent detail
    messages above, but the detail emission is independently gated
    by a second rate-limit check (`__ratelimit(&rs) && dev`,
    `scrub.c:968-981`). The detail can be elided even when the
    repair-summary line is present.

  Both groups are emitted from rate-limited helpers. Under a heavy
  burst of errors (many sectors affected in a short window) the
  kernel may drop messages, so the user can see fewer journal lines
  than braid's `error_count`. The scrub count (from `btrfs scrub
  status`) remains authoritative; journal lines are best-effort
  diagnostic clues.

  Superblock messages (emitted from `scrub_supers` /
  `scrub_one_super`, increment `super_errors` which contributes to
  `error_count`):
  - Bad checksum (`scrub.c:2815`): `super block at physical N devid
    N has bad csum`.
  - Bad generation (`scrub.c:2821`): `super block at physical N
    devid N has bad generation N expect N`.

  These do **not** go through `scrub_print_common_warning` -- every
  `scrub_print_common_warning` callsite passes `is_super=false`
  (`scrub.c:969`/`973`/`977`/`981`), so the `is_super=true` branch
  at `scrub.c:495-499` (`"%s on device %s, physical %llu"`) is dead
  code from scrub's perspective. An earlier draft of this plan
  cited that dead-code format and would have produced a regex that
  never matched a real scrub superblock message.

  The earlier draft's `--grep 'BTRFS.*(path:|path resolving failed)'`
  would silently return nothing when a non-zero `error_count` was
  entirely metadata, superblock, or RAID1-corrected -- the user
  pastes the printed command, sees empty output, and assumes the
  hint lied. A broader pattern fixes that, but it must stay
  scrub-specific to avoid surfacing unrelated BTRFS read-time
  checksum messages (e.g. `inode.c:172`'s `checksum error at
  logical N mirror N root N, inode N offset N`, which does not
  contain `on dev`, `on mirror`, or `super block at physical`).

  Pattern `BTRFS.*(at logical.*on (dev|mirror)|super block at physical)`
  catches every scrub message shape -- the three per-extent detail
  shapes, both repair-summary shapes, and both superblock shapes --
  and was verified by `grep -rn` to appear only in scrub.c across
  the kernel btrfs tree (`at logical.*on dev/mirror` is unique to
  `scrub.c:457/471/538/944/949/958/963`; `super block at physical`
  is unique to `scrub.c:2815/2821`). Mount, unmount, and inode-read
  corruption messages do not match. The label `scrub error details:`
  matches the broader scope -- it is honest about what the command
  surfaces, not promising file paths the kernel may not have logged.

  The command goes on its own line as a single copyable string, with
  two-space indentation. The full command exceeds 80 columns and will
  soft-wrap in a narrow terminal; that is acceptable because the user
  is meant to triple-click or shell-select the whole line and paste
  it. braid does not insert newlines inside the command -- the
  paragraph is one logical line, so terminal selection captures it
  cleanly.
- **Add unit tests** in the existing `#[cfg(test)]` block that exercise
  the new formatter and the hint rendering for at least:
  - `Finished` with `error_count > 0`: hint block appears, the `scrub
    error details:` label is present, and the printed command is the
    exact string `sudo journalctl -k --since '<iso-ts>' --grep
    'BTRFS.*(at logical.*on (dev|mirror)|super block at physical)'`
    with `<iso-ts>` populated from `journal_since`.
  - `Finished` with `error_count == 0`: no hint.
  - `Aborted` with `error_count > 0`: hint appears.
  - JSON serialization round-trip: `journal_since` does not appear in
    output (covers the `#[serde(skip)]` invariant).

The function/method that renders the scrub block already lives in the
human-formatter region around 1138-1164; extend it in place rather than
adding a new pub item -- avoids the AGENTS.md doc-comment requirement on
a new top-level item that exists only to print one extra line.

### 2. `cli/src/tui/view/mod.rs` (TUI parity)

The TUI Scrub tab uses a fixed-height layout: `view_scrub` (line 1046)
computes `scrub_height = scrub_lines(&pool.scrub) + 1` (line 1067) and
allocates exactly that many rows for the table. `scrub_lines` (line 611)
counts only the existing rows, so a hint row added inside the table
would clip vertically and a long single-line `journalctl ...` command
would truncate horizontally in the 60-column snapshot path.

Render the hint **as a wrapped paragraph beneath the table**, not as a
table row:

- `scrub_table()` (lines 481-609) stays focused on the metrics table.
  Do not add a row to it.
- **Hint condition: terminal states only, error_count > 0.** Define a
  helper `scrub_hint_command(scrub: &ScrubState) -> Option<String>`
  that returns `Some(...)` **only** when the state is `Finished`,
  `Aborted`, or `Interrupted` AND `error_count > 0`. The TUI views
  `ScrubState` directly (not `ScrubReport`), and `ScrubState::Running`
  also carries `error_count: u64` plus an `Option<ScrubTimestamp>`
  `started_at` (`cli/src/parse/types.rs:212-223`). A naive
  `error_count > 0` check would fire on a running scrub with
  early-detected errors, which (a) drifts from the CLI surface (the
  CLI hint comes from `ScrubReport` and `ScrubReport::Running` carries
  no `journal_since`), and (b) would have no stable `--since`
  timestamp to print while the scrub is still mutating. Excluding
  Running keeps the TUI consistent with the CLI and dodges the
  optional-timestamp branch entirely.
- Below the table, when `scrub_hint_command(...)` returns `Some(cmd)`,
  render a wrapped paragraph (ratatui
  `Paragraph::new(...).wrap(Wrap { trim: false })`) containing the
  same `scrub error details: sudo journalctl -k --since '<ts>' --grep
  'BTRFS.*(at logical.*on (dev|mirror)|super block at physical)'`
  text the CLI prints (label and grep pattern identical to section
  1, including the scrub-specific regex that catches the three
  per-extent detail shapes, both repair-summary shapes, and both
  superblock shapes while excluding inode-read corruption messages).
- Split `view_scrub` to allocate two stacked chunks when the hint is
  present: the existing `scrub_height` for the table, plus a hint
  block whose height is computed from the wrapped line count at the
  current panel width. A helper `hint_lines(area_width: u16) -> u16`
  encapsulates the wrap math so the same call sites stay declarative.
- **Snapshot expectations.** Add a new snapshot
  `snapshot_scrub_tab_with_errors.snap` rendering a `Finished` scrub
  with non-zero `error_count` at the standard 60-column snapshot width.
  The assertion must verify: (a) the `scrub error details:` label is
  present, (b) every token of the `sudo journalctl -k --since ...
  --grep 'BTRFS.*(at logical.*on (dev|mirror)|super block at
  physical)'` command is visible across the wrapped lines (i.e. the
  wrap inserts no truncation `>`/ellipsis), and (c) the existing
  rows (`Last run`, `Errors`, `Total`, ...) remain intact.
- **Negative case.** Add a focused unit test (not a full snapshot)
  asserting `scrub_hint_command(&ScrubState::Running { error_count: 5,
  ... })` returns `None`. This pins the "terminal states only" rule
  against accidental broadening in future edits.
- The existing snapshot `snapshot_scrub_tab.snap` (Errors 0) is
  unchanged because no hint is emitted in that case.
- TUI advisories live at the app level (`model.advisories`,
  `cli/src/tui/view/mod.rs:1352-1365`). Do not route this through global
  advisories -- it is scrub-specific and belongs next to the count, same
  as CLI.

### 3. `docs/guides/troubleshooting.md`

Insert a new section between `## Scrub won't start` (ends ~line 171) and
`## SMB/NFS service inactive after braid lock` (starts ~line 173). Match
the existing Symptom -> Fix style:

```markdown
## Scrub reported errors

**Symptom:** `braid status` shows `Last scrub: <ts> (N errors)` or
`braid monitor` raised a btrfs error alert after a scrub.

The scrub error count braid reports is authoritative -- braid
parses it from `btrfs scrub status`. Journal lines are diagnostic
*clues*, not a complete per-error ledger: the kernel emits scrub
messages through rate-limited helpers, so a busy or bursty scrub
can produce fewer journal lines than the count. A non-zero count
with sparse or missing journal lines is not a braid bug -- it
usually means the kernel dropped log entries to stay under its rate
limit.

Use the command printed under the scrub status, or run journalctl
directly:

```sh
sudo journalctl -k --since '<scrub-start-time>' --grep 'BTRFS.*(at logical.*on (dev|mirror)|super block at physical)'
```

Output comes in two distinct grammars depending on whether the
error is in a data/metadata extent or in a superblock copy.

**Extent errors (data and metadata).** Each affected sector may
log a repair-summary line:

- Corrected via RAID1 mirror: `fixed up error at logical N on dev
  /dev/mapper/braid-X physical N` (or `... on mirror N` when the
  source mirror has no device). btrfs RAID1 read the healthy
  mirror and wrote it back over the bad copy. **No file path** --
  corrected lines give block coordinates only. A count consisting
  mostly of `fixed up error` lines means data integrity was
  preserved; investigate the disk that produced the bad reads.
- Uncorrectable: `unable to fixup (regular) error at logical N on
  dev X physical N` (or `... on mirror N`). RAID1 could not
  recover -- the mirror was also bad or no mirror exists. The
  block is permanently damaged.

An uncorrectable extent error *may* also log an additional detail
line that identifies what was lost. The detail emission is gated
by a second rate-limit check, so it is not guaranteed to appear
for every uncorrectable error. When present, the shapes are:

- **Data extent, path resolved.** `... at logical N on dev X,
  physical N, root N, inode N, offset N, length N, links N
  (path: subdir/victim.bin)`. `(path: ...)` is **relative to the
  affected btrfs subvolume root**, not absolute. The kernel builds
  it from `paths_from_inode()`
  (`reference/linux/fs/btrfs/scrub.c:457`,
  `reference/linux/fs/btrfs/backref.c:2125`) and does not know
  what mount point exposes that subvolume. Prepend the mount point
  of the affected subvolume (default subvolume at `/mnt/storage`;
  named subvolumes wherever you configured them).
- **Data extent, path resolution failed.** Same shape but ends
  `... path resolving failed with ret=N` instead of `(path: ...)`.
  Usually means the extent has no remaining inode references (file
  already deleted) or the inode lives in a snapshot rooted under a
  different subvolume than the search root.
- **Metadata.** `... at logical N on dev X, physical N: metadata
  leaf|node (level N) in tree N`. Tree-block corruption -- no file
  path because the bad block lives in a btrfs tree, not in user
  data. Persistent metadata errors indicate disk failure.

**Superblock errors.** Logged as standalone messages from
`scrub_supers`, *not* as repair-summary + detail pairs. The grammar
is independent of the extent path:

- `super block at physical N devid N has bad csum`
- `super block at physical N devid N has bad generation N expect N`

Damage to one of the device's superblock copies. Investigate the
device (identified by `devid`), not a file.

For the **path-resolution-failed** case, you can try `inode-resolve`
as a best-effort:

```sh
sudo btrfs inspect-internal inode-resolve <inode> /mnt/storage
```

This succeeds only if the inode still exists in the subvolume
rooted at the supplied path. Deleted files, extents with no
remaining references, or files that live in a different subvolume
will still produce no result -- the kernel logged "path resolving
failed" for the same reason.

A non-zero error count after a scrub means at least one block
failed its checksum or I/O. With btrfs RAID1, blocks with a healthy
mirror are repaired automatically (counted as `Corrected` -- the
`fixed up` lines above); `Uncorrectable` means both copies were bad
and the file (for data) or tree block (for metadata) is now
damaged. The journal output is your best diagnostic surface, but
treat it as evidence rather than a complete ledger: rely on the
scrub count for "how many," and on the journal for "what kind, and
where the kernel could log it." Restore affected files from backup
and run `braid ack` once you have investigated.
```

The existing `## Related` section already has a Monitoring and alerts
bullet (`docs/guides/troubleshooting.md:187`). Update its description
in place to signal that the new section is the next-step destination,
e.g.: `[Monitoring and alerts](monitoring-and-alerts.md) -- alert
system details; see "Scrub reported errors" above for the post-alert
investigation steps.` Do not add a second bullet.

mdbook-linkcheck runs as part of `mdbook build docs` (see AGENTS.md
"Documentation" section) and will fail CI on a broken cross-link, so
keep the new links inside `docs/`.

## Existing functions/utilities to reuse

- `format_scrub_timestamp` (`cli/src/status.rs:778-784`) -- pattern for
  the new formatter; copy its structure, change the format string.
- `time::macros::format_description!` -- already used throughout the
  crate (`cli/src/status.rs`, `cli/src/util.rs`, `cli/src/membership.rs`).
- `ScrubTimestamp` (`cli/src/parse/types.rs:208`) -- already
  `time::PrimitiveDateTime`, formats natively.
- TUI snapshot harness -- `cli/src/tui/view/snapshots/` already contains
  three scrub snapshots; the new variant fits the same pattern.

## Out of scope

- Exposing the journal command or `journal_since` in JSON output.
  `journal_since` is `#[serde(skip)]` precisely because the existing
  JSON `started_at` is the ctime display string ("Wed May 20 10:05:30
  2026") and does not round-trip to `journalctl --since`. JSON output
  is intentionally cause-neutral; a machine consumer that wants the
  helper command must derive its own timestamp from `btrfs scrub
  status` directly, or a future plan can promote `journal_since` to a
  serialized field with an explicit name and contract. Do not bolt
  that on as part of this change.
- A new doc under `docs/internals/btrfs/`. The operator-facing guidance
  lives at the guides tier; an internals deep-dive can be added later if
  the kernel format ever changes meaningfully.
- Touching `braid monitor` / alert pipeline. ADR 014 governs that surface
  and is not the right layer for "where to look after the alert."

## Verification

1. `just test-rust` -- new formatter and human-output unit tests pass,
   including the JSON `#[serde(skip)]` round-trip assertion that
   `journal_since` does not leak into machine output.
2. `just test-rust` (snapshot review) -- the new TUI snapshot
   `snapshot_scrub_tab_with_errors.snap` is reviewed and accepted
   (`cargo insta review`). Confirm by inspection that the wrapped
   `sudo journalctl ...` command is visible end-to-end at the 60-column
   snapshot width (no truncation marks).
3. **End-to-end repro in a VM.** The existing
   `tests/repro/kernel-journal-bad-sector.py` does **not** run scrub --
   it triggers a direct `dd` read failure and scans kernel logs. Add a
   new focused repro with the **three** files the existing repro
   pattern requires:
   - `tests/repro/scrub-error-hint.py` -- the test script, modeled on
     the dm-dust setup from `kernel-journal-bad-sector.py`.
   - `tests/repro/scrub-error-hint.nix` -- the NixOS VM config that
     enables the `braid` module, installs the `braid` CLI, writes
     `/etc/braid/config.json`, and provides the dm-dust prerequisites.
     **Parameterize the file as `{ braid }: ...`** -- this repro needs
     the braid CLI binary to invoke `braid status`, which means it
     must accept the package as an argument the same way every other
     CLI-backed repro does (see `tests/repro/btrfs-remove-enospc.nix`
     and its registration at `flake.nix:596-600`). The
     `kernel-journal-*` repros at `flake.nix:644-649` use the bare
     `import` form because they never call the CLI; that pattern does
     not apply here.
   - `flake.nix` registration: add
     ```nix
     repro-scrub-error-hint = pkgs.testers.nixosTest (
       import ./tests/repro/scrub-error-hint.nix {
         braid = linuxCrane.braid;
       }
     );
     ```
     to the `checks.aarch64-darwin` block alongside the existing
     CLI-backed repros around `flake.nix:596-637`. Without this,
     `just test-repro` will not find or run the new test, and the
     sidecar has no way to reach the `braid` package.

   The test script: write the victim file, mark its first physical
   block bad via dm-dust, drop caches, then run scrub with
   `machine.execute("btrfs scrub start -B /mnt/storage")` and assert
   the exit code is **3**, not 0. `btrfs scrub start -B` returns 3
   when there are uncorrectable errors
   (`reference/btrfs-progs/cmds/scrub.c:1731-1734`), which is the
   expected outcome for the single-disk dm-dust setup with no mirror
   to repair from. Using `machine.succeed(...)` here would abort the
   test before any of the hint assertions run.

   After the scrub returns, assert: (a) `braid status` exit 0 and
   stdout contains the `scrub error details:` label plus the full
   `sudo journalctl -k --since '<ts>' --grep 'BTRFS.*(at logical.*on
   (dev|mirror)|super block at physical)'` command; (b) executing
   that exact printed command via `machine.succeed(...)` returns at
   least two distinct lines: one matching `unable to fixup (regular)
   error at logical .* on dev .* physical` (the repair-summary
   header for the uncorrectable case -- the single-disk dm-dust
   setup has no RAID1 mirror), and one matching `(path: victim.bin)`
   (the path-bearing detail line for the same error). The path is
   **relative to the subvolume root**, not the mount path, because
   the kernel only logs the ipath value from `paths_from_inode`.

   The dm-dust scenario corrupts a single block, which produces one
   logical error -- well under the kernel's `btrfs_err_rl_in_rcu`
   and `__ratelimit(&rs)` budgets. Both the repair-summary and the
   detail line are reliably emitted in this single-error case, so
   the assertions are deterministic. The plan's docs section
   intentionally calls out that the pairing is *best-effort* in the
   general case (high-volume scrubs may see the detail line elided);
   the repro proves the plumbing works when the kernel has rate-
   limit budget, not that the kernel guarantees ordering for every
   scrub. Superblock-shape coverage is not exercised by this repro
   because dm-dust corrupts a data extent block, not a superblock;
   the regex's superblock branch is verified by static inspection of
   `scrub.c:2815/2821` rather than runtime.
4. `mdbook build docs` -- linkcheck passes; new troubleshooting section
   renders cleanly.
5. Sanity-check: a broad `just test-vm` is not required (no
   module/systemd/VM-behavior surface is touched); the new repro added
   in step 3 is the targeted VM coverage, per AGENTS.md "Test scope"
   guidance.
