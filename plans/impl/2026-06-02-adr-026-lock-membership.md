# Fix: correct the stale lock-membership section in ADR 026

## Context

`docs/design/decisions/026-pool-lock-rust-owned.md` has a section,
**"Bootstrap-Journal Membership Fallback"** (lines 89-104), that describes a
mechanism the code no longer implements: it claims `braid lock` falls back to
`pending-op.json` -- reading `target_membership` from a structurally-bootstrap
`OpKind::Add` journal -- when `pool.json` is absent.

The section was accurate when written (`1e67c08b`), but `8db7277b fix(lock):
tolerate missing pool membership` replaced that mechanism the same day and
**updated docs 017, 018, and `luks-unlock.md` but missed doc 026**. The current
lock path consults no journal at all:

- `load_membership_for_lock` (`cli/src/main.rs:1127-1160`) calls
  `load_membership` (`cli/src/membership.rs:424-426`), which reads `pool.json`
  only. On *any* error it returns `PoolMembership::empty()` plus a warning.
- All three lock dispatch arms use it: `run_dry_run_lock:1166`,
  `run_plain_lock:1196`, `run_systemd_stop_lock:1266` (the ExecStop path).
- What lock closes is derived from observed state in three ways, not one
  uniform UUID probe (`build_close_sets_full:952`,
  `build_close_sets_uuid_scanned_fallback:1073`): (a) present mounted-pool
  devices proven by the probe's `cryptsetup status` + `cryptsetup luksUUID`
  (`cli/src/probe.rs:441,468`); (b) null-underlying mounted-pool entries
  classified by persisted btrfs devid (`lock.rs:985`); (c) UUID-scanned
  `/dev/mapper/braid-*` fallback candidates that pass `cryptsetup status` +
  `cryptsetup luksUUID` (`classify_candidate_mapper:220-264`). With empty
  membership these classify as unnamed orphans rather than named members and
  are still closed; unverified candidates, `/dev/mapper` scan failures, or
  duplicate-devid skips warn and may leave cleanup incomplete
  (`lock.rs:1018,1046`).

Outcome: a reader or future maintainer is told the lock path reads a journal
and is scoped per-`OpKind`; neither is true. The fix is a documentation-only
rewrite of this one section so it matches the code.

## Scope

- **One file, one section.** `docs/design/decisions/026-pool-lock-rust-owned.md`,
  the `## Bootstrap-Journal Membership Fallback` section (lines 89-104). Every
  other section of 026 (Snapshot Rule, Stop Coordinator, Consequences) describes
  unrelated mechanisms and is correct.
- **No sibling staleness.** A docs-tree sweep confirmed the only stale
  lock-path `pending-op.json` reference is this section; all other
  `pending-op.json` doc mentions (017, 022, 024, `discover`, `luks-unlock`) are
  legitimately about recover/journal/status/preflight.
- **Rename is safe.** No cross-link in the repo targets the
  `#bootstrap-journal-membership-fallback` anchor (verified by grep). Other
  links into 026 point at different anchors and are unaffected.
  `mdbook-linkcheck2` validates anchor fragments (`docs/book.toml:11`), so this
  was worth confirming -- it is clear.

## Constraints honored by the rewrite

