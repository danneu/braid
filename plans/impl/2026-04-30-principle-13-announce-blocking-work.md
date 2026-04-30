# Plan: Promote `[wait]` rule to a project principle and bring remaining commands into compliance

## Context

Commit `4150cf02` ("feat(unlock): announce per-disk open and mount with
[wait] rows") closed the silent gaps in `braid unlock` and the shared
mount helpers. ADR
[`021-wait-in-unlock.md`](../../docs/decisions/021-wait-in-unlock.md)
deliberately scoped the rule narrowly and listed every other interactive
command that still has un-announced blocking work, with the explicit
rule: promote to a project principle once the list is empty.

This follow-up does that promotion in one change:

- Add **Principle 13. Announce blocking work** to
  `docs/principles.md`.
- Mark ADR 021 `Superseded by Principle 13`, strike its "Path to
  promotion" list.
- Insert `[wait]` rows in the seven remaining command surfaces
  (`add`, `replace`, `remove`, `remove-missing`, `recover`'s replay
  tail, `lock`, `enroll`) plus the shared credential verifier
  (`credential_verify.rs`) and the recover kernel-replace barrier
  (`wait_for_kernel_replace_to_finish`).
- Convert the existing ad-hoc announcement `eprintln!("Doing X...")`
  strings to canonical `[wait]` / `[ok]` rows so output is uniformly
  consistent. Convert best-effort close failure prose
  (`Warning: failed to close LUKS mapper ...`,
  `cleanup: failed to close ...`) to canonical same-subject
  `[warn]` rows so every `[wait]` is closed per Principle 13.
- Pin every new row. **VM tests where reachable; deterministic
  Rust unit tests with a shared stderr-capture seam for branches
  that cannot be composed from existing braid commands.** The
  Rust-only pins are: the add Pass-1 closed `PresentLuks` unlock
  rows, the `LuksCleanupGuard::Drop` rollback rows (success and
  warn), the `wait_for_kernel_replace_to_finish` running-then-
  finished rows, the same function's err-after-wait `[warn]` row,
  and the best-effort close failure `[warn]` rows in
  `pool::evict_present_device` and `replace.rs`'s live-replace
  arm. Every other new row is pinned in a VM test.

The user-facing motivation is unchanged from ADR 021: cryptsetup
Argon2 derivations and btrfs `balance` / `replace` / `device remove`
leave the terminal idle for seconds-to-hours with no streaming output
until progress polling kicks in. Without an upfront `[wait]` row the
operator cannot tell the CLI from a hang.

## Scope (this round)

In:

1. New principle in `docs/principles.md` (Principle 13).
2. ADR 021 supersession + strike-through of its path-to-promotion list.
3. `docs/index.md` count bump (12 -> 13) and ADR 021 status flip.
3a. **Dry-run stderr contract update.** `README.md:112` and
   `docs/decisions/012-intent-cli.md` both currently state (or imply)
   that a successful dry-run leaves stderr empty. The new
   `braid enroll --dry-run` keyfile probe rows break that contract.
   Update both docs to explicitly allow canonical
   `[wait]`/`[ok]`/`[skip]` rows on dry-run stderr around any
   Argon2-bounded probe that runs during preview generation. The
   structured preview itself stays on stdout. See the
   "Documentation updates" subsection below for exact text.
4. `[wait]`/`[ok]` rows wired into all remaining blocking-work
   surfaces:
   - `add.rs` (both `ensure_luks_open` paths -- Pass 1 closed
     `PresentLuks` recoverable + Pass 2 fresh disks -- plus
     `luks_format`, in-add keyfile enroll, post-add
     `pool_balance_raid1`, and the rollback-path
     `LuksCleanupGuard::Drop` close)
   - `replace.rs` (luks_format, both `ensure_luks_open` arms,
     in-replace keyfile enroll, the live/missing replace
     kickoffs, and the live-replace old-mapper close)
   - `remove_missing.rs` (`pool_remove_device_using` call site)
   - `recover.rs` (both `relock_and_remount`'s umount + per-mapper
     close cycle and `replay_post_mutation`'s resume + soft
     balance replay; plus `wait_for_kernel_replace_to_finish`'s
     waiting-only [wait]/[ok] pair)
   - `lock.rs` (main umount + per-disk close + orphan close)
   - `enroll_key_file.rs` (`luks::enroll_key_file`)
   - `pool.rs` shared helpers (`evict_present_device` -- including
     the trailing best-effort LUKS close, `maybe_restore_raid1`)
5. VM-test assertions for every new row, indexed by a row coverage
   matrix. Every category named by Principle 13 is either pinned by a
   test or has an explicit row-emission rationale (none are claimed
   "indirectly covered" without a deterministic substring + ordering
   assertion).

Out:

- A status-row wrapper helper that auto-emits `[wait]` / `[ok]` around
  any subprocess. Defer; refactor cost vs. payoff is not justified
  while only mount.rs and the seven sites need it.
- Promoting the `cryptsetup luksHeaderBackup` calls to `[wait]` rows.
  Header backup is fast-bookkeeping and is explicitly exempted by the
  new principle.
- `cmd_unlock` already-mounted short-circuit and the bootstrap-mkfs
  path in `add`. Both are exempt under Principle 13 (no blocking
  Argon2/balance work).
- (No deferrals related to close-failure prose. The earlier
  iteration of this plan kept the legacy `Warning:` /
  `cleanup:` prose lines unchanged on the close-failure branches,
  but Principle 13 requires every `[wait]` to close with `[ok]`,
  `[fail]`, `[warn]`, `[skip]`, or error propagation. Best-effort
  closes that exit 0 leave a dangling wait, so failure-branch
  conversion to canonical `[warn]` rows is **in scope this round**
  -- see the per-file edits for `pool.rs::evict_present_device`,
  `replace.rs` live-replace old-mapper close, and
  `add.rs::LuksCleanupGuard::Drop`. Each `[warn]` row is pinned by
  a Rust unit test using the shared stderr-capture seam.)

## Principle 13: exact text

Insert in `docs/principles.md` immediately after the body of
Principle 12 (line 61), before the trailing `---` separator at
line 63. The surrounding file uses Unicode em-dashes (`—`) and the
right-arrow (`→`) in `[Why →]` links; match that convention rather
than ASCII even though CLI output uses ASCII (per CLAUDE.md, the
"surrounding file already uses Unicode" exception applies here):

```markdown
## 13. Announce blocking work

Every interactive command emits a `[wait]` row before any subprocess
that can stall the terminal long enough for the user to wonder
whether the CLI has hung. The bound categories:

- cryptsetup Argon2 operations (`luksFormat`, `luksOpen`,
  `luksAddKey`, `--test-passphrase`);
- `cryptsetup close` (single attempt or busy-retry loop);
- btrfs `balance`, `replace`, and `device remove` (potentially hours);
- `mount` and `umount` (kernel can drain in-flight I/O / replace
  workers / inhibitors).

A `[wait]` row is closed by one of:

- the same command's paired success row (`[ok]   {same subject}: ...`)
  on the success path,
- a same-subject `[fail]` row on a known failure path (e.g.
  `lock.rs`'s umount failure),
- a same-subject `[warn]` row on a non-fatal best-effort failure
  (e.g. `pool::evict_present_device`'s trailing LUKS close, or
  `wait_for_kernel_replace_to_finish`'s status-poll error -- the
  command continues despite the failure, and the warn row tells
  the user the wait window is closed without success),
- a same-subject `[skip]` row on a successful negative or no-op
  probe (e.g. `braid enroll`'s pre-mutation keyfile probe finding
  the keyfile not yet enrolled -- the work the wait announced
  completed, the answer is "no work yet"),
- or the command's normal error output (`MountError` / `LuksError` /
  `PoolError` propagation) on uncaught error paths.

A `[wait]` followed by none of these closers (i.e., success, fail,
warn, skip, or non-zero exit) is a documentation bug.

Fast bookkeeping that completes well under a second
(`mkfs.btrfs` on a fresh disk, `btrfs device add`,
`btrfs filesystem resize`, `btrfs device scan`,
`btrfs device scan --forget`, `cryptsetup luksHeaderBackup`,
`cryptsetup status`, `blkid`, JSON parses, journal writes,
`pool.json` saves, sysfs reads) does not warrant a row.

Rendering uses `status_tag::status_line(StatusTag::Wait, ...)`
against `color_enabled_for_stderr()` so plain stderr captures
contain unwrapped `[wait]` bytes and TTY output picks up the gray
ANSI tag. [Why →](decisions/021-wait-in-unlock.md)
```

Note: the user-facing **CLI output** in `[wait]` row bodies still
uses ASCII per `AGENTS.md` -- only the principles.md prose is
Unicode-styled to match the existing file.

## ADR 021 supersession (`docs/decisions/021-wait-in-unlock.md`)

- Line 11: replace `Status: Active` with
  `Status: Superseded by [Principle 13](../principles.md#13-announce-blocking-work)`.
- Lines 13-15 (the `> Principles:` blockquote with `(none yet ...)`):
  replace with
  ```markdown
  > Principle: [13. Announce blocking work](../principles.md#13-announce-blocking-work)
  ```
- Lines 90-138 (the entire `## Path to promotion` section): replace
  the heading and the bullet list with a brief resolution note:
  ```markdown
  ## Promotion outcome

  Promoted to Principle 13 once `add`, `replace`, `remove`,
  `remove-missing`, `recover`'s replay tail, `lock`, and `enroll`
  were brought into compliance.
  ```
- Leave `## Context`, `## Options considered`, `## Decision`, `##
  Tradeoffs accepted`, and `## See` intact -- they remain the
  historical "why" the principle exists.

## Documentation updates: dry-run stderr contract

The new `braid enroll --dry-run` keyfile probe emits canonical
`[wait]`/`[ok]`/`[skip]` rows to stderr (see the
`enroll_key_file.rs` per-file edit below). That contradicts the
current "successful dry-run leaves stderr empty" contract stated
in two user-visible docs. Both must be updated as part of this
change, otherwise the new behavior silently violates documented
invariants:

- **`README.md` line 112** (current text: `... warnings that
  qualify the preview are part of it, and stderr stays empty.
  Real runs may still print confirmations, progress, and failures
  to stderr.`): replace the `stderr stays empty` half-sentence and
  append a new paragraph carving out the probe-row exception:

  ```markdown
  ## Preview with --dry-run

  Every mutating command (`add`, `remove`, `remove-missing`, `replace`, `unlock`, `lock`, `recover`, `enroll`) takes `--dry-run`. A successful dry-run prints one complete preview to stdout -- warnings that qualify the preview are part of it. Real runs may still print confirmations, progress, and failures to stderr.

  `--dry-run` may also emit canonical `[wait]`/`[ok]`/`[skip]` status rows to stderr around any blocking probe that runs during preview generation -- for example, `braid enroll --dry-run` runs a passphrase-free `cryptsetup open --test-passphrase --key-file` against each disk to detect already-enrolled state, and announces that Argon2-bounded probe per Principle 13 ("announce blocking work"). These rows do not count as preview output -- the structured preview still lives entirely on stdout, and stderr is otherwise quiet on success.
  ```

- **`docs/decisions/012-intent-cli.md` line 51** (the
  `--dry-run performs side-effect-free, passphrase-free LUKS
  probes only ...` paragraph): append a new paragraph immediately
  after the existing one:

  ```markdown
  The dry-run preview itself stays on stdout. Side-effect-free probes that nevertheless do bound blocking work -- specifically the Argon2-bounded `--test-passphrase` evaluation in `braid enroll --dry-run` -- emit canonical `[wait]`/`[ok]`/`[skip]` status rows to stderr per [Principle 13. Announce blocking work](../principles.md#13-announce-blocking-work). The previous "successful dry-run leaves stderr empty" contract is intentionally relaxed for this case: an Argon2 derivation runs whether or not the user can see it, and silent dry-runs that take seconds-to-minutes look like hangs. The structured preview output is unchanged.
  ```

The two docs must be updated in the same change as the
`enroll_key_file.rs` source change. Without this, the new
behavior silently violates a user-visible contract.

## `docs/index.md` updates

- Line 17 (current text: `Twelve canonical invariants spanning
  resilient boot, ..., and pool-mutation serialization.`): change
  `Twelve canonical invariants` to `Thirteen canonical invariants`
  and append `, and blocking-work announcement` so the closing reads
  `... pool-mutation serialization, and blocking-work
  announcement.` (the file uses Unicode em-dashes; do not change the
  surrounding em-dash at the start of the line).
- Line 52 (the ADR 021 entry): change the `**Active.**` lead to
  `**Superseded by [Principle 13](principles.md#13-announce-blocking-work).**`
  and rewrite the trailing summary in past tense, e.g.
  "`braid unlock` (and `braid recover`'s shared mount tail) emitted
  a `[wait]` row before per-disk LUKS open and before the mount
  phase; promotion to a project-wide principle landed once the rest
  of the interactive commands complied." (Use the existing line's
  em-dash to lead in.)
