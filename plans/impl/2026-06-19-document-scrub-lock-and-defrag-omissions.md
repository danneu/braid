# Plan: document two deliberate ADR omissions (scrub-skips-pool-lock; no defrag)

## Context

Two intentional, already-shipped design decisions are left implicit in their ADRs,
so a future reader (or a future contributor tempted to "fix" the gap) has no record
of *why* the omission is deliberate:

1. **Periodic scrub deliberately does not take the pool lock.** The scrub subcommands
   map to `LockPolicy::None` (`cli/src/main.rs#lock_policy`), so `braid-scrub.service`
   never acquires `/run/braid-pool.lock`: braid imposes no pool-lock exclusion between a
   monthly scrub and a pool mutator, leaving any real btrfs conflict (notably a `replace`,
   which the kernel rejects mid-scrub) to the kernel. ADR 018 documents scrub-vs-scrub
   serialization and enumerates the pool-lock-acquiring commands -- but it silently
   omits scrub from that enumeration and never states that the omission is intentional
   or why the concurrency is safe.

2. **braid ships no defrag of any kind.** No `defrag` command, no periodic defrag
   timer/service, no `autodefrag` mount option. This is deliberate (HDD media/archive
   files fragment little; a blanket `btrfs filesystem defrag` unshares reflink/snapshot
   extents and can balloon space) but no ADR records the decision.

This is a **documentation-first** task: the bulk of the work makes the two decisions
explicit in the authority docs (ADRs) and, for the defrag footgun, warns operators where
they will actually look (the troubleshooting guide). It also carries one deliberate
non-doc edit -- **Change 3**, a single regression assertion pinning the new no-`autodefrag`
invariant. That edit is required, not optional: ADR 015 (2a) asserts "no `autodefrag`" as
an invariant, and braid treats an asserted invariant as law that must be enforced, so the
clarification is incomplete without the pin. This relaxes the original brief's strict
"documentation-only" framing by exactly one test line; see the SCOPE NOTE under Change 3.

## Decisions (locked) and declined alternatives

