# Plan: revise `braid discover` bare-mode refusal wording

## Context

The `/verify-issue` finding flagged an inconsistency between the two
"healthy `pool.json`" refusal messages in `braid discover`:

- Bare mode (`cli/src/main.rs:915-921`):
  `pool.json already exists at {} -- use 'braid add' to add disks`
- `--write` mode (`cli/src/discover.rs:181-184`,
  `DiscoverWriteError::ValidUuidKeyed`):
  `discover refusing to write pool.json: existing file at {path} is
  already a healthy UUID-keyed membership -- back it up and move it
  aside before retrying, or use 'braid add' / 'braid remove' / 'braid
  replace' to mutate membership (see docs/luks-unlock.md)`

The finding proposed unifying the two via a shared canonical message.
That is the wrong shape: the "back it up and move it aside before
retrying" remediation belongs only to `--write`. In bare mode there is
nothing to "retry" -- moving the `pool.json` aside would only let the
next bare `discover` proceed to a read-only scan, and suggesting that
workflow from an error caused by the file being present is misdirection.

A first-pass pivot (broaden the bare-mode hint to list `braid add` /
`braid remove` / `braid replace`) is also not right. Two refinements
drive the chosen wording:

1. **Don't overclaim what was checked.** Bare discover refuses *before*
   scanning live disks. `classify_pool_json` in `cli/src/discover.rs:238-248`
   only confirms that `load_membership_from` parsed the file; it has no
   live-disk evidence. Calling the on-disk file "healthy", "valid",
   "loadable", or "matching live disk state" overclaims, and operators
   reading recovery docs may infer guarantees the message did not earn.
2. **Don't dump a command menu.** Listing `braid add` / `braid remove` /
   `braid replace` (and arguably `braid status`) in the error is noise;
   the operator's first need is to understand why discover refused, not
   to be handed a menu of unrelated commands.

The intended outcome: replace the bare-mode message with a short,
focused refusal that names discover's purpose ("rebuilding missing or
corrupt pool state"), letting the operator self-redirect to the right
tool. Keep `--write` untouched.

## New bare-mode message

```
pool.json already exists at {} -- live discovery is not authoritative once pool.json exists; discover is for rebuilding missing or corrupt pool state
```

Word-level rationale:

- `live discovery is not authoritative once pool.json exists` names the
  underlying principle behind the refusal. Once `pool.json` carries
  persisted membership (notably the `DiskMember.devid` bindings that
  decision 024 designates as the authorized fallback identity for
  `null_underlying` mappers and btrfs `missing_devids`), a fresh live
  scan of attached disks no longer represents the source of truth.
  The phrasing avoids the overclaim of "healthy" / "valid" / "loadable"
  by speaking to *authority* rather than *file health*; the gate only
  knows that an existing membership file is present and was accepted by
  the classifier.
- `discover is for rebuilding missing or corrupt pool state` scopes the
  command to its actual purpose. Operators whose intent was `braid add`
  / `braid status` / mutation will recognize that discover is not their
  tool, without the error having to list every alternative.

## Critical files to modify

### 1. `cli/src/main.rs:915-921`

Replace the bare-mode `ValidUuidKeyed` arm's `format!` string. Current:

```rust
braid_cli::discover::PoolJsonShape::ValidUuidKeyed => {
    print_cli_error(&format!(
        "pool.json already exists at {} -- use 'braid add' to add disks",
        pool_json.display()
    ));
    std::process::exit(1);
}
```

New:

```rust
braid_cli::discover::PoolJsonShape::ValidUuidKeyed => {
    print_cli_error(&format!(
        "pool.json already exists at {} -- live discovery is not authoritative once pool.json exists; discover is for rebuilding missing or corrupt pool state",
        pool_json.display()
    ));
    std::process::exit(1);
}
```

No code comment is added at the call site. The new message states the
refusal and the command's purpose directly; it no longer shares any
phrase or framing with the `--write` variant, so there is no live
temptation to "unify" them.

No change to the adjacent `Corrupt` arm. No change to the
already-present block comment at `main.rs:904-910`.

### 2. `tests/cli/braid-discover.py:186-190`

Replace the bare-mode subtest's assertion to pin the new wording, and
rename the subtest so it no longer asserts "healthy" either. Current:

```python
with subtest("bare discover refuses healthy UUID-keyed pool.json"):
    out = machine.fail("braid discover 2>&1")
    assert "pool.json already exists at /var/lib/braid/pool.json" in out, (
        "expected existing pool.json refusal; got:\n" + out
    )
```

New:

```python
with subtest("bare discover refuses existing UUID-keyed pool.json"):
    out = machine.fail("braid discover 2>&1")
    assert "pool.json already exists at /var/lib/braid/pool.json" in out, (
        "expected existing pool.json refusal; got:\n" + out
    )
    # Pin the two load-bearing clauses of the bare-mode refusal text.
    assert "live discovery is not authoritative once pool.json exists" in out, (
        "expected authority-principle clause; got:\n" + out
    )
    assert "rebuilding missing or corrupt pool state" in out, (
        "expected command-purpose clause; got:\n" + out
    )
```

The adjacent `--write` subtest at lines 192-199 ("`discover --write`
also refuses healthy UUID-keyed pool.json") is unchanged -- the
`--write` message still says "healthy UUID-keyed membership", and the
existing substring assertion still matches.

The file-level test preamble (intent / why / scenario, per
`docs/testing.md`) stays as-is -- the contract being tightened is just
the bare-mode assertion, not the test's purpose.

### 3. `manual/commands/discover.md` (lines 56 and 70)

Two operator-facing prose bullets currently call the refused file
"healthy UUID-keyed". That phrasing predates this plan and contradicts
its rationale: the gate fires on classifier acceptance of an existing
file, not on any live-disk health check. Both bullets cover the bare
mode case (line 56 explicitly notes "(bare and `--write`)"; line 70
says "any operation"), so the manual must be brought into alignment
with the new bare-mode message even though the `--write` *error
string* still says "healthy".

Edit at line 56 -- swap "healthy" -> "existing":

Current:

```
2. Refuses on a healthy UUID-keyed `pool.json` (bare and `--write`). A corrupt or off-schema `pool.json` is the documented rebuild path: ...
```

New:

```
2. Refuses over an existing UUID-keyed `pool.json` (bare and `--write`). A corrupt or off-schema `pool.json` is the documented rebuild path: ...
```

Edit at line 70 -- swap "healthy" -> "existing":

Current:

```
- Refuses any operation on a healthy UUID-keyed `pool.json`. Corrupt or off-schema files are allowed for `--write` rebuild only; ...
```

New:

```
- Refuses any operation on an existing UUID-keyed `pool.json`. Corrupt or off-schema files are allowed for `--write` rebuild only; ...
```

No other prose in `manual/commands/discover.md` needs to change. The
rest of the document (corrupt/off-schema rebuild remediation, forensic
sidecar behavior, run-with-members-attached guidance) is independent
of the bare-mode wording shift.

## Files explicitly NOT modified

- `cli/src/discover.rs` -- `DiscoverWriteError::ValidUuidKeyed` Display
  stays exactly as it is. The unit test
  `discover_write_refuses_when_pool_json_is_valid_uuid_keyed` at
  `cli/src/discover.rs:1837-1876` asserts on
  `"is already a healthy UUID-keyed membership"`, which is unaffected.
- `tests/cli/braid-discover-name-order.py` -- does not assert on any
  pool.json refusal wording (grepped for `pool.json already exists`,
  `already a healthy`, `UUID-keyed`).
- `tests/module/pool-lock-discover-contention.py:71` -- asserts on the
  `--write` wording only.
- `docs/luks-unlock.md`, `manual/guides/recovery-scenarios.md` --
  contain operator-facing prose about mutators; unaffected by the
  bare-mode wording change.
- `plans/impl/2026-05-13-discover-valid-pool-json-refusal.md` and
  `plans/impl/2026-05-18-discover-write-pool-json-gates.md` -- completed
  plan documents; treated as historical artifacts.

## Verification

1. `just test-rust` -- the Rust unit test surface for `DiscoverWriteError`
   (`cli/src/discover.rs:1837-1876`) does not touch the bare-mode
   message, but the run confirms nothing else broke.
2. `just test-vm braid-discover` -- exercises the bare-mode subtest with
   the tightened assertions pinning the new authority-principle and
   command-purpose clauses. Should pass with the new message and fail
   if either clause is removed or reworded.
3. Manual sanity check (optional, after the VM test passes): in any
   test VM that already has a `pool.json` (e.g. the `braid-discover` VM
   after its initial `--write` step), run `braid discover` and confirm
   the new message ends with
   `discover is for rebuilding missing or corrupt pool state`.

No fixture regeneration is required -- parser-critical tool versions
are not involved.

## Implementation notes

- A prior refactor moved the bare-mode refusal from `cli/src/main.rs` into
  `cli/src/discover.rs` as `BareDiscoverError` and added a Rust unit test
  for that helper, so the implementation updated that current message
  surface. `DiscoverWriteError::ValidUuidKeyed` remains untouched.