- **Important:** the link target is `principles.md#...` (relative
  to `docs/`), **not** `../principles.md#...`. The `../` form would
  point outside `docs/`. Inside `docs/decisions/021-wait-in-unlock.md`
  the relative target is `../principles.md#...` (one level up out of
  `decisions/`).

## Per-file source edits

Convention used below: a "[wait] before / [ok] after" pair means
inserting

```rust
eprint!(
    "{}",
    status_line(StatusTag::Wait, color_enabled, &format!("..."))
);
```

immediately before the call, and a paired `[ok]` row immediately
after success. Existing post-fact `eprintln!("LUKS formatted: {}",
by_id);` lines are **converted** (not duplicated) to canonical `[ok]`
rows so we do not double-announce. Wording follows the terse
mount.rs precedent (no `(Argon2)` / `(can take hours)` annotations).

### `cli/src/credential_verify.rs` (shared credential verifier)

`verify_credential_for_targets` (lines 29-69) emits one `[wait]
{kind}: checking against {name}...` per target via the supplied
`emit` closure but never emits a terminal row. Under Principle 13
this is a documentation bug: every wait must be closed by `[ok]`,
`[fail]`, `[warn]`, `[skip]`, or error propagation, and the
cryptsetup `--test-passphrase` Argon2 derivation is explicitly
listed as bound work.

- Add a public helper `credential_ok_line(kind, color_enabled,
  name) -> String` next to the existing `credential_wait_line` in
  `status_tag.rs`. Body wording: `"{kind}: accepted by {name}"`,
  using the same `kind.label()` private helper that
  `credential_wait_line` uses. (`label()` is a private associated
  fn on `CredentialKind` -- both row helpers live in the same
  module so it does not need to be made public.)
- In `credential_verify.rs`, import `credential_ok_line` alongside
  `credential_wait_line` and `CredentialKind`.
- After the `Ok(VerifyOutcome::Authenticated) =>` arm at line 53,
  emit the paired terminal row through the same `emit` closure:
  `emit(&credential_ok_line(kind, color_enabled, &target.name));`.
  Do **not** open-code `kind.label()` at the call site -- it is
  not pub.
- Update the existing
  `credential_wait_line_formats_known_credentials` test (line 231)
  to also cover `credential_ok_line` for both `Passphrase` and
  `KeyFile` variants and both color states.
- Update the existing tests in `credential_verify.rs::tests`
  (`verify_credential_for_targets_authenticates_all_targets_in_order`
  at line 188, `verify_credential_for_targets_stops_at_first_rejection`
  at line 217, and any other order-asserting tests) so the
  `expected_waits` helper interleaves the new `[ok]` lines with
  the existing `[wait]` lines. Successful targets contribute a
  wait+ok pair; the rejected target (in the stop-at-first-rejection
  case) contributes only a wait line, since the helper returns
  before emitting the ok.
- The Rejected and Luks-error cases still return `Err` to the
  caller without emitting a terminal row. The wait row is closed
  by the caller's `MountError` / `LuksError` / validation prose
  per Principle 13's error-propagation closer.

### `cli/src/enroll_key_file.rs` (dry-run keyfile probe + idempotent check)

Two sites in this file emit a `[wait]` row without a terminal
closer:

1. **Idempotent check (line 187):** `emit_credential_wait_line(
   CredentialKind::KeyFile, ..., name)` is followed by a direct
   `luks::verify_key_file` call. On `Authenticated` the existing
   `eprintln!("ok: {} -- keyfile already enrolled", name)` runs,
   but it is informational prose, not a canonical `[ok]` row. On
   `Rejected` no row fires.
2. **Dry-run probe (line 567):** `luks::verify_key_file` is called
   inside the `if dry_run && !generate` block with no wait row at
   all -- the same Argon2 cost runs but the user sees nothing.

Convert both to the canonical form using the new helpers:

- For (1): replace `eprintln!("ok: {} -- keyfile already enrolled",
  name)` with
  `[ok]   keyfile: already enrolled on {name}`. On the `Rejected`
  branch (currently `=> {}`), emit
  `[skip] keyfile: not yet enrolled on {name}` so the wait is
  closed before the loop continues to per-disk passphrase verify.
  The skip row's `not yet enrolled` body matches the dry-run
  preview's `PreviewNote::PerDisk { level: NoteLevel::Skip,
  message: "keyfile already enrolled" }` framing.
- For (2): immediately before `luks::verify_key_file`, emit
  `[wait] keyfile: checking against {name}...` via
  `emit_credential_wait_line(CredentialKind::KeyFile,
  color_enabled_for_stderr(), name)`. After the call, on
  `Authenticated` emit
  `[ok]   keyfile: already enrolled on {name}` (matching the
  existing `notes.push(PreviewNote::PerDisk { ..., message:
  "keyfile already enrolled" })`); on `Rejected` emit
  `[skip] keyfile: not yet enrolled on {name}` before
  `needs_enroll.push(...)`.
- The two sites live in different functions, so each binds its
  own `let color_enabled = color_enabled_for_stderr();`:
  - The idempotent check (1) lives inside `plan_enrollment`
    (function starting around line 146; site is line ~187); add
    the binding once at the top of that function and reuse for
    every iteration of the candidate loop.
  - The dry-run probe (2) lives inside `plan_enroll` (the public
    planning function around line 547; site is line ~567 inside
    the `if dry_run && !generate` block); add the binding at the
    top of that block and reuse across the per-candidate loop.
  Do **not** try to share a single binding -- the two functions
  are independent entry points.

This brings the `--test-passphrase` Argon2 wait window in
enroll's preview path under Principle 13's invariant.

### `cli/src/pool.rs` (shared helpers)

Two functions emit canonical rows on behalf of multiple commands.
Putting `[wait]`/`[ok]` rows here means `cli/src/remove.rs` itself
needs no edits, and `replace`+`remove-missing` get the
soft-balance-restore row "for free".

- Add `use crate::status_tag::{StatusTag, color_enabled_for_stderr,
  status_line};` to the existing imports.
- `maybe_restore_raid1` (pool.rs:161-179): bind
  `let color_enabled = color_enabled_for_stderr();` once at the top.
  Replace line 174 (`eprintln!("Restoring RAID1 redundancy (soft
  balance)...")`) with
  `[wait] pool: restoring RAID1 redundancy...`. Replace line 176
  (`eprintln!("Soft balance complete.")`) with
  `[ok]   pool: RAID1 redundancy restored`.
- `evict_present_device` (pool.rs:295-385 in the post-edit file):
  bind `color_enabled` once at the top.
  - Line 313 (`eprintln!("Converting pool from RAID1 to single
    profile...")`): replace with
    `[wait] pool: balancing RAID1 to single profile...`. Add
    `[ok]   pool: balanced to single profile` after the
    `pool_balance_single` call returns Ok.
  - Line 317 (`eprintln!("Removing {} from pool (data will
    migrate)...", mapper)`): replace with
    `[wait] pool: removing {mapper}...`. Add
    `[ok]   pool: {mapper} removed` after `pool_remove_device`
    returns Ok.
  - **Best-effort trailing LUKS close** (post-edit file lines
    ~367-382): wrap the `runner.run(&CmdRequest::CryptsetupClose
    {...})` call with
    `[wait] disk {label}: locking...` before, and
    `[ok]   disk {label}: locked` after the success branch
    (`Ok(r) if r.exit_status == 0`). The `{label}` body strips the
    `braid-` prefix from the mapper for cross-command consistency
    with `lock.rs` (so the row reads `[wait] disk disk2:
    locking...` rather than `[wait] disk braid-disk2: locking...`).
    On failure, replace the existing
    `eprintln!("Warning: failed to close LUKS mapper {} (exit {})", ...)`
    and `eprintln!("Warning: failed to close LUKS mapper {}: {}", ...)`
    prose lines with same-subject canonical
    `[warn] disk {label}: lock failed (exit {})` and
    `[warn] disk {label}: lock failed ({err})` rows. This closes
    the wait per Principle 13's `[warn]` clause -- best-effort
    failure on a path that exits 0 must not leave a dangling
    `[wait]`. The detail (exit code, error text) is preserved in
    the warn body so existing operator information is not lost.

