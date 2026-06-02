# Fix stale "lock --dry-run requires a loadable pool.json" claim

## Context

Two docs assert that `braid lock --dry-run` is "the exception" that still
requires a loadable `pool.json` and hard-fails otherwise. This is false. Both
`braid lock` and `braid lock --dry-run` route through the same lenient loader
`cli/src/main.rs#load_membership_for_lock`, which on **every** `MembershipError`
variant (Io / Corrupt / Conflict / DuplicateDevid) returns
`PoolMembership::empty()` plus a `PreviewNote::Warn` -- it never errors. Both
`run_dry_run_lock` and `run_plain_lock` call it; the only difference is stream
routing (dry-run folds the warning into the stdout preview, the real paths emit
to stderr). The per-candidate `cryptsetup luksUUID` probe in
`cli/src/lock.rs#build_close_sets_full` / `#build_close_sets_uuid_scanned_fallback`
is the fail-closed guard, so cleanup is correct against empty membership.

The claim went stale at commit `cc1ae25e` (2026-05-25, "route membership warning
through preview notes"), which made dry-run lenient. The contract was then
**corrected canonically** in `docs/design/decisions/026-pool-lock-rust-owned.md`
on 2026-06-02 ("docs(adr): correct lock membership contract") -- but that pass
missed two copies of the old wording. Current behavior is pinned by
`tests/module/lock-tolerates-missing-pool-json.py` (subtest "Dry-run lock
previews teardown on stdout without pool.json").

Impact: an operator debugging a corrupt `pool.json` reads these docs, concludes
`braid lock --dry-run` will refuse as a loadability probe, and is wrong -- it
emits a `[warn]` preview note and renders a UUID-scanned cleanup plan against
empty membership.

Outcome: both stale statements corrected to agree with the canonical ADR-026
section. Documentation-only; no code or test change.

## The canonical statement (alignment target -- do not edit)

`docs/design/decisions/026-pool-lock-rust-owned.md#lock-tolerates-missing-or-corrupt-membership`
already says it right:

> If `pool.json` is missing, unreadable, corrupt, or fails its uniqueness
> checks, lock does not abort -- it warns and proceeds with empty membership. On
> the live plain-lock and `braid-online.service` ExecStop paths the warning goes
> to stderr; under `--dry-run` it is folded into the stdout preview to preserve
> the single-stream dry-run contract.

The corrected sentences below are worded to match this voice.

## Edits

### 1. `docs/internals/luks-unlock.md` -- "Unparseable state-file reconciliation"

The `Note:` paragraph (the two sentences that today end with "`braid lock
--dry-run` is the exception: the preview pathway still requires a loadable
`pool.json`.").

Replace the whole Note with:

> Note: `braid lock` -- the user-facing command, the `braid-online.service`
> ExecStop path, and `braid lock --dry-run` alike -- does NOT fail under a
> missing or corrupt `pool.json`. It warns and proceeds with empty membership;
> every observed `braid-*` mapper is then verified by its backing LUKS UUID
> before close, so shutdown cleanup stays complete. No lock pathway hard-fails
> on an unloadable `pool.json`.

Rationale: this section is an operator recovery recipe, so it states the
consequence ("won't refuse -- warns and cleans up") inline and self-contained.
No new cross-link here (keeps the recipe terse; the stream detail belongs in the
ADRs).

### 2. `docs/design/decisions/017-runtime-disk-membership.md` -- "State contract" bullet

The bullet beginning "Non-dry-run `braid lock` ... tolerates a missing or
corrupt `pool.json`". **Keep sentences 1-2 in substance** -- the
`build_close_sets_*` (`cli/src/lock.rs`) fail-closed-guard citation is accurate
and verified-live and must be preserved. Only fold dry-run into sentence 1 and
**replace the final sentence** (the wrong "still requires a loadable
`pool.json`" one).

Replace the bullet with:

> - `braid lock` -- the user-facing command, the `braid-online.service` ExecStop
>   reentry, and `braid lock --dry-run` -- tolerates a missing or corrupt
>   `pool.json`: it warns and proceeds with empty membership. The per-candidate
>   `cryptsetup luksUUID` probe in `build_close_sets_*` (`cli/src/lock.rs`) is
>   the fail-closed guard, so cleanup remains complete and correct. No lock
>   pathway hard-fails on an unloadable `pool.json`; dry-run folds the warning
>   into its stdout preview while the real paths emit it to stderr (see
>   [ADR 026](026-pool-lock-rust-owned.md#lock-tolerates-missing-or-corrupt-membership)).

Rationale: ADR-to-ADR cross-references are idiomatic here (ADR-026 itself links
ADR-022), and pointing ADR-017 at the canonical ADR-026 section is the
drift-prevention measure that keeps these from desyncing again. The link target
is same-directory and the heading slug is validated by `mdbook-linkcheck2`.

## Explicitly do NOT touch (already correct)

- `docs/design/decisions/026-pool-lock-rust-owned.md#lock-tolerates-missing-or-corrupt-membership` -- canonical, the alignment target.
- `docs/design/decisions/018-systemd-lifecycle.md` -- "Lock dispatch loads membership ... if `pool.json` is absent or corrupt, it warns and proceeds" (no dry-run exception; correct).
- `cli/src/main.rs#load_membership_for_lock` doc comment -- correct.
- `docs/guides/recovery-scenarios.md` `sudo braid lock` note -- correct.
- `docs/commands/lock.md` -- makes no pool.json claim.
- Code comments in `remove.rs` / `remove_missing.rs` / `replace.rs` / `enroll_key_file.rs` that say dry-run *refuses* on pool.json drift -- those are about **other** commands where pool.json is authoritative; lock is the documented exception. Do not sweep them.

No code change. No test change (`lock-tolerates-missing-pool-json.py` already
pins the correct behavior; ADR-022/ADR-026 already own the stream contract, so
no new stream note is needed anywhere).

## Verification

1. Sweep confirms zero remaining stale claims:
   `rg -n "dry-run.*(requires|exception).*pool\.json|pool\.json.*dry-run.*(requires|exception)" docs/ README.md`
   should return nothing. Also eyeball `rg -n "is the exception" docs/` -- the
   only remaining hits should be the unrelated "paused balance is the exception"
   lines in the command docs.
2. `mdbook build docs` succeeds -- validates the new ADR-026 cross-link and its
   heading anchor via `mdbook-linkcheck2` (a bad anchor fails the build).
3. Spot-read both edited sites to confirm the corrected text reads cleanly in
   context and the `build_close_sets_*` citation survived in ADR-017.

No VM tests required -- this is a docs-only accuracy fix and the governing
behavior test (`lock-tolerates-missing-pool-json.py`) is unchanged.
