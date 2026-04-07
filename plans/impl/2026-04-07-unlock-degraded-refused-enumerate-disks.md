# Plan: align unlock-time degraded-refused output with cross-command LUKS terminology

## Context

`plans/wip/cheeky-questing-popcorn.md` left two follow-ups out of scope as
"separate cosmetic / wider-scope PRs". Both have aged in place — neither has
been addressed — and both target the same function (`plan_open_pool` in
`cli/src/mount.rs`). Bundling them is the right call: they touch overlapping
lines, share the same VM test, and share the same cross-command-consistency
invariant. Two PRs would force the second one to re-touch everything the first
one moved.

The current state in `cli/src/mount.rs::plan_open_pool` has two mismatches with
the rest of the codebase's LUKS-corruption story:

1. **Wrong terminology on the per-disk status line.** When `probe_config_disk`
   returns `ConfigDiskState::PresentNotLuks` (which it does when
   `cryptsetup luksUuid` exits non-zero — i.e. the LUKS header is *unreadable*,
   not "damaged"), `mount.rs:88` prints
   `"disk: <name>     LUKS header damaged"`. After
   `cheeky-questing-popcorn.md` shipped, the canonical vocabulary in
   `cli/src/luks.rs` distinguishes:
   - `LuksHeaderState::Unreadable` → `isLuks` failed → guidance helper
     `luks::luks_header_unreadable_guidance()` (cli/src/luks.rs:281)
   - `LuksHeaderState::Damaged` → `isLuks` ok but `luksDump` failed → guidance
     helper `luks::luks_header_damaged_guidance(device)` (cli/src/luks.rs:289)

   The `PresentNotLuks` branch is the *unreadable* case (the entire header
   parse fails) — calling it "damaged" contradicts doctor, the unlock
   open-failure enrichment, and the shared guidance text.

2. **Generic degraded-refused error.** When `any_missing_member && !allow_degraded`,
   `mount.rs:120-126` returns
   ```
   pool has missing devices — refusing to mount degraded
   new writes would have ZERO redundancy (single-profile chunks)
   hint: braid <command> --allow-degraded
   ```
   The probe loop *already* knows which disks are missing and *why* — `Absent`
   (unplugged) vs `PresentNotLuks` (unreadable header) — but throws that
   information away. Users hitting this error see eprintln! status lines
   scroll past, then a generic "missing devices" sentence with no per-disk
   summary in the error itself. If the eprintln! output is captured into a
   log file or piped through journald, the per-disk reason is split across
   stderr lines that may not be co-located with the final error.

The fix: collect per-disk reasons into a single ordered
`Vec<(String, MissingReason)>` during the probe loop, then format that
list in insertion order so the structured error agrees with the
preceding eprintln! probe stream. Doctor's `summarize_declared_disks`
(`cli/src/doctor.rs:241-333`) is the loose inspiration for the
enumeration shape, but doctor groups disks by category (its output is a
warning summary, order-insensitive); unlock's error must match the
membership iteration order users just saw on stderr, which is why a
single tagged list fits unlock better than parallel category vectors.
Fix the eprintln! string at the same time so unlock and doctor agree on
the "LUKS header unreadable" vocabulary.

### Alignment with `docs/principles.md`

- **Cross-command consistency (narrow scope)** — after this PR, `unlock` and
  `doctor` agree on the "LUKS header unreadable" terminology. `status.rs`
  (and the TUI that consumes its data) currently maps
  `ConfigDiskState::PresentNotLuks` to a generic `DiskStatus::Unknown` /
  unpooled bucket at `cli/src/status.rs:820-833` and does not surface the
  unreadable/damaged distinction at all. Aligning `status` is a separate,
  larger refactor (it would touch the `DiskStatus` enum and downstream
  rendering) and is **out of scope** for this PR. The invariant claim here
  is intentionally narrowed to the two surfaces we actually touch.
- **Principle 3 (safe-by-construction)** — purely a presentation change. No
  new commands run, no new I/O, no new mutation paths. The probe data is
  already collected; we just stop discarding it.
