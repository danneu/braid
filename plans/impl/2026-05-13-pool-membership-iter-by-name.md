# Plan: `PoolMembership::iter_by_name()` -- one API for operator-visible iteration

## Context

Decision 024 (`docs/decisions/024-luks-uuid-identity.md` lines 175-176) states the
invariant explicitly:

> `pool.json` key order is UUID order, not disk-name order. Display surfaces
> that need stable operator ordering must sort by `DiskName`.

Today this invariant is enforced by convention. `PoolMembership::iter()`
(`cli/src/membership.rs:304-307`) returns UUID-sorted iteration over its
internal `BTreeMap`, and `PoolMembership::names()` (lines 309-313) returns
disk names in UUID order. Every operator-visible call site is expected
to collect-and-sort by name inline before printing. Five sites do
(`status.rs:244`, `status.rs:388`, `mount.rs:229`, `doctor.rs:409`,
`tui/mod.rs:33`). Four sites do not:

- `cli/src/main.rs:758` -- the `braid discover` preview output.
- `cli/src/lock.rs:584` -- the "already closed" status prelude.
- `cli/src/enroll_key_file.rs:85` -- `discover_enrollment_candidates`
  iterates `membership.iter()` and only sorts the accumulated
  `candidates` and `notes` vectors after the loop (line 126-141).
  Preserved-context failure returns at line 89 and line 108-117 fire
  before that sort, so user-visible notes come back in UUID order on
  any probe error or UUID mismatch.
- `cli/src/main.rs:885` (`disk_name_candidates`) -- the shell-completion
  candidates source feeding `add`, `remove`, `remove-missing`, and
  `replace` (referenced at `main.rs:168`, `:182`, `:200`, `:203`). Uses
  `membership.names()`, so tab completion lists disk names in UUID
  order, contradicting decision 024 for an operator-visible surface.

The bugs were introduced (or surfaced) in commit `844ed0f` during the
LUKS-UUID identity migration. Inline-sort enforcement is exactly the
kind of pattern that silently fails the next time someone adds a display
loop, so this refactor moves the invariant onto the API: a single
method, `iter_by_name()`, that returns name-sorted iteration. The four
missed sites get fixed; the five existing sites switch from inline
`sort_by` to the helper; the inline pattern disappears from the codebase.

The `manual/commands/discover.md:25-29` example currently telegraphs the bug
(it shows `toshiba` before `ironwolf`) and is updated to alphabetical.

## Approach

1. Add `PoolMembership::iter_by_name()` to `cli/src/membership.rs`.
2. Update `iter()`'s doc comment to point readers to `iter_by_name()` for
   operator-visible output.
3. Add one unit test in `cli/src/membership.rs` that pins both orderings
   (UUID via `iter()`, name via `iter_by_name()`) with two members whose
   UUID order is opposite to their name order.
