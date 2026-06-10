# discover --write: defer the preview print to the success path

## Context

`braid discover --write` prints the discovered membership preview to stdout
*before* its post-scan mutation gates fire, so a refused write still emits a
member listing. Concretely, in `cli/src/main.rs` the `Commands::Discover` arm
runs `render_preview_lines` -> `println!` (stdout) and only then calls
`write_discovered_membership` (`cli/src/discover.rs#write_discovered_membership`),
which owns the `--expect-count` and corrupt-sidecar gates. So
`braid discover --write --expect-count N > members` on a mismatch (or when the
forensic sidecar cannot be written) captures `name = by-id` rows for a write
that did not happen.

This is the residual of commit `1d8407b5` ("gate --write refusals before the
scan"), whose stated goal was "on refusal the command now exits ... with empty
stdout." That commit reached the goal only for the *scan-independent* gates
(pending-op, ValidUuidKeyed), which it moved ahead of the scan. The two
*scan-dependent* gates need `members.len()` and so cannot move before the scan;
they were left firing after the preview print. The fix completes `1d8407b5`'s
goal using the complementary mechanism: instead of moving the gate before the
print, defer the *print* until after the gate passes.

**Outcome / invariant:** a refused `discover --write` produces empty stdout,
uniformly across every refusal cause; a successful `discover --write` still
prints the rows to stdout. Bare `discover` (read-only, no post-preview refusal)
is unchanged. This also aligns discover's `--write` path with the ADR 022
precedent that the preview/product is emitted only on the success path, with
refusals emitting diagnostics to stderr.

**No data-safety change.** `pool.json` is already left byte-identical on every
refusal (gates precede `save_membership`); this is purely an output-ordering /
project-fit fix. Severity: Low. Scope confirmed discover-only: every other
mutating command runs its gates inside the planner before any stdout product,
so there is no sibling instance to fold in.

## Step 1 (TDD: write the failing test first) -- `tests/cli/braid-discover.py`

Per the repo's TDD rule (AGENTS.md "Testing": *write failing tests first,
confirm they fail for the right reason, then implement ... to make them pass*),
change the test **before** the code. The arm is binary-only (unreachable by
Rust unit tests), so the guard is the VM test.

The existing `"expect-count mismatch refuses and writes nothing"` subtest uses
`machine.fail("braid discover --write --expect-count N 2>&1")` -- merged
streams, so it asserts the error is present but **cannot** observe whether the
preview leaked. Upgrade it to split streams and assert empty stdout, mirroring
the file's two pre-scan refusal subtests (which already do
`braid discover --write >/tmp/discover-out 2>/tmp/discover-err` then
`assert out.strip() == ""`).

For each `expected in [1, 3]`: run under split streams, assert non-zero exit,
`out.strip() == ""`, the `expected exactly N members, found 2` message on
**stderr** (Display text at `cli/src/discover.rs#DiscoverWriteError`), and keep
the existing `read_pool_json() == corrupt` and `assert_no_corrupt_sidecars()`
guards. Update the subtest's intent/why comment to state that splitting the
streams is what distinguishes "rows withheld" from "rows printed then error".

**Prove RED before writing any code.** With only this test changed, run
`just test-vm braid-discover` and confirm it fails *for the right reason*:
against the two-disk fixture, `discover --write --expect-count 1` (and `3`)
currently prints both `disk1 = /dev/disk/by-id/...` and `disk2 = ...` rows to
stdout *before* refusing, so `/tmp/discover-out` is non-empty and
`out.strip() == ""` fails with the leaked rows echoed in the assertion message.
Record that failure output. A run that errors elsewhere, or whose failure is not
the empty-stdout assertion firing on the captured preview rows, means the test
is not exercising the leak -- fix the test before proceeding to Step 2.

## Step 2 -- code fix (`cli/src/main.rs`, Discover arm only)

With Step 1 red, make it green. Today the arm prints the preview
unconditionally, then branches on `args.write`. Rewrite so the rows are
rendered once up front (borrowing `members` before it is moved) but printed
*inside* the success paths.

`render_preview_lines(&members)` borrows; `write_discovered_membership(members,
..)` consumes `members` and returns `Ok(members)`. Render-before-move (not
render-from-`Ok`) is the clean shape: the bare branch has no `Ok` value to
render from, so computing `preview` once before the `if` serves both branches
from a single call site. `pool_json` is an independent owned `PathBuf` from
`paths.pool_json()` and is unaffected by the move.

Replace the current `for line in render_preview_lines(&members) { println!(..) }`
loop plus the `if args.write { .. } else { .. }` block with:

```rust
            // Render the preview once (borrows `members`), but in `--write`
            // mode defer printing it until write_discovered_membership returns
            // Ok: its post-scan gates (ExpectCountUnmet, CorruptSidecarFailed)
            // fire after the scan, so a refused --write must leave stdout empty,
            // matching the pre-scan refusals. Bare discover has no post-preview
            // refusal, so it prints immediately.
            let preview = braid_cli::discover::render_preview_lines(&members);
            if args.write {
                match braid_cli::discover::write_discovered_membership(
                    members,
                    &paths,
                    args.expect_count,
                ) {
                    Ok(_) => {
                        for line in preview {
                            println!("{line}");
                        }
                        eprintln!("pool membership written to {}", pool_json.display());
                    }
                    Err(e) => {
                        print_cli_error(&e.to_string());
                        std::process::exit(1);
                    }
                }
            } else {
                for line in preview {
                    println!("{line}");
                }
                eprintln!("pass --write to save to {}", pool_json.display());
            }
```

Notes:
- The two `for line in preview` loops are in mutually exclusive branches, so
  moving `preview` in each is borrow-clean (one move per control-flow path).
- On the success path the merged-stream (`2>&1`) order is still rows-then-
  "pool membership written", preserving existing merged-output assertions.
- The `Err` arm is cause-agnostic: both `ExpectCountUnmet` and
  `CorruptSidecarFailed` hit the same branch that never reaches the print loop,
  so the stdout suppression covers both post-scan refusals by construction.
- `write_discovered_membership` and `render_preview_lines` do **not** change.

Re-run `just test-vm braid-discover`: the Step 1 subtest now passes (empty
stdout on refusal) and the rest of the file stays green.

## What is intentionally NOT changed (and why)

- **Success-path stdout assertion:** not duplicated into `braid-discover.py`.
  `tests/module/single-disk.py` already runs `discover --write` with split
  streams and asserts `"= /dev/disk/by-id/" in outw` (rows on stdout) plus the
  confirmation on stderr -- a precise guard that the reorder preserves rows on
  success. Adding a second copy here is redundant.
- **Corrupt-sidecar VM test:** not added. The unit test
  `discover.rs#discover_write_refuses_when_corrupt_sidecar_cannot_be_written`
  already covers the helper's refusal + sidecar safety hermetically (it chmods
  the tempdir `0o500`; that mechanism does not transfer to a root VM). The
  arm-level stdout suppression is cause-agnostic, so the upgraded expect-count
  subtest transitively exercises the same `Err` branch the sidecar failure
  would take. A one-line note in the subtest comment records this.
- **`write_discovered_membership` gate placement:** unchanged. `--expect-count`
  must stay post-scan (it needs `members.len()`); this plan moves the *print*,
  not the *gate*.

## Step 3 -- docs (`docs/commands/discover.md`)

Append one sentence to the stdout/stderr paragraph (the
"The membership rows are written to stdout ... captures only the rows." block),
documenting the new observable invariant:

> A refused `discover --write` (for example an `--expect-count` mismatch, or a
> forensic sidecar that cannot be written) prints nothing to stdout and reports
> the refusal on stderr, so a redirected capture of a write that did not happen
> is empty rather than holding membership rows.

The "What happens under the hood" / "Safety checks" sections already describe
these refusals and need no change.

## Critical files (in execution order)

- `tests/cli/braid-discover.py` (Step 1) -- upgrade the `"expect-count mismatch
  refuses and writes nothing"` subtest to split streams + empty-stdout
  assertion; this is the test that must go red first.
- `cli/src/main.rs` (Step 2) -- `Commands::Discover` arm: the only source change.
- `docs/commands/discover.md` (Step 3) -- append the refusal-stdout sentence.
- Read-only references (must NOT change): `cli/src/discover.rs`
  (`render_preview_lines`, `write_discovered_membership`, the corrupt-sidecar
  unit test); `tests/module/single-disk.py` (success-path guard already present).

## Verification (TDD sequence)

1. **RED:** apply Step 1 only, then `just test-vm braid-discover`. Confirm the
   upgraded `"expect-count mismatch"` subtest fails on `out.strip() == ""` with
   the leaked `disk1 = ...` / `disk2 = ...` rows shown in the assertion message
   -- proof the test catches the current leak. Record the failure.
2. **GREEN (code):** apply Step 2. Run `cargo fmt` (the arm is re-indented) and
   `just clippy` (`cargo clippy --manifest-path cli/Cargo.toml --tests`) --
   clippy catches any borrow/move misuse (use-after-move of `members`,
   double-move of `preview`). Then `just test-rust` -- confirms the helpers are
   untouched and still pass (incl. the expect-count Display-text and
   corrupt-sidecar unit tests).
3. **GREEN (VM):** re-run `just test-vm braid-discover` -- the same subtest now
   passes (empty stdout on refusal) and the rest of the file stays green.
4. `just test-vm braid-module-single-disk` -- confirms the `--write` success
   path still routes rows to stdout and the confirmation to stderr after the
   reorder.
5. Apply Step 3 (docs); not test-gated.