- **Principle 8 (test every design decision)** — the VM test scenario in
  `tests/cli/braid-unlock.py::Test 7` is rewritten to deterministically
  exercise the new structured `DegradedRefused` path (see Verification).

## Scope

Two source files edited, one VM test updated:

1. **`cli/src/mount.rs`** — inside `plan_open_pool` (lines 56-142):
   - Add a small private enum `MissingReason { Unplugged, LuksHeaderUnreadable }`
     near the function (private to the module).
   - Replace the `any_missing_member: bool` accumulator with one ordered
     `Vec<(String, MissingReason)>` (`missing: Vec<...>`) that preserves
     membership iteration order. Both the `Absent` and `PresentNotLuks`
     branches push into the same list, tagged with the appropriate
     `MissingReason` variant.
   - Rename the eprintln! status line at line 88 from `"LUKS header damaged"`
     to `"LUKS header unreadable"`.
   - Compute `any_missing_member: bool = !missing.is_empty()` once after
     the loop (still needed by `OpenPlan` and `compile_open_steps`).
   - Replace the `DegradedRefused` error message (lines 120-126) with a
     multi-line structured version that enumerates each missing disk in
     **probe order**, preserving the `"refusing to mount degraded"` and
     `"braid <command> --allow-degraded"` substrings that existing unit
     tests assert on. Mixed-reason pools are formatted in the same order
     the eprintln! status lines were emitted, so the final error agrees
     with the preceding stderr stream.

2. **`tests/cli/braid-unlock.py`** — Test 7 (lines 191-217):
   - Rewrite the scenario as a **2-disk pool** containing one valid LUKS
     member (`disk1`, which was `braid add`'d during setup with the test
     passphrase) and one raw/unreadable member (`raw` pointing at
     `virtio-disk4`). The mapper for `disk1` is closed by the
     `close_all()` already at the top of Test 7, so `plan_open_pool`'s
     probe will classify `disk1` as `PresentLuks` (closed) → goes into
     `to_unlock`, and `raw` as `PresentNotLuks` → adds to `missing`.
     `to_unlock` is non-empty → does NOT hit the
     `"no unlockable disks found"` early return → falls through to the
     `DegradedRefused` check, which fires deterministically.
   - Replace the `"LUKS header damaged" or "no unlockable disks"`
     assertion with deterministic positive assertions on the new
     structured error: `"refusing to mount degraded"`,
     `"raw: LUKS header unreadable"`, and the cross-command negative
     invariants (`/var/lib/braid/luks-headers/` absent,
     `.luksheader` absent).

3. **`cli/src/mount.rs`** test module — add **four** new unit tests for
   the degraded-refused message format (single-disk, mixed-reason
   probe-order regression, singular/plural pluralization, negative
   cross-command invariant). See Verification §Unit tests for the full
   test list. The existing `degraded_refused`-style tests in
   `mount.rs:825-848`, `unlock.rs:435-454`, `unlock.rs:875-885`, and
   `recover.rs:1015-1037` only assert on the variant + the
   `"refusing to mount degraded"` / `"braid <cmd> --allow-degraded"`
   substrings, all of which are preserved verbatim. They keep passing
   without changes.

## Design

### 1. Restructure `plan_open_pool`'s probe loop

Current code (cli/src/mount.rs:75-126):

```rust
let mut to_unlock = Vec::new();
let mut any_open = false;
let mut any_missing_member = false;

for (name, member) in &membership.disks {
    let probed = probe::probe_config_disk(runner, fs, name, &member.by_id)?;
    match &probed.state {
        ConfigDiskState::Absent => {
            eprintln!("{}  disk: {:<10}not found (unplugged?)", tag("skip"), name);
            any_missing_member = true;
        }
        ConfigDiskState::PresentNotLuks => {
            eprintln!("{}  disk: {:<10}LUKS header damaged", tag("skip"), name);
            any_missing_member = true;
        }
        ConfigDiskState::PresentLuks { uuid, mapper_open } => { /* ... */ }
    }
}

if to_unlock.is_empty() && !any_open {
    return Err(MountError::Failed("no unlockable disks found".into()));
}

if any_missing_member && !allow_degraded {
    return Err(MountError::DegradedRefused(format!(
        "pool has missing devices — refusing to mount degraded\n\
         new writes would have ZERO redundancy (single-profile chunks)\n\
         hint: braid {} --allow-degraded",
        command_hint
    )));
}
```

