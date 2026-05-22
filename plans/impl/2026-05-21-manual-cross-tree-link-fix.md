# Fix source-relative cross-tree links in mdBook manual

## Context

Commit `ac27540 docs(idle): pin multi-btrfs rule in manual` shipped
the absolute-GitHub-URL form for an ADR reference in
`manual/commands/idle.md`, with the rationale captured in
`plans/impl/2026-05-21-idle-multi-btrfs-manual-rule.md` under "Link
form rationale": `manual/book.toml` sets `src = "."`, mdBook only
remaps `.md` -> `.html` inside its src root, and `manual/SUMMARY.md`
does not include `docs/` files, so any `../../docs/X.md` link
renders to `../../docs/X.html` in `manual/book/` -- a path that
does not exist and 404s on the deployed `/braid/` site.

That plan's Follow Up called for a consistency pass to swap any
remaining instances of the same bug class. The bug class has now
recurred 5 times across two passes (1 fixed in `idle.md`, 4
flagged here); nothing currently catches it automatically. The
existing `just check-docs` recipe (`justfile:214`) only verifies
SUMMARY.md / disk parity, the CI `docs.yml` workflow runs only
`mdbook build` (which does not validate cross-tree HREFs), and the
mdBook config has no link-lint hook. A future contributor adding
another `[X](../../docs/...)` link will silently reintroduce the
broken-rendering pattern.

The plan therefore makes two coupled changes: the link fixes
themselves, and a regression gate so the bug class fails the
existing `just check-docs` recipe on the next addition.

An audit of the entire `manual/` tree found 4 instances of the
bug class across 2 files:

- `manual/guides/ups.md:171` -- ADR 020 (UPS integration), inline
  body reference
- `manual/guides/ups.md:179` -- same ADR 020, second mention in the
  "Related" list
- `manual/commands/status.md:183` -- `docs/luks-unlock.md` with
  anchor `#unparseable-state-file-reconciliation`
- `manual/commands/status.md:200` -- `docs/luks-unlock.md` with
  anchor `#header-backup-workflow-and-messaging`

Both target files exist at the repo root
(`docs/decisions/020-ups-integration.md`, `docs/luks-unlock.md`),
and both anchor fragments in `status.md` correspond to real H2
headings (`docs/luks-unlock.md` lines 143 and 123). No content
changes are needed in the linked docs; only the link form changes.

Intended outcome: every cross-tree link in `manual/` resolves
correctly in both GitHub source view and the rendered/deployed
mdBook output, matching the precedent committed in
`manual/commands/idle.md`.

## Change

Two coupled scopes: four link-target substitutions across two
manual pages, and one extension to the existing `just check-docs`
recipe to catch the bug class on future additions.

### Link substitutions (4 instances)

Four mechanical link-target substitutions, all of the same shape:

```
[<text>](../../docs/<path>[#anchor])
  -> [<text>](https://github.com/danneu/braid/blob/master/docs/<path>[#anchor])
```

Link text and any `#anchor` fragment are preserved verbatim; only
the URL form changes. Pattern matches the precedent for ADR 016 in
`manual/commands/idle.md`.

### File: `manual/guides/ups.md`

- **Line 171** (inline body, "See ..." paragraph):

  Old: `[decisions/020-ups-integration.md](../../docs/decisions/020-ups-integration.md)`

  New: `[decisions/020-ups-integration.md](https://github.com/danneu/braid/blob/master/docs/decisions/020-ups-integration.md)`

- **Line 179** (under `## Related`, first bullet):

  Old: `[ADR: UPS integration](../../docs/decisions/020-ups-integration.md)`

  New: `[ADR: UPS integration](https://github.com/danneu/braid/blob/master/docs/decisions/020-ups-integration.md)`

### File: `manual/commands/status.md`

- **Line 183** (after the "warning: failed to parse pending-op.json"
  code block):

  Old: `[Unparseable state-file reconciliation](../../docs/luks-unlock.md#unparseable-state-file-reconciliation)`

  New: `[Unparseable state-file reconciliation](https://github.com/danneu/braid/blob/master/docs/luks-unlock.md#unparseable-state-file-reconciliation)`

- **Line 200** (after the "Pending LUKS header backups" paragraph):

  Old: `[LUKS header backup workflow](../../docs/luks-unlock.md#header-backup-workflow-and-messaging)`

  New: `[LUKS header backup workflow](https://github.com/danneu/braid/blob/master/docs/luks-unlock.md#header-backup-workflow-and-messaging)`

### Regression check: extend `just check-docs`

Extend the existing recipe at `justfile:214` so it also fails when
any markdown link in `manual/` escapes the mdBook src root. The
bug class is "link target escapes `manual/`"; the concrete pattern
is `](../../` in a `.md` file under `manual/`. The check should be
broader than the historical `../../docs/` destination so a future
`../../README.md` or `../../cli/...` also trips it -- the
underlying mdBook rendering failure is identical.