### `cli/src/add.rs`

Already imports `color_enabled_for_stderr`. The execute method binds
`color_enabled` near the top (search for `color_enabled_for_stderr()`
inside `execute`); reuse that local. Promote it to a function-scope
`let` if not already. There are **two** `ensure_luks_open` call
sites in `add.rs` -- the closed `PresentLuks` recoverable path
runs Argon2 too and was missing from the prior plan revision.

- Update the `use crate::status_tag::...` line to also import
  `StatusTag` and `status_line`.
- **Pass 1, Line 493** (`ensure_luks_open` for closed
  `PresentLuks` recoverable disks, inside the `if !mapper_open {`
  guard): insert `[wait] disk {name}: unlocking...` before.
  Convert line 495 (`eprintln!("LUKS opened: {} → {}", by_id,
  mn);`) to `[ok]   disk {name}: unlocked`.
- Pass 2, Line 584 (`luks_format(...)`): insert
  `[wait] disk {name}: formatting LUKS...` before. Convert line 585
  (`eprintln!("LUKS formatted: {}", by_id);`) to
  `[ok]   disk {name}: LUKS formatted`.
- Pass 2, Line 588 (`backup_luks_header`): no `[wait]` (fast
  bookkeeping). Leave the existing
  `eprintln!("LUKS header backed up: {}", ...)` unchanged.
- Pass 2, Line 590 (`ensure_luks_open(...)`): insert
  `[wait] disk {name}: unlocking...` before. Convert line 592
  (`eprintln!("LUKS opened: {} -> {}", by_id, mn);`) to
  `[ok]   disk {name}: unlocked`.
- Pass 2, Line 595 (optional `enroll_key_file` inside add): insert
  `[wait] disk {name}: enrolling keyfile in slot 1...` before.
  Convert line 596 (`eprintln!("Keyfile enrolled in slot 1: {}",
  by_id);`) to `[ok]   disk {name}: keyfile enrolled in slot 1`.
- Lines 614-621 (bootstrap mkfs path): leave as-is. mkfs is exempt.
- Lines 644-665 (`pool_add_device` loop): no `[wait]` (device add is
  exempt). Leave the existing `eprintln!("Device added to pool:
  {}", mp);` unchanged.
- Line 679 (`eprintln!("Balancing to RAID1...")`): replace with
  `[wait] pool: balancing to RAID1...`. Convert line 681
  (`eprintln!("Balance complete.")`) to
  `[ok]   pool: RAID1 balance complete`.
- **`LuksCleanupGuard::Drop`** (lines 216-242): the rollback-path
  `cryptsetup close` is in scope under Principle 13's
  `cryptsetup close` category. Convert this site as follows:
  - Bind `let color_enabled = color_enabled_for_stderr();` once
    inside `Drop::drop`, after the `if !self.armed { return; }`
    guard and before the `for mapper in self.mappers.iter().rev()`
    loop. Reuse on every iteration.
  - Before `self.runner.run(&CmdRequest::CryptsetupClose {...})`,
    emit `[wait] disk {label}: locking (cleanup)...` (deriving
    `{label}` by stripping the `braid-` prefix from `mapper`).
  - On success (`Ok(r) if r.exit_status == 0`): replace the existing
    `eprintln!("cleanup: closed LUKS mapper {}", mapper);` with
    `[ok]   disk {label}: locked (cleanup)`.
  - On failure (`Ok(r)` non-zero or `Err(e)`): replace the existing
    `eprintln!("cleanup: failed to close LUKS mapper ...")` lines
    with canonical
    `[warn] disk {label}: lock failed (cleanup, exit {})` /
    `[warn] disk {label}: lock failed (cleanup, {err})` rows. The
    rollback runs after a primary failure that produces a non-zero
    exit; on that exit path Principle 13's "command's normal error
    output" closer suffices for the entire rollback, but the
    per-mapper `[warn]` row still tells the user *which* mapper
    failed to close so a follow-up `braid lock` can be informed.
  - The `(cleanup)` annotation distinguishes rollback-path rows
    from primary-path locking rows in mixed log scrapes; this
    mirrors recover.rs's `(recover remount cycle)` annotation.

### `cli/src/replace.rs`

Already imports `color_enabled_for_stderr`. The execute method
already binds `color_enabled` at line 234 inside an unrelated render
call -- promote it to a function-scope `let color_enabled =
color_enabled_for_stderr();` at the top of `ReplacePlan::execute`,
before the journal-pre-flight calls.

- Update the `use crate::status_tag::...` line to also import
  `StatusTag` and `status_line`.
- Line 305 (`luks_format`): insert
  `[wait] disk {new_name}: formatting LUKS...` before. Convert line
  306 to `[ok]   disk {new_name}: LUKS formatted`.
- Line 309-310 (`backup_luks_header`): no `[wait]`. Leave existing
  `eprintln!("LUKS header backed up: ...")` unchanged.
- Line 312 (`ensure_luks_open` in PresentNotLuks arm): insert
  `[wait] disk {new_name}: unlocking...`. Convert line 313 to
  `[ok]   disk {new_name}: unlocked`.
- Line 316 (`enroll_key_file`): insert
  `[wait] disk {new_name}: enrolling keyfile in slot 1...`. Convert
  line 317 to `[ok]   disk {new_name}: keyfile enrolled in slot 1`.
- Line 322 (`ensure_luks_open` in PresentLuks arm, when `mapper_open`
  is false): insert `[wait] disk {new_name}: unlocking...`. Convert
  line 323 to `[ok]   disk {new_name}: unlocked`.
- Lines 357 / 361-364 (the kickoff `eprintln!`s): replace each with
  the corresponding `[wait]` row.
  - Live replace: `[wait] pool: replacing devid {devid} with
    {new_mn}...`
  - Missing replace: `[wait] pool: rebuilding missing devid {devid}
    onto {new_mn}...`
  Convert line 376 (`eprintln!("Replace complete.")`) to
  `[ok]   pool: replace complete`.
- Line 415 (`pool_resize_device`): no `[wait]` (resize is exempt).
- Line 419-426 (`maybe_restore_raid1`): rows come from the
  `cli/src/pool.rs` change above; no edit here.
- **Live-replace old-mapper close** (post-edit file line ~471, the
  `if let ReplaceSource::Live { mapper, .. } = &replace_source` block
  immediately before `pool_resize_device`): wrap the
  `runner.run(&CmdRequest::CryptsetupClose {...})` call with
  `[wait] disk {old_label}: locking...` before, and
  `[ok]   disk {old_label}: locked` after the success branch
  (`Ok(r) if r.exit_status == 0`). Derive `{old_label}` by stripping
  the `braid-` prefix from `mapper.0` for cross-command consistency
  with lock.rs / pool.rs rows. Leave the trailing informational
  `eprintln!("Old device closed. If repurposing the physical disk,
  wipe it separately.")` line intact -- it is user-facing prose
  that complements (does not duplicate) the new `[ok]` row. On
  failure, replace the existing `eprintln!("Warning: failed to
  close LUKS mapper ...", ...)` lines with same-subject canonical
  `[warn] disk {old_label}: lock failed (exit {})` /
  `[warn] disk {old_label}: lock failed ({err})` rows. The
  command continues (replace itself succeeded), exits 0, and the
  warn row is what closes the `[wait]` per Principle 13.

### `cli/src/remove.rs`

**No source-edit needed.** Verified: `cmd_remove` calls
`pool::evict_present_device` for the only mutation, and the new
`[wait]`/`[ok]` rows live in `pool.rs` (see above). Confirm by
re-grepping `eprintln` and `runner.run` in `remove.rs::execute`
during implementation; if any blocking subprocess turns up that
isn't dispatched through `evict_present_device`, list it here as a
follow-up.

### `cli/src/remove_missing.rs`

- Add `use crate::status_tag::{StatusTag, color_enabled_for_stderr,
  status_line};` to imports.
- Inside `execute` (the function containing lines 189-200): bind
  `let color_enabled = color_enabled_for_stderr();` once before the
  journal write. Reuse.
- Line 189 (`eprintln!("Removing missing device (devid {}) from
  pool...", resolved_devid);`): replace with
  `[wait] pool: removing missing devid {resolved_devid}...`. Add
  `[ok]   pool: missing devid {resolved_devid} removed` after
  `pool_remove_device_using` returns Ok.
- The optional soft-balance row comes from `pool::maybe_restore_raid1`
  (covered in the `pool.rs` edit). No edit here for that call.

### `cli/src/recover.rs`

`recover.rs` has **three** blocking-work surfaces:
`relock_and_remount` (the post-self-mount close+remount cycle),
`wait_for_kernel_replace_to_finish` (the kernel-resumed
dev_replace barrier), and `replay_post_mutation` (the
paused-balance resume + post-mutation soft balance replay). Each
gets its own subsection below.

- Add `use crate::status_tag::{StatusTag, color_enabled_for_stderr,
  status_line};` to imports.

**`relock_and_remount` (lines 747-855)** -- recover's
post-self-mount "drop kernel state and remount" cycle:

- Bind `let color_enabled = color_enabled_for_stderr();` at function
  entry.
- Line 759 (`runner.run(&CmdRequest::Umount {...})`): insert
  `[wait] pool: unmounting {mount_point} (recover remount cycle)...`
  before the call. Add a paired
  `[ok]   pool: unmounted {mount_point} (recover remount cycle)`
  after the success branch. The `(recover remount cycle)`
  annotation distinguishes this row from the lock.rs umount row in
  scenarios where both surfaces could be observed in one log scrape.
- Line 789 (`runner.run(&CmdRequest::BtrfsDeviceScanForget {...})`):
  no `[wait]` -- `scan --forget` is fast bookkeeping (kernel-side
  cache eviction).
- Line 808-832 (per-mapper close loop): inside the loop, **only when
  `fs.exists(&mapper_path)` returns true**, emit
  `[wait] disk {name}: locking...` before
  `runner.run(&CmdRequest::CryptsetupClose {...})`. Add a paired
  `[ok]   disk {name}: locked` after the success branch (i.e., when
  `close.exit_status == 0`). When the mapper does not exist
  (`continue` at line 812), emit no row.
