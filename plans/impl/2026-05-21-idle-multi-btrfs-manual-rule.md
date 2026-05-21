# Pin the conservative multi-btrfs rule in `braid idle` manual

## Context

`braid idle` scans `/sys/fs/btrfs/*` and reports Busy if any btrfs
filesystem on the host has an in-flight exclusive op -- not just the
pool's fsid. This is intentional: autosuspend errs conservative, so
"do not suspend while any btrfs is mid-balance/replace/etc." is the
right answer regardless of which fs is busy. The semantic is
documented in `docs/decisions/016-auto-suspend.md:55`.

`idle_any_busy_blocks_suspend_multi_btrfs` (`cli/src/idle.rs:602`)
seeds two fsids and asserts Busy, but as written the busy state is on
`IDLE_FSID` -- which `cli/src/test_fixtures/idle.rs:15` documents as
"Canonical fsid ... that model[s] the pool filesystem." A future
pool-scoped implementation that read only the pool's fsid would still
see `balance` on `IDLE_FSID` and return Busy, passing the test while
silently invalidating the host-wide rule the new manual text will
claim. The test needs to be sharpened so the pool fsid is idle and the
non-pool fsid is the one carrying the busy state.

The user manual page (`manual/commands/idle.md`, "What happens under
the hood" section, lines 84-91) describes the flow as "any active
exclusive operation on any btrfs filesystem" but does not call out the
user-facing implication: on a host with btrfs root alongside the pool,
an exclusive op on root keeps the NAS awake while the pool is mounted,
and the `busy:` output may name an op on the non-pool fs. A user
troubleshooting "why didn't my NAS suspend last night" cannot
reproduce this behavior from the manual.

The intended outcome is a one-paragraph addition to the manual that
pins the rule and its output implication, plus a sharpening of the
regression test so the manual claim has real coverage.

## Change

Two coupled edits:

1. **Sharpen the regression test** in `cli/src/idle.rs` so the busy
   state is on the non-pool fsid (`IDLE_FSID_OTHER`) and the pool
   fsid (`IDLE_FSID`) is idle. This gives the new manual claim
   non-trivial regression coverage: a future "scope to pool fsid"
   refactor would fail the assertion.

2. **Add the user-facing note** to `manual/commands/idle.md` with a
   rendered-safe ADR link.

### Edit 1: `cli/src/idle.rs`, the `idle_any_busy_blocks_suspend_multi_btrfs` test

Swap the busy/idle assignment and reorder the listing so the test
still exercises iterator non-short-circuit on the first entry while
*also* pinning that the busy fsid is the non-pool one. Update the
inline comment to reflect the dual invariant.

Target body (preserving listing-order property AND pinning
non-pool-busy):

```rust
#[test]
fn idle_any_busy_blocks_suspend_multi_btrfs() {
    let runner = idle_runner_with_scrub_finished();
    // Pool fsid (IDLE_FSID) is idle; non-pool fsid (IDLE_FSID_OTHER)
    // is balancing. List order puts the pool first so the loop must
    // continue past it to find Busy on the second entry. A future
    // change that scoped reads to only the pool fsid would read
    // IDLE_FSID, see `none`, and return Idle -- failing this test.
    let fs = IdleMockFs::mounted_btrfs_only()
        .seed_btrfs_listing(&[IDLE_FSID, IDLE_FSID_OTHER])
        .seed_exclop(IDLE_FSID, "none")
        .seed_exclop(IDLE_FSID_OTHER, "balance");

    let result = cmd_idle(&runner, &fs, &idle_mp());
    assert_eq!(
        result,
        IdleResult::Busy(BusyReason::Exclop(ExclusiveOp::Balance))
    );
}
```

Also update the test's `Intent` / `Why` / `Scenario` preamble so the
"pool idle, non-pool busy" framing is explicit in the doc-style
comment, matching project test convention (`AGENTS.md` "Test
Conventions").

### Edit 2: `manual/commands/idle.md`

Insert a short note paragraph **after step 5 of the "What happens
under the hood" list (currently line 90)**, **before the "Related
commands" heading (currently line 92)**.

Proposed text:

> When the host has more than one btrfs filesystem (e.g. a btrfs root
> in addition to the pool), an exclusive op on any of them keeps the
> system awake while the pool is mounted, and the `busy:` line above
> may name an op on the non-pool fs. This is intentionally
> conservative -- see [ADR 016: Auto-Suspend](https://github.com/danneu/braid/blob/master/docs/decisions/016-auto-suspend.md).

### Placement rationale (manual edit)

The numbered list is the mechanical flow ("what runs when"); the
multi-btrfs rule is a behavioral note that explains a user-facing
implication. The ADR formats it the same way (a separate paragraph
labeled "Semantics:" following the mechanical description). Keeping
it as a paragraph after the list -- not a sub-bullet inside step 3 --
matches the ADR shape and avoids overloading the cookbook-style
numbered flow.

### Wording rationale (manual edit)

- "e.g. a btrfs root in addition to the pool" -- gives the concrete
  scenario from the test's preamble so the reader recognizes their
  own setup.
- "while the pool is mounted" -- step 2 short-circuits to idle when
  unmounted; the multi-btrfs rule only applies in the mounted path.
- "the `busy:` line above may name an op on the non-pool fs" -- the
  ADR's `BusyReason` caveat in user-facing terms. Without this a
  reader sees `busy: balance running` and assumes it's the pool.
- "intentionally conservative" + ADR link -- signals this is a
  design choice, not a bug, and routes deeper questions to the
  canonical source.
- ASCII `--` per `AGENTS.md` ("CLI Output Style") and project global
  instructions.

### Link form rationale (manual edit)

Use an **absolute GitHub URL** (`https://github.com/danneu/braid/blob/master/docs/decisions/016-auto-suspend.md`),
not the source-relative `../../docs/decisions/...` form that
`manual/guides/ups.md:171` uses.

Why: mdBook's source tree is `manual/` (`book.toml: src = "."`) and
`SUMMARY.md` does not include `docs/decisions/*`. The rendered output
at `manual/book/` therefore has no `docs/` subtree. A
`../../docs/decisions/016-auto-suspend.md` link from
`manual/commands/idle.md` works in GitHub source view (resolves to
the ADR at the repo root) but renders to `../../docs/decisions/016-auto-suspend.html`
in `manual/book/commands/idle.html`, pointing at
`manual/docs/decisions/...` which does not exist -- and from a
deployed `/braid/commands/idle.html` it resolves above the site root.
An absolute GitHub URL is the only form that works in both source
view and the rendered/deployed manual without expanding mdBook's
scope to ship ADRs as book pages.

The existing `manual/guides/ups.md:171` link has the same
rendered-broken bug. Out of scope for this plan; flagged as a
separate cleanup below.

## Files Changed

- `cli/src/idle.rs` -- swap busy/idle assignment in
  `idle_any_busy_blocks_suspend_multi_btrfs` (lines 601-617) and
  refresh its preamble comment to make the "pool idle, non-pool busy"
  framing explicit.
- `manual/commands/idle.md` -- one paragraph addition in the "What
  happens under the hood" section, with an absolute GitHub ADR link.

## Files Reviewed but Not Changed

- `manual/guides/power-management.md:21` -- table-row summary using
  `<fsid>` in the sysfs path. Adding the multi-btrfs caveat here
  would bloat the table; the command page is the right home for the
  detail. The `<fsid>` notation already hints at the host-wide scan.
- `manual/guides/nixos-configuration.md:131` -- bullet-list summary
  of activity checks. Same reasoning as above.
- `docs/decisions/016-auto-suspend.md:55` -- already canonical and
  explicit. No change.
- `manual/guides/ups.md:171` -- existing ADR link with the same
  rendered-broken bug we're avoiding in `idle.md`. Out of scope here;
  fixing it would be a separate consistency pass and the reviewer's
  finding only asked for a rendered-safe form in the new doc.
- `manual/SUMMARY.md` / `manual/book.toml` -- ADR inclusion in
  mdBook would dissolve the link-form question entirely, but is a
  much larger architectural change and out of scope for this plan.

## Verification

1. **Test edit.** Run `just test-rust` (or
   `cargo test --manifest-path cli/Cargo.toml --lib idle::tests::idle_any_busy_blocks_suspend_multi_btrfs`).
   Test must still pass with the swapped busy/idle assignment. To
   confirm the test actually pins the new property, temporarily
   stub the implementation to early-return after reading
   `IDLE_FSID`'s exclop, re-run, and confirm the test now fails.
   Revert the stub before continuing.
2. **Manual edit, source view.** Re-read the edited "What happens
   under the hood" section in `manual/commands/idle.md` end-to-end.
   Confirm the new paragraph reads naturally between step 5 and the
   "Related commands" heading.
3. **Manual edit, rendered view.** Run `just docs` (the
   `nix run nixpkgs#mdbook -- serve manual --open` recipe at
   `justfile:210`). In the rendered book, navigate to the idle
   command page, click the ADR link, and confirm it resolves to the
   ADR on GitHub. (A relative-path link would 404 here -- that's the
   regression mode this verification step exists to catch.) The
   sibling recipe `just check-docs` (`justfile:214`) verifies
   `SUMMARY.md` / disk parity but does not catch broken external
   links; do not substitute it for the rendered-view click-through.
4. **Manual edit, source-link sanity.** Confirm the GitHub URL
   resolves by opening it (or `curl -I` for HTTP 200); the repo is
   private per `AGENTS.md` so the check requires an authenticated
   session.

No new tests added beyond sharpening the existing one. With the
swap, `idle_any_busy_blocks_suspend_multi_btrfs` is the regression
gate for the new manual claim.
