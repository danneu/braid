# Pivot: route `discover` preview rows to stdout, keep prose on stderr

## Context

Bare `braid discover` is a read-only command whose product is the discovered
membership rows (`name = /dev/disk/by-id/...`). Today those rows are printed
with `eprintln!` (stderr) at `cli/src/main.rs:917`, so the read-only product is
not pipeable: `braid discover | grep ironwolf` yields nothing, and the rows are
interleaved with warnings on the same stream. Every other read/preview surface
in the CLI -- `status` (`status.rs:576,579`), `doctor` (`doctor.rs:1551,1556`),
and `idle` (`main.rs:786-794`) -- writes its product to stdout. `discover` is
the sole outlier, and it also diverges from the project's documented
preview-stream philosophy (Decision 022 output contract, `README.md` dry-run
section: the preview product goes to stdout; confirmations/warnings/failures go
to stderr).

A `/verify-issue` pass confirmed the inconsistency but pivoted the fix: the
original finding proposed moving the rows **and** both trailing prose lines
("pass --write to save", "pool membership written") to stdout. That over-moves
prose onto the data stream -- it would dirty the very pipe we want to enable
(`braid discover > members` would capture the "pass --write to save" hint as if
it were data) and put a real-run confirmation on stdout against the README/022
"confirmations -> stderr" rule. The ideal split is simpler and pipe-clean:

| Output | Mode | Stream | Change |
| --- | --- | --- | --- |
| Membership rows (`render_preview_lines`) | bare + `--write` | **stdout** | `eprintln!` -> `println!` |
| "pass --write to save to X" hint | bare | stderr | none |
| "pool membership written to X" confirmation | `--write` | stderr | none |
| "no braid-labeled LUKS devices found" (exit 1) | bare + `--write` | stderr | none |
| Scan warnings (`drain_warnings`) | bare + `--write` | stderr | none |
| Errors (`print_cli_error`) | bare + `--write` | stderr | none |

One rule: **the membership rows are the data product (stdout); all prose is
diagnostic (stderr)**. Rows go to stdout uniformly in both modes -- keeping the
stream mode-independent is simpler than gating on `args.write`, and `verify-issue`
(which the user endorsed) assumed the uniform behavior.

## Change 1 -- code (one line)

`cli/src/main.rs:917`, inside the `Commands::Discover` arm:

```rust
for line in braid_cli::discover::render_preview_lines(&members) {
    println!("{line}");   // was: eprintln!("{line}")
}
```

Nothing else in the arm changes. Lines 913 (empty-case error + `exit(1)`), 926
(`--write` confirmation), and 934 (bare-mode hint) stay `eprintln!`. The
`render_preview_lines` helper (`cli/src/discover.rs:141-147`) is unchanged --
it already returns `Vec<String>` and the caller owns the stream.

## Change 2 -- regression test (extend an existing VM test)

The stream choice lives in the binary and only renders rows against real
braid-labeled LUKS disks, so it is unreachable by Rust unit tests; it must be
pinned in a VM test. Extend `tests/module/single-disk.py` (single disk -> one
row -> simplest assertion; reuses the already-booting VM, no new boot). Replace
the current bare subtest at lines 4-5:

```python
with subtest("braid discover finds pool members"):
    machine.succeed("braid discover --write")
```

with a stream-routing subtest. Run each redirected command under
`machine.succeed` (not `machine.execute`) so the exit-code assertion from the
original `succeed` call survives -- a `discover` that prints rows and writes
`pool.json` but exits nonzero must still fail the test. `machine.succeed`
returns stdout only, so send each stream to a file with `>out 2>err` and read
it back with `cat` (idiom from `tests/module/pool-lock-readonly-bypass.py`;
documented in `docs/dev/testing.md`). Pin the full contract in the one window
where `pool.json` is still absent:

```python
# Intent: `braid discover` (bare and --write) emits the membership rows on
#   stdout (the pipeable data product) while the "pass --write" hint, the
#   "pool membership written" confirmation, warnings, and errors stay on stderr.
# Why it exists: the rows were printed to stderr, so `braid discover | grep <disk>`
#   yielded nothing and the read-only product was not pipeable. No test pinned the
#   stream (callers used `2>&1` or bare `succeed`), so a regression could silently
#   move it back.
# Scenario: operator rebuilding a lost pool.json pipes `braid discover` into a
#   filter to confirm a specific drive was found before writing.
with subtest("braid discover routes membership rows to stdout, prose to stderr"):
    # Bare discover (pool.json absent): rows -> stdout, hint -> stderr, writes nothing.
    machine.succeed("braid discover >/tmp/d.out 2>/tmp/d.err")
    out, err = machine.succeed("cat /tmp/d.out"), machine.succeed("cat /tmp/d.err")
    assert "= /dev/disk/by-id/" in out, f"row not on stdout: {out!r}"
    assert "pass --write to save" in err, f"hint not on stderr: {err!r}"
    assert "pass --write to save" not in out, f"hint leaked to stdout: {out!r}"
    machine.succeed("test ! -e /var/lib/braid/pool.json")

    # discover --write (pool.json still absent): rows -> stdout, confirmation -> stderr.
    machine.succeed("braid discover --write >/tmp/dw.out 2>/tmp/dw.err")
    outw, errw = machine.succeed("cat /tmp/dw.out"), machine.succeed("cat /tmp/dw.err")
    assert "= /dev/disk/by-id/" in outw, f"row not on stdout: {outw!r}"
    assert "pool membership written" in errw, f"confirmation not on stderr: {errw!r}"
    assert "pool membership written" not in outw, f"confirmation leaked to stdout: {outw!r}"
    machine.succeed("test -e /var/lib/braid/pool.json")
```

The remaining subtests in `single-disk.py` (unlock, btrfs profile, round-trip)
are unchanged -- `discover --write` still creates `pool.json` for them. Leave
`raid1.py` as-is: the row-rendering path is identical regardless of disk count,
so single-disk coverage is sufficient.

## Change 3 -- docs

`docs/commands/discover.md:23-29` shows the bare-`discover` rows and the
`pass --write to save` hint together under "Output:", which hides the stream
split that is the whole point of this change. The README/Decision-022 preview
contract is dry-run scoped and does not directly govern `discover`, so the
stream behavior is otherwise undocumented. Add a short note after the "Output:"
block (before "## Common variations"):

> The membership rows are written to **stdout**; the `pass --write to save`
> hint, the `--write` "pool membership written" confirmation, scan warnings,
> and errors go to **stderr**. So `braid discover > members` (or
> `braid discover | grep <disk>`) captures only the rows.

## Decisions (no change needed)

- **`--write` rows on stdout:** intentional and uniform with bare mode. The
  rows are a preview of what is written; keeping one mode-independent rule is
  simpler than conditionally routing on `args.write`.

## Verification

1. `just test-rust` -- existing
   `render_preview_lines_returns_name_sorted_independent_of_uuid_order`
   (`discover.rs:725`) still passes (helper untouched); no Rust test pins the
   stream, so nothing else is affected.
2. `just test-vm braid-module-single-disk` -- exercises the new stream-routing
   subtest against the real binary; fails before Change 1, passes after. (The
   `test-vm` recipe passes names verbatim as `.#checks.<system>.<name>`; the
   flake attr is `braid-module-single-disk` (flake.nix:452), not `single-disk`.)
3. `just test-vm braid-module-single-disk braid-module-raid1 pool-lock-discover-contention`
   -- the discover-touching module tests (`braid-module-raid1` is flake.nix:457;
   `pool-lock-discover-contention` is flake.nix:726 and is correctly unprefixed).
   `raid1.py` (bare `succeed`) and `pool-lock-discover-contention.py` (uses
   `2>&1`, which merges streams) are unaffected by moving a row between stdout
   and stderr; this run confirms no collateral breakage. This is a localized
   change, so a focused run is appropriate -- no full-suite run required.