New code (private `MissingReason` enum lives near the top of `mount.rs`):

```rust
/// Why a membership disk is missing from the pool at unlock time.
/// Used to format the structured `DegradedRefused` error in probe order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingReason {
    /// Device file does not exist on the host (`ConfigDiskState::Absent`).
    Unplugged,
    /// Device exists but `cryptsetup luksUuid` failed
    /// (`ConfigDiskState::PresentNotLuks`). The header is unreadable.
    LuksHeaderUnreadable,
}
```

```rust
let mut to_unlock = Vec::new();
let mut any_open = false;
let mut missing: Vec<(String, MissingReason)> = Vec::new();

for (name, member) in &membership.disks {
    let probed = probe::probe_config_disk(runner, fs, name, &member.by_id)?;
    match &probed.state {
        ConfigDiskState::Absent => {
            eprintln!("{}  disk: {:<10}not found (unplugged?)", tag("skip"), name);
            missing.push((name.clone(), MissingReason::Unplugged));
        }
        ConfigDiskState::PresentNotLuks => {
            eprintln!("{}  disk: {:<10}LUKS header unreadable", tag("skip"), name);
            missing.push((name.clone(), MissingReason::LuksHeaderUnreadable));
        }
        ConfigDiskState::PresentLuks { uuid, mapper_open } => { /* unchanged */ }
    }
}

let any_missing_member = !missing.is_empty();

if to_unlock.is_empty() && !any_open {
    return Err(MountError::Failed("no unlockable disks found".into()));
}

if any_missing_member && !allow_degraded {
    return Err(MountError::DegradedRefused(format_degraded_refused(
        &missing,
        command_hint,
    )));
}
```

`OpenPlan { any_missing_member, .. }` (the field `compile_open_steps` and
`open_and_mount_pool` already read at lines 182, 413) is constructed
unchanged from the local `any_missing_member` variable. **No changes to
`OpenPlan` struct fields** — the per-disk list only needs to live in scope
during the probe loop and is consumed by the error formatter.

Because `missing` is appended to in the natural iteration order of
`membership.disks`, the formatter renders disks in the same order as the
preceding eprintln! status lines. The final error message therefore
*agrees* with the stderr probe stream — there is no ordering split
between "all unplugged first, then all unreadable" and the on-screen
status output.

### 2. New private formatter

Add as a private function near `plan_open_pool` (or just above it). Pure
function, no I/O — easy to unit-test in isolation.

```rust
/// Format a structured `DegradedRefused` error message that names each
/// missing disk and the reason in **probe order**. Preserves the
/// substrings `"refusing to mount degraded"` and
/// `"braid <command_hint> --allow-degraded"` that existing tests anchor on.
///
/// `missing` is guaranteed non-empty by the caller.
fn format_degraded_refused(
    missing: &[(String, MissingReason)],
    command_hint: &str,
) -> String {
    let total = missing.len();
    let header = if total == 1 {
        "pool has 1 missing device — refusing to mount degraded".to_owned()
    } else {
        format!("pool has {total} missing devices — refusing to mount degraded")
    };

    let mut lines = vec![header];
    for (name, reason) in missing {
        let reason_text = match reason {
            MissingReason::Unplugged => "not found (unplugged?)",
            MissingReason::LuksHeaderUnreadable => "LUKS header unreadable",
        };
        lines.push(format!("  {name}: {reason_text}"));
    }
    lines.push("new writes would have ZERO redundancy (single-profile chunks)".to_owned());
    lines.push(format!("hint: braid {command_hint} --allow-degraded"));
    lines.join("\n")
}
```

