# Mirror the per-orphan `[warn]` line in `braid lock --dry-run`

## Context

`braid lock --dry-run` is the user's last chance to inspect what
`braid lock` will actually do before committing. Today, when one or more
orphan `braid-*` mappers are present in `/dev/mapper`, the real run
emits a per-orphan `[warn] orphaned mapper {name} (not in pool.json --
likely a prior crash)` line just before each orphan close, but the
dry-run preview never shows that warn. The preview shows the orphan
*close steps* (annotated `(orphan)`), but loses the contextual
explanation of why the orphan is there.

This is the same shape of dry-run/real-run divergence that commit
`d4c00d3 fix(lock): mirror /dev/mapper scan warn in dry-run preview`
fixed for the *scan-failure* case, and that the existing
`dry_run_preview_warns_when_list_dir_fails` unit test was written to
prevent. The per-orphan case slipped through because `plan_lock` only
adds a `PreviewNote::Warn` for `scan_orphan_mappers` returning `Err`,
not for each entry of a successful `Ok(v)`.

The existing `dry_run_preview_mounted_happy_path` test (cli/src/lock.rs:1259-1303)
even *codifies* the divergence: it seeds `braid-ccc` as an orphan in
the dry-run scenario and asserts `!output.contains("[warn]")`.

Goal: make the per-orphan warn body byte-identical between dry-run and
real run by routing it through `LockPlan::notes`, the same channel the
scan-failure warn already uses.

## Fix

All Rust edits are in `cli/src/lock.rs`.

### 1. `plan_lock`: push one `PreviewNote::Warn` per orphan

At cli/src/lock.rs:440-447, after `scan_orphan_mappers` returns
`Ok(v)`, push a warn note per entry before assigning to
`orphan_mappers`. Use a shared body formatter so dry-run and real-run
can never drift, mirroring `orphan_scan_warn_body` (cli/src/lock.rs:60-62).

Add a helper next to `orphan_scan_warn_body`:

```rust
/// Message body (no `[warn]` prefix) for a per-orphan mapper note.
/// Shared between the dry-run preview and the real-run prelude so both
/// branches use identical wording.
fn orphan_mapper_warn_body(entry: &str) -> String {
    format!("orphaned mapper {entry} (not in pool.json -- likely a prior crash)")
}
```

Update the `plan_lock` orphan branch:

```rust
let orphan_mappers = match scan_orphan_mappers(fs, membership) {
    Ok(v) => {
        for entry in &v {
            notes.push(PreviewNote::Warn(orphan_mapper_warn_body(entry)));
        }
        v
    }
    Err(e) => {
        notes.push(PreviewNote::Warn(orphan_scan_warn_body(&e)));
        Vec::new()
    }
};
```

### 2. `LockPlan::execute`: drop the inline orphan-warn emit

At cli/src/lock.rs:340-346, remove the inline
`eprint!("{}", line(StatusTag::Warn, ...))` for the orphan warn. The
existing prelude loop at cli/src/lock.rs:199-203 already emits every
`PreviewNote::Warn` from `self.notes` to stderr before any mutation,
so once the warns live in `notes` the work is already done.

The orphan close-loop body keeps the `[wait]`/`[ok]`/`[fail]`/`[warn]`
status rows that *describe the close action itself* (cli/src/lock.rs:347-382).
Only the standalone "this is an orphan because..." warn at lines
340-346 is removed.

### 3. Refresh the doc + prelude comments to match the new invariant

The current docs only mention scan-failure warns; future refactors need
to know per-orphan warns are also part of the plan invariant.

- `plan_lock` doc comment (cli/src/lock.rs:402-407): change
  *"any `PreviewNote::Warn` accumulated from a failed orphan scan"* to
  cover both cases, e.g.
  *"plus one `PreviewNote::Warn` per detected orphan mapper, or a single
  warn for a failed orphan scan."*
- Execute prelude comment (cli/src/lock.rs:196-198): rewrite the
  "today's real-run orphan-scan warn sits at the top of the work
  section" sentence so it reads
  *"the plan carries the orphan-scan-failure warn and one warn per
  detected orphan mapper as `PreviewNote::Warn`; this loop is the
  single emit point for both."*

Wording is illustrative; the requirement is that both comments name
both warn sources so the channel-of-truth invariant is obvious from a
reader's perspective.

### Behavior change to flag

Today, real-run output interleaves
`warn-then-close, warn-then-close, ...` per orphan. After the fix, all
orphan warns render upfront via the existing notes prelude, then the
work block runs:

```
[warn] orphaned mapper braid-ccc (not in pool.json -- likely a prior crash)
[warn] orphaned mapper braid-ddd (not in pool.json -- likely a prior crash)
[wait] pool: unmounting /mnt/storage...
[ok]   pool: unmounted /mnt/storage
[wait] disk aaa: locking...
[ok]   disk aaa: locked
[wait] disk ccc: locking (orphan)...
[ok]   disk ccc: locked (orphan)
[wait] disk ddd: locking (orphan)...
[ok]   disk ddd: locked (orphan)
```

This is acceptable:

- The orphan-close steps still carry `(orphan)` in their status rows, so
  the user can pair each warn to its close by name.
- The shape now matches the structured dry-run preview (notes first,
  then steps), so dry-run and real-run stay byte-aligned.

## Test changes

### Update: `dry_run_preview_mounted_happy_path` (cli/src/lock.rs:1259-1303)