4. Fix the two display-loop bugs (`main.rs:758` discover, `lock.rs:584`
   already-closed prelude). For revert-resistant regression coverage of
   the actual command output (not just the helper), extract small
   printer helpers at each site, unit-test them, AND add VM-level order
   assertions on `braid discover` and `braid lock` stderr against an
   inverse-UUID/name fixture (see "Regression coverage at the buggy
   sites" below).
5. Fix the third bug at `cli/src/enroll_key_file.rs:85` -- migrate
   `discover_enrollment_candidates` to iterate `membership.iter_by_name()`.
   The preserved-context failure path at line 89 and line 108-117
   currently returns notes accumulated in UUID order (the post-loop sort
   at line 126-141 has not run yet). The post-loop sorts on `candidates`
   and `notes` become redundant once iteration is name-ordered -- remove
   both.
6. Fix the fourth bug at `cli/src/main.rs:885` (`disk_name_candidates`) --
   switch from `membership.names()` to `membership.iter_by_name()`. Pin
   the order with a new subtest in `tests/cli/shell-completion.py` that
   writes a hand-crafted `pool.json` with inverse-UUID/name members and
   asserts the bash completion candidate list is alphabetical.
7. Migrate the five existing inline-sort sites
   (`status.rs:244`, `status.rs:388`, `mount.rs:229`, `doctor.rs:409`,
   `tui/mod.rs:33`) to the helper.
8. Update the manual example to alphabetical order.

## Helper API

`cli/src/membership.rs`, sibling to `iter()` at line 304-307:

```rust
/// Iterate `(UUID, &DiskMember)` pairs sorted by `DiskName` -- the
/// operator-facing display order required by
/// `docs/decisions/024-luks-uuid-identity.md`. Use this for any
/// user-visible status line, summary, or preview; use `iter()` for
/// internal data processing where UUID order is the persistent
/// identity order.
pub fn iter_by_name(&self) -> Vec<(&LuksUuid, &DiskMember)> {
    let mut v: Vec<_> = self.disks.iter().collect();
    v.sort_by(|(_, a), (_, b)| a.name.cmp(&b.name));
    v
}
```

`DiskName` derives `Ord` (`cli/src/types.rs:104`), so `a.name.cmp(&b.name)`
works directly.

Touch up `iter()`'s existing doc (line 304):

```rust
/// Iterate `(UUID, &DiskMember)` pairs in UUID-sorted order. Use
/// for internal data processing; for operator-visible output, prefer
/// `iter_by_name()` (see decision 024).
```

The existing `names()` helper (line 309-313) keeps its doc as-is -- it's
already explicit about UUID-order and tells callers to sort if they need
name order. No call sites need to change.

## Unit tests

### Helper test (in `cli/src/membership.rs`)

Add to the `#[cfg(test)]` block, near the existing
`multi_disk_round_trip_stable_uuid_order` test (line 1127) which is the
closest stylistic precedent. Reuses the existing `test_uuid` and `member`
helpers. Preamble follows `docs/testing.md:13-22` (Intent / Why it exists /
Scenario).

```rust
#[test]
fn iter_by_name_returns_name_sorted_order_independent_of_uuid_order() {
    // Intent: iter_by_name() returns operator-visible name order even when
    //   UUID order is the opposite, and iter() stays in UUID order.
    // Why it exists: decision 024 requires display surfaces to sort by
    //   DiskName. This pins both orderings against future regressions of
    //   the kind that produced the discover and lock bugs (commit 844ed0f).
    // Scenario: a two-disk pool whose LUKS UUIDs happen to sort opposite
    //   to their disk names. A reader of `braid discover` or `braid lock`
    //   output expects rows in alphabetical name order regardless of
    //   what UUIDs cryptsetup assigned.
    //
    // Seed allocation: `cli/src/membership.rs` reserves seeds 100-199
    // for this module (see `test_uuid` doc at line 735). 160 and 161
    // are unused; 100-153 are claimed by existing tests.
    let mut uuids = [test_uuid(160), test_uuid(161)];
    uuids.sort();
    let [u_lo, u_hi] = [uuids[0].clone(), uuids[1].clone()];
    let mut m = PoolMembership::empty();
    // Lower UUID gets the higher-sorting name; higher UUID gets the
    // lower-sorting name. UUID order: [zeta, alpha]. Name order:
    // [alpha, zeta].
    m.insert(u_lo, member("zeta", "/dev/disk/by-id/ata-Z")).unwrap();
    m.insert(u_hi, member("alpha", "/dev/disk/by-id/ata-A")).unwrap();

    let uuid_order: Vec<&str> = m.iter().map(|(_, mem)| mem.name.as_str()).collect();
    assert_eq!(uuid_order, vec!["zeta", "alpha"], "iter() must be UUID order");

    let name_order: Vec<&str> = m
        .iter_by_name()
        .iter()
        .map(|(_, mem)| mem.name.as_str())
        .collect();
    assert_eq!(name_order, vec!["alpha", "zeta"], "iter_by_name() must be name order");
}
```

### Regression coverage at the buggy sites

The helper test above proves `iter_by_name()` works. To prevent silent
revert of the two call-site fixes, two layers of coverage are needed:

1. **Helper unit tests** -- extract a tiny printer helper at each buggy
   site (`discover::render_preview_lines`, `lock::already_closed_names`)
   and unit-test it. These run under `just test-rust` (`cargo test --lib`,
   per `justfile:104`). They pin the helper's behavior. Both helpers
   follow the project's existing pattern (`lock.rs::forget_paths`,
   `cmd.rs::base_mount_options`): small pure functions returning a `Vec`.