- **Contract, not internals.** Per CLAUDE.md ("Architecture docs describe
  behavioral contracts, not internal helper names"), the rewrite describes the
  observable contract -- pool.json is non-authoritative for lock, empty-on-error
  + warning, and how lock decides what to close (probe, btrfs devid, and
  UUID-scanned verification) -- and names only
  tool commands (`cryptsetup status`, `cryptsetup luksUUID`) and the public
  `braid lock --systemd-stop` entry point. It does not name internal helpers
  like `load_membership_for_lock` or `build_close_sets_uuid_scanned_fallback`.
- **ASCII / house style.** Use `--` not em-dash, straight quotes, Title Case
  heading (matches sibling ADR headings).

## The change

Rename the heading and replace the body. Recommended heading:
**`## Lock Tolerates Missing Or Corrupt Membership`** (mirrors the implementing
commit's own language, "tolerate missing pool membership"; acceptable
alternative: `## Membership Is Non-Authoritative For Lock`).

Replacement body:

```markdown
## Lock Tolerates Missing Or Corrupt Membership

Lock-side dispatch loads pool membership from `pool.json` only; it consults no
recovery journal. If `pool.json` is missing, unreadable, corrupt, or fails its
uniqueness checks, lock does not abort -- it warns and proceeds with empty
membership. On the live plain-lock and `braid-online.service` ExecStop paths the
warning goes to stderr; under `--dry-run` it is folded into the stdout preview
to preserve the single-stream dry-run contract ([ADR 022](022-dry-run-preview-model.md)).

Membership is advisory for lock, not authoritative -- its only role here is to
attach friendly member names to status output. What lock closes is decided from
observed state, not from `pool.json`:

- mappers backing the live mounted pool, proven during the per-device probe by
  `cryptsetup status` + `cryptsetup luksUUID`;
- mounted-pool members whose backing device is gone (`device: (null)`), matched
  by their persisted btrfs device id;
- otherwise-stranded `/dev/mapper/braid-*` mappers, each confirmed by
  `cryptsetup status` + `cryptsetup luksUUID` (see
  [ADR 024](024-luks-uuid-identity.md)) before it is closed.

With empty membership these mappers classify as unnamed orphans rather than
named members and are still closed. Fallback scanning is limited to
`/dev/mapper/braid-*`; mounted-pool cleanup closes only the mapper paths
reported by the pool mounted at the configured mount point. A candidate that
fails verification, a `/dev/mapper` scan that fails, or a duplicate-devid
conflict is skipped with a warning and may leave cleanup incomplete -- the
operator resolves it by re-running `braid lock` or reconciling `pool.json`.

This closes the failed-bootstrap-add lifecycle hole without a journal. A
bootstrap add can mount the pool and open its LUKS mappers, then fail before
`save_membership` writes the first `pool.json`. If shutdown follows,
`braid-online.service` ExecStop runs `braid lock --systemd-stop`, finds no
`pool.json`, and still unmounts and closes those mappers -- because what to
close is read from the live mounted pool and the observed mappers, not from
`pool.json`.

Lock therefore needs no special case for *which* operation was interrupted. An
interrupted `Remove`, `RemoveMissing`, `Replace`, or live-pool `Add` is
reconciled by `braid recover` against its `pending-op.json` journal; lock
neither reads nor needs that journal to perform safe shutdown cleanup.
```

Rationale for the final paragraph: the finding said to drop the per-`OpKind`
scope contrast (correct -- the old "falls back for Add, not Remove/Replace"
framing is meaningless once no journal is read). Rather than delete it outright,
this reframes it to answer the question a reader will still have ("what about an
interrupted remove/replace at shutdown?") with the accurate division of labor:
lock closes mappers from observed state regardless of op; `recover` owns journal-based
membership reconciliation. This is verified -- the lock path reads no journal,
and `recover.rs` is the sole consumer of `pre_membership`/`target_membership`.

## Verification

- **Accuracy read-through:** confirm each claim in the new section against the
  cited code -- membership load (`main.rs:1127-1160`, `membership.rs:424-426`),
  the per-device probe and null-underlying recording (`probe.rs:424-478`), and
  the three close-set paths (`build_close_sets_full` at `lock.rs:952-1067`,
  `build_close_sets_uuid_scanned_fallback` at `lock.rs:1073-1102`, and
  `classify_candidate_mapper` at `lock.rs:220-264`).
- **Link/anchor integrity (CI gate):** run the docs build that CI runs --
  `nix develop .#docs -c mdbook build docs` -- and confirm `linkcheck2` passes.
  This validates the two new file-level cross-links (to ADR 022 and ADR 024,
  both existing files) and confirms the heading rename broke nothing.
- No code, tests, or other docs change; nothing else to run.
```

## Implementation notes

- The implemented ADR text says "braid writes the first `pool.json`" instead of
  naming `save_membership`, because the plan's own contract-not-internals
  constraint is stricter than the draft replacement paragraph.