Resulting message for a 2-disk problem on `unlock` where `disk2` was
unplugged and `disk3` had an unreadable header (and `disk2` came first
in `membership.disks` iteration order):

```
pool has 2 missing devices — refusing to mount degraded
  disk2: not found (unplugged?)
  disk3: LUKS header unreadable
new writes would have ZERO redundancy (single-profile chunks)
hint: braid unlock --allow-degraded
```

For the new Test 7 scenario (`disk1` valid, `raw` unreadable):

```
pool has 1 missing device — refusing to mount degraded
  raw: LUKS header unreadable
new writes would have ZERO redundancy (single-profile chunks)
hint: braid unlock --allow-degraded
```

Substring guarantees preserved for all existing assertions:
- `"refusing to mount degraded"` → in the header line.
- `"--allow-degraded"` → in the hint line.
- `"braid recover --allow-degraded"` → in the hint line when
  `command_hint == "recover"`.

### 3. Why a small private enum (and not parallel vectors)

The first draft of this plan used two parallel `Vec<String>` lists
(matching doctor.rs's `missing`/`header_unreadable`/`header_damaged`
pattern at `cli/src/doctor.rs:241-333`). That worked for doctor because
doctor's output is *grouped by category* — disks with the same problem
appear together. But unlock's eprintln! probe stream emits status lines
in **membership iteration order**, intermixing reasons. Two parallel
vectors would force the formatter to render all unplugged disks before
all unreadable ones, producing an error summary that *disagrees* with
the order users just saw on stderr. A single ordered tagged list keeps
the two surfaces in sync.

The enum (two unit variants) is the smallest type that gives the
formatter compile-time exhaustiveness over the reason kinds without
stringly-typed labels in the producer. If a third reason appears later
(e.g. UUID mismatch), adding a variant is a one-line change.

### Things deliberately NOT done

- **No changes to `OpenPlan` struct fields.** The per-disk list is
  consumed by the error formatter inside `plan_open_pool` and never needs
  to escape the function. `any_missing_member: bool` stays on `OpenPlan`
  because `compile_open_steps` (mount.rs:182) and `open_and_mount_pool`
  (mount.rs:413) read it to decide whether to mount with `degraded`.
- **No changes to `ConfigDiskState`.** The `PresentNotLuks` variant stays a
  unit variant. We don't need to carry stderr or a richer reason — the
  fact that `cryptsetup luksUuid` failed is enough information for the
  caller, and the canonical "what to do about it" guidance lives in
  `luks::luks_header_unreadable_guidance()` which doctor already exposes
  in detail. Pulling the full guidance text into the `unlock` error would
  bloat the output for the more common "list of missing disks" case.
- **No changes to `probe_config_disk`.** Same reason.
- **No changes to the existing `DegradedRefused(String)` shape.** Keeping
  the variant as a string preserves the matching pattern in `main.rs:411`,
  `main.rs:626`, and the four unit tests that match on the variant.
- **No changes to `cli/src/status.rs` or the TUI.** `status.rs:820-833`
  routes `ConfigDiskState::PresentNotLuks` through the unpooled-bucket
  classification rather than emitting "unreadable" vocabulary. Aligning
  status with the new vocabulary is a separate, larger change (touching
  the `DiskStatus` enum and downstream rendering) and is intentionally
  out of scope here. This PR's cross-command consistency claim is
  narrowed to `unlock` and `doctor`.
- **No reference to `/var/lib/braid/luks-headers/` or `.luksheader`** — the
  cross-command negative invariant is preserved. The new error message
  says "LUKS header unreadable" without offering recovery instructions;
  users who want the full off-system-backup guidance run `braid doctor`,
  which is the proactive touchpoint for that workflow.
- **No bundling of the doctor-style off-system-backup guidance into the
  unlock error.** The unlock error is a *list* of problems; the
  remediation path is `braid doctor` (which already prints the full
  guidance per-disk). Duplicating that text in unlock would bloat the
  message and create two strings to keep in sync.

