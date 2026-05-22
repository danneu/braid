Read `/Users/dan/Code/braid/command-findings/index.md`. Find the next 10 rows from the top whose Status column is blank (`|        |` cells -- 8 spaces between pipes). Process top-to-bottom. If fewer than 10 blank rows exist, process what's there.

For each row:

1. **Build the prompt file.** The Finding column has a link like `[#N](./<command>.md:LINE)`. Open that per-command file at the `(N)` heading and capture the entire Severity / Category / Issue / Location / Impact / Fix block. Write it to `/tmp/verify-issue-<command>-<N>.md` with `/verify-issue` as the first line so the skill triggers when claude consumes the file as the initial message:

   ```
   /verify-issue

   **Severity:** ...
   **Category:** ...
   **Issue:** ...
   **Location:** ...
   **Impact:** ...
   **Fix:** ...
   ```

2. **Launch claude in a new tab,** piping the file in as stdin. Use `sh -c`
   because `danterm tab new --cmd` launches the command directly; the shell is
   what interprets the pipe:

   ```bash
   danterm tab new \
     --cwd /Users/dan/Code/braid \
     --title "<command> <N>: <sev>" \
     --cmd "sh -c 'cat /tmp/verify-issue-<command>-<N>.md | claude --effort max'"
   ```

   - This uses the known-good `cat file.md | claude --effort max` stdin path.
   - `sev` is lowercase abbreviation: `high` / `med` / `low`.

3. **Mark the row `wip` in `index.md`.** Replace that row's `|        |` cell (8 spaces) with `| wip    |` (4 trailing spaces). Column is 8 chars wide between pipes -- preserve alignment. The row is unique by its anchor like `[#N](./<command>.md:LINE)`.

After all launches, print a summary table of (command, #, severity, title). I'll tell you when each row should flip to `impl` (done) or `drop` (rejected).

## Syncing implemented rows

When I ask to **sync implemented rows** (or any equivalent -- "sync impl", "mark done tabs", etc.), reconcile `index.md` against danterm's live tab state. A **gray** tab means the launched claude session has exited, so the row's work is done and should flip from `wip` to `impl`. Tabs in any other state (blue/yellow/orange/red/no color) are still active or awaiting review -- leave them alone.

Scope the sync to the danterm group the agent is itself running in. The fanout launches above don't pass `--group`, so the new tabs inherit the caller's group; reading `danterm pane info` gives you that group's id directly.

### Procedure

1. **Resolve the agent's group id.**

   ```bash
   danterm pane info | jq -r '.group.id'
   ```

   Returns a UUID like `BF3C3BD6-9BAC-4AD2-BFCF-55936AE68CCF`. Use the id (not `.group.name`) so renames don't break the match.

2. **List gray tabs in that group.** Pipe the full snapshot through jq, scoped to the group id from step 1, and emit `customTitle` for every gray tab:

   ```bash
   GROUP_ID=$(danterm pane info | jq -r '.group.id')
   danterm ls | jq -r --arg gid "$GROUP_ID" '
     .groups[] | select(.id == $gid) | .tabs[]
     | select(.color == "gray")
     | .customTitle // empty
   '
   ```

   Expected output: one title per line in the launch format `<command> <N>: <sev>`, e.g.

   ```
   add 8: low
   discover 6: med
   ```

3. **Map each title to its row.** Parse the title as `<command> <N>: <sev>` (whitespace-separated; `<sev>` is informational and not used for matching). The row in `/Users/dan/Code/braid/command-findings/index.md` is uniquely identified by the anchor `[#<N>](./<command>.md:LINE)` in its Finding column -- match on `<command>` + `<N>` only.

4. **Flip each matched row's status cell to `impl`.** Replace `| wip    |` (4 trailing spaces) with `| impl   |` (3 trailing spaces). The Status column is 8 chars wide between pipes -- preserve alignment exactly.

   Skip (do not modify) any title where:
   - the title doesn't parse as `<command> <N>: <sev>` (unrelated tab that happened to be gray),
   - no row in `index.md` matches the parsed `<command>`/`<N>`,
   - the matched row's status cell is not `wip` (already `impl`, already `drop`, or still blank -- do not overwrite).

5. **Report.** Print two short lists:
   - **Flipped:** `(command, #, severity)` for every row updated.
   - **Skipped:** gray-tab titles that did not result in a flip, with the reason (no match, non-wip status, unparseable title).

Sync is read-only on the tab side: do not close, rename, focus, split, or send keys to any danterm tab. Do not touch tabs that aren't gray, and do not invent `impl` rows.