Implementation shape (a second pass added to the existing recipe,
sharing the recipe's `rc` accumulator):

```bash
# Markdown links that escape manual/ (rendered-broken in mdBook output).
# Use absolute https://github.com/danneu/braid/blob/master/<path> URLs
# instead -- see manual/commands/idle.md for the precedent.
escapes=$(grep -rn '\](\.\./\.\./' manual/ --include='*.md' || true)
if [ -n "$escapes" ]; then
    printf 'markdown links escape manual/ subtree (broken in rendered mdBook):\n'
    printf '%s\n' "$escapes"
    printf 'fix: replace with https://github.com/danneu/braid/blob/master/<path>\n'
    rc=1
fi
```

Add the block before the existing recipe's `if [ $rc -eq 0 ]; then
echo "SUMMARY.md is in sync"; fi` tail. Update the recipe's success
message to reflect the broader scope -- e.g. swap the final echo for
`echo "docs check ok"` so a passing run no longer falsely advertises
SUMMARY.md-only coverage.

Also update the recipe's leading `# Verify SUMMARY.md and manual
pages are in sync` comment to describe the broader scope, e.g.
`# Verify SUMMARY.md parity and manual link integrity`.

## Files Reviewed but Not Changed

- `manual/SUMMARY.md` -- audit confirmed `docs/decisions/*` and
  `docs/luks-unlock.md` are not listed. Adding them would dissolve
  the link-form problem entirely but is a much larger architectural
  change (out of scope, per the prior plan's same call).
- `manual/book.toml` -- `src = "."` confirmed. No change.
- `manual/commands/status.md:180` -- a fenced code block quoting a
  CLI-emitted error message that includes the literal string "see
  docs/luks-unlock.md". This is program output, not a markdown
  link, and targets a reader looking at a checked-out repo. Do not
  rewrite CLI output text from a manual edit.
- `manual/commands/discover.md:70` -- inline text "see
  `docs/luks-unlock.md`" inside backticks. Not a markdown link;
  the backticks render as code, not an HREF, so mdBook never
  resolves it as a URL and the rendering bug does not apply.
- `manual/guides/recovery-scenarios.md:140` -- entire line is a
  backticked CLI string `Remove /var/lib/braid/pending-op.json
  after manual reconciliation (see docs/luks-unlock.md) and
  re-run.` quoted as a code snippet. Same rationale as
  `discover.md:70`; not a markdown link.
- All other manual links -- audit confirmed only the 4 instances
  above escape `manual/`. Intra-`manual/` links resolve correctly
  under mdBook because they do not escape the src root.

## Files Changed

- `manual/guides/ups.md` -- two link-target substitutions (lines
  171 and 179).
- `manual/commands/status.md` -- two link-target substitutions
  (lines 183 and 200).
- `justfile` -- extend the `check-docs` recipe at line 214 with a
  link-escape grep pass and update the recipe's leading comment +
  success message to reflect the broader scope.

## Verification

1. **Recipe is the regression gate.** Run `just check-docs`. The
   recipe must:
   - Pre-link-edit, post-recipe-edit (apply the justfile change
     first): exit non-zero and list the 4 instances above. This
     proves the new check actually catches the bug class -- a
     stubbed check that always returns 0 would silently pass.
   - Post-link-edit, post-recipe-edit (apply both): exit 0 and
     print the updated success message.

2. **Rendered view.** Run `just docs` (the `nix run nixpkgs#mdbook
   -- serve manual --open` recipe at `justfile:210`). In the
   rendered book:
   - `guides/ups.html` -- click both ADR 020 links (one in the body
     paragraph, one in the "Related" list); each must resolve to
     `docs/decisions/020-ups-integration.md` on GitHub, not a 404.
   - `commands/status.html` -- click both `docs/luks-unlock.md`
     links; each must resolve to the correct H2 anchor on GitHub
     (`#unparseable-state-file-reconciliation` and
     `#header-backup-workflow-and-messaging`).

   The recipe in step 1 catches escapes from `manual/` but cannot
   verify that the rewritten absolute URLs actually point at real
   GitHub pages -- this click-through is what catches a typo in
   the replacement URL or anchor fragment.

3. **Source-link sanity.** Confirm each replaced URL resolves with
   `curl -I` for HTTP 200, or open in a browser. The repo is
   private per `AGENTS.md`, so the check requires an authenticated
   session.

No tests added. The bug class is mdBook rendering, not runtime
behavior; there is no Rust or VM test that observes it. The
extended `just check-docs` recipe is the regression gate going
forward.
