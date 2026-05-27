# Plan: document discover's intentional isLuks silent-skip

## Context

An ultrareview-style finding flagged `cli/src/discover.rs:351-357` as a Medium
correctness bug: a braid disk that returns nonzero from `cryptsetup isLuks` for a
transient/IO reason is silently dropped (`if raw.exit_status != 0 { continue; }`),
while the sibling `luksDump` failure path warns via
`DiscoverWarning::LuksDumpFailed`. The finding proposed adding a new
`DiscoverWarning::IsLuksFailed` and classifying isLuks stderr to separate "not a
LUKS device" (skip) from transient errors (warn).

Investigation (`/verify-issue`) showed the proposed fix is **inoperative** and the
current behavior is **correct by design**:

- `cryptsetup isLuks` calls `crypt_init` first, then installs `quiet_log` before
  `crypt_load`. Device setup can log before that quiet callback is installed, but
  the header-classification path the proposed fix depends on has no classifiable
  not-LUKS/header-read stderr: `crypt_load` calls `_crypt_load_luks(..., true,
  false)`, and the quiet LUKS load suppresses the normal "not a valid LUKS
  device" error (`reference/cryptsetup/src/cryptsetup.c:2475-2479`;
  `reference/cryptsetup/lib/setup.c:1121`, `:892-893`;
  `src/utils_tools.c:84-91` `quiet_log`).
- `translate_errno` collapses both `-EINVAL` ("not a LUKS device") and default
  `-EIO` to **exit code 1** (`src/utils_tools.c:219-235`), so a transient read
  error is also indistinguishable from a non-member by exit code.
- discover scans *every non-partition* `/dev/disk/by-id/` entry, where not-LUKS
  is the common case (boot disk, USB sticks). Warning on every nonzero isLuks would
  spam the operator and would break the existing
  `non_luks_device_never_reaches_luks_dump` test (asserts `scan.warnings.is_empty()`).
- The genuine "a member transiently vanished" hazard is already guarded by
  `discover --write --expect-count <N>`, which fails closed on a short count
  (documented in `docs/commands/discover.md` and `docs/internals/luks-unlock.md`).

So the asymmetry is real but correct, and the right action is to **document why**
at the code site -- not to change behavior. This dissolves a recurring confusion
(any future reader of `discover.rs` re-spots the skip-vs-warn asymmetry and re-files
the same finding) without regressing the silent bulk filter.

## Approach: single inline comment, no behavioral change

Replace the bare `// Check if LUKS` comment at `cli/src/discover.rs:351` with an
explanatory comment above the isLuks skip. Proposed wording (final, ready to apply):

```rust
// isLuks is the silent bulk filter: discover probes every non-partition by-id
// entry and most are legitimately not LUKS (boot disk, USB sticks), so a nonzero
// exit is the common case and must not warn. The header-classification failure is
// unclassifiable in-band: after crypt_init succeeds, action_isLuks installs
// quiet_log before crypt_load (reference/cryptsetup/src/cryptsetup.c:2475-2479;
// src/utils_tools.c:84-91), crypt_load calls _crypt_load_luks(..., true, false)
// (reference/cryptsetup/lib/setup.c:1121), and the quiet LUKS path suppresses the
// normal "not a valid LUKS device" error (reference/cryptsetup/lib/setup.c:892-893).
// translate_errno collapses both -EINVAL ("not a LUKS device") and default -EIO
// to exit 1 (src/utils_tools.c:219-235), so a transient read error is
// indistinguishable from a non-member. We skip silently and let
// `discover --write --expect-count <N>` fail closed if a member is momentarily
// unreadable. (probe_luks_header in luks.rs maps the same nonzero exit to
// `Unreadable` because its caller already knows the device is a pool member; the
// luksDump path below warns because isLuks has by then confirmed this is LUKS.)
let raw = runner.run(&CmdRequest::CryptsetupIsLuks {
    device: path_str.clone(),
})?;
if raw.exit_status != 0 {
    continue;
}
```

Why this home and shape (per braid conventions):

- The confusion is a code-reader/reviewer confusion at this exact line; the
  rationale belongs inline next to the code that implements it, matching the
  existing rich-comment style in this file (e.g. the luksDump comment at
  `discover.rs:359-388`) and the `(reference/...:lines)` citation style used
  elsewhere (e.g. `remove.rs:366`, `cmd.rs:3006-3007`).
- The comment is **self-defending**: it carries the two upstream facts (no
  classifiable header-classification stderr after the quiet `crypt_load` path;
  `-EIO` and `-EINVAL` both -> exit 1) that disprove the obvious "classify stderr /
  add a warning" fix, so the same finding can't be re-filed. A terse 3-line note
  would not do this -- the citations are what make it stick.
- It names the contrast with `probe_luks_header` (`luks.rs:675-690`), which the
  exploration identified as the codebase's sole *opposite* handling of the same
  isLuks->luksDump sequence -- the exact question a careful reader will have.

### Explicitly NOT doing

- **No behavioral change.** Do not add `DiscoverWarning::IsLuksFailed` or any
  stderr/exit-code classification on the isLuks path. The header-classification
  discriminators don't exist (quiet `crypt_load` provides no classifiable
  not-LUKS/header-read stderr; EIO and not-LUKS both exit 1), and warning per
  nonzero isLuks would regress `non_luks_device_never_reaches_luks_dump`.
- **No edit to `docs/commands/discover.md`.** Operator docs are cookbook/reference
  (AGENTS.md); implementation rationale is the wrong register, and the
  operator-actionable guard (`--expect-count`) is already documented there.
- **No edit to the luksDump comment or `luks.rs`.** The warn side is the
  unsurprising default; documenting it would be a second sync point for one idea,
  against braid's doc-comment policy ("if removing the comment would not lose
  information a reader could not recover from the code, do not write it"). The
  contrast is explained once, from the surprising (skip) side.

## Files

- `cli/src/discover.rs` -- replace the `// Check if LUKS` comment at ~line 351
  with the explanatory comment above. **Comment-only; no executable line changes.**

## Verification

- `just test-rust` -- confirms the comment compiles and that
  `non_luks_device_never_reaches_luks_dump` plus the other `discover` unit tests
  still pass unchanged, proving behavior is untouched.
- Spot-check the cited reference lines against the local checkout:
  `reference/cryptsetup/src/cryptsetup.c:2475-2479` (`crypt_init` before
  `quiet_log` before `crypt_load`), `reference/cryptsetup/lib/setup.c:1121`
  (quiet LUKS load), `reference/cryptsetup/lib/setup.c:892-893` (suppressed
  "not a valid LUKS device" error), `src/utils_tools.c:84-91` (`quiet_log`), and
  `src/utils_tools.c:219-235` (`translate_errno`).
- **No new test.** The change is documentation only; the existing test already
  pins the silent-skip behavior the comment explains.
