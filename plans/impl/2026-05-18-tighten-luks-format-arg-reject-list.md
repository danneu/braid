# Plan: tighten `--luks-format-arg` reject list

## Context

`braid add` and `braid replace` accept `--luks-format-arg=<token>` to pass raw
argv extras into `cryptsetup luksFormat`. The CLI flag is intentionally
permissive ("Advanced: pass one raw argv element", `cli/src/main.rs:162`), so
the validation seam is `LuksFormatExtraOpts::parse` in `cli/src/types.rs`. The
current seam rejects only `--uuid`/`--label` -- the two tokens braid emits
itself in `cmd.rs:792-811`.

That reject list is insufficient. cryptsetup exposes other `luksFormat`
options that silently break braid's storage model when passed through extras:

| Flag | Short | Effect when sneaked in |
|---|---|---|
| `--header <path>` | -- | Writes the LUKS header to a separate file; the data device has no on-disk header, so every later braid probe sees "not LUKS" and the disk is effectively unrecoverable through braid. |
| `--type luks1` | `-M` | LUKS1 has no label field; the `braid-<name>` label invariant `validate_braid_preconditions` (`cli/src/add.rs:140-147`) depends on is silently unsatisfiable. |
| `--key-file=<path>` | `-d` | Collides with braid's already-piped passphrase stdin (`--key-file=-` at `cmd.rs:798`); operator's expected passphrase flow is broken. |
| `--key-slot N` | `-S` | Perturbs braid's slot-0-passphrase / slot-1-`braid.key` invariant. |
| `--master-key-file` / `--volume-key-file` | -- | Pre-determines the master key; defeats braid's entropy contract. |
| `--integrity <alg>` | `-I` | Turns on LUKS2 inline integrity (dm-integrity superblock) which braid does not model. |
| `--integrity-*` accessory family | -- | Same family as above; only meaningful alongside `--integrity`. |
| `--keyfile-offset N` | -- | Skips the first N bytes of the keyfile when reading. Because braid pipes the passphrase via `--key-file=-`, this silently truncates the passphrase from the front. Slot-0 ends up keyed to a different string than the operator typed, and the per-format invariant pinned by `cmd.rs:2923-2961` (`cryptsetup_luks_format_omits_keyfile_size`) is bypassed. |
| `--keyfile-size N` | `-l` | Limits the keyfile read to N bytes. Same passphrase-truncation hazard as `--keyfile-offset`, but from the back. `cmd.rs:2923-2961` already pins the assertion that braid's `CryptsetupLuksFormat` MUST NOT carry `--keyfile-size`; allowing it via extras would silently break that. |

Short aliases (`-d`, `-S`, `-M`, `-I`, `-l`) bypass any long-form-only check.
popt also supports short-option *clusters* where toggle shorts (e.g. `-q`,
`OPT_BATCH_MODE` at `cryptsetup_arg_list.h:17`) can lead a value-taking
short -- so `-qMluks1` parses as `-q -M luks1`, slipping past any rule that
only checks the token's leading letters. The validator has to scan the whole
short cluster, not just the prefix.

The existing rationale comment on `is_managed_format_flag`
(`cli/src/types.rs:291-294`) explicitly waives short-form handling on a "no
short alias exists" claim that does not hold for the new entries.

## Goal

Extend the single chokepoint `is_managed_format_flag` so a `--luks-format-arg`
carrying any of the listed flags errors as
`AddError::ManagedFormatFlag` / `ReplaceError::ManagedFormatFlag` before any
probe, journal write, or `CryptsetupLuksFormat` request. Pin the new coverage
in unit tests next to the validator and extend the existing plumbing tests in
`add.rs` and `replace.rs` so the same matrix is exercised end-to-end.

## Critical files

| File | Lines | Role |
|---|---|---|
| `cli/src/types.rs` | 243-300 | `LuksFormatExtraOpts` + `is_managed_format_flag` (the only validator -- shared by add/replace) |
| `cli/src/types.rs` | 691-754 | Existing per-token unit tests; extend in-place |
| `cli/src/add.rs` | 8542-8609 | `add_rejects_managed_luks_format_args` -- existing loop test; broaden the loop |
| `cli/src/replace.rs` | 5872-5938 | `plan_replace_rejects_managed_format_flag` -- existing single-case test; promote to a loop to match add.rs |
| `cli/src/cmd.rs` | 786-816 | Read-only context for argv ordering (no changes) |
| `reference/cryptsetup/src/cryptsetup_arg_list.h` | 59-249 | Source of truth for flag names and short aliases |

No new file is created. No call sites change; both `add.rs:1434-1441` and
`replace.rs:1136-1141` already route through the same `LuksFormatExtraOpts::parse`.

## Design

### Reject list (long-form)

