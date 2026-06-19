# Plan: validate `braid.mountPoint` at eval instead of escaping at each site

## Context

A security review flagged that `cfg.mountPoint` (`lib.types.path`) is interpolated
**unquoted** into several root-context shell/systemd slots in `modules/braid/storage.nix`.
`types.path` only requires a leading `/` -- it accepts spaces, newlines, and `$(...)`
(verified by `nix eval`: `path.check "/mnt/$(reboot)"` -> `true`). A mount point with a
space silently breaks the generated root scripts at runtime.

The finding proposed escaping each site (`lib.escapeShellArg` in script bodies,
double-quotes in `ExecStart`). Investigation found that approach is the wrong shape for
braid:

- **It misses sites and needs three different mechanisms.** The value reaches a shell
  body (`mountpoint -q`), a systemd exec line (`ExecStart`), **and a tmpfiles rule**
  (`storage.nix:46`, `d ${cfg.mountPoint} 0755 ...`) the finding never mentions -- each
  with different quoting rules.
- **`escapeShellArg` can't neutralize newlines** (nixpkgs#25143), so even after escaping,
  the shell sites stay broken for a newline-bearing value. "Support spaces" would *still*
  need an assertion bolted on top.
- **braid already solved this class with eval-time assertions.** `poolAccessGroup`
  (`options.nix:97`), `autoUnlock.keyDevice` (`options.nix:105`), and
  `fanControl.pwm.platformDevice` (`fan-control.nix`, `[A-Za-z0-9_.-]+`) all validate
  admin-set shell-bound strings at eval. `platformDevice` is the exact precedent: assert
  the charset, then interpolate **unquoted** into a glob -- the assertion is the guard.

**Tension resolved:** `cli/src/mount_check.rs` deliberately decodes kernel octal escapes
and has two tests narrated as *"user configures `braid.mountPoint = "/mnt/storage pool"`"*
plus a UTF-8 path test -- i.e. the Rust layer claims to support spaced/Unicode mount
points while the Nix layer breaks them. There is no VM test, doc, or guide backing that
claim; it is careful-parser instinct narrated as a feature. We resolve the contradiction
toward the simpler, more robust invariant.

**Outcome:** one eval-time assertion makes `mountPoint` provably clean at every consumer
(shell, tmpfiles, systemd, JSON, the Rust CLI) by construction. Bad values fail at
`nixos-rebuild` with a clear message instead of at runtime with a silently broken unit.
No interpolation site changes -- matching the `platformDevice` precedent (assert, then
interpolate unquoted).

## Decision

Add an eval-time assertion constraining `braid.mountPoint` to a **canonical absolute path**
of safe characters. Do **not** quote the interpolation sites; the assertion is the guard.
Keep the Rust `decode_octal_escapes` logic (correct mountinfo parsing, cheap
defense-in-depth); only correct the test *narrations* that claim a spaced/Unicode
`braid.mountPoint` is supported.

**A single charset regex is insufficient.** `/[A-Za-z0-9_./-]+` accepts non-canonical paths
-- `/mnt/../storage`, `/mnt/./storage`, `/mnt//storage` -- that pass eval but break at
runtime: `cli/src/mount_check.rs#find_unique_target_entry` only trims a trailing slash
before comparing the configured target against the kernel's canonical mountinfo path, and
`cli/src/types.rs#MountPoint` (`MountPoint(String)`, no validation -- confirmed) never
normalizes. So `braid.mountPoint = "/mnt/../storage"` mounts at `/storage` while
mount-detection compares against the literal `/mnt/../storage` -> no match -> fail-open
`PoolOffline`. The assertion must be **segment-aware**: reject empty / `.` / `..` segments,
not just bad characters.

Define a helper in the `options.nix` `let` block, asserting over `toString cfg.mountPoint`:

```nix
mountPointOk =
  let
    mp = toString cfg.mountPoint;
    # One optional trailing slash is allowed (mount_check.rs trims it; covered by
    # fstype_at_mount_matches_trailing_slash_target). Everything else must be canonical.
    trimmed = if mp != "/" && lib.hasSuffix "/" mp
              then builtins.substring 0 (builtins.stringLength mp - 1) mp
              else mp;
    segs = lib.splitString "/" trimmed;          # "/mnt/storage" -> [ "" "mnt" "storage" ]
    body = builtins.tail segs;                    # drop the leading ""
    segOk = s: s != "" && s != "." && s != ".." && builtins.match "[A-Za-z0-9_.-]+" s != null;
  in
  lib.hasPrefix "/" trimmed && builtins.head segs == "" && body != [ ] && builtins.all segOk body;
```

```nix
{
  assertion = mountPointOk;
  message = "braid.mountPoint must be a canonical absolute path: segments of letters, digits, '_', '.', '-' separated by single '/', with no empty/'.'/'..' segments, spaces, newlines, or shell metacharacters. Got: '${toString cfg.mountPoint}'.";
}
```

Validated by `nix eval` (`builtins.match` is whole-string anchored). **Accepts**
`/mnt/storage`, `/mnt/storage/` (trailing slash), `/mnt/.snapshots` (hidden segment),
`/mnt/data.backup`. **Rejects** `/mnt/../storage`, `/mnt/./storage`, `/mnt//storage`,
`/mnt/storage//`, root-only `/`, space, newline, `$(...)`, backtick, `;`.

**Sub-decision (ASCII):** the per-segment charset rejects non-ASCII (`/mnt/café` ->
rejected), matching braid's three existing ASCII allowlist assertions. Intended, simplest
choice for an appliance NAS. If Unicode mount points must remain valid, widen the segment
charset -- a deliberate divergence from the sibling pattern, not recommended.

## Changes

### 1. `modules/braid/options.nix` -- add the helper + assertion + tighten the description
- Add the `mountPointOk` helper above to the existing `let` block (it has `cfg` and `lib`
  in scope).
- Append the assertion to the `assertions` list (alongside the existing five).
- Update the `mountPoint` option `description` (currently "Where to mount the btrfs
  pool.") to state the constraint, mirroring how `keyDevice` documents its `/dev/disk/by-id/`
  shape inline.

### 2. `cli/src/mount_check.rs` -- correct the now-false test narrations (keep all code)
The `decode_octal_escapes` function and every assertion stay (still correct parsing).
Only the `/* Scenario: ... */` lines that claim a spaced/Unicode `braid.mountPoint` is a
supported config are reworded, since the module now rejects such values:
- `fstype_at_mount_decodes_octal_escaped_path` (`:345`)
- `mount_entry_at_via_fs_decodes_octal_escaped_path` (`:716`)
- `fstype_at_mount_preserves_non_ascii_utf8_path` (`:453`)

Reframe each scenario from "user configures `braid.mountPoint = "/mnt/storage pool"`" to
"an unrelated mount elsewhere in the table has an escaped/non-ASCII path; the parser must
still decode and compare it correctly" (the parser visits every mountinfo line, so this
remains a real, load-bearing case). Optionally retarget the literal in the two decode
tests to a non-`braid` path (e.g. `/mnt/other backup`) so the narration is honest.

### 3. Eval tests + registration (TDD: write these first, watch them fail)
Follow the existing `eval-lock-systemd-stop-deadline-{ok,fails}` precedent exactly.
- **`tests/eval/_braid-eval-harness.nix`** -- add an optional `mountPoint ? "/mnt/storage"`
  parameter and set `braid.mountPoint = mountPoint;`. Give `lockSystemdStopDeadlineSecs`
  a default (`? 270`) too so each eval test only passes the knob it exercises. Existing
  callers keep working unchanged.
- **`tests/eval/mountpoint-assertion-fails.nix`** -- **table-driven** over a list of
  representative bad values; assert the assertion fires (a `config.assertions` entry with
  `assertion == false` carrying our message) for EVERY one, via `builtins.all` so a value
  that slips past fails the check. Cover each central claim:
  `"/mnt/my storage"` (space), `"/mnt/a\nb"` (newline), `"/mnt/$(reboot)"` (subshell),
  `"/mnt/x;y"` (semicolon; a backtick value is equivalent), `"/"` (root-only),
  `"/mnt//storage"` (empty segment), `"/mnt/./storage"` (`.` segment), `"/mnt/../storage"`
  (`..` segment). Each value is a plain Nix string -- no command runs at eval -- so the
  harness evaluates safely; wrap with `builtins.tryEval` as the lock-deadline test does.