- Line 851 (`mount::execute_unlock_and_mount`): no edit -- the
  shared mount helpers already emit their own
  `[wait] disk {name}: unlocking...` and
  `[wait] pool: mounting {mount_point}...` rows. The user will
  thus see, in this branch:

  ```
  [wait] pool: unmounting /mnt/storage (recover remount cycle)...
  [ok]   pool: unmounted /mnt/storage (recover remount cycle)
  [wait] disk disk1: locking...
  [ok]   disk disk1: locked
  [wait] disk disk2: locking...
  [ok]   disk disk2: locked
  [wait] passphrase: checking against disk1...   (from mount.rs)
  [wait] disk disk1: unlocking...                 (from mount.rs)
  [ok]   disk disk1: unlocked                     (from mount.rs)
  [wait] pool: mounting /mnt/storage...           (from mount.rs)
  [ok]   pool: mounted /mnt/storage               (from mount.rs)
  ```

**`wait_for_kernel_replace_to_finish`** (post-edit file lines
~738-762) -- the kernel-resumed dev_replace barrier called from
`RecoverPlan::execute` line 331:

The existing per-percent
`eprintln!("  waiting for kernel to finish resumed dev_replace...
{pct}%")` line only fires when `pct` *changes*; a stalled or
extremely slow resume worker is silent under the old code. Modify
the function to:

1. Take `color_enabled: bool` as an additional argument (passed
   from the caller's `color_enabled_for_stderr()`).
2. Track a `wait_emitted: bool` flag, initially false.
3. On the first `Running { pct }` iteration, emit
   `[wait] pool: waiting for kernel dev_replace to finish...`
   and set `wait_emitted = true` *before* the existing
   percent-progress line. Continue to emit the `  ... {pct:.1}%`
   progress sub-line on subsequent percent changes (drop the
   leading "waiting for kernel to finish resumed dev_replace"
   prefix from the sub-line so it reads as a continuation of
   the canonical wait row).
4. On `Finished | None`, if `wait_emitted` is true, emit
   `[ok]   pool: kernel dev_replace finished` before returning.
   If `wait_emitted` is false, return silently (no wait was
   observed -- the kernel already finished).
5. On the early-return `Err(_)` paths (status command or parse
   failure), if `wait_emitted` is true, emit
   `[warn] pool: kernel dev_replace status check failed --
   proceeding` before returning. If `wait_emitted` is false,
   return silently.

Note on row literal spacing: `status_line` pads `[ok]` to three
spaces (`[ok]   body`) for column alignment but pads `[warn]`,
`[fail]`, `[skip]`, and `[wait]` to one space (`[warn] body`,
etc.). All planned `[warn]` row literals in this plan use the
one-space form to match what `status_line(StatusTag::Warn, ...,
body)` actually renders -- exact-row test assertions must match
this spacing.

Update the call site at recover.rs:331 to pass `color_enabled`.
The enclosing `RecoverPlan::execute` method (lines 199-420) does
not currently bind a function-scope `color_enabled`; add
`let color_enabled = color_enabled_for_stderr();` near the top of
`execute` (after the `let RecoverPlan { ... } = self;` destructure
at line 219, before the `credential` resolution block). Reuse this
binding for the wait call and any future `[wait]`/`[ok]` rows in
the same scope. The existing call at line 215 (inside
`render_notes_for_stderr_with`) is fine to leave as a direct
`crate::status_tag::color_enabled_for_stderr()` call since it
precedes the destructure.

**`replay_post_mutation` (lines 627-686)** -- post-recover replay
of the mutation's owed maintenance:

- Bind `let color_enabled = color_enabled_for_stderr();` at function
  entry.
- Lines 637-640 (`Replaying post-replace resize`): no `[wait]`
  (resize is exempt). Leave the existing `eprintln!` as-is so the
  principle's exemption list stays honest.
- Lines 651-657 (paused-balance resume): replace line 651-654 with
  `[wait] pool: resuming paused balance left by interrupted
  {label}...`. Convert line 657 (`Balance resume complete.`) to
  `[ok]   pool: balance resume complete`.
- Lines 661-667 (post-X RAID1 soft balance): replace line 661-664
  with
  `[wait] pool: replaying post-{label} RAID1 soft balance (skip
  already-RAID1 chunks)...`. Convert line 667 (`Balance replay
  complete.`) to `[ok]   pool: RAID1 soft balance replay complete`.

### `cli/src/lock.rs`

`color_enabled` and the `line` closure are already in scope inside
`LockPlan::execute` at lines 238-239. Reuse.

- Line 257 (`runner.run(&CmdRequest::Umount {...})`): insert
  `[wait] pool: unmounting {mount_point}...` immediately before the
  call. Existing `[ok]   pool: unmounted {mount_point}` at line 281
  closes the row.
- Line 325 (the membership-disk `for name in membership.disks.keys()`
  loop, just inside the loop body before the `if fs.exists(...)`
  check or before the `match close_mapper_with_retry(...)`): emit
  `[wait] disk {name}: locking...` once per disk, only when the
  mapper actually exists (place it inside the `if fs.exists(&mapper_path)`
  branch). Existing `[ok]   disk {name}: locked` at line 332 closes it.
  The `else` branch ("already closed", line 352-355) does no
  blocking work, so no `[wait]` there.
- Line 364 (the orphan-mapper `for entry in orphan_mappers` loop):
  emit `[wait] disk {disk_name}: locking (orphan)...` immediately
  after the existing `[warn] orphaned mapper ...` row, before
  `close_mapper_with_retry`. Existing
  `[ok]   disk {disk_name}: locked (orphan)` at line 380 closes
  it. Subject must be `disk {disk_name}` (not `orphan {entry}`)
  so the wait and ok rows share the same subject per Principle 13;
  the `(orphan)` annotation in both rows distinguishes the path
  from the membership-loop locks above. `disk_name` is derived
  from the mapper via `name_from_mapper`, falling back to the
  raw entry when the mapper has no `braid-` prefix (matches the
  existing terminal-row binding at lock.rs:376).
- **Do not** emit `[wait]` inside `close_mapper_with_retry`
  (lock.rs:43-87). The retry loop's per-attempt `[warn]` rows are
  the existing busy-retry indicator; adding `[wait]` inside the
  retry loop would produce 1-3x duplicated rows per disk. The
  call-site placement above guarantees exactly one `[wait]` per
  close cycle.

### `cli/src/enroll_key_file.rs`

Already imports `color_enabled_for_stderr`. Inside
`apply_enrollment` (lines 246-281): bind `let color_enabled =
color_enabled_for_stderr();` at the top.

- Update the `use crate::status_tag::...` line to also import
  `StatusTag` and `status_line`.
- Line 263 (`luks::enroll_key_file(...)`): insert
  `[wait] disk {name}: enrolling keyfile in slot 1...` before.
  Convert line 264 (`eprintln!("ok: {} -- keyfile enrolled in slot
  1", name);`) to
  `[ok]   disk {name}: keyfile enrolled in slot 1`.
- Lines 267-269 (LUKS header backup): no `[wait]` (fast). Leave
  existing `eprintln!("LUKS header backed up: ...")` unchanged.
- Line 276 (`done: ... enrolled, ... already`): leave as-is. It is
  the per-command summary, not a per-step row.

## Test plan (VM tests)

Use the assertion pattern from `tests/cli/braid-unlock.py:79-101`:
capture stderr to a file, read with `machine.succeed("cat ...")`,
substring assert + `find()` ordering. Plain rows (no ANSI) since
`machine.succeed` redirects without a TTY.

For each existing test below, find the happy-path subtest that
exercises the relevant blocking step and append the new assertions
there. Several tests currently invoke commands without redirecting
stderr (e.g. `machine.succeed(remove_cmd("disk2"))`); change those
sites to the unlock-test pattern
`machine.succeed(f"{remove_cmd('disk2')} >/tmp/remove-stdout 2>/tmp/remove-stderr")`
and read with `machine.succeed("cat /tmp/remove-stderr")`. Do not
add fresh subtests unless the existing one cannot reach the
relevant code path.

- **`tests/cli/braid-add-disk.py`** (or the most direct add path).
  Pin in the existing happy-path subtests:
  `[wait] disk {name}: formatting LUKS...` precedes
  `[ok]   disk {name}: LUKS formatted`;
  `[wait] disk {name}: unlocking...` precedes
  `[ok]   disk {name}: unlocked`;
  `[wait] pool: balancing to RAID1...` precedes
  `[ok]   pool: RAID1 balance complete` (only when test triggers
  the balance branch -- multi-disk add to existing 1-disk pool).

  The Pass-1 closed `PresentLuks` recoverable branch (the only
  path that hits `add.rs:489-508`'s closed-mapper unlock rows) is
  pinned at the **Rust unit-test layer**, not via VM. The
  `BraidLabeledRecoverable` state -- pool mounted with disk2 as a
  PresentLuks closed-mapper candidate whose btrfs superblock matches
  the pool FSID -- is not reachable by composing existing braid
  commands: `braid remove` and `btrfs replace` both wipe the
  source's btrfs superblock during eviction, and braid ships no
  command that produces a recoverable disk + closed mapper +
  mounted-pool state in one step. The naive
  "lock the pool, then re-add" recipe also does not work --
  `validate_braid_preconditions` (add.rs:108) rejects braid-labeled
  disks when the pool is unmounted, so execution stops before
  `ensure_luks_open`. A degraded-mount workaround relies on
  unverified btrfs `device add` behavior against a recoverable
  device whose superblock is already part of the same FSID, which
  the existing `BtrfsDeviceAdd` cmd does not pass `-f` for, so
  empirical safety is not established.

  Instead, add a Rust unit test in `cli/src/add.rs::tests` named
  `pass1_recoverable_closed_mapper_emits_canonical_unlock_rows`
  that:
  1. Builds a `MockRunner` returning passphrase verify ok, LUKS
     open ok, and `btrfs filesystem show` output matching the
     pool's FSID for disk2.
  2. Builds a `PoolState` with `mounted: true`, FSID set, and
     `devices: [disk1]` (disk2 not present, satisfying the
     `BraidLabeledRecoverable` classification at
     `classify_braid_disk_fsid` line 163).
  3. Builds an `AddPlan` with `probed = [PresentLuks { mapper_open:
     false, ... }]` for disk2.
  4. Runs `AddPlan::execute` against the mock with the same
     stderr-capture seam used for the `LuksCleanupGuard::Drop`
     rollback test (see "Rust-unit-only pin" below).
  5. Asserts the captured stderr contains
     `"[wait] disk disk2: unlocking..."` followed by
     `"[ok]   disk disk2: unlocked"`, with substring +
     `find()` ordering. The
     `note: braid-labeled disk '...' verified as pool member.
     Completing recovery add.` line at add.rs:548 is a direct
     `eprintln!` and is **not** captured by the in-tree seam
     (which only intercepts `status_line`/`emit_status` output);
     do not assert on it. Branch coverage for the Recoverable
     classification itself is provided by the existing
     `classify_braid_disk_fsid` unit tests at add.rs:1561-1652;
     this new test only pins the unlock-row emission.

  This pin is deterministic because it does not depend on btrfs
  kernel behavior or device-add `-f` semantics.
- **`tests/cli/replace-live-disk.py`** (live path). Pin:
  `[wait] disk {new_name}: formatting LUKS...`,
  `[wait] pool: replacing devid {devid} with {new_mn}...`,
  `[wait] disk {old_name}: locking...` precedes
  `[ok]   disk {old_name}: locked` (the live-replace old-mapper
  close), and `[ok]   pool: replace complete`.

  Also add a new **`replace --enroll`** branch within this test (or
  immediately after the existing `--enroll`-omitted subtest) that
  invokes `braid replace ... --enroll /tmp/kf` once `/tmp/kf` is
  generated, and asserts:
  `[wait] disk {new_name}: enrolling keyfile in slot 1...`
  precedes
  `[ok]   disk {new_name}: keyfile enrolled in slot 1`.
  This replaces the prior matrix claim that
  `replace-preview-warnings.py` covers the in-replace enroll
  path -- it does not (that test only exercises pool-side keyfile
  enrollment via `braid enroll`, never `braid replace --enroll`).
- **`tests/cli/replace-dead-disk.py`** (missing path). Pin:
  `[wait] pool: rebuilding missing devid {devid} onto {new_mn}...`,
  and `[wait] pool: restoring RAID1 redundancy...` for the
  post-replace soft balance. **Do not** pin a live-replace
  old-mapper close row here -- the missing path has no old
  mapper to close (verified at replace.rs:1707-1776 in the
  existing unit tests).
- **`tests/cli/braid-remove-disk.py`** (2->1 case has the pre-remove
  balance). Pin:
  `[wait] pool: balancing RAID1 to single profile...`,
  `[wait] pool: removing braid-{name}...`,
  `[ok]   pool: braid-{name} removed`,
  and the new
  `[wait] disk {name}: locking...` precedes
  `[ok]   disk {name}: locked` (the trailing best-effort LUKS
  close in `pool::evict_present_device`). The disk-name body
  derives from stripping the `braid-` prefix from the mapper
  identifier passed into `evict_present_device`.
- **`tests/cli/braid-remove-missing-softwarn.py`** (or
  `braid-remove-missing-enospc.py` if it covers happy path). Pin:
  `[wait] pool: removing missing devid {N}...` and
  `[wait] pool: restoring RAID1 redundancy...`.
- **`tests/cli/braid-recover.py`** (Phase 4 self-mount subtest at
  line 267). Append to the existing stderr capture (`probe_err`):
  - `[wait] pool: unmounting /mnt/storage (recover remount
    cycle)...` precedes
    `[ok]   pool: unmounted /mnt/storage (recover remount cycle)`.
  - `[wait] disk disk1: locking...` precedes
    `[ok]   disk disk1: locked` (and same for `disk2`).
  - `[wait] pool: replaying post-{label} RAID1 soft balance (skip
    already-RAID1 chunks)...` precedes
    `[ok]   pool: RAID1 soft balance replay complete`.
  - The paused-balance resume row is **not** pinned here. The
    existing recover scenario (interrupted-add journal, no paused
    balance) cannot trigger `replay_post_mutation`'s resume
    branch. Pinning lives in `tests/module/ups-lb-during-balanced-
    add.py` instead, with a deterministic substring assertion (see
    below).
- **`tests/cli/braid-lock.py`** (Test 1, line 56-72). Pin:
  `[wait] pool: unmounting /mnt/storage...` precedes
  `[ok]   pool: unmounted /mnt/storage`;
  `[wait] disk longdisk3: locking...` precedes
  `[ok]   disk longdisk3: locked`.
- **`tests/cli/braid-enroll.py`**. Pin:
  `[wait] disk {name}: enrolling keyfile in slot 1...` precedes
  `[ok]   disk {name}: keyfile enrolled in slot 1`.

Module tests that explicitly pin the new rows on paths that
existing CLI tests cannot reach:

- **`tests/module/ups-lb-during-balanced-add.py`**: extend the
  existing `with subtest("braid recover completes cleanly")`
  capture (currently asserts only the post-add soft-balance
  replay substring, line 187). Add deterministic assertions for
  the paused-balance resume rows:
  `[wait] pool: resuming paused balance left by interrupted add`
  must appear in `recover_out`, and must precede
  `[ok]   pool: balance resume complete`. This is the only test
  in the suite that reliably leaves a paused balance on disk
  for recover to resume; without explicit assertions here those
  rows are not pinned anywhere.
- **`tests/module/ups-lb-during-replace.py`**: extend the existing
  `with subtest("braid recover completes cleanly")` capture
  (currently asserts only the `replace completed` guidance, line
  222). Add a **soft secondary** pin for the new
  `wait_for_kernel_replace_to_finish` rows: if
  `[ok]   pool: kernel dev_replace finished` appears in
  `recover_out`, then
  `[wait] pool: waiting for kernel dev_replace to finish...` must
  appear earlier in the same capture. The "if/then" form is
  necessary because the kernel may complete the resume before the
  function's first poll iteration (see comment at line 219-221 of
  the existing test). On runs where neither row appears, the
  rows-only-when-actually-waiting semantics from the function
  spec is correct behavior. **The primary deterministic pin lives
  in the new Rust unit test**
  `wait_for_kernel_replace_emits_canonical_rows_on_running_then_finished`
  (see row matrix). The VM assertion is integration coverage
  only; reverting the row emission would still fail the Rust
  unit test.

**Adding new VM-test subtests and breaking obsolete assertions.**
Adding canonical rows to `enroll_key_file.rs` requires both a new
subtest (to pin the dry-run `Rejected` branch) and replacement of
two stale assertions in `tests/cli/braid-enroll.py`. All three
land in the same change:

- **Add new Test 1a (between line 51 and line 54)**, *before* any
  enrollment has happened. Run
  `braid enroll /tmp --dry-run >/tmp/t1a.out 2>/tmp/t1a.err`,
  then assert per disk:
  - stderr contains `[wait] keyfile: checking against disk1...`
    followed by `[skip] keyfile: not yet enrolled on disk1`,
    in order; same for `disk2`.
  - stdout still shows the dry-run preview steps (the existing
    `enroll: disk1 -- will add keyfile to slot 1` and
    `enroll: disk2 -- ...` lines must appear).

  This is the only subtest that exercises the dry-run `Rejected`
  branch -- without it, reverting the `[skip]` row emission would
  pass everything else in the suite.
- **Modify Test 1b (line 106-142):** `assert err == ""` is the
  pre-Principle-13 dry-run empty-stderr contract. With the new
  `[wait] keyfile: checking against {name}...` plus terminal
  `[ok]   keyfile: already enrolled on {name}` rows in the dry-run
  probe (Authenticated branch, exercised after Test 1 enrolls
  both disks), stderr is no longer empty. Replace `assert err ==
  ""` with assertions that stderr contains the expected per-disk
  wait + ok pair in order, and that no unexpected lines appear
  beyond those rows. Keep the separate `nothing to do.` stdout
  assertion intact -- the dry-run preview output on stdout is
  unchanged. Update the subtest's docstring/comment block (lines
  106-122) to reference Principle 13 instead of the prior
  empty-stderr rationale.
