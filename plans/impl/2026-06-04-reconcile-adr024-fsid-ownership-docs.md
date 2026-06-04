# Plan: reconcile the ADR 024 / lock-code "FSID proves ownership" mismatch

## Context

The impl follow-up from `a41db23e` (test coverage for the `Snapshot::ProbeFailed`
lock arm) named a documented-vs-implemented mismatch to resolve:

ADR 024 paragraph 7 and three code comments claim that, when mounted per-device
probing fails, `braid lock` "requires the mounted filesystem FSID to prove braid
owns the mount." **The code does not do that.** In `cli/src/lock.rs#plan_lock`,
the `ProbeFailed` arm reads the FSID via `cli/src/probe.rs#probe_fsid` and passes
it to `cli/src/preflight.rs#require_lock_preflight`, which uses it *only* to key
`/sys/fs/btrfs/<fsid>/exclusive_operation`. The FSID is never compared to any
recorded pool identity, because **braid persists no durable pool FSID**:

- `PoolMembership` / `DiskMember` (pool.json, `cli/src/membership.rs`) is keyed by
  LUKS UUID with name/by_id/devid metadata -- no FSID field.
- The only persisted FSID is the journal's transient `verified_pool_fsid`
  (`cli/src/journal.rs#AddJournalMode::RecoverableBraidLabeled`), an `add`-recovery
  plan-vs-replay cross-check consumed in `cli/src/recover.rs` -- not a durable
  anchor, gone once the op completes.
- `replace`'s FSID check (`cli/src/replace.rs`, `fresh_pool.fsid != planned_pool.fsid`)
  is likewise plan-probe vs execute-probe, not a persisted anchor.

So the practical license to unmount is **mount-point occupancy**, not FSID
identity. The follow-up asked us to pick one of two reconciliations and apply it
across the touchpoints together (it named four; planning surfaced a fifth -- see
Changes):

- **(a) Reword to match the code** -- document mount-point ownership as the policy.
- **(b) Strengthen the code to match the ADR** -- persist a pool FSID and verify it.

## Decision: (a), reworded to the two-tier teardown model

Reword the five touchpoints to describe what the code actually does. **Do not**
persist a pool FSID.

Rationale for rejecting (b):

- **The only behavior (b) changes is benign and low-probability.** (b) matters
  solely when a *foreign* btrfs is mounted at braid's own mount point (operator
  misuse). Even today's outcome there is a reversible, EBUSY-safe `umount`
  (`cli/src/lock.rs#umount_with_retry` runs `umount <mp>` with no `-f`/`-l`, so an
  in-use foreign mount fails with an lsof hint rather than being yanked), then
  runs its normal cleanup. That cleanup is scoped to the `/dev/mapper/braid-*`
  namespace and gated by backing LUKS UUID -- members close as members, verified
  non-members as orphans, non-`braid-*` devices and unverified candidates are
  skipped -- so it acts on whatever `braid-*` mappers exist, independent of which
  devices backed the unmounted filesystem. A foreign btrfs normally sits on
  non-`braid-*` devices, so the realistic consequence is the unmount alone. An
  unexpected unmount of someone else's filesystem at braid's own mount point is
  not a fail-closed-worthy downstream failure mode.
- **(b) fights ADR 024's core win.** The ADR's headline benefit is *one* persistent
  identity axis. A durable pool-level FSID in pool.json is a second axis braid
  must write at pool creation, maintain, and bootstrap through `discover`.
- **(b) tensions with the arm's purpose.** `ProbeFailed` exists so teardown still
  works under a probe quirk; a hard FSID gate adds a new way for `lock` to refuse.
- **The "don't pre-gate with a weaker observable" heuristic is already satisfied**
  for the operation that has consequences: closing mappers queries the
  authoritative source (LUKS UUID) directly. The unmount is non-destructive and is
  not the unsafe operation that heuristic targets.