- **`tests/eval/mountpoint-assertion-ok.nix`** -- assert several valid overrides evaluate
  WITHOUT the assertion firing: `/srv/pool`, `/mnt/storage/` (trailing slash),
  `/mnt/.snapshots` (hidden segment).
- **`flake.nix`** -- register both beside the existing eval checks (around `flake.nix:893`),
  e.g. `eval-mountpoint-rejects-bad-chars` and `eval-mountpoint-accepts-valid`.
- Each new `.nix` test file opens with a contiguous block of `#` line comments (Intent /
  Why it exists / Scenario), matching the existing eval tests
  (`tests/eval/nixos-module-default-package.nix`, `version-matches-cargo.nix`). Note:
  `docs/dev/testing.md` phrases the rule as "`//` line comments," but `//` is Nix's
  attrset-merge operator, not a comment -- `.nix` files use `#`, as those eval tests do.

### 4. Docs -- record the new invariant (mandatory)
Adding a charset/canonical-path constraint is a new safety invariant; per AGENTS.md, an
invariant change must update `principles.md` + the owning ADR. These are **not optional** --
leaving them out invites future code/docs to reassert arbitrary-path support.
- `docs/design/decisions/028-immutable-unmounted-mountpoint.md` -- **mandatory.** Add the
  constraint to the static-mountpoint discussion. Tightest home: the `### 3. Seal from the
  boot/activation unit ONLY` bullet that already names the `d ${cfg.mountPoint}` tmpfiles
  rule ("The mountpoint is static and pre-exists every pool") -- note that the eval
  assertion is what makes interpolating `cfg.mountPoint` **unquoted** into that tmpfiles
  rule (and the scrub/seal plumbing) safe. The `### Static-vs-dynamic mountpoint
  distinction` subsection is an acceptable alternative location.
- `docs/design/principles.md` -- **mandatory.** Under `## 3. Safe-by-construction
  operations`, extend the existing sealed-mountpoint bullet ("The bare pool mountpoint is
  sealed immutable...") with a clause that `cfg.mountPoint` is constrained at eval to a
  canonical, whitespace/metacharacter-free path, so every consumer (shell, tmpfiles,
  systemd, JSON, the Rust CLI) interpolates it unquoted safely.
- `docs/guides/nixos-configuration.md` -- the options table row for `braid.mountPoint`
  (note: canonical absolute path, no whitespace/metacharacters).
- No change needed to `getting-started.md` / `README.md` (they show only the default).

## Out of scope
- **No quoting** of the shell/systemd/tmpfiles interpolation sites -- the assertion makes
  them safe, consistent with the `platformDevice` precedent. Adding partial quoting would
  create an inconsistent "why is this one quoted and not that one" surface.
- **No removal** of the Rust octal-decode logic -- it is correct mountinfo parsing and
  defense-in-depth.
- Findings 2-8 in `findings/6-nix-module-shell-scripts.md` (alertCommand, set -o pipefail,
  sandboxing, etc.) are separate items, not this plan.

## Verification
- `nix eval --impure` of `mountPointOk` against the good/bad matrix (done during planning:
  the canonical cases `/mnt/../storage`, `/mnt/./storage`, `/mnt//storage`, `/mnt/storage//`
  all reject; `/mnt/storage`, `/mnt/storage/`, `/mnt/.snapshots` accept).
- `nix build .#checks.<system>.eval-mountpoint-rejects-bad-chars` and
  `...eval-mountpoint-accepts-valid` -- the table-driven fails-test must fail before the
  assertion exists (TDD red), pass after (green).
- `just test-rust` -- confirms the reworded `mount_check.rs` tests still pass (code unchanged,
  only comments/literals touched).
- `just docs-build` -- `mdbook-linkcheck2` validates the doc edits.
- Manual eval sanity: configs with `braid.mountPoint = "/mnt/my storage"` AND
  `"/mnt/../storage"` must both fail `nixos-rebuild` with the new message.