- **Modify Test 3 (lines 175 and 178):** the assertion
  `"ok: disk1 -- keyfile already enrolled" in t3_err` pins the
  pre-canonical prose. Replace with substring + ordering
  assertions on the new canonical rows:
  `[wait] keyfile: checking against disk1...` precedes
  `[ok]   keyfile: already enrolled on disk1`, and same for
  disk2. The "Behavioral wording lock" comment block on lines
  169-173 should be updated to reference Principle 13 instead of
  the prior prose-pinning rationale.
- **Modify Test 4d Phase A (line 347):** `assert t4d_err == ""`
  currently pins the dry-run empty-stderr contract for the
  mixed-LUKS scenario (real disk1 + disk2, fake non-LUKS disk3).
  After the dry-run probe rows land, disk1 and disk2 hit the
  `Authenticated` branch and emit
  `[wait] keyfile: checking against disk1...` +
  `[ok]   keyfile: already enrolled on disk1` (and same for
  disk2) on stderr; disk3 is filtered out before the probe (it
  is not a LUKS candidate) and contributes nothing to stderr.
  Replace `assert t4d_err == ""` with substring + ordering
  assertions on the wait + ok pair for disk1 and disk2 on
  stderr, and keep the existing
  `assert "[skip] disk disk3: not LUKS-formatted\n" in t4d_out`
  stdout assertion intact -- the dry-run preview output on
  stdout is unchanged. Update the subtest's docstring to
  reference Principle 13 instead of the prior
  empty-stderr-contract rationale.
- **Modify Test 4d Phase B (lines 365-372):** lines 368 and 371
  assert the old prose
  `"ok: disk1 -- keyfile already enrolled"` /
  `"ok: disk2 -- keyfile already enrolled"`. Replace with
  substring + ordering assertions on the new canonical rows
  `[wait] keyfile: checking against disk1...` precedes
  `[ok]   keyfile: already enrolled on disk1` (and same for
  disk2). Keep the existing
  `"skip: disk3 not LUKS-formatted"` assertion at line 365 --
  that is the real-run plain prose for the non-LUKS skip and is
  unchanged by this round.

Two existing module tests grep for old eprintln strings and need
their substring updates as part of the same change:

- `tests/module/ups-lb-during-balanced-add.py:187` --
  `"Replaying post-add RAID1 soft balance"` ->
  `"replaying post-add RAID1 soft balance"`.
- `tests/module/ups-lb-during-remove-missing.py:331` --
  `"Replaying post-remove-missing RAID1 soft balance"` ->
  `"replaying post-remove-missing RAID1 soft balance"`.

Rust unit tests across `cli/src/{add,replace,remove,remove_missing,
recover,lock,enroll_key_file,pool}.rs` do not capture stderr text
today and are unchanged in this plan, with the following explicit
exceptions that the row coverage matrix below requires:

1. `cli/src/add.rs::tests` -- a new
   `pass1_recoverable_closed_mapper_emits_canonical_unlock_rows`
   pins the Pass-1 closed `PresentLuks` unlock rows; an extension
   to the existing `guard_closes_on_armed_drop` (line 2076) pins
   the `LuksCleanupGuard::Drop` rollback `(cleanup)` `[wait]`/
   `[ok]` rows; and a sibling test
   `guard_close_failure_emits_cleanup_warn_row` pins the
   rollback failure `[warn]` row.
2. `cli/src/recover.rs::tests` -- new
   `wait_for_kernel_replace_emits_canonical_rows_on_running_then_finished`
   and
   `wait_for_kernel_replace_emits_warn_on_status_error_after_wait`
   pin the kernel dev_replace wait rows.
3. `cli/src/credential_verify.rs::tests` -- the existing
   `verify_credential_for_targets_authenticates_all_targets_in_order`
   and `verify_credential_for_targets_stops_at_first_rejection`
   tests (lines 188-245) are updated so the `expected_waits`
   helper interleaves the new `[ok]` lines.
4. `cli/src/pool.rs::tests` -- a new
   `evict_present_device_close_failure_emits_warn_row` pins the
   trailing best-effort close `[warn] disk {name}: lock failed
   (...)` row by mocking a non-zero `CryptsetupClose` exit.
5. `cli/src/replace.rs::tests` -- a new
   `live_replace_old_close_failure_emits_warn_row` (or an
   extension of the existing close-issued harness at lines
   1707-1776) pins the live-replace old-mapper close
   `[warn] disk {old_name}: lock failed (...)` row.

These five files share a single new stderr-capture seam (one
`thread_local!` `RefCell<Option<String>>` in
`cli/src/status_tag.rs::testing` plus a small `capture_with` test
helper) so each test scope can flip stderr capture on, run the
code under test, and assert against the captured buffer. The
seam compiles only under `#[cfg(test)]` and adds no production
runtime cost. No existing non-stderr unit tests in the listed
files change.

### Row coverage matrix

Every new row introduced by this plan, with the test that pins it
or an explicit rationale for why it is not pinned. Substring +
ordering assertions follow the unlock-test pattern.

| Row body | Source | Pinned in |
|---|---|---|
| `[wait] disk {name}: unlocking...` (add Pass 1, closed PresentLuks) | add.rs:493 | **Rust unit test** `pass1_recoverable_closed_mapper_emits_canonical_unlock_rows` in `cli/src/add.rs::tests` (see Test plan section above). The `BraidLabeledRecoverable` + closed-mapper state cannot be composed from existing braid commands without unverified btrfs assumptions, so the row pin moves to the unit-test layer using the same stderr-capture seam as the `LuksCleanupGuard::Drop` rollback test. |
| `[ok]   disk {name}: unlocked` (add Pass 1) | add.rs:495 | same as above |
| `[wait] disk {name}: formatting LUKS...` | add.rs:584 | `tests/cli/braid-add-disk.py` happy path |
| `[ok]   disk {name}: LUKS formatted` | add.rs:585 | same |
| `[wait] disk {name}: unlocking...` (add Pass 2) | add.rs:590 | same |
| `[ok]   disk {name}: unlocked` (add Pass 2) | add.rs:592 | same |
| `[wait] disk {name}: enrolling keyfile in slot 1...` (add) | add.rs:595 | `tests/cli/braid-add-enroll.py` |
| `[ok]   disk {name}: keyfile enrolled in slot 1` (add) | add.rs:596 | same |
| `[wait] pool: balancing to RAID1...` | add.rs:679 | `tests/cli/braid-add-disk.py` (multi-disk add to existing 1-disk pool) |
| `[ok]   pool: RAID1 balance complete` | add.rs:681 | same |
| `[wait] disk {disk_name}: locking (cleanup)...` (add rollback) | add.rs LuksCleanupGuard::Drop, lines 216-242 | **Rust unit test extension** in `cli/src/add.rs` test module: extend the existing `guard_closes_on_armed_drop` (line 2076) to capture stderr via the shared seam (see "Rust-unit-only pin"), and assert the new `(cleanup)` rows fire. VM-level pinning is impractical without injecting a Pass-2 failure scenario; the Rust test pins the row emission deterministically. |
| `[ok]   disk {disk_name}: locked (cleanup)` (add rollback) | add.rs LuksCleanupGuard::Drop | same |
| `[warn] disk {disk_name}: lock failed (cleanup, exit {N})` / `lock failed (cleanup, {err})` (add rollback) | new (add.rs LuksCleanupGuard::Drop on Ok-non-zero / Err) | **Rust unit test** in `cli/src/add.rs::tests` -- companion to `guard_closes_on_armed_drop` (line 2076) that builds a `MockRunner` returning a non-zero `CryptsetupClose`, runs the Drop with the shared seam, and asserts both `[wait]` and the new `[warn]` rows fire. The Drop fires only on rollback after a primary failure, so the `[warn]` plus the parent command's non-zero exit together close the wait per Principle 13. |
| `[wait] disk {new_name}: formatting LUKS...` | replace.rs:305 | `tests/cli/replace-live-disk.py` |
| `[ok]   disk {new_name}: LUKS formatted` | replace.rs:306 | same |
| `[wait] disk {new_name}: unlocking...` (PresentNotLuks arm) | replace.rs:312 | same |
| `[ok]   disk {new_name}: unlocked` (PresentNotLuks arm) | replace.rs:313 | same |
| `[wait] disk {new_name}: enrolling keyfile in slot 1...` | replace.rs:316 | **`tests/cli/replace-live-disk.py`** -- add a real-run `braid replace ... --enroll /tmp/kf` branch (the existing `replace-preview-warnings.py` does NOT execute `replace --enroll`, only `enroll` against the pool side). Pin substring + ordering against the new branch's stderr capture. |
| `[ok]   disk {new_name}: keyfile enrolled in slot 1` | replace.rs:317 | same |
| `[wait] disk {new_name}: unlocking...` (PresentLuks arm) | replace.rs:322 | `tests/cli/replace-new-already-luks.py` (the one test that exercises a pre-existing closed LUKS device as the new replacement) |
| `[ok]   disk {new_name}: unlocked` (PresentLuks arm) | replace.rs:323 | same |
| `[wait] pool: replacing devid {devid} with {new_mn}...` | replace.rs:357 | `tests/cli/replace-live-disk.py` |
| `[wait] pool: rebuilding missing devid {devid} onto {new_mn}...` | replace.rs:361 | `tests/cli/replace-dead-disk.py` |
| `[ok]   pool: replace complete` | replace.rs:376 | both `replace-live-disk.py` and `replace-dead-disk.py` |
| `[wait] disk {old_name}: locking...` (live-replace old close) | replace.rs:471 (post-edit ~line ~471) | `tests/cli/replace-live-disk.py` (only the live arm reaches this; dead-disk path has no old mapper, asserted via the existing `replace.rs:1707-1776` unit checks) |
| `[ok]   disk {old_name}: locked` (live-replace old close) | new (replace.rs after CryptsetupClose Ok in live arm) | same |
| `[warn] disk {old_name}: lock failed (exit {N})` / `lock failed ({err})` (live-replace old close) | new (replace.rs after CryptsetupClose Ok-non-zero / Err in live arm) | **Rust unit test** in `cli/src/replace.rs::tests` -- extend or add a test that builds a `MockRunner` returning a non-zero `CryptsetupClose` for the old mapper, runs `ReplacePlan::execute` against the live arm via the shared stderr-capture seam, and asserts the captured stderr contains the canonical `[wait]` followed by the canonical `[warn]` row. The existing `replace.rs:1707-1776` MockRunner harness already has the close-issued assertion; extend it (or a sibling test) to also assert the `[warn]` body. VM-level pinning is impractical because best-effort close failure is hard to inject in a NixOS VM. |
| `[wait] pool: balancing RAID1 to single profile...` | pool.rs:313 (called from remove) | `tests/cli/braid-remove-disk.py` (Phase 3 redundancy-reducing remove) |
| `[ok]   pool: balanced to single profile` | new (pool.rs after pool_balance_single) | same |
| `[wait] pool: removing {mapper}...` | pool.rs:317 | `tests/cli/braid-remove-disk.py` |
| `[ok]   pool: {mapper} removed` | new (pool.rs after pool_remove_device) | same |
| `[wait] disk {name}: locking...` (pool.rs evict trailing close) | pool.rs ~line ~367 (the best-effort `CryptsetupClose` after `pool_remove_device`) | `tests/cli/braid-remove-disk.py` (every successful remove on a present device exercises this row pair; pin substring + ordering against the disk name stripped of `braid-` prefix) |
| `[ok]   disk {name}: locked` (pool.rs evict trailing close) | new (pool.rs after CryptsetupClose Ok in evict_present_device) | same |
| `[warn] disk {name}: lock failed (exit {N})` / `lock failed ({err})` (pool.rs evict trailing close) | new (pool.rs after CryptsetupClose Ok-non-zero / Err in evict_present_device) | **Rust unit test** in `cli/src/pool.rs::tests` -- builds a `MockRunner` whose `CryptsetupClose` returns a non-zero exit, runs `evict_present_device` against the shared stderr-capture seam, asserts both the `[wait]` and the new `[warn]` row are present in order. VM-level pinning is impractical because the close failure path is hard to trigger reliably in a NixOS VM. |
| `[wait] pool: restoring RAID1 redundancy...` | pool.rs:174 (maybe_restore_raid1) | `tests/cli/braid-remove-missing-softwarn.py` (and inherited by `replace-dead-disk.py` -- assert at the remove-missing site, sufficient) |
| `[ok]   pool: RAID1 redundancy restored` | pool.rs:176 | same |
| `[wait] pool: removing missing devid {N}...` | remove_missing.rs:189 | `tests/cli/braid-remove-missing-softwarn.py` |
| `[ok]   pool: missing devid {N} removed` | new (remove_missing.rs after pool_remove_device_using) | same |
| `[wait] pool: unmounting {mount_point} (recover remount cycle)...` | recover.rs:759 | `tests/cli/braid-recover.py` Phase 4 self-mount subtest |
| `[ok]   pool: unmounted {mount_point} (recover remount cycle)` | new (recover.rs after umount Ok) | same |
| `[wait] disk {name}: locking...` (recover cycle) | recover.rs:814 | same |
| `[ok]   disk {name}: locked` (recover cycle) | new (recover.rs after CryptsetupClose Ok) | same |
| `[wait] pool: waiting for kernel dev_replace to finish...` | new (recover.rs `wait_for_kernel_replace_to_finish`, on first observed Running iteration) | **Rust unit test** `wait_for_kernel_replace_emits_canonical_rows_on_running_then_finished` in `cli/src/recover.rs::tests`. Constructs a `MockRunner` queued to return `Running { pct: 5.0 }` then `Finished` for `BtrfsReplaceStatus`, captures stderr via the shared seam (see "Rust-unit-only pin" below), and asserts both the `[wait]` and `[ok]` rows fire in order. **Plus** a soft pin in `tests/module/ups-lb-during-replace.py`: if the `[ok]` row appears, the `[wait]` row must precede it (kernel may finish before the first poll, in which case neither appears -- correct behavior). |
| `[ok]   pool: kernel dev_replace finished` | new (recover.rs `wait_for_kernel_replace_to_finish`, on Finished/None when wait was emitted) | same |
| `[warn] pool: kernel dev_replace status check failed -- proceeding` | new (recover.rs `wait_for_kernel_replace_to_finish`, on Err early-return when wait was emitted) | **Rust unit test** `wait_for_kernel_replace_emits_warn_on_status_error_after_wait` in `cli/src/recover.rs::tests`. Mock returns `Running` then an Err on the second poll; assert the captured stderr contains both `[wait]` and the `[warn] pool: kernel dev_replace status check failed -- proceeding` row in order. No VM pin -- exercising the Err path requires faulting the BtrfsReplaceStatus subprocess, which is unstable in VM tests. |
| `[wait] pool: resuming paused balance left by interrupted {label}...` | recover.rs:651 | **`tests/module/ups-lb-during-balanced-add.py`** -- extend the existing `with subtest("braid recover completes cleanly")` (line 175-190) capture with deterministic substring + ordering assertions for the resume row pair. This module test reliably leaves a paused balance on disk for `replay_post_mutation` to resume; it is the only test that does so. The matrix previously claimed "indirect coverage" with no actual assertion -- this entry replaces that with a real pin. |
| `[ok]   pool: balance resume complete` | recover.rs:657 | same |
| `[wait] pool: replaying post-{label} RAID1 soft balance (skip already-RAID1 chunks)...` | recover.rs:661 | `tests/cli/braid-recover.py` (and the two existing module tests via the substring-rename change) |
| `[ok]   pool: RAID1 soft balance replay complete` | recover.rs:667 | `tests/cli/braid-recover.py` |
| `[wait] pool: unmounting {mount_point}...` (lock) | lock.rs:257 | `tests/cli/braid-lock.py` Test 1 |
| `[ok]   pool: unmounted {mount_point}` (lock) | existing lock.rs:281 | same (already pinned today; new wait-row precedes it) |
| `[wait] disk {name}: locking...` (lock membership loop) | lock.rs:325 | same |
| `[ok]   disk {name}: locked` (lock) | existing lock.rs:332 | same (already pinned today) |
| `[wait] disk {disk_name}: locking (orphan)...` (lock orphan loop) | lock.rs:364 | `tests/cli/braid-lock-orphan.py` |
| `[ok]   disk {disk_name}: locked (orphan)` (lock) | existing lock.rs:380 | same -- already pinned today; new wait row uses the same `disk {disk_name}` subject so the wait/ok pair satisfies Principle 13's same-subject rule, with `(orphan)` annotation distinguishing the path. |
| `[wait] disk {name}: enrolling keyfile in slot 1...` (enroll) | enroll_key_file.rs:263 | `tests/cli/braid-enroll.py` |
| `[ok]   disk {name}: keyfile enrolled in slot 1` (enroll) | enroll_key_file.rs:264 | same |
| `[ok]   passphrase: accepted by {name}` (credential verify success) | new (credential_verify.rs after Authenticated) | existing assertions in `tests/cli/braid-unlock.py:79`, `braid-recover.py:273`, and `braid-enroll.py:68` already substring-pin the preceding `[wait]` line; extend each to also assert the new `[ok]` row precedes the next subject's row (e.g. before the unlock `[wait]`). Rust unit tests in `credential_verify.rs::tests` (existing, lines 188-245) get their `expected_waits` helper updated to interleave the new ok lines. |
| `[ok]   keyfile: accepted by {name}` (credential verify success, KeyFile variant) | same | covered by the same `credential_verify.rs::tests` updates and by `tests/cli/braid-unlock-key-file.py` once the existing `[wait] keyfile: checking against ...` substring is followed by an analogous ordering check. |
| `[ok]   keyfile: already enrolled on {name}` (enroll idempotent check) | enroll_key_file.rs:190 (replacing the prose `eprintln!("ok: ...")`) | `tests/cli/braid-enroll.py` Test 3 (the re-enroll idempotent subtest at line 162-180) -- this is the only path that takes the `Authenticated` branch in `plan_enrollment`'s idempotent check. Test 1 (line 54) exercises the same code path but on first-time enrollment, where every disk takes the `Rejected` branch instead. Extend Test 3 to assert `[wait] keyfile: checking against disk1...` precedes `[ok]   keyfile: already enrolled on disk1` (and same for disk2), replacing the prior `ok: ... -- keyfile already enrolled` prose assertions. See "Modify Test 3" in the obsolete-assertion section above for the full assertion update. |
| `[skip] keyfile: not yet enrolled on {name}` (enroll idempotent check, Rejected branch) | new (enroll_key_file.rs around line 197) | `tests/cli/braid-enroll.py` Test 1 (the first-time enrollment subtest at line 54) -- on first-time enrollment, both disks hit the `Rejected` branch in `plan_enrollment`'s idempotent check, so this row fires per disk. Extend Test 1 to assert `[wait] keyfile: checking against disk1...` precedes `[skip] keyfile: not yet enrolled on disk1` (and same for disk2). Test 3 takes the `Authenticated` branch instead and pins the paired `[ok]` row (see the previous matrix entry). |
| `[wait] keyfile: checking against {name}...` (enroll dry-run probe) | new (enroll_key_file.rs ~line 567 before `verify_key_file`) | `tests/cli/braid-enroll.py` -- two subtests pin this row, one per outcome branch (see the next two rows). The wait row precedes both terminal rows, so substring + ordering against either suffices. |
| `[ok]   keyfile: already enrolled on {name}` (enroll dry-run probe, Authenticated) | new (enroll_key_file.rs after the Authenticated arm in the dry-run block) | `tests/cli/braid-enroll.py` Test 1b (the existing post-enroll dry-run subtest at line 126) -- assert each candidate emits `[wait] keyfile: checking against {name}...` followed by `[ok]   keyfile: already enrolled on {name}` on stderr, while stdout still contains the existing `[skip] disk {name}: keyfile already enrolled` preview note. The Test 1b `assert err == ""` line is removed in the same change (see "Breaking obsolete VM-test assertions"). |
| `[skip] keyfile: not yet enrolled on {name}` (enroll dry-run probe, Rejected) | new (enroll_key_file.rs after the Rejected arm in the dry-run block) | `tests/cli/braid-enroll.py` -- **add a new Test 1a subtest** immediately after `Generate random keyfile` (line 51) and before `Test 1: enroll keyfile into all pool disks` (line 54). Run `braid enroll /tmp --dry-run` against a pool where neither disk is yet enrolled, then assert: (a) command succeeds, (b) stderr contains `[wait] keyfile: checking against disk1...` followed by `[skip] keyfile: not yet enrolled on disk1` (and same for disk2), in order, (c) stdout still shows the dry-run preview (the existing `enroll: ... will add keyfile to slot 1` step lines are present). This is the only test in the suite that exercises the dry-run `Rejected` branch -- without it, reverting that row's emission would pass all other tests. |

