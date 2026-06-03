# Fix the `disks[].mapper` JSON field contract (docs + pin)

## Context

`docs/commands/status.md`'s `mapper` bullet tells monitoring authors the
JSON `disks[].mapper` field is the **observed** device-mapper name and to
"do not reconstruct it as `braid-${name}`". That is a blanket claim, but
`build_disk_reports` (`cli/src/status.rs#build_disk_reports`) reconstructs
the expected `braid-<name>` for **every** non-present row -- all five of
`missing`, `offline`, `unknown`, `luks-header-unreadable`, and
`luks-uuid-mismatch` flow through the single line
`let mapper = mapper_name(&cd.name).0;` (`cli/src/status.rs:1113`;
`mapper_name` is `cli/src/config.rs#mapper_name` -> `braid-<name>`).

The reconstructed value is **not wrong**: decision 024
(`docs/design/decisions/024-luks-uuid-identity.md`, point 5) explicitly
sanctions constructing `mapper_name(&member.name)` to address braid's
expected mapper, and the mapper is display/runtime only, never a membership
decision. Non-present rows deliberately carry *configured/expected* handles
(`name`, `by_id` = configured `cd.by_id_path`, `mapper` = `braid-<name>`)
while nulling *live-observed* state (`underlying`, `devid`, `errors`, and
`luks_uuid` -- except `luks-uuid-mismatch`, which surfaces the observed on-disk
UUID, `cli/src/status.rs:1079`). The mapper is consistent with that model.
**The doc is the defect** -- it states a present-only guarantee as if it held
for all rows.

Root cause: the non-present mapper contract was never pinned by a test, so
doc and code drifted silently. This mirrors the non-present `luks_uuid == ""`
contract, which commit `7240ed78` documented *and pinned with a test in the
same commit* precisely because it "was pinned by nothing." The ideal fix
follows that precedent: correct the doc **and** pin the contract so it cannot
drift again.

Rejected alternative -- nulling `mapper` for non-present rows to make the
"observed/never reconstructed" claim literally true: this would break the
configured-handle symmetry with `by_id` (both derive from config; nulling one
and not the other is incoherent), drop genuinely useful expected-handle
information, and is a contract-breaking behavior change for monitoring
consumers over a Low-severity doc mismatch. Not done.

## Changes

Three parts, one root cause. All in two files:
`docs/commands/status.md` and `cli/src/status.rs`.

### A. Docs -- scope the `mapper` bullet (the actual fix)

Rewrite the `mapper` bullet (`docs/commands/status.md`, currently the
"observed ... do not reconstruct" bullet around the `disks[]` field list) to
mirror the **two-case present-vs-non-present structure the adjacent `name`
bullet already uses** ("For a matched present member ... for a non-present
disk it is the configured name"). Proposed wording:

> - `mapper`: device-mapper name -- a runtime handle, not identity. For a
>   present pool member it is the **observed** live mapper; for a matched
>   member that is normally `braid-<name>` but may have drifted (decision 024
>   tolerates mapper drift), so do not reconstruct it as `braid-${name}` or you
>   will miss the drift. For a non-present disk (`missing`, `offline`,
>   `unknown`, `luks-header-unreadable`, `luks-uuid-mismatch`) braid does not
>   report an observed mapper, so it emits the **expected** `braid-<name>`
>   derived from the configured name, paralleling the configured `name` and
>   `by_id` on those rows.

Why this wording and not the obvious "(matched member or foreign live
device) ... normally `braid-<name>`": a **foreign** live device's mapper is
the observed dm name, not `braid-<name>` (`present_display_name` falls back to
`mapper.0.clone()`, `cli/src/status.rs#present_display_name`), and it is not a
"member" that drifts -- so the `braid-<name>`/drift nuance is scoped to matched
members. And the non-present justification is "braid carries no observed mapper
state for unpooled rows," **not** "no live mapper exists": line 1113 emits the
expected name unconditionally, even for a `luks-uuid-mismatch`/`offline` row
that has a mapper open (`mapper_open: true`).

Do **not** add `mapper` to the post-example note (the
`"luks_uuid": ""`, `devid: null`, ... note): that note enumerates fields that
*blank/null out* on non-present rows, and `mapper` does not -- it carries a
value. The two-case bullet is the right home for the distinction.

Leave the `by_id` bullet alone: it is thin (doesn't spell out that the
non-present value is the configured path) but **not false** -- it never
claimed "observed/never reconstruct", so it surfaces no contradiction. Out of
scope.

### B. Test -- pin the production contract (one assertion)

Add a mapper assertion to the existing integration test
`build_disk_reports_foreign_config_uuid_classified_as_uuid_mismatch`
(`cli/src/status.rs`, ends ~line 5151). It already exercises the real
`build_disk_reports` and builds a non-present (`LuksUuidMismatch`) row as
`ctx.disks[1]` but never checks mapper. Add:

```rust
// Non-present rows carry the expected braid-<name>, not an observed mapper
// (docs/commands/status.md mapper field contract).
assert_eq!(ctx.disks[1].mapper, "braid-disk1");
```

One assertion suffices: all five non-present statuses share the single
`mapper_name(&cd.name)` line (`cli/src/status.rs:1113`), so pinning one
non-present row pins the reconstruction for every status. The single
assertion guards all five only *while* that shared line exists; a future
per-branch refactor of the unpooled arm would narrow this guard to the
mismatch row and would need its own assertion per branch.

### C. Test -- make the serialization fixture realistic

In the hand-built serialization test `status_json_verbose_disks`
(`cli/src/status.rs`, ~lines 1984-2097), the three non-present `DiskReport`
fixtures set `mapper` to **bare names** that production can never emit
(`"disk3"`, `"disk4"`, `"disk6"`) and then assert them (the
`assert_eq!(d1["mapper"], "disk3")` / `d2`/`d3` lines). Change both the
fixture values and the matching assertions to the realistic `braid-<name>`
form (`"braid-disk3"`, `"braid-disk4"`, `"braid-disk6"`) so this JSON-shape
reference test exemplifies the real contract instead of an impossible value.
(The `present` fixture's `mapper: "disk1"` may likewise be set to
`"braid-disk1"` for realism, though present mappers can legitimately drift, so
that one is optional polish, not a correctness fix.)

## Verification

- `mdbook build docs` -- validates the doc edit and its cross-links
  (`docs/book.toml` runs `mdbook-linkcheck2`; a broken `decision 024` /
  status reference fails here). Visually confirm the rewritten bullet renders
  and reads as two cases.
- `just test-rust` -- runs the CLI unit tests, including the amended
  `build_disk_reports_foreign_config_uuid_classified_as_uuid_mismatch` (B) and
  `status_json_verbose_disks` (C). Both must pass. B is a **characterization
  pin, not red-green TDD**: it passes immediately because the current code is
  already correct (the doc was wrong, not the code). Its value is as a
  regression guard -- it would fail only if someone later took the rejected
  null-the-mapper alternative.

No VM tests needed -- this touches only docs prose and Rust unit
tests/fixtures, no systemd/mount/pool-lock surface.

## Implementation notes

- Part C: left the `present` fixture's `mapper: "disk1"` unchanged in
  `status_json_verbose_disks`. The plan flagged changing it as optional polish,
  and a present mapper is the observed live value that can legitimately drift,
  so a bare placeholder is not the impossible value the three non-present rows
  were. Kept the diff to the correctness fix (the three non-present rows).