## Critical files

- **`cli/src/mount.rs`** — edit `plan_open_pool` (lines 56-142): rename
  the eprintln! at line 88, add a private `MissingReason` enum, swap
  `any_missing_member: bool` for one ordered `Vec<(String, MissingReason)>`
  accumulator, replace the inline `DegradedRefused` format string at
  lines 120-126 with a call to a new private `format_degraded_refused`
  helper. Add the helper as a private function near the function. Add
  four new unit tests in the existing `tests` module.
- **`tests/cli/braid-unlock.py`** — Test 7 (lines 191-217): rewrite the
  scenario from a 1-disk raw pool to a 2-disk mixed pool
  (`disk1` real + `raw` unreadable) so the test deterministically
  exercises `DegradedRefused`. Replace the old `"LUKS header damaged" or
  "no unlockable disks"` assertion with positive assertions on the new
  structured error format.
- **`cli/src/luks.rs`** — **read-only reference** for the canonical
  `LuksHeaderState` vocabulary at lines 237-295. No edits.
- **`cli/src/doctor.rs`** — **read-only reference** for the
  `summarize_declared_disks` enumeration pattern at lines 241-333. The
  pattern there is the inspiration but not directly copied — see
  Design §3 for why a single ordered list fits unlock better than
  doctor's parallel category vectors. No edits.
- **`cli/src/probe.rs`** — **read-only reference** for `probe_config_disk`
  at lines 72-110. Confirms `ConfigDiskState::PresentNotLuks` is the
  cryptsetup-luksUuid-failed branch (line 92). No edits.
- **`cli/src/types.rs`** — **read-only reference** for `ConfigDiskState`
  variants at lines 120-129. No edits.
- **`cli/src/status.rs`** — **read-only reference** for the unpooled
  classification at lines 820-833. Confirms status does not currently
  use the unreadable/damaged vocabulary, justifying the narrowed scope
  in §Alignment. No edits.

## Verification

### Unit tests in `cli/src/mount.rs`

Add four new tests in the existing `mod tests` block, beside the
`degraded_refused` tests around line 825-848. Each gets the standard block
comment header (intent / why / scenario).

1. **`format_degraded_refused_single_unreadable_includes_disk_name_and_reason`**
   — call
   ```rust
   format_degraded_refused(
       &[("raw".to_owned(), MissingReason::LuksHeaderUnreadable)],
       "unlock",
   )
   ```
   Assert the result contains:
   - `"refusing to mount degraded"` (existing substring contract)
   - `"braid unlock --allow-degraded"` (existing substring contract)
   - `"raw: LUKS header unreadable"` (new structured per-disk line)
   - `"1 missing device"` singular form
   - `"new writes would have ZERO redundancy"` (preserved from old message)

2. **`format_degraded_refused_mixed_reasons_enumerates_each_disk_in_order`**
   — call with a deliberately interleaved input:
   ```rust
   format_degraded_refused(
       &[
           ("disk2".to_owned(), MissingReason::Unplugged),
           ("disk3".to_owned(), MissingReason::LuksHeaderUnreadable),
           ("disk5".to_owned(), MissingReason::Unplugged),
       ],
       "recover",
   )
   ```
   Assert each per-disk line is present AND assert that `"disk2"` appears
   before `"disk3"` which appears before `"disk5"` in the formatted output
   (using `find()` byte offsets). This is the regression test for the
   probe-order fidelity finding — two parallel vectors would group
   `disk2` and `disk5` together and break this test. Also assert
   `"3 missing devices"` (plural) and `"braid recover --allow-degraded"`.

3. **`format_degraded_refused_uses_singular_for_one_disk_and_plural_otherwise`**
   — call once with one entry, once with two. Assert `"1 missing device"`
   (no `s`) in the first result and `"2 missing devices"` in the second.

