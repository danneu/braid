# Fix: doctor.md lists a `luksDump` probe doctor no longer makes

## Context

`docs/commands/doctor.md` step 2 of "What happens under the hood" tells operators
that `braid doctor` probes each declared disk via three cryptsetup invocations:
`cryptsetup isLuks`, `cryptsetup luksDump`, and `cryptsetup luksUUID`. The
declared-disks code path makes only **two** of those calls.

This was accurate until commit `a9ab6d51` ("collapse the unreachable Damaged
header state"). Before it, the doctor path ran `isLuks` then `luksDump` (inside
the old `probe_luks_header`, which returned `Damaged` on dump failure) then
`luksUUID` -- three probes. `a9ab6d51` collapsed `probe_luks_header` to a single
`isLuks`, dropping the `luksDump` probe. That commit fixed the `declared_disks`
table row (line 74: removed "damaged") but missed the under-the-hood step at
line 104, leaving it stale.

An operator reading the doc to reason about doctor's side effects or to reproduce
the probe by hand is told about a `cryptsetup luksDump` call doctor never makes.
This violates the AGENTS.md rule that behavior docs must track the actual
invocation.

### Authoritative code (verified)

Doctor's declared-disks probe is `isLuks` then `luksUUID`, no `luksDump`:

- `cli/src/doctor.rs#classify_luks_identity` -- calls `probe_luks_header`, then
  one `CmdRequest::CryptsetupLuksUuid`. No `luksDump`.
- `cli/src/luks.rs#probe_luks_header` -- runs a single `CmdRequest::CryptsetupIsLuks`.
  Its doc comment states it outright: "A single `isLuks` is sufficient --
  `luksDump` gates on the same `crypt_load`, so a second probe could only
  disagree under a transient fault."

## Change

A single edit to `docs/commands/doctor.md`, line 104.

From:

> 2. Loads UUID-keyed `pool.json` and probes each declared disk via `cryptsetup isLuks`, `cryptsetup luksDump`, and `cryptsetup luksUUID`.

To:

> 2. Loads UUID-keyed `pool.json` and probes each declared disk via `cryptsetup isLuks` and `cryptsetup luksUUID`.

(Remove `cryptsetup luksDump` from the list; collapse the three-item Oxford-comma
list to a two-item "and" join. Probe order `isLuks` -> `luksUUID` matches the
code.)

## Explicitly out of scope (do NOT change)

- **The `declared_disks` "What it checks" table row (line 74).** It describes the
  check abstractly ("has a readable LUKS header, its live LUKS UUID matches the
  `pool.json` key") and contains no `isLuks`/`luksDump`/`luksUUID` token. It was
  already corrected by `a9ab6d51`. The original finding proposed editing it too;
  that is a misread -- there is nothing to remove there.
- **`docs/commands/discover.md:66`** ("Runs `cryptsetup luksDump`...") is correct.
  `discover` genuinely runs `luksDump` via `CmdRequest::CryptsetupLuksDumpText`
  (`cli/src/discover.rs#probe_disk`).
- **`docs/internals/luks-unlock.md` (the `PresentLuks` paragraph) and the in-code
  doc comment at `cli/src/mount.rs`** ("both `luksUuid` and `luksDump` succeeded")
  are correct. The *unlock* path's `cli/src/probe.rs#probe_config_disk` runs
  `luksUUID` then `luksDump` (`CryptsetupLuksDumpText`) to enforce the LUKS2-only
  invariant (reads `Version:`) and the label. The doctor and unlock paths
  legitimately differ; this is not copy-paste drift to unify.

No code changes. No new tests. Prose accuracy is not itself test-covered, but the
behavior the corrected doc rests on -- that doctor's probe path makes no `luksDump`
call -- is already pinned by `cli/src/luks.rs#probe_luks_header_ok`, whose
deliberately absent `luksDump` mock makes a regression re-adding the probe fail with
`MissingMock`. Its sibling `cli/src/luks.rs#probe_luks_header_unreadable_when_is_luks_fails`
covers the `isLuks`-fails branch.

## Verification

1. Re-read the edited line 104 against `cli/src/doctor.rs#classify_luks_identity`
   and `cli/src/luks.rs#probe_luks_header` to confirm the documented sequence
   (`isLuks` -> `luksUUID`) matches the code exactly.
2. `mdbook build docs` -- sanity check that the book still builds (linkcheck via
   `mdbook-linkcheck2`). This edit touches no links, so it is a light
   regression check only.
3. Optional sweep: `rg -n "luksDump" docs/commands/doctor.md` should return no
   results after the edit.