Add to the existing `--uuid` / `--label` rejection (bare and `--flag=...`
forms each):

- `--header`
- `--key-file`
- `--master-key-file`
- `--volume-key-file` (canonical name `--master-key-file` aliases)
- `--key-slot`
- `--type`
- `--integrity`
- `--integrity-key-size`
- `--integrity-inline`
- `--integrity-no-journal`
- `--integrity-no-wipe`
- `--integrity-legacy-padding`
- `--keyfile-offset`
- `--keyfile-size`

The `--integrity-*` accessory family is included because the accessory flags
only make sense alongside `--integrity` itself, and rejecting them up-front
avoids a future hole if cryptsetup ever accepts them standalone.
`--keyfile-offset` / `--keyfile-size` are included because cryptsetup applies
both to the `--key-file=-` stdin path braid relies on, so either flag would
silently change the slot-0 material relative to what the operator typed.

### Reject list (short aliases)

The new flags add five short aliases cryptsetup accepts: `-d` (key-file),
`-S` (key-slot), `-M` (type), `-I` (integrity), `-l` (keyfile-size). popt
accepts each short in several forms -- as a bare token (`-M`), concatenated
with its value (`-Mluks1`), with an equals sign (`-M=luks1`), and -- the
case the previous draft missed -- as part of a *short-option cluster* where
one or more toggle shorts (e.g. `-q`, `-v`) precede the value-taking short.
So `-qMluks1` parses as `-q -M luks1`, and a rule like `starts_with("-M")`
sees the leading `-q` and lets it through.

The accessory `--integrity-*` family and the `--header` / `--master-key-file`
/ `--volume-key-file` / `--keyfile-offset` entries have no short aliases per
`cryptsetup_arg_list.h`, so they need only their long form matched.

### Validator shape

`is_managed_format_flag` becomes a single function that returns true if any
of:

1. The token is in the long-form set above (bare, or starts with `<flag>=`).
2. The token starts with `-` (single hyphen, not `--`) and contains *any*
   character from `{d, S, M, I, l}` in the short-option cluster -- defined
   as the substring from after the leading `-` up to (but not including)
   the first `=` or end of token. Case-sensitive. This collapses bare
   (`-M`), concatenated (`-Mluks1`), equals (`-M=luks1`), and clustered
   (`-qMluks1`, `-vIhmac-sha256`, `-ql16`) forms into one rule and means a
   future toggle short that cryptsetup adds cannot smuggle a disallowed
   value-taking short in behind it.

Implementation pattern: keep the function as a free function next to
`LuksFormatExtraOpts::parse`. Drive the long-form check from a `&'static
[&'static str]` constant so the listing is a single source of truth and the
unit tests can iterate it. The short-cluster scan is a small inline check
(a `chars().any(|c| matches!(c, 'd' | 'S' | 'M' | 'I' | 'l'))` over the
pre-`=` slice).

### Doc / comment updates

- The existing rationale comment on `is_managed_format_flag`
  (`cli/src/types.rs:291-294`) is stale: "no short alias exists" no longer
  holds. Replace with a comment explaining (a) which long-form flags are
  managed-by-braid vs storage-model-breaking, (b) why the short-option
  side scans the whole cluster (popt allows toggle shorts like `-q` to
  lead a value-taking short, e.g. `-qMluks1` parsing as `-q -M luks1`),
  and (c) the link to `cryptsetup_arg_list.h` as the source of truth.
- The `LuksFormatExtraOpts` doc (`cli/src/types.rs:247-250`) and the
  `LuksFormatExtraOptsError` `#[error(...)]` text (`cli/src/types.rs:259-262`)
  hard-code the parenthetical `(--uuid, --label)` and the rationale "braid
  sets these itself and rejects user-supplied overrides". Both become stale
  with the extended list (e.g. `--header` is not "set by braid", it's
  refused because it breaks the on-disk model). Replace the parenthetical
  with a category-level summary ("braid-managed identity or
  storage-model-breaking cryptsetup options") and drop the
  "braid sets these itself" half of the rationale, since it no longer
  covers all rejected entries. The existing
  `add_rejects_managed_luks_format_args` test at `cli/src/add.rs:8542` only
  asserts the error variant, not the message text, so message edits do not
  cascade. The six existing per-token unit tests in `types.rs`
  (`luks_format_extra_opts_rejects_uuid_equals` etc.) DO assert the message
  text -- those assertions must be updated to match the new wording.
- The function name `is_managed_format_flag` and the error variant
  `ManagedFormatFlag` remain unchanged. They're slightly imprecise (some new
  entries aren't "managed", they're "disallowed") but renaming ripples
  through `AddError`, `ReplaceError`, two test names, and several call-site
  doc comments for purely cosmetic gain. Out of scope.

