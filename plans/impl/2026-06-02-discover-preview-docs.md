# Fix: stop braid docs presenting bare `discover` as a routine read-only preview

## Context

`docs/commands/discover.md` opens by selling bare `braid discover` (no
`--write`) as a general read-only preview/verify tool. It is not. Bare
`discover` prints membership rows **only when `pool.json` is missing**:

- `Missing` -> scans and prints preview rows (the documented example).
- `ValidUuidKeyed` -> refuses, exit 1
  (`"pool.json already exists ... discover is for rebuilding missing or
  corrupt pool state"`).
- `Corrupt` -> refuses, exit 1, pointing to `discover --write`.

Code path: `cli/src/main.rs:927-932` gates the no-`--write` arm through
`check_pool_json_for_bare_discover`, which returns `Err` for both
`ValidUuidKeyed` and `Corrupt` (`cli/src/discover.rs:571-581`, classifier
at `255-265`, error strings at `225-234`).

So a reader on a **healthy** NAS who follows line 11 ("verify which
braid-labeled LUKS devices the system can see") or copies the "Basic
example" gets exit 1 and a refusal -- the page promises a capability the
command does not offer. This is the High-severity case the finding flags.

The rest of the page is already correct: "What happens under the hood"
step 2 (line 61), "Safety checks" bullet 1 (line 75), and "Related
commands" (line 88, already links `status.md`) all document the refusal.
Within `discover.md` the defect is isolated to the two top sections, and
this change brings the intro/example into line with the sections that are
already accurate.

The same trap recurs, milder, in `docs/guides/troubleshooting.md`: its
"Pool won't mount" recipe leads with bare `sudo braid discover` under a
combined "missing or corrupted" symptom, and the corrupt-needs-`--write`
correction sits in a Note *after* the command block. A corrupt-state
reader runs the bare command first and hits the same exit-1 refusal. This
plan fixes both files so the user-facing docs tell one story.

### Correction to the original finding's prescribed wording

The finding said to reframe the example as the "missing/**corrupt**"
rebuild preview. That would reintroduce a smaller version of the same
bug: bare `discover` does **not** preview a corrupt file -- it refuses and
tells you to run `discover --write`. The bare preview is the **missing**
case only. The plan below scopes it correctly.

## Scope

- **In scope:**
  - `docs/commands/discover.md` -- top two sections ("When to use it",
    "Basic example"). Edits 1-2.
  - `docs/guides/troubleshooting.md` -- the "Pool won't mount" recipe
    (lines 40-69): split missing vs corrupt before the command block.
    Edit 3.
- **Out of scope (verified, no change needed):**
  `docs/guides/recovery-scenarios.md` -- its "Lost pool.json" walkthrough
  (line 20) is explicitly the *missing* case ("pool.json does not exist")
  and routes the corrupt case through `discover --write` in its Notes
  (line 74), so bare `discover` is never offered for corrupt state -- no
  trap. `README.md` / `docs/index.md` table rows ("Scan ... and rebuild
  pool.json") are accurate.
- **Do not touch:** the `BareDiscoverError` / `DiscoverWriteError`
  strings in `cli/src/discover.rs` -- they are pinned by unit tests and by
  `tests/cli/braid-discover.py`. We are changing doc prose only; no test
  pins the doc prose (confirmed).

## Change

File: `docs/commands/discover.md`. Intro (line 5) stays as-is -- it
already calls discover a "repair tool for recovering a lost or corrupt
`pool.json`".

### Edit 1 -- "When to use it" (lines 7-13)

Drop the misleading third bullet and rewrite the closing sentence so it
(a) states the refusal and (b) redirects the dropped intent to
`braid status`.

Replace:

```
- Your `pool.json` was deleted or corrupted.
- You're migrating disks to a new machine and need to rebuild pool state.
- You want to verify which braid-labeled LUKS devices the system can see.

The normal path for adding disks is `braid add`. Use `discover` when `pool.json` is missing or corrupt.
```

with:

```
- Your `pool.json` was deleted or corrupted.
- You're migrating disks to a new machine and need to rebuild pool state.

The normal path for adding disks is `braid add`. Use `discover` only when `pool.json` is missing or corrupt -- it refuses to run while a valid `pool.json` exists. To see the disks already in a healthy pool, use [`braid status`](status.md).
```

(Keep `braid add` as inline code, not a link, matching the current page
and avoiding an unverified `add.md` link target.)

### Edit 2 -- "Basic example" (lines 17, and add a caveat after the output block)

Scope the lead-in to the missing case, and add one caveat paragraph after
the output block (before the existing stdout/stderr paragraph at line 31).

Replace the lead-in line:

```
Preview discovered membership (no changes):
```

with:

```
When `pool.json` is missing, preview the membership `discover` would rebuild before saving it (no changes):
```

The `sudo braid discover` block and the example output block stay
unchanged. Immediately **after** the closing ``` of the output block
(current line 29) and before the existing "The membership rows are
written to stdout..." paragraph, insert:

```
Bare `discover` prints this preview only when `pool.json` is absent. Over a valid `pool.json` it exits with an error -- use [`braid status`](status.md) to view current membership. Over a corrupt `pool.json` it also refuses, pointing you to `discover --write` (see [Common variations](#common-variations)).
```

### Edit 3 -- `troubleshooting.md` "Pool won't mount" recipe (lines 40-69)

Split the recipe by `pool.json` state *before* the command block so a
corrupt-state reader never runs bare `discover`. The old standalone
**Note:** (corrupt -> `--write`) is absorbed into the corrupt branch --
do not keep a separate Note. Convert the em-dash in the in-block comment
to `--`. Keep the "healthy `pool.json` refuses on purpose" paragraph and
its `mv` block (current lines 61-69) unchanged.

Replace the current **Fix:** line, the single combined command block, the
"`discover` scans ..." sentence, and the **Note:** block (lines 44-59)
with:

````
**Fix:** Rebuild UUID-keyed pool.json from disk labels and LUKS UUIDs. How you
start depends on the state of `pool.json` -- bare `discover` previews only when
the file is absent; over a corrupt file it refuses and points you to
`discover --write`.

**If `pool.json` is missing** -- preview, then write:

```sh
sudo braid discover
# Shows discovered disks -- verify they look correct
sudo braid discover --write
```

**If `pool.json` is corrupt or unreadable** -- skip the preview and rebuild in
place (bare `discover` refuses corrupt state before scanning):

```sh
sudo braid discover --write
```

The corrupt rebuild preserves the original bytes at
`pool.json.corrupt-<RFC3339-UTC>` before overwriting; do not remove it first.

Then unlock normally:

```sh
sudo braid unlock
```

`discover` scans `/dev/disk/by-id/` for LUKS devices with `braid-*` labels and reconstructs the membership file. See [Recovery scenarios](recovery-scenarios.md) for details.
````

No new links are introduced (the `recovery-scenarios.md` link is
unchanged), so `mdbook build docs` link coverage is unaffected.

## Conventions reused (verified)

- Sibling command link style inside `docs/commands/*.md`: `[text](status.md)`
  (no `../`). `status.md` is already a link target in this file's
  "Related commands" section, so it is safe.
- Intra-page anchor link: `[Common variations](#common-variations)` ->
  the existing `## Common variations` heading (line 36). mdbook derives
  the anchor as the lowercased, hyphenated heading.
- ASCII only: use `--`, not an em-dash (matches the rest of the page and
  repo CLI-output style).

## Verification

1. **Link/anchor check (primary):** `mdbook build docs` -- this runs
   `mdbook-linkcheck2` (configured in `docs/book.toml`), which fails on a
   broken cross-link or anchor. Confirms the new `(status.md)` and
   `(#common-variations)` links resolve. If `mdbook` is not on PATH, run
   it from the project dev shell (e.g. `nix develop`).
2. **Internal-consistency read-through:** in `discover.md`, re-read the
   edited top sections against "What happens under the hood" step 2 and
   "Safety checks" bullet 1; in `troubleshooting.md`, confirm the
   "Pool won't mount" recipe routes corrupt -> `discover --write` before
   any bare `discover`. Both pages should tell one consistent story (bare
   preview = missing only; valid -> status; corrupt -> `--write`).
3. **No code tests required:** the change is doc prose; no test pins it,
   and the `BareDiscoverError` strings are untouched, so the existing
   wording-pinning unit tests and `tests/cli/braid-discover.py` stay
   green. A `cargo test` / VM run is not needed for this change.