2. **VM-level command-output assertions** -- helper unit tests alone do
   not exercise `main.rs`'s wiring, because `cargo test --lib` does not
   compile or run the binary entry point. A revert that drops the
   `render_preview_lines` call and reinstates an inline `outcome.members.iter()`
   loop in `main.rs:758` would leave the helper test green. VM tests
   that invoke the real `braid` binary against an inverse-UUID/name
   fixture are the durable regression coverage.

**`cli/src/discover.rs`** -- new helper, replaces the inline loop at
`main.rs:758-760`:

```rust
/// Format the operator-visible discover preview lines, one per member,
/// in DiskName order (decision 024). Returned for unit-test pinning;
/// the call site iterates and prints.
pub fn render_preview_lines(outcome: &DiscoverOutcome) -> Vec<String> {
    outcome
        .members
        .iter_by_name()
        .into_iter()
        .map(|(_, m)| format!("  {} = {}", m.name, m.by_id))
        .collect()
}
```

Call site at `main.rs:758` becomes:

```rust
for line in braid_cli::discover::render_preview_lines(&outcome) {
    eprintln!("{line}");
}
```

Unit test in `cli/src/discover.rs` `#[cfg(test)]`, name-vs-UUID-inverse
fixture:

```rust
#[test]
fn render_preview_lines_returns_name_sorted_independent_of_uuid_order() {
    // Intent: discover preview lines are returned in DiskName order
    //   regardless of underlying UUID order.
    // Why it exists: a previous regression (commit 844ed0f) printed the
    //   preview in UUID order, contradicting decision 024. This pins
    //   the ordering at the actual call-site path so a revert of the
    //   `iter_by_name()` switch fails this test.
    // Scenario: two-member outcome where UUID order is opposite name
    //   order; operator expects alphabetical lines.
    // [construct DiscoverOutcome with two members; assert the returned
    //  Vec<String> starts with "  alpha = ..." then "  zeta = ..."]
}
```

**`cli/src/lock.rs`** -- new helper, replaces the inline loop at
`lock.rs:584-598`:

```rust
/// Names of members that are "already closed" -- not in the planned
/// close set and not in skipped mappers, returned in DiskName order
/// (decision 024). The caller wraps each in
/// `line(StatusTag::Ok, ...)` for printing.
fn already_closed_names<'a>(
    membership: &'a PoolMembership,
    planned_members: &HashSet<&DiskName>,
    planned_mappers: &HashSet<&str>,
    skipped_mappers: &HashSet<&str>,
) -> Vec<&'a DiskName> {
    membership
        .iter_by_name()
        .into_iter()
        .filter_map(|(_, m)| {
            let mn = mapper_name(m.name.as_str());
            (!planned_members.contains(&m.name)
                && !planned_mappers.contains(mn.as_str())
                && !skipped_mappers.contains(mn.as_str()))
            .then_some(&m.name)
        })
        .collect()
}
```

Call site at `lock.rs:584` becomes:

```rust
for name in already_closed_names(membership, &planned_members, &planned_mappers, &skipped_mappers) {
    eprint!("{}", line(StatusTag::Ok, &format!("disk {name}: already closed")));
}
```

Unit test in `cli/src/lock.rs` `#[cfg(test)]`, also using a name-vs-UUID-inverse fixture (construct membership manually rather than using `lock_test_membership()` so we control UUID order):