- **Change 1 home: ADR 018 only.** ADR 018's `## Pool lock mutual exclusion` is the
  section that enumerates lock-takers and omits scrub -- the literal gap -- so the
  authoritative note lands there.
  - *Declined: mirror into ADR 026.* ADR 026's founding thesis is eliminating
    duplicated lock-command lists ("That split created two sources of truth... The
    lists drifted"); it deliberately points at `lock_policy` instead of enumerating
    commands. A scrub note there re-introduces the drift 026 exists to prevent. 026
    already cross-links 018.
  - *Declined: add a Principle 12 carve-out clause.* Principle 12 already frames lock
    disciplines as policy categories and stays abstract; scrub's exemption rests on a
    detailed kernel `exclusive_operation` argument that is ADR-altitude, and it is an
    implementation consequence rather than a promised UX invariant like the
    `status`/`doctor` diagnostic-surface guarantee. Leave the principle untouched.
- **Change 2 home: ADR 015 `## Alternatives considered` (new `###` subsection) + a new
  operator-facing troubleshooting section.** The ADR is the authority for *why*; the
  troubleshooting guide steers the operator who is about to run `btrfs filesystem
  defrag` by hand (on-mission per AGENTS.md: "run a NAS without fiddling with manpages
  or error-prone low-level commands"). The troubleshooting section links **up** to the
  ADR (authority flows up); the ADR does not link down (keeps `## See` conventional --
  code/ADRs/tests).
  - *Subsection vs. a `## Tradeoffs accepted` bullet:* use `## Alternatives considered`,
    parallel to the existing `### Default-on btrfs compression (compress=zstd:1)`.
    Periodic/automatic defrag is a feature we could have shipped (the upstream
    `btrfsmaintenance` toolbox ships one, off by default) and rejected with rationale --
    the same shape as the compression rejection, not the "accepted downside" shape of
    the TRIM/flash tradeoffs.

## Verification notes / deviations from the brief

- Re-ran `rg -i defrag --glob '!reference/**'`: hits are ADR 015 line 38 (the
  compression aside) **and** one plan file (`plans/impl/2026-05-23-btrfs-compression-omission.md`).
  Neither is an authoritative doc, so the gap stands; the brief's "only ADR 015" is
  off by one non-authoritative file.
- `btrfsmaintenance` and `autodefrag` appear nowhere in-repo (outside `reference/`).
  `btrfsmaintenance` is cited only as external precedent color (mirroring the existing
  "Fedora's `compress=zstd:1` precedent" prose), not an in-repo comparison; its upstream
  default is verified as `BTRFS_DEFRAG_PERIOD="none"` (off) in `sysconfig.btrfsmaintenance`.
- Principle 11 (HDD defaults) is a terse invariant that defers rationale to ADR 015 --
  no defrag mention, no update needed. Principle 12 already covers lock-free disciplines
  abstractly -- no update needed (see declined alternative above).
- `scripts/docs/check-output-ascii.py` scans `cli/src/**/*.rs` and `modules/**/*.nix`
  only, **not** `docs/*.md`, so these doc edits are not ASCII-gated by it. New content
  below still uses ASCII (`--`, straight quotes) per the writing-style preference.

---

## Change 1 -- ADR 018 (`docs/design/decisions/018-systemd-lifecycle.md`, status: Active)

Three edits.

### 1a. New paragraph in `## Pool lock mutual exclusion` (the authoritative note)

Insert **after** the existing single paragraph of that section (the one ending
`See [Principle 12](../principles.md#12-one-pool-operation-at-a-time).`) and **before**
the `### Lock acquisition site` subheading. Paste-ready:

> Periodic scrub is deliberately exempt. The scrub subcommands -- `scrub-cancel`,
> `scrub-needs-resume`, and `scrub-resume-or-start` -- take the `LockPolicy::None`
> discipline in `cli/src/main.rs#lock_policy`, so `braid-scrub.service` never acquires
> `/run/braid-pool.lock`. A long monthly scrub can therefore run while a pool mutator
> holds the lock. If scrub instead took the pool lock, every
> non-blocking mutator would be rejected for the scrub's entire multi-hour duration via the
> fail-fast `another braid operation is already in progress` path above -- the pool lock is
> built for short mutations that briefly exclude each other, not a multi-hour hold.
>
> Not holding the lock is safe because braid defers the real conflict check to the kernel.
> Scrub is not in btrfs' `exclusive_operation` set, so it does not hold the exclop lock and
> a `balance` can overlap a running scrub. The one documented conflict is `replace`, which
> reuses btrfs' scrub machinery: the kernel -- not braid -- refuses to start a `replace`
> while a scrub is in progress, returning "scrub is in progress" (the kernel's
> `SCRUB_INPROGRESS` result). braid classifies that on the stderr substring in
> `cli/src/pool.rs#replace_error` and turns it into a recovery hint pointing the operator at
> `btrfs scrub cancel` (and `braid status`).
> `tests/repro/btrfs-replace-rejected-during-scrub.py` pins both halves: the kernel
> rejection and the classified hint.

Citations used (all verified): `cli/src/main.rs#lock_policy` (scrub arms map to
`LockPolicy::None`; mutators map to `LockPolicy::NonBlocking`), `cli/src/pool.rs#replace_error`
(classifies on the `"scrub is in progress"` substring and emits the `btrfs scrub cancel`
hint), and the repro test. Code paths are bare code spans, not links (per
`doc-citations.md`: `cli/` lives outside the mdBook root, so linkifying 404s).

### 1b. New bullet in the scrub-unit subsection (cross-link)

In `### braid-scrub.timer + scrub service + resume trigger -- lifecycle-bound scrub`,
insert **immediately after** the existing `**Serialization via single runner.**` bullet
(which covers scrub-vs-scrub: "no `flock` and no `/run/braid-scrub.lock`") and before the
`Conflicts` + `Before` `shutdown.target` bullet. This distinguishes scrub-vs-scrub
serialization (already documented) from scrub-vs-mutator concurrency (the gap):

> - **No pool lock.** Distinct from the single-runner serialization above, the scrub
>   subcommands also take `LockPolicy::None`, so a scheduled scrub never holds
>   `/run/braid-pool.lock`: braid does not serialize it against a pool mutator, leaving
>   real btrfs conflicts to the kernel. See
>   [Pool lock mutual exclusion](#pool-lock-mutual-exclusion) for why that is safe (a
>   `balance` overlaps; a `replace` is the kernel's documented rejection case).

The same-file anchor `#pool-lock-mutual-exclusion` (slug of `## Pool lock mutual
exclusion`) is validated by `mdbook-linkcheck2`.

### 1c. New `## See` entry (discoverability)

Append to ADR 018's `## See` list (which already cites `tests/module/systemd-lifecycle.py`):

> - `tests/repro/btrfs-replace-rejected-during-scrub.py` -- kernel rejects a conflicting
>   mutator during scrub; recovery hint classified

`check-see-paths.py` parsing: bullet starts with `- `; the code-span path precedes the
` -- ` separator (a recognized `DESC_SEPARATOR`); the script checks only that
`tests/repro/btrfs-replace-rejected-during-scrub.py` exists (it does). Passes. (The
inline citation in 1a is the load-bearing source; this See entry is for discoverability,
matching the existing test-in-See pattern.)

---

## Change 2 -- ADR 015 + troubleshooting

### 2a. New subsection in ADR 015 (`docs/design/decisions/015-hdd-defaults.md`, status: Active)

Insert under `## Alternatives considered`, **after** the existing
`### Default-on btrfs compression (compress=zstd:1)` subsection and **before** `## See`.
Paste-ready:

> ### Periodic or automatic defrag
>
> Rejected. braid ships no defrag of any kind: no `defrag` command, no periodic defrag
> timer or service, and no `autodefrag` mount option (`cli/src/cmd.rs#base_mount_options`
> sets only `noatime`, `skip_balance`, and `subvolid=5`). The target workload is an HDD
> media/archive NAS where large, mostly-sequential files fragment little, so a scheduled
> defrag buys little; the upstream `btrfsmaintenance` toolbox ships its defrag job off by
> default for the same reason.
>
> More decisively, a blanket `btrfs filesystem defrag` unshares reflink and snapshot
> extents -- it rewrites shared extents into private copies. On a snapshot-capable pool,
> that unsharing can sharply increase real space usage, turning a routine maintenance
> pass into an ENOSPC incident. An automatic or periodic defrag would therefore be
> actively harmful,
> not merely unnecessary. An operator who hits a genuine fragmentation problem can still
> defrag a specific path by hand, accepting the one-time unsharing cost for just that
> path.

`cli/src/cmd.rs#base_mount_options` is already cited in ADR 015's `## See`; the inline
code-span citation here is consistent and needs no See change. No `## See` edit for
Change 2 (authority flows up from troubleshooting; the ADR does not link down).

### 2b. New section in `docs/guides/troubleshooting.md` (operator-facing warning)

Add a new symptom section. Place it **before** the final `## Related` section (advisory,
not an acute failure, so it does not disrupt the front-loaded failure-recovery ordering).
Matches the file's `**Symptom:** / **Fix:**`-style bold lead-ins and `sh` code blocks.
Paste-ready (note the inner ```sh fence):

````
## Pool is fragmented

**Symptom:** `filefrag` reports many extents on large files, and you're tempted to run
`btrfs filesystem defrag` to compact them.

**Don't run a blanket defrag.** `btrfs filesystem defrag` unshares reflink and snapshot
extents: it rewrites shared extents into private copies. On a pool that holds snapshots
or reflinked copies, that can sharply increase real space usage and push the filesystem
into ENOSPC. Recovering then means freeing space -- for example deleting snapshots or
reflinked copies you no longer need -- not a balance, because the space is now held by
private extents rather than reclaimable empty block groups. braid ships no automatic or
periodic defrag for exactly this reason.

**If a specific file is genuinely fragmented** and measurably hurts performance, defrag
just that path and accept the one-time unsharing cost for it:

```sh
sudo btrfs filesystem defrag /mnt/storage/path/to/fragmented-file
```

Large, mostly-sequential media and archive files -- braid's target workload -- fragment
little, so this is rarely needed. See
[ADR 015: HDD defaults](../design/decisions/015-hdd-defaults.md#periodic-or-automatic-defrag)
for the full rationale.
````

Anchor (validated by `mdbook-linkcheck2`):
- `../design/decisions/015-hdd-defaults.md#periodic-or-automatic-defrag` -- relative
  path from `docs/guides/` (`../` -> `docs/`, then `design/decisions/...`); the anchor is
  the slug of the new `### Periodic or automatic defrag` heading from 2a. **2a must land
  for this link to resolve.** No cross-link to the balance ENOSPC section: that recipe
  reclaims empty block groups for balance temp-space and is not the recovery for real
  space consumed by unshared extents.

---

## Change 3 -- pin the no-autodefrag invariant (test)

> **SCOPE NOTE.** This is the one edit outside `docs/`, and it is **required**, not
> optional. The original brief said documentation-only, but ADR 015 (2a) now asserts "no
> `autodefrag` mount option" as an *invariant*, and braid treats invariants as law that
> must be enforced -- an ADR that asserts an invariant with no regression pin is exactly the
> paper invariant the project's authority rules forbid. `tests/cli/braid-unlock.py` already
> pins the sibling mount-option invariants (`skip_balance`, `subvolid=5`, no `compress`) but
> would still pass if `base_mount_options()` gained `autodefrag`, leaving the new ADR claim
> untested. The fix is a one-line, behavioral, structure-insensitive extension of the
> existing assertion. This is a deliberate, surfaced deviation from the literal brief by one
> test line; if the user insists on strictly zero non-doc edits, the only correct
> alternative is to also drop the "no `autodefrag`" claim from 2a -- never ship an
> unenforced invariant -- not to keep the claim and skip the pin.

Edit `tests/cli/braid-unlock.py`, in the existing mount-options assertion block (the
`opts = machine.succeed("findmnt -o OPTIONS -n /mnt/storage")` block that already asserts
`skip_balance`, `subvolid=5`, and no `compress`). Name the new invariant in the comment and
add one assertion mirroring the existing `compress` check:

```python
    # Mount options pinned by ADR 015: skip_balance, subvolid=5, no compression, no autodefrag
    opts = machine.succeed("findmnt -o OPTIONS -n /mnt/storage").strip()
    assert "skip_balance" in opts, f"Expected skip_balance in mount options, got: {opts}"
    assert "subvolid=5" in opts, f"Expected subvolid=5 in mount options, got: {opts}"
    assert "compress" not in opts, (
        f"Expected no compression option in mount options "
        f"(see ADR 015), got: {opts}"
    )
    assert "autodefrag" not in opts, (
        f"Expected no autodefrag option in mount options "
        f"(ADR 015: braid ships no defrag), got: {opts}"
    )
```

`findmnt -o OPTIONS` reports the effective btrfs mount options -- the same surface the
existing `compress`/`subvolid=5` checks already rely on -- so the new assertion is
behavioral and representation-independent. No new test preamble is needed: this extends an
assertion inside an existing subtest, and the test already documents the mount-option pin.

## Conventions & enforcement honored

- **File citations:** code as bare code spans (`path#symbol`), docs as markdown links
  (`path#heading-slug`); no line numbers (`doc-citations.md`).
- **`## See` rules** (`check-see-paths.py`): the one new See entry (1c) is a code span
  whose file exists, before a ` -- ` separator. Both target ADRs are `status: Active`, so
  editing their bodies and See sections is allowed (the frozen-doc rule does not apply).
- **No `SUMMARY.md` change:** no new files are added; both ADRs and `troubleshooting.md`
  are already registered. Internal edits to registered files need no TOC update.
- **ASCII** (`--`, straight quotes) throughout new content.

## Verification

Run from the repo root after editing:

- `just docs-build` -- builds the mdBook and runs `mdbook-linkcheck2`, the CI cross-link
  gate. Confirms every new anchor resolves: the 1b same-file anchor
  `#pool-lock-mutual-exclusion`, and the 2b link to
  `015-hdd-defaults.md#periodic-or-automatic-defrag`. (Uses `nix develop .#docs`.)
- `just check-docs-see-paths` (`python3 scripts/docs/check-see-paths.py`) -- confirms the
  new 1c See path resolves.
- `just check-docs` -- SUMMARY parity / table check; expected clean (no new files), run as
  a backstop.
- `just test-vm braid-unlock` -- runs the NixOS VM test whose mount-option assertion now
  also fails on `autodefrag` (Change 3). Heavier VM lane; runs on macOS via
  `nix.linux-builder` (checks are `aarch64-darwin`), per the testing conventions.

Quick spot-checks (no nix needed):
- `rg -n "Periodic or automatic defrag" docs/design/decisions/015-hdd-defaults.md`
- `rg -n "Pool is fragmented" docs/guides/troubleshooting.md`
- `rg -n "deliberately exempt|No pool lock" docs/design/decisions/018-systemd-lifecycle.md`
- Re-confirm no stray changes: `git diff --stat` should touch exactly four files -- the
  three doc files (`docs/design/decisions/018-systemd-lifecycle.md`,
  `docs/design/decisions/015-hdd-defaults.md`, `docs/guides/troubleshooting.md`) plus the
  Change 3 test (`tests/cli/braid-unlock.py`) -- and nothing under `cli/src/**` or
  `modules/**`.

## Out of scope / non-goals

- No behavior or `cli/src`/`modules` source changes: no change to scrub locking behavior;
  no defrag feature, command, timer, service, or mount option. The single non-doc edit is
  the required regression assertion in Change 3 (a pin for an existing invariant, not a
  behavior change) -- a deliberate, surfaced one-line relaxation of the original brief's
  "documentation-only" framing.
- No edit to ADR 026, Principle 11, or Principle 12 (see declined alternatives).
- No `SUMMARY.md`, `README.md`, or `principles.md` changes.