4. **`format_degraded_refused_does_not_reference_local_header_backups`** —
   call with any non-empty input. Assert the result does NOT contain
   `"/var/lib/braid/luks-headers/"` or `".luksheader"`. This locks in the
   cross-command negative invariant at the formatter level so it can
   never drift.

The existing degraded-refused unit tests
(`mount.rs:825-848`, `unlock.rs:435-454`, `unlock.rs:875-885`,
`recover.rs:1015-1037`) continue to pass unchanged — they only assert on
the variant and the substrings `"refusing to mount degraded"` and
`"braid <cmd> --allow-degraded"`, all of which are preserved by the new
formatter.

Run with `just test-rust`.

### VM test in `tests/cli/braid-unlock.py`

Rewrite Test 7's pool.json scenario from a 1-disk raw pool to a 2-disk
pool that mixes one valid LUKS member and one raw/unreadable member.
This is the deterministic fix for the original review's "VM coverage
does not actually exercise `DegradedRefused`" finding: with the previous
1-disk scenario, `plan_open_pool` returned `"no unlockable disks found"`
before reaching the degraded check.

Test 7 currently (lines 191-217):

```python
with subtest("Test 7: uninitialized disk detected"):
    close_all()

    original_pool = machine.succeed("cat /var/lib/braid/pool.json")

    raw_pool = json.dumps({
        "disks": {
            "raw": {"by_id": "/dev/disk/by-id/virtio-disk4"},
        },
    })
    machine.succeed(f"echo '{raw_pool}' > /var/lib/braid/pool.json")

    cmd = unlock_cmd(passphrase) + " 2>&1"
    ret = machine.execute(cmd)
    assert ret[0] != 0, "Expected non-zero exit for uninitialized disk"
    assert "LUKS header damaged" in ret[1] or "no unlockable disks" in ret[1], \
        f"Expected 'LUKS header damaged' or 'no unlockable disks' in output, got: {ret}"

    machine.succeed(f"echo '{original_pool}' > /var/lib/braid/pool.json")
```

becomes:

```python
with subtest("Test 7: uninitialized disk detected — degraded-refused enumerates per-disk reasons"):
    close_all()

    original_pool = machine.succeed("cat /var/lib/braid/pool.json")

    # Two-disk pool: disk1 is real (was braid add'd during setup, so it
    # has a valid LUKS header that the test passphrase can open), and
    # 'raw' points at virtio-disk4 which has never been LUKS-formatted.
    # This mix is what makes plan_open_pool reach DegradedRefused
    # deterministically: disk1 → PresentLuks → to_unlock = [disk1]
    # (non-empty, so we skip the "no unlockable disks" early return);
    # raw → PresentNotLuks → missing = [(raw, LuksHeaderUnreadable)];
    # any_missing_member && !allow_degraded → DegradedRefused.
    mixed_pool = json.dumps({
        "disks": {
            "disk1": {"by_id": "/dev/disk/by-id/virtio-disk1"},
            "raw":   {"by_id": "/dev/disk/by-id/virtio-disk4"},
        },
    })
    machine.succeed(f"echo '{mixed_pool}' > /var/lib/braid/pool.json")

    cmd = unlock_cmd(passphrase) + " 2>&1"
    ret = machine.execute(cmd)
    assert ret[0] != 0, f"Expected non-zero exit for raw member in pool, got: {ret}"
    output = ret[1]

    # Deterministic: must reach the structured DegradedRefused path.
    assert "refusing to mount degraded" in output, \
        f"Expected DegradedRefused path, got: {output}"
    assert "raw: LUKS header unreadable" in output, \
        f"Expected per-disk reason 'raw: LUKS header unreadable', got: {output}"
    assert "braid unlock --allow-degraded" in output, \
        f"Expected --allow-degraded hint, got: {output}"

    # The renamed status line at mount.rs:88 must use the new vocabulary,
    # never the old "LUKS header damaged" wording.
    assert "LUKS header damaged" not in output, \
        f"Old 'LUKS header damaged' string must not appear after rename: {output}"

    # Cross-command negative invariant: unlock errors never point users at
    # local /var/lib/braid/luks-headers/ files (those are off-system).
    assert "/var/lib/braid/luks-headers/" not in output, \
        f"degraded-refused must not reference local backup directory: {output}"
    assert ".luksheader" not in output, \
        f"degraded-refused must not reference local .luksheader files: {output}"

    # Restore original pool.json
    machine.succeed(f"echo '{original_pool}' > /var/lib/braid/pool.json")

    # close_all() any leftover disk1 mapper. plan_open_pool unlocks
    # nothing on the failing path (the degraded check fires before any
    # cryptsetup open call), but be defensive in case future changes
    # reorder things.
    close_all()
```