```rust
#[test]
fn already_closed_names_returned_in_name_order_independent_of_uuid_order() {
    // Intent: the "already closed" prelude lists members in DiskName
    //   order regardless of underlying UUID order.
    // Why it exists: the LUKS-UUID migration (commit 844ed0f) left this
    //   loop iterating in UUID order; this pins name order at the
    //   call-site path so a revert fails the test.
    // Scenario: a two-disk pool where UUID order is opposite name order;
    //   no member is in the planned close set, so both appear in the
    //   "already closed" prelude.
    // [pass empty planned/skipped sets; assert names come back ["alpha", "zeta"]]
}
```

### VM-level command-output regression tests

Both VM tests exist solely to make a call-site revert in `main.rs` or
`lock.rs` fail loudly. They are small, focused, and run under
`just test-vm`.

**`tests/cli/braid-discover-name-order.{nix,py}` (new)** -- VM test that
boots a two-disk fixture where UUID order is inverse to name order, runs
`braid discover` (read-only preview), parses the lines, and asserts they
come back in alphabetical name order.

To produce inverse-order disks, `tests/module/lib/initrd-fixture.nix`
gains an optional `diskUuidMap ? null` parameter. When non-null, the
shell case statement at lines 99-105 reads from this map instead of the
hardcoded `disk1`..`disk5` fallback. The new test passes a map like:

```nix
diskNames = [ "alpha" "zeta" ];
diskUuidMap = {
  zeta  = "11111111-1111-1111-1111-111111111111";
  alpha = "99999999-9999-9999-9999-999999999999";
};
```

Name order: `[alpha, zeta]`. UUID order: `[zeta, alpha]`. Inverse.

Test body sketch (under 30 lines):

```python
# Intent: braid discover prints members in DiskName order, even when
#   their LUKS UUIDs sort opposite to their names.
# Why it exists: a previous regression (commit 844ed0f) printed the
#   preview in UUID order, contradicting decision 024. Helper unit
#   tests pass even if `main.rs` is reverted; only a binary-output
#   assertion catches that revert.
# Scenario: two LUKS-labeled disks where UUID order is opposite name
#   order; operator runs `braid discover` to preview the rebuild.
start_all()
machine.wait_for_unit("multi-user.target", timeout=120)
out = machine.succeed("braid discover 2>&1")
# Parse the "  name = /dev/disk/by-id/..." lines in order.
names = [line.strip().split(" = ")[0]
         for line in out.splitlines()
         if " = /dev/disk/by-id/" in line]
assert names == ["alpha", "zeta"], (
    "discover output must be in DiskName order, got: " + str(names)
)
```

**`tests/cli/braid-lock-name-order.{nix,py}` (new)** -- VM test that
writes a hand-crafted `/var/lib/braid/pool.json` with two members in
inverse UUID/name order (no real LUKS devices required, because the
"already closed" prelude prints for members whose mappers do not exist),
runs `braid lock`, and asserts the order of the
`disk <name>: already closed` lines is alphabetical.

The lock test does not need `initrd-fixture` at all -- the fixture is a
JSON file.

**`pool.json` schema (exact).** `PoolMembership` has
`#[serde(deny_unknown_fields)]` at `cli/src/membership.rs:222`, with a
test pinning rejection of unknown top-level keys
(`load_membership_rejects_unknown_top_level_key`, lines 1017-1029). The
on-disk shape is one top-level key `"disks"`, a UUID-keyed map whose
values are `DiskMember` (`name` and `by_id` required; `devid` and
`added_at` optional). No `version` / `schema_version` / other fields.
Example shape:

```json
{
  "disks": {
    "11111111-1111-1111-1111-111111111111": {
      "name": "zeta",
      "by_id": "/dev/disk/by-id/ata-Z"
    },
    "99999999-9999-9999-9999-999999999999": {
      "name": "alpha",
      "by_id": "/dev/disk/by-id/ata-A"
    }
  }
}
```

Test body sketch:

```python
# Intent: braid lock prints "already closed" prelude lines in DiskName
#   order, even when LUKS UUIDs sort opposite to names.
# Why it exists: `lock.rs:584` was iterating membership in UUID order;
#   helper unit tests pass even if the call site is reverted. This
#   asserts the binary's actual stderr output.
# Scenario: a pool.json with two members and no live mappers --
#   `braid lock` should emit two "already closed" lines in name order.
import json, re
pool = {"disks": {
  "11111111-1111-1111-1111-111111111111": {"name": "zeta",  "by_id": "/dev/disk/by-id/ata-Z"},
  "99999999-9999-9999-9999-999999999999": {"name": "alpha", "by_id": "/dev/disk/by-id/ata-A"},
}}
machine.succeed("mkdir -p /var/lib/braid")
machine.succeed("cat > /var/lib/braid/pool.json << 'EOF'\n" + json.dumps(pool) + "\nEOF")
out = machine.succeed("braid lock 2>&1 || true")
order = [m.group(1)
         for m in re.finditer(r"disk (\S+): already closed", out)]
assert order == ["alpha", "zeta"], (
    "lock already-closed prelude must be in DiskName order, got: " + str(order)
)
```

Both new tests register in `flake.nix` per `docs/testing.md:24-26`.

### Shell-completion order coverage (F1)

Extend the existing `tests/cli/shell-completion.py` with a new subtest
that writes a hand-crafted inverse-UUID/name `pool.json` and asserts
the bash disk-name completion candidates come back in alphabetical
order.

The existing test (`shell-completion.nix`) configures only
`/etc/braid/config.json`, so `disk_name_candidates` currently returns
empty in the test environment. The new subtest writes a pool.json
before invoking `bash /tmp/get-completions.sh braid add ''` (or the
equivalent fish path) and asserts the candidate list is alphabetical.

Uses the same `pool.json` schema documented for the lock test above
(top-level `disks` only -- `PoolMembership` rejects unknown fields).

```python
with subtest("disk name completion is in DiskName order"):
    pool = {"disks": {
      "11111111-1111-1111-1111-111111111111": {"name": "zeta",  "by_id": "/dev/disk/by-id/ata-Z"},
      "99999999-9999-9999-9999-999999999999": {"name": "alpha", "by_id": "/dev/disk/by-id/ata-A"},
    }}
    machine.succeed("mkdir -p /var/lib/braid")
    machine.succeed("cat > /var/lib/braid/pool.json << 'EOF'\n" + json.dumps(pool) + "\nEOF")
    out = machine.succeed("bash /tmp/get-completions.sh braid add ''")
    candidates = [c for c in out.splitlines() if c in ("alpha", "zeta")]
    assert candidates == ["alpha", "zeta"], (
        "completion candidates must be in DiskName order, got: " + str(candidates)
    )
```

### Regression coverage at `enroll_key_file.rs`

Unit test for the preserved-context failure path (per F2). Goes in
`cli/src/enroll_key_file.rs` `#[cfg(test)]`. The existing tests already
exercise probe-error and UUID-mismatch returns; this one specifically
pins that the returned `notes` Vec is name-ordered when the failure
fires on the second iterated member.

```rust
#[test]
fn preserved_context_failure_returns_notes_in_name_order() {
    // Intent: when discover_enrollment_candidates returns early due to
    //   a probe error or UUID mismatch, the notes accumulated so far
    //   are in DiskName order, not UUID order.
    // Why it exists: the function used to iterate membership.iter()
    //   (UUID order) and only sort notes after the loop completed;
    //   preserved-context failure paths returned notes pre-sort, in
    //   UUID order. iter_by_name() fixes this at the source.
    // Scenario: two-member pool where UUID order is opposite name order;
    //   the second iterated member triggers a UUID-mismatch failure.
    //   The returned notes Vec contains a single PerDisk note for the
    //   first iterated member, and that member must be the alphabetically
    //   first one (name order), not the UUID-first one.
    // [construct membership with `alpha` (UUID high) and `zeta` (UUID low);
    //  arrange runner/fs so the first iterated probe returns Absent
    //  (-> Skip note) and the second returns a PresentLuks with a
    //  mismatched UUID (-> early return); assert notes has one entry
    //  for "alpha", not "zeta"]
}
```

