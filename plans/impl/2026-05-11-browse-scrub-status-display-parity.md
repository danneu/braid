# Plan: fix browse "Scrub > Status" argv/display divergence

## Context

`SubTab::ScrubStatus` in the browse TUI displays the shell line
`btrfs scrub status <mp>` at the bottom of the screen and in the help
overlay's footer-promise (`cli/src/browse/model.rs:168`), but actually
executes `btrfs scrub status --raw <mp>`
(`cli/src/cmd.rs:359-367`). A user who copy-pastes the displayed
command into a shell will see `555.40GiB` formatting instead of the
raw byte count (`596353253376`) that browse just rendered. This
silently breaks browse's value proposition: "what you see is the
literal upstream tool's output for the command we're showing you".

Every other browse subtab's argv matches its `command_display` -- this
is the lone divergence. The `--raw` flag exists for
`parse_btrfs_scrub_status` (`cli/src/parse/btrfs_scrub_status.rs:78,
92, 104` -- raw `u64` byte parsing), which browse does not use. Browse
just renders stdout verbatim.

Outcome: browse displays the human-readable scrub status (matching the
displayed command), parser callers (`status`, `idle`,
`scrub_needs_resume`, `tui/probe`) continue to receive the raw-bytes
output they parse, and the structural opportunity for the bug to
recur on any other subtab is closed by construction.

## Approach

Two changes, kept in one PR:

1. **Add `CmdRequest::BtrfsScrubStatusHuman { mount_point }`** (no `--raw`)
   and route `SubTab::ScrubStatus` to it. Existing `BtrfsScrubStatus`
   (with `--raw`) is untouched for the 12+ parser-side callers.

2. **Derive every browse command-display string from the argv itself**,
   including the `SubvolDetail` branch in `view.rs`. After this, display
   uses `shell_words::join` quoting via `CmdArgs::to_shell_string`
   (`cli/src/cmd.rs:254-259`) and display/argv parity holds by
   construction for every browse code path -- the class of bug is gone,
   not just this one instance. This also fixes a latent bug where a
   subvolume path containing spaces would render unquoted in the
   `SubvolDetail` footer (`btrfs subvolume show /mnt/storage/my data`)
   but execute with the unquoted path token-split by the shell if
   pasted; deriving from argv produces the correctly-quoted form.

The user prefers the `Human` suffix over the existing `*Raw` suffix
convention (`BtrfsFilesystemUsage` / `BtrfsFilesystemUsageRaw`,
`BtrfsDeviceUsage` / `BtrfsDeviceUsageRaw`) because explicit `Raw` vs
`Human` reads more clearly than `*Raw` vs plain-name. Group the new
variant under the existing
`// Browse TUI -- human-readable display variants (no --raw / --format json)`
section at `cli/src/cmd.rs:204` so the convention is visible even if
the suffix differs.

## Critical files

- `cli/src/cmd.rs:204-219` -- declare `BtrfsScrubStatusHuman { mount_point: MountPoint }` alongside the other browse-only variants.
- `cli/src/cmd.rs:775-794` -- add the argv arm for the new variant: `["scrub", "status", mount_point.0]` (no `--raw`).
- `cli/src/browse/model.rs:151-153` -- change `SubTab::ScrubStatus => CmdRequest::BtrfsScrubStatus { .. }` to `SubTab::ScrubStatus => CmdRequest::BtrfsScrubStatusHuman { .. }`.
- `cli/src/browse/model.rs:160-171` -- delete the `SubTab::command_display` method entirely (it has exactly one caller -- verified by `grep "command_display" cli/src/`).
- `cli/src/browse/model.rs:233-236` -- rewrite `current_command_display` to derive from argv:
  ```rust
  pub fn current_command_display(&self) -> String {
      self.current_subtab()
          .request(&self.mount_point)
          .to_argv()
          .to_shell_string()
  }
  ```
- `cli/src/browse/view.rs:160-174` -- rewrite the `SubvolDetail` branch to derive from argv instead of hand-rendering. Replace `format!("btrfs subvolume show {}/{}", model.mount_point.as_str(), sv.path)` with:
  ```rust
  CmdRequest::BtrfsSubvolumeShow {
      path: format!("{}/{}", model.mount_point.as_str(), sv.path),
  }
  .to_argv()
  .to_shell_string()
  ```
  This closes the same display/argv divergence class for the detail
  view and produces shell-correct quoting for paths with spaces.

No changes to parser-side callers (`cli/src/scrub_needs_resume.rs:28`,
`cli/src/idle.rs:73`, `cli/src/status.rs` (multiple), `cli/src/tui/probe.rs:97`,
test fixtures). They keep using `BtrfsScrubStatus`.

## Test pin

Two tests, each pinning a contract the derivation alone does not cover.
Display/argv parity for browse subtabs holds by construction after the
derivation change, so no iteration test is needed.

**1. Parser raw-argv contract** in `cli/src/cmd.rs`'s existing
`mod tests` block (around line 1203):

```rust
#[test]
fn btrfs_scrub_status_argv_uses_raw_for_parser_path() {
    let argv = CmdRequest::BtrfsScrubStatus {
        mount_point: MountPoint("/mnt/storage".into()),
    }
    .to_argv()
    .to_shell_string();
    assert_eq!(argv, "btrfs scrub status --raw /mnt/storage");
}
```

Pins `--raw` to the parser variant so future refactors cannot
silently drop it and cause `parse_btrfs_scrub_status` to lose `u64`
byte values (`cli/src/parse/btrfs_scrub_status.rs:78, 92, 104`).
Btrfs docs confirm `--raw` is the `status`-subcommand flag that
prints byte counts without the `B` suffix
(`reference/btrfs-progs/Documentation/btrfs-scrub.rst:270-271`); the
default is `--human-readable` (same file, lines 272-273), which is
what `BtrfsScrubStatusHuman` will get.

Uses `MountPoint(pub String)` directly (`cli/src/types.rs:19`) -- no
fixture or runner needed.

**2. SubvolDetail shell-quoting** in
`cli/src/browse/view.rs`'s existing `mod tests` block, next to
`snapshot_subvol_detail` (`cli/src/browse/view.rs:417`). Add a
snapshot test that puts a subvolume with a space-bearing path
(e.g. `"my data"`) into `model.subvolumes` and asserts the rendered
footer contains the shell-quoted form
(`btrfs subvolume show '/mnt/storage/my data'`), not the
token-split form. This catches the latent bug that the
SubvolDetail rewrite fixes and locks in the argv-derived path.

Optional sanity test (small, drop if it feels redundant): assert
`BtrfsScrubStatusHuman` argv is `"btrfs scrub status /mnt/storage"`
(no `--raw`).

## Verification

1. `just test-rust` -- the new pin passes; existing parser fixture
   tests still pass (no parser-side argv changed).
2. Manual: run browse against a NixOS VM with a mounted pool, switch
   to Scrub > Status, copy-paste the footer command into the VM
   shell, confirm output matches the body.
3. `just test-parsers` -- confirms parser-driven scrub paths are
   unaffected (they still hit `BtrfsScrubStatus` -> `--raw`).
