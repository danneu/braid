# Document the command-error-prefix convention

## Context

A review finding flagged `RemoveMissingError::Pool` (`cli/src/remove_missing.rs#RemoveMissingError`)
for producing a "doubled prefix": `error: pool error: btrfs device remove failed ...`.
It claimed this was an outlier ("no other braid error variant emits it at the top
level") and proposed changing that one variant to `#[error("{0}")]`.

Investigation inverted the premise:

- The `<subsystem> error: {0}` prefix is the **dominant, intentional convention** --
  **45 variants across 16 files** (`add` 6, `replace` 6, `recover` 5, `ack` 4,
  `enroll_key_file` 4, `status`/`luks` 3, `remove`/`remove_missing`/`probe`/`pool_lock`
  2 each, plus `scrub_*`, `ups`, `discover`). Every command error reaches
  `print_cli_error` (`cli/src/main.rs#print_cli_error`) via `.to_string()`, which
  prepends the single `error: ` marker -- so `add`, `remove`, `replace`, `status`,
  etc. all render `error: <subsystem> error: ...`.
- `remove_missing` itself has a sibling `Probe` variant (`probe error: {0}`) with the
  identical shape, and `remove.rs` carries the exact same `Probe`/`Pool` pair.
- The subsystem tag carries real triage value: in the multi-step commands it
  disambiguates up to six failure domains (probe/luks/pool/command/parse/membership),
  which the inner message does not always self-identify (`parse error: ...`,
  `membership error: ...`).
- The untagged variants are the **exceptions**, not the rule, and they split two ways:
  terminal hand-authored messages (`Validation(String)`, `NoMemberForDevid`) and
  deliberate transparent passthroughs of an already-user-facing sub-error
  (`AddError::ManagedFormatFlag`, `RecoverError::Mount`). The dividing line is *role* --
  does the message already locate the failure on its own? -- not whether the variant
  uses `#[from]` (some `#[from]` variants pass through transparently) or `#[error("{0}")]`
  (`NoMemberForDevid` is terminal yet carries full custom wording).

So the finding's scoped patch would have **created** the project-fit inconsistency it
claimed to remove (breaking `Pool` from its own sibling `Probe` and from the whole
add/remove/replace/... family). The real root cause is that this pervasive convention
is **undocumented**, which let a reviewer misread it as an accident.

**Intended outcome:** codify the convention in one place so this class of false finding
stops recurring, and so nobody "fixes" the doubling on a single variant. No code change;
CLI output is unchanged.

## Change

Add one bullet to the `## Conventions (always)` section of `AGENTS.md`, immediately
after the existing **CLI output is ASCII only** bullet (both are user-facing-output
conventions, so they belong together; insert between current lines 55 and 56):

```markdown
- **Command error prefixes.** In command-level error enums (e.g.
  `cli/src/add.rs#AddError`), variants are tagged by *role*, not by syntax. A
  subsystem-wrapper variant -- whose inner error wouldn't reveal which braid layer
  produced it -- gets a `<subsystem> error:` tag (`probe error:`, `pool error:`,
  `luks error:`, `command error:`, `parse error:`, `membership error:`) so the operator
  can see which layer failed; `print_cli_error` (`cli/src/main.rs#print_cli_error`) then
  prepends the single `error: ` marker, so output reads `error: <subsystem> error: ...`
  (the doubling is intentional). A variant whose message already stands alone gets no
  tag -- both terminal refusals with full hand-authored wording (e.g.
  `cli/src/remove_missing.rs#RemoveMissingError::NoMemberForDevid`, which is not
  `#[error("{0}")]`) and deliberate transparent passthroughs of an already-user-facing
  sub-error (e.g. `cli/src/add.rs#AddError::ManagedFormatFlag`,
  `cli/src/recover.rs#RecoverError::Mount`, both `#[from]` + `#[error("{0}")]`). Tagging
  is per-role and codebase-wide; don't flip one variant to change the doubling.
```

Match house style: bold lead, terse body, ASCII only, `path#symbol` citations as code
spans (never line numbers).

## Explicitly out of scope (do NOT do)

- **Do not apply the finding's patch** (changing `RemoveMissingError::Pool` to
  `#[error("{0}")]`). It would make that variant inconsistent with its sibling and the
  other 44 variants.
- **Do not reformat the 45 variants** codebase-wide. The subsystem tag (the valuable
  part) already exists; shaving the redundant word would be high-churn cosmetic change
  and a new convention to maintain. (This was the rejected alternative.)
- **Do not add a prefix-asserting test.** No test currently pins these prefixes, and a
  `.contains("pool error:")`-style assertion is structure-sensitive -- it pins cosmetic
  wording rather than behavior. Adding one is an anti-goal.

## Files modified

- `AGENTS.md` -- one new bullet in `## Conventions (always)`. (Single-file, doc-only.)

## Verification

- `just check-doc-links` -- the AGENTS.md/README.md-scoped CI guard (`checks.yml`
  `doc-links` job; `scripts/docs/check-doc-links.py`); confirms the edit breaks no
  `](...)` markdown links in `AGENTS.md`. By design it does not validate backticked
  `path#symbol` code spans, so those are checked manually.
- Manual: bullet is ASCII-clean (`--`, straight quotes, `...`, `x`), and every
  source-symbol citation resolves -- e.g. `grep -n 'fn print_cli_error' cli/src/main.rs`,
  `grep -n 'enum AddError' cli/src/add.rs`, plus the `NoMemberForDevid`,
  `ManagedFormatFlag`, and `RecoverError::Mount` variants named in the bullet. (No CI
  guard covers source-file code-span cites; `check-code-doc-anchors.py` validates only
  `docs/*.md#anchor` citations.)
- CLI behavior is unchanged, so the existing suite (incl.
  `cmd_remove_missing_failure_emits_missing_replace_hint`,
  `enospc_hint_surfaces_through_error_chain`) stays green by construction.
