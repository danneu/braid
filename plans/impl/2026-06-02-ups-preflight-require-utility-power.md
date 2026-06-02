# Fix: UPS preflight must require verified utility power

## Context

`docs/design/decisions/020-ups-integration.md:71` currently documents the UPS
preflight as refusing only when the status set contains `OB` or `LB`. A prior
review showed that the implementation is already stricter than that:
`check_ups_not_on_battery` refuses on `upsc` query/invocation failure, empty
`ups.status`, critical flags (`LB`, `TESTFAIL`, `COMMBAD`, `FSD`), and `OB`
(`cli/src/preflight.rs:578-611`).

That stricter behavior is directionally right, but it is still not the ideal
safety contract. The function currently returns `Ok(())` for any non-empty
status set that has no known blocker. That means a status set with no `OL`
can still pass if it is not `OB` and not classified critical.

For pool-mutating commands, braid should not merely avoid known-bad UPS states;
it should verify that utility power is present at start, narrowing the
avoidable case of beginning a mutation while already on battery. NUT's own
reference material supports using `OL` as that line-power proof:

- `reference/nut/docs/new-drivers.txt:301-317` says clients must accept
  space-separated `ups.status` tokens and that `OL` / `OB` indicate input-line
  status.
- `reference/nut/docs/man/genericups.txt:107` defines `OL` as "On line (no
  power failure)" and the opposite of `OB`.
- `reference/nut/docs/man/failover.txt:206-214` treats fresh data in status
  `OL` as online, while fresh data without that proof "may not be fully
  online."

So the ideal contract is:

When `braid.ups.enable = true`, `braid add`, `braid remove`,
`braid remove-missing`, and `braid replace` refuse unless `upsc` successfully
returns a non-empty status set that explicitly contains `OL` and contains no
known blockers. The command passes for `OL` alone, for known non-critical
advisory combinations such as `OL RB`, and for unknown tokens co-present with
`OL` and no known blocker; it refuses for query failure, empty/missing
`ups.status`, `OB`, critical flags, or any status set without `OL`.

This is a production behavior change, not just a documentation cleanup. The ADR
and guide must describe the new stricter contract, and the unit tests must
prove it.

## Scope (verified)

A tracked-file sweep finds the stale "OB or LB"-only framing of the preflight
contract in three places:

- `docs/design/decisions/020-ups-integration.md:71` (primary contract
  paragraph).
- `tests/module/ups-preflight-on-battery.nix:40` (VM-test comment with
  "refuses on OB *or* LB").
- `cli/src/parse/upsc.rs:158` (parser-test preamble that justifies recognizing
  `OL` by reference to "status set contains OB or LB").

A second sweep on the weaker "on battery" / "while ... battery" framing -- which
the literal `OB or LB` grep does not catch -- finds three more statements of the
same preflight contract. They must be reconciled to the stricter wording, or the
ADR and guide contradict their own (rewritten) contract paragraphs:

- `docs/design/decisions/020-ups-integration.md:22` (Context guarantee #2:
  "Preflight refusal to start pool-mutating commands while already on battery").
- `docs/design/decisions/020-ups-integration.md:143` (Consequences bullet:
  "pool-mutating commands refuse to start while on battery").