## Call-site migrations

**Display-loop bug fix sites (was UUID order, now name order; both gain
extracted helpers per "Regression coverage at the buggy sites" above):**

| File:line                | Was                                            | After                              |
| ------------------------ | ---------------------------------------------- | ---------------------------------- |
| `cli/src/main.rs:758`    | inline UUID-order eprintln loop                | iterate `discover::render_preview_lines(&outcome)` |
| `cli/src/lock.rs:584`    | inline UUID-order `for` over `membership.iter()` | iterate `already_closed_names(...)` |

**Operator-facing migration of preserved-context failure path:**

| File:line                       | Before                                                                                       | After                                              |
| ------------------------------- | -------------------------------------------------------------------------------------------- | -------------------------------------------------- |
| `cli/src/enroll_key_file.rs:85` | `for (expected_uuid, member) in membership.iter() {` (UUID order; sorts notes after loop)    | `for (expected_uuid, member) in membership.iter_by_name() {` |
| `cli/src/enroll_key_file.rs:126-141` | `candidates.sort_by(...); notes.sort_by(...)`                                            | Remove both -- redundant once iteration is name-ordered |

**Shell-completion bug fix:**

| File:line                       | Before                                                                                       | After                                              |
| ------------------------------- | -------------------------------------------------------------------------------------------- | -------------------------------------------------- |
| `cli/src/main.rs:885` (`disk_name_candidates`) | `membership.names().map(...).collect()`                                       | `membership.iter_by_name().into_iter().map(\|(_, m)\| CompletionCandidate::new(m.name.as_str().to_owned())).collect()` |

**Already-sorting sites (semantics unchanged, just deduplicated):**

| File:line                  | Before                                                                                                                                | After                                              |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------- |
| `cli/src/status.rs:244-246`    | `let mut members: Vec<_> = membership.iter().collect(); members.sort_by(...); for (uuid, member) in members {`                  | `for (uuid, member) in membership.iter_by_name() {` |
| `cli/src/status.rs:388-389`    | `let mut members: Vec<_> = membership.iter().map(\|(_, m)\| m).collect(); members.sort_by(\|a, b\| a.name.cmp(&b.name));`            | `let members: Vec<_> = membership.iter_by_name().into_iter().map(\|(_, m)\| m).collect();` |
| `cli/src/mount.rs:229-234`     | `let mut members: Vec<_> = membership.iter().collect(); ...sort... for (expected_uuid, member) in members {`                    | `for (expected_uuid, member) in membership.iter_by_name() {` (drop the explanatory comment -- the helper name and doc carry it) |
| `cli/src/doctor.rs:409-410`    | `let mut members: Vec<_> = pool_membership.iter().map(\|(_, member)\| member).collect(); members.sort_by(...)`                  | `let members: Vec<_> = pool_membership.iter_by_name().into_iter().map(\|(_, m)\| m).collect();` |
| `cli/src/tui/mod.rs:33-34`     | `let mut members: Vec<_> = membership.iter().collect(); members.sort_by(...)`                                                    | `let members = membership.iter_by_name();` |

For sites where downstream code reuses `members` as a `Vec` (e.g.
`tui/mod.rs` and `doctor.rs`), `Vec<(&LuksUuid, &DiskMember)>` is the same
binding shape they had before, so the rest of each function is unchanged.