### Tests

#### `cli/src/types.rs` -- unit tests (lines 691-754, extend)

Existing tests are per-token, one function per form. To avoid a 30-function
expansion, drive the new rejections from one loop test plus a second loop
test for short-alias forms.

- `luks_format_extra_opts_rejects_long_form_set` -- iterate over each new
  long-form flag (including `--keyfile-offset` and `--keyfile-size`) in
  both bare and `=value` form, assert
  `LuksFormatExtraOptsError::ManagedFormatFlag` and that the error message
  echoes the offending token verbatim.
- `luks_format_extra_opts_rejects_short_aliases` -- iterate over `-d`, `-S`,
  `-M`, `-I`, `-l` in bare (`-d`), concatenated (`-dpath`), equals
  (`-d=path`), and clustered (`-qMluks1`, `-vIhmac-sha256`, `-ql16`) forms.
  Same assertion shape. The clustered cases are the regression pin against
  any future refactor that collapses the cluster-scan back to a prefix
  check.
- Keep the existing six `--uuid`/`--label` tests untouched (regression pins
  for the original contract).

#### `cli/src/add.rs::add_rejects_managed_luks_format_args` (line 8560, extend)

The loop currently iterates over four `--uuid` / `--label` variants. Add a
sample of each new category to prove the plumbing routes them all through
`AddError::ManagedFormatFlag`:

- `--header`, `--header=/tmp/x`
- `--type=luks1`
- `--key-file=/dev/null`
- `--key-slot=2`
- `--integrity=hmac-sha256`
- `--keyfile-offset=64`
- `--keyfile-size=16`
- `-M` (bare short-alias smoke)
- `-qMluks1` (clustered short-alias smoke)

Per-token form coverage (bare vs equals vs concatenated short) lives in
`types.rs` unit tests -- the `add.rs` test only needs to prove the plumbing
fires.

#### `cli/src/replace.rs::plan_replace_rejects_managed_format_flag` (line 5884, broaden)

Currently a single `--label=foo` case. Promote it to the same loop shape as
`add.rs::add_rejects_managed_luks_format_args` with one entry per new
category (same list as above). This restores the parity between add and
replace test coverage that the existing single-case version doesn't enforce.

## Out of scope

- Allowlist pivot (only allow `--pbkdf*`, `--cipher`, `--key-size`, `--hash`,
  `--use-random`, `--use-urandom`, etc.). More robust against future
  cryptsetup additions but breaks the documented "Advanced: pass one raw
  argv element" contract and rejects legitimate operator tuning. The
  denylist matches existing intent.
- Renaming `is_managed_format_flag` / `ManagedFormatFlag` variants for
  semantic precision. Cosmetic; multi-file ripple.
- Defense in depth at the executor seam (`cmd.rs::CryptsetupLuksFormat`
  assembly). Validation already lives at the parse boundary; a second seam
  would duplicate the rule with no new information.
- (Previously listed here: "Mid-format escape via short-option chaining".
  Now in scope -- handled by the cluster-scan rule in the validator
  shape section, with `-qMluks1`-style cases pinned in `types.rs` unit
  tests.)

## Verification

1. `just test-rust` -- unit tests in `types.rs` plus the `add.rs` and
   `replace.rs` plumbing loops cover the matrix.
2. Targeted scan: run `cargo test -p braid-cli luks_format_extra_opts`
   (the actual test prefix used by the unit tests in `types.rs`) and
   inspect output to confirm the new `_rejects_long_form_set` and
   `_rejects_short_aliases` tests appear and pass.
3. Manual cross-check against
   `reference/cryptsetup/man/cryptsetup-luksFormat.8.adoc` to confirm flag
   names and short aliases match the pinned cryptsetup.
4. No VM tests required -- the change is at the parse boundary, before any
   shell invocation. `just test-vm` would not exercise new behavior.

## Risks

- The cluster-scan rule rejects a token if *any* character in the short
  cluster is in `{d, S, M, I, l}`. That is broader than the literal
  cryptsetup alias set: a future cryptsetup short like `-ld` (hypothetical)
  combining unrelated meanings would be rejected. In practice cryptsetup
  short letters do not collide with one another and the rejected set
  exactly tracks the disallowed flags today; if a future cryptsetup
  release introduces a new value-taking short whose letter overlaps a
  legitimate operator-tuning intent, the operator-facing remedy is "use
  long form for `--luks-format-arg`", which the CLI doc already
  encourages by example. The constant `&[char]` of disallowed cluster
  letters in `types.rs` is the one place to update when the upstream set
  shifts.
- Error messages echo the offending token verbatim. None of the rejected
  values are sensitive (passphrases never reach `--luks-format-arg`), so
  no redaction needed.
