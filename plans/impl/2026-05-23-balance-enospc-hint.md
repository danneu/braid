# Plan: Improve balance ENOSPC hint to cover data + metadata

## Context

`cli/src/pool.rs:252-267` (`balance_error()`) detects "no space left" in
balance stderr and appends a recovery hint, but the hint only mentions
`-dusage=0` (data side). The three `balance start` requests issued by
`pool_balance_raid1`, `pool_balance_single`, and
`pool_balance_raid1_soft` all pass both `-dconvert` and `-mconvert`
(`cli/src/cmd.rs:647-681`), so ENOSPC can legitimately originate on
the metadata side too. The fourth caller, `pool_balance_resume`,
sends a bare `btrfs balance resume <mp>` (`cli/src/cmd.rs:682-685`)
that reuses the convert filters the kernel persisted in the chunk
tree's `BALANCE_ITEM` from one of the three interrupted braid
balances, so it carries the same data+metadata exposure. When ENOSPC
hits the metadata side the current hint sends the user down a dead
end.

The sibling helper `device_remove_error()` at
`cli/src/pool.rs:301-335` is the in-file precedent for pairing
`btrfs filesystem usage` with a targeted `btrfs balance` command in
recovery hints. `balance_error` intentionally diverges on order:
the device-remove case has no safe blind action (the operator must
inspect to decide which profile to convert back to), whereas the
balance ENOSPC case has a known workspace-free reclaim
(`-dusage=0 -musage=0`) that the user can run blindly, so the hint
leads with that action and only suggests `btrfs filesystem usage` if
the retry still fails.

`docs/guides/troubleshooting.md:7-19` mirrors the same data-only gap,
so the doc needs the same fix to stay consistent.

Intended outcome: a single ENOSPC during any braid balance gives the
user (a) one immediate zero-cost recovery action covering both data
and metadata, and (b) a diagnostic command (`btrfs filesystem usage`,
also used by `device_remove_error`) for the case where the retry
still fails. CLI hint and troubleshooting doc use the same order:
action first, diagnostic only on failure.

## Changes

### 1. `cli/src/pool.rs` -- extend the hint in `balance_error()` (lines 252-267)

Replace the single `-dusage=0` line with a hint that:

1. Recommends one combined `btrfs balance start -dusage=0 -musage=0
   {mount_point}` -- both filters are workspace-free
   empty-block-group reclaims per
   `reference/btrfs-progs/Documentation/btrfs-balance.rst:437-454`
   ("GETTING RID OF COMPLETELY UNUSED BLOCK GROUPS"), and combining
   `-d` and `-m` filters in one command is explicitly supported by
   the same doc at line 167 ("Options for all block group types can
   be specified in one command.").
2. Suggests `btrfs filesystem usage {mount_point}` as the follow-up
   diagnostic only when the reclaim retry still fails.
3. Uses the same source-line-continuation style as
   `device_remove_error` so the message reads as one logical line.

Sketch (final wording in implementation):

```rust
PoolError::Failed(format!(
    "{label} failed (exit {}): {}\n\
     hint: reclaim empty block groups with \
     `btrfs balance start -dusage=0 -musage=0 {mount_point}`, \
     then retry. If the failure repeats, inspect chunk usage with \
     `btrfs filesystem usage {mount_point}` to see whether data or \
     metadata is the bottleneck.",
    result.exit_status,
    result.stderr.trim(),
))
```

CLI style invariant from `AGENTS.md` (ASCII `--`, no em-dash) is
preserved.

### 2. Update the unit test for `balance_error` (`cli/src/pool.rs:1198-1222`)

`balance_error_detects_enospc` currently asserts `"hint:"` and
`"dusage=0"`. Tighten it to assert the new contract:

- still contains `"hint:"`
- contains the combined-filter substring `"-dusage=0 -musage=0"`
  (new -- single assertion proves both filters appear together in
  one command rather than independently)
- contains `"btrfs filesystem usage"` (new)
- contains the concrete mount point

Update the `// Why` preamble line to mention both data and metadata.
The negative test `balance_error_no_hint_for_other_failures` does not
need changes.

### 3. Update the integration test
   `enospc_hint_surfaces_through_error_chain` in
   `cli/src/remove_missing.rs:1662-1690`

The test already asserts the hint propagates through
`PoolError -> RemoveMissingError::Pool -> display`. Extend the
assertions to match the new contract:

- still requires `"hint:"`
- replace the `"dusage=0"` assertion with the combined-filter
  substring `"-dusage=0 -musage=0"`
- additionally requires `"btrfs filesystem usage"`

This confirms the full surface (not just the helper) carries the
expanded hint.

### 4. `docs/guides/troubleshooting.md` -- expand the ENOSPC entry (lines 7-19)

Rewrite the section to mirror the new CLI hint shape, in the same
order: action first, diagnostic only on failure.

- Note that braid's balances convert both data and metadata profiles,
  so either side can hit ENOSPC.
- Lead with the combined zero-cost reclaim:
  `sudo btrfs balance start -dusage=0 -musage=0 /mnt/storage`, then
  retry the original operation.
- If the retry still fails, show `sudo btrfs filesystem usage
  /mnt/storage` as the diagnostic step (look at the Data vs Metadata
  used/size ratios to identify the bottleneck side).
- Show the more expensive escalation last (`-dusage=10`,
  `-musage=10`), explicitly noting that non-zero thresholds move data
  and need temporary work space.
- Keep the section scannable -- the current entry is short and the
  rewrite should remain so.

No other doc changes are needed. Inbound links to this doc
(`README.md:164`, `docs/SUMMARY.md:18`, `docs/index.md:28`,
`docs/guides/recovery-scenarios.md`) keep working without edits.

## Files modified

- `cli/src/pool.rs` -- helper body + unit test assertions/comment
- `cli/src/remove_missing.rs` -- integration test assertions
- `docs/guides/troubleshooting.md` -- expanded ENOSPC entry

## Out of scope

- No changes to `balance_error`'s signature or call sites; only the
  message text changes.
- No new `PoolError` variant; existing `PoolError::Failed` carries the
  expanded hint exactly as today.
- No changes to `replace_error` or `device_remove_error`.
- No locale / `LC_ALL` work -- already handled by commit `48249c2`.

## Verification

1. `just test-rust` -- unit + integration tests pass with the
   tightened assertions. The two updated tests
   (`balance_error_detects_enospc` and
   `enospc_hint_surfaces_through_error_chain`) prove the hint is
   present at both the helper and the command-error-chain layer.
2. Visually inspect the rendered error in
   `balance_error_detects_enospc`'s captured message to confirm the
   final wording reads cleanly and uses ASCII `--` only.
3. `mdbook build docs` -- ensure `docs/guides/troubleshooting.md`
   renders and `mdbook-linkcheck` reports no broken cross-links.
4. No VM test run is required -- this is pure error-message + doc
   work, no behavior change to commands or systemd units.
