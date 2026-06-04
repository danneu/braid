# Inline-excerpt convention for external `reference/` citations

## Context

braid cites upstream tool source in code comments to justify parser/command
shape. Today ~60 of these citations use a bare line number into `reference/`
(e.g. `reference/nut/clients/upsc.c:141`). A review flagged the three `upsc.c:141`
copies as violating the AGENTS.md "File References" rule. Investigation showed the
finding is mis-scoped: line-number `reference/` cites are a deliberate, 60-instance,
author-sanctioned convention (the rule's examples and greppability rationale are about
braid's *own* tracked code), so a 3-site `path#symbol` patch would only create a
3-vs-57 inconsistency.

The real problems are two: (1) line numbers into `reference/` drift on every
`just fetch-references`, and (2) `reference/` is **gitignored** -- on a clean checkout
it is absent, so for a fresh agent every one of those citations is a dead pointer that
resolves to nothing. The fix the user chose: stop pointing, and **inline the relevant
upstream excerpt** as frozen, version-stamped ground truth a reader sees without
fetching `reference/`. This dissolves the original finding and the recurring class.

Scope of this change: write the convention into AGENTS.md and apply it to the three
`upsc.c` sites as the worked example. The other ~57 line-number cites are tolerated and
migrate opportunistically when their files are next touched -- no mass sweep (lossy on
the 37 range/region cites, no test coverage to catch transcription errors).

## Decision

Cite external `reference/` code by **shape**:

- **Short, behavior-defining snippet** (one line / small fn emitting a format, token, or
  exit code braid parses): inline the excerpt, stamped `pkg <version>, <path> (fn name)`,
  no line number. Fence it as a **non-`rust`** code block (```` ```c ```` for source,
  ```` ```text ```` for tool output) so rustdoc does not run it as a doctest -- `braid-cli`
  is a lib crate (`cli/src/lib.rs`), so an unannotated/`rust` block becomes a failing
  doctest. Guard is `cargo test -p braid-cli --doc`; `just test-rust` does *not* catch it
  (its `--lib --bin --test` selectors skip doctests). Inline code span is fine for tight
  function/field docs. Precedent already in tree:
  `cli/src/parse/cryptsetup_luks_version.rs#parse_cryptsetup_luks_version`.
- **Region / multi-line** (a 50-line fn, a struct, scattered lines -- no single quotable
  line): keep a *pointer*, `pkg <version>, <path> (fn name)` + one-line paraphrase. Prefer fn name over
  line number; bare line range is a last resort. Do not inline a wall of code.

Stamp = `pkg <version>` (the upstream release tag, `git -C reference/<pkg> describe
--tags`). No SHA (per user); both the inlined-excerpt and region-pointer forms carry it. The stamp is the re-verify trigger on a nixpkgs bump that
changes the tool version -- the same Parser Compatibility event that recaptures fixtures.

Concrete pin for the worked example: `nut 2.8.4`, `clients/upsc.c`, fn `list_vars`, the
emitting line `printf("%s: %s\n", answer[2], answer[3]);`. Verified: line 141 today is
that `printf`, inside `list_vars` (defined at the top of the same fn).

## Part A -- AGENTS.md convention

Insert a `### External `reference/` citations` subsection at the end of the existing
`## File References` section (after the "transient analysis in `plans/wip/` is exempt."
paragraph, before `## Decision Doc References`). Exact text:

````markdown
### External `reference/` citations

The rule above governs braid's own tracked files. External upstream code lives in
`reference/`, which is gitignored and refreshed wholesale by `just fetch-references`: it
is absent on a clean checkout and invisible to CI. A line number into it drifts on every
refresh, and a braid-style `path#symbol` is not greppable when the file is not on disk --
neither form validates or even resolves. Cite external upstream code by its **shape**:

- **Short, behavior-defining snippet** -- one line or small function emitting a format,
  token, or exit code braid parses. Inline the excerpt as frozen ground truth, so a reader
  sees the contract without fetching `reference/`. Stamp it `pkg <version>, <path> (fn name)`
  and drop the line number. Fence the excerpt with a non-`rust` language tag -- `c` for
  source, `text` for tool output -- so rustdoc does not run it as a doctest. An unannotated
  or `rust`-tagged block becomes a failing doctest, caught by `cargo test -p braid-cli --doc`
  (not `just test-rust`, whose `--lib --bin --test` selectors skip doctests).
  Precedent: `cli/src/parse/cryptsetup_luks_version.rs#parse_cryptsetup_luks_version`. An
  inline code span (`` `printf(...)` ``) is fine for a tight function or field doc where a
  fenced block is too heavy. The `pkg <version>` stamp is the upstream release tag (`git -C
  reference/<pkg> describe --tags`); it pins the excerpt and is the re-verify trigger when a
  nixpkgs bump changes that tool's version -- the same Parser Compatibility refresh event
  that recaptures fixtures.
- **Region or multi-line** -- a code area with no single quotable line (a long function, a
  struct, two scattered lines). Keep a pointer, not a wall of inlined code: `pkg <version>,
  <path> (fn name)` plus a one-line paraphrase of what's there. Prefer a function name over a line
  number; a bare line range is a last resort.

Existing bare-line-number `reference/` citations are tolerated -- nothing validates them
either way -- but migrate them toward the excerpt or pointer form when you next touch the
surrounding file.
````

## Part B -- rewrite the three `upsc.c:141` sites

### B1. `cli/src/parse/upsc.rs` (module doc) -- block excerpt

Before:
```rust
//! NUT's `upsc` client emits one `key: value` pair per line (see
//! `reference/nut/clients/upsc.c:141`). This parser splits the familiar
//! keys (`ups.status`, `battery.*`, `input.*`, `ups.load`,
```
After:
````rust
//! NUT's `upsc` client emits one `key: value` pair per line. Source,
//! nut 2.8.4, clients/upsc.c (fn `list_vars`):
//! ```c
//! printf("%s: %s\n", answer[2], answer[3]);
//! ```
//!
//! This parser splits the familiar keys (`ups.status`, `battery.*`,
//! `input.*`, `ups.load`, `ups.realpower.nominal`, `ups.test.result`,
//! `device.*` / `ups.mfr` / `ups.model` / `ups.serial`) into the typed
//! `UpscOutput` shape, and keeps every other line verbatim in `extra` so
//! unfamiliar driver keys are still observable via `braid ups status --json`.
````
(The rest of the module doc -- the "infallible by design" paragraph -- is unchanged. Note
the rewrap: the key list moves into the paragraph after the fenced block.)

### B2. `cli/src/parse/types.rs` (`UpsStatusFlag::as_token` doc) -- inline span

Before:
```rust
    /// Rendered token, matching NUT's own `ups.status` vocabulary
    /// (`reference/nut/clients/upsc.c:141` emits these verbatim).
```
After:
```rust
    /// Rendered token, matching NUT's own `ups.status` vocabulary. `upsc`
    /// emits these verbatim -- nut 2.8.4, clients/upsc.c (fn `list_vars`):
    /// `printf("%s: %s\n", answer[2], answer[3]);`
```

### B3. `cli/src/cmd.rs` (`UpscQuery` doc) -- inline span

Before:
```rust
    /// `upsc <name>` — NUT status query. Emits `key: value` lines (see
    /// `reference/nut/clients/upsc.c:141`) on stdout; non-zero exit when the
    /// upsd daemon is unreachable or the UPS name is unknown. braid uses
    /// this for preflight-on-battery and `braid ups status`.
```
After:
```rust
    /// `upsc <name>` -- NUT status query. Emits `key: value` lines on stdout
    /// (nut 2.8.4, clients/upsc.c fn `list_vars`:
    /// `printf("%s: %s\n", answer[2], answer[3]);`); non-zero exit when the
    /// upsd daemon is unreachable or the UPS name is unknown. braid uses
    /// this for preflight-on-battery and `braid ups status`.
```
(Incidental: the leading `—` em-dash becomes `--`, matching the sibling `EthtoolShow`
doc and the global ASCII style.)

## Notes / trade-offs

- **Deliberate duplication.** The one-line `printf` contract is now inlined in three
  places. For a single, stable output-format line this is an acceptable cost of
  self-contained citations (the user's explicit goal: no link-following). It is *not* a
  license to inline multi-line regions in N places -- that is what the "region" pointer
  rule prevents.
- **`as_token` precision.** The existing comment claims upsc.c "emits these verbatim."
  Accurate: `list_vars` prints whatever `ups.status` string the daemon sends (e.g. `OL
  OB LB`) verbatim; the rewrite preserves that claim and does not re-source the token
  *vocabulary* (defined in NUT drivers, out of scope).
- **No new CI surface.** `reference/` is gitignored, so nothing validates these excerpts
  in CI -- same as the line numbers they replace. The `pkg <version>` stamp + the Parser
  Compatibility refresh discipline is the maintenance mechanism.

## Critical files

- `AGENTS.md` -- add `### External `reference/` citations` under `## File References`.
- `cli/src/parse/upsc.rs` -- module-doc block excerpt (B1).
- `cli/src/parse/types.rs` -- `UpsStatusFlag::as_token` inline excerpt (B2).
- `cli/src/cmd.rs` -- `UpscQuery` inline excerpt (B3).

Reuse / precedent (do not modify): `cli/src/parse/cryptsetup_luks_version.rs` and
`cli/src/parse/cryptsetup_luks_label.rs` already use ```` ```text ```` fences to inline
upstream output without doctesting -- the pattern B1 follows.

## Verification

1. `cargo test -p braid-cli --doc` -- the required doctest check, and the *only* command here
   that exercises doctests. Confirms the B1 ```` ```c ```` fence is not extracted as a doctest
   (an unannotated or `rust`-tagged block would fail to compile `printf(...)` as Rust). B2/B3
   are inline spans, never doctested. Note `just test-rust` does *not* cover this -- it runs
   `cargo test --lib --bin braid --test ...`, whose target selectors exclude `--doc`.
2. `cargo doc -p braid-cli --no-deps` (optional) -- eyeball that the module doc renders the
   excerpt as a C code block and the surrounding prose reflows cleanly.
3. `rg -n 'reference/nut/clients/upsc\.c:141'` -- must return nothing (all three line-number
   cites removed).
4. No fixture/VM tests touched; this is comments + one AGENTS.md section. No `just test-vm`
   needed.

## Implementation notes

- The raw verification search also found a pre-existing historical plan cite in
  `plans/impl/2026-05-18-upsc-status-flag-order.md`; the implementation verified the
  planned target files and the non-plan tree instead of rewriting that older plan
  artifact.