- `docs/guides/ups.md:15-18` (top "Preflight refusal on battery" feature bullet:
  "refuse to begin a pool mutation while the UPS is on battery or reporting low
  battery").

One adjacent line is a judgment call, not a contract restatement:
`020-ups-integration.md:17` ("reject avoidable starts on battery up front")
describes the Context section's control model, not the enumerated contract.
Leave it unless the stricter `OL` framing reads more clearly there. The upsmon
shutdown-trigger mentions (`OB` + `LB`) and OB-state / TUI-yellow descriptions
are about a different surface and stay unchanged (see "Deliberately unchanged").

The current behavior also appears in:

- `cli/src/preflight.rs:559-577` doc comment, which says the safety decision
  needs a "non-critical status set" rather than verified utility power.
- `cli/src/preflight.rs:600-610`, where the final `Ok(())` follows only the
  empty / critical / `OB` blocklist.
- `docs/guides/ups.md:120-140`, which describes refusal on red/yellow states
  and query failure, but not missing `OL`.
- `README.md:47`, which says mutations refuse "while on battery" and should be
  tightened to "unless utility power is verified."
- `docs/commands/{add,remove,remove-missing,replace}.md`, whose safety-check
  lists do not mention UPS preflight; since this plan changes a user-facing
  refusal case, add a concise UPS-enabled refusal bullet to each.
- `tests/module/lib/ups-fixture.nix:21-22,69,77`, whose default `ups.status:
  OL` is already the end-to-end OL pass-path precondition for the
  `ups-lb-during-*` recovery matrix, but the comment does not yet tie that
  default to the mutating-command preflight requirement.

Generated `docs/book/` output is ignored; do not edit it directly.

## Changes

### 1. Production behavior: require explicit `OL`

Update `check_ups_not_on_battery` in `cli/src/preflight.rs` so the pass path is
an allowlist around verified utility power:

1. No configured UPS: unchanged no-op.
2. `upsc` invocation failure: refuse.
3. `upsc` query exits non-zero: refuse.
4. Empty or missing `ups.status`: refuse.
5. Critical flag present (`LB`, `TESTFAIL`, `COMMBAD`, `FSD`): refuse.
6. `OB` present: refuse.
7. `OL` absent: refuse.
8. Otherwise pass.

Use explicit refusal wording for the new branches, for example:

- `UPS does not report utility power (OL missing)`

Keep the existing top-level wording:

> cannot verify UPS is on utility power (...) -- refusing to start <op>. Check
> 'braid ups status', restore utility power, then retry.

Rationale:

- `OL` is the authoritative line-power proof for this preflight. A non-empty
  status set without `OL` is not enough.
- `OL RB` should still pass: `RB` is an advisory battery-replacement flag, not
  evidence that input power is absent. This plan is about start-time utility
  power, not a full battery-health gate.
- Unknown status tokens should not block preflight when `OL` is present and no
  known blocker is present. NUT explicitly permits clients to ignore
  unidentified tokens (`reference/nut/docs/new-drivers.txt:301-303`), and
  failing closed on every new advisory token would create avoidable maintenance
  lockouts on routine NUT/device changes. The parser still preserves unknown
  tokens so `braid ups status` and `--json` expose them for operator
  inspection.

Do not fold the new `OL` requirement into
`UpsStatusFlag::is_critical`: the TUI's "critical/red" classification and the
mutating-command "safe to start" policy are related but not identical. Keep the
extra preflight gate local to `check_ups_not_on_battery`, or add a narrowly
named helper such as `reports_utility_power` if the implementation reads
cleaner. Any new `pub` / `pub(crate)` Rust item must get a short `///` doc
comment per AGENTS.md.

### 2. Unit tests for the new contract

Extend the `check_ups_not_on_battery` unit tests in `cli/src/preflight.rs`.

Keep the existing tests for:

- no configured UPS no-op
- `OL` passes
- `OB` refuses
- `LB`, `TESTFAIL`, `COMMBAD`, `FSD` refuse
- query failure refuses
- empty status refuses
- invocation failure refuses

Add these behavioral tests:

1. `OL RB` passes.
   - Intent: known advisory statuses do not block when utility power is
     explicitly present.
   - Revert proof: fails if the implementation changes to require exactly
     `{OL}` or treats `RB` as a blocker.

2. Non-empty status without `OL` refuses, for example `ups.status: RB`.
   - Intent: preflight requires affirmative utility-power evidence, not just
     absence of `OB`.
   - Revert proof: fails under today's final-`Ok(())` blocklist.

3. Unknown status token with `OL` passes, for example `ups.status: OL NEWFLAG`.
   - Intent: new NUT/device advisory tokens do not block mutations when utility
     power is explicitly present and no known blocker is present.
   - Revert proof: fails if the implementation accidentally treats unknown
     tokens as blockers.

The existing `parse_upsc` unknown-token test should remain: the parser must
continue preserving unknown tokens so `braid ups status` can show them.

No new VM test is required for the new `OL` gate:

- The existing `ups-preflight-on-battery` VM test proves the end-to-end
  `config.ups` plumbing, wrapper `upsc` dispatch, and NUT integration for the
  refusal path.
- The committed online parser fixture
  `cli/tests/fixtures/nixos-26.05/upsc/upsc-online.txt` contains `ups.status:
  OL`, proving the stable captured online state parses to `OL`.
- The existing `ups-lb-during-*` recovery matrix already exercises the OL
  pass-path end to end: the shared fixture boots with `ups.status: OL` and the
  tests start their mutating command before flipping the UPS to `OB LB`.

The implementation should add a one-line comment near
`tests/module/lib/ups-fixture.nix`'s default `ups.status: OL` noting that the
default is load-bearing for the mutating-command preflight pass path.

### 3. `docs/design/decisions/020-ups-integration.md`

Update the section header:

- From: `### Reject pool-mutating commands on battery (preflight hygiene only)`
- To: `### Reject pool-mutating commands unless UPS reports utility power (preflight hygiene only)`

Replace the contract paragraph with the stricter behavior:

> When `braid.ups.enable = true`, `braid add`, `braid remove`,
> `braid remove-missing`, and `braid replace` query UPS status at preflight and
> refuse with a `Validation`-shaped error unless the UPS status can be trusted
> as explicitly on utility power. The check is fail-closed: it refuses on
> `upsc` invocation or query failure (dead upsd, unknown UPS name, or exec
> failure), an empty or missing `ups.status`, any critical flag (`LB`,
> `TESTFAIL`, `COMMBAD`, `FSD` -- the same set the TUI paints red), on-battery
> (`OB`), or any status set missing `OL`. Known non-critical advisory states
> such as `OL RB`, and unknown tokens co-present with `OL` and no known blocker,
> still pass because utility power is explicitly present. The check sits
> alongside the existing preflight
> checks, before any journal write.

Reconcile the two other statements of the same contract in this ADR so it does
not contradict the rewritten paragraph two sections away:

- Context guarantee #2 (currently "Preflight refusal to start pool-mutating
  commands while already on battery") -> "unless the UPS reports verified
  utility power (`OL`)" framing.
- The Consequences bullet (currently "pool-mutating commands refuse to start
  while on battery, narrowing the journal-recovery surface to the mid-mutation
  case") -> the same "unless utility power (`OL`) is verified" framing; keep the
  journal-recovery-surface clause.

Leave the Context control-model sentence ("reject avoidable starts on battery up
front, and prove journal recovery for the unavoidable mid-mutation case") unless
the `OL` framing reads more clearly there -- it states intent, not the
enumerated contract. Leave guarantee #3's upsmon shutdown description (`OB` +
`LB`) unchanged: that is the shutdown trigger, not the start preflight.

Rationale for the wording:

- Architecture docs should state the behavioral contract, not the helper names.
- The contract is now an allowlist around verified `OL`, not merely the old
  `OB`/`LB` shorthand and not merely today's blocklist.
- This is a deliberate tightening of the `Active` ADR's mutating-command
  preflight contract, grounded in NUT's `OL` line-power semantics. It does not
  reinterpret the parser policy: unknown tokens are still preserved and exposed
  for display/JSON, but they are not preflight blockers when `OL` is present
  and no known blocker is present.
- The text distinguishes preflight start safety from upsmon shutdown safety:
  upsmon's shutdown trigger remains normally `OB` + `LB`, while braid's start
  preflight is stricter.
- The `OL` gate proves utility power at the start of the mutation. It does not
  prove the UPS can carry a later shutdown window; states such as `OL BYPASS`
  may still indicate reduced battery protection. Treating `BYPASS` / `OFF` as
  blockers is a possible future policy extension, not part of this plan.

Verified linkcheck-safe: no tracked source markdown links the old header
anchor. Generated `docs/book/` matches are ignored.

### 4. `docs/guides/ups.md`

Two places state the preflight contract -- a top feature bullet and a lower
detail section -- and both must be updated so they agree.

First, the top "Preflight refusal on battery" feature bullet (in the
three-behavior list near the top of the guide) currently says mutations refuse
"while the UPS is on battery or reporting low battery." Rewrite it to the
stricter framing -- refuse unless the UPS reports verified utility power
(`OL`) -- and rename the bold label away from "on battery" (for example
"Preflight refusal without verified utility power"), so the intro does not
contradict the detail section below.

Second, update the lower "Mutation refusal on battery" detail section so it no
longer names only red/yellow states and query failure.

Rename that section heading to "Mutation refusal when utility power is not
verified" or equivalent.

State that with UPS enabled, pool mutations refuse unless `upsc` returns a
non-empty status set with `OL` and without known blockers. Name the blockers:

- `OB`
- `LB`, `TESTFAIL`, `COMMBAD`, `FSD`
- missing `OL`
- query/invocation failure
- empty or missing `ups.status`

Keep the existing recovery guidance: run `braid ups status`, fix the UPS/NUT
state, restore utility power, wait for a trusted `OL` status, and retry.

Add two clarifying notes:

- `doctor`'s `ups_daemon: ok` means the configured NUT daemon is reachable; it
  is not a guarantee that mutating-command preflight will pass. The refusal
  error from `add` / `remove` / `remove-missing` / `replace` is the primary
  channel for the exact mutation-readiness blocker.
- The `OL` gate assumes the configured NUT driver reports `OL` on utility
  power as documented by NUT. If a device or driver violates that contract,
  inspect with `braid ups status`; the recovery is to fix the NUT driver/config
  or disable `braid.ups` until the UPS state can be trusted.

### 5. README and command docs

Update `README.md:47` from "mutating commands refuse to start while on battery"
to a brief version of the stricter contract, for example:

> mutating commands refuse unless UPS utility power is verified

Add a concise UPS-enabled bullet to the safety-check lists in:

- `docs/commands/add.md`
- `docs/commands/remove.md`
- `docs/commands/remove-missing.md`
- `docs/commands/replace.md`

Suggested wording:

> Refuses when UPS support is enabled and `braid ups status` cannot verify a
> trusted `OL` (utility-power) state.

This keeps the command references in sync without repeating the full UPS guide.

### 6. Source comments and parser-test preamble

Update the stale source comments found by the tracked-file sweep:

1. `tests/module/ups-preflight-on-battery.nix:40`

   From:

   > # preflight refuses on OB *or* LB (see check_ups_not_on_battery),
   > # so OB alone is sufficient to exercise the refusal path.

   To:

   > # preflight refuses on OB before any mutation work starts; OB alone is
   > # sufficient to exercise the on-battery refusal path.

2. `cli/src/parse/upsc.rs:158`

   From:

   > // Why: preflight treats "status set contains OB or LB" as refuse; "status
   > // set equals {OL}" must therefore be recognized, not treated as unknown.

   To:

   > // Why: TUI severity and UPS preflight both key on the typed Ol flag:
   > // the TUI renders Ol green, and preflight requires Ol before starting
   > // pool mutations.

Also update the `check_ups_not_on_battery` doc comment in
`cli/src/preflight.rs:559-577` so it says the safety decision needs explicit
utility-power proof, not just a "non-critical status set." Narrow the edit to
that clause: leave the "Critical-state classification is shared with the TUI via
`UpsStatusFlag::is_critical`" paragraph intact -- the `OL` gate does not change
which tokens count as critical, so that sentence (and the matching note in
`tui/view/mod.rs#ups_severity_color`) stays accurate.

## Deliberately unchanged

- The NUT/upsmon shutdown path remains unchanged. `OB` + `LB` is still the
  normal upsmon critical shutdown trigger; this plan changes only braid's
  mutating-command start preflight.
- TUI severity coloring remains unchanged. `OL` is green, `OB` is yellow, the
  existing critical set is red, and unknown/empty states remain DarkGray.
- `braid ups status` human and JSON rendering remain unchanged. Unknown tokens
  should still be displayed/serialized via `as_token()` so operators can see
  exactly what the UPS reported.
- `braid doctor`'s UPS daemon check remains a reachability check unless the
  implementation deliberately expands it. If it stays unchanged, the
  `braid-doctor-ups` VM test should remain green.
- No new command-line flags or override escape hatches. If the UPS status
  cannot verify utility power, the operator fixes UPS/NUT state and retries.

## Verification

1. `just test-rust`
   - Proves the new preflight unit tests.
   - Re-runs parser tests touched by the `upsc.rs` preamble edit.
2. `nix develop .#docs -c mdbook build docs`
   - Proves the ADR/guide/command-doc links and renamed anchors.
3. Manual tracked-file sweeps -- two passes, because the literal `OB or LB`
   grep does not catch the "on battery" framing of the same contract:

   a. Tight pass -- the stale literal framing. Expected after implementation:
      no matches.

      ```sh
      git ls-files -z | xargs -0 rg -n 'OB \*or\* LB|OB or LB|status set contains `OB` or `LB`|status set contains OB or LB'
      ```

   b. Broad pass -- the weaker "on battery" contract phrasing. This is a
      classify-each review sweep, not an expected-empty check; it surfaces
      legitimate hits too.

      ```sh
      git ls-files -z | xargs -0 rg -n 'on battery|on-battery|while .*battery'
      ```

      No remaining hit may state the *preflight start contract* in the weaker
      "on battery"-only framing (in particular, after implementation none should
      survive in `020-ups-integration.md` guarantee #2 / Consequences, or the
      `ups.md` top feature bullet). These categories are expected to remain and
      must be left intact:

      - OB-state descriptions and the TUI yellow legend ("OB (on battery, not
        yet critical)").
      - upsmon critical-shutdown mentions (`OB` + `LB`, `ups.status: OB LB`).
      - the recovery-path framing for mid-mutation power loss.
      - the ADR Context control-model line ("reject avoidable starts on battery
        up front"), unless the stricter framing was adopted there as a judgment
        call.

No VM run is required unless the implementation changes command plumbing beyond
`check_ups_not_on_battery`; the new behavior is pure status classification.

## Implementation notes

- Implemented the `OL` gate via a new `reports_utility_power()` method on
  `UpscOutput` (`cli/src/parse/types.rs`), the helper the plan named as the
  cleaner option, rather than inlining the membership check in
  `check_ups_not_on_battery`. It sits beside `is_on_battery` / `is_critical`
  and carries the required `///` doc comment.
- Reconciled a fourth live statement of the preflight start contract that the
  plan's broad-sweep scope did not list: `docs/guides/nixos-configuration.md`
  (the `### UPS` option-reference paragraph) said pool mutations "refuse to
  start while the UPS is on battery." Rewrote it to the stricter "unless the
  UPS reports verified utility power (`OL`)" framing, matching the
  README/ADR/guide edits, so the mdBook-linked option reference does not
  contradict the rewritten contract and the broad-sweep acceptance criterion
  holds. Historical `plans/impl/*` records quoting the old wording were left
  unchanged -- they are point-in-time records, not live contract docs.