Notes on the rewrite:

- The intent and "Why it exists" framing of Test 7 are unchanged: a raw
  disk in pool.json should be detected and surfaced as an unreadable
  LUKS header. We've just expanded the scenario so the failure mode
  exercises `DegradedRefused` instead of the upstream `to_unlock.is_empty()`
  short-circuit.
- All assertions are now positive and deterministic — no `or` fallback
  branches.
- The trailing `close_all()` is defensive: today `plan_open_pool` returns
  `DegradedRefused` *before* any cryptsetup open in step 6, so disk1's
  mapper is never opened. But if a future refactor moves the degraded
  check after the open loop, the trailing `close_all()` keeps Test 8
  (which assumes a clean state) from inheriting an open mapper.

Run with `just test-vm braid-unlock`.

### Negative coverage cross-check

The existing assertion in `cli/src/mount.rs:843-847` continues to verify
that the formatted message contains `"braid recover --allow-degraded"` —
this is the canary that the substring contract is preserved. No new
assertion is needed there.

The existing assertion in `cli/src/unlock.rs:446-453` continues to verify
that the message contains both `"refusing to mount degraded"` and
`"--allow-degraded"`. Same canary, different command.

### Manual smoke test (developer, not required for merge)

Boot any braid VM with a 2+ disk pool, close the pool, corrupt one disk's
LUKS header with the established recipe
(`dd if=/dev/zero of=/dev/disk/by-id/virtio-disk2 bs=1M count=16 conv=notrunc oflag=direct`
+ `sync && echo 3 > /proc/sys/vm/drop_caches`), run `braid unlock`, and
confirm:
- The status line reads `disk2     LUKS header unreadable` (not "damaged").
- The final error has `disk2: LUKS header unreadable` on its own line.
- The hint still says `braid unlock --allow-degraded`.
- The output never references `/var/lib/braid/luks-headers/`.

## Out of scope / future follow-ups

- **Aligning `cli/src/status.rs` and the TUI** with the new
  unreadable/damaged vocabulary. `status.rs:820-833` currently routes
  `ConfigDiskState::PresentNotLuks` to a generic unpooled bucket; making
  it surface "LUKS header unreadable" would require new `DiskStatus`
  variants and downstream rendering work. Worth doing as a follow-up so
  the cross-command consistency story is complete; tracked here so it
  doesn't get forgotten.
- Inlining the full `luks::luks_header_unreadable_guidance()` text into
  the unlock error per disk. Deliberately not done — the unlock error is
  a list, and the proactive doctor command already prints the full
  guidance. If users report confusion, revisit with a single-line
  "run `braid doctor` for recovery guidance" footer.
- Promoting `ConfigDiskState::PresentNotLuks` to carry the cryptsetup
  stderr or the `LuksHeaderState` distinction (`Unreadable` vs `Damaged`).
  Currently `plan_open_pool` only runs `cryptsetup luksUuid` once via
  `probe_config_disk`, so it cannot tell the metadata-damaged case
  (`isLuks` ok, `luksDump` fails) apart from the fully-unreadable case.
  Adding that distinction would require either a second cryptsetup probe
  in the unlock hot path or refactoring `probe_config_disk` to call
  `luks::probe_luks_header` directly. Worth doing if a real user reports
  the misclassification, but adds I/O to the common-case unlock probe loop
  for a rare diagnostic improvement.