The pre-sort comment at `mount.rs:230-231` ("Membership is UUID-keyed for
persistence, but this probe emits operator-visible rows. Keep the visible
unlock order by disk name.") is the precedent for the helper's doc. It can
be deleted from the call site once the helper's doc carries the same
information.

## Manual update

`manual/commands/discover.md:25-29` -- alphabetize:

```
  ironwolf = /dev/disk/by-id/ata-ST12000VN0008_XXXXXXXX
  toshiba = /dev/disk/by-id/ata-TOSHIBA_MN08ACA16T_XXXXXXXX
```

## Files touched

Rust source:

- `cli/src/membership.rs` -- new `iter_by_name()` method + new unit test;
  small doc tweak on `iter()`.
- `cli/src/discover.rs` -- new `render_preview_lines()` helper + new unit
  test.
- `cli/src/lock.rs` -- new `already_closed_names()` helper + new unit
  test.
- `cli/src/enroll_key_file.rs` -- migrate `discover_enrollment_candidates`
  loop to `iter_by_name()`; remove redundant post-loop sorts on
  `candidates` and `notes`; new unit test for the preserved-context
  failure path.
- `cli/src/main.rs` -- discover preview call site (now iterates
  `render_preview_lines`); `disk_name_candidates` switches from
  `membership.names()` to `membership.iter_by_name()`.
- `cli/src/status.rs` -- two call sites.
- `cli/src/mount.rs` -- one call site (drop explanatory comment).
- `cli/src/doctor.rs` -- one call site.
- `cli/src/tui/mod.rs` -- one call site.

Test infrastructure and tests:

- `tests/module/lib/initrd-fixture.nix` -- new optional `diskUuidMap`
  parameter for tests that need non-default UUID assignments.
- `tests/cli/braid-discover-name-order.{nix,py}` (new) -- VM order
  assertion for `braid discover` against an inverse-UUID/name fixture;
  registered in `flake.nix`.
- `tests/cli/braid-lock-name-order.{nix,py}` (new) -- VM order
  assertion for `braid lock` "already closed" prelude using a
  hand-crafted `pool.json`; registered in `flake.nix`.
- `tests/cli/shell-completion.py` -- new subtest asserting disk-name
  completion order against an inverse-UUID/name `pool.json`.

Docs:

- `manual/commands/discover.md` -- alphabetize example.

Net effect: 1 new API method on `PoolMembership`, 2 new tiny printer
helpers, 4 new unit tests + 3 new VM-level order assertions
(2 standalone + 1 subtest), 4 bug fixes (discover, lock, enroll
preserved-context failure, shell completion), and ~15 lines of inline
sort removed across the migrated sites.

## Verification

1. `just test-rust` -- runs `cargo test --lib --test golden_nixos_25_11 --test tty_guard`
   (`justfile:104`), exercising library-level tests:
   - `iter_by_name_returns_name_sorted_order_independent_of_uuid_order`
     (helper test in `membership.rs`).
   - `render_preview_lines_returns_name_sorted_independent_of_uuid_order`
     (discover helper test in `discover.rs`).
   - `already_closed_names_returned_in_name_order_independent_of_uuid_order`
     (lock helper test in `lock.rs`).
   - `preserved_context_failure_returns_notes_in_name_order`
     (enroll preserved-context test in `enroll_key_file.rs`).
   - All existing membership / discover / lock / enroll tests; existing
     `multi_disk_round_trip_stable_uuid_order` continues to pin `iter()`
     UUID order.
2. `just test-vm braid-discover-name-order braid-lock-name-order shell-completion` --
   runs the three new command-output order assertions. These exercise
   the actual binary's wiring and fail loudly on a call-site revert in
   `main.rs` or `lock.rs`. The helper unit tests in step 1 do NOT cover
   `main.rs` because `cargo test --lib` does not compile the binary.
3. `just test-vm` -- full VM suite to catch regressions in the migrated
   `status`, `mount`, `doctor`, `tui`, `enroll`, and `disk_name_candidates`
   paths. Existing `tests/cli/braid-discover.py`, `tests/cli/braid-lock.py`,
   and `tests/cli/luks-mapper-drift.py` continue to pass unchanged (they
   assert set membership or substring presence, not order).
4. `cargo build` and `cargo clippy` -- no new warnings.

No fixture refresh is needed -- parser-critical tool versions are
unchanged.

## Out of scope

- `journal.rs:431`, `recover.rs:1081/1615/3577` iterate membership for
  internal data processing in UUID order, which is correct; not changed.
- The existing `PoolMembership::names()` (membership.rs:309-313) is not
  used by any operator-visible path that lacks a sort, and its UUID-order
  semantics are correct for its callers. Leave as-is.