The current test seeds `braid-ccc` as an orphan and asserts
`!output.contains("[warn]")`. After the fix the preview *must* contain
the orphan warn. Flip the assertion to:

- assert the preview contains the exact body
  `orphaned mapper braid-ccc (not in pool.json -- likely a prior crash)`
  on a `[warn]` line;
- assert that warn renders **before** the first step row (the notes
  block sits at the top of the preview, by `Preview::render`'s
  contract).

### Add: focused per-orphan dry-run preview unit test

Mirror `dry_run_preview_warns_when_list_dir_fails` (cli/src/lock.rs:1198-1245)
for the successful-scan-with-orphan case. Pin the wording end-to-end so a
future refactor can't silently drop the warn:

```rust
/*
 * Intent: `braid lock --dry-run` preview surfaces a `[warn]` line per
 *   orphan mapper found in /dev/mapper.
 * Why it exists: the dry-run branch previously omitted the per-orphan
 *   warn that the real run prints, so users couldn't see WHY an
 *   `(orphan)` close step was about to run from the preview alone.
 * Scenario: prior crash left braid-ccc as an orphan; user runs
 *   `braid lock --dry-run` and must see the explanatory warn body
 *   above the orphan close step, identical to the real-run wording.
 */
#[test]
fn dry_run_preview_warns_per_orphan_mapper() { ... }
```

The test should:

- mock a mounted pool with two membership mappers (`braid-aaa`,
  `braid-bbb`) and one orphan (`braid-ccc`);
- call `plan_lock(...).preview().render()`;
- assert the rendered output starts with the literal
  `[warn] orphaned mapper braid-ccc (not in pool.json -- likely a prior crash)\n`;
- assert it still contains the `close LUKS mapper braid-ccc (orphan)`
  step further down.

### Update: VM test pins the real-run warn

`tests/cli/braid-lock-orphan.py:76-90` already runs `braid lock` with
an orphan present and captures stderr to `/tmp/lock-orphan.err`. Today
it only asserts `orphan_wait < orphan_ok`. Extend that subtest to pin
the new real-run shape:

- assert exactly one occurrence of the literal
  `[warn] orphaned mapper braid-orphan (not in pool.json -- likely a prior crash)`
  in `lock_err` (counts catch a regression that re-introduces a
  duplicate inline emit);
- assert that `[warn]` line appears **before** the first work row --
  e.g. before either of `[wait] pool: unmounting` or
  `[wait] disk disk1: locking...` -- to pin the notes-prelude
  ordering;
- keep the existing `orphan_wait in lock_err` and
  `lock_err.find(orphan_wait) < lock_err.find(orphan_ok)` assertions
  unchanged.

This catches three real-run regressions the dry-run unit tests can't
see: (a) duplicate warns if the inline emitter is left in by accident,
(b) the warn dropping out entirely if the prelude loop is refactored
away, and (c) the warn drifting back below the work rows.

### Existing tests that should stay green untouched

- `dry_run_preview_warns_when_list_dir_fails` (cli/src/lock.rs:1198-1245)
  -- scan-failure path, unaffected.
- `dry_run_preview_nothing_to_do` (cli/src/lock.rs:1315-1330) -- empty
  fs, unaffected.
- `lock_closes_orphaned_mapper` (cli/src/lock.rs:1077-1121) -- real-run
  orphan close. Does not assert on stderr text.
- `tests/cli/braid-lock.py:160-175` (Test 5: dry-run stream routing) --
  no-op preview, no orphans. Unaffected.

## Files

- `cli/src/lock.rs`
  - add `orphan_mapper_warn_body` helper (alongside
    `orphan_scan_warn_body` at lines 60-62);
  - update the `plan_lock` orphan branch at lines 440-447;
  - delete the inline `eprint!` for the orphan warn at lines 340-346;
  - refresh the `plan_lock` doc comment (lines 402-407) and the
    execute prelude comment (lines 196-198) to name both warn sources;
  - update `dry_run_preview_mounted_happy_path` test at lines 1259-1303;
  - add new `dry_run_preview_warns_per_orphan_mapper` test next to it.
- `tests/cli/braid-lock-orphan.py`
  - extend the existing `braid lock closes membership mappers and
    orphan` subtest at lines 76-90 with the count + ordering
    assertions described above.

No NixOS module changes, no doc/decision changes.

## Out of scope (noted)

- Routing the real-run prelude through
  `preview::render_notes_for_stderr_with` instead of the hand-rolled
  Warn-only loop at cli/src/lock.rs:199-203 is a tasteful unification
  with [`docs/decisions/022-dry-run-preview-model.md`](docs/decisions/022-dry-run-preview-model.md),
  but is not needed for byte-correctness once the warns live in
  `notes`. Defer to a separate change.

## Verification

1. `just test-rust` -- the new and updated unit tests must pass; the
   broader lock-test surface (close paths, umount-busy, retry, scan
   failure) must stay green.
2. `just test-vm braid-lock braid-lock-orphan` -- the existing
   no-op-dry-run VM test plus the orphan VM test (now extended with
   warn count + ordering) must stay green; the orphan VM test
   exercises the real-run path that now sources its warn from
   `LockPlan::notes`.
3. Manual sanity check (optional): in the orphan VM, `braid lock
   --dry-run` should now print a `[warn] orphaned mapper braid-orphan
   ...` line above the step block on stdout, byte-matching the warn
   body the real run prints to stderr.