Branches not exercised by any current test are marked above as
"new subtest" or "add a real-run branch"; those test extensions
land in the same change.

**Rust-unit-only pins:** seven row pins move to the unit-test
layer because their state cannot be composed deterministically
in VM tests:

1. The add-rollback `(cleanup)` `[wait]`/`[ok]` rows.
2. The add-rollback `(cleanup, ...)` `[warn]` row (close failure
   on the rollback path).
3. The Pass-1 closed `PresentLuks` recoverable unlock rows.
4. The `wait_for_kernel_replace_to_finish` Running -> Finished
   `[wait]`/`[ok]` pair.
5. The same function's Err-after-wait `[warn]` row.
6. The `pool::evict_present_device` trailing-close
   `[warn] disk {name}: lock failed (...)` row (close failure on
   the live-remove path).
7. The `replace.rs` live-replace old-mapper close
   `[warn] disk {old_name}: lock failed (...)` row.

All seven share a **single new stderr-capture seam** in
`cli/src/status_tag.rs` (under `#[cfg(test)]`):

```rust
#[cfg(test)]
pub mod testing {
    use std::cell::RefCell;
    thread_local! {
        pub static CAPTURED: RefCell<Option<String>> =
            const { RefCell::new(None) };
    }
    pub fn capture_with<F: FnOnce()>(f: F) -> String {
        CAPTURED.with(|c| *c.borrow_mut() = Some(String::new()));
        f();
        CAPTURED.with(|c| c.borrow_mut().take().unwrap_or_default())
    }
}
```

Wire it by changing the production-path `eprint!("{}",
status_line(...))` invocations into a thin helper
`emit_status(line: &str)` that, when `cfg(test)` and
`CAPTURED.is_some()`, pushes to the buffer instead of writing to
stderr. In non-test builds the helper inlines to the existing
`eprint!` and adds no runtime cost. The seam is the same
mechanism `tests/cli/braid-unlock.py` already exercises at the
VM layer; the unit-test version just substitutes a thread-local
buffer for the OS-level stderr fd.

This round uses **only** the in-tree `status_tag.rs::testing`
capture seam. A `gag::BufferRedirect::stderr()` alternative was
considered and rejected: `gag`'s redirect is process-global, so
two unit tests running concurrently under `cargo test`'s default
parallel execution would race on the OS stderr fd and capture
each other's output. The in-tree thread-local seam composes
cleanly with parallel tests because each test scope owns its
buffer. Do not introduce `gag` as a dev-dependency in this
change.

## Risks / edge cases

- **Lock retry loop ordering.** Emitting `[wait] disk {name}:
  locking...` inside `close_mapper_with_retry` would produce up to
  three rows per disk. Solution adopted: emit at the call site
  (lines 325 and 364), one row per close cycle. Verified by
  re-reading the retry loop -- it has no other entry point. The
  same retry loop is reused by `recover.rs::relock_and_remount`?
  -- **No**: `relock_and_remount` calls `runner.run(&CmdRequest::
  CryptsetupClose {...})` directly without the retry helper, so
  the recover-cycle `[wait]` is emitted exactly once per disk per
  cycle.
