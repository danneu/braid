# Plan: make `braid status` degrade (not blank) on a corrupt `pool.json`

## Context

`braid status` hard-fails with exit 1 when `pool.json` is corrupt, unreadable
(non-NotFound I/O), or fails the load-time uniqueness sweep -- even when the pool
is mounted and fully probeable. The whole report (pool summary, capacity, scrub,
balance, per-disk detail) disappears at exactly the moment an operator most needs
live state.

This contradicts the project's own stance. principles.md #3 names `status` and
`doctor` as read-only diagnostic surfaces that "stay available"; status.md calls
`status` "the always-available read-only diagnostic." The sibling surfaces already
honor this: `doctor` degrades the identical input to a `Warn` and keeps rendering
every other check (`load_membership_or_check_result`, `cli/src/doctor.rs:539`),
and bare `discover` routes a corrupt `pool.json` into its rebuild path
(`classify_pool_json`, `cli/src/discover.rs:255`). `status` is the lone outlier
among the protected surfaces.

The offending arm at `cli/src/status.rs:432` (`Err(e) => return Err(e.into())`)
also predates the principle it violates: the corrupt hard-fail landed in
`9e7ff222` (2026-03-30), while the "status degrades, never blanks" rule was
cemented for the config-probe path in `f64aa1e0` (2026-06-09) -- the membership
arm was simply left behind. The same function already documents the rule
(`cli/src/status.rs:510-518`) and a test already locks it for a different input
(`status_surfaces_mapper_conflict`).

The mounted pool is fully probeable from btrfs independently of `pool.json`. The
only thing lost when membership is unavailable is the operator-name join, which
already has a defined fallback to the mapper basename
(`present_display_name(None, ..)`, `cli/src/membership.rs:732`). The `Corrupt`
error's `Display` is self-remediating (it ends with "run 'braid discover --write'
to rebuild ..."); `Conflict` and non-NotFound `Io` are not, so `status` supplies
the same rebuild remediation at the advisory boundary (step 1). Either way the
fault reads cleanly as an advisory rather than a fatal error.

**Outcome:** a corrupt/unloadable `pool.json` on a mounted pool yields exit 0 --
the live pool renders with present devices under their mapper basenames, and a
`discover --write` rebuild advisory (carrying the underlying fault detail) appears.
Corruption is still surfaced; it just no longer blanks the report. Missing
`pool.json` keeps its existing silent-empty behavior; the offline path is
unchanged.

## Scope decision (settled)

- **In scope:** `status` only.
- **Out of scope (deliberate):**
  - **TUI (`braid tui`, `cli/src/tui/mod.rs:34`)** hard-fails the same way, but its
    disk-identity layer is membership-first (`DiskIdentity::from_membership`), so
    degrading to empty membership yields a near-empty dashboard rather than a clean
    mapper-basename fallback. It is not on principles.md's stay-available list. A
    hard-fail there still prints the loader's error message (with remediation for
    the `Corrupt` case), which is acceptable. Tracked as a separate, larger change
    if ever wanted.
  - **Mutating commands** (`add`, `replace`, `remove`, `remove-missing`) hard-fail
    on corrupt `pool.json` by design (fail-closed; `pool.json` is authoritative for
    membership mutations). No change.
  - **`doctor`, bare `discover`, `lock`** already correct (`doctor`/`discover`
    degrade; `lock` never reads membership). No change.

## The fix

### 1. Degrade the membership-load arm -- `cli/src/status.rs` (`build_status`, ~line 425)

`advisories` is already declared `mut` at status.rs:420, just above this match.
Replace the fatal catch-all so it mirrors `doctor`'s `load_membership_or_check_result`
shape -- NotFound stays silent-empty (expected), every other error degrades to
empty + advisory:

```rust
let membership = match membership::load_membership(paths) {
    Ok(m) => m,
    Err(membership::MembershipError::Io { source, .. })
        if source.kind() == std::io::ErrorKind::NotFound =>
    {
        // No pool.json yet: treat as no declared members (silent, expected).
        PoolMembership::empty()
    }
    // Corrupt / value-side Conflict / non-NotFound I/O: degrade like `doctor`
    // (load_membership_or_check_result) and bare `discover` (classify_pool_json,
    // which collapses this same union into one corrupt-or-unreadable rebuild
    // message). The live pool is fully probeable from btrfs; only the operator-name
    // join is lost, and it already falls back to the mapper basename (decision 024).
    // Surface the fault as an advisory and keep rendering -- this read-only
    // diagnostic stays exit 0 (principles.md #3).
    Err(e) => {
        advisories.push(membership_load_advisory(&e));
        PoolMembership::empty()
    }
};
```

This is broader than "Corrupt + Conflict" on purpose: a non-NotFound I/O failure
(EACCES/EIO, or `pool.json` being a directory) must also degrade to match
`doctor`'s catch-all, or `status` would still hard-fail where `doctor` warns.

The advisory text must NOT be a verbatim `e.to_string()`: only
`MembershipError::Corrupt`'s `Display` carries the `discover --write` remediation
(membership.rs:35). `Conflict`'s `Display` is just the conflict text and `Io`'s is
just "failed to read ..." -- pushing those verbatim would leave the operator (and
the status.md docs) with no remediation. Format them through a small status-side
helper instead, mirroring the wording `discover` already uses for the identical
union (`BareDiscoverError::Corrupt`, "pool.json ... is corrupt or unreadable -- run
'braid discover --write' ...", `cli/src/discover.rs:220`):

```rust
/// Advisory text for a membership-load fault that `build_status` degrades to
/// empty membership. `Corrupt`'s pinned `Display` already carries the
/// `discover --write` remediation, so it passes through verbatim; `Conflict` and
/// non-NotFound `Io` carry none, so wrap them in the same corrupt-or-unreadable
/// rebuild remediation `discover` surfaces for the identical union
/// (`BareDiscoverError::Corrupt`). The match is exhaustive, not a `_` wildcard:
/// `load_membership` provably yields only `Corrupt`/`Conflict`/`Io`
/// (`load_membership_from`, membership.rs:435), so `DuplicateDevid` (only from
/// `by_devid`) and `Save` (only from the write path) are `unreachable!` here.
/// Exhaustiveness makes a future load-time variant fail to compile rather than
/// silently inherit the discover wording. NotFound never reaches here -- it is
/// handled silently one arm above.
fn membership_load_advisory(e: &membership::MembershipError) -> String {
    use membership::MembershipError;
    match e {
        MembershipError::Corrupt { .. } => e.to_string(),
        MembershipError::Conflict(_) | MembershipError::Io { .. } => format!(
            "pool membership unreadable: {e} -- run 'braid discover --write' to \
             rebuild from existing disks (with all intended pool members attached; \
             see docs/internals/luks-unlock.md)"
        ),
        // `load_membership` cannot return these: `DuplicateDevid` comes only from
        // `by_devid`, `Save` only from `save_membership_to`. This is an internal
        // invariant assertion, not a response to operator input -- every
        // operator-controllable pool.json fault maps to one of the three arms
        // above -- so the panic does not weaken the stay-available guarantee.
        MembershipError::DuplicateDevid { .. } | MembershipError::Save { .. } => {
            unreachable!(
                "membership_load_advisory only sees load_membership errors \
                 (Corrupt/Conflict/Io)"
            )
        }
    }
}
```

Keep the rebuild clause byte-aligned with `BareDiscoverError::Corrupt`
(discover.rs:220) and `MembershipError::Corrupt` (membership.rs:35). Those two live
in `#[error(...)]` attrs (literal-only -- they cannot share a `const`), so this
third copy is the project's accepted pinned-string pattern, not new drift; a
`discover` test already pins the byte-exact rebuild remediation (discover.rs
~1870).

With empty membership, the live pool still renders: `build_devid_names`
(status.rs:270) and the `build_disk_views` present loop (status.rs:1014) both
resolve names through `present_display_name(None, &pd.mapper)` -> mapper basename.
Present rows survive; only the "configured-but-absent" enumeration and the
operator-name decoration degrade -- exactly `doctor`'s tradeoff.

### 2. Remove the now-dead `StatusError::Membership` variant -- `cli/src/status.rs:312`

The arm above was the sole constructor of `StatusError::Membership` (via
`#[from] membership::MembershipError`); no other site in status.rs converts a
`MembershipError` into `StatusError` (the only other `.into()` at status.rs:417
builds `StatusError::Probe` from `ProbeError`). After step 1, the variant is dead.
Delete the variant and its `#[from]`. The fix dissolves a type obligation rather
than leaving an unreachable error shape.

### 3. Reconcile the stale doc comment -- `cli/src/status.rs:265-267`

`build_devid_names`'s doc says "`membership` is `load_membership`-validated ... and
`load_membership` owns the refusal." After step 1, `status` no longer refuses on a
corrupt/conflicting `pool.json` -- it degrades to `PoolMembership::empty()`. The
*invariant* the comment protects still holds (an empty membership has no devids, so
`by_devid`'s `DuplicateDevid` remains unreachable here), but the "owns the refusal"
clause is now inaccurate for `status`. Reword to: the join only ever sees a
uniqueness-swept membership or the empty fallback, neither of which carries a
duplicate devid, so display still never aborts -- without claiming `status` refuses
on corruption.

### 4. Document the advisory -- `docs/commands/status.md` (Advisories section, ~line 349)

Add a "Pool-membership load fault" subsection parallel to the existing
"Config-disk probe fault" bullet. State that a missing `pool.json` is treated
silently as "no declared members," while a corrupt / unreadable / conflicting
`pool.json` keeps `status` non-fatal (exit 0): it surfaces a `discover --write`
rebuild advisory -- the same corrupt-or-unreadable remediation `discover` uses
(`BareDiscoverError::Corrupt`), carrying the underlying fault detail -- and still
renders the live pool, with present devices shown under their mapper basenames
(decision 024). Only the operator-name join and the configured-but-absent
enumeration are lost. (AGENTS.md requires docs to track behavior changes; keep
README.md in sync if it documents this exit-code behavior -- it currently does
not, so likely no change there.)

## Tests

### Rewrite `cmd_status_corrupt_membership_returns_error` -- `cli/src/status.rs:6693`

The current test asserts `StatusError::Membership(Corrupt(..))`; its contract is
inverted by this change. Rewrite and rename it (e.g.
`cmd_status_corrupt_membership_degrades_to_advisory`), following the
`status_surfaces_mapper_conflict` template (status.rs:6781) -- call `build_status`
directly and inspect `BuiltStatus.report`:

- Seed corrupt `pool.json`: `std::fs::write(paths.pool_json(), "not valid json {{{")`.
- Reuse `status_runner_healthy_3disk_base()`, `status_fs_three_disk()`,
  `status_config()`, `mock_virtio_backing_path_resolver()`.
- Assert: `report.status == StatusCode::Intact`; `report.present_count == Some(3)`;
  `report.advisories` contains the corrupt remediation substring (e.g.
  `"run 'braid discover --write'"`, here via `membership_load_advisory`'s
  verbatim-`Corrupt` branch); the three present rows render under their pool-side
  mapper basenames (`disk1`/`disk2`/`disk3` per the fixture), proving the
  `present_display_name(None, ..)` fallback.
- Reuse `assert_capacity_and_allocation_retained` and
  `assert_scrub_and_balance_retained` (status.rs ~4119-4136) to prove body sections
  survive.
- Update the `// Intent / Why it exists / Scenario` preamble: the contract is now
  "surface corruption as an advisory, never blank the report." Note explicitly that
  this supersedes `9e7ff222`'s "surface as error" -- corruption must still be
  visible (the advisory), but the always-available diagnostic must not refuse all
  output (principles.md #3), matching `doctor`/`discover`.

Leave `cmd_status_unmounted_corrupt_membership_returns_ok` (status.rs:6739)
unchanged -- it still locks the offline path.

### Mandatory: `Conflict` and non-NotFound `Io` variant tests

The catch-all changes behavior for three error shapes that now produce two
different advisory formats (Corrupt verbatim vs. wrapped by
`membership_load_advisory`), so all three are pinned -- not just Corrupt. Both new
tests follow the same `build_status`-direct template and assert the full contract:
`Ok`/exit-0, `report.status == StatusCode::Intact`, retained body
(`assert_capacity_and_allocation_retained` + `assert_scrub_and_balance_retained`),
present rows under their pool-side mapper basenames, AND `report.advisories`
containing the `run 'braid discover --write'` remediation -- the assertion that
proves `membership_load_advisory` wrapped a no-remediation `Display`.

- **`cmd_status_conflict_membership_degrades_to_advisory`.** A value-side
  `Conflict` needs a *valid-JSON* `pool.json` whose load-time name-uniqueness sweep
  fails (`load_membership_from`, membership.rs:455-468). `PoolMembership::insert` is
  fail-closed on name/by_id collisions, so build the fixture structurally rather
  than through the typed API: write a valid 2-disk membership (distinct
  names/UUIDs/by-ids) with `save_membership_to`, read it back and parse as
  `serde_json::Value`, set the second member's `name` -- addressed by its UUID key
  under the top-level `disks` object (`{ "disks": { "<uuid>": { "name": .. } } }`,
  membership.rs:228-242) -- equal to the first member's `name`, then write the
  re-serialized value back. The file still deserializes (UUID keys stay unique),
  and the sweep then raises `Conflict`. Mutating the parsed value (not
  string-replacing `save_membership_to`'s pretty output) keeps the fixture
  insensitive to JSON layout and field-order changes. Additionally assert the
  advisory carries the underlying conflict detail (the duplicated name), not just
  the remediation.
- **`cmd_status_io_membership_degrades_to_advisory`.** Force a non-NotFound `Io`
  by making `pool.json` a directory: `std::fs::create_dir(paths.pool_json())`.
  `read_to_string` then fails with a non-NotFound kind, hitting the degrade arm.
  Assert the same wrapped-advisory-plus-Ok contract.

## Verification

- `just test-rust` (or `cargo test -p braid-cli status`) -- the rewritten Corrupt
  test plus the new `Conflict` and `Io` degrade tests pass; the full status suite,
  `status_surfaces_mapper_conflict`, and
  `cmd_status_unmounted_corrupt_membership_returns_ok` still pass.
- `cargo build -p braid-cli` -- confirms the `StatusError::Membership` removal left
  no dangling references.
- `cargo clippy -p braid-cli` -- confirms no dead-code/unused-import warnings from
  the removed variant.
- Manual end-to-end (VM or host with a mounted braid pool): corrupt `pool.json`
  (e.g. `echo 'garbage' > <state>/pool.json`), run `braid status` -- expect exit 0,
  the full pool summary/capacity/scrub/per-disk detail rendered with present
  devices under mapper basenames, and a top advisory carrying the
  `run 'braid discover --write'` remediation. Confirm `braid status --json` exits 0
  and includes the message in its `advisories` array. Contrast with `braid doctor`
  on the same state (already a `Warn`) to confirm parity.
- `just docs-build` -- link-checks the status.md edit.
- ASCII check: `scripts/docs/check-output-ascii.py` stays green -- the new
  `membership_load_advisory` literal must stay ASCII (it mirrors the existing
  `discover` remediation, already ASCII).