The honest model the code implements is a **two-tier teardown**: the *unmount* is
licensed by mount-point ownership (non-destructive, EBUSY-safe); the *close/lock*
of LUKS mappers is gated by per-mapper backing-LUKS-UUID classification -- verified
members close as members, verified non-member `braid-*` mappers close as orphans,
and unverified candidates are skipped -- the real, implemented gate, decided by
UUID, not mapper name. The FSID's only job is to key the exclusive-op preflight so
lock won't unmount mid balance/replace.

## Changes (doc-only; no code logic, no behavior, no test changes)

All edits are `///` doc comments or ADR prose. The inventory is these five
touchpoints: the four the follow-up named, plus
`cli/src/lock.rs#build_close_sets_uuid_scanned_fallback`'s doc comment
("only FSID proof for the filesystem") -- touchpoint 5, which the Explore sweep's
broad `prove|own` pattern missed because that line carries no `prove`/`own` token.
An FSID-anchored grep (see Verification step 3) finds exactly these five in
source. That broad pattern also matches four *legitimate* LUKS-UUID ownership
comments (`lock.rs:218`, `lock.rs:1065`, `lock.rs:4968`,
`cli/src/probe_mapper_uuid.rs`) that correctly defer ownership to the UUID -- those
are **left untouched** (editing them to silence a grep would regress accurate
docs). Rewording is test-safe: the pinned warn substrings
(`"falling back to UUID-scanned mapper cleanup."`,
`"unverified candidates are skipped."`, `"per-device probe failed ("`,
`"not a /dev/mapper/ path"`) live in the `format!` return value of
`uuid_scanned_fallback_warn_body`, **not** in any doc comment being changed, and
no test asserts the literal phrases "proves ownership" / "FSID matched" /
"owns the mount".

### 1. `cli/src/lock.rs#Snapshot` enum doc comment

Swap the inaccurate parenthetical only.

- Before: `... a mounted pool whose per-device probe failed (FSID still proved ownership), and an unmounted pool ...`
- After:  `... a mounted pool whose per-device probe failed (FSID only keys the exclusive-op preflight), and an unmounted pool ...`

### 2. `cli/src/lock.rs#Snapshot::ProbeFailed` variant doc comment

- Before:
  ```
  /// Pool is mounted and FSID matched, but per-device probing
  /// failed. `fsid` feeds preflight; `probe_error` is quoted in the
  /// fallback warning.
  ```
- After:
  ```
  /// Pool is mounted (btrfs occupies the mount point); per-device
  /// probing failed. `fsid` is read only to key the exclusive-op
  /// preflight -- it is not compared to any persisted pool identity
  /// (braid persists none); `probe_error` is quoted in the fallback
  /// warning.
  ```

### 3. `cli/src/lock.rs#uuid_scanned_fallback_warn_body` doc comment

Change the `///` comment above the fn only -- the returned `format!` string is
untouched.

- Before:
  ```
  /// Message body (no `[warn]` prefix) for the mounted fallback warning.
  /// The FSID preflight still proves braid owns the mount, but mapper
  /// cleanup remains UUID-gated and unverified candidates are skipped.
  ```
- After:
  ```
  /// Message body (no `[warn]` prefix) for the mounted fallback warning.
  /// The unmount is licensed by mount-point ownership; the destructive
  /// close stays UUID-gated, so only verified braid-* mappers are closed
  /// and unverified candidates are skipped. The FSID only keys the
  /// exclusive-op preflight, not an ownership check.
  ```

### 4. `docs/design/decisions/024-luks-uuid-identity.md` (Active ADR)

Two coordinated edits keep mechanism and rationale in their right structural
homes and avoid duplicating the policy text.

**(4a) `## Runtime Handles And Labels`, item 7** -- make the mechanism accurate:

- Before: `If mounted per-device probing fails, `lock` first requires the mounted filesystem FSID to prove braid owns the mount, then scans `/dev/mapper/braid-*` candidates and closes only those with verified backing LUKS UUIDs.`
- After:  `If mounted per-device probing fails, `lock` reads the mounted filesystem FSID to key the exclusive-operation preflight (so it will not unmount mid balance/replace), then scans `/dev/mapper/braid-*` candidates and closes only those with verified backing LUKS UUIDs. The unmount is licensed by mount-point ownership, not an FSID identity match (see Limits And Non-Goals).`

**(4b) `## Limits And Non-Goals`** -- add one bullet recording the accepted
behavior and the rationale for not persisting a pool FSID:

> - `lock`'s mounted-fallback teardown unmounts the configured btrfs mount point
>   (licensed by mount-point ownership, not an FSID identity match -- braid
>   persists no durable pool FSID to compare a probe against), then scans only
>   `/dev/mapper/braid-*` and closes by backing LUKS UUID: verified member UUIDs
>   close as members, verified non-member `braid-*` mappers close as orphans;
>   non-`braid-*` devices and unverified candidates are skipped. The cleanup is
>   scoped by the `braid-*` namespace plus UUID, not by which devices backed the
>   unmounted filesystem. Consequence: a foreign btrfs at braid's mount point
>   would be unmounted (a non-destructive, EBUSY-safe `umount` with no `-f`/`-l`);
>   a foreign filesystem normally sits on non-`braid-*` devices, so the realistic
>   consequence is the unmount alone. This is accepted, and gating it would
>   require a durable pool-FSID identity axis this decision deliberately omits to
>   keep membership single-axis.

### 5. `cli/src/lock.rs#build_close_sets_uuid_scanned_fallback` doc comment

Drop the "FSID proof" framing -- the FSID is filesystem-level evidence that keys
the preflight, not per-mapper ownership. The sentence's logic (every candidate
must be UUID-verified before it enters the close set) is unchanged; only the word
"proof" goes. **Leave the next clause's "every candidate must prove ownership ...
by backing LUKS UUID" alone** -- that is the correct UUID-ownership statement.

- Before:
  ```
  /// Close-set construction for fallback cleanup. The mounted variant has
  /// only FSID proof for the filesystem, and the unmounted variant has no
  /// btrfs probe at all, so every candidate must prove ownership or orphan
  /// status by backing LUKS UUID before it enters the close set.
  ```
- After:
  ```
  /// Close-set construction for fallback cleanup. The mounted variant has
  /// only the filesystem FSID (it keys the exclusive-op preflight, not an
  /// ownership check), and the unmounted variant has no btrfs probe at all,
  /// so every candidate must prove ownership or orphan status by backing
  /// LUKS UUID before it enters the close set.
  ```

## Explicitly NOT in scope

- No change to `plan_lock`, `probe_fsid`, `require_lock_preflight`, or the
  `uuid_scanned_fallback_warn_body` return string.
- No new persisted FSID field in pool.json, config, or the journal.
- No test changes (the new `braid-lock-probe-failed` VM test and the unit tests
  from `a41db23e` were written to not depend on the reworded wording).

## Verification

1. `just test-rust` -- still green (no source-logic or warn-string change;
   `uuid_scanned_fallback_warn_body_contains_pinned_substrings` unaffected).
2. `mdbook build docs` -- ADR edits introduce no broken cross-links (no new links
   added; "see Limits And Non-Goals" is prose, not a Markdown link, matching the
   surrounding numbered-list style which uses no `path#anchor` refs).
3. `rg -n "FSID.*(prove|proved|proof|matched|owns the mount)" cli/src docs README.md -g '!docs/book'`
   -- before the edits this matches exactly the five touchpoints; after rewording
   all five it returns nothing. The pattern is FSID-anchored and uses "matched"
   (past tense), so it skips the legitimate LUKS-UUID ownership comments
   (`lock.rs:218`/`1065`/`4968`, `probe_mapper_uuid.rs`) and the many valid
   "FSID match/mismatch" add/replace references. `-g '!docs/book'` excludes the
   generated mdBook output (gitignored; regenerated by step 2).
4. Doc-only, additive-clarity change -- no VM suite run required.