- **`wait_for_kernel_replace_to_finish`** is now in scope: the
  existing per-percent progress lines only fire when the percentage
  changes, leaving stalled or extremely slow resume workers silent.
  The function gains a `wait_emitted`-gated canonical `[wait]`/`[ok]`
  pair (see the recover.rs section). Tests pin this softly via
  `tests/module/ups-lb-during-replace.py` because the kernel may
  finish faster than the function's 200ms poll on small VM datasets;
  the soft pin form is "if `[ok]` appears, `[wait]` must precede
  it", and on instant-completion runs neither row appears (correct
  per the function's `wait_emitted=false` branch).
- **Recover self-mount cycle ordering.** After the
  `relock_and_remount` close cycle, `mount::execute_unlock_and_mount`
  emits its own `[wait] disk {name}: unlocking...` rows. The full
  output for a 2-disk recover-self-mount run is documented inline
  in the `recover.rs` per-file section above so future readers can
  spot a regression visually.
- **`replay-post-mutation` resize line.** Resize is exempt. Leaving
  the existing `eprintln!("Replaying post-replace resize on devid
  {} ...")` as-is is **intentional** -- canonicalizing a non-`[wait]`
  step into a `[wait]` row would directly contradict Principle 13's
  exemption list. If consistency is preferred, drop the eprintln
  entirely or convert it to a non-canonical `note:` informational
  line; either is acceptable but unnecessary for this round.
- **All `cryptsetup close` sites are now in scope** (resolves the
  earlier review's High finding that Principle 13's
  `cryptsetup close` clause was broader than the implementation
  scope). The three previously-deferred sites get rows + tests on
  **both branches**:
  - `pool::evict_present_device`'s trailing best-effort close --
    success `[wait]`/`[ok]` pinned via `braid-remove-disk.py`;
    failure `[warn] disk {name}: lock failed (...)` pinned via a
    Rust unit test in `pool.rs::tests`.
  - `replace.rs`'s live-replace old-mapper close -- success rows
    pinned via `replace-live-disk.py`; failure `[warn] disk
    {old_name}: lock failed (...)` pinned via a Rust unit test in
    `replace.rs::tests`.
  - `add.rs`'s `LuksCleanupGuard::Drop` rollback close -- success
    `(cleanup)` rows and failure `(cleanup, ...)` `[warn]` row
    both pinned via Rust unit tests that extend the existing
    `guard_closes_on_armed_drop` harness through the shared
    stderr-capture seam.

  The failure prose (`Warning: failed to close LUKS mapper ...`,
  `cleanup: failed to close LUKS mapper ...`) is **converted**, not
  retained -- canonical `[warn]` rows preserve the operator detail
  (exit code, error text) in the row body so no information is
  lost, and the wait is closed per Principle 13's `[warn]` clause.
- **Add Pass-1 closed `PresentLuks` test scenario.** The naive
  "lock the pool then re-add" recipe does not work --
  `validate_braid_preconditions` (add.rs:108) rejects braid-labeled
  disks against an unmounted pool, so the test path stops before
  `ensure_luks_open`. A degraded-mount recipe was considered but
  rejected because it relies on unverified `btrfs device add`
  behavior against a recoverable disk that already carries a
  matching FSID superblock (`BtrfsDeviceAdd` does not pass `-f`,
  see `cmd.rs:471-477`). The pin therefore moves to the unit-test
  layer: a Rust unit test in `add.rs::tests` mocks the runner +
  pool state to construct the recoverable closed-mapper case and
  asserts the canonical wait/ok rows fire. See the row matrix
  entry for the exact test name and shape.
- **Module tests greppping the old "Replaying post-X" substring.**
  The body text of the new `[wait]` row preserves the substring
  modulo capitalization (`Replaying` -> `replaying`). Both tests
  must be updated in the same change.
- **Per-command error paths.** Several blocking calls early-return
  on Err. The paired `[ok]` row only fires on success; an error
  surfaces as the existing `MountError` / `LuksError` /
  `PoolError` propagation. This matches the
  `open_disks_with_passphrase` precedent in mount.rs:556-585,
  where the explainer fires on Err and the `[ok]` only fires on
  Ok.
- **TUI / module command paths.** The `braid-auto-unlock.service`
  unit and module integrations route through the same `cmd_*`
  entry points, so the new rows appear in journals. Plain stderr
  (no ANSI) is correct -- `color_enabled_for_stderr()` returns
  false for non-TTY destinations, which is what systemd journals
  capture.
- **Add-rollback VM pinning is impractical without a fault-injection
  seam.** The `LuksCleanupGuard::Drop` close fires only when an
  intra-Pass-2 step fails *after* `luks_guard.track(...)`. Pass-2
  failures that are reachable from CLI flags alone (bad
  passphrase, invalid arg, missing disk path) all reject before
  `track` runs, so rollback would have nothing to close. Triggering
  the rollback in a VM requires either (a) a fault-injection seam
  (e.g. `BRAID_TEST_FAIL_AFTER_TRACK=1` consulted in the loop), (b)
  a wrapper script around `cryptsetup` that fails on the second
  invocation, or (c) hardware-level shenanigans (read-only second
  disk). Option (a) is the cleanest but adds production-code test
  surface; (b)/(c) are fragile. This round pins the rollback rows
  via a Rust unit test that captures stderr (see the row-coverage
  matrix's "Rust-unit-only pin" note); a follow-up may add a
  fault-injection seam if VM-level pinning becomes desirable.

## Verification

1. `cargo test -p braid-cli` (alias: `just test-rust`) -- existing
   Rust unit tests must keep passing, plus the seven new
   stderr-capture tests:
   - `add.rs::tests::pass1_recoverable_closed_mapper_emits_canonical_unlock_rows`
   - `add.rs::tests::guard_close_success_emits_cleanup_ok_row`
     (extension of `guard_closes_on_armed_drop`)
   - `add.rs::tests::guard_close_failure_emits_cleanup_warn_row`
   - `recover.rs::tests::wait_for_kernel_replace_emits_canonical_rows_on_running_then_finished`
   - `recover.rs::tests::wait_for_kernel_replace_emits_warn_on_status_error_after_wait`
   - `pool.rs::tests::evict_present_device_close_failure_emits_warn_row`
   - `replace.rs::tests::live_replace_old_close_failure_emits_warn_row`
   plus the updated `credential_verify.rs::tests` checking the new
   `[ok]` interleaving.
2. `just test-vm braid-unlock braid-unlock-key-file braid-recover`
   -- the original-commit assertions must still pass; the
   credential `[ok]` row addition is verified here.
3. `just test-vm braid-add-disk braid-add-enroll
   replace-live-disk replace-dead-disk replace-new-already-luks
   braid-remove-disk braid-remove-missing-softwarn braid-recover
   braid-lock braid-lock-orphan braid-enroll` -- new assertions for
   each touched command. Includes the new `replace --enroll`
   branch in `replace-live-disk`, the orphan-mapper wait row in
   `braid-lock-orphan`, the keyfile-enroll wait row in
   `braid-add-enroll`, and the rewritten dry-run-stderr
   assertions in `braid-enroll` (new Test 1a, modified Test 1b,
   modified Test 3, and modified Test 4d Phases A and B -- see
   the "Adding new VM-test subtests and breaking obsolete
   assertions" subsection of the test plan).
4. `just test-vm ups-lb-during-balanced-add
   ups-lb-during-remove-missing ups-lb-during-replace` --
   substring-rename safety, paused-balance resume row pinning,
   and (soft) kernel dev_replace wait/ok pinning.
5. `just test-all` -- full sweep before merging.
6. Manual / VM end-to-end: bring up a 3-disk test VM, run `sudo
   braid add disk2=...`, `sudo braid lock`, `sudo braid unlock`,
   `sudo braid enroll --generate ...`, `sudo braid enroll
   --dry-run /tmp/key`, etc. with both `NO_COLOR=1` and a real
   TTY, and confirm the new rows render plain in pipes
   (`braid add | cat`) and gray-colored on a TTY. Also confirm
   the dry-run preview output is unchanged on stdout while the
   probe rows appear on stderr (the contract change documented
   in `README.md` and `docs/decisions/012-intent-cli.md`).

## Critical files

- `docs/principles.md`
- `docs/decisions/021-wait-in-unlock.md`
- `docs/decisions/012-intent-cli.md` (dry-run stderr contract
  carve-out for blocking probes)
- `docs/index.md`
- `README.md` (dry-run section: relax the "stderr stays empty"
  half-sentence and append the probe-row exception paragraph)
- `cli/src/status_tag.rs` (`credential_ok_line` helper +
  `#[cfg(test)]` capture seam)
- `cli/src/credential_verify.rs` (terminal `[ok]` rows + test
  updates)
- `cli/src/pool.rs` (shared helpers)
- `cli/src/add.rs`
- `cli/src/replace.rs`
- `cli/src/remove_missing.rs`
- `cli/src/recover.rs`
- `cli/src/lock.rs`
- `cli/src/enroll_key_file.rs`
- `tests/cli/braid-add-disk.py`
- `tests/cli/braid-add-enroll.py`
- `tests/cli/replace-live-disk.py`
- `tests/cli/replace-dead-disk.py`
- `tests/cli/replace-new-already-luks.py`
- `tests/cli/braid-remove-disk.py`
- `tests/cli/braid-remove-missing-softwarn.py`
- `tests/cli/braid-recover.py`
- `tests/cli/braid-lock.py`
- `tests/cli/braid-lock-orphan.py`
- `tests/cli/braid-enroll.py`
- `tests/module/ups-lb-during-balanced-add.py`
  (substring-rename + new paused-balance resume row pin)
- `tests/module/ups-lb-during-remove-missing.py`
  (substring-rename only)
- `tests/module/ups-lb-during-replace.py`
  (new soft pin for the kernel dev_replace wait/ok pair)

## Reused helpers (already exist; do not reimplement)

- `cli/src/status_tag.rs::StatusTag::Wait`,
  `status_line(tag, color_enabled, body)`,
  `color_enabled_for_stderr()` -- the canonical row writer.
- `cli/src/pool.rs::evict_present_device`,
  `cli/src/pool.rs::maybe_restore_raid1` -- shared mutation
  helpers; already-existing call sites do not change, only their
  internal eprintlns.
- `cli/src/mount.rs::open_disks_with_passphrase`,
  `open_disks_with_key_file`, `scan_and_mount` -- the precedent
  for per-disk `[wait]`/`[ok]` row pairing; do not modify.
